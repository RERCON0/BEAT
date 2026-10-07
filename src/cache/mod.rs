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

mod index;
use index::*;
mod paths;
pub(crate) use paths::safe_path;
use paths::*;
mod progress;

mod download;
pub use download::start_download;
#[cfg(test)]
mod tests;

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
    pub fn preview(root: PathBuf) -> Self {
        Self::from_loaded(
            root,
            INDEX_FILE.into(),
            LoadedIndex { tracks: HashMap::new(), warning: None, save_blocked: true },
        )
    }
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
            loaded.warning = Some(crate::i18n::trf!(
                "не удалось перенести прежний индекс кеша: {error}; аудиофайлы сохранены",
                error = error
            ));
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
            let path = self.resolve_rel(&entry.path).ok_or(crate::i18n::tr("путь трека в индексе кеша небезопасен"))?;
            if path.exists() {
                return Err(crate::i18n::tr("трек уже находится в кеше").into());
            }
            if reserved.contains(&entry.path.to_lowercase()) {
                return Err(crate::i18n::tr("трек уже загружается").into());
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
        let path = self
            .resolve_rel(&candidate)
            .ok_or(crate::i18n::tr("папка назначения кеша содержит ссылку или небезопасный путь"))?;
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
            return Err(crate::i18n::tr("путь трека выходит за пределы папки кеша").into());
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
        let path = self.resolve_rel(&entry.path).ok_or_else(|| {
            std::io::Error::other(crate::i18n::tr("путь файла содержит ссылку или выходит за пределы кеша"))
        })?;
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
                return Err(crate::i18n::trf!("не удалось удалить файл: {err}", err = err));
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
            Err(crate::i18n::trf!("не удалось удалить файлов: {count}", count = count))
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
            return Err(crate::i18n::tr(
                "индекс кеша не был прочитан; сохранение заблокировано во избежание потери данных",
            )
            .into());
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
            return Err(crate::i18n::tr("индекс кеша слишком большой").into());
        }
        std::fs::create_dir_all(&self.inner.root)
            .map_err(|e| crate::i18n::trf!("не удалось создать кеш: {e}", e = e))?;
        atomic_write(&self.inner.root.join(&self.inner.index_name), &raw)
    }
}

enum PathError {
    Unsafe,
    Unavailable,
}

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
