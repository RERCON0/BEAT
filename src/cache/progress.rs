use super::*;

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
                return Err(crate::i18n::tr("воспроизведение остановлено").into());
            }
            if let Some(err) = &state.failed {
                return Err(err.clone());
            }
            if state.downloaded >= needed || state.finished {
                return Ok(());
            }
            if nonblocking.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                return Err(crate::i18n::tr("декодеру нужно дождаться дополнительных данных").into());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(crate::i18n::tr("сервер слишком медленно отдаёт трек").into());
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
    pub(super) fn opened(&self, part: PathBuf, total: Option<u64>) {
        let mut state = crate::lock(&self.state);
        state.part = Some(part);
        if total.is_some() {
            state.total = total;
        }
        self.cond.notify_all();
    }

    pub(super) fn set_total(&self, total: Option<u64>) {
        if let Some(total) = total {
            crate::lock(&self.state).total = Some(total);
        }
    }

    pub(super) fn add(&self, bytes: u64) {
        let mut state = crate::lock(&self.state);
        state.downloaded = state.downloaded.saturating_add(bytes);
        self.cond.notify_all();
    }

    pub(super) fn finish(&self) {
        let mut state = crate::lock(&self.state);
        state.finished = true;
        self.cond.notify_all();
    }

    pub(super) fn fail(&self, message: String) {
        let mut state = crate::lock(&self.state);
        state.failed = Some(message);
        self.cond.notify_all();
    }
}

impl GrowingReader {
    pub fn open(progress: Arc<Progress>, path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| crate::i18n::trf!("не удалось открыть {}: {e}", path.display(), e = e))?;
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

    pub(super) fn wait_until_available(&self, needed: u64) -> std::io::Result<()> {
        if self.startup.load(Ordering::SeqCst) {
            let (downloaded, _, finished, failed) = self.progress.snapshot();
            if failed.is_none() && !finished && downloaded < needed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    crate::i18n::tr("декодеру нужно дождаться дополнительных данных"),
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
