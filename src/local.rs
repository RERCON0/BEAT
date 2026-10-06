//! Local music: audio files dropped by hand into the cache folder are
//! scanned, tagged and played without a Navidrome server. Downloaded cache
//! entries are excluded by their indexed paths; the folder itself is
//! read-only for BEAT — files are never moved or deleted.

use crate::api::Song;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{StandardTagKey, StandardVisualKey, Tag, Value, Visual};
use symphonia::core::probe::{Hint, ProbeResult};

/// Local song ids start with this prefix, so they can never collide with
/// server ids.
pub const LOCAL_ID_PREFIX: &str = "local:";

// Opus is not in the decoder stack (rodio/symphonia), so `.opus` is skipped
// rather than offered and then failing at playback.
const AUDIO_EXTS: [&str; 8] = ["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "mp4"];
const MAX_DEPTH: usize = 8;
const MAX_TRACKS: usize = 5000;
/// How much of a hand-dropped file tag probing may read. Real tracks are far
/// below this (it is about three hours of 320 kbps), but nothing capped the
/// read before: a huge or hostile file in the cache folder was parsed end to
/// end by symphonia, twice, on every rescan that saw it change.
const MAX_PROBE_BYTES: u64 = 512 * 1024 * 1024;

/// The first `MAX_PROBE_BYTES` of a file, as a symphonia `MediaSource`. Seeking
/// past the end reports the limit instead of the real length, so the format
/// probe still sees a consistent stream — and tags, which live at the front,
/// are unaffected.
struct ProbeSource {
    file: std::fs::File,
    pos: u64,
    size: u64,
}

impl ProbeSource {
    fn new(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let size = file.metadata().ok()?.len().min(MAX_PROBE_BYTES);
        Some(Self { file, pos: 0, size })
    }
}

impl std::io::Read for ProbeSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        if self.pos >= self.size {
            return Ok(0);
        }
        let want = (self.size - self.pos).min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(self.pos))?;
        let read = self.file.read(&mut buf[..want])?;
        self.pos += read as u64;
        Ok(read)
    }
}

impl std::io::Seek for ProbeSource {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let target = match from {
            SeekFrom::Start(pos) => pos as i128,
            SeekFrom::Current(offset) => self.pos as i128 + i128::from(offset),
            SeekFrom::End(offset) => self.size as i128 + i128::from(offset),
        };
        let clamped = target.clamp(0, self.size as i128) as u64;
        self.pos = clamped;
        Ok(clamped)
    }
}

impl symphonia::core::io::MediaSource for ProbeSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.size)
    }
}

#[derive(Clone, Debug)]
pub struct LocalTrack {
    pub id: String,
    pub path: PathBuf,
    /// Path relative to the music root, `/`-separated (for display).
    pub rel: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub suffix: String,
    pub size: u64,
}

impl LocalTrack {
    pub fn to_song(&self) -> Song {
        Song {
            id: self.id.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration: self.duration,
            suffix: self.suffix.clone(),
            ..Song::default()
        }
    }
}

pub fn is_local_id(id: &str) -> bool {
    id.starts_with(LOCAL_ID_PREFIX)
}

/// Recursively collects playable files under `root`, skipping paths already
/// indexed as cache downloads (`excluded`, `/`-separated relative paths).
/// Unreadable folders are skipped; symlinks and junctions are not followed
/// (cycle safety).
#[cfg(test)]
pub fn scan(root: &Path, excluded: &HashSet<String>) -> Vec<LocalTrack> {
    scan_with(root, excluded, &ProbeCache::default())
}

/// `scan` that reads the tags of a file only when it is new or changed: a
/// rescan happens on every visit of the list and after every download, and
/// opening thousands of files each time (on a synced folder that can even
/// download them) is what made it slow.
pub fn scan_with(root: &Path, excluded: &HashSet<String>, cache: &ProbeCache) -> Vec<LocalTrack> {
    let mut tracks = Vec::new();
    walk(root, root, 0, excluded, cache, &mut tracks);
    // A long-running app may see arbitrarily many replaced/deleted files;
    // retain only probes for files still present in this scan.
    let present: HashSet<&str> = tracks.iter().map(|track| track.rel.as_str()).collect();
    crate::lock(&cache.entries).retain(|rel, _| present.contains(rel.as_str()));
    tracks.sort_by(|a, b| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()));
    tracks
}

