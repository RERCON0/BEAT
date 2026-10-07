use super::*;

impl BeatApp {
    pub(super) fn update_playback(&mut self, ctx: &egui::Context) {
        if self.accept_prefetched() {
            self.save_session_debounced();
        }
        self.update_playback_state(ctx);
        if !self.restore_autoplay && matches!(self.play_state, PlayState::Playing) {
            if let Some(player) = &self.player {
                player.pause();
            }
        }
        self.apply_resume();
        self.count_current_play();
        self.pump_prefetch();
        if matches!(self.play_state, PlayState::Playing)
            && self.session_saved_at.elapsed() >= std::time::Duration::from_secs(15)
        {
            self.save_session();
        }
    }
    pub(super) fn local_file(&self, id: &str) -> Option<std::path::PathBuf> {
        local::resolve(id, self.cache.root(), &self.cfg.local_roots())
    }
    pub(super) fn song_file(&self, song: &api::Song) -> Option<std::path::PathBuf> {
        if local::is_local_id(&song.id) {
            self.local_file(&song.id)
        } else {
            self.cache.entry(&song.id).and_then(|entry| self.cache.absolute(&entry))
        }
    }
    pub(super) fn playback_position(&self) -> f64 {
        if self.restore_pending || matches!(self.play_state, PlayState::Idle) {
            self.resume_position
        } else {
            self.player.as_ref().map(|player| player.position()).unwrap_or(self.resume_position)
        }
    }
    pub(super) fn apply_resume(&mut self) {
        if !self.restore_pending || !matches!(self.play_state, PlayState::Playing) {
            return;
        }
        let Some(player) = &self.player else {
            return;
        };
        player.pause();
        let duration = self.current.as_ref().map(|song| song.duration).unwrap_or(0.0);
        if self.seek_limit(duration).is_none_or(|limit| limit + 0.01 < self.resume_position) {
            return;
        }
        let result = player.seek(self.resume_position);
        if self.restore_autoplay {
            player.resume();
        }
        self.restore_pending = false;
        if let Err(error) = result {
            self.play_error = Some(error);
        }
    }
    /// Counts one listen once playback of the current track actually started;
    /// `start_song` resets the marker, so repeat-one counts every replay.
    pub(super) fn count_current_play(&mut self) {
        if !matches!(self.play_state, PlayState::Playing) {
            return;
        }
        // How far the source really got: a valid header over zero samples never
        // advances it, and such a track must not be counted at all.
        let paused = self.player.as_ref().map(|player| player.is_paused()).unwrap_or(true);
        let position = self.player.as_ref().map(|player| player.position()).unwrap_or(0.0);
        let now = std::time::Instant::now();
        self.played_secs += listen_advance(
            self.listen_position,
            position,
            now.duration_since(self.listen_checked_at).as_secs_f64(),
            paused,
        );
        self.listen_position = position;
        self.listen_checked_at = now;
        if self.played_secs < MIN_LISTEN_SECS {
            return;
        }
        let Some(song) = &self.current else { return };
        if self.counted_current.as_deref() == Some(song.id.as_str()) {
            return;
        }
        self.counted_current = Some(song.id.clone());
        self.stats.increment(&song.id);
        self.frequent_dirty = true;
    }
    pub(super) fn update_playback_state(&mut self, ctx: &egui::Context) {
        let _ = ctx;
        if let PlayState::Waiting(song) = &self.play_state {
            if let Some(handle) = self.downloads.get(&song.id) {
                self.play_state = new_buffering(handle.clone());
            } else if let Some(entry) = self.cache.entry(&song.id) {
                let result =
                    self.cache.absolute(&entry).map(|path| self.player.as_ref().map(|player| player.play_file(&path)));
                match result {
                    Some(Some(Ok(()))) => self.play_state = PlayState::Playing,
                    Some(Some(Err(err))) => {
                        self.play_error = Some(err);
                        self.play_state = PlayState::Idle;
                    }
                    Some(None) => self.play_state = PlayState::Idle,
                    None => self.play_state = PlayState::Idle,
                }
            } else if !self.download_queue.iter().any(|queued| queued.id == song.id) {
                self.play_error = Some(crate::i18n::tr("не удалось загрузить трек для воспроизведения").into());
                self.play_state = PlayState::Idle;
            }
        }
        if let PlayState::Buffering { handle, needed, attempted_at } = &self.play_state {
            let handle = handle.clone();
            let needed = *needed;
            let attempted_at = *attempted_at;
            let (failed, downloaded, finished) = {
                let (downloaded, _total, finished, failed) = handle.progress.snapshot();
                (failed, downloaded, finished)
            };
            if let Some(err) = failed {
                self.play_error = Some(err);
                self.play_state = PlayState::Idle;
                return;
            }
            // One attempt per `needed` bytes instead of one per frame: once the
            // requirement passes the file size, retrying every frame would
            // rebuild the decoder in a loop on the UI thread until the download
            // ends. Doubling the requirement keeps the attempts logarithmic.
            if buffering_attempt_ready(downloaded, needed, attempted_at, finished) {
                // A fast download may have completed (and been indexed and
                // renamed) before this frame: play the cached file then.
                if let Some(entry) = self.cache.entry(&handle.song.id) {
                    let result = self
                        .cache
                        .absolute(&entry)
                        .map(|path| self.player.as_ref().map(|player| player.play_file(&path)));
                    match result {
                        Some(Some(Ok(()))) => {
                            self.play_state = PlayState::Playing;
                            return;
                        }
                        Some(Some(Err(err))) => {
                            self.play_error = Some(err);
                            self.play_state = PlayState::Idle;
                            return;
                        }
                        Some(None) | None => {
                            self.play_state = PlayState::Idle;
                            return;
                        }
                    }
                }
                // The `.part` file is published once the stream is open; bytes
                // (or a finished download) cannot exist before that.
                let Some(part) = handle.part() else { return };
                let total = handle.progress.snapshot().1;
                let result =
                    self.player.as_ref().map(|player| player.play_streaming(handle.progress.clone(), &part, total));
                match result {
                    Some(Ok(())) => self.play_state = PlayState::Playing,
                    Some(Err(err)) if finished => {
                        self.play_error = Some(err);
                        self.play_state = PlayState::Idle;
                    }
                    Some(Err(_)) => {
                        // The decoder needs more than the first buffer: wait
                        // for more (up to the whole file).
                        self.play_state =
                            PlayState::Buffering { handle, needed: needed.saturating_mul(2), attempted_at: downloaded };
                    }
                    None => self.play_state = PlayState::Idle,
                }
            }
        } else if matches!(self.play_state, PlayState::Playing) {
            let ended = self.player.as_ref().map(|player| player.ended()).unwrap_or(true);
            let paused = self.player.as_ref().map(|player| player.is_paused()).unwrap_or(false);
            if ended && !paused {
                // Repeat-one replays the same track; otherwise advance
                // (repeat-all wrapping is handled by `step_index`). A source
                // that produced no audio at all (a valid header over zero
                // samples) would restart on every tick, so it advances instead.
                if should_repeat_one(self.repeat, self.played_secs) {
                    if let Some(song) = self.current.clone() {
                        self.start_song(song);
                        return;
                    }
                }
                self.next_track();
            }
        }
    }
}
