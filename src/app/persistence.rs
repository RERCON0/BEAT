use super::*;

impl BeatApp {
    pub(super) fn request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }
    pub(super) fn save(&mut self) {
        if self.demo.is_some() {
            return;
        }
        // `warning` carries what the save itself had to give up (a password that
        // cannot be protected on this platform), which belongs next to the error.
        self.save_error = self.cfg.save().err().or_else(|| self.cfg.warning.clone());
    }
    /// Brings back the last track, queue, shuffle and repeat; playback itself
    /// never starts by itself.
    pub(super) fn restore_session(&mut self) {
        let (session, warning) = self.session_store.load();
        if warning.is_some() {
            self.notice = warning;
        }
        self.shuffle = session.shuffle;
        self.repeat = session.repeat;
        if !session.source.is_empty() && session.source != profile_key(&self.cfg) {
            return;
        }
        self.play_queue = session.queue;
        self.play_index = session.index.min(self.play_queue.len().saturating_sub(1));
        self.current = session.song;
        self.resume_position = session.position;
        if self.shuffle && !self.play_queue.is_empty() {
            self.refresh_shuffle();
        }
    }
    /// Persists the playback state; also called on exit.
    pub(super) fn save_session(&mut self) {
        if self.demo.is_some() {
            return;
        }
        self.accept_prefetched();
        let mut song = self.current.clone();
        let mut index = self.play_index;
        let mut position = self.playback_position();
        if !self.restore_pending && matches!(self.play_state, PlayState::Playing) {
            if let Some(player) = &self.player {
                let (token, clock_position) = player.snapshot();
                position = clock_position;
                // A boundary can win after accept_prefetched() but before
                // persistence. Save metadata and time from that same track.
                if let Some((prepared, next_index, next_song)) = &self.prefetch_token {
                    if *prepared == token {
                        song = Some(next_song.clone());
                        index = *next_index;
                    }
                }
            }
        }
        self.session_saved_at = std::time::Instant::now();
        if let Err(error) = self.session_store.save(session::State {
            source: &profile_key(&self.cfg),
            song: &song,
            queue: &self.play_queue,
            index,
            position,
            shuffle: self.shuffle,
            repeat: self.repeat,
        }) {
            self.notice = Some(error);
        }
    }
    /// Track changes can come in bursts (holding «next»): writes are
    /// throttled, the final state always lands on exit and stop.
    pub(super) fn save_session_debounced(&mut self) {
        if self.session_saved_at.elapsed() >= std::time::Duration::from_secs(2) {
            self.save_session();
        }
    }
}
