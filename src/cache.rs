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

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct CachedTrack {
    pub id: String,
    /// Relative path inside the cache root, `/`-separated.
    pub path: String,
    pub title: String,
    pub artist: String,
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

#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    index: Mutex<HashMap<String, CachedTrack>>,
    /// Final paths (lowercase, `/`-separated, relative) claimed by running
    /// downloads: nothing on disk marks them as taken until the download ends.
    reserved: Mutex<HashSet<String>>,
    /// Serialises index writes (see `save_index`).
    save_lock: Mutex<()>,
    /// Longest file a download may write.
    max_track_bytes: AtomicU64,
    /// Why the index started empty (unreadable/oversized file), for the UI.
    warning: Option<String>,
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
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let loaded = load_index(&root, stamp);
        Self::from_loaded(root, loaded)
    }

    fn from_loaded(root: PathBuf, loaded: LoadedIndex) -> Cache {
        Cache { inner: Arc::new(Inner {
            root,
            index: Mutex::new(loaded.tracks),
            reserved: Mutex::new(HashSet::new()),
            save_lock: Mutex::new(()),
            max_track_bytes: AtomicU64::new(MAX_TRACK_BYTES),
            warning: loaded.warning,
            save_blocked: loaded.save_blocked,
        }) }
    }

    /// The cache to use for `root` after a settings change: this very instance
    /// when the folder did not change (downloads still running write through
    /// it, and two instances over one folder overwrite each other's index).
    pub fn for_root(&self, root: PathBuf) -> Cache {
        if root == self.inner.root { self.clone() } else { Cache::load(root) }
    }

    #[cfg(test)]
    fn set_max_track_bytes(&self, bytes: u64) {
        self.inner.max_track_bytes.store(bytes, Ordering::SeqCst);
    }

    /// A problem found while loading the index, worth showing once.
    pub fn warning(&self) -> Option<String> {
        self.inner.warning.clone()
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn entry(&self, id: &str) -> Option<CachedTrack> {
        let entry = self.inner.index.lock().unwrap().get(id).cloned()?;
        self.resolve_rel(&entry.path)?.is_file().then_some(entry)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entry(id).is_some()
    }

    /// The index entry for `id`, without checking that its file still exists.
    /// For code that runs every frame: `entry` touches the disk, and a stale
    /// entry is dropped by the next scan or refresh anyway.
    pub fn indexed_entry(&self, id: &str) -> Option<CachedTrack> {
        self.inner.index.lock().unwrap().get(id).cloned()
    }

    pub fn is_indexed(&self, id: &str) -> bool {
        self.indexed_entry(id).is_some()
    }

    pub fn absolute(&self, entry: &CachedTrack) -> PathBuf {
        self.absolute_rel(&entry.path)
    }

    /// Resolves a `/`-separated relative path against the cache root; local
    /// file ids carry their path this way.
    pub fn absolute_rel(&self, rel: &str) -> PathBuf {
        rel.split('/').fold(self.inner.root.clone(), |path, part| path.join(part))
    }

    /// `absolute_rel` for a path that came from outside (a `local:` id, a
    /// cover key): `None` unless it stays inside the cache folder.
    pub fn resolve_rel(&self, rel: &str) -> Option<PathBuf> {
        safe_path(&self.inner.root, rel)
    }

    /// Deterministic destination for a song; collisions with another track's
    /// file (or with a download still running) get a ` (2)` suffix. Returns
    /// the final path and the `.part` path. The path stays reserved until
    /// `release` is called with it.
    pub fn dest_for(&self, song: &Song, suffix: &str) -> Result<(PathBuf, PathBuf), String> {
        let index = self.inner.index.lock().unwrap();
        let mut reserved = self.inner.reserved.lock().unwrap();
        if let Some(entry) = index.get(&song.id) {
            let path = self.resolve_rel(&entry.path)
                .ok_or("путь трека в индексе кеша небезопасен")?;
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
        let rel: String = relative_path(song, suffix).to_string_lossy().replace('\\', "/");
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
        let path = self.resolve_rel(&candidate)
            .ok_or("папка назначения кеша содержит ссылку или небезопасный путь")?;
        reserved.insert(candidate.to_lowercase());
        let part = part_path(&path);
        Ok((path, part))
    }

    /// Gives back a path claimed by `dest_for` once its download ended
    /// (finished or failed).
    pub fn release(&self, path: &Path) {
        if let Ok(rel) = path.strip_prefix(&self.inner.root) {
            let key = rel.to_string_lossy().replace('\\', "/").to_lowercase();
            self.inner.reserved.lock().unwrap().remove(&key);
        }
    }

    /// Records a finished download; saves the index immediately.
    pub fn insert(&self, entry: CachedTrack) -> Result<(), String> {
        if self.resolve_rel(&entry.path).is_none() {
            return Err("путь трека выходит за пределы папки кеша".into());
        }
        {
            let mut index = self.inner.index.lock().unwrap();
            index.insert(entry.id.clone(), entry);
        }
        self.save_index()
    }

    /// Deletes an indexed file; one that is already gone counts as deleted.
    fn delete_file(&self, entry: &CachedTrack) -> std::io::Result<()> {
        let path = self.resolve_rel(&entry.path)
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
            let mut index = self.inner.index.lock().unwrap();
            index.remove(id)
        };
        if let Some(entry) = entry {
            if let Err(err) = self.delete_file(&entry) {
                self.inner.index.lock().unwrap().insert(entry.id.clone(), entry);
                return Err(format!("не удалось удалить файл: {err}"));
            }
        }
        self.save_index()
    }

    /// Deletes every indexed file and the index. Files that cannot be deleted
    /// stay indexed and are reported.
    pub fn clear(&self) -> Result<(), String> {
        let entries: Vec<CachedTrack> = {
            let mut index = self.inner.index.lock().unwrap();
            index.drain().map(|(_, entry)| entry).collect()
        };
        let stuck: Vec<CachedTrack> = entries.into_iter()
            .filter(|entry| self.delete_file(entry).is_err()).collect();
        let count = stuck.len();
        {
            let mut index = self.inner.index.lock().unwrap();
            for entry in stuck {
                index.insert(entry.id.clone(), entry);
            }
        }
        self.save_index()?;
        if count == 0 { Ok(()) } else { Err(format!("не удалось удалить файлов: {count}")) }
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
        let snapshot: Vec<(String, String)> = self.inner.index.lock().unwrap()
            .values().map(|entry| (entry.id.clone(), entry.path.clone())).collect();
        let gone: Vec<(String, String)> = snapshot.into_iter()
            .filter(|(_, path)| self.resolve_rel(path).is_none_or(|path| !path.is_file())).collect();
        let removed = {
            let mut index = self.inner.index.lock().unwrap();
            // Only entries still describing the same file: a download may have
            // replaced one meanwhile.
            gone.iter().filter(|(id, path)| {
                index.get(id).is_some_and(|entry| &entry.path == path) && index.remove(id).is_some()
            }).count()
        };
        if removed > 0 {
            let _ = self.save_index();
        }
        removed
    }

    pub fn stats(&self) -> (usize, u64) {
        let index = self.inner.index.lock().unwrap();
        (index.len(), index.values().map(|entry| entry.size).sum())
    }

    /// All indexed entries whose file still exists, artist/album/track sorted.
    pub fn list(&self) -> Vec<CachedTrack> {
        let all: Vec<CachedTrack> = self.inner.index.lock().unwrap().values().cloned().collect();
        let mut entries: Vec<CachedTrack> = all.into_iter()
            .filter(|entry| self.resolve_rel(&entry.path).is_some_and(|path| path.is_file())).collect();
        entries.sort_by(|a, b| a.artist.to_lowercase().cmp(&b.artist.to_lowercase())
            .then_with(|| a.album.to_lowercase().cmp(&b.album.to_lowercase()))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())));
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
        let _writer = self.inner.save_lock.lock().unwrap();
        let file = {
            let index = self.inner.index.lock().unwrap();
            IndexFile { version: 1, tracks: index.clone() }
        };
        let raw = serde_json::to_vec(&file).map_err(|e| e.to_string())?;
        if raw.len() as u64 > MAX_INDEX_BYTES {
            return Err("индекс кеша слишком большой".into());
        }
        std::fs::create_dir_all(&self.inner.root)
            .map_err(|e| format!("не удалось создать кеш: {e}"))?;
        atomic_write(&self.inner.root.join(INDEX_FILE), &raw)
    }
}

