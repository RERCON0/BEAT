// GUI exe: no console window when launched from Explorer. Panics are routed
// to a MessageBox in main() (windows-subsystem exes die silently otherwise).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod banner;
mod cache;
mod catalog;
mod config;
mod local;
mod player;
mod theme;

use cache::{Cache, CachedTrack, DlEvent, DlHandle};
use config::{Config, StreamFormat};
use eframe::egui;
use local::LocalTrack;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const TELEGRAM_URL: &str = "https://t.me/rercon";
const MAX_COVERS: usize = 200;
const COVER_PX: u32 = 260;
/// Cover textures extracted from files on disk are keyed with this prefix.
const FILE_COVER_PREFIX: &str = "file:";
/// Row/player thumbnails decode smaller than album cards (memory).
const ROW_COVER_PX: u32 = 96;
/// Covers fetched or extracted at the same time: one thread per cover meant a
/// page of 60 albums opened 60 connections and decoded 60 images at once.
const MAX_COVER_JOBS: usize = 4;
/// Widest/tallest cover that is decoded, and the most memory one decode may use.
const MAX_COVER_SIDE: u32 = 8192;
const MAX_COVER_DECODE_BYTES: u64 = 128 * 1024 * 1024;
/// Poll interval while something runs without producing input events.
const UI_TICK: std::time::Duration = std::time::Duration::from_millis(200);
/// Rescan interval of the on-disk list while downloads keep finishing.
const DISK_REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Worker events
// ---------------------------------------------------------------------------

enum LibEvent {
    /// Draft-settings connectivity check (id, result).
    Check(u64, Result<(), String>),
    Artists(u64, Result<Vec<api::Artist>, String>),
    Artist(u64, Result<(api::Artist, Vec<api::Album>), String>),
    Album(u64, Result<(api::Album, Vec<api::Song>), String>),
    AlbumList(u64, Result<Vec<api::Album>, String>),
    Search(u64, Result<api::SearchResult, String>),
    /// Cache-folder scan: (indexed downloads, hand-dropped local files).
    Disk(u64, Vec<CachedTrack>, Vec<LocalTrack>),
    /// Cheap local count walk: (track count, bytes).
    LocalStats(u64, (usize, u64)),
}

enum CoverEvent {
    Loaded(u64, String, Option<egui::ColorImage>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Albums,
    Artists,
    Artist,
    Album,
    Search,
    Cached,
}

enum PlayState {
    Idle,
    /// The download slots are full; playback waits for its queued download.
    Waiting(api::Song),
    /// Playing starts once enough of the download has arrived.
    Buffering { handle: DlHandle, needed: u64 },
    Playing,
}

/// Player repeat mode; the toggle cycles Off -> All -> One -> Off.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Off,
    All,
    One,
}

struct CatalogScan {
    rx: Receiver<catalog::Event>,
    cancel: Arc<AtomicBool>,
    mode: catalog::Mode,
    baseline: bool,
    albums: usize,
    added: usize,
}

struct BeatApp {
    cfg: Config,
    cache: Cache,
    client: Option<Arc<api::Client>>,
    player: Option<player::Player>,
    server_status: Option<Result<(), String>>,
    view: View,
    album_list_title: String,
    artists: Vec<api::Artist>,
    album_list: Vec<api::Album>,
    artist_open: Option<(api::Artist, Vec<api::Album>)>,
    album_open: Option<(api::Album, Vec<api::Song>)>,
    /// Everything on disk: downloaded cache entries plus hand-dropped local
    /// files found in the cache folder.
    disk_entries: Arc<Vec<DiskEntry>>,
    /// Request id of the in-flight cache-folder scan.
    disk_scan_id: Option<u64>,
    /// Downloads finished since the last scan of the folder.
    disk_stale: bool,
    disk_scanned_at: std::time::Instant,
    /// Tags already read from files, so a rescan opens only what changed.
    probe_cache: Arc<local::ProbeCache>,
    /// Hand-dropped local files counted by a cheap background walk:
    /// (count, bytes), for the sidebar stats before the full scan runs.
    local_stats: (usize, u64),
    local_stats_request: Option<u64>,
    search_query: String,
    search_result: Option<api::SearchResult>,
    /// Request id of the in-flight library fetch (stale replies are dropped).
    loading: Option<u64>,
    next_request: u64,
    /// Request ids of the in-flight settings check and server ping.
    check_request: Option<u64>,
    ping_request: Option<u64>,
    /// Album id that must start playing / downloading as soon as it loads.
    pending_play: Option<String>,
    pending_download: Option<String>,
    lib_tx: Sender<LibEvent>,
    lib_rx: Receiver<LibEvent>,
    cover_tx: Sender<CoverEvent>,
    cover_rx: Receiver<CoverEvent>,
    covers: HashMap<String, egui::TextureHandle>,
    cover_order: VecDeque<String>,
    cover_pending: HashSet<String>,
    cover_failed: VecDeque<String>,
    cover_generation: u64,
    /// Cover jobs (fetch or extraction) running right now.
    cover_inflight: usize,
    dl_tx: Sender<(String, DlEvent)>,
    dl_rx: Receiver<(String, DlEvent)>,
    /// song id -> running download (explicit or for playback).
    downloads: HashMap<String, DlHandle>,
    download_queue: VecDeque<api::Song>,
    /// Pending songs introduced only by automatic discovery. Disabling the
    /// option removes these, while explicit requests and active transfers stay.
    auto_queued: HashSet<String>,
    catalog_scan: Option<CatalogScan>,
    next_catalog_check: std::time::Instant,
    current: Option<api::Song>,
    play_queue: Vec<api::Song>,
    play_index: usize,
    play_state: PlayState,
    shuffle: bool,
    repeat: Repeat,
    /// Play order of `play_queue` indices while shuffle is on; `shuffle_pos`
    /// is the position of the current track inside it.
    shuffle_order: Vec<usize>,
    shuffle_pos: usize,
    play_error: Option<String>,
    /// Position picked on the seek slider, sent when the pointer is released.
    pending_seek: Option<f64>,
    /// Last user-visible message (downloads, cache, playback).
    notice: Option<String>,
    save_error: Option<String>,
    settings_open: bool,
    settings_draft: Config,
    settings_check: Option<Result<(), String>>,
    settings_checking: bool,
    show_password: bool,
    cache_clear_armed: bool,
    banner_size: f32,
    banner_fit: f32,
    icon_tex: Option<egui::TextureHandle>,
    dark_mode: bool,
    #[cfg(windows)]
    maximized: bool,
}

