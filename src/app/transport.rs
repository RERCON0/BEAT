use super::*;

impl BeatApp {
    pub(super) fn pump_media_keys(&mut self) {
        if self.accept_prefetched() {
            self.save_session_debounced();
        }
        for _ in 0..32 {
            let Some(command) = self.media_controls.as_ref().and_then(|controls| controls.poll()) else {
                break;
            };
            match command {
                media_keys::Command::Next => self.next_track(),
                media_keys::Command::Previous => self.prev_track(),
                media_keys::Command::Stop => self.stop_playback(),
                media_keys::Command::Toggle => self.toggle_play(),
                media_keys::Command::Play => {
                    if matches!(self.play_state, PlayState::Idle) {
                        self.toggle_play();
                    } else {
                        self.restore_autoplay = true;
                        if let Some(player) = &self.player {
                            player.resume();
                        }
                    }
                }
                media_keys::Command::Pause => {
                    self.restore_autoplay = false;
                    if let Some(player) = &self.player {
                        player.pause();
                    }
                    self.save_session();
                }
                media_keys::Command::Seek(position) => {
                    let duration = self.current.as_ref().map(|song| song.duration).unwrap_or(0.0);
                    if matches!(self.play_state, PlayState::Idle) {
                        self.resume_position = session::safe_position(position);
                        self.save_session();
                    } else {
                        self.seek_to(position, duration);
                    }
                }
            }
        }
    }
    pub(super) fn sync_media_controls(&mut self) {
        let playing = matches!(self.play_state, PlayState::Playing)
            && !self.restore_pending
            && self.player.as_ref().is_some_and(|player| !player.is_paused());
        let position = self.playback_position();
        if let Some(controls) = &mut self.media_controls {
            if let Err(error) = controls.update(self.current.as_ref(), self.current.is_some(), playing, position) {
                self.notice = Some(format!("Windows media controls: {error}"));
                self.media_controls = None;
            }
        }
    }
    /// How far a seek may go: the whole track, or only the downloaded part
    /// while the track is still arriving (`None` = not possible right now).
    pub(super) fn seek_limit(&self, duration: f64) -> Option<f64> {
        let song = self.current.as_ref()?;
        match self.downloads.get(&song.id) {
            Some(handle) => handle.progress.seekable_fraction().map(|fraction| duration * f64::from(fraction)),
            None => Some(f64::INFINITY),
        }
    }
    pub(super) fn seek_to(&mut self, target: f64, duration: f64) {
        let Some(position) = player::seek_position(target, duration, self.seek_limit(duration)) else {
            self.play_error = Some(crate::i18n::tr("перемотка станет доступна, когда трек докачается").into());
            return;
        };
        if let Some(player) = &self.player {
            // A seek into a dead stream would hang the UI thread; rebuild the
            // output instead and keep the track at its position.
            if player.has_stream_error() {
                self.rebuild_output(crate::i18n::tr("аудиоустройство недоступно"));
                return;
            }
            if let Err(err) = player.seek(position) {
                self.play_error = Some(err);
            } else {
                self.resume_position = position;
                self.restore_pending = false;
            }
        }
        self.save_session();
    }
    pub(super) fn stop_playback(&mut self) {
        self.invalidate_prefetch();
        self.resume_position = 0.0;
        self.restore_pending = false;
        if let Some(player) = &self.player {
            player.stop();
        }
        self.play_state = PlayState::Idle;
        self.current = None;
        self.save_session();
    }
    /// The footer's play/pause button: resumes or pauses the current track,
    /// and starts something when nothing is playing at all — after a restart
    /// the library itself is the queue, so plain «play» just works.
    pub(super) fn toggle_play(&mut self) {
        if self.player.as_ref().is_some_and(|player| player.has_stream_error()) {
            self.rebuild_output(crate::i18n::tr("аудиоустройство было недоступно"));
        }
        if self.player.is_none() {
            // A device may have appeared while the output was missing.
            match player::Player::new(self.cfg.volume) {
                Ok(fresh) => {
                    self.player = Some(fresh);
                    self.output_device = current_output_device_id();
                }
                Err(err) => {
                    self.play_error = Some(err);
                    return;
                }
            }
        }
        let Some(player) = &self.player else { return };
        let loaded = !matches!(self.play_state, PlayState::Idle);
        if self.current.is_some() && loaded {
            if self.restore_pending || !matches!(self.play_state, PlayState::Playing) {
                self.restore_autoplay = !self.restore_autoplay;
                self.save_session();
                return;
            }
            if player.is_paused() {
                self.restore_autoplay = true;
                player.resume();
            } else {
                self.restore_autoplay = false;
                player.pause();
            }
            self.save_session();
            return;
        }
        // A track restored from the last session (or one whose start failed)
        // is not loaded into the output yet: start it properly.
        if let Some(song) = self.current.clone() {
            self.start_song_at(song, self.resume_position);
            return;
        }
        if let Some(song) = self.play_queue.get(self.play_index).cloned() {
            self.start_song(song);
            return;
        }
        let rows = idle_start_rows(self.view, &self.library_rows, &self.library_filtered).clone();
        if rows.is_empty() {
            self.notice = Some(crate::i18n::tr("библиотека пуста — добавьте музыку или обновите список").into());
            return;
        }
        let queue: Vec<api::Song> = rows.iter().map(LibRow::to_song).collect();
        let first = queue[0].clone();
        self.play_song(first, queue, 0);
    }
}