/// Reads the index. A missing file is a fresh cache; an unreadable, oversized
/// or damaged one is set aside (never silently replaced by the next save), and
/// entries whose path leaves the cache folder are dropped: the index sits in a
/// folder that may be synced or shared, and its paths are used for deletion.
fn load_index(root: &Path, stamp: u128) -> LoadedIndex {
    let path = root.join(INDEX_FILE);
    let parsed = match read_capped(&path, MAX_INDEX_BYTES) {
        Ok(None) => return LoadedIndex { tracks: HashMap::new(), warning: None, save_blocked: false },
        Ok(Some(raw)) => serde_json::from_slice::<IndexFile>(&raw).map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match parsed {
        Ok(file) => {
            let total = file.tracks.len();
            let tracks: HashMap<String, CachedTrack> = file.tracks.into_iter()
                .filter(|(id, entry)| id == &entry.id && safe_path(root, &entry.path).is_some()).collect();
            let dropped = total - tracks.len();
            let warning = (dropped > 0)
                .then(|| format!("в индексе кеша пропущено записей с небезопасным путём: {dropped}"));
            LoadedIndex { tracks, warning, save_blocked: false }
        }
        Err(_) => match set_aside(&path, stamp) {
            Some(name) => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some(format!("индекс кеша повреждён и начат заново; старый файл сохранён как {name}")),
                save_blocked: false,
            },
            None => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some("индекс кеша повреждён; копию сделать не удалось, сохранение индекса заблокировано".into()),
                save_blocked: true,
            },
        },
    }
}