impl BeatApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = Config::load();
        theme::apply(&cc.egui_ctx, cfg.dark_mode);
        let cache = Cache::load(cfg.cache_root());
        let player = match player::Player::new(cfg.volume) {
            Ok(player) => Some(player),
            Err(err) => {
                eprintln!("beat: {err}");
                None
            }
        };
        let (lib_tx, lib_rx) = channel();
        let (cover_tx, cover_rx) = channel();
        let (dl_tx, dl_rx) = channel();
        let mut app = Self {
            dark_mode: cfg.dark_mode,
            client: Self::build_client(&cfg),
            player,
            server_status: None,
            view: View::Albums,
            album_list_title: "НОВЫЕ АЛЬБОМЫ".into(),
            artists: Vec::new(),
            album_list: Vec::new(),
            artist_open: None,
            album_open: None,
            disk_entries: Arc::new(Vec::new()),
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
            icon_tex: app_icon_texture(&cc.egui_ctx),
            cfg,
            cache,
            #[cfg(windows)]
            maximized: false,
        };
        app.notice = app.cache.warning();
        app.ping_server();
        app.refresh_local_stats();
        if app.client.is_some() {
            app.refresh_albums("newest", "НОВЫЕ АЛЬБОМЫ");
        } else {
            // No server configured: start on the on-disk list (local files
            // dropped into the cache folder) instead of an empty prompt.
            app.view = View::Cached;
            app.refresh_disk();
        }
        app
    }

    fn build_client(cfg: &Config) -> Option<Arc<api::Client>> {
        let server = api::Server::from_config(cfg);
        if !server.ready() { return None; }
        match api::Client::new(&server, cfg.stream_format, cfg.bit_rate) {
            Ok(client) => Some(Arc::new(client)),
            Err(err) => {
                eprintln!("beat: {err}");
                None
            }
        }
    }

    fn request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    fn save(&mut self) {
        self.save_error = self.cfg.save().err();
    }

    fn require_client(&mut self) -> Option<Arc<api::Client>> {
        match &self.client {
            Some(client) => Some(client.clone()),
            None => {
                self.notice = Some("сначала укажите сервер, логин и пароль в настройках".into());
                None
            }
        }
    }

    /// Rescans the cache folder on a worker thread: indexed downloads plus
    /// hand-dropped local files (tags and durations are read there, so the UI
    /// thread never touches files).
    fn refresh_disk(&mut self) {
        if self.disk_scan_id.is_some() {
            self.disk_stale = true;
            return;
        }
        let id = self.request_id();
        self.disk_scan_id = Some(id);
        self.disk_stale = false;
        self.disk_scanned_at = std::time::Instant::now();
        let cache = self.cache.clone();
        let probes = self.probe_cache.clone();
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(cache.root());
            // Entries whose file the user deleted by hand stop being "in cache".
            cache.prune_missing();
            let indexed = cache.list();
            let local = local::scan_with(cache.root(), &indexed_paths(&cache), &probes);
            let _ = tx.send(LibEvent::Disk(id, indexed, local));
        });
    }

    /// Counts hand-dropped files without opening them (sidebar stats), so the
    /// numbers are there before the full scan of «кеш на диске».
    fn refresh_local_stats(&mut self) {
        let id = self.request_id();
        self.local_stats_request = Some(id);
        let cache = self.cache.clone();
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let stats = local::count(cache.root(), &indexed_paths(&cache));
            let _ = tx.send(LibEvent::LocalStats(id, stats));
        });
    }

    fn open_cache_folder(&mut self) {
        let root = self.cache.root();
        if let Err(err) = std::fs::create_dir_all(root) {
            self.notice = Some(format!("не удалось создать {}: {err}", root.display()));
            return;
        }
        open_in_explorer(&root.to_string_lossy());
    }

    fn refresh_albums(&mut self, kind: &str, title: &str) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.album_list_title = title.to_owned();
        let tx = self.lib_tx.clone();
        let kind = kind.to_owned();
        std::thread::spawn(move || {
            let result = client.album_list(&kind, 60, 0);
            let _ = tx.send(LibEvent::AlbumList(id, result));
        });
    }

    fn fetch_artists(&mut self) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.artists();
            let _ = tx.send(LibEvent::Artists(id, result));
        });
    }

    fn open_artist(&mut self, artist: api::Artist) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Artist;
        self.artist_open = Some((artist.clone(), Vec::new()));
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.artist(&artist.id);
            let _ = tx.send(LibEvent::Artist(id, result));
        });
    }

    /// Opens an album on the user's click. An auto-play/download requested for
    /// an album that was never opened must not fire when it is opened later.
    fn browse_album(&mut self, album: api::Album) {
        self.pending_play = None;
        self.pending_download = None;
        self.open_album(album);
    }

    fn open_album(&mut self, album: api::Album) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Album;
        self.album_open = Some((album.clone(), Vec::new()));
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.album(&album.id);
            let _ = tx.send(LibEvent::Album(id, result));
        });
    }

    fn run_search(&mut self) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() { return; }
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Search;
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.search(&query);
            let _ = tx.send(LibEvent::Search(id, result));
        });
    }

    fn check_connection(&mut self, draft: &Config) {
        self.settings_checking = true;
        self.settings_check = None;
        let server = api::Server::from_config(draft);
        if !server.ready() {
            self.settings_checking = false;
            self.settings_check = Some(Err("заполните адрес, логин и пароль".into()));
            return;
        }
        let id = self.request_id();
        self.check_request = Some(id);
        let tx = self.lib_tx.clone();
        let format = draft.stream_format;
        let bit_rate = draft.bit_rate;
        std::thread::spawn(move || {
            let result = api::Client::new(&server, format, bit_rate)
                .and_then(|client| client.ping());
            let _ = tx.send(LibEvent::Check(id, result));
        });
    }

    /// Server connectivity in the statusbar: ping once per config change.
    fn ping_server(&mut self) {
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

    fn open_settings(&mut self) {
        self.settings_draft = self.cfg.clone();
        self.settings_check = None;
        self.settings_checking = false;
        self.show_password = false;
        self.settings_open = true;
    }

    fn save_settings(&mut self) {
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
            self.save_error = Some("дождитесь окончания загрузок перед сменой сервера, формата или папки кеша".into());
            return;
        }
        let root_changed = draft.cache_root() != self.cache.root();
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
        if source_changed || (disabling_auto && self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Automatic)) {
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
        }
        self.cfg = next;
        self.client = Self::build_client(&self.cfg);
        self.cache = self.cache.for_root(self.cfg.cache_root());
        self.cover_generation = self.cover_generation.wrapping_add(1);
        self.covers.clear();
        self.cover_order.clear();
        self.cover_pending.clear();
        self.cover_failed.clear();
        self.cover_inflight = 0;
        self.check_request = None;
        self.settings_checking = false;
        if root_changed {
            self.disk_scan_id = None;
            self.local_stats_request = None;
            self.probe_cache = Arc::new(local::ProbeCache::default());
            self.disk_stale = false;
            self.local_stats = (0, 0);
        }
        self.disk_entries = Arc::new(Vec::new());
        self.loading = None;
        self.pending_play = None;
        self.pending_download = None;
        self.album_open = None;
        self.artist_open = None;
        self.search_result = None;
        self.album_list.clear();
        self.artists.clear();
        self.settings_open = false;
        self.ping_server();
        self.refresh_local_stats();
        if self.client.is_some() {
            self.refresh_albums("newest", "НОВЫЕ АЛЬБОМЫ");
        } else {
            self.loading = None;
            self.view = View::Cached;
            self.refresh_disk();
        }
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.dark_mode = !self.dark_mode;
        theme::set_mode(ctx, self.dark_mode);
        self.cfg.dark_mode = self.dark_mode;
        self.save();
    }

    fn cancel_catalog_scan(&mut self) {
        if let Some(scan) = self.catalog_scan.take() {
            scan.cancel.store(true, Ordering::Relaxed);
            // Dropping the receiver releases a worker blocked on the bounded
            // channel, even if the network request is still finishing.
        }
    }

    fn start_catalog_scan(&mut self, mode: catalog::Mode) {
        if self.catalog_scan.is_some() { return; }
        let Some(client) = self.require_client() else { return };
        let (tx, rx) = sync_channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        catalog::spawn(client, self.cache.clone(), mode, tx, cancel.clone());
        self.catalog_scan = Some(CatalogScan { rx, cancel, mode, baseline: false, albums: 0, added: 0 });
        self.notice = Some("проверяю библиотеку Navidrome…".into());
    }

    fn maybe_start_automatic_scan(&mut self) {
        if self.cfg.auto_cache_new && self.client.is_some() && self.catalog_scan.is_none()
            && std::time::Instant::now() >= self.next_catalog_check {
            self.start_catalog_scan(catalog::Mode::Automatic);
        }
    }

    fn pump_catalog(&mut self) {
        let mut events = Vec::new();
        let mut proposed = 0;
        while events.len() < 128 && self.download_queue.len() + proposed < catalog::MAX_QUEUED {
            let Some(scan) = &self.catalog_scan else { break };
            match scan.rx.try_recv() {
                Ok(event) => {
                    let finished = matches!(event, catalog::Event::Finished(_));
                    if matches!(event, catalog::Event::Song(_)) { proposed += 1; }
                    events.push(event);
                    if finished { break; }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    events.push(catalog::Event::Finished(Err("проверка библиотеки неожиданно прервалась".into())));
                    break;
                }
            }
        }
        for event in events {
            match event {
                catalog::Event::Started { baseline } => {
                    if let Some(scan) = &mut self.catalog_scan { scan.baseline = baseline; }
                }
                catalog::Event::AlbumScanned => {
                    if let Some(scan) = &mut self.catalog_scan { scan.albums += 1; }
                }
                catalog::Event::Song(song) => {
                    let automatic = self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Automatic);
                    let added = self.offer_download(song, automatic);
                    if added {
                        if let Some(scan) = &mut self.catalog_scan { scan.added += 1; }
                    }
                }
                catalog::Event::Finished(result) => {
                    if let Some(scan) = self.catalog_scan.take() {
                        self.next_catalog_check = std::time::Instant::now()
                            + if result.is_err() { catalog::RETRY_AFTER }
                                else if scan.mode == catalog::Mode::All && self.cfg.auto_cache_new {
                                    std::time::Duration::ZERO
                                } else { catalog::POLL_EVERY };
                        self.notice = Some(match result {
                            Ok(()) if scan.baseline => format!(
                                "запомнено альбомов: {}; новые песни будут скачиваться автоматически", scan.albums),
                            Ok(()) => format!("проверено альбомов: {}; добавлено в загрузки: {}", scan.albums, scan.added),
                            Err(err) => format!("проверка библиотеки остановлена после {} альбомов (добавлено: {}): {err}",
                                scan.albums, scan.added),
                        });
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Worker pumps
    // -----------------------------------------------------------------------

    fn pump_library(&mut self) {
        while let Ok(event) = self.lib_rx.try_recv() {
            match event {
                LibEvent::Check(id, result) => {
                    if self.check_request == Some(id) {
                        self.check_request = None;
                        self.settings_checking = false;
                        self.settings_check = Some(result);
                    } else if self.ping_request == Some(id) {
                        self.ping_request = None;
                        self.server_status = Some(result);
                    }
                }
                LibEvent::Artists(id, result) => {
                    if !self.accept(id) { continue; }
                    match result {
                        Ok(artists) => {
                            self.artists = artists;
                            self.view = View::Artists;
                        }
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Artist(id, result) => {
                    if !self.accept(id) { continue; }
                    match result {
                        Ok((artist, albums)) => self.artist_open = Some((artist, albums)),
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Album(id, result) => {
                    if !self.accept(id) { continue; }
                    match result {
                        Ok((album, songs)) => {
                            let play = self.pending_play.as_deref() == Some(album.id.as_str());
                            let download = self.pending_download.as_deref() == Some(album.id.as_str());
                            if play { self.pending_play = None; }
                            if download { self.pending_download = None; }
                            self.album_open = Some((album, songs.clone()));
                            if play {
                                if let Some(first) = songs.first() {
                                    self.play_song(first.clone(), songs.clone(), 0);
                                }
                            } else if download {
                                self.enqueue_album(&songs);
                            }
                        }
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::AlbumList(id, result) => {
                    if !self.accept(id) { continue; }
                    match result {
                        Ok(albums) => {
                            self.album_list = albums;
                            self.view = View::Albums;
                        }
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Search(id, result) => {
                    if !self.accept(id) { continue; }
                    match result {
                        Ok(search) => self.search_result = Some(search),
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Disk(id, indexed, local) => {
                    // The scan owns its own request slot: it never clears
                    // `loading` of a library fetch that may be in flight.
                    if self.disk_scan_id != Some(id) { continue; }
                    self.disk_scan_id = None;
                    self.local_stats_request = None;
                    self.local_stats = (local.len(), local.iter().map(|track| track.size).sum());
                    self.disk_entries = Arc::new(merge_disk_entries(indexed, local));
                    if self.disk_stale {
                        self.refresh_disk();
                    }
                }
                LibEvent::LocalStats(id, stats) => {
                    if self.local_stats_request != Some(id) { continue; }
                    self.local_stats_request = None;
                    self.local_stats = stats;
                }
            }
        }
    }

    /// A reply is fresh when it belongs to the in-flight request; pings carry
    /// their own ids and never clear `loading`.
    fn accept(&mut self, id: u64) -> bool {
        if self.loading == Some(id) {
            self.loading = None;
            true
        } else {
            false
        }
    }

    fn pump_covers(&mut self, ctx: &egui::Context) {
        while let Ok(CoverEvent::Loaded(generation, id, image)) = self.cover_rx.try_recv() {
            if generation != self.cover_generation { continue; }
            self.cover_inflight = self.cover_inflight.saturating_sub(1);
            // A failed fetch stays "pending" so the UI does not retry it every
            // frame. Old failures are evicted to bound the memory used by ids.
            let Some(image) = image else {
                self.cover_failed.push_back(id);
                if self.cover_failed.len() > MAX_COVERS {
                    if let Some(old) = self.cover_failed.pop_front() {
                        self.cover_pending.remove(&old);
                    }
                }
                continue;
            };
            self.cover_pending.remove(&id);
            let texture = ctx.load_texture(format!("cover-{id}"), image, egui::TextureOptions::LINEAR);
            self.covers.insert(id.clone(), texture);
            self.cover_order.push_back(id);
            while self.cover_order.len() > MAX_COVERS {
                if let Some(old) = self.cover_order.pop_front() {
                    self.covers.remove(&old);
                }
            }
        }
    }

    fn pump_downloads(&mut self) {
        while let Ok((id, event)) = self.dl_rx.try_recv() {
            match event {
                DlEvent::Done(entry) => {
                    self.downloads.remove(&id);
                    self.notice = Some(format!("скачано: {} — {}", entry.artist, entry.title));
                    // The new file is indexed now: the on-disk view must be
                    // rescanned so it appears (and stops looking like a local
                    // file); `pump_downloads` does that, not once per track.
                    self.disk_stale = true;
                }
                DlEvent::Failed(err) => {
                    self.downloads.remove(&id);
                    self.notice = Some(format!("не скачалось: {err}"));
                }
            }
        }
        while self.downloads.len() < self.cfg.parallel_downloads {
            let Some(song) = self.download_queue.pop_front() else { break };
            self.auto_queued.remove(&song.id);
            if self.cache.contains(&song.id) || self.downloads.contains_key(&song.id) { continue; }
            self.spawn_download(song);
        }
        let downloading = !self.downloads.is_empty() || !self.download_queue.is_empty();
        if self.view == View::Cached && disk_refresh_due(
            self.disk_stale, self.disk_scan_id.is_some(), downloading, self.disk_scanned_at.elapsed())
        {
            self.refresh_disk();
        }
    }

    fn spawn_download(&mut self, song: api::Song) {
        let Some(client) = self.client.clone() else { return };
        let label = format_label(self.cfg.stream_format);
        let handle = cache::start_download((*client).clone(), self.cache.clone(), song.clone(), label, self.dl_tx.clone());
        self.downloads.insert(song.id, handle);
    }

    fn enqueue_download(&mut self, song: api::Song) {
        if self.cache.contains(&song.id) {
            self.notice = Some(format!("уже в кеше: {}", song.title));
            return;
        }
        if self.offer_download(song, false) {
            self.notice = Some("добавлено в загрузки".into());
        }
    }

    fn offer_download(&mut self, song: api::Song, automatic: bool) -> bool {
        if !automatic { self.auto_queued.remove(&song.id); }
        if song.id.is_empty() || self.cache.contains(&song.id) || self.downloads.contains_key(&song.id)
            || self.download_queue.iter().any(|queued| queued.id == song.id) {
            return false;
        }
        if automatic { self.auto_queued.insert(song.id.clone()); }
        self.download_queue.push_back(song);
        true
    }

    fn enqueue_album(&mut self, songs: &[api::Song]) {
        let mut added = 0;
        for song in songs {
            if self.offer_download(song.clone(), false) { added += 1; }
        }
        self.notice = Some(if added > 0 { format!("в загрузки добавлено треков: {added}") }
            else { "все треки уже скачаны".into() });
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    /// Starts a fresh queue at `index` (a new shuffle order is rolled when
    /// shuffle is on).
    fn play_song(&mut self, song: api::Song, queue: Vec<api::Song>, index: usize) {
        self.play_queue = queue;
        self.play_index = index;
        self.refresh_shuffle();
        self.start_song(song);
    }

    /// Rebuilds the shuffle order around the current track; no-op when
    /// shuffle is off or the queue is empty.
    fn refresh_shuffle(&mut self) {
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
    fn step_index(&mut self, forward: bool) -> Option<usize> {
        let len = self.play_queue.len();
        if len == 0 { return None; }
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
            if self.play_index + 1 < len { return Some(self.play_index + 1); }
            if self.repeat == Repeat::All { return Some(0); }
            None
        } else {
            if self.play_index > 0 { return Some(self.play_index - 1); }
            if self.repeat == Repeat::All { return Some(len - 1); }
            None
        }
    }

    /// Plays one track from the current queue (no queue/shuffle changes).
    fn start_song(&mut self, song: api::Song) {
        if let Some(player) = &self.player { player.stop(); }
        self.pending_seek = None;
        self.play_state = PlayState::Idle;
        self.play_error = None;
        self.current = Some(song.clone());
        // Local files play straight from disk; the id carries the path inside
        // the cache folder. No server request and no index entry involved.
        if let Some(rel) = song.id.strip_prefix(local::LOCAL_ID_PREFIX) {
            let path = self.cache.resolve_rel(rel).filter(|path| path.is_file());
            let Some(path) = path else {
                self.play_error = Some("файл не найден в папке кеша — обновите список".into());
                self.play_state = PlayState::Idle;
                return;
            };
            match self.player.as_ref().map(|player| player.play_file(&path)) {
                Some(Ok(())) => self.play_state = PlayState::Playing,
                Some(Err(err)) => {
                    self.play_error = Some(err);
                    self.play_state = PlayState::Idle;
                }
                None => {
                    self.play_error = Some("аудиовыход недоступен".into());
                    self.play_state = PlayState::Idle;
                }
            }
            return;
        }
        if let Some(entry) = self.cache.entry(&song.id) {
            let path = self.cache.absolute(&entry);
            match self.player.as_ref().map(|player| player.play_file(&path)) {
                Some(Ok(())) => {
                    self.play_state = PlayState::Playing;
                    return;
                }
                Some(Err(err)) => {
                    self.play_error = Some(err);
                    self.play_state = PlayState::Idle;
                    return;
                }
                None => {
                    self.play_error = Some("аудиовыход недоступен".into());
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
        self.play_state = PlayState::Buffering { handle, needed: cache::PLAYBACK_BUFFER_BYTES };
    }

    fn start_playback_download(&mut self, song: api::Song) -> Option<DlHandle> {
        let client = self.client.clone()?;
        let label = format_label(self.cfg.stream_format);
        let handle = cache::start_download((*client).clone(), self.cache.clone(), song.clone(), label, self.dl_tx.clone());
        self.downloads.insert(song.id.clone(), handle.clone());
        Some(handle)
    }

    fn update_playback(&mut self, ctx: &egui::Context) {
        let _ = ctx;
        if let PlayState::Waiting(song) = &self.play_state {
            if let Some(handle) = self.downloads.get(&song.id) {
                self.play_state = PlayState::Buffering { handle: handle.clone(), needed: cache::PLAYBACK_BUFFER_BYTES };
            } else if let Some(entry) = self.cache.entry(&song.id) {
                let result = self.player.as_ref().map(|player| player.play_file(&self.cache.absolute(&entry)));
                match result {
                    Some(Ok(())) => self.play_state = PlayState::Playing,
                    Some(Err(err)) => { self.play_error = Some(err); self.play_state = PlayState::Idle; }
                    None => self.play_state = PlayState::Idle,
                }
            } else if !self.download_queue.iter().any(|queued| queued.id == song.id) {
                self.play_error = Some("не удалось загрузить трек для воспроизведения".into());
                self.play_state = PlayState::Idle;
            }
        }
        if matches!(self.play_state, PlayState::Buffering { .. }) {
            let (failed, ready, finished, needed, handle) = {
                let PlayState::Buffering { handle, needed } = &self.play_state else { return };
                let (downloaded, _total, finished, failed) = handle.progress.snapshot();
                (failed, downloaded >= *needed, finished, *needed, handle.clone())
            };
            if let Some(err) = failed {
                self.play_error = Some(err);
                self.play_state = PlayState::Idle;
                return;
            }
            if ready || finished {
                // A fast download may have completed (and been indexed and
                // renamed) before this frame: play the cached file then.
                if let Some(entry) = self.cache.entry(&handle.song.id) {
                    let path = self.cache.absolute(&entry);
                    match self.player.as_ref().map(|player| player.play_file(&path)) {
                        Some(Ok(())) => {
                            self.play_state = PlayState::Playing;
                            return;
                        }
                        Some(Err(err)) => {
                            self.play_error = Some(err);
                            self.play_state = PlayState::Idle;
                            return;
                        }
                        None => {
                            self.play_state = PlayState::Idle;
                            return;
                        }
                    }
                }
                // The `.part` file is published once the stream is open; bytes
                // (or a finished download) cannot exist before that.
                let Some(part) = handle.part() else { return };
                let total = handle.progress.snapshot().1;
                let result = self.player.as_ref()
                    .map(|player| player.play_streaming(handle.progress.clone(), &part, total));
                match result {
                    Some(Ok(())) => self.play_state = PlayState::Playing,
                    Some(Err(err)) if finished => {
                        self.play_error = Some(err);
                        self.play_state = PlayState::Idle;
                    }
                    Some(Err(_)) => {
                        // The decoder needs more than the first buffer: wait
                        // for more (up to the whole file).
                        let doubled = needed.saturating_mul(2);
                        self.play_state = PlayState::Buffering { handle, needed: doubled };
                    }
                    None => self.play_state = PlayState::Idle,
                }
            }
        } else if matches!(self.play_state, PlayState::Playing) {
            let ended = self.player.as_ref().map(|player| player.ended()).unwrap_or(true);
            let paused = self.player.as_ref().map(|player| player.is_paused()).unwrap_or(false);
            if ended && !paused {
                // Repeat-one replays the same track; otherwise advance
                // (repeat-all wrapping is handled by `step_index`).
                if self.repeat == Repeat::One {
                    if let Some(song) = self.current.clone() {
                        self.start_song(song);
                        return;
                    }
                }
                self.next_track();
            }
        }
    }

    fn next_track(&mut self) {
        let Some(index) = self.step_index(true) else {
            self.stop_playback();
            return;
        };
        self.play_index = index;
        let song = self.play_queue[index].clone();
        self.start_song(song);
    }

    fn prev_track(&mut self) {
        let position = self.player.as_ref().map(|player| player.position()).unwrap_or(0.0);
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
        let sought = self.player.as_ref().map(|player| player.seek(0.0).is_ok()).unwrap_or(true);
        if !sought {
            if let Some(song) = self.current.clone() {
                self.start_song(song);
            }
        }
    }

    /// eframe redraws only on input, but downloads, workers and playback move
    /// without any: ask for the next frame while any of them is running.
    fn schedule_repaint(&self, ctx: &egui::Context) {
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
        ].into_iter().filter(|running| *running).count();
        let transfers = self.downloads.len() + self.download_queue.len();
        if let Some(delay) = repaint_after(playing, transfers, requests) {
            ctx.request_repaint_after(delay);
        }
        if self.cfg.auto_cache_new && self.client.is_some() && self.catalog_scan.is_none() {
            ctx.request_repaint_after(self.next_catalog_check.saturating_duration_since(std::time::Instant::now()));
        }
    }

    /// How far a seek may go: the whole track, or only the downloaded part
    /// while the track is still arriving (`None` = not possible right now).
    fn seek_limit(&self, duration: f64) -> Option<f64> {
        let song = self.current.as_ref()?;
        match self.downloads.get(&song.id) {
            Some(handle) => handle.progress.seekable_fraction().map(|fraction| duration * f64::from(fraction)),
            None => Some(f64::INFINITY),
        }
    }

    fn seek_to(&mut self, target: f64, duration: f64) {
        let Some(position) = player::seek_position(target, duration, self.seek_limit(duration)) else {
            self.play_error = Some("перемотка станет доступна, когда трек докачается".into());
            return;
        };
        if let Some(player) = &self.player {
            if let Err(err) = player.seek(position) {
                self.play_error = Some(err);
            }
        }
    }

    fn stop_playback(&mut self) {
        if let Some(player) = &self.player {
            player.stop();
        }
        self.play_state = PlayState::Idle;
        self.current = None;
    }

    fn cover_texture(&mut self, cover_id: &str) -> Option<egui::TextureHandle> {
        if cover_id.is_empty() { return None; }
        if let Some(texture) = self.covers.get(cover_id) {
            return Some(texture.clone());
        }
        if self.cover_pending.contains(cover_id) { return None; }
        // At the limit the cover is simply asked for again on a later frame.
        if self.cover_inflight >= MAX_COVER_JOBS { return None; }
        // Files on disk (local drops and finished downloads): the artwork is
        // extracted from the file itself, so this works offline.
        if let Some(rel) = cover_id.strip_prefix(FILE_COVER_PREFIX) {
            self.cover_pending.insert(cover_id.to_owned());
            self.cover_inflight += 1;
            let id = cover_id.to_owned();
            let rel = rel.to_owned();
            let cache = self.cache.clone();
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            std::thread::spawn(move || {
                let image = cache.resolve_rel(&rel)
                    .and_then(|path| local::embedded_cover(&path))
                    .and_then(|bytes| decode_cover_sized(&bytes, ROW_COVER_PX));
                let _ = tx.send(CoverEvent::Loaded(generation, id, image));
            });
            return None;
        }
        if let Some(client) = self.client.clone() {
            self.cover_pending.insert(cover_id.to_owned());
            self.cover_inflight += 1;
            let id = cover_id.to_owned();
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            std::thread::spawn(move || {
                let image = client.cover_bytes(&id, COVER_PX).ok()
                    .and_then(|bytes| decode_cover(&bytes));
                let _ = tx.send(CoverEvent::Loaded(generation, id, image));
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for BeatApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_library();
        self.pump_catalog();
        self.pump_covers(ctx);
        self.pump_downloads();
        self.maybe_start_automatic_scan();
        self.update_playback(ctx);

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)) {
            self.run_search();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.settings_open {
                self.settings_open = false;
            } else if self.view != View::Albums {
                self.view = View::Albums;
            }
        }
        #[cfg(windows)]
        self.ui_title_bar(ctx);

        egui::TopBottomPanel::top("header")
            .exact_height(58.0)
            .frame(egui::Frame::none().fill(theme::bg()).inner_margin(egui::Margin::symmetric(16.0, 0.0)))
            .show_separator_line(true)
            .show(ctx, |ui| self.ui_header(ui));

        egui::TopBottomPanel::bottom("player")
            .exact_height(64.0)
            .frame(egui::Frame::none().fill(theme::bg()).inner_margin(egui::Margin::symmetric(14.0, 0.0)))
            .show_separator_line(true)
            .show(ctx, |ui| self.ui_player_bar(ui));

        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(26.0)
            .frame(egui::Frame::none().fill(theme::bg()).inner_margin(egui::Margin::symmetric(10.0, 0.0)))
            .show_separator_line(false)
            .show(ctx, |ui| self.ui_statusbar(ui));

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(238.0)
            .frame(egui::Frame::none().fill(theme::bg()).inner_margin(egui::Margin::symmetric(12.0, 14.0)))
            .show_separator_line(true)
            .show(ctx, |ui| self.ui_sidebar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::bg()).inner_margin(egui::Margin::symmetric(14.0, 12.0)))
            .show(ctx, |ui| self.ui_central(ui));

        self.ui_settings_modal(ctx);

        #[cfg(windows)]
        resize_edges(ctx);

        self.schedule_repaint(ctx);
    }
}

impl BeatApp {
    fn ui_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            match &self.icon_tex {
                Some(tex) => { ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(30.0, 30.0))); }
                None => { ui.label(egui::RichText::new("┌─┐\n│B│\n└─┘").font(egui::FontId::monospace(10.0)).color(theme::accent())); }
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.set_min_height(58.0);
                ui.add_space(12.0);
                ui.label(egui::RichText::new("BEAT // NAVIDROME КЛИЕНТ").size(13.0).color(theme::text()));
                ui.label(egui::RichText::new("музыка, кеш и плеер в одном окне").size(11.0).color(theme::dim()));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new("[ НАСТРОЙКИ ]").min_size(egui::vec2(0.0, 30.0))).clicked() {
                    self.open_settings();
                }
                let (cached, bytes) = self.cache.stats();
                ui.label(egui::RichText::new(format!("КЕШ {cached} · {}", human_size(bytes))).size(11.0).color(theme::dim()));
                let (label, color) = match &self.server_status {
                    None => ("СЕРВЕР…", theme::faint()),
                    Some(Ok(())) => ("СЕРВЕР ГОТОВ", theme::accent()),
                    Some(Err(_)) => ("НЕТ СВЯЗИ", theme::err()),
                };
                ui.label(egui::RichText::new(label).size(11.0).color(color));
            });
        });
    }

    fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let wordmark = banner::BANNER.trim_matches(['\r', '\n']);
            let lines: Vec<&str> = wordmark.lines().collect();
            let width_at = |ui: &egui::Ui, size: f32| -> f32 {
                lines.iter().map(|line| ui.fonts(|f| {
                    let job = egui::text::LayoutJob::single_section(
                        (*line).to_owned(),
                        egui::TextFormat { font_id: egui::FontId::monospace(size), ..Default::default() },
                    );
                    f.layout_job(job).size().x
                })).fold(0.0_f32, f32::max)
            };
            let mut size = self.banner_size;
            let avail = ui.available_width();
            if size <= 0.0 || (self.banner_fit - avail).abs() > 0.5 {
                size = 9.0_f32;
                while size > 4.0 && width_at(ui, size) > avail { size -= 0.25; }
                self.banner_size = size;
                self.banner_fit = avail;
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.vertical_centered(|ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(wordmark).font(egui::FontId::monospace(size)).color(theme::text()),
                ).halign(egui::Align::LEFT).wrap_mode(egui::TextWrapMode::Extend));
                ui.label(egui::RichText::new(banner::TAGLINE).font(egui::FontId::monospace(size)).color(theme::accent()));
            });
            ui.add_space(6.0);

            ui.spacing_mut().item_spacing.y = 4.0;
            theme::section_label(ui, "БИБЛИОТЕКА");
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new("новые альбомы")).clicked() {
                self.refresh_albums("newest", "НОВЫЕ АЛЬБОМЫ");
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new("случайные альбомы")).clicked() {
                self.refresh_albums("random", "СЛУЧАЙНЫЕ АЛЬБОМЫ");
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new("все артисты")).clicked() {
                self.fetch_artists();
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new("кеш на диске")).clicked() {
                self.view = View::Cached;
                self.refresh_disk();
            }

            theme::section_label(ui, "ЗАГРУЗКИ");
            if ui.add_enabled(self.client.is_some() && self.catalog_scan.is_none(),
                egui::Button::new("↓ скачать все песни").min_size(egui::vec2(ui.available_width(), 26.0)))
                .on_hover_text("Найти все треки Navidrome и поставить отсутствующие в очередь")
                .clicked() {
                self.start_catalog_scan(catalog::Mode::All);
            }
            if let Some(scan) = &self.catalog_scan {
                let label = if scan.baseline { "запоминаю библиотеку" } else { "проверяю библиотеку" };
                ui.label(egui::RichText::new(format!("{label}: {} альбомов", scan.albums)).size(10.0).color(theme::warn()));
                if scan.added > 0 {
                    ui.label(egui::RichText::new(format!("новых загрузок: {}", scan.added)).size(10.0).color(theme::dim()));
                }
                if ui.button("× остановить поиск").on_hover_text("Уже запущенные и добавленные в очередь загрузки продолжатся").clicked() {
                    self.cancel_catalog_scan();
                    self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    self.notice = Some("поиск остановлен; уже добавленные загрузки продолжаются".into());
                }
            }
            let active: Vec<(String, DlHandle)> = self.downloads.iter()
                .map(|(id, handle)| (id.clone(), handle.clone())).collect();
            if active.is_empty() && self.download_queue.is_empty() {
                ui.label(egui::RichText::new("нет активных загрузок").size(11.0).color(theme::faint()));
            }
            for (_, handle) in &active {
                let ratio = handle.progress.ratio();
                ui.label(egui::RichText::new(format!("↓ {}", clip(&handle.song.title, 24))).size(11.0).color(theme::dim()));
                let bar = match ratio {
                    Some(ratio) => format!("{:.0}%", ratio * 100.0),
                    None => human_size(handle.progress.snapshot().0),
                };
                ui.label(egui::RichText::new(bar).size(10.0).color(theme::accent()));
            }
            if !self.download_queue.is_empty() {
                theme::kv_row(ui, "в очереди", &format!("{}", self.download_queue.len()), theme::dim());
                if ui.button("× очистить очередь").on_hover_text("Отменить ожидающие загрузки; уже начатые продолжатся").clicked() {
                    // Otherwise the still-running catalog worker immediately
                    // fills the queue again on the next frame.
                    if self.catalog_scan.is_some() {
                        self.cancel_catalog_scan();
                        self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    }
                    self.download_queue.clear();
                    self.auto_queued.clear();
                    self.notice = Some("поиск и ожидающие загрузки отменены; начатые продолжаются".into());
                }
            }

            theme::section_label(ui, "КЕШ");
            let (cached, cached_bytes) = self.cache.stats();
            let (local, local_bytes) = self.local_stats;
            theme::kv_row(ui, "треков", &format!("{}", cached + local), theme::text());
            theme::kv_row(ui, "в кеше", &format!("{cached}"), theme::dim());
            theme::kv_row(ui, "локальных", &format!("{local}"), theme::dim());
            theme::kv_row(ui, "размер", &human_size(cached_bytes + local_bytes), theme::text());
            ui.add_space(4.0);
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new("открыть папку кеша")).clicked() {
                self.open_cache_folder();
            }
            let label = if self.cache_clear_armed { "× точно очистить кеш?" } else { "× очистить кеш" };
            if ui.add_enabled(self.downloads.is_empty() && self.download_queue.is_empty(),
                egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 26.0))).clicked() {
                if self.cache_clear_armed {
                    match self.cache.clear() {
                        Ok(()) => self.notice = Some("кеш очищен".into()),
                        Err(err) => self.notice = Some(err),
                    }
                    self.cache_clear_armed = false;
                    self.disk_entries = Arc::new(Vec::new());
                    self.refresh_local_stats();
                } else {
                    self.cache_clear_armed = true;
                }
            }

            if let Some(notice) = &self.notice {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(notice).size(10.0).color(theme::warn()));
            }
        });
    }

    fn ui_player_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let has_player = self.player.is_some();
            let paused = self.player.as_ref().map(|p| p.is_paused()).unwrap_or(false);
            if ui.add_enabled(has_player, egui::Button::new("◀◀")).on_hover_text("предыдущий").clicked() {
                self.prev_track();
            }
            // Fixed width: the pause glyph is wider than play, and the row
            // must not jump when toggling. ▮ (U+25AE) exists in Cascadia;
            // the old ❚ (U+275A) was not and fell back to replacement boxes.
            let play_label = if paused { "▶" } else { "▮▮" };
            let play_button = egui::Button::new(play_label).min_size(egui::vec2(38.0, 28.0));
            if ui.add_enabled(has_player, play_button).clicked() {
                if let Some(player) = &self.player {
                    if paused { player.resume(); } else { player.pause(); }
                }
            }
            if ui.add_enabled(has_player, egui::Button::new("▶▶")).on_hover_text("следующий").clicked() {
                self.next_track();
            }
            if ui.add_enabled(has_player, egui::Button::new("■")).on_hover_text("стоп").clicked() {
                self.stop_playback();
            }
            if mode_button(ui, "⇄", self.shuffle, "случайный порядок") {
                self.shuffle = !self.shuffle;
                self.refresh_shuffle();
            }
            let (repeat_label, repeat_hover) = match self.repeat {
                Repeat::Off => ("↻", "повтор выключен — нажмите: весь список"),
                Repeat::All => ("↻", "повтор всего списка — нажмите: одна песня"),
                Repeat::One => ("↻1", "повтор одной песни — нажмите: выключить"),
            };
            if mode_button(ui, repeat_label, self.repeat != Repeat::Off, repeat_hover) {
                self.repeat = match self.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
            }

            let duration = self.current.as_ref().map(|song| song.duration).unwrap_or(0.0);
            let position = self.player.as_ref().map(|p| p.position().min(duration)).unwrap_or(0.0);
            // While the slider is being dragged it shows the picked position.
            let shown = self.pending_seek.unwrap_or(position);
            let mut seek = shown;
            // A wider track than egui's default: scrubbing long tracks in a
            // ~100px slider was too fiddly. Kept adaptive so narrow windows
            // do not overflow.
            ui.spacing_mut().slider_width = (ui.available_width() * 0.26).clamp(170.0, 380.0);
            let slider = ui.add_enabled(has_player && self.current.is_some(), egui::Slider::new(&mut seek, 0.0..=duration.max(1.0)).show_value(false));
            if slider.changed() {
                self.pending_seek = Some(seek);
            }
            if let Some(target) = due_seek(self.pending_seek, ui.input(|i| i.pointer.any_down())) {
                self.pending_seek = None;
                self.seek_to(target, duration);
            }
            ui.label(egui::RichText::new(format!("{} / {}", format_time(shown), format_time(duration)))
                .size(11.0).color(theme::dim()));

            // Now-playing cover: taken from the file on disk (local or cached
            // download); streams without a finished file get the empty slot.
            let cover_rel: Option<String> = match &self.current {
                Some(song) => match song.id.strip_prefix(local::LOCAL_ID_PREFIX) {
                    Some(rel) => Some(rel.to_owned()),
                    None => self.cache.indexed_entry(&song.id).map(|entry| entry.path),
                },
                None => None,
            };
            let cover = match cover_rel {
                Some(rel) => self.cover_texture_of(&format!("{FILE_COVER_PREFIX}{rel}")),
                None => None,
            };
            let _ = row_cover(ui, 38.0, cover, egui::Sense::hover());

            let (title, subtitle) = match &self.current {
                Some(song) => (clip(&song.title, 40), track_subtitle(&song.artist, &song.album)),
                None => ("—".into(), "ничего не играет".into()),
            };
            ui.vertical(|ui| {
                ui.add_space(14.0);
                ui.label(egui::RichText::new(title).size(12.0).color(theme::text()));
                ui.label(egui::RichText::new(subtitle).size(10.0).color(theme::faint()));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Undo the wide seek width: the volume slider stays compact.
                ui.spacing_mut().slider_width = 110.0;
                let mut volume = self.player.as_ref().map(|p| p.volume()).unwrap_or(self.cfg.volume);
                let volume_response = ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false).text("громкость"));
                if volume_response.changed() {
                    if let Some(player) = &mut self.player { player.set_volume(volume); }
                    self.cfg.volume = volume;
                }
                if volume_response.drag_stopped() {
                    self.save();
                }
                let badge = if let Some(current) = &self.current {
                    if local::is_local_id(&current.id) { ("ЛОКАЛЬНО", theme::dim()) }
                    else if self.cache.is_indexed(&current.id) { ("В КЕШЕ", theme::accent()) }
                    else if self.downloads.contains_key(&current.id) { ("КЕШИРУЕТСЯ", theme::warn()) }
                    else if self.play_state_is_buffering() { ("БУФЕР…", theme::warn()) }
                    else { ("ПОТОК", theme::dim()) }
                } else { ("", theme::faint()) };
                ui.label(egui::RichText::new(badge.0).size(10.0).color(badge.1));
            });
        });
    }

    fn play_state_is_buffering(&self) -> bool {
        matches!(self.play_state, PlayState::Buffering { .. })
    }

    fn ui_statusbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let state = match &self.play_state {
                PlayState::Waiting(_) => ("● ОЖИДАНИЕ ЗАГРУЗКИ", theme::warn()),
                PlayState::Buffering { .. } => ("● БУФЕРИЗАЦИЯ", theme::warn()),
                PlayState::Playing => ("● ИГРАЕТ", theme::accent()),
                PlayState::Idle => ("● ГОТОВО", theme::accent()),
            };
            ui.label(egui::RichText::new(state.0).size(10.0).color(state.1));
            status_sep(ui);
            ui.label(egui::RichText::new(format!("треков в очереди: {}", self.play_queue.len()))
                .size(10.0).color(theme::dim()));
            status_sep(ui);
            ui.label(egui::RichText::new(format!("кеш: {}", self.cache.root().to_string_lossy()))
                .size(10.0).color(theme::faint()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.hyperlink_to(egui::RichText::new(TELEGRAM_URL.trim_start_matches("https://")).size(10.0), TELEGRAM_URL);
                ui.label(egui::RichText::new(format!("beat v{APP_VERSION} — by rercon prod.")).size(10.0).color(theme::faint()));
                if let Some(err) = &self.save_error {
                    status_sep(ui);
                    ui.label(egui::RichText::new(format!("конфиг не сохранён: {err}")).size(10.0).color(theme::err()));
                }
                if let Some(err) = &self.play_error {
                    status_sep(ui);
                    ui.label(egui::RichText::new(err).size(10.0).color(theme::err()));
                }
            });
        });
    }

    fn ui_central(&mut self, ui: &mut egui::Ui) {
        match self.view {
            View::Albums => {
                let title = self.album_list_title.clone();
                self.ui_album_grid(ui, &title, None);
            }
            View::Artist => self.ui_artist_albums(ui),
            View::Artists => self.ui_artists(ui),
            View::Album => self.ui_album(ui),
            View::Search => self.ui_search(ui),
            View::Cached => self.ui_cached(ui),
        }
    }

    fn ui_artists(&mut self, ui: &mut egui::Ui) {
        if self.loading.is_some() {
            ui.label(egui::RichText::new("загрузка…").size(12.0).color(theme::faint()));
        }
        let mut open = None;
        theme::section_label(ui, "АРТИСТЫ");
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 28.0, self.artists.len(), |ui, range| {
            for artist in &self.artists[range] {
                if profile_row(ui, &artist.name, artist.album_count as usize, false) {
                    open = Some(artist.clone());
                }
            }
        });
        if let Some(artist) = open {
            self.open_artist(artist);
        }
    }

    fn ui_artist_albums(&mut self, ui: &mut egui::Ui) {
        let (artist, albums) = match &self.artist_open {
            Some((artist, albums)) => (artist.clone(), albums.clone()),
            None => return,
        };
        ui.horizontal(|ui| {
            if ui.button("‹ назад").clicked() {
                self.view = View::Artists;
            }
            ui.label(theme::window_title(&format!("[ {} ]", artist.name)));
            ui.label(egui::RichText::new(format!("альбомов: {}", albums.len())).size(11.0).color(theme::faint()));
        });
        ui.add_space(4.0);
        self.ui_album_grid(ui, "", Some(albums));
    }

    fn ui_album_grid(&mut self, ui: &mut egui::Ui, title: &str, albums: Option<Vec<api::Album>>) {
        let albums = albums.unwrap_or_else(|| self.album_list.clone());
        if self.loading.is_some() && albums.is_empty() {
            ui.label(egui::RichText::new("загрузка…").size(12.0).color(theme::faint()));
            return;
        }
        if albums.is_empty() && self.client.is_none() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("СЕРВЕР НЕ НАСТРОЕН").size(14.0).color(theme::warn()));
                ui.add_space(6.0);
                ui.label(egui::RichText::new("укажите адрес Navidrome, логин и пароль — или закиньте файлы прямо в папку кеша")
                    .size(12.0).color(theme::dim()));
                ui.add_space(10.0);
                if ui.add(theme::accent_button("[ НАСТРОЙКИ ]")).clicked() {
                    self.open_settings();
                }
                if ui.button("[ КЕШ НА ДИСКЕ ]").clicked() {
                    self.view = View::Cached;
                    self.refresh_disk();
                }
                if ui.button("[ ОТКРЫТЬ ПАПКУ КЕША ]").clicked() {
                    self.open_cache_folder();
                }
            });
            return;
        }
        if !title.is_empty() {
            ui.label(theme::window_title(&format!("[ {title} ]")));
            ui.add_space(4.0);
        }
        let mut open = None;
        let mut play = None;
        let mut download = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                for album in &albums {
                    let card = album_card(ui, album, self.cover_texture_of(&album.cover_id));
                    match card {
                        AlbumCardAction::Open => open = Some(album.clone()),
                        AlbumCardAction::Play => play = Some(album.clone()),
                        AlbumCardAction::Download => download = Some(album.clone()),
                        AlbumCardAction::None => {}
                    }
                }
            });
        });
        if let Some(album) = open { self.browse_album(album); }
        if let Some(album) = play { self.play_album(album); }
        if let Some(album) = download { self.download_album(album); }
    }

    /// Cover lookup that does not borrow the whole app inside UI closures.
    fn cover_texture_of(&mut self, cover_id: &str) -> Option<egui::TextureHandle> {
        self.cover_texture(cover_id)
    }

    fn ui_album(&mut self, ui: &mut egui::Ui) {
        let (album, songs) = match &self.album_open {
            Some((album, songs)) => (album.clone(), songs.clone()),
            None => return,
        };
        ui.horizontal(|ui| {
            if ui.button("‹ назад").clicked() {
                self.view = View::Albums;
            }
            ui.label(theme::window_title(&format!("[ {} — {} ]",
                album.name, if album.artist.is_empty() { "?" } else { &album.artist })));
        });
        ui.horizontal(|ui| {
            if let Some(year) = (album.year > 0).then_some(album.year) {
                ui.label(egui::RichText::new(format!("{year}")).size(11.0).color(theme::faint()));
            }
            ui.label(egui::RichText::new(format!("треков: {}, {}", songs.len(), format_time(album.duration as f64)))
                .size(11.0).color(theme::faint()));
            if ui.add(theme::accent_button("[ СЛУШАТЬ ]")).clicked() {
                if let Some(song) = songs.first() {
                    self.play_song(song.clone(), songs.clone(), 0);
                }
            }
            if ui.button("[ СКАЧАТЬ АЛЬБОМ ]").clicked() {
                self.enqueue_album(&songs);
            }
        });
        ui.add_space(6.0);
        let row_height = 30.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_height, songs.len(), |ui, range| {
            for index in range {
                let song = &songs[index];
                ui.horizontal(|ui| {
                    ui.add_sized([26.0, 24.0], egui::Label::new(
                        egui::RichText::new(if song.track > 0 { format!("{:02}", song.track) } else { "·".into() })
                            .size(11.0).color(theme::faint())));
                    let playing = self.current.as_ref().is_some_and(|c| c.id == song.id);
                    let title_color = if playing { theme::accent() } else { theme::text() };
                    ui.add_sized([(ui.available_width() - 240.0).max(80.0), 24.0], egui::Label::new(
                        egui::RichText::new(clip(&song.title, 70)).size(12.0).color(title_color)));
                    ui.label(egui::RichText::new(format_time(song.duration)).size(11.0).color(theme::faint()));
                    let cached = self.cache.is_indexed(&song.id);
                    if cached {
                        ui.label(egui::RichText::new("в кеше").size(10.0).color(theme::accent()));
                    } else if self.downloads.contains_key(&song.id) {
                        ui.label(egui::RichText::new("качается").size(10.0).color(theme::warn()));
                    } else {
                        ui.add_space(38.0);
                    }
                    if ui.add_enabled(self.player.is_some(), egui::Button::new("▶")).clicked() {
                        self.play_song(song.clone(), songs.clone(), index);
                    }
                    if ui.add_enabled(!cached, egui::Button::new("↓")).on_hover_text("скачать в кеш").clicked() {
                        self.enqueue_download(song.clone());
                    }
                });
            }
        });
    }

    fn play_album(&mut self, album: api::Album) {
        if self.album_open.as_ref().is_some_and(|(open, _)| open.id == album.id) {
            let songs = self.album_open.as_ref().map(|(_, songs)| songs.clone()).unwrap_or_default();
            if let Some(first) = songs.first() {
                self.play_song(first.clone(), songs.clone(), 0);
            }
        } else {
            self.pending_download = None;
            self.pending_play = Some(album.id.clone());
            self.open_album(album);
        }
    }

    fn download_album(&mut self, album: api::Album) {
        if self.album_open.as_ref().is_some_and(|(open, _)| open.id == album.id) {
            let songs = self.album_open.as_ref().map(|(_, songs)| songs.clone()).unwrap_or_default();
            self.enqueue_album(&songs);
        } else {
            self.pending_play = None;
            self.pending_download = Some(album.id.clone());
            self.open_album(album);
        }
    }

    fn ui_search(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title("[ ПОИСК ]"));
            let field = ui.add_sized([420.0, 28.0],
                egui::TextEdit::singleline(&mut self.search_query).hint_text("артист, альбом или трек…"));
            if (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || ui.button("найти").clicked() {
                self.run_search();
            }
            if self.loading.is_some() {
                ui.label(egui::RichText::new("ищу…").size(11.0).color(theme::faint()));
            }
        });
        ui.add_space(6.0);
        let Some(search) = self.search_result.clone() else {
            ui.label(egui::RichText::new("введите запрос").size(11.0).color(theme::faint()));
            return;
        };
        let mut open_artist = None;
        let mut open_album = None;
        let mut play_album = None;
        let mut download_album = None;
        let mut play_song = None;
        let mut download = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if !search.artists.is_empty() {
                theme::section_label(ui, "АРТИСТЫ");
                for artist in &search.artists {
                    if profile_row(ui, &artist.name, artist.album_count as usize, false) {
                        open_artist = Some(artist.clone());
                    }
                }
            }
            if !search.albums.is_empty() {
                theme::section_label(ui, "АЛЬБОМЫ");
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                    for album in &search.albums {
                        let cover = self.cover_texture_of(&album.cover_id);
                        match album_card(ui, album, cover) {
                            AlbumCardAction::Open => open_album = Some(album.clone()),
                            AlbumCardAction::Play => play_album = Some(album.clone()),
                            AlbumCardAction::Download => download_album = Some(album.clone()),
                            AlbumCardAction::None => {}
                        }
                    }
                });
            }
            if !search.songs.is_empty() {
                theme::section_label(ui, "ТРЕКИ");
                for song in &search.songs {
                    ui.horizontal(|ui| {
                        ui.add_sized([(ui.available_width() - 150.0).max(80.0), 24.0], egui::Label::new(
                            egui::RichText::new(format!("{} — {}", clip(&song.title, 50), clip(&song.artist, 30)))
                                .size(12.0).color(theme::text())));
                        let cached = self.cache.is_indexed(&song.id);
                        if cached {
                            ui.label(egui::RichText::new("в кеше").size(10.0).color(theme::accent()));
                        } else {
                            ui.add_space(38.0);
                        }
                        if ui.add_enabled(self.player.is_some(), egui::Button::new("▶")).clicked() { play_song = Some(song.clone()); }
                        if ui.add_enabled(!cached, egui::Button::new("↓")).clicked() { download = Some(song.clone()); }
                    });
                }
            }
        });
        if let Some(artist) = open_artist { self.open_artist(artist); }
        if let Some(album) = open_album { self.browse_album(album); }
        if let Some(album) = play_album { self.play_album(album); }
        if let Some(album) = download_album { self.download_album(album); }
        if let Some(song) = play_song {
            self.play_song(song.clone(), vec![song], 0);
        }
        if let Some(song) = download { self.enqueue_download(song); }
    }

    fn ui_cached(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title("[ КЕШ НА ДИСКЕ ]"));
            if ui.button("обновить").clicked() {
                self.refresh_disk();
            }
            if self.disk_scan_id.is_some() {
                ui.label(egui::RichText::new("сканирую папку…").size(11.0).color(theme::faint()));
            } else {
                let count = self.disk_entries.len();
                let bytes: u64 = self.disk_entries.iter().map(DiskEntry::size).sum();
                ui.label(egui::RichText::new(format!("{count} треков · {}", human_size(bytes)))
                    .size(11.0).color(theme::faint()));
            }
            if ui.button("открыть папку").clicked() {
                self.open_cache_folder();
            }
        });
        ui.add_space(6.0);
        if self.disk_entries.is_empty() {
            if self.disk_scan_id.is_some() {
                ui.label(egui::RichText::new("ищу файлы…").size(12.0).color(theme::faint()));
            } else {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("В ПАПКЕ КЕША ПУСТО").size(13.0).color(theme::warn()));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(
                        "скачанные треки и закинутые вручную mp3 / flac / ogg / wav / m4a появятся здесь")
                        .size(11.0).color(theme::dim()));
                    ui.add_space(10.0);
                    if ui.add(theme::accent_button("[ ОТКРЫТЬ ПАПКУ ]")).clicked() {
                        self.open_cache_folder();
                    }
                });
            }
            return;
        }
        let mut play: Option<usize> = None;
        let mut remove: Option<String> = None;
        // A shared handle: cloning the whole list every frame was thousands of
        // string copies per redraw.
        let entries = self.disk_entries.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 30.0, entries.len(), |ui, range| {
            for index in range {
                let entry = &entries[index];
                ui.horizontal(|ui| {
                    let row_top = ui.cursor().top();
                    let cover = row_cover(ui, 24.0, self.cover_texture_of(&entry.cover_key()),
                        egui::Sense::click());
                    if self.player.is_some() && cover.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        play = Some(index);
                    }
                    // Constant slot: local files carry a folder mark, cached
                    // entries leave it empty so titles stay aligned.
                    if entry.is_local() { local_marker(ui); } else { ui.add_space(14.0); }
                    let playing = self.current.as_ref().is_some_and(|current| current.id == entry.id());
                    let color = if playing { theme::accent() } else { theme::text() };
                    // Left-aligned fixed-width slot (`add_sized` would centre
                    // the text inside it, leaving a huge gap after the icon).
                    let title_w = (ui.available_width() - 150.0).max(80.0);
                    let title = ui.allocate_ui_with_layout(
                        egui::vec2(title_w, 24.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| ui.add(egui::Label::new(egui::RichText::new(entry_label(entry))
                            .size(12.0).color(color)).truncate()),
                    ).inner;
                    if let Some(path) = entry.local_path() {
                        title.on_hover_text(path.display().to_string());
                    }
                    // Size and the play slot are right-aligned: the button
                    // appears on row hover (so it never sits in a random spot
                    // between rows) and stays put once shown.
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.min_rect().left(), row_top),
                        egui::pos2(ui.max_rect().right(), row_top + 30.0),
                    );
                    let row_hovered = ui.rect_contains_pointer(row_rect);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let DiskEntry::Cached(cached) = entry {
                            if ui.button("×").on_hover_text("удалить из кеша").clicked() {
                                remove = Some(cached.id.clone());
                            }
                        } else {
                            // Keep the play slot at the same x as cached rows.
                            ui.add_space(27.0);
                        }
                        let slot = ui.allocate_exact_size(egui::vec2(30.0, 24.0), egui::Sense::click());
                        if row_hovered || playing {
                            let color = if playing { theme::accent() } else { theme::dim() };
                            ui.painter().text(slot.0.center(), egui::Align2::CENTER_CENTER, "▶",
                                egui::FontId::proportional(13.0), color);
                            if self.player.is_some() && slot.1.clicked() {
                                play = Some(index);
                            }
                            let _ = slot.1.on_hover_text("слушать");
                        }
                        ui.label(egui::RichText::new(human_size(entry.size())).size(10.0).color(theme::faint()));
                    });
                });
            }
        });
        if let Some(index) = play {
            // One queue over the whole on-disk list, so next/prev walk it.
            let song = entries[index].to_song();
            let play_queue = entries.iter().map(DiskEntry::to_song).collect::<Vec<_>>();
            self.play_song(song, play_queue, index);
        }
        if let Some(id) = remove {
            match self.cache.remove(&id) {
                Ok(()) => {
                    self.disk_entries = Arc::new(self.disk_entries.iter()
                        .filter(|entry| entry.id() != id).cloned().collect());
                    self.notice = Some("удалено из кеша".into());
                }
                Err(err) => self.notice = Some(err),
            }
        }
    }

    fn ui_settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open { return; }
        let mut save = false;
        let mut cancel = false;
        let mut check = false;
        let mut pick_dir = false;
        let save_error = self.save_error.clone();
        let checking = self.settings_checking;
        let check_result = self.settings_check.clone();
        let mut show_password = self.show_password;
        {
            let draft = &mut self.settings_draft;
            egui::Window::new(theme::window_title("[ НАСТРОЙКИ СЕРВЕРА ]"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(620.0);
                    field_label(ui, "> адрес Navidrome (https://…)");
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.server_url).font(theme::field_font())
                            .hint_text("https://music.example.com"));
                    field_label(ui, "> логин");
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.user).font(theme::field_font()));
                    field_label(ui, "> пароль");
                    ui.horizontal(|ui| {
                        let field = ui.add_sized([420.0, 30.0],
                            egui::TextEdit::singleline(&mut draft.password).font(theme::field_font())
                                .password(!show_password));
                        if field.changed() {
                            draft.forget_unreadable_password();
                        }
                        if ui.add(egui::Button::new(if show_password { "[ скрыть ]" } else { "[ показать ]" })).clicked() {
                            show_password = !show_password;
                        }
                    });
                    field_label(ui, "> папка кеша");
                    ui.horizontal(|ui| {
                        let dir = draft.cache_dir.clone();
                        let hint = if dir.trim().is_empty() {
                            format!("по умолчанию: {}", Config::default().cache_root().to_string_lossy())
                        } else { dir };
                        ui.add_sized([420.0, 30.0], egui::Label::new(
                            egui::RichText::new(hint).size(11.0).color(theme::dim())));
                        if ui.button("[ выбрать ]").clicked() { pick_dir = true; }
                        if ui.button("[ сбросить ]").clicked() { draft.cache_dir.clear(); }
                    });
                    field_label(ui, "> формат загрузки");
                    egui::ComboBox::from_id_salt("format")
                        .selected_text(egui::RichText::new(draft.stream_format.label()).size(12.0))
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Raw, "оригинал");
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Mp3, "mp3 (транскод сервером)");
                        });
                    if draft.stream_format == StreamFormat::Mp3 {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("битрейт, kbps").size(11.0).color(theme::dim()));
                            ui.add(egui::DragValue::new(&mut draft.bit_rate).speed(16.0).range(64..=320));
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("параллельных загрузок").size(11.0).color(theme::dim()));
                        ui.add(egui::DragValue::new(&mut draft.parallel_downloads).speed(1.0).range(1..=3));
                    });
                    ui.add_space(6.0);
                    ui.checkbox(&mut draft.auto_cache_new, "автоматически кешировать новые песни");
                    ui.label(egui::RichText::new("При первом включении запоминает текущие треки; затем проверяет сервер каждые 10 минут, пока BEAT открыт.")
                        .size(10.0).color(theme::dim()));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!checking, egui::Button::new("[ ПРОВЕРИТЬ СВЯЗЬ ]")).clicked() {
                            check = true;
                        }
                        if checking {
                            ui.label(egui::RichText::new("проверяю…").size(11.0).color(theme::faint()));
                        } else if let Some(result) = &check_result {
                            let (text, color) = match result {
                                Ok(()) => ("связь есть".into(), theme::accent()),
                                Err(err) => (err.clone(), theme::err()),
                            };
                            ui.label(egui::RichText::new(text).size(11.0).color(color));
                        }
                        if let Some(err) = &save_error {
                            ui.label(egui::RichText::new(format!("конфиг не сохранён: {err}")).size(10.0).color(theme::err()));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(theme::accent_button("[ СОХРАНИТЬ ]")).clicked() { save = true; }
                            if ui.add(egui::Button::new("[ ОТМЕНА ]")).clicked() { cancel = true; }
                        });
                    });
                });
        }
        if pick_dir {
            if let Some(dir) = pick_folder() {
                self.settings_draft.cache_dir = dir;
            }
        }

        self.show_password = show_password;
        if check {
            let draft = self.settings_draft.clone();
            self.check_connection(&draft);
        }
        if save {
            self.save_settings();
        } else if cancel {
            self.settings_open = false;
        }
    }

    /// Borderless-window title bar, same as STRIKE/SNATCH.
    #[cfg(windows)]
    fn ui_title_bar(&mut self, ctx: &egui::Context) {
        const THEME_BTN_WIDTH: f32 = 56.0;
        egui::TopBottomPanel::top("app_titlebar")
            .exact_height(42.0)
            .show_separator_line(true)
            .frame(egui::Frame::none()
                .fill(ctx.style().visuals.window_fill)
                .inner_margin(egui::Margin::symmetric(14.0, 0.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let title_width = (ui.available_width() - 3.0 * 40.0 - THEME_BTN_WIDTH).max(0.0);
                    let (rect, drag) = ui.allocate_exact_size(
                        egui::vec2(title_width, 42.0), egui::Sense::click_and_drag());
                    ui.painter().text(
                        egui::pos2(rect.left(), rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        "BEAT — by rercon prod.",
                        egui::FontId::new(13.0, egui::FontFamily::Name("title".into())),
                        ui.visuals().weak_text_color(),
                    );
                    if drag.double_clicked() {
                        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    } else if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    let label = if self.dark_mode { "день" } else { "ночь" };
                    if ui.add_sized([THEME_BTN_WIDTH, 38.0], egui::Button::new(label).frame(false))
                        .on_hover_text(if self.dark_mode { "Светлая тема" } else { "Тёмная тема" })
                        .clicked()
                    {
                        self.toggle_theme(ctx);
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text("Свернуть").clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text("Развернуть / восстановить").clicked()
                    {
                        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    }
                    if ui.add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                        .on_hover_text("Закрыть").clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }
}

// ---------------------------------------------------------------------------
// Views and small widgets
// ---------------------------------------------------------------------------

enum AlbumCardAction {
    None,
    Open,
    Play,
    Download,
}

fn album_card(ui: &mut egui::Ui, album: &api::Album, cover: Option<egui::TextureHandle>) -> AlbumCardAction {
    let mut action = AlbumCardAction::None;
    egui::Frame::none()
        .stroke(egui::Stroke::new(1.0, theme::line()))
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.set_width(168.0);
            let (rect, response) = ui.allocate_exact_size(egui::vec2(168.0, 168.0), egui::Sense::click());
            match cover {
                Some(texture) => {
                    ui.painter().image(texture.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE);
                }
                None => {
                    ui.painter().rect_filled(rect, 0.0, theme::field());
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "♪",
                        egui::FontId::proportional(36.0), theme::faint());
                }
            }
            if response.clicked() {
                action = AlbumCardAction::Open;
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.add_space(4.0);
            ui.label(egui::RichText::new(clip(&album.name, 24)).size(12.0).color(theme::text()));
            ui.label(egui::RichText::new(clip(&album.artist, 24)).size(10.0).color(theme::faint()));
            ui.horizontal(|ui| {
                if ui.button("▶").on_hover_text("слушать").clicked() {
                    action = AlbumCardAction::Play;
                }
                if ui.button("↓").on_hover_text("скачать альбом").clicked() {
                    action = AlbumCardAction::Download;
                }
                if album.year > 0 {
                    ui.label(egui::RichText::new(format!("{}", album.year)).size(10.0).color(theme::faint()));
                }
            });
        });
    action
}

fn profile_row(ui: &mut egui::Ui, name: &str, count: usize, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let title = clip_to_width(ui, name, egui::FontId::monospace(13.0), (rect.width() - 44.0).max(10.0));
    let painter = ui.painter();
    if active || response.hovered() {
        painter.rect_filled(rect, 0.0, theme::lift());
    }
    painter.text(rect.left_center() + egui::vec2(7.0, 0.0), egui::Align2::LEFT_CENTER,
        if active { "●" } else { "○" }, egui::FontId::monospace(11.0),
        if active { theme::accent() } else { theme::faint() });
    painter.text(rect.left_center() + egui::vec2(23.0, 0.0), egui::Align2::LEFT_CENTER,
        title, egui::FontId::monospace(13.0),
        if active { theme::text() } else { theme::dim() });
    if count > 0 {
        painter.text(rect.right_center() - egui::vec2(7.0, 0.0), egui::Align2::RIGHT_CENTER,
            format!("{count:02}"), egui::FontId::monospace(10.0), theme::faint());
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// Fixed-width toggle for the player modes (shuffle/repeat): accent when on,
/// faint when off, so the transport row never shifts.
fn mode_button(ui: &mut egui::Ui, glyph: &str, active: bool, hover: &str) -> bool {
    let color = if active { theme::accent() } else { theme::faint() };
    let stroke = if active { theme::accent() } else { theme::line() };
    ui.add(egui::Button::new(egui::RichText::new(glyph).color(color))
        .stroke(egui::Stroke::new(1.0, stroke))
        .min_size(egui::vec2(34.0, 28.0)))
        .on_hover_text(hover)
        .clicked()
}

/// Square cover thumbnail with the "♪" placeholder used across the app. The
/// response lets callers make the cover itself clickable (list rows).
fn row_cover(ui: &mut egui::Ui, size: f32, texture: Option<egui::TextureHandle>, sense: egui::Sense) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
    match texture {
        Some(texture) => {
            ui.painter().image(texture.id(), rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, theme::field());
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "♪",
                egui::FontId::proportional(size * 0.5), theme::faint());
        }
    }
    response
}

/// Vector "local file" mark: a small folder glyph drawn with the painter
/// (no SVG-renderer dependency), meaning "dropped into the cache folder by
/// hand, not downloaded from the server".
fn local_marker(ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    let color = theme::accent();
    let x0 = rect.left() + 1.0;
    let x1 = rect.right() - 1.0;
    let tab = egui::Rect::from_min_max(
        egui::pos2(x0, rect.top() + 3.0), egui::pos2(x0 + 6.0, rect.top() + 6.0));
    let body = egui::Rect::from_min_max(
        egui::pos2(x0, rect.top() + 5.0), egui::pos2(x1, rect.bottom() - 3.0));
    painter.rect_filled(tab, 0.0, color);
    painter.rect_filled(body, 1.0, color);
    let _ = response.on_hover_text("локальный файл — лежит в папке кеша, не скачан с сервера");
}

fn status_sep(ui: &mut egui::Ui) {
    ui.label(egui::RichText::new("│").size(10.0).color(theme::status_sep()));
}

fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text).size(11.0).color(theme::dim()));
    ui.add_space(3.0);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_label(format: StreamFormat) -> String {
    match format {
        StreamFormat::Raw => "raw".into(),
        StreamFormat::Mp3 => "mp3".into(),
    }
}

