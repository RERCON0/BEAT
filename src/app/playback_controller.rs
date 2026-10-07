use super::*;

impl BeatApp {
    /// Starts a fresh queue at `index` (a new shuffle order is rolled when
    /// shuffle is on).
    pub(super) fn play_song(&mut self, song: api::Song, queue: Vec<api::Song>, index: usize) {
        self.invalidate_prefetch();
        self.play_queue = queue;
        self.play_index = index;
        self.refresh_shuffle();
        self.start_song(song);
    }
    /// Plays a song found through the filter or the search box with the whole
    /// library as its queue: the finder is for finding, and playback keeps
    /// going in order (or shuffled) once the song ends. A single-song queue
    /// is used only when the library does not know the song.
    pub(super) fn play_from_library(&mut self, song: api::Song) {
        if let Some(start) = library_queue_start(&self.library_rows, &song.id) {
            let queue: Vec<api::Song> = self.library_rows.iter().map(LibRow::to_song).collect();
            self.play_song(song, queue, start);
        } else {
            self.play_song(song.clone(), vec![song], 0);
        }
    }
    /// Rebuilds the shuffle order around the current track; no-op when
    /// shuffle is off or the queue is empty.
    pub(super) fn refresh_shuffle(&mut self) {
        self.shuffle_order.clear();
        self.shuffle_pos = 0;
        if self.shuffle && !self.play_queue.is_empty() {
            let (order, pos) = shuffled_order(self.play_queue.len(), self.play_index);
            self.shuffle_order = order;
            self.shuffle_pos = pos;
        }
    }
    /// Next/previous queue index honoring shuffle and repeat; `None` means
    /// the queue is finished.
    pub(super) fn step_index(&mut self, forward: bool) -> Option<usize> {
        let len = self.play_queue.len();
        if len == 0 {
            return None;
        }
        if self.shuffle && !self.shuffle_order.is_empty() {
            if forward {
                if self.shuffle_pos + 1 < self.shuffle_order.len() {
                    self.shuffle_pos += 1;
                    return self.shuffle_order.get(self.shuffle_pos).copied();
                }
                if self.repeat == Repeat::All {
                    self.shuffle_pos = 0;
                    return self.shuffle_order.first().copied();
                }
                None
            } else {
                if self.shuffle_pos > 0 {
                    self.shuffle_pos -= 1;
                    return self.shuffle_order.get(self.shuffle_pos).copied();
                }
                if self.repeat == Repeat::All {
                    self.shuffle_pos = self.shuffle_order.len().saturating_sub(1);
                    return self.shuffle_order.last().copied();
                }
                None
            }
        } else if forward {
            if self.play_index + 1 < len {
                return Some(self.play_index + 1);
            }
            if self.repeat == Repeat::All {
                return Some(0);
            }
            None
        } else {
            if self.play_index > 0 {
                return Some(self.play_index - 1);
            }
            if self.repeat == Repeat::All {
                return Some(len - 1);
            }
            None
        }
    }
    /// Plays one track from the current queue (no queue/shuffle changes).
    pub(super) fn start_song(&mut self, song: api::Song) {
        self.start_song_at(song, 0.0);
    }
    pub(super) fn start_song_at(&mut self, song: api::Song, position: f64) {
        // The caller has already selected the new queue index. A boundary
        // from the previous track must not overwrite that selection.
        if let Some(player) = &self.player {
            player.clear_next();
        }
        self.prefetch_generation = self.prefetch_generation.wrapping_add(1);
        self.prefetch_target = None;
        self.prefetch_download = None;
        self.prefetch_token = None;
        self.resume_position = session::safe_position(position);
        if song.duration > 0.0 {
            self.resume_position = self.resume_position.min(song.duration);
        }
        self.restore_pending = self.resume_position > 0.0;
        self.restore_autoplay = true;
        if let Some(player) = &self.player {
            player.stop();
        }
        self.counted_current = None;
        self.played_secs = 0.0;
        self.listen_position = 0.0;
        self.listen_checked_at = std::time::Instant::now();
        self.pending_seek = None;
        self.play_state = PlayState::Idle;
        self.play_error = None;
        self.current = Some(song.clone());
        self.save_session_debounced();
        // Local files play straight from disk; the id carries the path inside
        // the cache folder. No server request and no index entry involved.
        if let Some(rel) = song.id.strip_prefix(local::LOCAL_ID_PREFIX) {
            let _ = rel;
            let path = self.local_file(&song.id);
            let Some(path) = path else {
                self.play_error = Some(crate::i18n::tr("файл не найден в папках музыки — обновите список").into());
                self.play_state = PlayState::Idle;
                return;
            };
            match self.player.as_ref().map(|player| player.play_file_at(&path, self.resume_position)) {
                Some(Ok(())) => {
                    self.play_state = PlayState::Playing;
                    self.restore_pending = false;
                }
                Some(Err(err)) => {
                    self.play_error = Some(err);
                    self.play_state = PlayState::Idle;
                }
                None => {
                    self.play_error = Some(crate::i18n::tr("аудиовыход недоступен").into());
                    self.play_state = PlayState::Idle;
                }
            }
            return;
        }
        if let Some(entry) = self.cache.entry(&song.id) {
            let Some(path) = self.cache.absolute(&entry) else {
                self.play_error = Some(crate::i18n::tr("файл кеша недоступен — обновите список").into());
                self.play_state = PlayState::Idle;
                return;
            };
            match self.player.as_ref().map(|player| player.play_file_at(&path, self.resume_position)) {
                Some(Ok(())) => {
                    self.play_state = PlayState::Playing;
                    self.restore_pending = false;
                    return;
                }
                Some(Err(err)) => {
                    self.play_error = Some(err);
                    self.play_state = PlayState::Idle;
                    return;
                }
                None => {
                    self.play_error = Some(crate::i18n::tr("аудиовыход недоступен").into());
                    self.play_state = PlayState::Idle;
                    return;
                }
            }
        }
        // Not cached: stream + cache it. A corrupt cached file stays in place
        // until the user removes it instead of being overwritten silently.
        let handle = if let Some(handle) = self.downloads.get(&song.id) {
            handle.clone()
        } else {
            if let Some(position) = self.download_queue.iter().position(|queued| queued.id == song.id) {
                self.download_queue.remove(position);
                self.auto_queued.remove(&song.id);
                self.download_queue.push_front(song.clone());
                self.play_state = PlayState::Waiting(song);
                return;
            }
            if self.downloads.len() >= self.cfg.parallel_downloads {
                self.download_queue.push_front(song.clone());
                self.play_state = PlayState::Waiting(song);
                return;
            }
            match self.start_playback_download(song.clone()) {
                Some(handle) => handle,
                None => return,
            }
        };
        self.play_state = new_buffering(handle);
    }
    pub(super) fn start_playback_download(&mut self, song: api::Song) -> Option<DlHandle> {
        let Some(client) = self.client.clone() else {
            self.play_error = Some(crate::i18n::tr("трек не скачан; настройте подключение к серверу").into());
            self.play_state = PlayState::Idle;
            return None;
        };
        let label = format_label(self.cfg.stream_format);
        let handle =
            cache::start_download((*client).clone(), self.cache.clone(), song.clone(), label, self.dl_tx.clone());
        self.downloads.insert(song.id.clone(), handle.clone());
        Some(handle)
    }
    pub(super) fn next_track(&mut self) {
        self.invalidate_prefetch();
        let Some(index) = self.step_index(true) else {
            self.stop_playback();
            return;
        };
        self.play_index = index;
        let song = self.play_queue[index].clone();
        self.start_song(song);
    }
    pub(super) fn prev_track(&mut self) {
        self.invalidate_prefetch();
        let position = self.playback_position();
        if position <= 5.0 {
            if let Some(index) = self.step_index(false) {
                self.play_index = index;
                let song = self.play_queue[index].clone();
                self.start_song(song);
                return;
            }
        }
        // No previous track (or already past 5 seconds): restart this one. A
        // source that cannot seek (a stream of unknown size) is started over.
        if matches!(self.play_state, PlayState::Idle) {
            self.resume_position = 0.0;
            self.save_session();
            return;
        }
        let sought = self.player.as_ref().map(|player| player.seek(0.0).is_ok()).unwrap_or(true);
        if !sought {
            if let Some(song) = self.current.clone() {
                self.start_song(song);
            }
        }
    }
    pub(super) fn play_album(&mut self, album: api::Album) {
        if album.id.starts_with(LOCAL_ALBUM_PREFIX) {
            if let Some(local) = self.local_albums.iter().find(|local| local.id == album.id) {
                let songs = local.songs.clone();
                if let Some(first) = songs.first() {
                    self.album_open = Some((album, Arc::new(songs.clone())));
                    self.play_song(first.clone(), songs, 0);
                }
            }
            return;
        }
        if self.album_open.as_ref().is_some_and(|(open, _)| open.id == album.id) {
            let songs = self.album_open.as_ref().map(|(_, songs)| songs.clone()).unwrap_or_default();
            if let Some(first) = songs.first() {
                self.play_song(first.clone(), songs.as_ref().clone(), 0);
            }
        } else {
            self.pending_download = None;
            self.pending_play = Some(album.id.clone());
            self.open_album(album);
        }
    }
    pub(super) fn edit_queue(&mut self, action: QueueAction) {
        self.invalidate_prefetch();
        let was_loaded = !matches!(self.play_state, PlayState::Idle);
        let paused = !self.restore_autoplay;
        let restart = match action {
            QueueAction::Move(from, to) => {
                move_queue_item(&mut self.play_queue, &mut self.play_index, from, to);
                false
            }
            QueueAction::Remove(index) => {
                let restart = index == self.play_index;
                remove_queue_item(&mut self.play_queue, &mut self.play_index, index);
                restart
            }
            QueueAction::KeepCurrent => {
                self.play_queue = self.play_queue.get(self.play_index).cloned().into_iter().collect();
                self.play_index = 0;
                false
            }
            QueueAction::Play(index) => {
                if index >= self.play_queue.len() {
                    return;
                }
                self.play_index = index;
                true
            }
        };
        self.refresh_shuffle();
        if self.play_queue.is_empty() {
            self.stop_playback();
        } else if restart {
            let song = self.play_queue[self.play_index].clone();
            if was_loaded || matches!(action, QueueAction::Play(_)) {
                self.start_song(song);
                if paused && !matches!(action, QueueAction::Play(_)) {
                    self.restore_autoplay = false;
                    if let Some(player) = &self.player {
                        player.pause();
                    }
                }
            } else {
                self.current = Some(song);
                self.resume_position = 0.0;
            }
        }
        self.save_session();
    }
}