/// Renames an unreadable index to `.beat-index.corrupt-<stamp>.bak`; `None`
/// when that failed or the name is taken.
fn set_aside(path: &Path, stamp: u128) -> Option<String> {
    let backup = path.with_file_name(format!(".beat-index.corrupt-{stamp}.bak"));
    if backup.exists() { return None; }
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
    !rel.is_empty() && rel.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !is_device_name(part)
            && !part.to_ascii_lowercase().starts_with(".beat-index.")
            && !part.to_ascii_lowercase().ends_with(".part")
            && !part.ends_with(['.', ' '])
            && !part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
    })
}

/// Refuse existing symlinks and Windows junctions at every level: a lexical
/// path inside the cache can otherwise resolve outside it when read or deleted.
fn safe_path(root: &Path, rel: &str) -> Option<PathBuf> {
    if !is_safe_rel(rel) { return None; }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() { return None; }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 { return None; } // FILE_ATTRIBUTE_REPARSE_POINT
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(path)
}

pub fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(".part");
    path.with_file_name(name)
}

/// `Artist/Album/NN - Title.ext`, sanitized for Windows file names.
fn relative_path(song: &Song, suffix: &str) -> PathBuf {
    let artist = sanitize_component(&song.artist, "Неизвестный артист");
    let album = sanitize_component(&song.album, "Без альбома");
    let title = sanitize_component(&song.title, "Трек");
    let track = if song.track > 0 { format!("{:02} - {title}", song.track) } else { title };
    PathBuf::from(artist).join(album).join(format!("{track}.{suffix}"))
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

pub fn sanitize_component(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
        if out.chars().count() >= 90 {
            break;
        }
    }
    let mut out = out.trim().trim_end_matches(['.', ' ']).to_owned();
    if is_device_name(&out) {
        out.insert(0, '_');
    }
    if out.is_empty() { fallback.to_owned() } else { out }
}

