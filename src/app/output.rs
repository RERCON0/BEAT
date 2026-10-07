use super::*;

impl BeatApp {
    /// eframe redraws only on input, but downloads, workers and playback move
    /// without any: ask for the next frame while any of them is running.
    pub(super) fn schedule_repaint(&self, ctx: &egui::Context) {
        let paused = self.player.as_ref().map(|player| player.is_paused()).unwrap_or(false);
        let playing = match self.play_state {
            PlayState::Idle => false,
            PlayState::Waiting(_) => true,
            PlayState::Buffering { .. } => true,
            PlayState::Playing => !paused,
        };
        let requests = [
            self.loading.is_some(),
            self.disk_scan_id.is_some(),
            self.local_stats_request.is_some(),
            self.check_request.is_some(),
            self.ping_request.is_some(),
            self.cover_inflight > 0,
            self.catalog_scan.is_some(),
            self.library_save_pending,
            self.prepare_busy,
        ]
        .into_iter()
        .filter(|running| *running)
        .count();
        let transfers = self.downloads.len() + self.download_queue.len();
        if let Some(delay) = repaint_after(playing, transfers, requests) {
            ctx.request_repaint_after(delay);
        }
        if self.cfg.auto_cache_new && self.client.is_some() && self.catalog_scan.is_none() {
            ctx.request_repaint_after(self.next_catalog_check.saturating_duration_since(std::time::Instant::now()));
        }
    }
    /// Watches the output: an OS stream error (Bluetooth switched off or out
    /// of range) or a different default device means the old sink is dead —
    /// it plays nothing and can hang a seek forever — so the output is
    /// rebuilt with the same track and position.
    pub(super) fn check_output_health(&mut self) {
        if self.player.as_ref().is_some_and(|player| player.has_stream_error()) {
            self.rebuild_output(crate::i18n::tr("поток аудиоустройства прерван"));
            return;
        }
        if self.output_checked_at.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.output_checked_at = std::time::Instant::now();
        let current = current_output_device_id();
        if self.player.is_none() {
            if current.is_some() {
                self.rebuild_output(crate::i18n::tr("аудиоустройство появилось"));
            } else {
                self.output_device = current;
            }
            return;
        }
        if current != self.output_device {
            self.rebuild_output(if current.is_some() {
                crate::i18n::tr("аудиоустройство изменилось")
            } else {
                crate::i18n::tr("аудиоустройство исчезло")
            });
        }
    }
    /// Replaces the output sink, reloading the current track at the same
    /// position. A track that was playing keeps playing; a paused or idle
    /// player stays paused.
    pub(super) fn rebuild_output(&mut self, reason: &str) {
        self.invalidate_prefetch();
        let position = self.playback_position();
        self.resume_position = position;
        let was_playing = matches!(self.play_state, PlayState::Playing)
            && self.player.as_ref().is_some_and(|player| !player.is_paused());
        let song = self.current.clone();
        let loaded = !matches!(self.play_state, PlayState::Idle);
        self.play_state = PlayState::Idle;
        self.pending_seek = None;
        match player::Player::new(self.cfg.volume) {
            Ok(fresh) => self.player = Some(fresh),
            Err(err) => {
                self.player = None;
                self.output_device = current_output_device_id();
                self.notice = Some(format!("{reason}: {err}"));
                return;
            }
        }
        self.output_device = current_output_device_id();
        // Recovery is silent: the user pressed nothing and everything works.
        // Only a failure to open a new output is worth a message.
        if let Some(song) = song.filter(|_| loaded) {
            self.reload_after_output_change(song, position, was_playing);
        }
    }
    /// Reopens the current song on the rebuilt output. Files on disk keep
    /// their position; a stream still downloading restarts from the start.
    pub(super) fn reload_after_output_change(&mut self, song: api::Song, position: f64, resume: bool) {
        let path = if let Some(rel) = song.id.strip_prefix(local::LOCAL_ID_PREFIX) {
            let _ = rel;
            self.local_file(&song.id)
        } else {
            self.cache.entry(&song.id).and_then(|entry| self.cache.absolute(&entry))
        };
        if let Some(path) = path {
            let result = self.player.as_ref().map(|player| player.play_file(&path));
            match result {
                Some(Ok(())) => {
                    // Pause before seeking: no audible blip while the position
                    // is restored, and rodio applies seeks while paused too.
                    if !resume {
                        if let Some(player) = &self.player {
                            player.pause();
                        }
                    }
                    if position > 0.5 {
                        if let Some(player) = &self.player {
                            if !player.has_stream_error() {
                                let _ = player.seek(position);
                            }
                        }
                    }
                    // Keep the listen marker, including a track which had not
                    // played for a full second before its output disappeared.
                    self.play_state = PlayState::Playing;
                    return;
                }
                Some(Err(err)) => {
                    self.play_error = Some(err);
                    return;
                }
                None => return,
            }
        }
        // Still downloading: the position inside a partial file cannot be
        // restored reliably after the output was rebuilt. Resume restarts it;
        // a paused player waits for the next «play» press.
        if resume {
            self.start_song(song);
        }
    }
}
