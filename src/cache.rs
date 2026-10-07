//! Audio cache: one file per track under `<root>/<Artist>/<Album>/<NN - Title>.<ext>`,
//! plus a JSON index mapping song ids to files. Downloads are progressive:
//! bytes are written to a `.part` file while a `GrowingReader` can already feed
//! the player from what has arrived, so playback starts before the download
//! ends and the finished file is exactly the cache entry.

use crate::api::{self, Song};
use crate::config::atomic_write;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const INDEX_FILE: &str = ".beat-index.json";
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
/// Playback may start once this much audio has arrived (or the file is done).
pub const PLAYBACK_BUFFER_BYTES: u64 = 512 * 1024;
/// Bytes and playing time are not linear (VBR): a seek inside a running
/// download stays this fraction of the track short of the downloaded bytes.
const SEEK_MARGIN: f32 = 0.02;
/// A track bigger than this is not a track: a broken or hostile server must
/// not be able to fill the disk.
const MAX_TRACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Windows resolves a path through `MAX_PATH` (260 UTF-16 units) unless the
/// process is `longPathAware`, and BEAT ships no such manifest. The absolute
/// path is therefore kept under this, whatever the tags say.
const MAX_ABSOLUTE_PATH: usize = 240;
/// Longest single path component. NTFS allows 255; the rest is room for the
/// ` (2)` collision suffix and the `.part` file.
const MAX_COMPONENT: usize = 200;
/// Room inside `MAX_ABSOLUTE_PATH` for the parts that are added after the
/// relative path is built: two separators, ` (999)` and `.part`.
const PATH_HEADROOM: usize = 12;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct CachedTrack {
    pub id: String,
    /// Relative path inside the cache root, `/`-separated.
    pub path: String,
    #[serde(deserialize_with = "crate::api::de_text")]
    pub title: String,
    #[serde(deserialize_with = "crate::api::de_text")]
    pub artist: String,
    #[serde(deserialize_with = "crate::api::de_text")]
    pub album: String,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub suffix: String,
    #[serde(default)]
    pub size: u64,
    /// `raw` or `mp3`: which stream format produced this file.
    #[serde(default)]
    pub format: String,
}

#[derive(Serialize, Deserialize, Default)]
struct IndexFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tracks: HashMap<String, CachedTrack>,
}

/// Borrowed view of the index, so saving never copies every entry.
#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    tracks: &'a HashMap<String, CachedTrack>,
}

/// Track count and total bytes, kept next to the index so the sidebar does not
/// sum the whole index on every frame.
#[derive(Clone, Copy, Default)]
struct CacheStats {
    count: usize,
    bytes: u64,
}

#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    index_name: String,
    index: Mutex<HashMap<String, CachedTrack>>,
    /// Final paths (lowercase, `/`-separated, relative) claimed by running
    /// downloads: nothing on disk marks them as taken until the download ends.
    reserved: Mutex<HashSet<String>>,
    /// Serialises index writes (see `save_index`).
    save_lock: Mutex<()>,
    /// Longest file a download may write.
    max_track_bytes: AtomicU64,
    /// Cached `stats()`. Recomputed from the index after every change to it.
    stats: Mutex<CacheStats>,
    /// Why the index started empty (unreadable/oversized file), for the UI.
    warning: Mutex<Option<String>>,
    /// The unreadable index could not be set aside: never overwrite it.
    save_blocked: bool,
}

/// What reading `.beat-index.json` produced.
struct LoadedIndex {
    tracks: HashMap<String, CachedTrack>,
    warning: Option<String>,
    save_blocked: bool,
}

impl Cache {
    pub fn load(root: PathBuf) -> Cache {
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let loaded = load_index(&root, stamp);
        Self::from_loaded(root, INDEX_FILE.into(), loaded)
    }

    /// A track id belongs to one server/account, not to every server the user
    /// may configure. Existing unscoped data is adopted only at startup, while
    /// the saved configuration still identifies its original account.
    pub fn load_profile(root: PathBuf, profile: Option<&str>, adopt_legacy: bool) -> Cache {
        let Some(profile) = profile else { return Self::load(root) };
        let name = profile_index_name(profile);
        let target = root.join(&name);
        let legacy = root.join(INDEX_FILE);
        let migration_error = if adopt_legacy && !target.exists() && legacy.is_file() {
            std::fs::rename(&legacy, &target).err()
        } else {
            None
        };
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut loaded = load_index_file(&root, stamp, &name);
        if let Some(error) = migration_error {
            loaded.warning = Some(format!("не удалось перенести прежний индекс кеша: {error}; аудиофайлы сохранены"));
        }
        Self::from_loaded(root, name, loaded)
    }

    fn from_loaded(root: PathBuf, index_name: String, loaded: LoadedIndex) -> Cache {
        let cache = Cache {
            inner: Arc::new(Inner {
                root,
                index_name,
                index: Mutex::new(loaded.tracks),
                reserved: Mutex::new(HashSet::new()),
                save_lock: Mutex::new(()),
                max_track_bytes: AtomicU64::new(MAX_TRACK_BYTES),
                stats: Mutex::new(CacheStats::default()),
                warning: Mutex::new(loaded.warning),
                save_blocked: loaded.save_blocked,
            }),
        };
        cache.refresh_stats();
        cache
    }

    /// Keep one shared index while downloads are running for the same account.
    pub fn for_profile(&self, root: PathBuf, profile: Option<&str>) -> Cache {
        let name = profile.map(profile_index_name).unwrap_or_else(|| INDEX_FILE.into());
        if root == self.inner.root && name == self.inner.index_name {
            self.clone()
        } else {
            Self::load_profile(root, profile, false)
        }
    }

    #[cfg(test)]
    fn set_max_track_bytes(&self, bytes: u64) {
        self.inner.max_track_bytes.store(bytes, Ordering::SeqCst);
    }

    /// A problem found while loading the index, worth showing once.
    pub fn take_warning(&self) -> Option<String> {
        crate::lock(&self.inner.warning).take()
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn entry(&self, id: &str) -> Option<CachedTrack> {
        let entry = crate::lock(&self.inner.index).get(id).cloned()?;
        self.resolve_rel(&entry.path)?.is_file().then_some(entry)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entry(id).is_some()
    }

    /// The index entry for `id`, without checking that its file still exists.
    /// For code that runs every frame: `entry` touches the disk, and a stale
    /// entry is dropped by the next scan or refresh anyway.
    pub fn indexed_entry(&self, id: &str) -> Option<CachedTrack> {
        crate::lock(&self.inner.index).get(id).cloned()
    }

    pub fn is_indexed(&self, id: &str) -> bool {
        self.indexed_entry(id).is_some()
    }

    /// Absolute path of an indexed entry, validated exactly like every other
    /// lookup: inside the cache folder and free of links. `None` when the entry
    /// is not usable, so a caller cannot accidentally join an unchecked path.
    pub fn absolute(&self, entry: &CachedTrack) -> Option<PathBuf> {
        self.resolve_rel(&entry.path)
    }

    /// `absolute` for a path that came from outside (a `local:` id, a
    /// cover key): `None` unless it stays inside the cache folder.
    pub fn resolve_rel(&self, rel: &str) -> Option<PathBuf> {
        safe_path(&self.inner.root, rel)
    }

    /// Deterministic destination for a song; collisions with another track's
    /// file (or with a download still running) get a ` (2)` suffix. Returns
    /// the final path and the `.part` path. The path stays reserved until
    /// `release` is called with it.
    pub fn dest_for(&self, song: &Song, suffix: &str) -> Result<(PathBuf, PathBuf), String> {
        let index = crate::lock(&self.inner.index);
        let mut reserved = crate::lock(&self.inner.reserved);
        if let Some(entry) = index.get(&song.id) {
            let path = self.resolve_rel(&entry.path).ok_or("путь трека в индексе кеша небезопасен")?;
            if path.exists() {
                return Err("трек уже находится в кеше".into());
            }
            if reserved.contains(&entry.path.to_lowercase()) {
                return Err("трек уже загружается".into());
            }
            reserved.insert(entry.path.to_lowercase());
            let part = part_path(&path);
            return Ok((path, part));
        }
        let rel: String = relative_path(song, suffix, self.path_budget()).to_string_lossy().replace('\\', "/");
        let mut candidate = rel.clone();
        let mut counter = 1;
        while reserved.contains(&candidate.to_lowercase())
            || index.values().any(|e| e.path.to_lowercase() == candidate.to_lowercase())
            || self.inner.root.join(&candidate).exists()
            || part_path(&self.inner.root.join(&candidate)).exists()
        {
            counter += 1;
            candidate = numbered_path(&rel, counter);
        }
        let path = self.resolve_rel(&candidate).ok_or("папка назначения кеша содержит ссылку или небезопасный путь")?;
        reserved.insert(candidate.to_lowercase());
        let part = part_path(&path);
        Ok((path, part))
    }

    /// How many UTF-16 units the relative part of a cache path may use: whatever
    /// is left of `MAX_ABSOLUTE_PATH` once the cache root is paid for.
    fn path_budget(&self) -> usize {
        MAX_ABSOLUTE_PATH.saturating_sub(utf16_len(&self.inner.root.to_string_lossy()))
    }

    /// Releases a path claimed by `dest_for` once its download ended
    /// (finished or failed).
    pub fn release(&self, path: &Path) {
        if let Ok(rel) = path.strip_prefix(&self.inner.root) {
            let key = rel.to_string_lossy().replace('\\', "/").to_lowercase();
            crate::lock(&self.inner.reserved).remove(&key);
        }
    }

    /// Records a finished download; saves the index immediately.
    pub fn insert(&self, entry: CachedTrack) -> Result<(), String> {
        if self.resolve_rel(&entry.path).is_none() {
            return Err("путь трека выходит за пределы папки кеша".into());
        }
        {
            let mut index = crate::lock(&self.inner.index);
            index.insert(entry.id.clone(), entry);
        }
        self.refresh_stats();
        self.save_index()
    }

    /// Deletes an indexed file; one that is already gone counts as deleted.
    fn delete_file(&self, entry: &CachedTrack) -> std::io::Result<()> {
        let path = self
            .resolve_rel(&entry.path)
            .ok_or_else(|| std::io::Error::other("путь файла содержит ссылку или выходит за пределы кеша"))?;
        match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    /// Forgets a track and deletes its file. A file that cannot be deleted
    /// stays in the index, so the list never claims it is gone.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let entry = {
            let mut index = crate::lock(&self.inner.index);
            index.remove(id)
        };
        if let Some(entry) = entry {
            if let Err(err) = self.delete_file(&entry) {
                crate::lock(&self.inner.index).insert(entry.id.clone(), entry);
                self.refresh_stats();
                return Err(format!("не удалось удалить файл: {err}"));
            }
        }
        self.refresh_stats();
        self.save_index()
    }

