use super::*;

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
pub(super) fn guarded<T>(step: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    outcome.unwrap_or_else(|_| Err(crate::i18n::tr("внутренняя ошибка при загрузке трека").into()))
}

/// Picks the destination, writes the stream there and cleans up after a
/// failure (`.part` file and path reservation).
pub(super) fn download_stream(
    cache: &Cache,
    song: &Song,
    stream: api::AudioStream,
    progress: &Arc<Progress>,
    format_label: &str,
) -> Result<CachedTrack, String> {
    let (path, part) = cache.dest_for(song, &stream.suffix)?;
    // A panic in the worker must also free this reservation.
    pub(super) struct Reservation<'a>(&'a Cache, PathBuf);
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

pub(super) fn run_download(
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
        return Err(crate::i18n::tr("сервер прислал слишком большой файл").into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::i18n::trf!("не удалось создать {parent:?}: {e}", e = e, parent = parent))?;
    }
    let mut cleanup = PartCleanup { path: part, created: false };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .map_err(|e| crate::i18n::trf!("не удалось создать {}: {e}", part.display(), e = e))?;
    cleanup.created = true;
    progress.opened(part.to_path_buf(), total);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| crate::i18n::trf!("поток оборвался: {e}", e = e))?;
        if n == 0 {
            break;
        }
        if written + n as u64 > max_bytes {
            return Err(crate::i18n::tr("поток превысил допустимый размер файла").into());
        }
        file.write_all(&buf[..n]).map_err(|e| crate::i18n::trf!("не удалось записать кеш: {e}", e = e))?;
        written += n as u64;
        progress.add(n as u64);
    }
    file.sync_all().map_err(|e| crate::i18n::trf!("не удалось сохранить кеш: {e}", e = e))?;
    drop(file);
    if written == 0 {
        return Err(crate::i18n::tr("сервер прислал пустой трек").into());
    }
    // A known Content-Length that does not match means a broken download.
    if let Some(total) = progress.snapshot().1 {
        if written < total {
            return Err(crate::i18n::tr("поток оборвался до конца трека").into());
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
        return Err(crate::i18n::tr("путь файла кеша изменился во время загрузки").into());
    }
    std::fs::rename(part, path).map_err(|e| crate::i18n::trf!("не удалось завершить файл кеша: {e}", e = e))?;
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