/// One row of the on-disk list: a downloaded cache entry or a hand-dropped
/// local file found in the cache folder.
#[derive(Clone)]
enum DiskEntry {
    Cached(CachedTrack),
    Local(LocalTrack),
}

impl DiskEntry {
    fn id(&self) -> &str {
        match self { Self::Cached(entry) => &entry.id, Self::Local(track) => &track.id }
    }
    fn title(&self) -> &str {
        match self { Self::Cached(entry) => &entry.title, Self::Local(track) => &track.title }
    }
    fn artist(&self) -> &str {
        match self { Self::Cached(entry) => &entry.artist, Self::Local(track) => &track.artist }
    }
    fn album(&self) -> &str {
        match self { Self::Cached(entry) => &entry.album, Self::Local(track) => &track.album }
    }
    fn size(&self) -> u64 {
        match self { Self::Cached(entry) => entry.size, Self::Local(track) => track.size }
    }
    fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
    fn local_path(&self) -> Option<&std::path::Path> {
        match self { Self::Cached(_) => None, Self::Local(track) => Some(&track.path) }
    }
    /// Cover key for the artwork embedded in this row's file on disk.
    fn cover_key(&self) -> String {
        let rel = match self { Self::Cached(entry) => &entry.path, Self::Local(track) => &track.rel };
        format!("{FILE_COVER_PREFIX}{rel}")
    }
    fn to_song(&self) -> api::Song {
        match self {
            Self::Cached(entry) => cached_to_song(entry),
            Self::Local(track) => track.to_song(),
        }
    }
}