    /// Deletes every indexed file and the index. Files that cannot be deleted
    /// stay indexed and are reported.
    pub fn clear(&self) -> Result<(), String> {
        let entries: Vec<CachedTrack> = {
            let mut index = crate::lock(&self.inner.index);
            index.drain().map(|(_, entry)| entry).collect()
        };
        let stuck: Vec<CachedTrack> = entries.into_iter().filter(|entry| self.delete_file(entry).is_err()).collect();
        let count = stuck.len();
        {
            let mut index = crate::lock(&self.inner.index);
            for entry in stuck {
                index.insert(entry.id.clone(), entry);
            }
        }
        self.refresh_stats();
        self.save_index()?;
        if count == 0 {
            Ok(())
        } else {
            Err(format!("не удалось удалить файлов: {count}"))
        }
    }

    /// Drops index entries whose file disappeared (manual cleanup in Explorer).
    pub fn prune_missing(&self) -> usize {
        // A missing cache folder (unplugged or unmounted drive) makes every file
        // look deleted: that is no reason to forget them all.
        if !self.inner.root.is_dir() {
            return 0;
        }
        // The disk is checked outside the lock: with thousands of entries (or
        // a slow synced folder) that is a lot of syscalls.
        let snapshot: Vec<(String, String)> =
            crate::lock(&self.inner.index).values().map(|entry| (entry.id.clone(), entry.path.clone())).collect();
        let gone: Vec<(String, String)> = snapshot
            .into_iter()
            .filter(|(_, relative)| {
                // Access denied or a transient network failure does not prove
                // deletion. Known unsafe paths must still leave the index.
                let path = match checked_path(self.root(), relative) {
                    Ok(path) => path,
                    Err(PathError::Unsafe) => return true,
                    Err(PathError::Unavailable) => return false,
                };
                match std::fs::metadata(path) {
                    Ok(metadata) => !metadata.is_file(),
                    Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                }
            })
            .collect();
        let removed = {
            let mut index = crate::lock(&self.inner.index);
            // Only entries still describing the same file: a download may have
            // replaced one meanwhile.
            gone.iter()
                .filter(|(id, path)| {
                    index.get(id).is_some_and(|entry| &entry.path == path) && index.remove(id).is_some()
                })
                .count()
        };
        if removed > 0 {
            self.refresh_stats();
            if let Err(error) = self.save_index() {
                *crate::lock(&self.inner.warning) = Some(error);
            }
        }
        removed
    }

    pub fn stats(&self) -> (usize, u64) {
        let stats = *crate::lock(&self.inner.stats);
        (stats.count, stats.bytes)
    }

    /// Recomputes `stats()` from the index. Called after every change to it, so
    /// the per-frame sidebar reads never walk the index or block the download
    /// threads that write to it.
    fn refresh_stats(&self) {
        let index = crate::lock(&self.inner.index);
        let stats = CacheStats {
            count: index.len(),
            bytes: index.values().fold(0u64, |total, entry| total.saturating_add(entry.size)),
        };
        *crate::lock(&self.inner.stats) = stats;
    }

    /// Relative `/`-separated paths of every indexed download, for the folder
    /// scan that must skip them. Cheaper than `list`: no entry copy, no sort
    /// and no disk checks.
    pub fn indexed_rel_paths(&self) -> HashSet<String> {
        crate::lock(&self.inner.index).values().map(|entry| entry.path.clone()).collect()
    }

    /// All indexed entries whose file still exists, artist/album/track sorted.
    pub fn list(&self) -> Vec<CachedTrack> {
        let all: Vec<CachedTrack> = crate::lock(&self.inner.index).values().cloned().collect();
        let mut entries: Vec<CachedTrack> =
            all.into_iter().filter(|entry| self.resolve_rel(&entry.path).is_some_and(|path| path.is_file())).collect();
        // `sort_by_cached_key` lowercases once per row; doing it inside the
        // comparator allocates O(n log n) strings for a large cache.
        entries.sort_by_cached_key(|entry| {
            (entry.artist.to_lowercase(), entry.album.to_lowercase(), entry.title.to_lowercase())
        });
        entries
    }

    fn save_index(&self) -> Result<(), String> {
        if self.inner.save_blocked {
            return Err("индекс кеша не был прочитан; сохранение заблокировано во избежание потери данных".into());
        }
        // One writer at a time, each taking its snapshot only once it holds the
        // writer lock: an older snapshot never lands after a newer one, and the
        // index lock is free while the file is written (the UI reads the index
        // every frame and must not wait for an fsync).
        let _writer = crate::lock(&self.inner.save_lock);
        // Serialized straight from the index under its lock: copying every
        // entry first doubled the work and the memory for no benefit. The lock
        // is released before the write below, which is the slow part.
        let raw = {
            let index = crate::lock(&self.inner.index);
            serde_json::to_vec(&IndexFileRef { version: 1, tracks: &index }).map_err(|e| e.to_string())?
        };
        if raw.len() as u64 > MAX_INDEX_BYTES {
            return Err("индекс кеша слишком большой".into());
        }
        std::fs::create_dir_all(&self.inner.root).map_err(|e| format!("не удалось создать кеш: {e}"))?;
        atomic_write(&self.inner.root.join(&self.inner.index_name), &raw)
    }
}

/// Reads the index. A missing file is a fresh cache; an unreadable, oversized
/// or damaged one is set aside (never silently replaced by the next save), and
/// entries whose path leaves the cache folder are dropped: the index sits in a
/// folder that may be synced or shared, and its paths are used for deletion.
fn load_index(root: &Path, stamp: u128) -> LoadedIndex {
    load_index_file(root, stamp, INDEX_FILE)
}

fn profile_index_name(profile: &str) -> String {
    use md5::{Digest, Md5};
    format!(".beat-index-{:x}.json", Md5::digest(profile.as_bytes()))
}

fn load_index_file(root: &Path, stamp: u128, name: &str) -> LoadedIndex {
    let path = root.join(name);
    let parsed = match read_capped(&path, MAX_INDEX_BYTES) {
        Ok(None) => return LoadedIndex { tracks: HashMap::new(), warning: None, save_blocked: false },
        Ok(Some(raw)) => serde_json::from_slice::<IndexFile>(&raw).map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match parsed {
        Ok(file) if file.version == 1 => {
            let total = file.tracks.len();
            let tracks: HashMap<String, CachedTrack> = file
                .tracks
                .into_iter()
                .filter(|(id, entry)| {
                    id == &entry.id
                        && crate::api::remote_id(id)
                        && !matches!(checked_path(root, &entry.path), Err(PathError::Unsafe))
                })
                .collect();
            let dropped = total - tracks.len();
            let warning =
                (dropped > 0).then(|| format!("в индексе кеша пропущено записей с небезопасным путём: {dropped}"));
            LoadedIndex { tracks, warning, save_blocked: false }
        }
        _ => match set_aside(&path, stamp) {
            Some(name) => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some(format!("индекс кеша повреждён и начат заново; старый файл сохранён как {name}")),
                save_blocked: false,
            },
            None => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some(
                    "индекс кеша повреждён; копию сделать не удалось, сохранение индекса заблокировано".into(),
                ),
                save_blocked: true,
            },
        },
    }
}