/// What was read from one file, and the size and modification time it was
/// valid for.
struct ProbedFile {
    size: u64,
    modified: Option<SystemTime>,
    probe: Probe,
}

/// Tags and durations read so far, valid while a file keeps its size and
/// modification time.
#[derive(Default)]
pub struct ProbeCache {
    entries: Mutex<HashMap<String, ProbedFile>>,
    probed: AtomicUsize,
}

impl ProbeCache {
    /// How many files were actually read (not served from the cache).
    #[cfg(test)]
    pub fn probed(&self) -> usize {
        self.probed.load(Ordering::SeqCst)
    }

    /// The tags of `path`: from the cache while `size` and `modified` still
    /// match, otherwise read from the file.
    fn probe(&self, rel: &str, path: &Path, size: u64, modified: Option<SystemTime>) -> Probe {
        if let Some(cached) = crate::lock(&self.entries).get(rel) {
            if cached.size == size && cached.modified == modified {
                return cached.probe.clone();
            }
        }
        let probe = probe_file(path);
        self.probed.fetch_add(1, Ordering::SeqCst);
        crate::lock(&self.entries).insert(rel.to_owned(), ProbedFile { size, modified, probe: probe.clone() });
        probe
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    excluded: &HashSet<String>,
    cache: &ProbeCache,
    tracks: &mut Vec<LocalTrack>,
) {
    if depth > MAX_DEPTH || tracks.len() >= MAX_TRACKS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if tracks.len() >= MAX_TRACKS {
            return;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if linked(&entry.path()) {
            continue;
        }
        if kind.is_dir() {
            walk(root, &entry.path(), depth + 1, excluded, cache, tracks);
        } else if kind.is_file() {
            if let Some(track) = track_for(root, &entry.path(), excluded, cache) {
                tracks.push(track);
            }
        }
    }
}

/// Cheap filter shared by the full scan and the counting walk: is this path a
/// playable, not-yet-indexed file? Returns (relative path, suffix, size, mtime).
fn audio_file(
    root: &Path,
    path: &Path,
    excluded: &HashSet<String>,
) -> Option<(String, String, u64, Option<SystemTime>)> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    let suffix = path.extension()?.to_str()?.to_ascii_lowercase();
    if !AUDIO_EXTS.contains(&suffix.as_str()) {
        return None;
    }
    let rel = path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    if rel.is_empty() || excluded.contains(&rel) {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    let size = metadata.len();
    if size == 0 {
        return None;
    }
    Some((rel, suffix, size, metadata.modified().ok()))
}

/// Counts hand-dropped files and their bytes without opening any of them
/// (sidebar stats while no full scan has run).
pub fn count(root: &Path, excluded: &HashSet<String>) -> (usize, u64) {
    let mut stats = (0usize, 0u64);
    count_walk(root, root, 0, excluded, &mut stats);
    stats
}

fn count_walk(root: &Path, dir: &Path, depth: usize, excluded: &HashSet<String>, stats: &mut (usize, u64)) {
    if depth > MAX_DEPTH || stats.0 >= MAX_TRACKS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if stats.0 >= MAX_TRACKS {
            return;
        }
        let Ok(kind) = entry.file_type() else { continue };
        if linked(&entry.path()) {
            continue;
        }
        if kind.is_dir() {
            count_walk(root, &entry.path(), depth + 1, excluded, stats);
        } else if kind.is_file() {
            if let Some((_, _, size, _)) = audio_file(root, &entry.path(), excluded) {
                stats.0 += 1;
                stats.1 = stats.1.saturating_add(size);
            }
        }
    }
}

fn linked(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return true };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        } // Windows junction/reparse point
    }
    false
}