/// `Title — Artist [Album]` for a list row, leaving out whatever is missing:
/// untagged files must not show empty `[]` or dangling dashes.
fn entry_label(entry: &DiskEntry) -> String {
    let mut label = clip(entry.title(), 50);
    if !entry.artist().trim().is_empty() {
        label.push_str(" — ");
        label.push_str(&clip(entry.artist(), 30));
    }
    if !entry.album().trim().is_empty() {
        label.push_str(" [");
        label.push_str(&clip(entry.album(), 30));
        label.push(']');
    }
    label
}

/// `Artist · Album` for the player bar, with the same missing-part rules.
fn track_subtitle(artist: &str, album: &str) -> String {
    match (artist.trim().is_empty(), album.trim().is_empty()) {
        (false, false) => format!("{} · {}", clip(artist, 26), clip(album, 26)),
        (false, true) => clip(artist, 26),
        (true, false) => clip(album, 26),
        (true, true) => String::new(),
    }
}

/// Fisher-Yates shuffle with a tiny xorshift PRNG (no `rand` dependency);
/// `current` appears in the order exactly once, like every other index.
fn shuffled_order(len: usize, current: usize) -> (Vec<usize>, usize) {
    let mut order: Vec<usize> = (0..len).collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    seed ^= u64::from(std::process::id()).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    if seed == 0 { seed = 0x9e37_79b9_7f4a_7c15; }
    for i in (1..len).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        order.swap(i, seed as usize % (i + 1));
    }
    let pos = order.iter().position(|&index| index == current).unwrap_or(0);
    (order, pos)
}