/// Windows reserves these names (with any extension) for devices; a file or
/// folder called like that cannot be created.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| ('1'..='9').contains(&c)))
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
        Arc::new(Self {
            state: Mutex::new(ProgressState { total, ..ProgressState::default() }),
            cond: Condvar::new(),
        })
    }

    /// (downloaded, total, finished, failed)
    pub fn snapshot(&self) -> (u64, Option<u64>, bool, Option<String>) {
        let state = self.state.lock().unwrap();
        (state.downloaded, state.total, state.finished, state.failed.clone())
    }

    pub fn ratio(&self) -> Option<f32> {
        let state = self.state.lock().unwrap();
        state.total.filter(|total| *total > 0)
            .map(|total| (state.downloaded as f32 / total as f32).min(1.0))
    }

    /// Blocks until at least `needed` bytes are downloaded, the download is
    /// finished, or it failed. `Ok(())` also means "finished with less". A
    /// waiter that must be interruptible passes a `cancel` flag.
    pub fn wait_for(&self, needed: u64, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
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
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err("сервер слишком медленно отдаёт трек".into());
            }
            // The flag is not tied to the condvar: look at it regularly.
            let mut wait = deadline - now;
            if cancel.is_some() {
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
        let state = self.state.lock().unwrap();
        if state.finished {
            return Some(1.0);
        }
        let total = state.total.filter(|total| *total > 0)?;
        Some((state.downloaded as f32 / total as f32 - SEEK_MARGIN).clamp(0.0, 1.0))
    }

    /// The `.part` file being written, once the stream has been opened.
    pub fn part(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().part.clone()
    }

    /// The stream is open and its `.part` file exists.
    fn opened(&self, part: PathBuf, total: Option<u64>) {
        let mut state = self.state.lock().unwrap();
        state.part = Some(part);
        if total.is_some() {
            state.total = total;
        }
        self.cond.notify_all();
    }

    fn set_total(&self, total: Option<u64>) {
        if let Some(total) = total {
            self.state.lock().unwrap().total = Some(total);
        }
    }

    fn add(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        state.downloaded = state.downloaded.saturating_add(bytes);
        self.cond.notify_all();
    }

    fn finish(&self) {
        let mut state = self.state.lock().unwrap();
        state.finished = true;
        self.cond.notify_all();
    }

    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap();
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
        let file = std::fs::File::open(path)
            .map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
        Ok(Self { progress, file, pos: 0, path: path.to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)), startup: Arc::new(AtomicBool::new(false)) })
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
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock,
                    "декодеру нужно дождаться дополнительных данных"));
            }
        }
        self.progress
            .wait_for(needed, Duration::from_secs(60), Some(&self.cancel))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut,
                format!("{}: {e}", self.path.display())))
    }
}

impl std::io::Read for GrowingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        if buf.is_empty() { return Ok(0); }
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
        let result = guarded(|| client.open_stream(&song)
            .and_then(|stream| download_stream(&cache, &song, stream, &progress, &format_label)));
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
        fn drop(&mut self) { self.0.release(&self.1); }
    }
    let _reservation = Reservation(cache, path.clone());
    run_download(cache, song, stream, &part, &path, progress, format_label)
}

/// Remove only a partial file opened by this download. A pre-existing `.part`
/// may belong to the user or another process and must never be deleted on an
/// `OpenOptions::create_new` failure.
struct PartCleanup<'a> { path: &'a Path, created: bool }