fn track_for(root: &Path, path: &Path, excluded: &HashSet<String>, cache: &ProbeCache) -> Option<LocalTrack> {
    let (rel, suffix, size, modified) = audio_file(root, path, excluded)?;
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    let (file_artist, file_title) = split_title(stem);
    let probed = cache.probe(&rel, path, size, modified);
    let title = probed.title.unwrap_or_else(|| if file_title.trim().is_empty() { stem.to_owned() } else { file_title });
    let artist = probed.artist.unwrap_or(file_artist);
    let album = probed.album.unwrap_or_else(|| {
        path.parent()
            .filter(|parent| *parent != root)
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    Some(LocalTrack {
        id: format!("{LOCAL_ID_PREFIX}{rel}"),
        path: path.to_path_buf(),
        rel,
        title,
        artist,
        album,
        duration: probed.duration,
        suffix,
        size,
    })
}

/// `Artist - Title` split for untagged files; a leading track number is not
/// an artist (`01 - Song`, `01. Song`).
fn split_title(stem: &str) -> (String, String) {
    let stem = stem.trim();
    if let Some((left, right)) = stem.split_once(" - ") {
        let (left, right) = (left.trim(), right.trim());
        if !left.is_empty() && left.chars().all(|c| c.is_ascii_digit()) {
            return (String::new(), strip_track_number(right).to_owned());
        }
        if !left.is_empty() && !right.is_empty() {
            return (left.to_owned(), strip_track_number(right).to_owned());
        }
    }
    (String::new(), strip_track_number(stem).to_owned())
}

fn strip_track_number(raw: &str) -> &str {
    let digits = raw.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 || digits == raw.chars().count() {
        return raw;
    }
    let rest = raw[digits..].trim_start();
    let Some(rest) = rest.strip_prefix(['.', '-', ')']) else { return raw };
    let rest = rest.trim_start();
    if rest.is_empty() {
        raw
    } else {
        rest
    }
}

#[derive(Default, Clone)]
struct Probe {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: f64,
}

/// Tags and duration of one file. Decoders run on arbitrary user files, so
/// panics are contained the same way server-supplied bytes are (the global
/// panic dialog must not fire for one broken file).
fn probe_file(path: &Path) -> Probe {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(|| probe(path));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    result.unwrap_or_default()
}

fn probe_format(path: &Path) -> Option<ProbeResult> {
    let source = ProbeSource::new(path)?;
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }
    symphonia::default::get_probe().format(&hint, stream, &Default::default(), &Default::default()).ok()
}

/// Embedded artwork of a file (front cover preferred), for the on-disk list
/// and the player bar. Panic-guarded like every other decoder path.
pub fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(|| embedded_cover_inner(path));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    result.ok().flatten()
}

fn embedded_cover_inner(path: &Path) -> Option<Vec<u8>> {
    const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;
    let mut probed = probe_format(path)?;
    let mut cover = None;
    if let Some(mut metadata) = probed.metadata.get() {
        if let Some(revision) = metadata.skip_to_latest() {
            cover = pick_visual(revision.visuals(), MAX_COVER_BYTES);
        }
    }
    if cover.is_none() {
        if let Some(revision) = probed.format.metadata().skip_to_latest() {
            cover = pick_visual(revision.visuals(), MAX_COVER_BYTES);
        }
    }
    cover
}

fn pick_visual(visuals: &[Visual], max_bytes: usize) -> Option<Vec<u8>> {
    let visual = visuals
        .iter()
        .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
        .or_else(|| visuals.first())?;
    if visual.data.is_empty() || visual.data.len() > max_bytes {
        return None;
    }
    Some(visual.data.to_vec())
}

fn probe(path: &Path) -> Probe {
    let mut probe = Probe::default();
    if let Some(mut probed) = probe_format(path) {
        if let Some(params) = probed.format.default_track().map(|track| track.codec_params.clone()) {
            probe.duration = duration_from_params(&params);
        }
        if let Some(mut metadata) = probed.metadata.get() {
            if let Some(revision) = metadata.skip_to_latest() {
                collect_tags(revision.tags(), &mut probe);
            }
        }
        if let Some(revision) = probed.format.metadata().skip_to_latest() {
            collect_tags(revision.tags(), &mut probe);
        }
    }
    // MP3 without a Xing header has no frame count until decoded; rodio's
    // decoder derives the duration from the byte length instead.
    if probe.duration <= 0.0 {
        if let Some(source) = ProbeSource::new(path) {
            let stream = MediaSourceStream::new(Box::new(source), Default::default());
            if let Ok(decoder) = rodio::Decoder::new(stream) {
                if let Some(duration) = rodio::Source::total_duration(&decoder) {
                    probe.duration = duration.as_secs_f64();
                }
            }
        }
    }
    probe.duration =
        if probe.duration.is_finite() { probe.duration.clamp(0.0, crate::api::MAX_DURATION_SECS) } else { 0.0 };
    probe
}

