use super::download::{download_stream, guarded};
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
    let bytes = include_bytes!("../../tests/fixtures/tone.webm");
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
    let result =
        std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&junction).arg(&outside).output().unwrap();
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
    let bytes = include_bytes!("../../tests/fixtures/tone.opus");
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
    assert!(
        download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw").is_err()
    );
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
    let result = download_stream(&cache, &song("1", "Band", "Album", "Song", 1), stream, &Progress::new(None), "raw");
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
    let entry =
        |id: &str, size: u64| CachedTrack { id: id.into(), path: format!("{id}.mp3"), size, ..CachedTrack::default() };
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
