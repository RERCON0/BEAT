use super::*;

impl BeatApp {
    pub(super) fn planned_next(&self) -> Option<usize> {
        if self.repeat == Repeat::One && self.played_secs >= MIN_LISTEN_SECS {
            return Some(self.play_index);
        }
        let len = self.play_queue.len();
        if len == 0 {
            return None;
        }
        if self.shuffle && !self.shuffle_order.is_empty() {
            self.shuffle_order
                .get(self.shuffle_pos + 1)
                .copied()
                .or_else(|| (self.repeat == Repeat::All).then(|| self.shuffle_order[0]))
        } else if self.play_index + 1 < len {
            Some(self.play_index + 1)
        } else {
            (self.repeat == Repeat::All).then_some(0)
        }
    }
    pub(super) fn accept_prefetched(&mut self) -> bool {
        let token = self.player.as_ref().map(|player| player.token());
        if self.prefetch_token.as_ref().is_none_or(|(prepared, _, _)| Some(*prepared) != token) {
            return false;
        }
        let (_, index, song) = self.prefetch_token.take().unwrap();
        self.current = Some(song);
        self.play_index = index;
        if self.shuffle {
            self.shuffle_pos = self.shuffle_order.iter().position(|i| *i == index).unwrap_or(0);
        }
        self.played_secs = 0.0;
        self.counted_current = None;
        self.listen_position = 0.0;
        self.listen_checked_at = std::time::Instant::now();
        self.resume_position = 0.0;
        self.restore_pending = false;
        self.pending_seek = None;
        self.play_error = None;
        self.prefetch_target = None;
        self.prefetch_download = None;
        self.prefetch_generation = self.prefetch_generation.wrapping_add(1);
        if let Some(player) = &self.player {
            player.advance_slot();
        }
        true
    }
    pub(super) fn invalidate_prefetch(&mut self) {
        if let Some(player) = &self.player {
            player.clear_next();
        }
        // clear() and the audio transition share a lock; accept a boundary
        // which already won before applying an edit to the queue.
        self.accept_prefetched();
        self.prefetch_generation = self.prefetch_generation.wrapping_add(1);
        self.prefetch_target = None;
        self.prefetch_download = None;
        self.prefetch_token = None;
    }
    pub(super) fn pump_prefetch(&mut self) {
        while let Ok((generation, index, result)) = self.prepare_rx.try_recv() {
            self.prepare_busy = false;
            if generation != self.prefetch_generation || !matches!(self.play_state, PlayState::Playing) {
                continue;
            }
            let Some(song) = self.play_queue.get(index).cloned() else {
                continue;
            };
            if self.prefetch_target.as_ref() != Some(&(index, song.id.clone())) {
                continue;
            }
            if let Ok(source) = result {
                if let Some(token) = self.player.as_ref().and_then(|player| player.queue_prepared(source)) {
                    self.prefetch_token = Some((token, index, song));
                }
            }
        }
        if !matches!(self.play_state, PlayState::Playing) || self.restore_pending {
            return;
        }
        let Some(index) = self.planned_next() else {
            if self.prefetch_target.is_some() {
                self.invalidate_prefetch();
            }
            return;
        };
        let Some(song) = self.play_queue.get(index).cloned() else {
            return;
        };
        let target = (index, song.id.clone());
        if self.prefetch_target.as_ref().is_some_and(|previous| previous != &target) {
            self.invalidate_prefetch();
            return;
        }
        if self.prefetch_token.is_some() || self.prepare_busy || self.prefetch_target.as_ref() == Some(&target) {
            return;
        }
        if let Some(path) = self.song_file(&song) {
            self.prefetch_target = Some(target);
            self.prepare_busy = true;
            let generation = self.prefetch_generation;
            let tx = self.prepare_tx.clone();
            std::thread::spawn(move || {
                let result = player::open_file_decoder(&path);
                let _ = tx.send((generation, index, result));
            });
        } else if !local::is_local_id(&song.id) && self.client.is_some() {
            // One attempt per planned track, even after an HTTP failure or
            // cancellation. The shared download cap still applies.
            if self.prefetch_download.as_ref() != Some(&target) {
                self.prefetch_download = Some(target);
                let id = song.id.clone();
                self.offer_download(song, false);
                if let Some(index) = self.download_queue.iter().position(|queued| queued.id == id) {
                    if let Some(next) = self.download_queue.remove(index) {
                        self.download_queue.push_front(next);
                    }
                }
            }
        }
    }
}
