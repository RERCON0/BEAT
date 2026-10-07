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
    pub source: String,
    #[serde(default)]
    pub song: Option<Song>,
    #[serde(default)]
    pub queue: Vec<Song>,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
}

pub fn path() -> PathBuf {
    config_path().with_file_name("session.json")
}

pub struct Store {
    path: PathBuf,
    save_blocked: bool,
}

impl Store {
    pub fn new() -> Self {
        Self { path: path(), save_blocked: false }
    }

    pub fn load(&mut self) -> (Session, Option<String>) {
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        self.load_at(stamp)
    }

    fn load_at(&mut self, stamp: u128) -> (Session, Option<String>) {
        match read_session(&self.path) {
            Ok(session) => (session, None),
            Err(()) => {
                let backup = self.path.with_extension(format!("corrupt-{stamp}.bak"));
                self.save_blocked = backup.exists() || std::fs::rename(&self.path, backup).is_err();
                let detail = if self.save_blocked {
                    crate::i18n::tr("сохранение заблокировано, прежний файл оставлен на месте")
                } else {
                    crate::i18n::tr("прежний файл сохранён отдельно")
                };
                (Session::default(), Some(crate::i18n::trf!("не удалось прочитать сессию: {detail}", detail = detail)))
            }
        }
    }

    pub fn save(&self, state: State<'_>) -> Result<(), String> {
        if self.save_blocked {
            return Err(crate::i18n::tr("прежняя сессия недоступна; её файл не заменён").into());
        }
        save_state(&self.path, state)
    }
}

fn read_session(path: &Path) -> Result<Session, ()> {
    let bytes = match read_capped(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Session::default()),
        Err(()) => return Err(()),
    };
    #[derive(Deserialize)]
    struct File {
        #[serde(default = "version_one")]
        version: u32,
        #[serde(flatten)]
        session: Session,
    }
    fn version_one() -> u32 {
        1
    }
    match serde_json::from_slice::<File>(&bytes) {
        Ok(File { version: 1, mut session }) => {
            session.position = safe_position(session.position);
            session.queue.retain(|song| !song.id.is_empty());
            session.index = session.index.min(session.queue.len().saturating_sub(1));
            if session.song.as_ref().is_some_and(|song| song.id.is_empty()) {
                session.song = None;
            }
            // A long queue is stored as a window around the current track. If the
            // two ever disagree (an older file, a hand edit), the current song
            // would be unreachable: "next" would run off the end of the queue
            // and clear it. The queue index already points at the right track,
            // so dropping the orphan is the safe half.
            if session.song.as_ref().is_some_and(|song| !session.queue.iter().any(|queued| queued.id == song.id)) {
                session.song = None;
            }
            if session.song.is_none() {
                session.position = 0.0;
            }
            if let Some(song) = &session.song {
                if session.queue.get(session.index).is_none_or(|queued| queued.id != song.id) {
                    if let Some(index) = session.queue.iter().position(|queued| queued.id == song.id) {
                        session.index = index;
                    }
                }
            }
            let (start, len, index) = queue_window(session.queue.len(), session.index);
            session.queue.drain(..start);
            session.queue.truncate(len);
            session.index = index;
            Ok(session)
        }
        _ => Err(()),
    }
}

pub struct State<'a> {
    pub source: &'a str,
    pub song: &'a Option<Song>,
    pub queue: &'a [Song],
    pub index: usize,
    pub position: f64,
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[cfg(test)]
pub fn save_to(path: &Path, session: &Session) -> Result<(), String> {
    save_state(
        path,
        State {
            source: &session.source,
            song: &session.song,
            queue: &session.queue,
            index: session.index,
            position: session.position,
            shuffle: session.shuffle,
            repeat: session.repeat,
        },
    )
}

fn save_state(path: &Path, state: State<'_>) -> Result<(), String> {
    let (start, len, index) = queue_window(state.queue.len(), state.index);
    #[derive(Serialize)]
    struct Ref<'a> {
        version: u32,
        source: &'a str,
        song: &'a Option<Song>,
        queue: &'a [Song],
        index: usize,
        position: f64,
        shuffle: bool,
        repeat: Repeat,
    }
    let bytes = serde_json::to_vec(&Ref {
        version: 1,
        source: state.source,
        song: state.song,
        queue: &state.queue[start..start + len],
        index,
        position: safe_position(state.position),
        shuffle: state.shuffle,
        repeat: state.repeat,
    })
    .map_err(|e| crate::i18n::trf!("не удалось сохранить сессию: {e}", e = e))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(crate::i18n::tr("сессия слишком большая; прежний файл не заменён").into());
    }
    let parent = path.parent().ok_or(crate::i18n::tr("не удалось определить папку сессии"))?;
    std::fs::create_dir_all(parent).map_err(|e| crate::i18n::trf!("не удалось создать папку сессии: {e}", e = e))?;
    atomic_write(path, &bytes)
}

pub fn safe_position(position: f64) -> f64 {
    if position.is_finite() {
        position.clamp(0.0, crate::api::MAX_DURATION_SECS)
    } else {
        0.0
    }
}