/// Relative paths of indexed cache files, so the local walk can skip them.
fn indexed_paths(cache: &Cache) -> HashSet<String> {
    cache.list().iter().map(|entry| entry.path.clone()).collect()
}

/// Indexed downloads plus local files, sorted artist/album/title like the
/// cache list itself.
fn merge_disk_entries(indexed: Vec<CachedTrack>, local: Vec<LocalTrack>) -> Vec<DiskEntry> {
    let mut entries: Vec<DiskEntry> = indexed.into_iter().map(DiskEntry::Cached)
        .chain(local.into_iter().map(DiskEntry::Local))
        .collect();
    entries.sort_by(|a, b| a.artist().to_lowercase().cmp(&b.artist().to_lowercase())
        .then_with(|| a.album().to_lowercase().cmp(&b.album().to_lowercase()))
        .then_with(|| a.title().to_lowercase().cmp(&b.title().to_lowercase())));
    entries
}

fn cached_to_song(entry: &CachedTrack) -> api::Song {
    api::Song {
        id: entry.id.clone(),
        title: entry.title.clone(),
        artist: entry.artist.clone(),
        album: entry.album.clone(),
        duration: entry.duration,
        suffix: entry.suffix.clone(),
        ..api::Song::default()
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn clip_to_width(ui: &egui::Ui, text: &str, font: egui::FontId, max_w: f32) -> String {
    let width = |s: &str| ui.fonts(|f| f.layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE).rect.width());
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&cand) <= max_w { lo = mid; } else { hi = mid - 1; }
    }
    if lo == 0 { return "…".to_owned(); }
    chars[..lo].iter().collect::<String>() + "…"
}