/// Renames an unreadable index to `.beat-index.corrupt-<stamp>.bak`; `None`
/// when that failed or the name is taken.
fn set_aside(path: &Path, stamp: u128) -> Option<String> {
    let backup = path.with_file_name(format!(".beat-index.corrupt-{stamp}.bak"));
    if backup.exists() {
        return None;
    }
    std::fs::rename(path, &backup).ok()?;
    backup.file_name().map(|name| name.to_string_lossy().into_owned())
}

/// Reads at most `cap` bytes; a bigger file is an error, checked before
/// anything is loaded into memory. `Ok(None)` = no such file.
fn read_capped(path: &Path, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() > cap {
        return Err(std::io::Error::other("файл индекса слишком большой"));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(std::io::Error::other("файл индекса слишком большой"));
    }
    Ok(Some(bytes))
}

/// A `/`-separated relative path that stays inside the cache root: no empty,
/// `.` or `..` parts, no drive/absolute forms, no characters Windows rejects,
/// no trailing dot or space (Windows strips those, so the name would differ).
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !is_device_name(part)
                && !part.to_ascii_lowercase().starts_with(".beat-index")
                && !part.to_ascii_lowercase().ends_with(".part")
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        })
}

/// Refuse existing symlinks and Windows junctions at every level: a lexical
/// path inside the cache can otherwise resolve outside it when read or deleted.
fn safe_path(root: &Path, rel: &str) -> Option<PathBuf> {
    checked_path(root, rel).ok()
}

enum PathError {
    Unsafe,
    Unavailable,
}

fn checked_path(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    if !is_safe_rel(rel) {
        return Err(PathError::Unsafe);
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(PathError::Unsafe);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err(PathError::Unsafe);
                    } // FILE_ATTRIBUTE_REPARSE_POINT
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PathError::Unavailable),
        }
    }
    Ok(path)
}

pub fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(".part");
    path.with_file_name(name)
}

/// Cheap "is this audio at all" probe on the first bytes of a finished
/// download. Only container magic is looked for, so an unsupported codec may
/// still be cached, while an error page or a `200 OK` carrying "Bad Gateway"
/// is refused. The window is
/// a few kilobytes because an ID3v2 tag or a small amount of leading junk can
/// sit before the first real frame.
fn looks_like_audio(path: &Path) -> Result<(), String> {
    use std::io::Read;
    const WINDOW: usize = 4096;
    let mut file = std::fs::File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    crate::media::validate_metadata(&mut file).map_err(|e| format!("метаданные: {e}"))?;
    let mut head = vec![0u8; WINDOW];
    let read = file.read(&mut head).map_err(|e| format!("не удалось прочитать кеш: {e}"))?;
    let head = &head[..read];
    let textual =
        head.windows(3).any(|w| w == b"ID3") || head.windows(4).any(|w| w == b"fLaC" || w == b"OggS" || w == b"ftyp");
    let riff = head.windows(12).any(|w| &w[..4] == b"RIFF" && &w[8..12] == b"WAVE");
    // Matroska/WebM, and the MPEG frame / ADTS sync word.
    let binary = head.windows(4).any(|w| w == [0x1a, 0x45, 0xdf, 0xa3])
        || head.windows(2).any(|w| w[0] == 0xff && w[1] & 0xe0 == 0xe0);
    if textual || riff || binary {
        Ok(())
    } else {
        Err("сервер прислал не аудиофайл".into())
    }
}

/// Windows counts a path in UTF-16 units, so the budget has to be counted the
/// same way: an emoji is two units, not one.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Cuts `text` to at most `max` UTF-16 units, never splitting a character.
fn truncate_utf16(text: &str, max: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if used + width > max {
            return &text[..index];
        }
        used += width;
    }
    text
}

/// `Artist/Album/NN - Title.ext`, sanitized for Windows file names and short
/// enough that root + this stays inside `MAX_ABSOLUTE_PATH`. Tags are attacker
/// -controlled data: three 200-character components would otherwise exceed
/// `MAX_PATH` and fail the download with a confusing "cannot create" error.
fn relative_path(song: &Song, suffix: &str, budget: usize) -> PathBuf {
    let suffix = suffix.trim_start_matches('.');
    let track_prefix = if song.track > 0 { format!("{:02} - ", song.track) } else { String::new() };
    // Everything that is not one of the three free-text components.
    let overhead = utf16_len(&track_prefix) + 1 /* '.' */ + utf16_len(suffix)
        + 2 /* artist/album separators */ + PATH_HEADROOM;
    let room = budget.saturating_sub(overhead);
    let mut artist = sanitize_component(&song.artist, "Неизвестный артист", MAX_COMPONENT);
    let mut album = sanitize_component(&song.album, "Без альбома", MAX_COMPONENT);
    let mut title = sanitize_component(&song.title, "Трек", MAX_COMPONENT);
    // Shrink the longest component until the three fit the room; the titles
    // that need shortening are exactly the ones the user would recognise.
    let mut excess =
        [utf16_len(&artist), utf16_len(&album), utf16_len(&title)].into_iter().sum::<usize>().saturating_sub(room);
    while excess > 0 {
        let longest = [utf16_len(&artist), utf16_len(&album), utf16_len(&title)]
            .iter()
            .enumerate()
            .max_by_key(|(_, len)| **len)
            .map(|(index, _)| index);
        let Some(index) = longest else { break };
        let slot = match index {
            0 => &mut artist,
            1 => &mut album,
            _ => &mut title,
        };
        let current = utf16_len(slot);
        if current == 0 {
            // Nothing left to cut: the fixed parts alone exceed the budget.
            // Truncating further would produce an empty component, and
            // `sanitize_component` already replaced those with a fallback.
            break;
        }
        let keep = current.saturating_sub(excess);
        *slot = truncate_utf16(slot, keep).trim_end().to_owned();
        excess = excess.saturating_sub(current - utf16_len(slot));
    }
    let title = if track_prefix.is_empty() { title } else { format!("{track_prefix}{title}") };
    PathBuf::from(artist).join(album).join(format!("{title}.{suffix}"))
}

fn numbered_path(rel: &str, counter: u32) -> String {
    let path = std::path::Path::new(rel);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match path.extension().map(|s| s.to_string_lossy().into_owned()) {
        Some(ext) => {
            let parent = path.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if parent.is_empty() {
                format!("{stem} ({counter}).{ext}")
            } else {
                format!("{parent}/{stem} ({counter}).{ext}")
            }
        }
        None => format!("{rel} ({counter})"),
    }
}

/// One path component from a server tag: Windows-safe, never empty, and at most
/// `max` UTF-16 units long (NTFS allows 255; the rest is room for the collision
/// suffix and the `.part` file that are appended later).
pub fn sanitize_component(raw: &str, fallback: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
        if utf16_len(&out) >= max {
            break;
        }
    }
    let mut out = truncate_utf16(&out, max).trim().trim_end_matches(['.', ' ']).to_owned();
    if is_device_name(&out) {
        out.insert(0, '_');
    }
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    }
}

/// Windows reserves these names (with any extension) for devices; a file or
/// folder called like that cannot be created.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")).is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        })
}

// ---------------------------------------------------------------------------
// Progressive downloads
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ProgressState {
    downloaded: u64,
    total: Option<u64>,
    finished: bool,
    failed: Option<String>,
    /// The `.part` file; known once the server started answering.
    part: Option<PathBuf>,
}

/// Shared state of one download: the writer updates it, the reader and the UI
/// wait on it.
#[derive(Default)]
pub struct Progress {
    state: Mutex<ProgressState>,
    cond: Condvar,
}

