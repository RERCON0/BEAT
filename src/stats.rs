//! Local play counters feeding the «часто прослушиваемые» view. The Subsonic
//! API has no global top-songs endpoint, so BEAT keeps its own per-track
//! counts; the file stores only song ids and counters and never leaves the
//! machine. Writes are debounced: at most one file write per `SAVE_EVERY`.

use crate::config::{atomic_write, config_path};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const SAVE_EVERY: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Default)]
struct StatsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    counts: HashMap<String, u64>,
}

pub struct Stats {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    counts: HashMap<String, u64>,
    dirty: bool,
    saved_at: Instant,
}

impl Stats {
    pub fn load() -> Self {
        Self::load_from(default_path())
    }

    pub fn load_from(path: PathBuf) -> Self {
        let counts = match read_counts(&path) {
            Ok(counts) => counts,
            Err(()) => {
                // A damaged counter file must not be silently replaced: keep
                // a copy for the user and start over.
                set_aside(&path);
                HashMap::new()
            }
        };
        Self { path, inner: Mutex::new(Inner { counts, dirty: false, saved_at: Instant::now() }) }
    }

    pub fn count(&self, id: &str) -> u64 {
        crate::lock(&self.inner).counts.get(id).copied().unwrap_or(0)
    }

    /// Counts one listen; writes to disk at most once per `SAVE_EVERY`.
    pub fn increment(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut inner = crate::lock(&self.inner);
        let count = inner.counts.entry(id.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
        inner.dirty = true;
        if inner.saved_at.elapsed() >= SAVE_EVERY {
            let _ = save(&self.path, &mut inner);
        }
    }

    /// Writes pending counts (frequent view open, app exit).
    pub fn flush(&self) {
        let mut inner = crate::lock(&self.inner);
        if inner.dirty {
            let _ = save(&self.path, &mut inner);
        }
    }
}

fn default_path() -> PathBuf {
    config_path().with_file_name("play-stats.json")
}

/// `Ok` also for a missing file (fresh counters); `Err` only for a file that
/// exists but cannot be trusted.
fn read_counts(path: &Path) -> Result<HashMap<String, u64>, ()> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(_) => return Err(()),
    };
    if file.metadata().map_err(|_| ())?.len() > MAX_BYTES {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(());
    }
    let parsed: StatsFile = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if parsed.version != 1 {
        return Err(());
    }
    Ok(parsed
        .counts
        .into_iter()
        .filter(|(id, count)| !id.is_empty() && *count > 0)
        .map(|(id, count)| (id, count.min(u32::MAX as u64)))
        .collect())
}

fn save(path: &Path, inner: &mut Inner) -> Result<(), String> {
    // Bound the file: the least played entries fall off first.
    if inner.counts.len() > MAX_ENTRIES {
        let mut entries: Vec<(String, u64)> = inner.counts.iter().map(|(id, count)| (id.clone(), *count)).collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(MAX_ENTRIES);
        inner.counts = entries.into_iter().collect();
    }
    let bytes =
        serde_json::to_vec(&StatsFile { version: 1, counts: inner.counts.clone() }).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    atomic_write(path, &bytes)?;
    inner.dirty = false;
    inner.saved_at = Instant::now();
    Ok(())
}

/// Renames an unreadable counter file out of the way; a taken name is kept as
/// is (the save path then starts from a fresh in-memory map anyway).
fn set_aside(path: &Path) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let backup = path.with_extension(format!("corrupt-{stamp}.bak"));
    if backup.exists() {
        return;
    }
    let _ = std::fs::rename(path, &backup);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "beat-stats-{tag}-{}-{}/play-stats.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn counts_roundtrip_through_the_file() {
        let path = test_path("roundtrip");
        let stats = Stats::load_from(path.clone());
        stats.increment("a");
        stats.increment("a");
        stats.increment("b");
        assert_eq!(stats.count("a"), 2);
        stats.flush();
        let reloaded = Stats::load_from(path.clone());
        assert_eq!(reloaded.count("a"), 2);
        assert_eq!(reloaded.count("b"), 1);
        assert_eq!(reloaded.count("missing"), 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_file_is_set_aside_not_overwritten_in_place() {
        let path = test_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let stats = Stats::load_from(path.clone());
        assert_eq!(stats.count("a"), 0);
        assert!(!path.exists(), "the damaged file was overwritten in place");
        let backup = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .any(|name| name.contains("corrupt"));
        assert!(backup, "no backup was kept for the damaged counter file");
        stats.increment("a");
        stats.flush();
        assert_eq!(Stats::load_from(path.clone()).count("a"), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_foreign_version_is_treated_as_damaged() {
        let path = test_path("version");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version":99,"counts":{"a":5}}"#).unwrap();
        let stats = Stats::load_from(path.clone());
        assert_eq!(stats.count("a"), 0);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