/// At most `MAX_QUEUE` songs centred on `index`, with the index rebased.
fn queue_window(len: usize, index: usize) -> (usize, usize, usize) {
    let index = index.min(len.saturating_sub(1));
    if len <= MAX_QUEUE {
        return (0, len, index);
    }
    let mut start = index.saturating_sub(MAX_QUEUE / 2);
    start = start.min(len - MAX_QUEUE);
    (start, MAX_QUEUE, index - start)
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

#[cfg(test)]
pub fn load_from(path: &Path) -> Session {
    Store { path: path.to_owned(), save_blocked: false }.load().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrecoverable_session_is_never_overwritten() {
        let path = temp_path("blocked");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"damaged").unwrap();
        std::fs::write(path.with_extension("corrupt-5.bak"), b"older backup").unwrap();
        let mut store = Store { path: path.clone(), save_blocked: false };
        let (loaded, warning) = store.load_at(5);
        assert!(warning.is_some());
        assert!(store
            .save(State {
                source: "a",
                song: &loaded.song,
                queue: &loaded.queue,
                index: 0,
                position: 0.0,
                shuffle: false,
                repeat: Repeat::Off
            })
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn externally_written_long_queues_are_bounded_and_keep_the_current_song() {
        let path = temp_path("large-load");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let session = Session {
            source: "server-a".into(),
            song: Some(songs(4001).pop().unwrap()),
            queue: songs(5000),
            index: 3,
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.source, "server-a");
        assert_eq!(loaded.queue.len(), MAX_QUEUE);
        assert_eq!(loaded.queue[loaded.index].id, "s4000");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "beat-session-{tag}-{}-{}/session.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    fn songs(count: usize) -> Vec<Song> {
        (0..count)
            .map(|index| Song { id: format!("s{index}"), title: format!("T{index}"), ..Song::default() })
            .collect()
    }

    #[test]
    fn a_session_roundtrips_through_the_file() {
        let path = temp_path("roundtrip");
        let session = Session {
            source: String::new(),
            song: Some(Song { id: "s3".into(), title: "T3".into(), ..Song::default() }),
            queue: songs(5),
            index: 3,
            position: 12.5,
            shuffle: true,
            repeat: Repeat::One,
        };
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.song.as_ref().unwrap().id, "s3");
        assert_eq!(loaded.queue.len(), 5);
        assert_eq!(loaded.index, 3);
        assert_eq!(loaded.position, 12.5);
        assert!(loaded.shuffle);
        assert_eq!(loaded.repeat, Repeat::One);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_long_queue_is_stored_as_a_window_around_the_current_song() {
        let path = temp_path("window");
        let session = Session {
            song: None,
            queue: songs(5000),
            index: 4000,
            shuffle: false,
            repeat: Repeat::Off,
            ..Session::default()
        };
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.queue.len(), MAX_QUEUE);
        assert_eq!(loaded.queue[loaded.index].id, "s4000", "the current track must stay in the window");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn position_is_bounded_and_old_sessions_remain_compatible() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0] {
            assert_eq!(safe_position(invalid), 0.0);
        }
        assert_eq!(safe_position(1e300), crate::api::MAX_DURATION_SECS);
        let path = temp_path("old-position");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version":1,"song":{"id":"s"},"queue":[{"id":"s"}],"index":0}"#).unwrap();
        let session = load_from(&path);
        assert_eq!(session.position, 0.0);
        assert_eq!(session.song.unwrap().id, "s");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn repeated_song_ids_keep_the_selected_queue_occurrence() {
        let path = temp_path("duplicate-id");
        let session = Session {
            song: Some(songs(1).remove(0)),
            queue: vec![songs(1).remove(0); 3],
            index: 2,
            position: 30.0,
            ..Session::default()
        };
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.index, 2);
        assert_eq!(loaded.position, 30.0);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_current_song_outside_the_stored_queue_is_dropped() {
        // A long queue is saved as a window; if the saved `song` is not in it,
        // «next» would run off the end of the queue and clear it. The index
        // already points at the right track, so the orphan goes.
        let path = temp_path("orphan");
        let queue = songs(50);
        let raw = format!(
            r#"{{"version":1,"song":{{"id":"not-in-queue","title":"Orphan"}},"queue":[{}],"index":7,"shuffle":false,"repeat":"off"}}"#,
            queue.iter().map(|song| format!(r#"{{"id":"{}","title":"T"}}"#, song.id)).collect::<Vec<_>>().join(",")
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, raw).unwrap();
        let loaded = load_from(&path);
        assert!(loaded.song.is_none(), "an unreachable current song was restored");
        assert_eq!(loaded.queue.len(), 50);
        assert_eq!(loaded.index, 7);
        // The normal case keeps both.
        let mut session = Session {
            song: Some(queue[7].clone()),
            queue,
            index: 7,
            shuffle: false,
            repeat: Repeat::Off,
            ..Session::default()
        };
        session.song = Some(session.queue[session.index].clone());
        save_to(&path, &session).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.song.map(|song| song.id), Some("s7".into()));
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
        let backup = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("corrupt"));
        assert!(backup, "no backup was kept for the damaged session");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
