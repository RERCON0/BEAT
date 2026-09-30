//! Last playback session, restored after a restart: the footer shows the
//! track that was playing, and the queue, shuffle and repeat state come back
//! as the user left them. Playback itself never starts by itself.

use crate::api::Song;
use crate::config::{atomic_write, config_path};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Longer queues are stored as a window around the current track, otherwise
/// one play would rewrite a multi-megabyte file per track.
pub const MAX_QUEUE: usize = 1000;
const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(default)]
    pub song: Option<Song>,
    #[serde(default)]
    pub queue: Vec<Song>,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
}

pub fn path() -> PathBuf {
    config_path().with_file_name("session.json")
}

pub fn load() -> Session {
    load_from(&path())
}

/// Missing or damaged file: an empty session (damaged files are set aside so
/// they are never silently replaced).
pub fn load_from(path: &Path) -> Session {
    let bytes = match read_capped(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Session::default(),
        Err(()) => {
            set_aside(path);
            return Session::default();
        }
    };
    match serde_json::from_slice::<Session>(&bytes) {
        Ok(mut session) => {
            session.queue.retain(|song| !song.id.is_empty());
            session.index = session.index.min(session.queue.len().saturating_sub(1));
            if session.song.as_ref().is_some_and(|song| song.id.is_empty()) {
                session.song = None;
            }
            session
        }
        Err(_) => {
            set_aside(path);
            Session::default()
        }
    }
}

pub fn save(session: &Session) {
    let _ = save_to(&path(), session);
}

pub fn save_to(path: &Path, session: &Session) -> Result<(), String> {
    let (queue, index) = windowed_queue(&session.queue, session.index);
    #[derive(Serialize)]
    struct Ref<'a> {
        version: u32,
        song: &'a Option<Song>,
        queue: &'a [Song],
        index: usize,
        shuffle: bool,
        repeat: Repeat,
    }
    let bytes = serde_json::to_vec(&Ref {
        version: 1, song: &session.song, queue: &queue, index, shuffle: session.shuffle, repeat: session.repeat,
    }).map_err(|e| format!("не удалось сохранить сессию: {e}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("сессия слишком большая; прежний файл не заменён".into());
    }
    let parent = path.parent().ok_or("не удалось определить папку сессии")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("не удалось создать папку сессии: {e}"))?;
    atomic_write(path, &bytes)
}

/// At most `MAX_QUEUE` songs centred on `index`, with the index rebased.
fn windowed_queue(queue: &[Song], index: usize) -> (Vec<Song>, usize) {
    if queue.len() <= MAX_QUEUE {
        return (queue.to_vec(), index);
    }
    let index = index.min(queue.len() - 1);
    let mut start = index.saturating_sub(MAX_QUEUE / 2);
    start = start.min(queue.len() - MAX_QUEUE);
    (queue[start..start + MAX_QUEUE].to_vec(), index - start)
}

fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    Ok(Some(bytes))
}

/// Renames a broken file out of the way; a taken name stays as is.
fn set_aside(path: &Path) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup = path.with_extension(format!("corrupt-{stamp}.bak"));
    if backup.exists() {
        return;
    }
    let _ = std::fs::rename(path, &backup);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("beat-session-{tag}-{}-{}/session.json", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }

    fn songs(count: usize) -> Vec<Song> {
        (0..count).map(|index| Song { id: format!("s{index}"), title: format!("T{index}"), ..Song::default() }).collect()
    }

    #[test]
    fn a_session_roundtrips_through_the_file() {
        let path = temp_path("roundtrip");
        let session = Session {
            song: Some(Song { id: "s3".into(), title: "T3".into(), ..Song::default() }),
            queue: songs(5),
            index: 3,
            shuffle: true,
            repeat: Repeat::One,
        };
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.song.as_ref().unwrap().id, "s3");
        assert_eq!(loaded.queue.len(), 5);
        assert_eq!(loaded.index, 3);
        assert!(loaded.shuffle);
        assert_eq!(loaded.repeat, Repeat::One);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_long_queue_is_stored_as_a_window_around_the_current_song() {
        let path = temp_path("window");
        let session = Session { song: None, queue: songs(5000), index: 4000, shuffle: false, repeat: Repeat::Off };
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.queue.len(), MAX_QUEUE);
        assert_eq!(loaded.queue[loaded.index].id, "s4000", "the current track must stay in the window");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_session_is_set_aside_and_never_blocks_startup() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let loaded = load_from(&path);
        assert!(loaded.song.is_none() && loaded.queue.is_empty());
        assert!(!path.exists());
        let backup = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("corrupt"));
        assert!(backup, "no backup was kept for the damaged session");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
