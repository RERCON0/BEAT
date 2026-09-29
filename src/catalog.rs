//! Full-library discovery for bulk downloads and automatic caching. The first
//! automatic pass saves a baseline of existing song IDs; later passes find new
//! IDs even when they are added to an old album. Songs are sent through a
//! bounded channel so a large library cannot fill the UI's download queue.

use crate::api::{Client, Song};
use crate::cache::Cache;
use crate::config::{atomic_write, config_path};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::SyncSender, Arc};

pub const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);
pub const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(2 * 60);
pub const MAX_QUEUED: usize = 128;
#[cfg(not(test))]
const PAGE_SIZE: u32 = 100;
#[cfg(test)]
const PAGE_SIZE: u32 = 2;
const MAX_ALBUMS: usize = 50_000;
const MAX_SONGS: usize = 500_000;
const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode { Automatic, All }

pub enum Event {
    Started { baseline: bool },
    AlbumScanned,
    Song(Song),
    Finished(Result<(), String>),
}

#[derive(Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    known: Vec<String>,
}

pub fn state_path(client: &Client) -> PathBuf {
    config_path().with_file_name(format!("catalog-{}.json", client.catalog_key()))
}

/// Each run uses an independent reader. Dropping that reader and setting
/// `cancel` prevents an old server's results from entering a new queue.
pub fn spawn(
    client: Arc<Client>, cache: Cache, mode: Mode,
    tx: SyncSender<Event>, cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let path = state_path(&client);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            scan(&client, &cache, mode, &path, &tx, &cancel)
        ));
        let result = outcome.unwrap_or_else(|_| Err("внутренняя ошибка при проверке библиотеки".into()));
        if !cancel.load(Ordering::Relaxed) {
            let _ = tx.send(Event::Finished(result));
        }
    });
}