/// How soon the UI must run again without any input. Workers, downloads and
/// the player change state on their own, and eframe redraws only on input or
/// a repaint request: without this a finished track never advances and the
/// clock stands still until the mouse moves.
fn repaint_after(playing: bool, transfers: usize, requests: usize) -> Option<std::time::Duration> {
    (playing || transfers > 0 || requests > 0).then_some(UI_TICK)
}

/// Whether the on-disk list should be rescanned now. Every finished download
/// marks it stale, but a rescan reads the whole folder: during an album
/// download it runs at most every few seconds instead of once per track.
fn disk_refresh_due(stale: bool, scanning: bool, downloading: bool, since_last: std::time::Duration) -> bool {
    stale && !scanning && (!downloading || since_last >= DISK_REFRESH_EVERY)
}

/// A position picked on the seek slider is sent once the pointer is released,
/// not on every frame of the drag (each seek resets the decoder and, in a
/// download in progress, may wait for the network).
fn due_seek(pending: Option<f64>, pointer_down: bool) -> Option<f64> {
    if pointer_down { None } else { pending }
}

fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00".into();
    }
    let seconds = seconds as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn human_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        format!("{kb:.0} КБ")
    } else if kb < 1024.0 * 1024.0 {
        format!("{:.1} МБ", kb / 1024.0)
    } else {
        format!("{:.2} ГБ", kb / 1024.0 / 1024.0)
    }
}