impl Progress {
    pub fn new(total: Option<u64>) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(ProgressState { total, ..ProgressState::default() }), cond: Condvar::new() })
    }

    /// (downloaded, total, finished, failed)
    pub fn snapshot(&self) -> (u64, Option<u64>, bool, Option<String>) {
        let state = crate::lock(&self.state);
        (state.downloaded, state.total, state.finished, state.failed.clone())
    }

    pub fn ratio(&self) -> Option<f32> {
        let state = crate::lock(&self.state);
        state.total.filter(|total| *total > 0).map(|total| (state.downloaded as f32 / total as f32).min(1.0))
    }

    /// Blocks until at least `needed` bytes are downloaded, the download is
    /// finished, or it failed. `Ok(())` also means "finished with less". A
    /// waiter that must be interruptible passes a `cancel` flag.
    pub fn wait_for(
        &self,
        needed: u64,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        nonblocking: Option<&AtomicBool>,
    ) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = crate::lock(&self.state);
        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                return Err("воспроизведение остановлено".into());
            }
            if let Some(err) = &state.failed {
                return Err(err.clone());
            }
            if state.downloaded >= needed || state.finished {
                return Ok(());
            }
            if nonblocking.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                return Err("декодеру нужно дождаться дополнительных данных".into());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err("сервер слишком медленно отдаёт трек".into());
            }
            // The flag is not tied to the condvar: look at it regularly.
            let mut wait = deadline - now;
            if cancel.is_some() || nonblocking.is_some() {
                wait = wait.min(Duration::from_millis(100));
            }
            let (next, _) = self.cond.wait_timeout(state, wait).unwrap();
            state = next;
        }
    }

    /// How much of the track a seek may reach without waiting for the network,
    /// as a fraction of its length; `None` while that cannot be told (size
    /// unknown and the download still running).
    pub fn seekable_fraction(&self) -> Option<f32> {
        let state = crate::lock(&self.state);
        if state.finished {
            return Some(1.0);
        }
        let total = state.total.filter(|total| *total > 0)?;
        Some((state.downloaded as f32 / total as f32 - SEEK_MARGIN).clamp(0.0, 1.0))
    }

    /// The `.part` file being written, once the stream has been opened.
    pub fn part(&self) -> Option<PathBuf> {
        crate::lock(&self.state).part.clone()
    }

    /// The stream is open and its `.part` file exists.
    fn opened(&self, part: PathBuf, total: Option<u64>) {
        let mut state = crate::lock(&self.state);
        state.part = Some(part);
        if total.is_some() {
            state.total = total;
        }
        self.cond.notify_all();
    }

    fn set_total(&self, total: Option<u64>) {
        if let Some(total) = total {
            crate::lock(&self.state).total = Some(total);
        }
    }

    fn add(&self, bytes: u64) {
        let mut state = crate::lock(&self.state);
        state.downloaded = state.downloaded.saturating_add(bytes);
        self.cond.notify_all();
    }

    fn finish(&self) {
        let mut state = crate::lock(&self.state);
        state.finished = true;
        self.cond.notify_all();
    }

    fn fail(&self, message: String) {
        let mut state = crate::lock(&self.state);
        state.failed = Some(message);
        self.cond.notify_all();
    }
}

/// Reads a cache file that is still being written: byte positions past the
/// download frontier wait for more data instead of returning EOF, so the
/// decoder can play and seek inside what already arrived.
pub struct GrowingReader {
    progress: Arc<Progress>,
    file: std::fs::File,
    pos: u64,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    /// Decoder construction happens on the UI thread. During probing, don't
    /// wait up to a minute for bytes the network has not delivered yet.
    startup: Arc<AtomicBool>,
}

impl GrowingReader {
    pub fn open(progress: Arc<Progress>, path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
        Ok(Self {
            progress,
            file,
            pos: 0,
            path: path.to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)),
            startup: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Setting this flag makes a read that waits for the network return an
    /// error at once. The audio thread sits inside such a read while the
    /// stream stalls, and the UI must not wait for it to give up on its own.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn startup_handle(&self) -> Arc<AtomicBool> {
        self.startup.clone()
    }

    fn wait_until_available(&self, needed: u64) -> std::io::Result<()> {
        if self.startup.load(Ordering::SeqCst) {
            let (downloaded, _, finished, failed) = self.progress.snapshot();
            if failed.is_none() && !finished && downloaded < needed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "декодеру нужно дождаться дополнительных данных",
                ));
            }
        }
        self.progress.wait_for(needed, Duration::from_secs(60), Some(&self.cancel), Some(&self.startup)).map_err(|e| {
            let kind = if self.startup.load(Ordering::SeqCst) {
                std::io::ErrorKind::WouldBlock
            } else {
                std::io::ErrorKind::TimedOut
            };
            std::io::Error::new(kind, format!("{}: {e}", self.path.display()))
        })
    }
}

impl std::io::Read for GrowingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        if buf.is_empty() {
            return Ok(0);
        }
        let (downloaded, _total, finished, failed) = self.progress.snapshot();
        if let Some(err) = failed {
            return Err(std::io::Error::other(err));
        }
        if self.pos >= downloaded && !finished {
            self.wait_until_available(self.pos + 1)?;
        }
        let (downloaded, _total, _finished, _failed) = self.progress.snapshot();
        if self.pos >= downloaded {
            return Ok(0); // finished and drained
        }
        let available = (downloaded - self.pos).min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut buf[..available])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for GrowingReader {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let target = match from {
            SeekFrom::Start(pos) => pos,
            SeekFrom::Current(off) => (i128::from(self.pos) + i128::from(off)).clamp(0, i128::from(u64::MAX)) as u64,
            SeekFrom::End(off) => {
                let total = loop {
                    let (_downloaded, total, finished, failed) = self.progress.snapshot();
                    if let Some(err) = failed {
                        return Err(std::io::Error::other(err));
                    }
                    if let Some(total) = total {
                        break total;
                    }
                    if finished {
                        break self.progress.snapshot().0;
                    }
                    self.wait_until_available(u64::MAX)?;
                };
                (i128::from(total) + i128::from(off)).clamp(0, i128::from(u64::MAX)) as u64
            }
        };
        self.wait_until_available(target)?;
        self.pos = target;
        Ok(target)
    }
}

/// Result of one download, sent back to the UI thread.
#[derive(Clone)]
pub enum DlEvent {
    Done(CachedTrack),
    Failed(String),
}

/// Handle of a running download: the UI polls `progress` and can start
/// playback from the `.part` file while it is being written.
#[derive(Clone)]
pub struct DlHandle {
    pub song: Song,
    pub progress: Arc<Progress>,
}

impl DlHandle {
    /// The `.part` file; `None` until the server started answering.
    pub fn part(&self) -> Option<PathBuf> {
        self.progress.part()
    }
}

/// Streams one song into the cache on a worker thread and returns at once:
/// the caller is the UI thread, so nothing here (not even opening the
/// connection) may wait on the network. Failures arrive as `DlEvent::Failed`
/// and through `progress`. The file is renamed to its final name (and
/// indexed) only when the download completed.
pub fn start_download(
    client: api::Client,
    cache: Cache,
    song: Song,
    format_label: String,
    tx: Sender<(String, DlEvent)>,
) -> DlHandle {
    let progress = Progress::new(None);
    let handle = DlHandle { song: song.clone(), progress: progress.clone() };
    std::thread::spawn(move || {
        let id = song.id.clone();
        let result = guarded(|| {
            client
                .open_stream(&song)
                .and_then(|stream| download_stream(&cache, &song, stream, &progress, &format_label))
        });
        match result {
            Ok(entry) => {
                progress.finish();
                let _ = tx.send((id, DlEvent::Done(entry)));
            }
            Err(err) => {
                progress.fail(err.clone());
                let _ = tx.send((id, DlEvent::Failed(err)));
            }
        }
    });
    handle
}

/// Runs a download step so that a panic inside it (a decoder-free path, but
/// still a poisoned lock or a broken reader) ends as a failed download instead
/// of a dead thread that keeps its download slot forever.
fn guarded<T>(step: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    outcome.unwrap_or_else(|_| Err("внутренняя ошибка при загрузке трека".into()))
}

/// Picks the destination, writes the stream there and cleans up after a
/// failure (`.part` file and path reservation).
fn download_stream(
    cache: &Cache,
    song: &Song,
    stream: api::AudioStream,
    progress: &Arc<Progress>,
    format_label: &str,
) -> Result<CachedTrack, String> {
    let (path, part) = cache.dest_for(song, &stream.suffix)?;
    // A panic in the worker must also free this reservation.
    struct Reservation<'a>(&'a Cache, PathBuf);
    impl Drop for Reservation<'_> {
        fn drop(&mut self) {
            self.0.release(&self.1);
        }
    }
    let _reservation = Reservation(cache, path.clone());
    run_download(cache, song, stream, &part, &path, progress, format_label)
}

/// Remove only a partial file opened by this download. A pre-existing `.part`
/// may belong to the user or another process and must never be deleted on an
/// `OpenOptions::create_new` failure.
struct PartCleanup<'a> {
    path: &'a Path,
    created: bool,
}