fn duration_from_params(params: &CodecParameters) -> f64 {
    let Some(frames) = params.n_frames else { return 0.0 };
    if let Some(time_base) = params.time_base {
        let time = time_base.calc_time(frames);
        return time.seconds as f64 + time.frac;
    }
    match params.sample_rate {
        Some(rate) if rate > 0 => frames as f64 / rate as f64,
        _ => 0.0,
    }
}

fn collect_tags(tags: &[Tag], probe: &mut Probe) {
    for tag in tags {
        let Value::String(value) = &tag.value else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match tag.std_key {
            Some(StandardTagKey::TrackTitle) if probe.title.is_none() => probe.title = Some(value.to_owned()),
            Some(StandardTagKey::Artist) if probe.artist.is_none() => probe.artist = Some(value.to_owned()),
            Some(StandardTagKey::Album) if probe.album.is_none() => probe.album = Some(value.to_owned()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "beat-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn titles_split_without_mistaking_track_numbers_for_artists() {
        assert_eq!(split_title("01 - Song"), (String::new(), "Song".into()));
        assert_eq!(split_title("01. Song"), (String::new(), "Song".into()));
        assert_eq!(split_title("Artist - Song"), ("Artist".into(), "Song".into()));
        assert_eq!(split_title("Artist - 01 - Song"), ("Artist".into(), "Song".into()));
        assert_eq!(split_title("99 Red Balloons"), (String::new(), "99 Red Balloons".into()));
        assert_eq!(split_title(""), (String::new(), String::new()));
    }

    #[test]
    fn scan_collects_audio_recursively_and_skips_junk() {
        let root = temp_dir("local-scan");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("03 - Track.flac"), b"not audio").unwrap();
        std::fs::write(root.join("Artist - Song.mp3"), b"not audio").unwrap();
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        std::fs::write(root.join("empty.mp3"), b"").unwrap();
        let tracks = scan(&root, &HashSet::new());
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Song");
        assert_eq!(tracks[0].artist, "Artist");
        assert!(tracks[0].id.starts_with(LOCAL_ID_PREFIX));
        assert!(is_local_id(&tracks[0].id));
        let nested = tracks.iter().find(|track| track.title == "Track").unwrap();
        assert_eq!(nested.album, "sub");
        assert_eq!(nested.suffix, "flac");
        assert_eq!(nested.rel, "sub/03 - Track.flac");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_files_are_not_read_again_on_a_rescan() {
        let root = temp_dir("probe-cache");
        std::fs::create_dir_all(&root).unwrap();
        write_wav(&root.join("a.wav"), 1);
        write_wav(&root.join("b.wav"), 1);
        let cache = ProbeCache::default();
        let first = scan_with(&root, &HashSet::new(), &cache);
        assert_eq!(first.len(), 2);
        assert_eq!(cache.probed(), 2);
        let second = scan_with(&root, &HashSet::new(), &cache);
        assert_eq!(cache.probed(), 2, "unchanged files were read again");
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].duration, first[0].duration);
        std::fs::remove_file(root.join("b.wav")).unwrap();
        scan_with(&root, &HashSet::new(), &cache);
        assert_eq!(crate::lock(&cache.entries).len(), 1, "deleted files must leave the probe cache");
        // A file that changed (new size) is read again and shows the new length.
        write_wav(&root.join("a.wav"), 2);
        let third = scan_with(&root, &HashSet::new(), &cache);
        assert_eq!(cache.probed(), 3);
        let changed = third.iter().find(|track| track.rel == "a.wav").unwrap();
        assert!((changed.duration - 2.0).abs() < 0.1, "duration {}", changed.duration);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn probing_stops_at_the_read_ceiling() {
        // A huge hand-dropped file must not be parsed end to end on every
        // rescan; the source reports the ceiling as its length instead.
        let path = std::env::temp_dir().join(format!(
            "beat-probe-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let source = ProbeSource::new(&path).expect("the file opens");
        assert_eq!(source.size, 4096);
        assert_eq!(symphonia::core::io::MediaSource::byte_len(&source), Some(4096));
        let mut limited = ProbeSource { file: source.file, pos: 0, size: 1024 };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut limited, &mut buf).unwrap();
        assert_eq!(buf.len(), 1024, "read past the ceiling");
        // Seeking beyond the ceiling clamps to it instead of failing.
        use std::io::Seek;
        assert_eq!(limited.seek(std::io::SeekFrom::Start(99_999)).unwrap(), 1024);
        assert_eq!(limited.seek(std::io::SeekFrom::End(10)).unwrap(), 1024);
        assert_eq!(limited.seek(std::io::SeekFrom::Current(-50)).unwrap(), 974);
        assert_eq!(limited.seek(std::io::SeekFrom::Start(5)).unwrap(), 5);
        // And a real file still probes normally.
        let real_dir = temp_dir("probe-ceiling-real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("a.wav");
        write_wav(&real, 1);
        assert!(probe_format(&real).is_some(), "a normal file stopped probing");
        let _ = std::fs::remove_dir_all(&real_dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_skips_paths_indexed_as_cache_downloads() {
        let root = temp_dir("local-excluded");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Artist - Song.mp3"), b"not audio").unwrap();
        std::fs::write(root.join("Dropped.mp3"), b"not audio").unwrap();
        let excluded: HashSet<String> = ["Artist - Song.mp3".to_owned()].into_iter().collect();
        let tracks = scan(&root, &excluded);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Dropped");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_of_a_missing_folder_is_empty_not_an_error() {
        let root = temp_dir("local-missing");
        assert!(scan(&root, &HashSet::new()).is_empty());
    }

    #[test]
    fn counts_skip_indexed_files_and_junk() {
        let root = temp_dir("local-count");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.mp3"), vec![0u8; 10]).unwrap();
        std::fs::write(root.join("sub").join("b.flac"), vec![0u8; 20]).unwrap();
        std::fs::write(root.join("c.txt"), b"x").unwrap();
        std::fs::write(root.join("d.mp3"), b"").unwrap();
        let excluded: HashSet<String> = ["a.mp3".to_owned()].into_iter().collect();
        assert_eq!(count(&root, &excluded), (1, 20));
        assert_eq!(count(&root, &HashSet::new()), (2, 30));
        assert_eq!(count(&temp_dir("local-count-missing"), &HashSet::new()), (0, 0));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn embedded_cover_of_a_coverless_file_is_none() {
        let root = temp_dir("local-cover");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Tone.wav");
        write_wav(&path, 1);
        assert!(embedded_cover(&path).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn probe_reads_the_duration_of_a_real_file() {
        let root = temp_dir("local-wav");
        std::fs::create_dir_all(&root).unwrap();
        write_wav(&root.join("Artist - Tone.wav"), 1);
        let tracks = scan(&root, &HashSet::new());
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].artist, "Artist");
        assert_eq!(tracks[0].title, "Tone");
        assert!((tracks[0].duration - 1.0).abs() < 0.1, "duration {}", tracks[0].duration);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Minimal 8-bit mono PCM WAV, one second by default.
    fn write_wav(path: &Path, seconds: u32) {
        let sample_rate = 8000u32;
        let samples = sample_rate * seconds;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + samples).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&samples.to_le_bytes());
        bytes.extend(std::iter::repeat_n(128u8, samples as usize));
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn local_tracks_become_playable_songs() {
        let track = LocalTrack {
            id: "local:a/b.mp3".into(),
            path: PathBuf::from("x"),
            rel: "a/b.mp3".into(),
            title: "T".into(),
            artist: "A".into(),
            album: "B".into(),
            duration: 12.0,
            suffix: "mp3".into(),
            size: 1,
        };
        let song = track.to_song();
        assert_eq!(song.id, track.id);
        assert_eq!(song.title, "T");
        assert_eq!(song.duration, 12.0);
        assert!(!is_local_id("42"));
    }
}
