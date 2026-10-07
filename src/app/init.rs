use super::*;

impl BeatApp {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>, demo: Option<demo::Snapshot>) -> Self {
        Self::initialize(&cc.egui_ctx, demo, || media_keys::Controls::new(cc))
    }
    fn initialize(
        ctx: &egui::Context,
        demo: Option<demo::Snapshot>,
        controls: impl FnOnce() -> Result<media_keys::Controls, String>,
    ) -> Self {
        let cfg = demo.as_ref().map(|snapshot| snapshot.config()).unwrap_or_else(Config::load);
        i18n::set(cfg.language);
        theme::apply(ctx, cfg.dark_mode);
        let profile = api::Server::from_config(&cfg).catalog_key();
        let cache = if demo.is_some() {
            Cache::preview(cfg.cache_root())
        } else {
            Cache::load_profile(cfg.cache_root(), profile.as_deref(), true)
        };
        let player = if demo.is_some() {
            None
        } else {
            match player::Player::new(cfg.volume) {
                Ok(player) => Some(player),
                Err(err) => {
                    eprintln!("beat: {err}");
                    None
                }
            }
        };
        let output_device = if demo.is_some() { None } else { current_output_device_id() };
        let (lib_tx, lib_rx) = channel();
        let (cover_tx, cover_rx) = channel();
        let (dl_tx, dl_rx) = channel();
        let (prepare_tx, prepare_rx) = sync_channel(1);
        let (media_controls, media_warning) = if demo.is_some() {
            (None, None)
        } else {
            match controls() {
                Ok(controls) => (Some(controls), None),
                Err(e) => (None, Some(format!("Windows media controls: {e}"))),
            }
        };
        let stats = if demo.is_some() { stats::Stats::preview() } else { stats::Stats::load_scoped(profile_key(&cfg)) };
        let mut app = Self {
            commands: Vec::new(),
            demo,
            dark_mode: cfg.dark_mode,
            client: Self::build_client(&cfg),
            player,
            output_device,
            output_checked_at: std::time::Instant::now(),
            server_status: None,
            view: View::Albums,
            album_list_title: crate::i18n::tr("НОВЫЕ АЛЬБОМЫ").into(),
            artists: Vec::new(),
            album_list: Arc::new(Vec::new()),
            artist_open: None,
            album_open: None,
            disk_entries: Arc::new(Vec::new()),
            server_songs: Arc::new(Vec::new()),
            library_pending: Vec::new(),
            library_save_pending: false,
            library_rows: Arc::new(Vec::new()),
            library_filter: String::new(),
            library_filter_applied: String::new(),
            library_filtered: Arc::new(Vec::new()),
            local_albums: Arc::new(Vec::new()),
            local_album_cards: Arc::new(Vec::new()),
            local_artists: Arc::new(Vec::new()),
            artists_server: Vec::new(),
            search_local: Arc::new(Vec::new()),
            search_local_key: (String::new(), 0, 0),
            stats,
            counted_current: None,
            session_saved_at: std::time::Instant::now() - std::time::Duration::from_secs(10),
            session_store: session::Store::new(),
            frequent_entries: Arc::new(Vec::new()),
            frequent_dirty: false,
            disk_scan_id: None,
            disk_stale: false,
            disk_scanned_at: std::time::Instant::now(),
            probe_cache: Arc::new(local::ProbeCache::default()),
            local_stats: (0, 0),
            local_stats_request: None,
            search_query: String::new(),
            search_result: None,
            loading: None,
            next_request: 0,
            check_request: None,
            ping_request: None,
            pending_play: None,
            pending_download: None,
            lib_tx,
            lib_rx,
            cover_tx,
            cover_rx,
            covers: HashMap::new(),
            cover_order: VecDeque::new(),
            cover_pending: HashSet::new(),
            cover_failed: VecDeque::new(),
            cover_generation: 0,
            disk_cover_generation: 0,
            cover_inflight: 0,
            dl_tx,
            dl_rx,
            downloads: HashMap::new(),
            download_queue: VecDeque::new(),
            auto_queued: HashSet::new(),
            catalog_scan: None,
            next_catalog_check: std::time::Instant::now(),
            current: None,
            play_queue: Vec::new(),
            play_index: 0,
            play_state: PlayState::Idle,
            shuffle: false,
            repeat: Repeat::Off,
            shuffle_order: Vec::new(),
            shuffle_pos: 0,
            play_error: None,
            resume_position: 0.0,
            restore_pending: false,
            restore_autoplay: true,
            queue_open: false,
            prefetch_generation: 0,
            prefetch_target: None,
            prefetch_download: None,
            prefetch_token: None,
            prepare_busy: false,
            prepare_tx,
            prepare_rx,
            media_controls,
            played_secs: 0.0,
            listen_position: 0.0,
            listen_checked_at: std::time::Instant::now(),
            pending_seek: None,
            notice: None,
            save_error: None,
            settings_open: false,
            settings_draft: Config::default(),
            settings_check: None,
            settings_checking: false,
            show_password: false,
            cache_clear_armed: false,
            banner_size: 0.0,
            banner_fit: -1.0,
            icon_tex: app_icon_texture(ctx),
            cfg,
            cache,
            #[cfg(windows)]
            maximized: false,
        };
        if app.demo.is_some() {
            demo::populate(&mut app, ctx);
            return app;
        }
        app.notice = app.cache.take_warning().or_else(|| app.stats.take_warning()).or(media_warning);
        app.restore_session();
        app.ping_server();
        app.refresh_local_stats();
        // The local library feeds the merged views, so scan in both modes.
        app.refresh_disk();
        // The cached server list makes the unified library instant.
        if let Some(client) = app.client.clone() {
            match catalog::load_library(&client) {
                Ok(songs) if !songs.is_empty() => app.set_server_songs(songs),
                Ok(_) => {}
                Err(err) => app.notice = Some(err),
            }
        }
        if app.client.is_some() {
            app.refresh_albums("newest", crate::i18n::tr("НОВЫЕ АЛЬБОМЫ"));
        } else {
            // No server configured: start on the library (local files dropped
            // into the cache folder) instead of an empty prompt.
            app.view = View::Library;
        }
        app
    }
    #[cfg(test)]
    pub(super) fn preview(ctx: &egui::Context) -> Self {
        Self::initialize(ctx, Some(demo::Snapshot::for_test()), || panic!("preview opened native controls"))
    }
    pub(super) fn build_client(cfg: &Config) -> Option<Arc<api::Client>> {
        let server = api::Server::from_config(cfg);
        if !server.ready() {
            return None;
        }
        match api::Client::new(&server, cfg.stream_format, cfg.bit_rate) {
            Ok(client) => Some(Arc::new(client)),
            Err(err) => {
                eprintln!("beat: {err}");
                None
            }
        }
    }
    pub(super) fn require_client(&mut self) -> Option<Arc<api::Client>> {
        match &self.client {
            Some(client) => Some(client.clone()),
            None => {
                self.notice = Some(crate::i18n::tr("сначала укажите сервер, логин и пароль в настройках").into());
                None
            }
        }
    }
}