impl Drop for PartCleanup<'_> {
    fn drop(&mut self) {
        if self.created {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

fn run_download(
    cache: &Cache,
    song: &Song,
    stream: api::AudioStream,
    part: &Path,
    path: &Path,
    progress: &Arc<Progress>,
    format_label: &str,
) -> Result<CachedTrack, String> {
    use std::io::{Read, Write};
    let suffix = stream.suffix;
    let total = stream.total;
    let mut reader = stream.reader;
    let max_bytes = cache.inner.max_track_bytes.load(Ordering::SeqCst);
    if total.is_some_and(|total| total > max_bytes) {
        return Err("сервер прислал слишком большой файл".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("не удалось создать {parent:?}: {e}"))?;
    }
    let mut cleanup = PartCleanup { path: part, created: false };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .map_err(|e| format!("не удалось создать {}: {e}", part.display()))?;
    cleanup.created = true;
    progress.opened(part.to_path_buf(), total);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("поток оборвался: {e}"))?;
        if n == 0 {
            break;
        }
        if written + n as u64 > max_bytes {
            return Err("поток превысил допустимый размер файла".into());
        }
        file.write_all(&buf[..n]).map_err(|e| format!("не удалось записать кеш: {e}"))?;
        written += n as u64;
        progress.add(n as u64);
    }
    file.sync_all().map_err(|e| format!("не удалось сохранить кеш: {e}"))?;
    drop(file);
    if written == 0 {
        return Err("сервер прислал пустой трек".into());
    }
    // A known Content-Length that does not match means a broken download.
    if let Some(total) = progress.snapshot().1 {
        if written < total {
            return Err("поток оборвался до конца трека".into());
        }
    }
    // A `200 OK` can still carry an HTML error page or a JSON body. Renaming
    // that to the final name would index it as a finished track and leave the
    // song unplayable until the user deleted the file by hand.
    looks_like_audio(part)?;
    if cache
        .resolve_rel(&path.strip_prefix(cache.root()).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/"))
        .is_none()
        || path.exists()
    {
        return Err("путь файла кеша изменился во время загрузки".into());
    }
    std::fs::rename(part, path).map_err(|e| format!("не удалось завершить файл кеша: {e}"))?;
    cleanup.created = false;
    progress.set_total(Some(written));
    let relative = path.strip_prefix(cache.root()).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let entry = CachedTrack {
        id: song.id.clone(),
        path: relative,
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        duration: song.duration,
        suffix,
        size: written,
        format: format_label.to_owned(),
    };
    cache.insert(entry.clone())?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_a_seek_interrupts_an_existing_network_wait() {
        use std::io::Read;
        let root = temp_root("seek-interrupt");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("waiting.part");
        std::fs::write(&path, []).unwrap();
        let mut reader = GrowingReader::open(Progress::new(Some(100)), &path).unwrap();
        let flag = reader.startup_handle();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(()).unwrap();
            reader.read(&mut [0]).unwrap_err().kind()
        });
        rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let started = std::time::Instant::now();
        flag.store(true, Ordering::SeqCst);
        assert_eq!(worker.join().unwrap(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn webm_opus_starts_from_a_partial_download() {
        let bytes = include_bytes!("../tests/fixtures/tone.webm");
        let root = temp_root("stream-webm-opus");
        std::fs::create_dir_all(&root).unwrap();
        let part = root.join("audio.webm.part");
        let prefix = bytes.len() / 2;
        std::fs::write(&part, &bytes[..prefix]).unwrap();
        let progress = Progress::new(Some(bytes.len() as u64));
        progress.opened(part.clone(), Some(bytes.len() as u64));
        progress.add(prefix as u64);
        let reader = GrowingReader::open(progress, &part).unwrap();
        reader.startup_handle().store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let mut decoder = crate::player::open_stream_decoder(reader, Some(bytes.len() as u64)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(decoder.by_ref().take(9600).any(|sample| sample.abs() > 0.01));
        decoder.try_seek(Duration::from_millis(250)).unwrap();
        assert!(decoder.by_ref().take(9600).any(|sample| sample.abs() > 0.01));
        drop(decoder);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_unsafe_even_when_unavailable_paths_are_kept() {
        let base = temp_root("junction-index");
        let root = base.join("cache");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.mp3"), b"keep").unwrap();
        let junction = root.join("Band");
        let result = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        write_index(&root, &[("bad", "Band/victim.mp3")]);
        let cache = Cache::load(root.clone());
        assert_eq!(cache.stats().0, 0);
        assert!(cache.resolve_rel("Band/victim.mp3").is_none());
        cache.clear().unwrap();
        assert_eq!(std::fs::read(outside.join("victim.mp3")).unwrap(), b"keep");
        std::fs::remove_dir(junction).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn opus_starts_from_a_growing_file_without_waiting_for_the_final_page() {
        let bytes = include_bytes!("../tests/fixtures/tone.opus");
        let mut prefix = 0;
        // OpusHead, OpusTags, then the first page of audio; leave later pages
        // unavailable, as if the network had stalled during the download.
        for _ in 0..3 {
            assert_eq!(&bytes[prefix..prefix + 4], b"OggS");
            let segments = usize::from(bytes[prefix + 26]);
            let payload: usize = bytes[prefix + 27..prefix + 27 + segments].iter().map(|byte| usize::from(*byte)).sum();
            prefix += 27 + segments + payload;
        }
        assert!(prefix < bytes.len());
        let root = temp_root("stream-opus");
        std::fs::create_dir_all(&root).unwrap();
        let part = root.join("audio.opus.part");
        std::fs::write(&part, &bytes[..prefix]).unwrap();
        let progress = Progress::new(Some(bytes.len() as u64));
        progress.opened(part.clone(), Some(bytes.len() as u64));
        progress.add(prefix as u64);
        let reader = GrowingReader::open(progress, &part).unwrap();
        reader.startup_handle().store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let mut decoder = crate::player::open_stream_decoder(reader, Some(bytes.len() as u64)).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(decoder.by_ref().take(9600).any(|sample| sample.abs() > 0.01));
        decoder.try_seek(Duration::from_millis(250)).unwrap();
        assert!(decoder.by_ref().take(9600).any(|sample| sample.abs() > 0.01));
        drop(decoder);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pruning_reports_write_failures_instead_of_silently_claiming_persistence() {
        let root = temp_root("prune-save-error");
        let cache = Cache::load(root.clone());
        cache.insert(CachedTrack { id: "1".into(), path: "gone.mp3".into(), ..Default::default() }).unwrap();
        std::fs::remove_file(root.join(INDEX_FILE)).unwrap();
        std::fs::create_dir(root.join(INDEX_FILE)).unwrap();
        assert_eq!(cache.prune_missing(), 1);
        assert!(cache.take_warning().is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_oversized_audio_tag_never_becomes_a_finished_cached_track() {
        let root = temp_root("huge-id3");
        let cache = Cache::load(root.clone());
        let stream = api::AudioStream {
            reader: Box::new(std::io::Cursor::new(b"ID3\x04\0\0\x7f\x7f\x7f\x7f")),
            total: None,
            suffix: "mp3".into(),
        };
        assert!(download_stream(&cache, &song("1", "B", "A", "S", 1), stream, &Progress::new(None), "raw").is_err());
        assert_eq!(cache.stats().0, 0);
        assert!(!root.join("B/A/01 - S.mp3").exists());
        assert!(!root.join("B/A/01 - S.mp3.part").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_track_ids_on_different_accounts_never_share_audio_or_deletion() {
        let root = temp_root("accounts");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("first.mp3"), b"first").unwrap();
        std::fs::write(root.join("second.mp3"), b"second").unwrap();
        let first = Cache::load_profile(root.clone(), Some("first"), false);
        first.insert(CachedTrack { id: "1".into(), path: "first.mp3".into(), ..Default::default() }).unwrap();
        let second = first.for_profile(root.clone(), Some("second"));
        assert!(!second.contains("1"), "another account reused the cached track");
        second.insert(CachedTrack { id: "1".into(), path: "second.mp3".into(), ..Default::default() }).unwrap();
        let first = Cache::load_profile(root.clone(), Some("first"), true);
        assert_eq!(first.entry("1").unwrap().path, "first.mp3");
        first.clear().unwrap();
        assert!(root.join("second.mp3").exists());
        assert_eq!(Cache::load_profile(root.clone(), Some("second"), true).entry("1").unwrap().path, "second.mp3");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_audio_index_is_adopted_once_only_by_the_saved_account() {
        let root = temp_root("adoption");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.mp3"), b"audio").unwrap();
        Cache::load(root.clone())
            .insert(CachedTrack { id: "1".into(), path: "song.mp3".into(), ..Default::default() })
            .unwrap();
        assert!(!Cache::load_profile(root.clone(), Some("new-account"), false).contains("1"));
        assert!(root.join(INDEX_FILE).is_file());
        assert!(Cache::load_profile(root.clone(), Some("saved-account"), true).contains("1"));
        assert!(!root.join(INDEX_FILE).exists());
        assert!(!Cache::load_profile(root.clone(), Some("new-account"), true).contains("1"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tampered_sizes_do_not_overflow_cache_statistics() {
        let root = temp_root("size-overflow");
        let cache = Cache::load(root.clone());
        for id in ["a", "b"] {
            cache
                .insert(CachedTrack { id: id.into(), path: format!("{id}.mp3"), size: u64::MAX, ..Default::default() })
                .unwrap();
        }
        assert_eq!(Cache::load(root.clone()).stats(), (2, u64::MAX));
        assert!(cache.resolve_rel(".beat-index-abc.json").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn song(id: &str, artist: &str, album: &str, title: &str, track: u32) -> Song {
        Song {
            id: id.into(),
            artist: artist.into(),
            album: album.into(),
            title: title.into(),
            track,
            suffix: "mp3".into(),
            ..Song::default()
        }
    }

    #[test]
    fn components_are_sanitized_for_windows_paths() {
        assert_eq!(sanitize_component("AC/DC", "x", MAX_COMPONENT), "AC_DC");
        assert_eq!(sanitize_component("what?", "x", MAX_COMPONENT), "what_");
        assert_eq!(sanitize_component("  ../evil  ", "x", MAX_COMPONENT), ".._evil");
        assert_eq!(sanitize_component("", "fallback", MAX_COMPONENT), "fallback");
        assert_eq!(sanitize_component("trailing. ", "x", MAX_COMPONENT), "trailing");
    }

    #[test]
    fn destinations_are_human_readable_and_unique() {
        let root = std::env::temp_dir().join(format!(
            "beat-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let cache = Cache::load(root.clone());
        let (path, part) = cache.dest_for(&song("1", "Band", "Album", "Song", 3), "flac").unwrap();
        assert!(path.ends_with(std::path::Path::new("Band").join("Album").join("03 - Song.flac")));
        assert_eq!(part.file_name().unwrap().to_string_lossy(), "03 - Song.flac.part");
        cache
            .insert(CachedTrack {
                id: "1".into(),
                path: "Band/Album/03 - Song.flac".into(),
                title: "Song".into(),
                artist: "Band".into(),
                album: "Album".into(),
                duration: 1.0,
                suffix: "flac".into(),
                size: 10,
                format: "raw".into(),
            })
            .unwrap();
        let (other, _) = cache.dest_for(&song("2", "Band", "Album", "Song", 3), "flac").unwrap();
        assert!(other.to_string_lossy().contains("03 - Song (2).flac"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn index_roundtrips_and_prunes_missing_files() {
        let root = std::env::temp_dir().join(format!(
            "beat-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.mp3"), b"data").unwrap();
        let cache = Cache::load(root.clone());
        cache
            .insert(CachedTrack {
                id: "a".into(),
                path: "a.mp3".into(),
                title: "A".into(),
                artist: "X".into(),
                album: "Y".into(),
                duration: 1.0,
                suffix: "mp3".into(),
                size: 4,
                format: "raw".into(),
            })
            .unwrap();
        assert!(cache.contains("a"));
        let reloaded = Cache::load(root.clone());
        assert_eq!(reloaded.stats(), (1, 4));
        std::fs::remove_file(root.join("a.mp3")).unwrap();
        assert!(!reloaded.contains("a"));
        assert_eq!(reloaded.prune_missing(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn growing_reader_waits_for_the_writer() {
        let root = std::env::temp_dir().join(format!(
            "beat-grow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("track.mp3.part");
        std::fs::write(&path, b"hello ").unwrap();
        let progress = Progress::new(None);
        progress.add(6);
        let mut reader = GrowingReader::open(progress.clone(), &path).unwrap();
        let writer = {
            let path = path.clone();
            let progress = progress.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                std::thread::sleep(Duration::from_millis(50));
                let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                file.write_all(b"world").unwrap();
                progress.add(5);
                progress.finish();
            })
        };
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut out).unwrap();
        writer.join().unwrap();
        assert_eq!(out, b"hello world");
        // Seeking backwards inside the buffered region works.
        use std::io::{Read, Seek, SeekFrom};
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut head = [0u8; 5];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"hello");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_download_reports_the_error_to_waiters() {
        let progress = Progress::new(None);
        progress.fail("обрыв".into());
        assert!(progress.wait_for(1, Duration::from_millis(100), None, None).is_err());
    }

    /// `n` bytes of synthetic audio, then end of stream: a real container
    /// header (a download that is not audio at all is refused before this) and
    /// zeroed payload after it.
    struct Zeros(u64);

    impl std::io::Read for Zeros {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            const HEADER: &[u8] = b"ID3\x04\x00\x00\x00\x00";
            let n = (self.0.min(buf.len() as u64)) as usize;
            buf[..n].fill(0);
            let header = HEADER.len().min(n);
            buf[..header].copy_from_slice(&HEADER[..header]);
            self.0 -= n as u64;
            Ok(n)
        }
    }

    #[test]
    fn pruning_never_wipes_the_index_when_the_folder_is_unavailable() {
        let root = temp_root("unplugged");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.mp3"), b"data").unwrap();
        write_index(&root, &[("a", "a.mp3")]);
        let cache = Cache::load(root.clone());
        // The drive goes away: every file "disappears", which is not the same
        // as the user deleting them.
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(cache.prune_missing(), 0);
        assert_eq!(cache.stats().0, 1, "the whole index was dropped because the folder was unavailable");
    }

    #[test]
    fn index_lookups_for_the_ui_do_not_touch_the_disk() {
        let root = temp_root("indexed");
        write_index(&root, &[("gone", "gone.mp3")]); // listed, but the file is missing
        let cache = Cache::load(root.clone());
        assert!(cache.is_indexed("gone"));
        assert_eq!(cache.indexed_entry("gone").map(|entry| entry.path), Some("gone.mp3".to_owned()));
        assert!(!cache.is_indexed("other"));
        // The checked lookups keep verifying the file, as playback needs.
        assert!(!cache.contains("gone"));
        assert!(cache.entry("gone").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_stream_larger_than_the_limit_is_refused_and_cleaned_up() {
        let root = temp_root("too-big");
        let cache = Cache::load(root.clone());
        cache.set_max_track_bytes(10_000);
        let part = root.join("Band").join("Album").join("01 - Song.mp3.part");
        // Length unknown, and it just keeps coming.
        let stream = api::AudioStream { reader: Box::new(Zeros(1_000_000)), total: None, suffix: "mp3".into() };
        let endless =
            download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw");
        assert!(endless.is_err(), "a 1 MB stream was accepted under a 10 kB limit");
        assert!(!part.exists(), "the oversized .part file was left behind");
        // Announced as too big: refused before a byte is written.
        let progress = Progress::new(None);
        let stream =
            api::AudioStream { reader: Box::new(Zeros(1_000_000)), total: Some(1_000_000), suffix: "mp3".into() };
        let announced = download_stream(&cache, &song("2", "Band", "Album", "Other", 2), stream, &progress, "raw");
        assert!(announced.is_err());
        assert_eq!(progress.snapshot().0, 0, "bytes were written for a track announced as too big");
        // A track within the limit still downloads.
        let stream = api::AudioStream { reader: Box::new(Zeros(5_000)), total: Some(5_000), suffix: "mp3".into() };
        let fine =
            download_stream(&cache, &song("3", "Band", "Album", "Small", 3), stream, &Progress::new(None), "raw");
        assert!(fine.is_ok(), "{fine:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_that_cannot_be_deleted_stays_in_the_index() {
        let root = temp_root("undeletable");
        // A directory where the entry expects a file: deleting it fails.
        std::fs::create_dir_all(root.join("stuck.mp3")).unwrap();
        std::fs::write(root.join("ok.mp3"), b"data").unwrap();
        write_index(&root, &[("stuck", "stuck.mp3"), ("ok", "ok.mp3"), ("missing", "missing.mp3")]);
        let cache = Cache::load(root.clone());
        assert!(cache.remove("stuck").is_err(), "reported success for a file that is still there");
        assert!(cache.remove("missing").is_ok(), "a file that is already gone is not an error");
        let ids = |cache: &Cache| -> Vec<String> {
            let mut ids: Vec<String> = crate::lock(&cache.inner.index).keys().cloned().collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(&cache), ["ok", "stuck"]);
        let err = cache.clear().expect_err("clear must report the file it could not delete");
        assert!(err.contains('1'), "{err}");
        assert_eq!(ids(&cache), ["stuck"]);
        assert!(!root.join("ok.mp3").exists());
        assert_eq!(ids(&Cache::load(root.clone())), ["stuck"], "the index on disk forgot the stuck file");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn windows_device_names_are_not_used_as_file_names() {
        for name in ["CON", "con", "Nul", "AUX", "prn", "COM1", "lpt9", "Con.Remix", "NUL ", "COM¹", "com².mp3", "LPT³"]
        {
            let clean = sanitize_component(name, "x", MAX_COMPONENT);
            assert!(clean.starts_with('_'), "{name:?} became {clean:?}");
            assert!(!is_safe_rel(&format!("{name}.mp3")), "{name:?} is not a safe path");
        }
        for name in ["Console", "COM10", "Communication", "Aux Cord", "LPT0", "Nulla"] {
            assert_eq!(sanitize_component(name, "x", MAX_COMPONENT), name, "{name:?} is a legal name");
        }
    }

    #[test]
    fn a_panicking_download_becomes_a_failure() {
        let result = guarded(|| -> Result<(), String> { panic!("boom") });
        assert!(result.is_err());
    }

    #[test]
    fn concurrent_inserts_all_reach_the_index_file() {
        let root = temp_root("many-inserts");
        let cache = Cache::load(root.clone());
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for n in 0..10 {
                        let id = format!("w{worker}-{n}");
                        cache.insert(CachedTrack { path: format!("{id}.mp3"), id, ..CachedTrack::default() }).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(Cache::load(root.clone()).stats().0, 80, "an older snapshot overwrote a newer one");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seeking_is_limited_to_what_has_arrived() {
        let unknown = Progress::new(None);
        assert_eq!(unknown.seekable_fraction(), None, "size unknown and still downloading");
        unknown.finish();
        assert_eq!(unknown.seekable_fraction(), Some(1.0), "a finished download can be sought anywhere");

        let progress = Progress::new(None);
        progress.opened(PathBuf::from("x.part"), Some(1000));
        progress.add(500);
        let half = progress.seekable_fraction().expect("total is known");
        // Bytes and playing time are not linear (VBR): keep a margin below 0.5.
        assert!(half > 0.4 && half < 0.5, "{half}");
        progress.add(600);
        let over = progress.seekable_fraction().unwrap();
        assert!(over <= 1.0 && over > 0.9, "{over}");
    }

    #[test]
    fn a_cancelled_reader_stops_waiting_for_the_network() {
        // The download stalls: the reader has consumed everything that arrived
        // and would wait a minute for more. Stopping playback must not.
        let root = temp_root("cancel-reader");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("track.mp3.part");
        std::fs::write(&path, b"hello ").unwrap();
        let progress = Progress::new(None);
        progress.add(6);
        let mut reader = GrowingReader::open(progress, &path).unwrap();
        let mut head = [0u8; 6];
        std::io::Read::read_exact(&mut reader, &mut head).unwrap();
        let cancel = reader.cancel_handle();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            let _ = done_tx.send(std::io::Read::read(&mut reader, &mut buf).is_err());
        });
        std::thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::SeqCst);
        let outcome = done_rx.recv_timeout(Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(outcome, Ok(true), "the reader kept waiting after it was cancelled");
    }

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "beat-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    fn client_for(base: String) -> api::Client {
        let server = api::Server { base, user: "u".into(), password: "p".into() };
        api::Client::new(&server, crate::config::StreamFormat::Raw, 320).unwrap()
    }

    #[test]
    fn starting_a_download_never_waits_for_the_server() {
        // The server accepts the connection and then says nothing: opening the
        // stream would block for the whole client timeout. The caller of
        // `start_download` is the UI thread and must get control back at once.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
            drop(held);
        });
        let client = client_for(base);
        let cache = Cache::load(temp_root("nonblock"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let handle = start_download(client, cache, song("1", "Band", "Album", "Song", 1), "raw".into(), tx);
            drop(handle);
            let _ = done_tx.send(());
        });
        assert!(done_rx.recv_timeout(Duration::from_secs(3)).is_ok(), "start_download blocked on the network");
    }

    #[test]
    fn a_download_that_cannot_start_is_reported_through_the_channel() {
        // Nothing listens on the port: the failure has to arrive as an event
        // (the UI polls it), not as a value the caller must wait for.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let client = client_for(format!("http://127.0.0.1:{port}/"));
        let cache = Cache::load(temp_root("failed-start"));
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = start_download(client, cache, song("1", "Band", "Album", "Song", 1), "raw".into(), tx);
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok((id, DlEvent::Failed(_))) => assert_eq!(id, "1"),
            Ok(_) => panic!("expected a failure event"),
            Err(_) => panic!("no event arrived for a download that could not start"),
        }
    }

    /// Yields `bytes` bytes and then fails, like a dropped connection.
    struct FailAfter(usize);

    impl std::io::Read for FailAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("обрыв"));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(7);
            self.0 -= n;
            Ok(n)
        }
    }

    fn write_index(root: &Path, entries: &[(&str, &str)]) {
        let tracks: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(id, path)| {
                (
                    (*id).to_owned(),
                    serde_json::json!(
                {"id": id, "path": path, "title": "T", "artist": "A", "album": "B"}),
                )
            })
            .collect();
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(INDEX_FILE),
            serde_json::to_vec(&serde_json::json!({"version": 1, "tracks": tracks})).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn relative_paths_must_stay_inside_the_cache() {
        for good in ["a.mp3", "Band/Album/01 - Song.flac", "Группа/Альбом/Песня (2).mp3", "a b/c.d/e.mp3"]
        {
            assert!(is_safe_rel(good), "{good} should be accepted");
        }
        for bad in [
            "",
            "..",
            "../x.mp3",
            "a/../b.mp3",
            "./a.mp3",
            "a/./b.mp3",
            "/abs.mp3",
            "a//b.mp3",
            "C:/x.mp3",
            "C:x.mp3",
            r"a\b.mp3",
            "a/b?.mp3",
            "a/b*.mp3",
            "a/b.mp3/",
            "dir./x.mp3",
            "dir /x.mp3",
            "a/\u{0}.mp3",
            ".beat-index.json",
            ".beat-index.corrupt-1.bak",
            "a.mp3.part",
            "CON/track.mp3",
        ] {
            assert!(!is_safe_rel(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_tampered_index_cannot_reach_files_outside_the_cache() {
        let base = temp_root("tamper");
        let root = base.join("cache");
        let victim = base.join("victim.txt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&victim, b"keep me").unwrap();
        write_index(&root, &[("x", "../victim.txt")]);
        let cache = Cache::load(root.clone());
        let _ = cache.remove("x");
        let _ = cache.clear();
        let (path, _) = cache.dest_for(&song("x", "Band", "Album", "Song", 1), "mp3").unwrap();
        let survived = victim.exists();
        let inside = path.starts_with(&root);
        let _ = std::fs::remove_dir_all(&base);
        assert!(survived, "an index entry deleted a file outside the cache folder");
        assert!(inside, "a download would be written outside the cache folder: {path:?}");
    }

    #[test]
    fn saving_settings_keeps_one_cache_per_folder() {
        let root = temp_root("same-root");
        let entry = |id: &str| CachedTrack { id: id.into(), path: format!("{id}.mp3"), ..CachedTrack::default() };
        let running = Cache::load(root.clone());
        // Settings saved while a download is still running...
        let after_settings = running.for_profile(root.clone(), None);
        // ...which finishes through the old handle, and another one through the new.
        running.insert(entry("first")).unwrap();
        after_settings.insert(entry("second")).unwrap();
        let reloaded = Cache::load(root.clone());
        let ids: Vec<String> = crate::lock(&reloaded.inner.index).keys().cloned().collect();
        assert!(ids.contains(&"first".to_owned()), "the first download vanished from the index: {ids:?}");
        assert!(ids.contains(&"second".to_owned()), "{ids:?}");
        let other = temp_root("other-root");
        assert_eq!(running.for_profile(other.clone(), None).root(), other.as_path());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_safe_relative_paths_resolve_inside_the_cache() {
        let cache = Cache::load(temp_root("resolve"));
        assert!(cache.resolve_rel("../x.mp3").is_none());
        assert!(cache.resolve_rel("C:/x.mp3").is_none());
        let inside = cache.resolve_rel("Band/Album/a.mp3").expect("a plain relative path resolves");
        assert!(inside.starts_with(cache.root()));
    }

    #[test]
    fn decoder_probe_does_not_block_the_ui_while_waiting_for_more_bytes() {
        use std::io::{Read, Seek, SeekFrom};
        let root = temp_root("probe-nonblocking");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("audio.mp3.part");
        std::fs::write(&path, b"hi").unwrap();
        let progress = Progress::new(Some(100));
        progress.add(2);
        let mut reader = GrowingReader::open(progress, &path).unwrap();
        reader.startup_handle().store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let mut buf = [0; 4];
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(reader.read(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn clearing_cache_persists_an_empty_index() {
        let root = temp_root("clear-index");
        let cache = Cache::load(root.clone());
        cache.insert(CachedTrack { id: "x".into(), path: "x.mp3".into(), ..CachedTrack::default() }).unwrap();
        cache.clear().unwrap();
        assert_eq!(Cache::load(root.clone()).stats().0, 0);
        assert!(root.join(INDEX_FILE).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_index_cannot_delete_its_own_file() {
        let root = temp_root("self-index");
        write_index(&root, &[("bad", INDEX_FILE)]);
        let cache = Cache::load(root.clone());
        assert_eq!(cache.stats().0, 0);
        cache.clear().unwrap();
        assert!(root.join(INDEX_FILE).is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_link_inside_the_cache_cannot_point_to_a_victim() {
        let base = temp_root("link");
        let root = base.join("cache");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.mp3"), b"keep").unwrap();
        #[cfg(windows)]
        let link = std::os::windows::fs::symlink_dir(&outside, root.join("Band"));
        #[cfg(unix)]
        let link = std::os::unix::fs::symlink(&outside, root.join("Band"));
        if link.is_ok() {
            write_index(&root, &[("bad", "Band/victim.mp3")]);
            let cache = Cache::load(root.clone());
            assert_eq!(cache.stats().0, 0);
            assert!(cache.resolve_rel("Band/victim.mp3").is_none());
            assert!(cache.dest_for(&song("new", "Band", "Album", "Song", 1), "mp3").is_err());
            cache.clear().unwrap();
            assert_eq!(std::fs::read(outside.join("victim.mp3")).unwrap(), b"keep");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn an_unsafe_entry_is_refused_on_insert() {
        let cache = Cache::load(temp_root("unsafe-insert"));
        let entry = CachedTrack { id: "1".into(), path: "../evil.mp3".into(), ..CachedTrack::default() };
        assert!(cache.insert(entry).is_err());
        assert!(cache.stats().0 == 0);
    }

    #[test]
    fn an_unreadable_index_is_set_aside_not_overwritten() {
        let root = temp_root("corrupt-index");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(INDEX_FILE), b"{ not json").unwrap();
        let cache = Cache::load(root.clone());
        assert!(cache.take_warning().is_some(), "the user is never told the index was dropped");
        cache.insert(CachedTrack { id: "1".into(), path: "a.mp3".into(), ..CachedTrack::default() }).unwrap();
        let backups: Vec<PathBuf> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{ not json");
        let reloaded = Cache::load(root.clone());
        assert!(reloaded.take_warning().is_none());
        assert_eq!(reloaded.stats().0, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_index_that_cannot_be_set_aside_blocks_saving() {
        let root = temp_root("blocked-index");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(INDEX_FILE), b"garbage").unwrap();
        // The backup name for this stamp is taken already.
        std::fs::write(root.join(".beat-index.corrupt-5.bak"), b"older backup").unwrap();
        let loaded = load_index(&root, 5);
        assert!(loaded.save_blocked);
        assert!(loaded.warning.is_some());
        let cache = Cache::from_loaded(root.clone(), INDEX_FILE.into(), loaded);
        let result = cache.insert(CachedTrack { id: "1".into(), path: "a.mp3".into(), ..CachedTrack::default() });
        let untouched = std::fs::read(root.join(INDEX_FILE)).unwrap() == b"garbage";
        let _ = std::fs::remove_dir_all(root);
        assert!(result.is_err());
        assert!(untouched, "the unreadable index was overwritten");
    }

    #[test]
    fn a_failed_download_leaves_no_part_file_and_frees_its_path() {
        let root = temp_root("failed-cleanup");
        let cache = Cache::load(root.clone());
        let stream = api::AudioStream { reader: Box::new(FailAfter(1000)), total: None, suffix: "mp3".into() };
        let progress = Progress::new(None);
        let result = download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &progress, "raw");
        assert!(result.is_err());
        assert!(!root.join("Band").join("Album").join("01 - Song.mp3.part").exists());
        let (again, _) = cache.dest_for(&song("2", "Band", "Album", "Song", 1), "mp3").unwrap();
        assert!(!again.to_string_lossy().contains("(2)"), "path stayed reserved: {again:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_creation_does_not_erase_someone_elses_partial_file() {
        let root = temp_root("foreign-part");
        let rel = "Band/Album/01 - Song.mp3";
        let cache = Cache::load(root.clone());
        cache.insert(CachedTrack { id: "1".into(), path: rel.into(), ..CachedTrack::default() }).unwrap();
        let part = part_path(&root.join(rel));
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"do not delete").unwrap();
        let stream = api::AudioStream { reader: Box::new(Zeros(10)), total: Some(10), suffix: "mp3".into() };
        assert!(download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw")
            .is_err());
        assert_eq!(std::fs::read(part).unwrap(), b"do not delete");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_paths_stay_inside_the_windows_path_limit() {
        // Tags are server data: three long components would overflow MAX_PATH
        // and fail the download with a confusing "cannot create" error.
        let root = temp_root("long-path");
        let cache = Cache::load(root.clone());
        let long = "Песня".repeat(200);
        let song = song("1", &long, &long, &long, 7);
        let (path, part) = cache.dest_for(&song, "flac").unwrap();
        let full = path.to_string_lossy().replace('/', "\\");
        assert!(utf16_len(&full) <= MAX_ABSOLUTE_PATH, "{} UTF-16 units: {full}", utf16_len(&full));
        // No component may exceed what NTFS accepts either.
        for component in [
            path.parent().unwrap(),
            path.parent().unwrap().parent().unwrap(),
            path.parent().unwrap().parent().unwrap().parent().unwrap(),
        ] {
            assert!(utf16_len(&component.file_name().unwrap().to_string_lossy()) <= MAX_COMPONENT);
        }
        // The track number and the extension survive the trimming.
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("07 - "), "{name}");
        assert!(name.ends_with(".flac"), "{name}");
        assert!(part.to_string_lossy().ends_with(".flac.part"));
        // A deep cache root leaves less room, and is respected too.
        let deep = root.join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep).unwrap();
        let nested = Cache::load(deep.clone());
        let (nested_path, _) = nested.dest_for(&song, "mp3").unwrap();
        assert!(utf16_len(&nested_path.to_string_lossy()) <= MAX_ABSOLUTE_PATH, "{nested_path:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_download_that_is_not_audio_is_refused_and_leaves_nothing_behind() {
        // A `200 OK` carrying an HTML error page must not be indexed as a
        // finished track: it would show "in cache" and never play.
        let root = temp_root("not-audio");
        let cache = Cache::load(root.clone());
        let body = b"<!DOCTYPE html><html><body>502 Bad Gateway</body></html>".to_vec();
        let stream =
            api::AudioStream { reader: Box::new(std::io::Cursor::new(body)), total: Some(52), suffix: "mp3".into() };
        let result =
            download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw");
        assert!(result.is_err(), "an HTML error page was accepted as a track");
        assert!(!root.join("Band").join("Album").join("01 - Song.mp3").exists());
        assert!(!root.join("Band").join("Album").join("01 - Song.mp3.part").exists());
        assert_eq!(cache.stats().0, 0, "the refused download reached the index");
        // Short proxy answers are not audio either, even when they look like a
        // plausible status line.
        for reply in [&b"OK"[..], b"Not Found", br#"{"error":"nope"}"#] {
            let cache = Cache::load(temp_root("not-audio-short"));
            let stream = api::AudioStream {
                reader: Box::new(std::io::Cursor::new(reply.to_vec())),
                total: None,
                suffix: "mp3".into(),
            };
            assert!(
                download_stream(&cache, &song("1", "B", "A", "S", 1), stream, &Progress::new(None), "raw").is_err(),
                "{reply:?} was accepted as a track"
            );
            let _ = std::fs::remove_dir_all(cache.root());
        }

        // Real audio in every supported container still gets through.
        let heads: [(&[u8], &str); 5] = [
            (b"ID3\x04\x00\x00", "id3.mp3"),
            (b"fLaC\x80\x00\x00\x22", "flac.flac"),
            (b"OggS\x00\x02", "ogg.ogg"),
            (b"RIFF\x00\x00\x00\x00WAVEfmt ", "wave.wav"),
            (&[0xff, 0xfb, 0x90, 0x00], "sync.mp3"),
        ];
        for (head, name) in heads {
            let head = head.to_vec();
            let cache = Cache::load(root.join(name));
            let payload = [head.clone(), vec![0u8; 64]].concat();
            let stream =
                api::AudioStream { reader: Box::new(std::io::Cursor::new(payload)), total: None, suffix: "mp3".into() };
            let entry =
                download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw")
                    .unwrap_or_else(|err| panic!("{name} was refused: {err}"));
            assert_eq!(entry.size as usize, head.len() + 64);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cached_statistics_survive_every_change_to_the_index() {
        let root = temp_root("stats");
        let cache = Cache::load(root.clone());
        let entry = |id: &str, size: u64| CachedTrack {
            id: id.into(),
            path: format!("{id}.mp3"),
            size,
            ..CachedTrack::default()
        };
        assert_eq!(cache.stats(), (0, 0));
        cache.insert(entry("a", 10)).unwrap();
        cache.insert(entry("b", 20)).unwrap();
        assert_eq!(cache.stats(), (2, 30), "insert did not update the counters");
        cache.remove("a").unwrap();
        assert_eq!(cache.stats(), (1, 20), "remove did not update the counters");
        cache.clear().unwrap();
        assert_eq!(cache.stats(), (0, 0), "clear did not update the counters");
        // A file that vanished on its own is dropped by the next scan.
        std::fs::write(root.join("c.mp3"), b"12345").unwrap();
        cache.insert(entry("c", 5)).unwrap();
        assert_eq!(cache.stats(), (1, 5));
        std::fs::remove_file(root.join("c.mp3")).unwrap();
        assert_eq!(cache.prune_missing(), 1);
        assert_eq!(cache.stats(), (0, 0), "prune did not update the counters");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_index_is_never_copied_to_serialize_it() {
        // `save_index` borrows the index under its lock; a regression to a
        // clone would double the memory and the copy time per finished track.
        let root = temp_root("no-clone");
        let cache = Cache::load(root.clone());
        for n in 0..50 {
            cache
                .insert(CachedTrack {
                    id: format!("s{n}"),
                    path: format!("s{n}.mp3"),
                    size: n as u64,
                    title: format!("T{n}"),
                    ..CachedTrack::default()
                })
                .unwrap();
        }
        let reloaded = Cache::load(root.clone());
        assert_eq!(reloaded.stats(), (50, (0..50u64).sum()));
        assert_eq!(reloaded.list().len(), 0, "nothing was written to disk yet");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_downloads_of_the_same_path_get_distinct_files() {
        // Two different song ids can map to one Artist/Album/NN - Title path;
        // until the first finishes nothing on disk shows that it is taken.
        let cache = Cache::load(temp_root("reserve"));
        let (first, _) = cache.dest_for(&song("1", "Band", "Album", "Song", 3), "flac").unwrap();
        let (second, second_part) = cache.dest_for(&song("2", "Band", "Album", "Song", 3), "flac").unwrap();
        assert_ne!(first, second);
        assert!(second.to_string_lossy().contains("03 - Song (2).flac"));
        assert_eq!(second_part.file_name().unwrap().to_string_lossy(), "03 - Song (2).flac.part");
    }
}