/// Cover bytes -> egui image, size-capped and panic-guarded (decoders run on
/// server-supplied pixels and on arbitrary embedded artwork).
fn decode_cover(bytes: &[u8]) -> Option<egui::ColorImage> {
    decode_cover_sized(bytes, COVER_PX)
}

/// A small file can describe a gigantic picture: cap the dimensions and the
/// memory before anything is allocated for it.
#[allow(clippy::field_reassign_with_default)]
fn cover_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_COVER_SIDE);
    limits.max_image_height = Some(MAX_COVER_SIDE);
    limits.max_alloc = Some(MAX_COVER_DECODE_BYTES);
    limits
}

fn decode_cover_sized(bytes: &[u8], size: u32) -> Option<egui::ColorImage> {
    const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;
    if bytes.is_empty() || bytes.len() > MAX_COVER_BYTES {
        return None;
    }
    HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        reader.limits(cover_limits());
        let image = reader.decode().ok()?;
        let thumb = image.thumbnail(size, size).to_rgba8();
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [thumb.width() as usize, thumb.height() as usize], thumb.as_raw()))
    }));
    HANDLED_PANIC.with(|handled| handled.set(false));
    result.ok().flatten()
}

thread_local! {
    /// Set while a panic is expected and handled locally (image and audio
    /// decoding, local tag probing), so the global panic dialog does not pop
    /// up for one broken file.
    pub(crate) static HANDLED_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(windows)]