impl Drop for PartCleanup<'_> {
    fn drop(&mut self) {
        if self.created { let _ = std::fs::remove_file(self.path); }
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
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(part)
        .map_err(|e| format!("не удалось создать {}: {e}", part.display()))?;
    cleanup.created = true;
    progress.opened(part.to_path_buf(), total);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("поток оборвался: {e}"))?;
        if n == 0 { break; }
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
    if cache.resolve_rel(&path.strip_prefix(cache.root()).map_err(|e| e.to_string())?
        .to_string_lossy().replace('\\', "/")).is_none() || path.exists() {
        return Err("путь файла кеша изменился во время загрузки".into());
    }
    std::fs::rename(part, path).map_err(|e| format!("не удалось завершить файл кеша: {e}"))?;
    cleanup.created = false;
    progress.set_total(Some(written));
    let relative = path.strip_prefix(cache.root()).unwrap_or(path)
        .to_string_lossy().replace('\\', "/");
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

    fn song(id: &str, artist: &str, album: &str, title: &str, track: u32) -> Song {
        Song { id: id.into(), artist: artist.into(), album: album.into(), title: title.into(),
            track, suffix: "mp3".into(), ..Song::default() }
    }

    #[test]
    fn components_are_sanitized_for_windows_paths() {
        assert_eq!(sanitize_component("AC/DC", "x"), "AC_DC");
        assert_eq!(sanitize_component("what?", "x"), "what_");
        assert_eq!(sanitize_component("  ../evil  ", "x"), ".._evil");
        assert_eq!(sanitize_component("", "fallback"), "fallback");
        assert_eq!(sanitize_component("trailing. ", "x"), "trailing");
    }

    #[test]
    fn destinations_are_human_readable_and_unique() {
        let root = std::env::temp_dir().join(format!("beat-cache-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let cache = Cache::load(root.clone());
        let (path, part) = cache.dest_for(&song("1", "Band", "Album", "Song", 3), "flac").unwrap();
        assert!(path.ends_with(std::path::Path::new("Band").join("Album").join("03 - Song.flac")));
        assert_eq!(part.file_name().unwrap().to_string_lossy(), "03 - Song.flac.part");
        cache.insert(CachedTrack { id: "1".into(), path: "Band/Album/03 - Song.flac".into(),
            title: "Song".into(), artist: "Band".into(), album: "Album".into(),
            duration: 1.0, suffix: "flac".into(), size: 10, format: "raw".into() }).unwrap();
        let (other, _) = cache.dest_for(&song("2", "Band", "Album", "Song", 3), "flac").unwrap();
        assert!(other.to_string_lossy().contains("03 - Song (2).flac"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn index_roundtrips_and_prunes_missing_files() {
        let root = std::env::temp_dir().join(format!("beat-index-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.mp3"), b"data").unwrap();
        let cache = Cache::load(root.clone());
        cache.insert(CachedTrack { id: "a".into(), path: "a.mp3".into(), title: "A".into(),
            artist: "X".into(), album: "Y".into(), duration: 1.0, suffix: "mp3".into(),
            size: 4, format: "raw".into() }).unwrap();
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
        let root = std::env::temp_dir().join(format!("beat-grow-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
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
        assert!(progress.wait_for(1, Duration::from_millis(100), None).is_err());
    }

    /// `n` zero bytes, then end of stream.
    struct Zeros(u64);

    impl std::io::Read for Zeros {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = (self.0.min(buf.len() as u64)) as usize;
            buf[..n].fill(0);
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
        let endless = download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw");
        assert!(endless.is_err(), "a 1 MB stream was accepted under a 10 kB limit");
        assert!(!part.exists(), "the oversized .part file was left behind");
        // Announced as too big: refused before a byte is written.
        let progress = Progress::new(None);
        let stream = api::AudioStream { reader: Box::new(Zeros(1_000_000)), total: Some(1_000_000), suffix: "mp3".into() };
        let announced = download_stream(&cache, &song("2", "Band", "Album", "Other", 2), stream, &progress, "raw");
        assert!(announced.is_err());
        assert_eq!(progress.snapshot().0, 0, "bytes were written for a track announced as too big");
        // A track within the limit still downloads.
        let stream = api::AudioStream { reader: Box::new(Zeros(5_000)), total: Some(5_000), suffix: "mp3".into() };
        let fine = download_stream(&cache, &song("3", "Band", "Album", "Small", 3), stream, &Progress::new(None), "raw");
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
            let mut ids: Vec<String> = cache.inner.index.lock().unwrap().keys().cloned().collect();
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
        for name in ["CON", "con", "Nul", "AUX", "prn", "COM1", "lpt9", "Con.Remix", "NUL "] {
            let clean = sanitize_component(name, "x");
            assert!(clean.starts_with('_'), "{name:?} became {clean:?}");
        }
        for name in ["Console", "COM10", "Communication", "Aux Cord", "LPT0", "Nulla"] {
            assert_eq!(sanitize_component(name, "x"), name, "{name:?} is a legal name");
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
        let workers: Vec<_> = (0..8).map(|worker| {
            let cache = cache.clone();
            std::thread::spawn(move || {
                for n in 0..10 {
                    let id = format!("w{worker}-{n}");
                    cache.insert(CachedTrack { path: format!("{id}.mp3"), id, ..CachedTrack::default() }).unwrap();
                }
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
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
        std::env::temp_dir().join(format!("beat-{tag}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
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
        assert!(done_rx.recv_timeout(Duration::from_secs(3)).is_ok(),
            "start_download blocked on the network");
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
        let tracks: serde_json::Map<String, serde_json::Value> = entries.iter()
            .map(|(id, path)| ((*id).to_owned(), serde_json::json!(
                {"id": id, "path": path, "title": "T", "artist": "A", "album": "B"})))
            .collect();
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join(INDEX_FILE),
            serde_json::to_vec(&serde_json::json!({"version": 1, "tracks": tracks})).unwrap()).unwrap();
    }

    #[test]
    fn relative_paths_must_stay_inside_the_cache() {
        for good in ["a.mp3", "Band/Album/01 - Song.flac", "Группа/Альбом/Песня (2).mp3", "a b/c.d/e.mp3"] {
            assert!(is_safe_rel(good), "{good} should be accepted");
        }
        for bad in ["", "..", "../x.mp3", "a/../b.mp3", "./a.mp3", "a/./b.mp3", "/abs.mp3", "a//b.mp3",
            "C:/x.mp3", "C:x.mp3", r"a\b.mp3", "a/b?.mp3", "a/b*.mp3", "a/b.mp3/", "dir./x.mp3", "dir /x.mp3",
            "a/\u{0}.mp3", ".beat-index.json", ".beat-index.corrupt-1.bak", "a.mp3.part", "CON/track.mp3"] {
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
        let entry = |id: &str| CachedTrack { id: id.into(), path: format!("{id}.mp3"),
            ..CachedTrack::default() };
        let running = Cache::load(root.clone());
        // Settings saved while a download is still running...
        let after_settings = running.for_root(root.clone());
        // ...which finishes through the old handle, and another one through the new.
        running.insert(entry("first")).unwrap();
        after_settings.insert(entry("second")).unwrap();
        let reloaded = Cache::load(root.clone());
        let ids: Vec<String> = reloaded.inner.index.lock().unwrap().keys().cloned().collect();
        assert!(ids.contains(&"first".to_owned()), "the first download vanished from the index: {ids:?}");
        assert!(ids.contains(&"second".to_owned()), "{ids:?}");
        let other = temp_root("other-root");
        assert_eq!(running.for_root(other.clone()).root(), other.as_path());
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
        assert!(cache.warning().is_some(), "the user is never told the index was dropped");
        cache.insert(CachedTrack { id: "1".into(), path: "a.mp3".into(), ..CachedTrack::default() }).unwrap();
        let backups: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().flatten()
            .map(|entry| entry.path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().contains("corrupt")).collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{ not json");
        let reloaded = Cache::load(root.clone());
        assert!(reloaded.warning().is_none());
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
        let cache = Cache::from_loaded(root.clone(), loaded);
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
        assert!(download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream,
            &Progress::new(None), "raw").is_err());
        assert_eq!(std::fs::read(part).unwrap(), b"do not delete");
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
