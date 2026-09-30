//! On-disk cover cache. Extracting artwork from audio files (and fetching it
//! from the server) is the slow part of showing a list; the decoded thumbnail
//! is stored as a small PNG under `%APPDATA%\beat\covers` so the next launch
//! paints covers almost at once. Keys include the file size and modification
//! time (or the server cover id), so changed artwork is re-read.

use crate::config::{atomic_write, config_path};
use eframe::egui::{self, ColorImage};
use md5::{Digest, Md5};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_FILES: usize = 3000;
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const PRUNE_EVERY: usize = 64;

static STORED: AtomicUsize = AtomicUsize::new(0);

fn dir() -> PathBuf {
    config_path().with_file_name("covers")
}

fn file_for(dir: &Path, key: &str) -> PathBuf {
    let mut hasher = Md5::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut name = String::with_capacity(32);
    for byte in digest { name.push_str(&format!("{byte:02x}")); }
    dir.join(format!("{name}.png"))
}

/// Cache key for artwork embedded in a file on disk: changes when the file
/// changes (size or modification time).
pub fn file_key(rel: &str, path: &Path, px: u32) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(format!("file:{rel}:{}:{mtime}:{px}", meta.len()))
}

/// Cache key for a server cover id at a given thumbnail size.
pub fn server_key(cover_id: &str, px: u32) -> String {
    format!("cover:{cover_id}:{px}")
}

/// Decoded thumbnail from the cache, if this exact key was stored before.
pub fn load(key: &str) -> Option<ColorImage> {
    load_in(&dir(), key)
}

pub fn load_in(dir: &Path, key: &str) -> Option<ColorImage> {
    let path = file_for(dir, key);
    guarded(|| {
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() > MAX_FILE_BYTES { return None; }
        let bytes = std::fs::read(&path).ok()?;
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?.to_rgba8();
        let (width, height) = decoded.dimensions();
        if width == 0 || height == 0 || width > 1024 || height > 1024 { return None; }
        let pixels = decoded.as_raw().chunks_exact(4)
            .map(|p| egui::Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
            .collect();
        Some(ColorImage { size: [width as usize, height as usize], pixels })
    })
}

/// Stores a decoded thumbnail; best effort, a failure just means the next
/// launch re-reads the source.
pub fn store(key: &str, image: &ColorImage) -> Option<PathBuf> {
    let dir = dir();
    let path = store_in(&dir, key, image)?;
    // The directory listing is throttled: one check every few stores.
    if STORED.fetch_add(1, Ordering::Relaxed) % PRUNE_EVERY == 0 {
        prune(&dir);
    }
    Some(path)
}

pub fn store_in(dir: &Path, key: &str, image: &ColorImage) -> Option<PathBuf> {
    let (width, height) = (image.size[0], image.size[1]);
    if width == 0 || height == 0 || width > 1024 || height > 1024 { return None; }
    guarded(|| {
        let raw = image.as_raw().to_vec();
        let rgba = image::RgbaImage::from_raw(width as u32, height as u32, raw)?;
        let mut bytes = Vec::new();
        rgba.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).ok()?;
        let path = file_for(dir, key);
        std::fs::create_dir_all(dir).ok()?;
        atomic_write(&path, &bytes).ok()?;
        Some(path)
    })
}

/// Keeps the cache bounded (by file count and total size): the oldest
/// thumbnails are dropped first. Best effort; leftovers are re-pruned on the
/// next store.
pub fn prune(dir: &Path) {
    prune_to(dir, MAX_FILES, MAX_TOTAL_BYTES);
}

fn prune_to(dir: &Path, max_files: usize, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries.flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() { return None; }
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((modified, meta.len(), entry.path()))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if files.len() <= max_files && total <= max_bytes {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    // Drop down to an eighth below the caps, so pruning runs rarely.
    let file_budget = max_files.saturating_sub(max_files / 8);
    let byte_budget = max_bytes.saturating_sub(max_bytes / 8);
    let mut kept = files.len();
    let mut kept_bytes = total;
    let mut remove = 0usize;
    for (_, len, _) in &files {
        if kept <= file_budget && kept_bytes <= byte_budget { break; }
        kept -= 1;
        kept_bytes = kept_bytes.saturating_sub(*len);
        remove += 1;
    }
    for (_, _, path) in files.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}

/// Image decoding runs on cached bytes too, so its panics stay contained
/// like every other decoder path.
fn guarded<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    result.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("beat-covers-{tag}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }

    #[test]
    fn a_stored_thumbnail_loads_back_with_the_same_pixels() {
        let dir = temp_dir("roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let key = "file:a.mp3:4:123:96";
        let image = ColorImage::from_rgba_unmultiplied([2, 1], &[1, 2, 3, 4, 250, 251, 252, 255]);
        assert!(store_in(&dir, key, &image).is_some());
        let loaded = load_in(&dir, key).expect("the stored thumbnail must load");
        assert_eq!(loaded.size, [2, 1]);
        assert_eq!(loaded.as_raw(), image.as_raw());
        assert!(load_in(&dir, "other-key").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_key_changes_when_the_file_changes() {
        let dir = temp_dir("key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mp3");
        std::fs::write(&path, b"one").unwrap();
        let first = file_key("a.mp3", &path, 96).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"a longer payload").unwrap();
        let second = file_key("a.mp3", &path, 96).unwrap();
        assert_ne!(first, second, "size/mtime changes must invalidate the cached cover");
        assert_eq!(file_key("a.mp3", &path, 96).unwrap(), second);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pruning_keeps_the_newest_thumbnails() {
        let dir = temp_dir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..6 {
            std::fs::write(dir.join(format!("{index}.png")), b"x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        prune_to(&dir, 3, u64::MAX);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.contains(&"5.png".to_owned()), "the newest file was pruned: {names:?}");
        assert!(names.len() <= 3, "file cap not enforced: {names:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pruning_enforces_the_size_budget() {
        let dir = temp_dir("prune-size");
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..6 {
            std::fs::write(dir.join(format!("{index}.png")), vec![0u8; 100]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        prune_to(&dir, usize::MAX, 300);
        let total: u64 = std::fs::read_dir(&dir).unwrap().flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        assert!(total <= 300, "size cap not enforced: {total} bytes left");
        let _ = std::fs::remove_dir_all(dir);
    }
}