fn open_in_explorer(path: &str) {
    let explorer = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("explorer.exe");
    let _ = std::process::Command::new(explorer).arg(path).spawn();
}

#[cfg(not(windows))]
fn open_in_explorer(_path: &str) {}

#[cfg(windows)]
fn pick_folder() -> Option<String> {
    rfd::FileDialog::new().pick_folder().map(|path| path.to_string_lossy().into_owned())
}

#[cfg(not(windows))]
fn pick_folder() -> Option<String> { None }

const ICON_PNG: &[u8] = include_bytes!("../icons/beat-256.png");

fn app_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let rgba = img.into_rgba8();
    Some(egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

fn app_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let rgba = img.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    Some(ctx.load_texture("app-icon", egui::ColorImage::from_rgba_unmultiplied(size, &pixels), egui::TextureOptions::LINEAR))
}

/// Borderless windows have no native resize border; begin the OS resize on
/// mouse-down so an outward drag is not lost.
#[cfg(windows)]
fn resize_edges(ctx: &egui::Context) {
    let screen = ctx.screen_rect();
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) { return; }
    let e = 10.0;
    let origin = screen.min;
    let w = screen.width();
    let h = screen.height();
    let handles = [
        (origin, egui::vec2(e, e), egui::ResizeDirection::NorthWest),
        (origin + egui::vec2(e, 0.0), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::North),
        (origin + egui::vec2(w - e, 0.0), egui::vec2(e, e), egui::ResizeDirection::NorthEast),
        (origin + egui::vec2(0.0, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::West),
        (origin + egui::vec2(w - e, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::East),
        (origin + egui::vec2(0.0, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthWest),
        (origin + egui::vec2(e, h - e), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::South),
        (origin + egui::vec2(w - e, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthEast),
    ];
    for (i, (pos, size, direction)) in handles.into_iter().enumerate() {
        egui::Area::new(egui::Id::new(("window_resize", i)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(size, egui::Sense::drag());
                if response.hovered() {
                    let icon = match direction {
                        egui::ResizeDirection::North | egui::ResizeDirection::South => egui::CursorIcon::ResizeVertical,
                        egui::ResizeDirection::East | egui::ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
                        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => egui::CursorIcon::ResizeNwSe,
                        egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => egui::CursorIcon::ResizeNeSw,
                    };
                    ctx.set_cursor_icon(icon);
                }
                if response.is_pointer_button_down_on() && ctx.input(|input| input.pointer.primary_pressed()) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                }
            });
    }
}

/// A windows-subsystem exe has no console: a startup failure must be shown in
/// a MessageBox or the process dies silently.
#[cfg(windows)]
fn fatal_dialog(title: &str, text: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, mb_type: u32) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;
    let wide = |s: &str| -> Vec<u16> { std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect() };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), MB_ICONERROR);
    }
}

#[cfg(not(windows))]
fn fatal_dialog(_title: &str, _text: &str) {}

/// Two copies would race on the cache index and config; hold a per-session
/// named mutex so the second window is refused.
#[cfg(windows)]
fn acquire_single_instance(name: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn CreateMutexW(attrs: *const c_void, initially_owned: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(format!("не удалось создать мьютекс: {}", std::io::Error::last_os_error()));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("BEAT уже запущен".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn acquire_single_instance(_name: &str) -> Result<(), String> { Ok(()) }

fn main() -> eframe::Result {
    if let Err(err) = acquire_single_instance("Local\\beat-single-instance") {
        fatal_dialog("BEAT", &format!("{err}.\n\nЗакройте уже открытое окно BEAT и запустите это снова."));
        std::process::exit(1);
    }
    std::panic::set_hook(Box::new(|info| {
        if HANDLED_PANIC.with(|handled| handled.get()) { return; }
        fatal_dialog("BEAT — внутренняя ошибка", &format!("BEAT не смог продолжить работу.\n\n{info}"));
    }));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([1000.0, 640.0])
        .with_resizable(true)
        .with_title("BEAT — by rercon prod.");
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native("BEAT", options, Box::new(|cc| Ok(Box::new(BeatApp::new(cc)))));
    if let Err(e) = &result {
        fatal_dialog("BEAT — не удалось открыть окно",
            &format!("Причина: {e}\n\nНа виртуальной машине или в RDP-сеансе это обычно означает, \
                      что недоступен OpenGL 2.1+. Включите 3D-ускорение в настройках ВМ либо \
                      запустите программу на обычном рабочем столе."));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_and_size_formatting() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(61.4), "1:01");
        assert_eq!(format_time(-3.0), "0:00");
        assert_eq!(human_size(4 * 1024), "4 КБ");
        assert!(human_size(5 * 1024 * 1024).contains("МБ"));
    }

    #[test]
    fn the_ui_keeps_ticking_only_while_something_is_going_on() {
        assert_eq!(repaint_after(false, 0, 0), None, "an idle window must not redraw by itself");
        let playing = repaint_after(true, 0, 0).expect("a playing track needs its clock");
        assert!(repaint_after(false, 2, 0).is_some(), "downloads in progress");
        assert!(repaint_after(false, 0, 1).is_some(), "a request in flight");
        assert!(playing >= std::time::Duration::from_millis(50) && playing <= std::time::Duration::from_millis(250),
            "{playing:?}");
    }

    #[test]
    fn the_disk_list_is_rescanned_at_most_every_few_seconds_during_downloads() {
        let secs = std::time::Duration::from_secs;
        assert!(!disk_refresh_due(false, false, false, secs(60)), "nothing changed");
        assert!(!disk_refresh_due(true, true, false, secs(60)), "a scan is already running");
        assert!(disk_refresh_due(true, false, false, secs(0)), "downloads finished: refresh at once");
        assert!(!disk_refresh_due(true, false, true, secs(1)), "still downloading, scanned a second ago");
        assert!(disk_refresh_due(true, false, true, secs(6)), "still downloading, but it has been a while");
    }

    #[test]
    fn oversized_cover_dimensions_are_refused_before_decoding() {
        let png = |width: u32, height: u32| {
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            out
        };
        assert!(decode_cover_sized(&png(64, 64), 32).is_some(), "an ordinary cover must decode");
        assert!(decode_cover_sized(&png(9_000, 8), 32).is_none(), "9000 px wide accepted");
        assert!(decode_cover_sized(&png(8, 9_000), 32).is_none(), "9000 px tall accepted");
    }

    #[test]
    fn a_slider_seek_is_sent_only_after_the_pointer_is_released() {
        assert_eq!(due_seek(Some(42.0), true), None, "still dragging");
        assert_eq!(due_seek(Some(42.0), false), Some(42.0));
        assert_eq!(due_seek(None, false), None);
        assert_eq!(due_seek(None, true), None);
    }

    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(clip("привет", 4), "при…");
        assert_eq!(clip("ok", 4), "ok");
    }

    #[test]
    fn button_font_has_every_transport_glyph() {
        let ctx = egui::Context::default();
        theme::apply(&ctx, true);
        ctx.begin_pass(egui::RawInput::default());
        let button = egui::FontId::new(12.5, egui::FontFamily::Name("button".into()));
        let missing: Vec<char> = "▶▮◀■⇄↻1♪×"
            .chars()
            .filter(|c| !ctx.fonts(|fonts| fonts.has_glyph(&button, *c)))
            .collect();
        let _ = ctx.end_pass();
        assert!(missing.is_empty(), "missing button glyphs: {missing:?}");
    }

    #[test]
    fn shuffle_order_is_a_permutation_that_contains_the_current_track() {
        let (order, pos) = shuffled_order(8, 3);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>());
        assert_eq!(order[pos], 3);
        let (order, pos) = shuffled_order(1, 0);
        assert_eq!(order, vec![0]);
        assert_eq!(pos, 0);
        assert!(shuffled_order(0, 0).0.is_empty());
    }

    #[test]
    fn labels_skip_missing_artist_and_album() {
        let bare = DiskEntry::Local(LocalTrack {
            id: "local:x.mp3".into(), path: std::path::PathBuf::from("x"), rel: "x.mp3".into(),
            title: "T".into(), artist: String::new(), album: String::new(),
            duration: 1.0, suffix: "mp3".into(), size: 1,
        });
        assert_eq!(entry_label(&bare), "T");
        let tagged = DiskEntry::Cached(CachedTrack { id: "1".into(), path: "x".into(),
            title: "T".into(), artist: "A".into(), album: "B".into(), duration: 1.0,
            suffix: "mp3".into(), size: 1, format: "raw".into() });
        assert_eq!(entry_label(&tagged), "T — A [B]");
        assert_eq!(track_subtitle("", ""), "");
        assert_eq!(track_subtitle("A", ""), "A");
        assert_eq!(track_subtitle("", "B"), "B");
        assert_eq!(track_subtitle("A", "B"), "A · B");
    }

    #[test]
    fn cached_entry_roundtrips_into_a_song() {
        let entry = CachedTrack { id: "7".into(), title: "T".into(), artist: "A".into(),
            album: "B".into(), duration: 12.0, suffix: "flac".into(), path: "x".into(),
            size: 1, format: "raw".into() };
        let song = cached_to_song(&entry);
        assert_eq!(song.id, "7");
        assert_eq!(song.suffix, "flac");
        assert_eq!(song.artist, "A");
    }
}