fn scan(
    client: &Client, cache: &Cache, mode: Mode, path: &Path,
    tx: &SyncSender<Event>, cancel: &AtomicBool,
) -> Result<(), String> {
    let (mut known, baseline) = if mode == Mode::Automatic {
        match load_known(path)? {
            Some(known) => (known, false),
            None => (HashSet::new(), true),
        }
    } else {
        (HashSet::new(), false)
    };
    send(tx, Event::Started { baseline }, cancel)?;

    let mut seen_albums = HashSet::new();
    let mut seen_songs = HashSet::new();
    let mut offset = 0;
    loop {
        check_cancel(cancel)?;
        // Alphabetical order lets us reach every album, unlike `newest` (only
        // recent albums) or `random` (duplicates and omissions).
        let page = client.album_list("alphabeticalByName", PAGE_SIZE, offset)?;
        let page_len = page.len();
        if page_len > PAGE_SIZE as usize {
            return Err("сервер прислал больше альбомов, чем запрошено в одной странице".into());
        }
        for album in page {
            check_cancel(cancel)?;
            if !seen_albums.insert(album.id.clone()) {
                return Err("список альбомов изменился во время обхода или сервер повторяет страницу; повторите проверку".into());
            }
            if seen_albums.len() > MAX_ALBUMS {
                return Err("слишком много альбомов: полный обход прерван, список не обрезан молча".into());
            }
            let songs = client.catalog_album(&album.id)?;
            for song in songs {
                check_cancel(cancel)?;
                if !seen_songs.insert(song.id.clone()) { continue; }
                if seen_songs.len() > MAX_SONGS {
                    return Err("слишком много песен: полный обход прерван, список не обрезан молча".into());
                }
                if baseline {
                    known.insert(song.id);
                } else if mode == Mode::All {
                    if !cache.contains(&song.id) { send(tx, Event::Song(song), cancel)?; }
                } else if !known.contains(&song.id) {
                    if cache.contains(&song.id) {
                        // Also remember files downloaded manually: deleting
                        // one later should not make it appear "new" again.
                        known.insert(song.id);
                    } else {
                        send(tx, Event::Song(song), cancel)?;
                    }
                }
            }
            send(tx, Event::AlbumScanned, cancel)?;
        }
        if page_len == 0 { break; }
        // A server may cap responses below `size`; the next offset must be
        // based on what was actually received, not on what was requested.
        offset = offset.checked_add(page_len as u32).ok_or("слишком много альбомов для постраничного обхода")?;
    }
    check_cancel(cancel)?;
    if mode == Mode::Automatic {
        // New uncached songs are deliberately NOT marked known yet: a failed
        // download or app restart must leave them eligible on the next scan.
        // Keep earlier IDs even if a changing server omitted an album this
        // time: dropping them would turn old songs into "new" on the next pass.
        save_known(path, &known)?;
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { Err("проверка библиотеки отменена".into()) }
    else { Ok(()) }
}

fn send(tx: &SyncSender<Event>, event: Event, cancel: &AtomicBool) -> Result<(), String> {
    check_cancel(cancel)?;
    tx.send(event).map_err(|_| "проверка библиотеки отменена".to_owned())
}

fn load_known(path: &Path) -> Result<Option<HashSet<String>>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("не удалось прочитать список известных песен: {err}")),
    };
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_STATE_BYTES {
        return Err("список известных песен слишком большой; файл сохранён без изменений".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)
        .map_err(|e| format!("не удалось прочитать список известных песен: {e}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("список известных песен слишком большой; файл сохранён без изменений".into());
    }
    let state: CatalogFile = serde_json::from_slice(&bytes)
        .map_err(|_| "список известных песен повреждён; файл сохранён без изменений".to_owned())?;
    if state.version != 1 || state.known.iter().any(String::is_empty) {
        return Err("неподдерживаемый список известных песен; файл сохранён без изменений".into());
    }
    Ok(Some(state.known.into_iter().collect()))
}

fn save_known(path: &Path, known: &HashSet<String>) -> Result<(), String> {
    let mut ids: Vec<String> = known.iter().cloned().collect();
    ids.sort_unstable();
    let bytes = serde_json::to_vec(&CatalogFile { version: 1, known: ids })
        .map_err(|e| format!("не удалось сохранить список песен: {e}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("список известных песен слишком большой; прежний файл не заменён".into());
    }
    let parent = path.parent().ok_or("не удалось определить папку списка песен")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("не удалось создать папку списка песен: {e}"))?;
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Server;
    use crate::config::StreamFormat;
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    fn root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("beat-catalog-{tag}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
    }

    fn mock_server(album_count: usize, requests: usize, stage: Arc<AtomicUsize>)
        -> (Client, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Server { base: format!("http://{}", listener.local_addr().unwrap()),
            user: "test".into(), password: "password".into() };
        let client = Client::new(&server, StreamFormat::Raw, 320).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                }
                let line = String::from_utf8_lossy(&request);
                let target = line.split_whitespace().nth(1).unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
                let body = if url.path().ends_with("getAlbumList2.view") {
                    let offset: usize = if stage.load(Ordering::SeqCst) == 4 { 0 }
                        else { params["offset"].parse().unwrap() };
                    let page_size = if stage.load(Ordering::SeqCst) == 3 { 1 } else { PAGE_SIZE as usize };
                    let albums: Vec<_> = (offset..album_count.min(offset + page_size))
                        .map(|index| serde_json::json!({"id": format!("album-{index}")})).collect();
                    serde_json::json!({"subsonic-response": {"status": "ok", "albumList2": {"album": albums}}})
                } else {
                    assert!(url.path().ends_with("getAlbum.view"));
                    let id = params["id"].clone();
                    if stage.load(Ordering::SeqCst) == 2 && id == "album-1" {
                        let body = r#"{"subsonic-response":{"status":"failed","error":{"code":0,"message":"temporary problem"}}}"#;
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        continue;
                    }
                    let mut songs = if stage.load(Ordering::SeqCst) == 7 && id == "album-0" {
                        Vec::new()
                    } else {
                        vec![serde_json::json!({"id": format!("song-{id}")})]
                    };
                    if stage.load(Ordering::SeqCst) == 1 && id == "album-0" {
                        songs.push(serde_json::json!({"id": "new-in-old-album"}));
                    }
                    let song_count = if stage.load(Ordering::SeqCst) == 5 { songs.len() + 1 } else { songs.len() };
                    serde_json::json!({"subsonic-response": {"status": "ok", "album": {"id": id, "songCount": song_count, "song": songs}}})
                }.to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (client, worker)
    }

    fn run(client: &Client, cache: &Cache, path: &Path, mode: Mode) -> (bool, usize, Vec<String>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        scan(client, cache, mode, path, &tx, &AtomicBool::new(false)).unwrap();
        drop(tx);
        let mut baseline = false;
        let mut albums = 0;
        let mut songs = Vec::new();
        for event in rx {
            match event {
                Event::Started { baseline: first } => baseline = first,
                Event::AlbumScanned => albums += 1,
                Event::Song(song) => songs.push(song.id),
                Event::Finished(_) => panic!("scan() does not send the worker's final event"),
            }
        }
        (baseline, albums, songs)
    }

    #[test]
    fn auto_baselines_then_finds_tracks_in_old_albums_and_bulk_backfills() {
        let dir = root("new-songs");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let stage = Arc::new(AtomicUsize::new(0));
        let (client, worker) = mock_server(1, 15, stage.clone());
        let (baseline, albums, songs) = run(&client, &cache, &path, Mode::Automatic);
        assert!(baseline);
        assert_eq!(albums, 1);
        assert!(songs.is_empty(), "enabling auto mode must not download the old library");
        assert!(load_known(&path).unwrap().unwrap().contains("song-album-0"));

        stage.store(1, Ordering::SeqCst);
        let (_, _, songs) = run(&client, &cache, &path, Mode::Automatic);
        assert_eq!(songs, ["new-in-old-album"]);
        // The first scan queued it but cannot call it done until it exists on
        // disk; a failed transfer must still be retried on the next scan.
        let (_, _, songs) = run(&client, &cache, &path, Mode::Automatic);
        assert_eq!(songs, ["new-in-old-album"]);

        let cached = dir.join("cache").join("new.mp3");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"audio").unwrap();
        cache.insert(crate::cache::CachedTrack { id: "new-in-old-album".into(), path: "new.mp3".into(),
            ..Default::default() }).unwrap();
        assert!(run(&client, &cache, &path, Mode::Automatic).2.is_empty());
        cache.remove("new-in-old-album").unwrap();
        // Once successfully downloaded and checkpointed, deleting the file
        // does not make it look like a newly arrived song again.
        assert!(run(&client, &cache, &path, Mode::Automatic).2.is_empty());
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bulk_download_paginates_and_does_not_modify_the_auto_checkpoint() {
        let dir = root("pages");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let (client, worker) = mock_server(3, 6, Arc::new(AtomicUsize::new(0)));
        let (baseline, albums, songs) = run(&client, &cache, &path, Mode::All);
        assert!(!baseline);
        assert_eq!(albums, 3);
        assert_eq!(songs.len(), 3);
        assert!(!path.exists(), "bulk mode must not establish an auto baseline");
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_checkpoint_is_kept_and_never_treated_as_an_empty_baseline() {
        let dir = root("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("checkpoint.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_known(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_baseline_is_not_checkpointed_and_the_next_scan_retries_every_album() {
        let dir = root("retry-baseline");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let stage = Arc::new(AtomicUsize::new(2));
        let (client, worker) = mock_server(2, 7, stage.clone());
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let error = scan(&client, &cache, Mode::Automatic, &path, &tx, &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("temporary problem"), "{error}");
        assert!(!path.exists(), "a partial baseline must never be saved");
        stage.store(0, Ordering::SeqCst);
        let (baseline, albums, songs) = run(&client, &cache, &path, Mode::Automatic);
        assert!(baseline);
        assert_eq!(albums, 2);
        assert!(songs.is_empty());
        assert_eq!(load_known(&path).unwrap().unwrap().len(), 2);
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn server_side_page_cap_does_not_skip_albums() {
        let dir = root("short-pages");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let (client, worker) = mock_server(3, 7, Arc::new(AtomicUsize::new(3)));
        let (_, albums, songs) = run(&client, &cache, &path, Mode::All);
        assert_eq!(albums, 3);
        assert_eq!(songs.len(), 3);
        worker.join().unwrap();
    }

    #[test]
    fn repeated_page_or_incomplete_album_never_creates_an_auto_baseline() {
        let dir = root("partial-baseline");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let (client, worker) = mock_server(2, 4, Arc::new(AtomicUsize::new(4)));
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let err = scan(&client, &cache, Mode::Automatic, &path, &tx, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("повторяет страницу"), "{err}");
        assert!(!path.exists());
        worker.join().unwrap();

        let (client, worker) = mock_server(1, 2, Arc::new(AtomicUsize::new(5)));
        let err = scan(&client, &cache, Mode::Automatic, &path, &tx, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("неполный список песен"), "{err}");
        assert!(!path.exists());
        worker.join().unwrap();
    }

    #[test]
    fn temporarily_missing_albums_or_songs_do_not_become_new_again() {
        let dir = root("temporarily-missing");
        let path = dir.join("checkpoint.json");
        let cache = Cache::load(dir.join("cache"));
        let stage = Arc::new(AtomicUsize::new(0));
        let (client, worker) = mock_server(1, 9, stage.clone());
        assert!(run(&client, &cache, &path, Mode::Automatic).2.is_empty());
        stage.store(7, Ordering::SeqCst);
        assert!(run(&client, &cache, &path, Mode::Automatic).2.is_empty());
        assert!(load_known(&path).unwrap().unwrap().contains("song-album-0"));
        stage.store(0, Ordering::SeqCst);
        assert!(run(&client, &cache, &path, Mode::Automatic).2.is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn dropping_a_full_scan_channel_releases_the_worker() {
        let dir = root("cancel-channel");
        let cache = Cache::load(dir.join("cache"));
        let client = Client::new(&Server { base: "http://127.0.0.1:1".into(),
            user: "test".into(), password: "password".into() }, StreamFormat::Raw, 320).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let path = dir.join("checkpoint.json");
        std::thread::spawn(move || {
            let result = scan(&client, &cache, Mode::Automatic, &path, &tx, &AtomicBool::new(false));
            let _ = done_tx.send(result.is_err());
        });
        drop(rx);
        assert_eq!(done_rx.recv_timeout(std::time::Duration::from_secs(3)), Ok(true));
    }
}
