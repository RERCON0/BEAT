use super::*;

impl BeatApp {
    pub(super) fn check_connection(&mut self, draft: &Config) {
        self.settings_checking = true;
        self.settings_check = None;
        let server = api::Server::from_config(draft);
        if !server.ready() {
            self.settings_checking = false;
            self.settings_check = Some(Err(crate::i18n::tr("заполните адрес, логин и пароль").into()));
            return;
        }
        let id = self.request_id();
        self.check_request = Some(id);
        let tx = self.lib_tx.clone();
        let format = draft.stream_format;
        let bit_rate = draft.bit_rate;
        std::thread::spawn(move || {
            let result = api::Client::new(&server, format, bit_rate).and_then(|client| client.ping());
            let _ = tx.send(LibEvent::Check(id, result));
        });
    }
    /// Server connectivity in the statusbar: ping once per config change.
    pub(super) fn ping_server(&mut self) {
        self.ping_request = None;
        let Some(client) = self.client.clone() else {
            self.server_status = None;
            return;
        };
        self.server_status = None;
        let id = self.request_id();
        self.ping_request = Some(id);
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.ping();
            let _ = tx.send(LibEvent::Check(id, result));
        });
    }
    pub(super) fn open_settings(&mut self) {
        self.settings_draft = self.cfg.clone();
        self.settings_check = None;
        self.settings_checking = false;
        self.show_password = false;
        self.settings_open = true;
    }
    pub(super) fn save_settings(&mut self) {
        let mut draft = self.settings_draft.clone();
        draft.sanitize();
        if (!self.downloads.is_empty() || !self.download_queue.is_empty())
            && (draft.cache_root() != self.cache.root()
                || draft.server_url.trim() != self.cfg.server_url
                || draft.user.trim() != self.cfg.user
                || draft.password != self.cfg.password
                || draft.stream_format != self.cfg.stream_format
                || draft.bit_rate != self.cfg.bit_rate)
        {
            self.save_error = Some(
                crate::i18n::tr("дождитесь окончания загрузок перед сменой сервера, формата или папки кеша").into(),
            );
            return;
        }
        let root_changed = draft.cache_root() != self.cache.root();
        let library_changed = draft.library_dirs != self.cfg.library_dirs;
        let source_changed = root_changed
            || draft.server_url.trim() != self.cfg.server_url
            || draft.user.trim() != self.cfg.user
            || draft.password != self.cfg.password;
        let disabling_auto = self.cfg.auto_cache_new && !draft.auto_cache_new;
        let enabling_auto = !self.cfg.auto_cache_new && draft.auto_cache_new;
        let mut next = self.cfg.clone();
        next.server_url = draft.server_url.trim().to_owned();
        next.user = draft.user.trim().to_owned();
        next.password = draft.password;
        next.unreadable_password = draft.unreadable_password;
        next.cache_dir = draft.cache_dir.trim().to_owned();
        next.library_dirs = draft.library_dirs;
        next.language = draft.language;
        next.stream_format = draft.stream_format;
        next.bit_rate = draft.bit_rate;
        next.parallel_downloads = draft.parallel_downloads;
        next.auto_cache_new = draft.auto_cache_new;
        // Persist first: if DPAPI or the filesystem refuses the write, keep
        // the running client, cache and settings consistent with the disk.
        if let Err(err) = next.save() {
            self.save_error = Some(err);
            return;
        }
        self.save_error = None;
        if source_changed
            || (disabling_auto && self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Automatic))
        {
            self.cancel_catalog_scan();
        }
        if disabling_auto {
            self.download_queue.retain(|song| !self.auto_queued.contains(&song.id));
            self.auto_queued.clear();
        }
        // Only a changed source or a fresh opt-in needs an immediate pass:
        // resetting on every settings save would rescan the whole library.
        if source_changed || enabling_auto {
            self.next_catalog_check = std::time::Instant::now();
        }
        if source_changed {
            self.stop_playback();
            self.play_queue.clear();
            self.shuffle_order.clear();
            self.pending_seek = None;
            self.library_pending.clear();
            self.set_server_songs(Vec::new());
        }
        if library_changed {
            self.invalidate_prefetch();
        }
        self.cfg = next;
        i18n::set(self.cfg.language);
        self.stats.set_scope(profile_key(&self.cfg));
        if let Some(warning) = self.cfg.warning.clone() {
            self.notice = Some(warning);
        }
        self.client = Self::build_client(&self.cfg);
        if source_changed {
            if let Some(client) = &self.client {
                match catalog::load_library(client) {
                    Ok(songs) => self.set_server_songs(songs),
                    Err(err) => self.notice = Some(err),
                }
            }
        }
        let profile = api::Server::from_config(&self.cfg).catalog_key();
        self.cache = self.cache.for_profile(self.cfg.cache_root(), profile.as_deref());
        if let Some(warning) = self.cache.take_warning() {
            self.notice = Some(warning);
        }
        if source_changed {
            self.disk_scan_id = None;
            self.local_stats_request = None;
            self.save_session();
        }
        self.cover_generation = self.cover_generation.wrapping_add(1);
        self.covers.clear();
        self.cover_order.clear();
        self.cover_pending.clear();
        self.cover_failed.clear();
        self.cover_inflight = 0;
        self.check_request = None;
        self.settings_checking = false;
        if root_changed || library_changed {
            self.invalidate_prefetch();
            if library_changed {
                let roots = self.cfg.local_roots();
                if self.current.as_ref().is_some_and(|song| !local::in_roots(&song.id, &roots)) {
                    self.stop_playback();
                }
                let old_index = self.play_index;
                self.play_index =
                    self.play_queue.iter().take(old_index).filter(|song| local::in_roots(&song.id, &roots)).count();
                self.play_queue.retain(|song| local::in_roots(&song.id, &roots));
                self.play_index = self.play_index.min(self.play_queue.len().saturating_sub(1));
                self.refresh_shuffle();
                self.save_session();
            }
            self.disk_scan_id = None;
            self.local_stats_request = None;
            self.probe_cache = Arc::new(local::ProbeCache::default());
            self.disk_stale = false;
            self.local_stats = (0, 0);
        }
        self.set_disk_entries(Vec::new());
        self.loading = None;
        self.pending_play = None;
        self.pending_download = None;
        self.album_open = None;
        self.artist_open = None;
        self.search_result = None;
        self.album_list = Arc::new(Vec::new());
        self.artists_server.clear();
        self.rebuild_artists_view();
        self.search_local = Arc::new(Vec::new());
        self.search_local_key = (String::new(), 0, 0);
        self.settings_open = false;
        self.ping_server();
        self.refresh_local_stats();
        self.refresh_disk();
        if self.client.is_some() {
            self.refresh_albums("newest", crate::i18n::tr("НОВЫЕ АЛЬБОМЫ"));
        } else {
            self.loading = None;
            self.view = View::Library;
        }
    }
    pub(super) fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.dark_mode = !self.dark_mode;
        theme::set_mode(ctx, self.dark_mode);
        self.cfg.dark_mode = self.dark_mode;
        self.save();
    }
}
