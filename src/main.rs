// GUI exe: no console window when launched from Explorer. Panics are routed
// to a MessageBox in main() (windows-subsystem exes die silently otherwise).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod banner;
mod cache;
mod catalog;
mod config;
mod covers;
mod demo;
mod i18n;
mod local;
mod media;
mod media_keys;
mod opus;
mod playback;
mod player;
mod session;
mod stats;
mod theme;

use cache::{Cache, CachedTrack, DlEvent, DlHandle};
use config::{Config, StreamFormat};
use eframe::egui;
use local::LocalTrack;
use session::Repeat;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// Lock that recovers from poisoning. Every mutex in this app guards a plain
/// collection, so a panic elsewhere must not turn each later frame into
/// another panic: the data behind the flag is still consistent enough to keep
/// the app running, which is what a media player should do.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
/// Longest «часто прослушиваемые» list.
const MAX_FREQUENT: usize = 100;

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
    LibrarySaved(String, Result<(), String>),
}

type PreparedEvent = (u64, usize, Result<opus::AudioSource, String>);

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
    /// One list of every song: server, cached downloads and hand-dropped files.
    Library,
    /// Top tracks by locally counted listens.
    Frequent,
}

enum PlayState {
    Idle,
    /// The download slots are full; playback waits for its queued download.
    Waiting(api::Song),
    /// Playing starts once enough of the download has arrived. `attempted_at` is
    /// how many bytes were there at the last decoder attempt, so the next one
    /// waits for real progress instead of running every frame.
    Buffering {
        handle: DlHandle,
        needed: u64,
        attempted_at: u64,
    },
    Playing,
}

/// Playback waiting for its first decoder attempt: the initial buffer size, no
/// attempt made yet.
fn new_buffering(handle: DlHandle) -> PlayState {
    PlayState::Buffering { handle, needed: cache::PLAYBACK_BUFFER_BYTES, attempted_at: 0 }
}

/// Whether building the decoder again is worth it.
///
/// `needed` bytes must have arrived, and at least `needed` *more* since the last
/// attempt. Without the second condition the requirement grows past the file
/// size and every following frame would rebuild the decoder on the UI thread
/// until the download ends. A finished download always gets a last attempt.
fn buffering_attempt_ready(downloaded: u64, needed: u64, attempted_at: u64, finished: bool) -> bool {
    finished || (downloaded >= needed && downloaded >= attempted_at.saturating_add(needed))
}

/// Repeat-one restarts the track, but only one that actually played: a valid
/// header over zero samples never advances, and restarting it on every tick
/// would spin the UI thread forever.
fn should_repeat_one(repeat: Repeat, played_secs: f64) -> bool {
    repeat == Repeat::One && played_secs > 0.0
}

/// A track shorter than this is not counted as a listen. It also keeps a file
/// that decodes but yields no audio from being counted over and over while
/// repeat-one restarts it.
const MIN_LISTEN_SECS: f64 = 1.0;

struct CatalogScan {
    rx: Receiver<catalog::Event>,
    cancel: Arc<AtomicBool>,
    mode: catalog::Mode,
    baseline: bool,
    albums: usize,
    added: usize,
}

struct BeatApp {
    demo: Option<demo::Snapshot>,
    cfg: Config,
    cache: Cache,
    client: Option<Arc<api::Client>>,
    player: Option<player::Player>,
    /// Name of the output device the current player was opened on, and when
    /// it was last checked; a different default device (Bluetooth connected
    /// or switched off) means the output must be rebuilt.
    output_device: Option<String>,
    output_checked_at: std::time::Instant,
    server_status: Option<Result<(), String>>,
    view: View,
    album_list_title: String,
    artists: Vec<api::Artist>,
    album_list: Arc<Vec<api::Album>>,
    artist_open: Option<(api::Artist, Arc<Vec<api::Album>>)>,
    album_open: Option<(api::Album, Arc<Vec<api::Song>>)>,
    /// Everything on disk: downloaded cache entries plus hand-dropped local
    /// files found in the cache folder.
    disk_entries: Arc<Vec<DiskEntry>>,
    /// Full server song list for the unified library view (from the last scan
    /// or the on-disk cache), and the buffer of the scan in progress.
    server_songs: Arc<Vec<api::Song>>,
    library_pending: Vec<api::Song>,
    library_save_pending: bool,
    /// Merged, de-duplicated rows of the library view (server + disk).
    library_rows: Arc<Vec<LibRow>>,
    /// Filter text and its cached result, so the library list is not
    /// re-filtered (with lowercase allocations) on every frame.
    library_filter: String,
    library_filter_applied: String,
    library_filtered: Arc<Vec<LibRow>>,
    /// Hand-dropped files grouped into albums/artists, so the library sections
    /// work with local music even without a server.
    local_albums: Arc<Vec<LocalAlbum>>,
    local_album_cards: Arc<Vec<api::Album>>,
    local_artists: Arc<Vec<api::Artist>>,
    /// Server artists as fetched; `artists` is the combined view list.
    artists_server: Vec<api::Artist>,
    /// Local matches of the current search text, recomputed when the query or
    /// the folder scan changes.
    search_local: Arc<Vec<DiskEntry>>,
    search_local_key: (String, usize, usize),
    /// Local listen counters and the «часто прослушиваемые» list built from
    /// them (rebuilt lazily when the folder or the counters change).
    stats: stats::Stats,
    counted_current: Option<String>,
    /// When the session file was last written; bursts of track changes are
    /// throttled so holding «next» does not hammer the disk.
    session_saved_at: std::time::Instant,
    session_store: session::Store,
    frequent_entries: Arc<Vec<(DiskEntry, u64)>>,
    frequent_dirty: bool,
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
    disk_cover_generation: u64,
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
    resume_position: f64,
    restore_pending: bool,
    restore_autoplay: bool,
    queue_open: bool,
    prefetch_generation: u64,
    prefetch_target: Option<(usize, String)>,
    prefetch_download: Option<(usize, String)>,
    prefetch_token: Option<(u64, usize, api::Song)>,
    prepare_busy: bool,
    prepare_tx: std::sync::mpsc::SyncSender<PreparedEvent>,
    prepare_rx: Receiver<PreparedEvent>,
    media_controls: Option<media_keys::Controls>,
    /// Time the current source advanced while playing, excluding pauses and
    /// instantaneous position changes from seeking.
    played_secs: f64,
    listen_position: f64,
    listen_checked_at: std::time::Instant,
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
    fn new(cc: &eframe::CreationContext<'_>, demo: Option<demo::Snapshot>) -> Self {
        let cfg = demo.as_ref().map(|snapshot| snapshot.config()).unwrap_or_else(Config::load);
        i18n::set(cfg.language);
        theme::apply(&cc.egui_ctx, cfg.dark_mode);
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
            match media_keys::Controls::new(cc) {
                Ok(controls) => (Some(controls), None),
                Err(e) => (None, Some(format!("Windows media controls: {e}"))),
            }
        };
        let stats = if demo.is_some() { stats::Stats::preview() } else { stats::Stats::load_scoped(profile_key(&cfg)) };
        let mut app = Self {
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
            icon_tex: app_icon_texture(&cc.egui_ctx),
            cfg,
            cache,
            #[cfg(windows)]
            maximized: false,
        };
        if app.demo.is_some() {
            demo::populate(&mut app, &cc.egui_ctx);
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

    fn build_client(cfg: &Config) -> Option<Arc<api::Client>> {
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

    fn request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    fn save(&mut self) {
        if self.demo.is_some() {
            return;
        }
        // `warning` carries what the save itself had to give up (a password that
        // cannot be protected on this platform), which belongs next to the error.
        self.save_error = self.cfg.save().err().or_else(|| self.cfg.warning.clone());
    }

    /// Brings back the last track, queue, shuffle and repeat; playback itself
    /// never starts by itself.
    fn restore_session(&mut self) {
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
    fn save_session(&mut self) {
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
    fn save_session_debounced(&mut self) {
        if self.session_saved_at.elapsed() >= std::time::Duration::from_secs(2) {
            self.save_session();
        }
    }

    fn require_client(&mut self) -> Option<Arc<api::Client>> {
        match &self.client {
            Some(client) => Some(client.clone()),
            None => {
                self.notice = Some(crate::i18n::tr("сначала укажите сервер, логин и пароль в настройках").into());
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
        let roots = self.cfg.local_roots();
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(cache.root());
            // Entries whose file the user deleted by hand stop being "in cache".
            cache.prune_missing();
            let indexed = cache.list();
            // The excluded set comes from the list just built: asking the cache
            // for it again would clone and sort the whole index a second time.
            let excluded: HashSet<String> = indexed.iter().map(|entry| entry.path.clone()).collect();
            let local = local::scan_roots(cache.root(), &roots, &excluded, &probes);
            let _ = tx.send(LibEvent::Disk(id, indexed, local));
        });
    }

    /// Counts hand-dropped files without opening them (sidebar stats), so the
    /// numbers are there before the full scan of «кеш на диске».
    fn refresh_local_stats(&mut self) {
        if !self.cfg.library_dirs.is_empty() {
            self.refresh_disk();
            return;
        }
        // The full scan produces the same two numbers; walking the folder twice
        // at once would only cost another pass over it.
        if self.disk_scan_id.is_some() {
            return;
        }
        let id = self.request_id();
        self.local_stats_request = Some(id);
        let cache = self.cache.clone();
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let stats = local::count(cache.root(), &cache.indexed_rel_paths());
            let _ = tx.send(LibEvent::LocalStats(id, stats));
        });
    }

    fn open_cache_folder(&mut self) {
        let root = self.cache.root();
        if let Err(err) = std::fs::create_dir_all(root) {
            self.notice = Some(crate::i18n::trf!("не удалось создать {}: {err}", root.display(), err = err));
            return;
        }
        open_in_explorer(&root.to_string_lossy());
    }

    /// Replaces the on-disk list and refreshes everything derived from it:
    /// the merged library rows now, the frequent list on its next visit.
    fn set_disk_entries(&mut self, entries: Vec<DiskEntry>) {
        self.disk_entries = Arc::new(entries);
        self.frequent_dirty = true;
        self.rebuild_local_library();
        self.rebuild_library_rows();
    }

    /// Replaces the full server song list (fresh scan or the on-disk cache).
    fn set_server_songs(&mut self, songs: Vec<api::Song>) {
        self.server_songs = Arc::new(songs);
        self.rebuild_library_rows();
    }

    fn rebuild_library_rows(&mut self) {
        self.library_rows = Arc::new(build_library_rows(&self.disk_entries, &self.server_songs));
        self.rebuild_library_filter();
    }

    fn rebuild_library_filter(&mut self) {
        let needle = self.library_filter.trim().to_lowercase();
        let list: Vec<LibRow> = if needle.is_empty() {
            (*self.library_rows).clone()
        } else {
            self.library_rows
                .iter()
                .filter(|row| {
                    row.title().to_lowercase().contains(&needle)
                        || row.artist().to_lowercase().contains(&needle)
                        || row.album().to_lowercase().contains(&needle)
                })
                .cloned()
                .collect()
        };
        self.library_filtered = Arc::new(list);
        self.library_filter_applied = self.library_filter.clone();
    }

    fn rebuild_local_library(&mut self) {
        let albums = build_local_albums(&self.disk_entries);
        self.local_artists = Arc::new(build_local_artists(&albums));
        self.local_album_cards = Arc::new(albums.iter().map(LocalAlbum::to_api_album).collect());
        self.local_albums = Arc::new(albums);
        self.rebuild_artists_view();
    }

    /// Server artists first, then local ones: both open through `open_artist`.
    fn rebuild_artists_view(&mut self) {
        let mut list = self.artists_server.clone();
        list.extend(self.local_artists.iter().cloned());
        self.artists = list;
    }

    fn rebuild_frequent(&mut self) {
        self.frequent_entries = Arc::new(top_played(&self.disk_entries, &self.stats, MAX_FREQUENT));
        self.frequent_dirty = false;
    }

    fn refresh_albums(&mut self, kind: &str, title: &str) {
        if self.client.is_none() {
            // Local-only mode: show the albums assembled from the cache folder.
            let mut albums: Vec<api::Album> = self.local_albums.iter().map(LocalAlbum::to_api_album).collect();
            let local_title = if kind == "random" {
                shuffle_albums(&mut albums);
                crate::i18n::tr("СЛУЧАЙНЫЕ ЛОКАЛЬНЫЕ АЛЬБОМЫ")
            } else {
                crate::i18n::tr("ЛОКАЛЬНЫЕ АЛЬБОМЫ")
            };
            self.album_list = Arc::new(albums);
            self.album_list_title = local_title.to_owned();
            self.view = View::Albums;
            self.loading = None;
            return;
        }
        let Some(client) = self.client.clone() else { return };
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
        if self.client.is_none() {
            // Local-only mode: the combined list already holds local artists.
            self.view = View::Artists;
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.artists();
            let _ = tx.send(LibEvent::Artists(id, result));
        });
    }

    fn open_artist(&mut self, artist: api::Artist) {
        if artist.id.starts_with(LOCAL_ARTIST_PREFIX) {
            let albums: Vec<api::Album> = self
                .local_albums
                .iter()
                .filter(|local| local.artist == artist.name)
                .map(LocalAlbum::to_api_album)
                .collect();
            self.loading = None;
            self.view = View::Artist;
            self.artist_open = Some((artist, Arc::new(albums)));
            return;
        }
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Artist;
        self.artist_open = Some((artist.clone(), Arc::new(Vec::new())));
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
        if self.open_local_album(&album) {
            return;
        }
        self.open_album(album);
    }

    /// Opens an album assembled from hand-dropped files; `false` means the id
    /// belongs to the server and the caller must fetch it.
    fn open_local_album(&mut self, album: &api::Album) -> bool {
        if !album.id.starts_with(LOCAL_ALBUM_PREFIX) {
            return false;
        }
        match self.local_albums.iter().find(|local| local.id == album.id) {
            Some(local) => {
                self.view = View::Album;
                self.loading = None;
                self.album_open = Some((album.clone(), Arc::new(local.songs.clone())));
            }
            None => self.notice = Some(crate::i18n::tr("альбом не найден — обновите список на диске").into()),
        }
        true
    }

    fn open_album(&mut self, album: api::Album) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Album;
        self.album_open = Some((album.clone(), Arc::new(Vec::new())));
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.album(&album.id);
            let _ = tx.send(LibEvent::Album(id, result));
        });
    }

    fn run_search(&mut self) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.view = View::Search;
        let Some(client) = self.client.clone() else {
            // Local-only mode: matches are computed live from the disk list.
            self.search_result = None;
            return;
        };
        let id = self.request_id();
        self.loading = Some(id);
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
            if scan.mode == catalog::Mode::Library {
                self.library_pending.clear();
            }
        }
    }

    fn start_catalog_scan(&mut self, mode: catalog::Mode) {
        if self.catalog_scan.is_some() || (mode == catalog::Mode::Library && self.library_save_pending) {
            return;
        }
        let Some(client) = self.require_client() else { return };
        let (tx, rx) = sync_channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        catalog::spawn(client, self.cache.clone(), mode, tx, cancel.clone());
        self.catalog_scan = Some(CatalogScan { rx, cancel, mode, baseline: false, albums: 0, added: 0 });
        if mode == catalog::Mode::Library {
            self.library_pending = Vec::new();
            self.notice = Some(crate::i18n::tr("загружаю библиотеку Navidrome…").into());
        } else {
            self.notice = Some(crate::i18n::tr("проверяю библиотеку Navidrome…").into());
        }
    }

    fn maybe_start_automatic_scan(&mut self) {
        if self.cfg.auto_cache_new
            && self.client.is_some()
            && self.catalog_scan.is_none()
            && std::time::Instant::now() >= self.next_catalog_check
        {
            self.start_catalog_scan(catalog::Mode::Automatic);
        }
    }

    /// Fills the unified library from the server; uses the on-disk cache when
    /// it is already there and `force` is false (opening the view).
    fn refresh_server_library(&mut self, force: bool) {
        if self.client.is_none() || self.catalog_scan.is_some() {
            return;
        }
        if !force && !self.server_songs.is_empty() {
            return;
        }
        self.start_catalog_scan(catalog::Mode::Library);
    }

    /// A finished full server walk replaces the list and persists it, so the
    /// next launch opens instantly. A failed walk keeps the previous list.
    fn finish_library_scan(&mut self, result: Result<(), String>, albums: usize) {
        match result {
            Ok(()) => {
                let songs = std::mem::take(&mut self.library_pending);
                let count = songs.len();
                self.set_server_songs(songs);
                if self.cfg.auto_cache_new {
                    // The walk just read everything; postpone the auto pass.
                    self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                }
                self.notice = Some(crate::i18n::trf!("библиотека обновлена: {count} треков с сервера", count = count));
                if let Some(client) = self.client.clone() {
                    let songs = self.server_songs.clone();
                    let tx = self.lib_tx.clone();
                    self.library_save_pending = true;
                    std::thread::spawn(move || {
                        let result = catalog::save_library(&client, &songs);
                        let _ = tx.send(LibEvent::LibrarySaved(client.catalog_key(), result));
                    });
                }
            }
            Err(err) => {
                self.library_pending.clear();
                self.notice = Some(crate::i18n::trf!(
                    "не удалось обновить библиотеку после {albums} альбомов: {err}",
                    albums = albums,
                    err = err
                ));
            }
        }
    }

    fn pump_catalog(&mut self) {
        let library_scan = self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Library);
        let mut events = Vec::new();
        let mut proposed = 0;
        while events.len() < 512 && (library_scan || self.download_queue.len() + proposed < catalog::MAX_QUEUED) {
            let Some(scan) = &self.catalog_scan else { break };
            match scan.rx.try_recv() {
                Ok(event) => {
                    let finished = matches!(event, catalog::Event::Finished(_));
                    if !library_scan && matches!(event, catalog::Event::Song(_)) {
                        proposed += 1;
                    }
                    events.push(event);
                    if finished {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    events.push(catalog::Event::Finished(Err(crate::i18n::tr(
                        "проверка библиотеки неожиданно прервалась",
                    )
                    .into())));
                    break;
                }
            }
        }
        for event in events {
            match event {
                catalog::Event::Started { baseline } => {
                    if let Some(scan) = &mut self.catalog_scan {
                        scan.baseline = baseline;
                    }
                }
                catalog::Event::AlbumScanned => {
                    if let Some(scan) = &mut self.catalog_scan {
                        scan.albums += 1;
                    }
                }
                catalog::Event::Song(song) => {
                    if library_scan {
                        // Metadata only: the list is filled when the walk ends.
                        self.library_pending.push(song);
                    } else {
                        let automatic =
                            self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Automatic);
                        let added = self.offer_download(song, automatic);
                        if added {
                            if let Some(scan) = &mut self.catalog_scan {
                                scan.added += 1;
                            }
                        }
                    }
                }
                catalog::Event::Finished(result) => {
                    if let Some(scan) = self.catalog_scan.take() {
                        if scan.mode == catalog::Mode::Library {
                            self.finish_library_scan(result, scan.albums);
                            continue;
                        }
                        self.next_catalog_check = std::time::Instant::now()
                            + if result.is_err() {
                                catalog::RETRY_AFTER
                            } else if scan.mode == catalog::Mode::All && self.cfg.auto_cache_new {
                                std::time::Duration::ZERO
                            } else {
                                catalog::POLL_EVERY
                            };
                        self.notice = Some(match result {
                            Ok(()) if scan.baseline => crate::i18n::trf!(
                                "запомнено альбомов: {}; новые песни будут скачиваться автоматически",
                                scan.albums
                            ),
                            Ok(()) => {
                                crate::i18n::trf!(
                                    "проверено альбомов: {}; добавлено в загрузки: {}",
                                    scan.albums,
                                    scan.added
                                )
                            }
                            Err(err) => crate::i18n::trf!(
                                "проверка библиотеки остановлена после {} альбомов (добавлено: {}): {err}",
                                scan.albums,
                                scan.added,
                                err = err
                            ),
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
                LibEvent::LibrarySaved(profile, result) => {
                    self.library_save_pending = false;
                    if self.client.as_ref().is_some_and(|client| client.catalog_key() == profile) {
                        if let Err(error) = result {
                            self.notice = Some(error);
                        }
                    }
                }
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
                    if !self.accept(id) {
                        continue;
                    }
                    match result {
                        Ok(artists) => {
                            self.artists_server = artists;
                            self.rebuild_artists_view();
                            self.view = View::Artists;
                        }
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Artist(id, result) => {
                    if !self.accept(id) {
                        continue;
                    }
                    match result {
                        Ok((artist, albums)) => self.artist_open = Some((artist, Arc::new(albums))),
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Album(id, result) => {
                    if !self.accept(id) {
                        continue;
                    }
                    match result {
                        Ok((album, songs)) => {
                            let play = self.pending_play.as_deref() == Some(album.id.as_str());
                            let download = self.pending_download.as_deref() == Some(album.id.as_str());
                            if play {
                                self.pending_play = None;
                            }
                            if download {
                                self.pending_download = None;
                            }
                            self.album_open = Some((album, Arc::new(songs.clone())));
                            if play {
                                if let Some(first) = songs.first() {
                                    self.play_song(first.clone(), songs.clone(), 0);
                                }
                            } else if download {
                                self.enqueue_album(&songs);
                            }
                        }
                        Err(err) => {
                            // Whatever play/download this fetch was carrying died
                            // with it. An armed flag left behind would fire on
                            // the next successful load of the same album, with
                            // no click from the user.
                            self.pending_play = None;
                            self.pending_download = None;
                            self.notice = Some(err);
                        }
                    }
                }
                LibEvent::AlbumList(id, result) => {
                    if !self.accept(id) {
                        continue;
                    }
                    match result {
                        Ok(albums) => {
                            self.album_list = Arc::new(albums);
                            self.view = View::Albums;
                        }
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Search(id, result) => {
                    if !self.accept(id) {
                        continue;
                    }
                    match result {
                        Ok(search) => self.search_result = Some(search),
                        Err(err) => self.notice = Some(err),
                    }
                }
                LibEvent::Disk(id, indexed, local) => {
                    // The scan owns its own request slot: it never clears
                    // `loading` of a library fetch that may be in flight.
                    if self.disk_scan_id != Some(id) {
                        continue;
                    }
                    self.disk_scan_id = None;
                    self.disk_cover_generation = self.disk_cover_generation.wrapping_add(1);
                    self.local_stats_request = None;
                    self.local_stats =
                        (local.len(), local.iter().fold(0u64, |bytes, track| bytes.saturating_add(track.size)));
                    self.set_disk_entries(merge_disk_entries(indexed, local));
                    // Local album mode opened before the first scan finished:
                    // fill the grid now that the folder is known.
                    if self.client.is_none()
                        && self.view == View::Albums
                        && self.album_list.is_empty()
                        && !self.local_albums.is_empty()
                    {
                        self.album_list = self.local_album_cards.clone();
                        self.album_list_title = crate::i18n::tr("ЛОКАЛЬНЫЕ АЛЬБОМЫ").into();
                    }
                    if self.disk_stale {
                        self.refresh_disk();
                    }
                }
                LibEvent::LocalStats(id, stats) => {
                    if self.local_stats_request != Some(id) {
                        continue;
                    }
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
            if generation != self.cover_generation {
                continue;
            }
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
        if let Some(warning) = self.cache.take_warning() {
            self.notice = Some(warning);
        }
        if let Some(warning) = self.stats.take_warning() {
            self.notice = Some(warning);
        }
        while let Ok((id, event)) = self.dl_rx.try_recv() {
            match event {
                DlEvent::Done(entry) => {
                    self.downloads.remove(&id);
                    self.notice = Some(crate::i18n::trf!("скачано: {} — {}", entry.artist, entry.title));
                    // The new file is indexed now: the on-disk view must be
                    // rescanned so it appears (and stops looking like a local
                    // file); `pump_downloads` does that, not once per track.
                    self.disk_stale = true;
                }
                DlEvent::Failed(err) => {
                    self.downloads.remove(&id);
                    self.notice = Some(crate::i18n::trf!("не скачалось: {err}", err = err));
                }
            }
        }
        while self.downloads.len() < self.cfg.parallel_downloads {
            let Some(song) = self.download_queue.pop_front() else { break };
            self.auto_queued.remove(&song.id);
            if self.cache.contains(&song.id) || self.downloads.contains_key(&song.id) {
                continue;
            }
            self.spawn_download(song);
        }
        let downloading = !self.downloads.is_empty() || !self.download_queue.is_empty();
        if self.view == View::Library
            && disk_refresh_due(
                self.disk_stale,
                self.disk_scan_id.is_some(),
                downloading,
                self.disk_scanned_at.elapsed(),
            )
        {
            self.refresh_disk();
        }
    }

    fn spawn_download(&mut self, song: api::Song) {
        let Some(client) = self.client.clone() else { return };
        let label = format_label(self.cfg.stream_format);
        let handle =
            cache::start_download((*client).clone(), self.cache.clone(), song.clone(), label, self.dl_tx.clone());
        self.downloads.insert(song.id, handle);
    }

    fn enqueue_download(&mut self, song: api::Song) {
        if local::is_local_id(&song.id) {
            self.notice = Some(crate::i18n::trf!("файл уже лежит в папке кеша: {}", song.title));
            return;
        }
        if self.cache.contains(&song.id) {
            self.notice = Some(crate::i18n::trf!("уже в кеше: {}", song.title));
            return;
        }
        if self.offer_download(song, false) {
            self.notice = Some(crate::i18n::tr("добавлено в загрузки").into());
        }
    }

    fn offer_download(&mut self, song: api::Song, automatic: bool) -> bool {
        if !automatic {
            self.auto_queued.remove(&song.id);
        }
        if song.id.is_empty()
            || local::is_local_id(&song.id)
            || self.cache.contains(&song.id)
            || self.downloads.contains_key(&song.id)
            || self.download_queue.iter().any(|queued| queued.id == song.id)
        {
            return false;
        }
        if automatic {
            self.auto_queued.insert(song.id.clone());
        }
        self.download_queue.push_back(song);
        true
    }

    fn enqueue_album(&mut self, songs: &[api::Song]) {
        let mut added = 0;
        for song in songs {
            if self.offer_download(song.clone(), false) {
                added += 1;
            }
        }
        self.notice = Some(if added > 0 {
            crate::i18n::trf!("в загрузки добавлено треков: {added}", added = added)
        } else {
            crate::i18n::tr("все треки уже скачаны").into()
        });
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    /// Starts a fresh queue at `index` (a new shuffle order is rolled when
    /// shuffle is on).
    fn play_song(&mut self, song: api::Song, queue: Vec<api::Song>, index: usize) {
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
    fn play_from_library(&mut self, song: api::Song) {
        if let Some(start) = library_queue_start(&self.library_rows, &song.id) {
            let queue: Vec<api::Song> = self.library_rows.iter().map(LibRow::to_song).collect();
            self.play_song(song, queue, start);
        } else {
            self.play_song(song.clone(), vec![song], 0);
        }
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
    fn start_song(&mut self, song: api::Song) {
        self.start_song_at(song, 0.0);
    }

    fn start_song_at(&mut self, song: api::Song, position: f64) {
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

    fn start_playback_download(&mut self, song: api::Song) -> Option<DlHandle> {
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

    fn update_playback(&mut self, ctx: &egui::Context) {
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

    fn local_file(&self, id: &str) -> Option<std::path::PathBuf> {
        local::resolve(id, self.cache.root(), &self.cfg.local_roots())
    }

    fn song_file(&self, song: &api::Song) -> Option<std::path::PathBuf> {
        if local::is_local_id(&song.id) {
            self.local_file(&song.id)
        } else {
            self.cache.entry(&song.id).and_then(|entry| self.cache.absolute(&entry))
        }
    }

    fn playback_position(&self) -> f64 {
        if self.restore_pending || matches!(self.play_state, PlayState::Idle) {
            self.resume_position
        } else {
            self.player.as_ref().map(|player| player.position()).unwrap_or(self.resume_position)
        }
    }

    fn apply_resume(&mut self) {
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

    fn planned_next(&self) -> Option<usize> {
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

    fn accept_prefetched(&mut self) -> bool {
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

    fn invalidate_prefetch(&mut self) {
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

    fn pump_prefetch(&mut self) {
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

    fn pump_media_keys(&mut self) {
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

    fn sync_media_controls(&mut self) {
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

    /// Counts one listen once playback of the current track actually started;
    /// `start_song` resets the marker, so repeat-one counts every replay.
    fn count_current_play(&mut self) {
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

    fn update_playback_state(&mut self, ctx: &egui::Context) {
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

    fn next_track(&mut self) {
        self.invalidate_prefetch();
        let Some(index) = self.step_index(true) else {
            self.stop_playback();
            return;
        };
        self.play_index = index;
        let song = self.play_queue[index].clone();
        self.start_song(song);
    }

    fn prev_track(&mut self) {
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

    fn stop_playback(&mut self) {
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
    fn toggle_play(&mut self) {
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

    /// Watches the output: an OS stream error (Bluetooth switched off or out
    /// of range) or a different default device means the old sink is dead —
    /// it plays nothing and can hang a seek forever — so the output is
    /// rebuilt with the same track and position.
    fn check_output_health(&mut self) {
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
    fn rebuild_output(&mut self, reason: &str) {
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
    fn reload_after_output_change(&mut self, song: api::Song, position: f64, resume: bool) {
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

    fn cover_texture(&mut self, cover_id: &str, px: u32) -> Option<egui::TextureHandle> {
        if cover_id.is_empty() {
            return None;
        }
        // The same artwork is drawn at two sizes (24 px list rows, 168 px album
        // cards), so the in-memory key carries the requested size: one texture
        // cannot serve both, and sharing the small one would upscale it.
        let key = if cover_id.starts_with(FILE_COVER_PREFIX) {
            format!("{cover_id}#{px}#{}", self.disk_cover_generation)
        } else {
            format!("{cover_id}#{px}")
        };
        if let Some(texture) = self.covers.get(&key) {
            return Some(texture.clone());
        }
        if self.demo.is_some() {
            return None;
        }
        if self.cover_pending.contains(&key) {
            return None;
        }
        // At the limit the cover is simply asked for again on a later frame.
        if self.cover_inflight >= MAX_COVER_JOBS {
            return None;
        }
        // Files on disk (local drops and finished downloads): the artwork is
        // extracted from the file itself, so this works offline. Extracted
        // thumbnails are cached on disk, so unchanged files are not re-read
        // on every launch.
        if let Some(rel) = cover_id.strip_prefix(FILE_COVER_PREFIX) {
            self.cover_pending.insert(key.clone());
            self.cover_inflight += 1;
            let cache = self.cache.clone();
            let roots = self.cfg.local_roots();
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            let rel = rel.to_owned();
            std::thread::spawn(move || {
                let image = local::resolve(&format!("{}{}", local::LOCAL_ID_PREFIX, rel), cache.root(), &roots)
                    .and_then(|path| {
                        let cache_key = covers::file_key(&rel, &path, px);
                        if let Some(cache_key) = &cache_key {
                            if let Some(image) = covers::load(cache_key) {
                                return Some(image);
                            }
                        }
                        let image = local::embedded_cover(&path).and_then(|bytes| decode_cover_sized(&bytes, px));
                        if let (Some(cache_key), Some(image)) = (&cache_key, &image) {
                            covers::store(cache_key, image);
                        }
                        image
                    });
                let _ = tx.send(CoverEvent::Loaded(generation, key, image));
            });
            return None;
        }
        if let Some(client) = self.client.clone() {
            self.cover_pending.insert(key.clone());
            self.cover_inflight += 1;
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            let cover_id = cover_id.to_owned();
            std::thread::spawn(move || {
                let cache_key = covers::server_key(&client.catalog_key(), &cover_id, px);
                let image = covers::load(&cache_key).or_else(|| {
                    let image = client.cover_bytes(&cover_id, px).ok().and_then(|bytes| decode_cover_sized(&bytes, px));
                    if let Some(image) = &image {
                        covers::store(&cache_key, image);
                    }
                    image
                });
                let _ = tx.send(CoverEvent::Loaded(generation, key, image));
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
        if let Some(snapshot) = &mut self.demo {
            if snapshot.tick(ctx) {
                return;
            }
        } else {
            self.check_output_health();
            self.pump_media_keys();
            self.pump_library();
            self.pump_catalog();
            self.pump_covers(ctx);
            self.pump_downloads();
            self.maybe_start_automatic_scan();
            self.update_playback(ctx);
        }

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
        self.ui_queue(ctx);
        self.sync_media_controls();

        #[cfg(windows)]
        resize_edges(ctx);

        self.schedule_repaint(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.demo.is_some() {
            return;
        }
        // The play counters are debounced; the last listens must land on disk.
        self.invalidate_prefetch();
        self.stats.flush();
        self.save_session();
    }
}

impl BeatApp {
    fn ui_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            match &self.icon_tex {
                Some(tex) => {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(30.0, 30.0)));
                }
                None => {
                    ui.label(
                        egui::RichText::new("┌─┐\n│B│\n└─┘").font(egui::FontId::monospace(10.0)).color(theme::accent()),
                    );
                }
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.set_min_height(58.0);
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(crate::i18n::tr("BEAT // NAVIDROME КЛИЕНТ")).size(13.0).color(theme::text()),
                );
                ui.label(
                    egui::RichText::new(crate::i18n::tr("музыка, кеш и плеер в одном окне"))
                        .size(11.0)
                        .color(theme::dim()),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(crate::i18n::tr("[ НАСТРОЙКИ ]")).min_size(egui::vec2(0.0, 30.0))).clicked()
                {
                    self.open_settings();
                }
                let (cached, bytes) = self.cache.stats();
                ui.label(
                    egui::RichText::new(crate::i18n::trf!("КЕШ {cached} · {}", human_size(bytes), cached = cached))
                        .size(11.0)
                        .color(theme::dim()),
                );
                let (label, color) = match &self.server_status {
                    None if self.client.is_none() => (crate::i18n::tr("ЛОКАЛЬНО"), theme::faint()),
                    None => (crate::i18n::tr("СЕРВЕР…"), theme::faint()),
                    Some(Ok(())) => (crate::i18n::tr("СЕРВЕР ГОТОВ"), theme::accent()),
                    Some(Err(_)) => (crate::i18n::tr("НЕТ СВЯЗИ"), theme::err()),
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
                lines
                    .iter()
                    .map(|line| {
                        ui.fonts(|f| {
                            let job = egui::text::LayoutJob::single_section(
                                (*line).to_owned(),
                                egui::TextFormat { font_id: egui::FontId::monospace(size), ..Default::default() },
                            );
                            f.layout_job(job).size().x
                        })
                    })
                    .fold(0.0_f32, f32::max)
            };
            let mut size = self.banner_size;
            let avail = ui.available_width();
            if size <= 0.0 || (self.banner_fit - avail).abs() > 0.5 {
                size = 9.0_f32;
                while size > 4.0 && width_at(ui, size) > avail {
                    size -= 0.25;
                }
                self.banner_size = size;
                self.banner_fit = avail;
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.vertical_centered(|ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(wordmark).font(egui::FontId::monospace(size)).color(theme::text()),
                    )
                    .halign(egui::Align::LEFT)
                    .wrap_mode(egui::TextWrapMode::Extend),
                );
                ui.label(
                    egui::RichText::new(banner::TAGLINE).font(egui::FontId::monospace(size)).color(theme::accent()),
                );
            });
            ui.add_space(6.0);

            ui.spacing_mut().item_spacing.y = 4.0;
            theme::section_label(ui, crate::i18n::tr("БИБЛИОТЕКА"));
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("поиск"))).clicked() {
                self.view = View::Search;
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("новые альбомы"))).clicked()
            {
                self.refresh_albums("newest", crate::i18n::tr("НОВЫЕ АЛЬБОМЫ"));
            }
            if ui
                .add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("случайные альбомы")))
                .clicked()
            {
                self.refresh_albums("random", crate::i18n::tr("СЛУЧАЙНЫЕ АЛЬБОМЫ"));
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("все артисты"))).clicked()
            {
                self.fetch_artists();
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("библиотека"))).clicked()
            {
                self.view = View::Library;
                self.refresh_disk();
                self.refresh_server_library(false);
            }
            if ui
                .add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("часто прослушиваемые")))
                .clicked()
            {
                self.view = View::Frequent;
                self.stats.flush();
                self.refresh_disk();
            }

            theme::section_label(ui, crate::i18n::tr("ЗАГРУЗКИ"));
            let scan_enabled = self.client.is_some() && self.catalog_scan.is_none();
            let width = ui.available_width();
            // `add_sized` centres the whole label, the «↓» included.
            let scan_clicked = ui
                .add_enabled_ui(scan_enabled, |ui| {
                    ui.add_sized([width, 26.0], egui::Button::new(crate::i18n::tr("↓ скачать все песни")))
                })
                .inner
                .on_hover_text(crate::i18n::tr("Найти все треки Navidrome и поставить отсутствующие в очередь"))
                .clicked();
            if scan_clicked {
                self.start_catalog_scan(catalog::Mode::All);
            }
            if let Some(scan) = &self.catalog_scan {
                let label = if scan.baseline {
                    crate::i18n::tr("запоминаю библиотеку")
                } else {
                    crate::i18n::tr("проверяю библиотеку")
                };
                ui.label(
                    egui::RichText::new(crate::i18n::trf!("{label}: {} альбомов", scan.albums, label = label))
                        .size(10.0)
                        .color(theme::warn()),
                );
                if scan.added > 0 {
                    ui.label(
                        egui::RichText::new(crate::i18n::trf!("новых загрузок: {}", scan.added))
                            .size(10.0)
                            .color(theme::dim()),
                    );
                }
                if ui
                    .button(crate::i18n::tr("× остановить поиск"))
                    .on_hover_text(crate::i18n::tr("Уже запущенные и добавленные в очередь загрузки продолжатся"))
                    .clicked()
                {
                    self.cancel_catalog_scan();
                    self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    self.notice =
                        Some(crate::i18n::tr("поиск остановлен; уже добавленные загрузки продолжаются").into());
                }
            }
            let active: Vec<(String, DlHandle)> =
                self.downloads.iter().map(|(id, handle)| (id.clone(), handle.clone())).collect();
            if active.is_empty() && self.download_queue.is_empty() {
                ui.label(
                    egui::RichText::new(crate::i18n::tr("нет активных загрузок")).size(11.0).color(theme::faint()),
                );
            }
            for (_, handle) in &active {
                let ratio = handle.progress.ratio();
                ui.label(
                    egui::RichText::new(format!("↓ {}", clip(&handle.song.title, 24))).size(11.0).color(theme::dim()),
                );
                let bar = match ratio {
                    Some(ratio) => format!("{:.0}%", ratio * 100.0),
                    None => human_size(handle.progress.snapshot().0),
                };
                ui.label(egui::RichText::new(bar).size(10.0).color(theme::accent()));
            }
            if !self.download_queue.is_empty() {
                theme::kv_row(
                    ui,
                    crate::i18n::tr("в очереди"),
                    &format!("{}", self.download_queue.len()),
                    theme::dim(),
                );
                if ui
                    .button(crate::i18n::tr("× очистить очередь"))
                    .on_hover_text(crate::i18n::tr("Отменить ожидающие загрузки; уже начатые продолжатся"))
                    .clicked()
                {
                    // Otherwise the still-running catalog worker immediately
                    // fills the queue again on the next frame.
                    if self.catalog_scan.is_some() {
                        self.cancel_catalog_scan();
                        self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    }
                    self.download_queue.clear();
                    self.auto_queued.clear();
                    self.notice =
                        Some(crate::i18n::tr("поиск и ожидающие загрузки отменены; начатые продолжаются").into());
                }
            }

            theme::section_label(ui, crate::i18n::tr("КЕШ"));
            let (cached, cached_bytes) = self.cache.stats();
            let (local, local_bytes) = self.local_stats;
            theme::kv_row(ui, crate::i18n::tr("треков"), &format!("{}", cached + local), theme::text());
            theme::kv_row(ui, crate::i18n::tr("в кеше"), &format!("{cached}"), theme::dim());
            theme::kv_row(ui, crate::i18n::tr("локальных"), &format!("{local}"), theme::dim());
            theme::kv_row(
                ui,
                crate::i18n::tr("размер"),
                &human_size(cached_bytes.saturating_add(local_bytes)),
                theme::text(),
            );
            ui.add_space(4.0);
            if ui
                .add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("открыть папку кеша")))
                .clicked()
            {
                self.open_cache_folder();
            }
            let label = if self.cache_clear_armed {
                crate::i18n::tr("× точно очистить кеш?")
            } else {
                crate::i18n::tr("× очистить кеш")
            };
            let clear_enabled = self.downloads.is_empty() && self.download_queue.is_empty();
            let width = ui.available_width();
            // `add_sized` (like the button above) centres the whole label,
            // «×» included; `min_size` would leave it hanging on the left.
            let clear_clicked = ui
                .add_enabled_ui(clear_enabled, |ui| ui.add_sized([width, 26.0], egui::Button::new(label)))
                .inner
                .clicked();
            if clear_clicked {
                if self.cache_clear_armed {
                    if self.current.as_ref().is_some_and(|song| !local::is_local_id(&song.id)) {
                        self.stop_playback();
                    }
                    match self.cache.clear() {
                        Ok(()) => self.notice = Some(crate::i18n::tr("кеш очищен").into()),
                        Err(err) => self.notice = Some(err),
                    }
                    self.cache_clear_armed = false;
                    let kept = self
                        .disk_entries
                        .iter()
                        .filter(|entry| entry.is_local() || self.cache.is_indexed(entry.id()))
                        .cloned()
                        .collect();
                    self.set_disk_entries(kept);
                    self.refresh_disk();
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

    /// Footer: now playing and seek on the left, transport in the centre of
    /// the whole bar, volume on the right.
    fn ui_player_bar(&mut self, ui: &mut egui::Ui) {
        const CONTROL_W: f32 = 40.0;
        const CONTROL_COUNT: usize = 6;
        let bar = ui.available_rect_before_wrap();
        let spacing = ui.spacing().item_spacing.x;
        let controls_w = CONTROL_W * CONTROL_COUNT as f32 + spacing * (CONTROL_COUNT as f32 - 1.0);
        // Centred on the whole bar, not on the space left by the side
        // sections, so it never drifts when the window resizes.
        let center = egui::Rect::from_center_size(bar.center(), egui::vec2(controls_w, bar.height()));
        let left_rect = egui::Rect::from_min_max(bar.min, egui::pos2((center.left() - 16.0).max(bar.min.x), bar.max.y));
        let right_rect =
            egui::Rect::from_min_max(egui::pos2((center.right() + 16.0).min(bar.max.x), bar.min.y), bar.max);

        ui.allocate_new_ui(
            egui::UiBuilder::new().max_rect(left_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| self.ui_now_playing(ui),
        );
        ui.allocate_new_ui(
            egui::UiBuilder::new().max_rect(center).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| self.ui_transport(ui, CONTROL_W),
        );
        ui.allocate_new_ui(
            egui::UiBuilder::new().max_rect(right_rect).layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| self.ui_volume(ui),
        );
    }

    fn ui_now_playing(&mut self, ui: &mut egui::Ui) {
        let has_player = self.player.is_some() || self.demo.is_some();
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
            Some(rel) => self.cover_texture_of(&format!("{FILE_COVER_PREFIX}{rel}"), ROW_COVER_PX),
            None => None,
        };
        let _ = row_cover(ui, 38.0, cover, egui::Sense::hover());
        ui.add_space(2.0);

        let width = (ui.available_width() - 4.0).max(120.0);
        ui.vertical(|ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 3.0;
            let (title, artist, album) = match &self.current {
                Some(song) => (song.title.clone(), song.artist.clone(), song.album.clone()),
                None => ("—".into(), crate::i18n::tr("ничего не играет").into(), String::new()),
            };
            let name_w = (ui.available_width() - 8.0).max(80.0);
            let name = ui
                .allocate_ui_with_layout(
                    egui::vec2(name_w, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.add(egui::Label::new(texts_job(&title, &artist, &album, false)).truncate()),
                )
                .inner;
            let hover = texts_hover(&title, &artist, &album);
            if !hover.trim().is_empty() {
                name.on_hover_text(hover);
            }

            let duration = self.current.as_ref().map(|song| song.duration).unwrap_or(0.0);
            let position = self.playback_position().min(duration);
            // While the slider is being dragged it shows the picked position.
            let shown = self.pending_seek.unwrap_or(position);
            let mut seek = shown;
            let time = format!("{} / {}", format_time(shown), format_time(duration));
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 92.0).max(80.0);
                let slider = ui.add_enabled(
                    has_player && self.current.is_some(),
                    egui::Slider::new(&mut seek, 0.0..=duration.max(1.0)).show_value(false),
                );
                if slider.changed() {
                    self.pending_seek = Some(seek);
                }
                if let Some(target) = due_seek(self.pending_seek, ui.input(|i| i.pointer.any_down())) {
                    self.pending_seek = None;
                    self.seek_to(target, duration);
                }
                ui.label(egui::RichText::new(time).size(10.0).color(theme::faint()));
            });
        });
    }

    fn ui_transport(&mut self, ui: &mut egui::Ui, button_w: f32) {
        let has_player = self.player.is_some() || self.demo.is_some();
        let paused = self.player.as_ref().map(|p| p.is_paused()).unwrap_or(false);
        // «Loaded» means the track is actually in the output: a restored
        // session has a current song but nothing loaded yet, and the button
        // must read «play», not «pause».
        let loaded = !matches!(self.play_state, PlayState::Idle);
        let size = egui::vec2(button_w, 30.0);
        if ui
            .add_enabled(has_player, egui::Button::new("◀◀").min_size(size))
            .on_hover_text(crate::i18n::tr("предыдущий"))
            .clicked()
        {
            self.prev_track();
        }
        // Fixed width: the pause glyph is wider than play, and the row
        // must not jump when toggling. ▮ (U+25AE) exists in Cascadia;
        // the old ❚ (U+275A) was not and fell back to replacement boxes.
        let (play_label, play_hover) = transport_play_label(self.current.is_some(), loaded, paused);
        if ui.add_enabled(has_player, egui::Button::new(play_label).min_size(size)).on_hover_text(play_hover).clicked()
        {
            self.toggle_play();
        }
        if ui
            .add_enabled(has_player, egui::Button::new("▶▶").min_size(size))
            .on_hover_text(crate::i18n::tr("следующий"))
            .clicked()
        {
            self.next_track();
        }
        if ui
            .add_enabled(has_player, egui::Button::new("■").min_size(size))
            .on_hover_text(crate::i18n::tr("стоп"))
            .clicked()
        {
            self.stop_playback();
        }
        if mode_button(ui, "⇄", self.shuffle, crate::i18n::tr("случайный порядок")) {
            self.shuffle = !self.shuffle;
            self.invalidate_prefetch();
            self.refresh_shuffle();
            self.save_session();
        }
        let (repeat_label, repeat_hover) = match self.repeat {
            Repeat::Off => ("↻", crate::i18n::tr("повтор выключен — нажмите: весь список")),
            Repeat::All => ("↻", crate::i18n::tr("повтор всего списка — нажмите: одна песня")),
            Repeat::One => ("↻1", crate::i18n::tr("повтор одной песни — нажмите: выключить")),
        };
        if mode_button(ui, repeat_label, self.repeat != Repeat::Off, repeat_hover) {
            self.invalidate_prefetch();
            self.repeat = match self.repeat {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            };
            self.save_session();
        }
    }

    fn ui_volume(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = 110.0;
        let mut volume = self.player.as_ref().map(|p| p.volume()).unwrap_or(self.cfg.volume);
        let volume_response =
            ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false).text(crate::i18n::tr("громкость")));
        if volume_response.changed() {
            if let Some(player) = &mut self.player {
                player.set_volume(volume);
            }
            self.cfg.volume = volume;
        }
        if volume_response.drag_stopped() {
            self.save();
        }
    }

    fn ui_statusbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let state = match &self.play_state {
                PlayState::Waiting(_) => (crate::i18n::tr("● ОЖИДАНИЕ ЗАГРУЗКИ"), theme::warn()),
                PlayState::Buffering { .. } => (crate::i18n::tr("● БУФЕРИЗАЦИЯ"), theme::warn()),
                PlayState::Playing => (crate::i18n::tr("● ИГРАЕТ"), theme::accent()),
                PlayState::Idle => (crate::i18n::tr("● ГОТОВО"), theme::accent()),
            };
            ui.label(egui::RichText::new(state.0).size(10.0).color(state.1));
            status_sep(ui);
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(crate::i18n::trf!("треков в очереди: {}", self.play_queue.len()))
                            .size(10.0)
                            .color(theme::dim()),
                    )
                    .frame(false),
                )
                .clicked()
            {
                self.queue_open = true;
            }
            status_sep(ui);
            ui.label(
                egui::RichText::new(crate::i18n::trf!("кеш: {}", self.cache.root().to_string_lossy()))
                    .size(10.0)
                    .color(theme::faint()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.hyperlink_to(
                    egui::RichText::new(TELEGRAM_URL.trim_start_matches("https://")).size(10.0),
                    TELEGRAM_URL,
                );
                ui.label(
                    egui::RichText::new(format!("beat v{APP_VERSION} — by rercon prod."))
                        .size(10.0)
                        .color(theme::faint()),
                );
                if let Some(err) = &self.save_error {
                    status_sep(ui);
                    ui.label(
                        egui::RichText::new(crate::i18n::trf!("конфиг не сохранён: {err}", err = err))
                            .size(10.0)
                            .color(theme::err()),
                    );
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
            View::Library => self.ui_library(ui),
            View::Frequent => self.ui_frequent(ui),
        }
    }

    fn ui_artists(&mut self, ui: &mut egui::Ui) {
        if self.loading.is_some() {
            ui.label(egui::RichText::new(crate::i18n::tr("загрузка…")).size(12.0).color(theme::faint()));
        }
        theme::section_label(ui, crate::i18n::tr("АРТИСТЫ"));
        if self.artists.is_empty() && self.loading.is_none() {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new(crate::i18n::tr(
                    "артистов пока нет — добавьте файлы в папку кеша или настройте сервер",
                ))
                .size(11.0)
                .color(theme::faint()),
            );
            return;
        }
        let mut open = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
            ui,
            28.0,
            self.artists.len(),
            |ui, range| {
                for artist in &self.artists[range] {
                    if profile_row(ui, &artist.name, artist.album_count as usize, false) {
                        open = Some(artist.clone());
                    }
                }
            },
        );
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
            if ui.button(crate::i18n::tr("‹ назад")).clicked() {
                self.view = View::Artists;
            }
            ui.label(theme::window_title(&format!("[ {} ]", artist.name)));
            ui.label(
                egui::RichText::new(crate::i18n::trf!("альбомов: {}", albums.len())).size(11.0).color(theme::faint()),
            );
        });
        ui.add_space(4.0);
        self.ui_album_grid(ui, "", Some(albums));
    }

    fn ui_album_grid(&mut self, ui: &mut egui::Ui, title: &str, albums: Option<Arc<Vec<api::Album>>>) {
        // In server mode hand-dropped albums get their own section below the
        // server grid, so the library buttons cover local music too.
        let show_local = albums.is_none() && self.client.is_some();
        let primary = albums.unwrap_or_else(|| self.album_list.clone());
        let local_extra = if show_local { self.local_album_cards.clone() } else { Arc::new(Vec::new()) };
        if self.loading.is_some() && primary.is_empty() && local_extra.is_empty() {
            ui.label(egui::RichText::new(crate::i18n::tr("загрузка…")).size(12.0).color(theme::faint()));
            return;
        }
        if primary.is_empty() && local_extra.is_empty() && self.client.is_none() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(crate::i18n::tr("СЕРВЕР НЕ НАСТРОЕН")).size(14.0).color(theme::warn()));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(crate::i18n::tr(
                        "укажите адрес Navidrome, логин и пароль — или закиньте файлы прямо в папку кеша",
                    ))
                    .size(12.0)
                    .color(theme::dim()),
                );
                ui.add_space(10.0);
                if ui.add(theme::accent_button(crate::i18n::tr("[ НАСТРОЙКИ ]"))).clicked() {
                    self.open_settings();
                }
                if ui.button(crate::i18n::tr("[ БИБЛИОТЕКА ]")).clicked() {
                    self.view = View::Library;
                    self.refresh_disk();
                }
                if ui.button(crate::i18n::tr("[ ОТКРЫТЬ ПАПКУ КЕША ]")).clicked() {
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
            if !primary.is_empty() {
                visible_album_grid(ui, "primary-albums", &primary, |ui, album| {
                    let card = album_card(
                        ui,
                        album,
                        self.cover_texture_of(&album.cover_id, COVER_PX),
                        !album.id.starts_with(LOCAL_ALBUM_PREFIX),
                    );
                    match card {
                        AlbumCardAction::Open => open = Some(album.clone()),
                        AlbumCardAction::Play => play = Some(album.clone()),
                        AlbumCardAction::Download => download = Some(album.clone()),
                        AlbumCardAction::None => {}
                    }
                });
            }
            if !local_extra.is_empty() {
                theme::section_label(ui, crate::i18n::tr("ЛОКАЛЬНЫЕ АЛЬБОМЫ"));
                visible_album_grid(ui, "local-albums", &local_extra, |ui, album| {
                    let card = album_card(ui, album, self.cover_texture_of(&album.cover_id, COVER_PX), false);
                    match card {
                        AlbumCardAction::Open => open = Some(album.clone()),
                        AlbumCardAction::Play => play = Some(album.clone()),
                        AlbumCardAction::Download => download = Some(album.clone()),
                        AlbumCardAction::None => {}
                    }
                });
            }
        });
        if let Some(album) = open {
            self.browse_album(album);
        }
        if let Some(album) = play {
            self.play_album(album);
        }
        if let Some(album) = download {
            self.download_album(album);
        }
    }

    /// Cover lookup that does not borrow the whole app inside UI closures.
    fn cover_texture_of(&mut self, cover_id: &str, px: u32) -> Option<egui::TextureHandle> {
        self.cover_texture(cover_id, px)
    }

    fn ui_album(&mut self, ui: &mut egui::Ui) {
        let (album, songs) = match &self.album_open {
            Some((album, songs)) => (album.clone(), songs.clone()),
            None => return,
        };
        ui.horizontal(|ui| {
            if ui.button(crate::i18n::tr("‹ назад")).clicked() {
                self.view = View::Albums;
            }
            ui.label(theme::window_title(&format!(
                "[ {} — {} ]",
                album.name,
                if album.artist.is_empty() { "?" } else { &album.artist }
            )));
        });
        ui.horizontal(|ui| {
            if let Some(year) = (album.year > 0).then_some(album.year) {
                ui.label(egui::RichText::new(format!("{year}")).size(11.0).color(theme::faint()));
            }
            ui.label(
                egui::RichText::new(crate::i18n::trf!(
                    "треков: {}, {}",
                    songs.len(),
                    format_time(album.duration as f64)
                ))
                .size(11.0)
                .color(theme::faint()),
            );
            if ui.add(theme::accent_button(crate::i18n::tr("[ СЛУШАТЬ ]"))).clicked() {
                if let Some(song) = songs.first() {
                    self.play_song(song.clone(), songs.as_ref().clone(), 0);
                }
            }
            if !album.id.starts_with(LOCAL_ALBUM_PREFIX) && ui.button(crate::i18n::tr("[ СКАЧАТЬ АЛЬБОМ ]")).clicked()
            {
                self.enqueue_album(&songs);
            }
        });
        ui.add_space(6.0);
        let row_height = 30.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_height, songs.len(), |ui, range| {
            for index in range {
                let song = &songs[index];
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [26.0, 24.0],
                        egui::Label::new(
                            egui::RichText::new(if song.track > 0 {
                                format!("{:02}", song.track)
                            } else {
                                "·".into()
                            })
                            .size(11.0)
                            .color(theme::faint()),
                        ),
                    );
                    let playing = self.current.as_ref().is_some_and(|c| c.id == song.id);
                    let title_color = if playing { theme::accent() } else { theme::text() };
                    ui.add_sized(
                        [(ui.available_width() - 240.0).max(80.0), 24.0],
                        egui::Label::new(egui::RichText::new(clip(&song.title, 70)).size(12.0).color(title_color)),
                    );
                    ui.label(egui::RichText::new(format_time(song.duration)).size(11.0).color(theme::faint()));
                    let cached = self.cache.is_indexed(&song.id);
                    let local_song = local::is_local_id(&song.id);
                    if cached {
                        ui.label(egui::RichText::new(crate::i18n::tr("в кеше")).size(10.0).color(theme::accent()));
                    } else if self.downloads.contains_key(&song.id) {
                        ui.label(egui::RichText::new(crate::i18n::tr("качается")).size(10.0).color(theme::warn()));
                    } else if local_song {
                        ui.label(egui::RichText::new(crate::i18n::tr("на диске")).size(10.0).color(theme::faint()));
                    } else {
                        ui.add_space(38.0);
                    }
                    if ui.add_enabled(self.player.is_some(), egui::Button::new("▶")).clicked() {
                        self.play_song(song.clone(), songs.as_ref().clone(), index);
                    }
                    if !local_song
                        && ui
                            .add_enabled(!cached, egui::Button::new("↓"))
                            .on_hover_text(crate::i18n::tr("скачать в кеш"))
                            .clicked()
                    {
                        self.enqueue_download(song.clone());
                    }
                });
            }
        });
    }

    fn play_album(&mut self, album: api::Album) {
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

    fn download_album(&mut self, album: api::Album) {
        if album.id.starts_with(LOCAL_ALBUM_PREFIX) {
            self.notice = Some(crate::i18n::tr("этот альбом уже лежит в папке кеша").into());
            return;
        }
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
            ui.label(theme::window_title(crate::i18n::tr("[ ПОИСК ]")));
            let field = ui.add_sized(
                [420.0, 28.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text(crate::i18n::tr("артист, альбом или трек…")),
            );
            if (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.button(crate::i18n::tr("найти")).clicked()
            {
                self.run_search();
            }
            if self.loading.is_some() {
                ui.label(egui::RichText::new(crate::i18n::tr("ищу…")).size(11.0).color(theme::faint()));
            }
        });
        ui.add_space(6.0);
        // Live local matches: the same search box must find hand-dropped and
        // downloaded files, not only server results. Recomputed when the query
        // or the folder scan changes, not every frame.
        let key = (
            self.search_query.trim().to_lowercase(),
            Arc::as_ptr(&self.disk_entries) as usize,
            self.disk_entries.len(),
        );
        if key.0 != self.search_local_key.0 || key.1 != self.search_local_key.1 || key.2 != self.search_local_key.2 {
            self.search_local = Arc::new(search_local_matches(&self.disk_entries, &key.0, self.client.is_none(), 50));
            self.search_local_key = key;
        }
        let search = self.search_result.clone();
        let local = self.search_local.clone();
        if search.is_none() && local.is_empty() {
            let hint = if self.search_query.trim().is_empty() {
                crate::i18n::tr("введите запрос")
            } else {
                crate::i18n::tr("ничего не найдено")
            };
            ui.label(egui::RichText::new(hint).size(11.0).color(theme::faint()));
            return;
        }
        let mut open_artist = None;
        let mut open_album = None;
        let mut play_album = None;
        let mut download_album = None;
        let mut play_song = None;
        let mut download = None;
        let mut play_local: Option<usize> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if let Some(search) = &search {
                if !search.artists.is_empty() {
                    theme::section_label(ui, crate::i18n::tr("АРТИСТЫ"));
                    for artist in &search.artists {
                        if profile_row(ui, &artist.name, artist.album_count as usize, false) {
                            open_artist = Some(artist.clone());
                        }
                    }
                }
                if !search.albums.is_empty() {
                    theme::section_label(ui, crate::i18n::tr("АЛЬБОМЫ"));
                    album_grid(ui, |ui| {
                        for album in &search.albums {
                            let cover = self.cover_texture_of(&album.cover_id, COVER_PX);
                            match album_card(ui, album, cover, true) {
                                AlbumCardAction::Open => open_album = Some(album.clone()),
                                AlbumCardAction::Play => play_album = Some(album.clone()),
                                AlbumCardAction::Download => download_album = Some(album.clone()),
                                AlbumCardAction::None => {}
                            }
                        }
                    });
                }
                if !search.songs.is_empty() {
                    theme::section_label(ui, crate::i18n::tr("ТРЕКИ"));
                    for song in &search.songs {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [(ui.available_width() - 150.0).max(80.0), 24.0],
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{} — {}",
                                        clip(&song.title, 50),
                                        clip(&song.artist, 30)
                                    ))
                                    .size(12.0)
                                    .color(theme::text()),
                                ),
                            );
                            let cached = self.cache.is_indexed(&song.id);
                            if cached {
                                ui.label(
                                    egui::RichText::new(crate::i18n::tr("в кеше")).size(10.0).color(theme::accent()),
                                );
                            } else {
                                ui.add_space(38.0);
                            }
                            if ui.add_enabled(self.player.is_some(), egui::Button::new("▶")).clicked() {
                                play_song = Some(song.clone());
                            }
                            if ui.add_enabled(!cached, egui::Button::new("↓")).clicked() {
                                download = Some(song.clone());
                            }
                        });
                    }
                }
            }
            if !local.is_empty() {
                theme::section_label(
                    ui,
                    if search.is_some() {
                        crate::i18n::tr("ЛОКАЛЬНЫЕ ФАЙЛЫ")
                    } else {
                        crate::i18n::tr("НА ДИСКЕ")
                    },
                );
                for (index, entry) in local.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let playing = self.current.as_ref().is_some_and(|current| current.id == entry.id());
                        ui.add_sized(
                            [(ui.available_width() - 60.0).max(80.0), 24.0],
                            egui::Label::new(entry_row_job(entry, playing)).truncate(),
                        )
                        .on_hover_text(entry_hover(entry));
                        if ui.add_enabled(self.player.is_some(), egui::Button::new("▶")).clicked() {
                            play_local = Some(index);
                        }
                    });
                }
            }
        });
        if let Some(artist) = open_artist {
            self.open_artist(artist);
        }
        if let Some(album) = open_album {
            self.browse_album(album);
        }
        if let Some(album) = play_album {
            self.play_album(album);
        }
        if let Some(album) = download_album {
            self.download_album(album);
        }
        if let Some(song) = play_song {
            self.play_from_library(song);
        }
        if let Some(song) = download {
            self.enqueue_download(song);
        }
        if let Some(index) = play_local {
            self.play_from_library(local[index].to_song());
        }
    }

    /// The main screen: every song in one list — server library, cached
    /// downloads and hand-dropped files, with a filter and per-row actions.
    fn ui_library(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title(crate::i18n::tr("[ БИБЛИОТЕКА ]")));
            if ui.button(crate::i18n::tr("обновить")).clicked() {
                self.refresh_disk();
                self.refresh_server_library(true);
            }
            if self.disk_scan_id.is_some() || self.catalog_scan.is_some() {
                ui.label(egui::RichText::new(crate::i18n::tr("обновляю…")).size(11.0).color(theme::faint()));
            } else {
                let total = self.library_rows.len();
                let disk = self.disk_entries.len();
                ui.label(
                    egui::RichText::new(crate::i18n::trf!(
                        "всего {total} · на диске {disk}",
                        disk = disk,
                        total = total
                    ))
                    .size(11.0)
                    .color(theme::faint()),
                );
            }
            if ui.button(crate::i18n::tr("открыть папку")).clicked() {
                self.open_cache_folder();
            }
        });
        if self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Library) {
            ui.horizontal(|ui| {
                if let Some(scan) = &self.catalog_scan {
                    ui.label(
                        egui::RichText::new(crate::i18n::trf!("загружаю с сервера: {} альбомов", scan.albums))
                            .size(10.0)
                            .color(theme::warn()),
                    );
                }
                if ui
                    .button(crate::i18n::tr("× остановить"))
                    .on_hover_text(crate::i18n::tr("Список останется прежним"))
                    .clicked()
                {
                    self.cancel_catalog_scan();
                }
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 70.0).max(140.0);
            let field = ui.add_sized(
                [width, 26.0],
                egui::TextEdit::singleline(&mut self.library_filter)
                    .font(theme::field_font())
                    .hint_text(crate::i18n::tr("фильтр: название, артист или альбом…")),
            );
            if field.changed() {
                self.rebuild_library_filter();
            }
            if !self.library_filter.is_empty() {
                if ui.button("×").on_hover_text(crate::i18n::tr("сбросить фильтр")).clicked() {
                    self.library_filter.clear();
                    self.rebuild_library_filter();
                }
                let (found, total) = (self.library_filtered.len(), self.library_rows.len());
                ui.label(
                    egui::RichText::new(crate::i18n::trf!("найдено {found} из {total}", found = found, total = total))
                        .size(10.0)
                        .color(theme::faint()),
                );
            }
        });
        ui.add_space(6.0);
        if self.library_rows.is_empty() {
            if self.disk_scan_id.is_some() || self.catalog_scan.is_some() {
                ui.label(egui::RichText::new(crate::i18n::tr("ищу файлы…")).size(12.0).color(theme::faint()));
            } else if self.client.is_none() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(crate::i18n::tr("МУЗЫКА НЕ НАЙДЕНА")).size(13.0).color(theme::warn()));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(crate::i18n::tr(
                            "Добавьте папку с MP3 / FLAC / OGG / Opus / WAV / M4A в настройках",
                        ))
                        .size(11.0)
                        .color(theme::dim()),
                    );
                    ui.add_space(10.0);
                    if ui.add(theme::accent_button(crate::i18n::tr("[ ОТКРЫТЬ ПАПКУ ]"))).clicked() {
                        self.open_cache_folder();
                    }
                });
            } else {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(crate::i18n::tr("БИБЛИОТЕКА ПУСТА")).size(13.0).color(theme::warn()));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(crate::i18n::tr("нажмите «обновить», чтобы загрузить список с сервера"))
                            .size(11.0)
                            .color(theme::dim()),
                    );
                });
            }
            return;
        }
        if self.library_filtered.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(crate::i18n::tr("НИЧЕГО НЕ НАЙДЕНО")).size(13.0).color(theme::warn()));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(crate::i18n::tr("измените фильтр или сбросьте его"))
                        .size(11.0)
                        .color(theme::dim()),
                );
            });
            return;
        }
        let mut play: Option<usize> = None;
        let mut remove: Option<String> = None;
        let mut download: Option<api::Song> = None;
        // A shared handle: cloning the whole list every frame was thousands of
        // string copies per redraw. The filtered view owns its own list.
        let rows = self.library_filtered.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 30.0, rows.len(), |ui, range| {
            for index in range {
                let row = &rows[index];
                ui.horizontal(|ui| {
                    let row_top = ui.cursor().top();
                    let cover = row_cover(
                        ui,
                        24.0,
                        self.cover_texture_of(&row.cover_key(), ROW_COVER_PX),
                        egui::Sense::click(),
                    );
                    if self.player.is_some() && cover.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        play = Some(index);
                    }
                    // Constant slot: local files carry a folder mark, cached
                    // and server rows leave it empty so titles stay aligned.
                    if row.is_local() {
                        local_marker(ui);
                    } else {
                        ui.add_space(14.0);
                    }
                    let playing = self.current.as_ref().is_some_and(|current| current.id == row.id());
                    // Left-aligned fixed-width slot (`add_sized` would centre
                    // the text inside it, leaving a huge gap after the icon).
                    let title_w = (ui.available_width() - 170.0).max(80.0);
                    let title = ui
                        .allocate_ui_with_layout(
                            egui::vec2(title_w, 24.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::Label::new(texts_job(row.title(), row.artist(), row.album(), playing))
                                        .truncate(),
                                )
                            },
                        )
                        .inner;
                    let mut hover = texts_hover(row.title(), row.artist(), row.album());
                    if let Some(path) = row.local_path() {
                        hover.push('\n');
                        hover.push_str(&path.display().to_string());
                    }
                    title.on_hover_text(hover);
                    // Actions and the size/duration are right-aligned: the
                    // play slot appears on row hover and stays put once shown.
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.min_rect().left(), row_top),
                        egui::pos2(ui.max_rect().right(), row_top + 30.0),
                    );
                    let row_hovered = ui.rect_contains_pointer(row_rect);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if row.is_cached() {
                            if ui.button("×").on_hover_text(crate::i18n::tr("удалить из кеша")).clicked()
                            {
                                remove = Some(row.id().to_owned());
                            }
                        } else if row.is_server() {
                            let busy = self.downloads.contains_key(row.id())
                                || self.download_queue.iter().any(|queued| queued.id == row.id());
                            if busy {
                                ui.label(
                                    egui::RichText::new(crate::i18n::tr("качается")).size(10.0).color(theme::warn()),
                                );
                            } else if ui.button("↓").on_hover_text(crate::i18n::tr("скачать в кеш")).clicked()
                            {
                                download = Some(row.to_song());
                            }
                        } else {
                            // Keep the play slot at the same x as other rows.
                            ui.add_space(27.0);
                        }
                        let slot = ui.allocate_exact_size(egui::vec2(30.0, 24.0), egui::Sense::click());
                        if row_hovered || playing {
                            let color = if playing { theme::accent() } else { theme::dim() };
                            ui.painter().text(
                                slot.0.center(),
                                egui::Align2::CENTER_CENTER,
                                "▶",
                                egui::FontId::proportional(13.0),
                                color,
                            );
                            if self.player.is_some() && slot.1.clicked() {
                                play = Some(index);
                            }
                            let _ = slot.1.on_hover_text(crate::i18n::tr("слушать"));
                        }
                        let info = match row.size() {
                            Some(bytes) => human_size(bytes),
                            None => format_time(row.duration()),
                        };
                        ui.label(egui::RichText::new(info).size(10.0).color(theme::faint()));
                    });
                });
            }
        });
        if let Some(index) = play {
            let song = rows[index].to_song();
            self.play_from_library(song);
        }
        if let Some(id) = remove {
            match self.cache.remove(&id) {
                Ok(()) => {
                    let kept: Vec<DiskEntry> =
                        self.disk_entries.iter().filter(|entry| entry.id() != id).cloned().collect();
                    self.set_disk_entries(kept);
                    self.notice = Some(crate::i18n::tr("удалено из кеша").into());
                }
                Err(err) => self.notice = Some(err),
            }
        }
        if let Some(song) = download {
            self.enqueue_download(song);
        }
    }

    /// Top tracks by locally counted listens, so «most played» also works for
    /// hand-dropped files without any server.
    fn ui_frequent(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title(crate::i18n::tr("[ ЧАСТО ПРОСЛУШИВАЕМЫЕ ]")));
            if ui.button(crate::i18n::tr("обновить")).clicked() {
                self.refresh_disk();
            }
            if self.disk_scan_id.is_some() {
                ui.label(egui::RichText::new(crate::i18n::tr("сканирую папку…")).size(11.0).color(theme::faint()));
            } else if !self.frequent_entries.is_empty() {
                ui.label(
                    egui::RichText::new(crate::i18n::trf!("{} треков по истории BEAT", self.frequent_entries.len()))
                        .size(11.0)
                        .color(theme::faint()),
                );
            }
        });
        ui.add_space(6.0);
        if self.frequent_dirty {
            self.rebuild_frequent();
        }
        if self.frequent_entries.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                if self.disk_entries.is_empty() && self.disk_scan_id.is_none() {
                    ui.label(egui::RichText::new(crate::i18n::tr("МУЗЫКА НЕ НАЙДЕНА")).size(13.0).color(theme::warn()));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(crate::i18n::tr(
                            "закиньте файлы в папку кеша или скачайте треки с сервера",
                        ))
                        .size(11.0)
                        .color(theme::dim()),
                    );
                } else if self.disk_scan_id.is_some() {
                    ui.label(egui::RichText::new(crate::i18n::tr("ищу файлы…")).size(12.0).color(theme::faint()));
                } else {
                    ui.label(
                        egui::RichText::new(crate::i18n::tr("ПОКА НЕТ ИСТОРИИ ПРОСЛУШИВАНИЙ"))
                            .size(13.0)
                            .color(theme::warn()),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(crate::i18n::tr(
                            "включите что-нибудь — треки появятся здесь по числу прослушиваний",
                        ))
                        .size(11.0)
                        .color(theme::dim()),
                    );
                }
            });
            return;
        }
        let mut play: Option<usize> = None;
        let entries = self.frequent_entries.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 30.0, entries.len(), |ui, range| {
            for index in range {
                let (entry, count) = &entries[index];
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [26.0, 24.0],
                        egui::Label::new(
                            egui::RichText::new(format!("{:02}", index + 1)).size(11.0).color(theme::faint()),
                        ),
                    );
                    let cover = row_cover(
                        ui,
                        24.0,
                        self.cover_texture_of(&entry.cover_key(), ROW_COVER_PX),
                        egui::Sense::click(),
                    );
                    if self.player.is_some() && cover.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        play = Some(index);
                    }
                    if entry.is_local() {
                        local_marker(ui);
                    } else {
                        ui.add_space(14.0);
                    }
                    let playing = self.current.as_ref().is_some_and(|current| current.id == entry.id());
                    let title_w = (ui.available_width() - 120.0).max(80.0);
                    let title = ui
                        .allocate_ui_with_layout(
                            egui::vec2(title_w, 24.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| ui.add(egui::Label::new(entry_row_job(entry, playing)).truncate()),
                        )
                        .inner;
                    title.on_hover_text(entry_hover(entry));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(play_count_label(*count)).size(10.0).color(theme::accent()));
                    });
                });
            }
        });
        if let Some(index) = play {
            let song = entries[index].0.to_song();
            let play_queue = entries.iter().map(|(entry, _)| entry.to_song()).collect::<Vec<_>>();
            self.play_song(song, play_queue, index);
        }
    }

    fn ui_settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut save = false;
        let mut cancel = false;
        let mut check = false;
        let mut pick_dir = false;
        let mut add_library_dir = false;
        let mut remove_library_dir = None;
        let save_error = self.save_error.clone();
        let checking = self.settings_checking;
        let check_result = self.settings_check.clone();
        let mut show_password = self.show_password;
        {
            let draft = &mut self.settings_draft;
            egui::Window::new(theme::window_title(crate::i18n::tr("[ НАСТРОЙКИ ]")))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .max_height((ctx.screen_rect().height() - 60.0).max(300.0))
                .vscroll(true)
                .show(ctx, |ui| {
                    ui.set_width(620.0);
                    field_label(ui, crate::i18n::tr("> адрес Navidrome (https://…)"));
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.server_url).font(theme::field_font())
                            .hint_text("https://music.example.com"));
                    field_label(ui, crate::i18n::tr("> логин"));
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.user).font(theme::field_font()));
                    field_label(ui, crate::i18n::tr("> пароль"));
                    ui.horizontal(|ui| {
                        let field = ui.add_sized([420.0, 30.0],
                            egui::TextEdit::singleline(&mut draft.password).font(theme::field_font())
                                .password(!show_password));
                        if field.changed() {
                            draft.forget_unreadable_password();
                        }
                        if ui.add(egui::Button::new(if show_password { crate::i18n::tr("[ скрыть ]") } else { crate::i18n::tr("[ показать ]") })).clicked() {
                            show_password = !show_password;
                        }
                    });
                    field_label(ui, crate::i18n::tr("> папка кеша"));
                    ui.horizontal(|ui| {
                        let dir = draft.cache_dir.clone();
                        let hint = if dir.trim().is_empty() {
                            crate::i18n::trf!("по умолчанию: {}", Config::default().cache_root().to_string_lossy())
                        } else { dir };
                        ui.add_sized([420.0, 30.0], egui::Label::new(
                            egui::RichText::new(hint).size(11.0).color(theme::dim())));
                        if ui.button(crate::i18n::tr("[ выбрать ]")).clicked() { pick_dir = true; }
                        if ui.button(crate::i18n::tr("[ сбросить ]")).clicked() { draft.cache_dir.clear(); }
                    });
                    field_label(ui, crate::i18n::tr("> музыкальные папки"));
                    egui::ScrollArea::vertical().id_salt("music-roots").max_height(110.0).show(ui, |ui| {
                        for (index, dir) in draft.library_dirs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_sized([530.0, 24.0], egui::Label::new(dir).truncate()).on_hover_text(dir);
                                if ui.button("×").on_hover_text(crate::i18n::tr("Убрать папку из библиотеки")).clicked() { remove_library_dir = Some(index); }
                            });
                        }
                    });
                    if ui.add_enabled(draft.library_dirs.len() < 16, egui::Button::new(crate::i18n::tr("[ ДОБАВИТЬ ПАПКУ ]"))).clicked() { add_library_dir = true; }
                    ui.label(egui::RichText::new(crate::i18n::tr("Эти папки читаются напрямую; очистка кеша их не затрагивает.")).size(10.0).color(theme::dim()));
                    field_label(ui, crate::i18n::tr("> формат загрузки"));
                    egui::ComboBox::from_id_salt("format")
                        .selected_text(egui::RichText::new(draft.stream_format.label()).size(12.0))
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Raw, crate::i18n::tr("оригинал"));
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Mp3, crate::i18n::tr("mp3 (транскод сервером)"));
                        });
                    if draft.stream_format == StreamFormat::Mp3 {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(crate::i18n::tr("битрейт, kbps")).size(11.0).color(theme::dim()));
                            ui.add(egui::DragValue::new(&mut draft.bit_rate).speed(16.0).range(64..=320));
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(crate::i18n::tr("параллельных загрузок")).size(11.0).color(theme::dim()));
                        ui.add(egui::DragValue::new(&mut draft.parallel_downloads).speed(1.0).range(1..=3));
                    });
                    ui.add_space(6.0);
                    ui.checkbox(&mut draft.auto_cache_new, crate::i18n::tr("автоматически кешировать новые песни"));
                    ui.label(egui::RichText::new(crate::i18n::tr("При первом включении запоминает текущие треки; затем проверяет сервер каждые 10 минут, пока BEAT открыт."))
                        .size(10.0).color(theme::dim()));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!checking, egui::Button::new(crate::i18n::tr("[ ПРОВЕРИТЬ СВЯЗЬ ]"))).clicked() {
                            check = true;
                        }
                        if checking {
                            ui.label(egui::RichText::new(crate::i18n::tr("проверяю…")).size(11.0).color(theme::faint()));
                        } else if let Some(result) = &check_result {
                            let (text, color) = match result {
                                Ok(()) => (crate::i18n::tr("связь есть").into(), theme::accent()),
                                Err(err) => (err.clone(), theme::err()),
                            };
                            ui.label(egui::RichText::new(text).size(11.0).color(color));
                        }
                        if let Some(err) = &save_error {
                            ui.label(egui::RichText::new(crate::i18n::trf!("конфиг не сохранён: {err}", err = err)).size(10.0).color(theme::err()));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(theme::accent_button(crate::i18n::tr("[ СОХРАНИТЬ ]"))).clicked() { save = true; }
                            if ui.add(egui::Button::new(crate::i18n::tr("[ ОТМЕНА ]"))).clicked() { cancel = true; }
                        });
                    });
                });
        }
        if pick_dir {
            if let Some(dir) = pick_folder() {
                self.settings_draft.cache_dir = dir;
            }
        }
        if add_library_dir {
            if let Some(dir) = pick_folder() {
                self.settings_draft.library_dirs.push(dir);
            }
        }
        if let Some(index) = remove_library_dir {
            self.settings_draft.library_dirs.remove(index);
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

    fn ui_queue(&mut self, ctx: &egui::Context) {
        if !self.queue_open {
            return;
        }
        let mut open = self.queue_open;
        let mut action = None;
        egui::Window::new(crate::i18n::tr("[ ОЧЕРЕДЬ ]"))
            .open(&mut open)
            .default_width(620.0)
            .default_height(430.0)
            .show(ctx, |ui| {
                if ui.button(crate::i18n::tr("[ ОСТАВИТЬ ТЕКУЩИЙ ТРЕК ]")).clicked() {
                    action = Some(QueueAction::KeepCurrent);
                }
                if self.play_queue.is_empty() {
                    ui.label(crate::i18n::tr("Очередь пуста"));
                }
                egui::ScrollArea::vertical().show_rows(ui, 28.0, self.play_queue.len(), |ui, rows| {
                    for index in rows {
                        let song = &self.play_queue[index];
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button(if index == self.play_index { "●" } else { "▶" }).clicked() {
                                    action = Some(QueueAction::Play(index));
                                }
                                let text = format!("{} · {}", song.title, song.artist);
                                ui.add_sized(
                                    [(ui.available_width() - 104.0).max(80.0), 24.0],
                                    egui::Label::new(egui::RichText::new(text).color(if index == self.play_index {
                                        theme::accent()
                                    } else {
                                        theme::text()
                                    }))
                                    .truncate(),
                                );
                                if ui.add_enabled(index > 0, egui::Button::new("↑")).clicked() {
                                    action = Some(QueueAction::Move(index, index - 1));
                                }
                                if ui.add_enabled(index + 1 < self.play_queue.len(), egui::Button::new("↓")).clicked()
                                {
                                    action = Some(QueueAction::Move(index, index + 1));
                                }
                                if ui.button("×").on_hover_text(crate::i18n::tr("Убрать из очереди")).clicked()
                                {
                                    action = Some(QueueAction::Remove(index));
                                }
                            });
                        });
                    }
                });
            });
        self.queue_open = open;
        if let Some(action) = action {
            self.edit_queue(action);
        }
    }

    fn edit_queue(&mut self, action: QueueAction) {
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

    /// Borderless-window title bar, same as STRIKE/SNATCH.
    #[cfg(windows)]
    fn ui_title_bar(&mut self, ctx: &egui::Context) {
        const THEME_BTN_WIDTH: f32 = 56.0;
        egui::TopBottomPanel::top("app_titlebar")
            .exact_height(42.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(14.0, 0.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let title_width = (ui.available_width() - 3.0 * 40.0 - THEME_BTN_WIDTH - 40.0).max(0.0);
                    let (rect, drag) =
                        ui.allocate_exact_size(egui::vec2(title_width, 42.0), egui::Sense::click_and_drag());
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
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new(self.cfg.language.switch_label()).frame(false))
                        .on_hover_text("English / Русский")
                        .clicked()
                    {
                        self.cfg.language = self.cfg.language.toggle();
                        self.settings_draft.language = self.cfg.language;
                        i18n::set(self.cfg.language);
                        self.album_list_title = i18n::tr(&self.album_list_title).to_owned();
                        self.save();
                    }
                    let label = if self.dark_mode { crate::i18n::tr("день") } else { crate::i18n::tr("ночь") };
                    if ui
                        .add_sized([THEME_BTN_WIDTH, 38.0], egui::Button::new(label).frame(false))
                        .on_hover_text(if self.dark_mode {
                            crate::i18n::tr("Светлая тема")
                        } else {
                            crate::i18n::tr("Тёмная тема")
                        })
                        .clicked()
                    {
                        self.toggle_theme(ctx);
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("─").frame(false))
                        .on_hover_text(crate::i18n::tr("Свернуть"))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("□").frame(false))
                        .on_hover_text(crate::i18n::tr("Развернуть / восстановить"))
                        .clicked()
                    {
                        self.maximized = !ctx.input(|i| i.viewport().maximized.unwrap_or(self.maximized));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
                    }
                    if ui
                        .add_sized([40.0, 38.0], egui::Button::new("×").frame(false))
                        .on_hover_text(crate::i18n::tr("Закрыть"))
                        .clicked()
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

#[derive(Clone, Copy)]
enum QueueAction {
    Move(usize, usize),
    Remove(usize),
    Play(usize),
    KeepCurrent,
}

fn move_queue_item(queue: &mut Vec<api::Song>, current: &mut usize, from: usize, to: usize) {
    if from >= queue.len() || to >= queue.len() || from == to {
        return;
    }
    let song = queue.remove(from);
    queue.insert(to, song);
    if *current == from {
        *current = to;
    } else if from < *current && to >= *current {
        *current -= 1;
    } else if from > *current && to <= *current {
        *current += 1;
    }
}

fn remove_queue_item(queue: &mut Vec<api::Song>, current: &mut usize, index: usize) {
    if index >= queue.len() {
        return;
    }
    queue.remove(index);
    if index < *current {
        *current -= 1;
    }
    *current = (*current).min(queue.len().saturating_sub(1));
}

/// Outer width of one album card; fixed so the grid packs whole rows.
const ALBUM_CARD_WIDTH: f32 = 184.0;
const ALBUM_CARD_HEIGHT: f32 = 280.0;

/// Reserve the full scroll range, but lay out cards and request artwork only
/// for rows intersecting the viewport. Large local libraries stay responsive.
fn visible_album_grid(
    ui: &mut egui::Ui,
    id: &str,
    albums: &[api::Album],
    mut card: impl FnMut(&mut egui::Ui, &api::Album),
) {
    if albums.is_empty() {
        return;
    }
    let gap = 12.0;
    let width = ui.available_width();
    let columns = ((width + gap) / (ALBUM_CARD_WIDTH + gap)).floor().max(1.0) as usize;
    let rows = albums.len().div_ceil(columns);
    let stride = ALBUM_CARD_HEIGHT + gap;
    let height = rows as f32 * stride - gap;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let clip = ui.clip_rect();
    let start = (((clip.top() - rect.top()) / stride).floor().max(0.0) as usize).min(rows);
    let end = (((clip.bottom() - rect.top()) / stride).ceil().max(0.0) as usize).min(rows);
    for row in start..end {
        let row_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(0.0, row as f32 * stride),
            egui::vec2(width, ALBUM_CARD_HEIGHT),
        );
        ui.allocate_new_ui(egui::UiBuilder::new().id_salt((id, row)).max_rect(row_rect), |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                let start = row * columns;
                for album in &albums[start..(start + columns).min(albums.len())] {
                    card(ui, album);
                }
            });
        });
    }
}

/// Wrapped grid of album cards. `horizontal_wrapped` centers items on the
/// cross axis, so cards of different heights end up at different tops; this
/// top-aligned wrapped layout keeps every row straight and wraps into rows.
fn album_grid(ui: &mut egui::Ui, add_cards: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true), |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
        add_cards(ui);
    });
}

fn album_card(
    ui: &mut egui::Ui,
    album: &api::Album,
    cover: Option<egui::TextureHandle>,
    can_download: bool,
) -> AlbumCardAction {
    let mut action = AlbumCardAction::None;
    // A bare `Frame` never wraps in a wrapped grid: egui advances its cursor
    // without running the wrap decision, and the row runs off the panel.
    // Allocating a fixed-width vertical slot first gives the grid an item it
    // can wrap into rows; the frame then draws inside that slot.
    ui.allocate_ui_with_layout(
        egui::vec2(ALBUM_CARD_WIDTH, ALBUM_CARD_HEIGHT),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .stroke(egui::Stroke::new(1.0, theme::line()))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.set_width(168.0);
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(168.0, 168.0), egui::Sense::click());
                    match cover {
                        Some(texture) => {
                            ui.painter().image(
                                texture.id(),
                                rect,
                                square_cover_uv(texture.size()),
                                egui::Color32::WHITE,
                            );
                        }
                        None => {
                            ui.painter().rect_filled(rect, 0.0, theme::field());
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "♪",
                                egui::FontId::proportional(36.0),
                                theme::faint(),
                            );
                        }
                    }
                    if response.clicked() {
                        action = AlbumCardAction::Open;
                    }
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    ui.add_space(4.0);
                    // Fixed-height single-line rows: cards with different
                    // titles must keep the same height, or the wrapped grid
                    // turns into a staircase.
                    let text_row = |ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(168.0, size + 5.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).truncate());
                            },
                        );
                    };
                    text_row(ui, &clip(&album.name, 30), 12.0, theme::text());
                    text_row(ui, &clip(&album.artist, 30), 10.0, theme::faint());
                    ui.horizontal(|ui| {
                        if ui.button("▶").on_hover_text(crate::i18n::tr("слушать")).clicked() {
                            action = AlbumCardAction::Play;
                        }
                        if can_download && ui.button("↓").on_hover_text(crate::i18n::tr("скачать альбом")).clicked()
                        {
                            action = AlbumCardAction::Download;
                        }
                        if album.year > 0 {
                            ui.label(egui::RichText::new(format!("{}", album.year)).size(10.0).color(theme::faint()));
                        }
                    });
                });
        },
    );
    action
}

fn profile_row(ui: &mut egui::Ui, name: &str, count: usize, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let title = clip_to_width(ui, name, egui::FontId::monospace(13.0), (rect.width() - 44.0).max(10.0));
    let painter = ui.painter();
    if active || response.hovered() {
        painter.rect_filled(rect, 0.0, theme::lift());
    }
    painter.text(
        rect.left_center() + egui::vec2(7.0, 0.0),
        egui::Align2::LEFT_CENTER,
        if active { "●" } else { "○" },
        egui::FontId::monospace(11.0),
        if active { theme::accent() } else { theme::faint() },
    );
    painter.text(
        rect.left_center() + egui::vec2(23.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::monospace(13.0),
        if active { theme::text() } else { theme::dim() },
    );
    if count > 0 {
        painter.text(
            rect.right_center() - egui::vec2(7.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{count:02}"),
            egui::FontId::monospace(10.0),
            theme::faint(),
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// Glyph and hover of the transport play/pause button. It offers «pause»
/// only while the track is really playing; a track restored from the last
/// session (or one that failed to start) shows «play».
fn transport_play_label(has_current: bool, loaded: bool, paused: bool) -> (&'static str, &'static str) {
    if loaded && !paused {
        ("▮▮", crate::i18n::tr("пауза"))
    } else if has_current {
        ("▶", crate::i18n::tr("продолжить"))
    } else {
        ("▶", crate::i18n::tr("начать воспроизведение"))
    }
}

/// Fixed-width toggle for the player modes (shuffle/repeat): accent when on,
/// faint when off, so the transport row never shifts.
fn mode_button(ui: &mut egui::Ui, glyph: &str, active: bool, hover: &str) -> bool {
    let color = if active { theme::accent() } else { theme::faint() };
    let stroke = if active { theme::accent() } else { theme::line() };
    // Matches the transport buttons, so the centred block keeps its width.
    ui.add(
        egui::Button::new(egui::RichText::new(glyph).color(color))
            .stroke(egui::Stroke::new(1.0, stroke))
            .min_size(egui::vec2(40.0, 30.0)),
    )
    .on_hover_text(hover)
    .clicked()
}

/// Square cover thumbnail with the "♪" placeholder used across the app. The
/// response lets callers make the cover itself clickable (list rows).
fn row_cover(ui: &mut egui::Ui, size: f32, texture: Option<egui::TextureHandle>, sense: egui::Sense) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
    match texture {
        Some(texture) => {
            ui.painter().image(texture.id(), rect, square_cover_uv(texture.size()), egui::Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, theme::field());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "♪",
                egui::FontId::proportional(size * 0.5),
                theme::faint(),
            );
        }
    }
    response
}

/// Fill the square cover slot without stretching rectangular textures. New
/// thumbnails are square; this also handles an older/manually replaced cache.
fn square_cover_uv([width, height]: [usize; 2]) -> egui::Rect {
    let full = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    if width == 0 || height == 0 {
        return full;
    }
    if width > height {
        let margin = (1.0 - height as f32 / width as f32) * 0.5;
        egui::Rect::from_min_max(egui::pos2(margin, 0.0), egui::pos2(1.0 - margin, 1.0))
    } else {
        let margin = (1.0 - width as f32 / height as f32) * 0.5;
        egui::Rect::from_min_max(egui::pos2(0.0, margin), egui::pos2(1.0, 1.0 - margin))
    }
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
    let tab = egui::Rect::from_min_max(egui::pos2(x0, rect.top() + 3.0), egui::pos2(x0 + 6.0, rect.top() + 6.0));
    let body = egui::Rect::from_min_max(egui::pos2(x0, rect.top() + 5.0), egui::pos2(x1, rect.bottom() - 3.0));
    painter.rect_filled(tab, 0.0, color);
    painter.rect_filled(body, 1.0, color);
    let _ = response.on_hover_text(crate::i18n::tr("локальный файл — лежит в папке кеша, не скачан с сервера"));
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

/// Stable id of the current default output device; `None` when there is no
/// device or the id cannot be read. Used to notice that the OS switched
/// devices (Bluetooth connected or switched off) while BEAT runs.
fn current_output_device_id() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let device = rodio::cpal::default_host().default_output_device()?;
    device.id().ok().map(|id| id.to_string())
}

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
        match self {
            Self::Cached(entry) => &entry.id,
            Self::Local(track) => &track.id,
        }
    }
    fn title(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.title,
            Self::Local(track) => &track.title,
        }
    }
    fn artist(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.artist,
            Self::Local(track) => &track.artist,
        }
    }
    fn album(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.album,
            Self::Local(track) => &track.album,
        }
    }
    fn size(&self) -> u64 {
        match self {
            Self::Cached(entry) => entry.size,
            Self::Local(track) => track.size,
        }
    }
    fn duration(&self) -> f64 {
        match self {
            Self::Cached(entry) => entry.duration,
            Self::Local(track) => track.duration,
        }
    }
    fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
    fn local_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Cached(_) => None,
            Self::Local(track) => Some(&track.path),
        }
    }
    /// Cover key for the artwork embedded in this row's file on disk.
    fn cover_key(&self) -> String {
        let rel = match self {
            Self::Cached(entry) => &entry.path,
            Self::Local(track) => track.id.strip_prefix(local::LOCAL_ID_PREFIX).unwrap_or(&track.rel),
        };
        format!("{FILE_COVER_PREFIX}{rel}")
    }
    fn to_song(&self) -> api::Song {
        match self {
            Self::Cached(entry) => cached_to_song(entry),
            Self::Local(track) => track.to_song(),
        }
    }
}

/// One row of the unified library: a file on disk or a server-only song.
#[derive(Clone)]
enum LibRow {
    Disk(DiskEntry),
    Server(api::Song),
}

impl LibRow {
    fn id(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.id(),
            Self::Server(song) => &song.id,
        }
    }
    fn title(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.title(),
            Self::Server(song) => &song.title,
        }
    }
    fn artist(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.artist(),
            Self::Server(song) => &song.artist,
        }
    }
    fn album(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.album(),
            Self::Server(song) => &song.album,
        }
    }
    fn duration(&self) -> f64 {
        match self {
            Self::Disk(entry) => entry.duration(),
            Self::Server(song) => song.duration,
        }
    }
    /// Human size of the file; server-only songs have none to show.
    fn size(&self) -> Option<u64> {
        match self {
            Self::Disk(entry) => Some(entry.size()),
            Self::Server(_) => None,
        }
    }
    fn is_local(&self) -> bool {
        matches!(self, Self::Disk(entry) if entry.is_local())
    }
    fn is_cached(&self) -> bool {
        matches!(self, Self::Disk(entry) if !entry.is_local())
    }
    fn is_server(&self) -> bool {
        matches!(self, Self::Server(_))
    }
    /// Cover key: `<rel>`-based file path for disk rows, cover id for server.
    fn cover_key(&self) -> String {
        match self {
            Self::Disk(entry) => entry.cover_key(),
            Self::Server(song) => song.cover_id.clone(),
        }
    }
    fn local_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Disk(entry) => entry.local_path(),
            Self::Server(_) => None,
        }
    }
    fn to_song(&self) -> api::Song {
        match self {
            Self::Disk(entry) => entry.to_song(),
            Self::Server(song) => song.clone(),
        }
    }
}

/// Merged library: every disk file plus server songs that are not cached
/// yet, sorted artist/album/title. Cached ids are skipped so a downloaded
/// song does not appear twice.
fn build_library_rows(disk: &[DiskEntry], server: &[api::Song]) -> Vec<LibRow> {
    let cached: HashSet<&str> = disk.iter().filter(|entry| !entry.is_local()).map(DiskEntry::id).collect();
    let mut rows: Vec<LibRow> = disk.iter().cloned().map(LibRow::Disk).collect();
    rows.extend(
        server
            .iter()
            .filter(|song| !song.id.is_empty() && !cached.contains(song.id.as_str()))
            .cloned()
            .map(LibRow::Server),
    );
    // Cached keys: one lowercase allocation per row instead of per comparison,
    // which matters for large server libraries rebuilt on every rescan.
    rows.sort_by_cached_key(|row| {
        (row.artist().to_lowercase(), row.album().to_lowercase(), row.title().to_lowercase())
    });
    rows
}

/// Position of a song in the full library rows; `None` when the library does
/// not know it (then it plays alone).
fn library_queue_start(rows: &[LibRow], song_id: &str) -> Option<usize> {
    rows.iter().position(|row| row.id() == song_id)
}

/// What a bare «play» press queues when nothing is playing: the visible
/// (filtered) library list while the library screen is open, else all rows.
fn idle_start_rows<'a>(
    view: View,
    library_rows: &'a Arc<Vec<LibRow>>,
    library_filtered: &'a Arc<Vec<LibRow>>,
) -> &'a Arc<Vec<LibRow>> {
    if view == View::Library && !library_filtered.is_empty() {
        library_filtered
    } else {
        library_rows
    }
}

/// An album assembled from hand-dropped local files, so the library sections
/// («новые альбомы», «все артисты», поиск) also work without a server.
#[derive(Clone)]
struct LocalAlbum {
    id: String,
    name: String,
    artist: String,
    /// `file:<rel>` cover key of one of the tracks (may resolve to none).
    cover_id: String,
    songs: Vec<api::Song>,
    duration: f64,
}

const LOCAL_ALBUM_PREFIX: &str = "local-album:";
const LOCAL_ARTIST_PREFIX: &str = "local-artist:";

impl LocalAlbum {
    fn to_api_album(&self) -> api::Album {
        api::Album {
            id: self.id.clone(),
            name: self.name.clone(),
            artist: self.artist.clone(),
            cover_id: self.cover_id.clone(),
            year: 0,
            duration: self.duration as u64,
        }
    }
}

fn local_album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1}{album}")
}

/// Groups hand-dropped files into albums (artist + album tag, with folder
/// fallbacks already applied by the scanner) and stable ids.
fn build_local_albums(entries: &[DiskEntry]) -> Vec<LocalAlbum> {
    let mut map: HashMap<String, LocalAlbum> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry.is_local()) {
        let key = local_album_key(entry.artist().trim(), entry.album().trim());
        let album = map.entry(key.clone()).or_insert_with(|| LocalAlbum {
            id: format!("{LOCAL_ALBUM_PREFIX}{key}"),
            name: if entry.album().trim().is_empty() {
                crate::i18n::tr("без альбома").into()
            } else {
                entry.album().trim().to_owned()
            },
            artist: if entry.artist().trim().is_empty() {
                crate::i18n::tr("неизвестный артист").into()
            } else {
                entry.artist().trim().to_owned()
            },
            cover_id: entry.cover_key(),
            songs: Vec::new(),
            duration: 0.0,
        });
        album.duration += entry.duration();
        album.songs.push(entry.to_song());
    }
    for album in map.values_mut() {
        album.songs.sort_by_cached_key(|song| song.title.to_lowercase());
    }
    let mut albums: Vec<LocalAlbum> = map.into_values().collect();
    albums.sort_by_cached_key(|album| (album.artist.to_lowercase(), album.name.to_lowercase()));
    albums
}

/// Artist rows for the local albums, shaped like server artists so the same
/// list can show both.
fn build_local_artists(albums: &[LocalAlbum]) -> Vec<api::Artist> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for album in albums {
        *counts.entry(album.artist.clone()).or_insert(0) += 1;
    }
    let mut artists: Vec<api::Artist> = counts
        .into_iter()
        .map(|(name, album_count)| api::Artist { id: format!("{LOCAL_ARTIST_PREFIX}{name}"), name, album_count })
        .collect();
    artists.sort_by_cached_key(|artist| artist.name.to_lowercase());
    artists
}

/// Case-insensitive matches of `needle` over title/artist/album. `include_cached`
/// is true without a server, so the search also finds downloaded files.
fn search_local_matches(entries: &[DiskEntry], needle: &str, include_cached: bool, limit: usize) -> Vec<DiskEntry> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|entry| {
            (include_cached || entry.is_local())
                && (entry.title().to_lowercase().contains(&needle)
                    || entry.artist().to_lowercase().contains(&needle)
                    || entry.album().to_lowercase().contains(&needle))
        })
        .take(limit)
        .cloned()
        .collect()
}

/// Dim separator between the title, artist and album of a list row.
const ROW_SEP: &str = "  ·  ";

/// List row as one coloured line: title, then a dim artist, then a fainter
/// album. Missing parts leave no dangling separators, and a long row is cut
/// off by the label's truncation (the full text stays in the hover).
fn texts_job(title: &str, artist: &str, album: &str, playing: bool) -> egui::text::LayoutJob {
    let format = |size: f32, color: egui::Color32| egui::TextFormat {
        font_id: egui::FontId::new(size, egui::FontFamily::Proportional),
        color,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(title, 0.0, format(12.0, if playing { theme::accent() } else { theme::text() }));
    if !artist.trim().is_empty() {
        job.append(ROW_SEP, 0.0, format(12.0, theme::faint()));
        job.append(artist, 0.0, format(11.5, theme::dim()));
    }
    if !album.trim().is_empty() {
        job.append(ROW_SEP, 0.0, format(12.0, theme::faint()));
        job.append(album, 0.0, format(11.5, theme::faint()));
    }
    job
}

fn entry_row_job(entry: &DiskEntry, playing: bool) -> egui::text::LayoutJob {
    texts_job(entry.title(), entry.artist(), entry.album(), playing)
}

/// Full, untruncated row text for the hover.
fn texts_hover(title: &str, artist: &str, album: &str) -> String {
    let mut out = title.trim().to_owned();
    if !artist.trim().is_empty() {
        out.push_str(" — ");
        out.push_str(artist.trim());
    }
    if !album.trim().is_empty() {
        out.push('\n');
        out.push_str(album.trim());
    }
    out
}

/// `texts_hover` plus the file path for rows that have one on disk.
fn entry_hover(entry: &DiskEntry) -> String {
    let mut out = texts_hover(entry.title(), entry.artist(), entry.album());
    if let Some(path) = entry.local_path() {
        out.push('\n');
        out.push_str(&path.display().to_string());
    }
    out
}

/// «1 раз», «2 раза», «5 раз» — Russian plural for the listen counter.
fn play_count_label(count: u64) -> String {
    if i18n::current() == i18n::Language::En {
        return format!("{count} {}", if count == 1 { "play" } else { "plays" });
    }
    let word = if (11..=14).contains(&(count % 100)) {
        crate::i18n::tr("раз")
    } else {
        match count % 10 {
            2..=4 => crate::i18n::tr("раза"),
            _ => crate::i18n::tr("раз"),
        }
    };
    format!("{count} {word}")
}

/// Top tracks by locally counted listens, title as the tie-break. Counts are
/// the only source: a track never played in BEAT has nothing to show yet.
fn top_played(entries: &[DiskEntry], stats: &stats::Stats, limit: usize) -> Vec<(DiskEntry, u64)> {
    let mut list: Vec<(DiskEntry, u64)> = entries
        .iter()
        .filter_map(|entry| {
            let count = stats.count(entry.id());
            if count > 0 {
                Some((entry.clone(), count))
            } else {
                None
            }
        })
        .collect();
    // `Reverse` keeps the descending count order under a cached sort key.
    list.sort_by_cached_key(|(entry, count)| (std::cmp::Reverse(*count), entry.title().to_lowercase()));
    list.truncate(limit);
    list
}

/// Reorders the list with the same tiny PRNG used for playback shuffle.
fn shuffle_albums(albums: &mut Vec<api::Album>) {
    let (order, _) = shuffled_order(albums.len(), 0);
    let mut slots: Vec<Option<api::Album>> = albums.drain(..).map(Some).collect();
    for index in order {
        if let Some(album) = slots[index].take() {
            albums.push(album);
        }
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
    if seed == 0 {
        seed = 0x9e37_79b9_7f4a_7c15;
    }
    for i in (1..len).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        order.swap(i, seed as usize % (i + 1));
    }
    let pos = order.iter().position(|&index| index == current).unwrap_or(0);
    (order, pos)
}

/// Indexed downloads plus local files, sorted artist/album/title like the
/// cache list itself.
fn merge_disk_entries(indexed: Vec<CachedTrack>, local: Vec<LocalTrack>) -> Vec<DiskEntry> {
    let mut entries: Vec<DiskEntry> =
        indexed.into_iter().map(DiskEntry::Cached).chain(local.into_iter().map(DiskEntry::Local)).collect();
    // One lowercase per row instead of one per comparison.
    entries.sort_by_cached_key(|entry| {
        (entry.artist().to_lowercase(), entry.album().to_lowercase(), entry.title().to_lowercase())
    });
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

fn profile_key(cfg: &Config) -> String {
    use md5::{Digest, Md5};
    let server = api::Server::from_config(cfg).catalog_key().unwrap_or_default();
    format!("{:x}", Md5::digest(format!("{server}\0{}", cfg.cache_root().to_string_lossy())))
}

/// Seeking changes source position instantly; it must not manufacture minutes
/// of listening. Paused time and backward seeks add nothing to the counter.
fn listen_advance(previous: f64, position: f64, elapsed: f64, paused: bool) -> f64 {
    if paused || !previous.is_finite() || !position.is_finite() || !elapsed.is_finite() {
        return 0.0;
    }
    (position - previous).max(0.0).min(elapsed.max(0.0))
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
        if width(&cand) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return "…".to_owned();
    }
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
    if pointer_down {
        None
    } else {
        pending
    }
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
        crate::i18n::trf!("{kb:.0} КБ", kb = kb)
    } else if kb < 1024.0 * 1024.0 {
        crate::i18n::trf!("{:.1} МБ", kb / 1024.0)
    } else {
        crate::i18n::trf!("{:.2} ГБ", kb / 1024.0 / 1024.0)
    }
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
        // Crop a borrowed view BEFORE resizing. Resizing a very wide image to
        // fill a square first could allocate a huge intermediate bitmap.
        let (width, height) = (image.width(), image.height());
        let side = width.min(height);
        if side == 0 || size == 0 {
            return None;
        }
        let cropped = image::imageops::crop_imm(&image, (width - side) / 2, (height - side) / 2, side, side);
        let thumb = image::imageops::thumbnail(&*cropped, size, size);
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [thumb.width() as usize, thumb.height() as usize],
            thumb.as_raw(),
        ))
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
fn pick_folder() -> Option<String> {
    None
}

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
    Some(ctx.load_texture(
        "app-icon",
        egui::ColorImage::from_rgba_unmultiplied(size, &pixels),
        egui::TextureOptions::LINEAR,
    ))
}

/// Borderless windows have no native resize border; begin the OS resize on
/// mouse-down so an outward drag is not lost.
#[cfg(windows)]
fn resize_edges(ctx: &egui::Context) {
    let screen = ctx.screen_rect();
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
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
        egui::Area::new(egui::Id::new(("window_resize", i))).order(egui::Order::Foreground).fixed_pos(pos).show(
            ctx,
            |ui| {
                let (_, response) = ui.allocate_exact_size(size, egui::Sense::drag());
                if response.hovered() {
                    let icon = match direction {
                        egui::ResizeDirection::North | egui::ResizeDirection::South => egui::CursorIcon::ResizeVertical,
                        egui::ResizeDirection::East | egui::ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
                        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
                            egui::CursorIcon::ResizeNwSe
                        }
                        egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
                            egui::CursorIcon::ResizeNeSw
                        }
                    };
                    ctx.set_cursor_icon(icon);
                }
                if response.is_pointer_button_down_on() && ctx.input(|input| input.pointer.primary_pressed()) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                }
            },
        );
    }
}

/// A windows-subsystem exe has no console: a startup failure must be shown in
/// a MessageBox or the process dies silently.
#[cfg(windows)]
fn fatal_dialog(title: &str, text: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
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

/// Two copies would race on the cache index and config, so a named mutex
/// refuses the second window. The name is `Global\`, not `Local\`: a
/// per-session name would let another session (RDP, fast user switching) start
/// its own copy over the same cache folder and config.
#[cfg(windows)]
fn acquire_single_instance(name: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn CreateMutexW(attrs: *const c_void, initially_owned: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(crate::i18n::trf!("не удалось создать мьютекс: {}", std::io::Error::last_os_error()));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(crate::i18n::tr("BEAT уже запущен").into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn acquire_single_instance(_name: &str) -> Result<(), String> {
    Ok(())
}

fn main() -> eframe::Result {
    let demo = demo::Snapshot::from_args().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2)
    });
    if demo.is_none() {
        if let Err(err) = acquire_single_instance("Global\\beat-single-instance") {
            fatal_dialog(
                "BEAT",
                &crate::i18n::trf!("{err}.\n\nЗакройте уже открытое окно BEAT и запустите это снова.", err = err),
            );
            std::process::exit(1);
        }
    }
    std::panic::set_hook(Box::new(|info| {
        if HANDLED_PANIC.with(|handled| handled.get()) {
            return;
        }
        fatal_dialog(
            crate::i18n::tr("BEAT — внутренняя ошибка"),
            &crate::i18n::trf!("BEAT не смог продолжить работу.\n\n{info}", info = info),
        );
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
    if demo.is_some() {
        viewport = viewport.with_inner_size([1280.0, 800.0]).with_resizable(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native("BEAT", options, Box::new(move |cc| Ok(Box::new(BeatApp::new(cc, demo)))));
    if let Err(e) = &result {
        fatal_dialog(
            crate::i18n::tr("BEAT — не удалось открыть окно"),
            &crate::i18n::trf!("Причина: {e}\n\nДля запуска нужен OpenGL 2.1+. Включите 3D-ускорение в настройках ВМ либо запустите программу на обычном рабочем столе.", e = e),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_edits_preserve_the_current_occurrence_even_with_duplicate_ids() {
        let songs: Vec<_> =
            (0..6).map(|i| api::Song { id: "same-id".into(), title: i.to_string(), ..Default::default() }).collect();
        for current in 0..songs.len() {
            for from in 0..songs.len() {
                for to in 0..songs.len() {
                    let mut queue = songs.clone();
                    let mut index = current;
                    move_queue_item(&mut queue, &mut index, from, to);
                    assert_eq!(queue[index].title, songs[current].title);
                    let mut titles: Vec<_> = queue.iter().map(|song| song.title.clone()).collect();
                    titles.sort();
                    assert_eq!(titles, (0..6).map(|i| i.to_string()).collect::<Vec<_>>());
                }
            }
        }
        for current in 0..songs.len() {
            for removed in 0..songs.len() {
                let mut queue = songs.clone();
                let mut index = current;
                remove_queue_item(&mut queue, &mut index, removed);
                assert_eq!(queue.len(), 5);
                if current != removed {
                    assert_eq!(queue[index].title, songs[current].title);
                } else {
                    assert_eq!(
                        queue[index].title,
                        songs[if removed + 1 < songs.len() { removed + 1 } else { removed - 1 }].title
                    );
                }
            }
        }
        let mut empty = Vec::new();
        let mut index = 0;
        remove_queue_item(&mut empty, &mut index, usize::MAX);
        move_queue_item(&mut empty, &mut index, 0, usize::MAX);
        assert!(empty.is_empty());
    }

    #[test]
    fn seeking_and_pausing_do_not_manufacture_listens() {
        let mut listened = 0.0;
        let mut previous = 0.0;
        for step in 1..=100 {
            let position = step as f64 * 30.0;
            listened += listen_advance(previous, position, 0.001, false);
            previous = position;
        }
        assert!(listened < MIN_LISTEN_SECS, "rapid seeks counted as a listen");
        listened += listen_advance(previous, previous + 10.0, 100.0, true);
        assert!(listened < MIN_LISTEN_SECS, "paused time counted as a listen");
        assert_eq!(listen_advance(previous, 0.0, 1.0, false), 0.0);
        assert_eq!(listen_advance(0.0, 2.0, 2.0, false), 2.0);
    }

    #[test]
    fn a_large_album_grid_draws_only_visible_cards_and_keeps_the_scroll_range() {
        let ctx = egui::Context::default();
        theme::apply(&ctx, true);
        let albums: Vec<_> =
            (0..5000).map(|id| api::Album { id: id.to_string(), name: "Album".into(), ..Default::default() }).collect();
        let mut drawn = 0;
        let mut max_height = 0.0f32;
        let mut content_height = 0.0;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 600.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let scroll = egui::ScrollArea::vertical().show(ui, |ui| {
                        visible_album_grid(ui, "test-albums", &albums, |ui, album| {
                            let start = ui.cursor().top();
                            album_card(ui, album, None, true);
                            max_height = max_height.max(ui.min_rect().bottom() - start);
                            drawn += 1;
                        });
                    });
                    content_height = scroll.content_size.y;
                });
            },
        );
        assert!(drawn > 0 && drawn <= 12, "laid out {drawn} of 5000 cards");
        assert!(max_height <= ALBUM_CARD_HEIGHT + 1.0, "card height {max_height} exceeds its reserved row");
        assert!(content_height > 400_000.0, "offscreen rows disappeared: {content_height}");
    }

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
        assert!(
            playing >= std::time::Duration::from_millis(50) && playing <= std::time::Duration::from_millis(250),
            "{playing:?}"
        );
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
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        assert!(decode_cover_sized(&png(64, 64), 32).is_some(), "an ordinary cover must decode");
        assert!(decode_cover_sized(&png(9_000, 8), 32).is_none(), "9000 px wide accepted");
        assert!(decode_cover_sized(&png(8, 9_000), 32).is_none(), "9000 px tall accepted");
    }

    #[test]
    fn rectangular_artwork_is_cropped_before_making_a_square_thumbnail() {
        for (width, height) in [(128, 72), (72, 128), (72, 72), (8192, 1)] {
            let side = width.min(height);
            let left = (width - side) / 2;
            let top = (height - side) / 2;
            let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
                if x >= left && x < left + side && y >= top && y < top + side {
                    image::Rgba([40, 100, 160, 255])
                } else {
                    image::Rgba([0, 0, 0, 255])
                }
            });
            let mut bytes = Vec::new();
            pixels.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
            let cover = decode_cover_sized(&bytes, 32).unwrap();
            assert_eq!(cover.size, [32, 32], "{width} x {height}");
            assert!(
                cover.pixels.iter().all(|p| *p == egui::Color32::from_rgb(40, 100, 160)),
                "the outer bars must be cropped, not squeezed"
            );
            assert!(decode_cover_sized(&bytes, 0).is_none());
        }
    }

    #[test]
    fn square_cover_uv_keeps_cached_rectangles_in_proportion() {
        for size in [[48, 27], [27, 48], [48, 48]] {
            let uv = square_cover_uv(size);
            let width = uv.width() * size[0] as f32;
            let height = uv.height() * size[1] as f32;
            assert!((width - height).abs() < 0.001);
            assert!((uv.center().x - 0.5).abs() < 0.001);
            assert!((uv.center().y - 0.5).abs() < 0.001);
        }
    }

    #[test]
    fn a_decoder_attempt_waits_for_real_progress() {
        const FIRST: u64 = cache::PLAYBACK_BUFFER_BYTES;
        // Nothing has arrived yet.
        assert!(!buffering_attempt_ready(FIRST - 1, FIRST, 0, false));
        // The first attempt goes ahead as soon as the buffer is there.
        assert!(buffering_attempt_ready(FIRST, FIRST, 0, false));
        // Same bytes as the last failed attempt: retrying now would rebuild the
        // decoder every frame.
        assert!(!buffering_attempt_ready(2 * FIRST, 2 * FIRST, 2 * FIRST, false));
        assert!(!buffering_attempt_ready(2 * FIRST, FIRST, 2 * FIRST, false));
        // Enough new data, but not past the (grown) requirement yet.
        assert!(!buffering_attempt_ready(2 * FIRST, 2 * FIRST, FIRST, false));
        // Both conditions met.
        assert!(buffering_attempt_ready(3 * FIRST, 2 * FIRST, FIRST, false));
        // A finished download always gets a final attempt.
        assert!(buffering_attempt_ready(7, u64::MAX, 7, true));
        // Attempts stay logarithmic: a 64 MB track arriving in 64 KB chunks must
        // not be probed on every chunk, which is what the old per-frame gate did.
        let mut downloaded = 0u64;
        let mut attempted_at = 0u64;
        let mut needed = FIRST;
        let mut attempts = 0;
        while downloaded < 64 * 1024 * 1024 {
            downloaded += 64 * 1024;
            if buffering_attempt_ready(downloaded, needed, attempted_at, false) {
                attempts += 1;
                attempted_at = downloaded;
                needed = needed.saturating_mul(2);
            }
        }
        assert!((1..=16).contains(&attempts), "{attempts} decoder attempts for a 64 MB track is not logarithmic");
        assert!(downloaded >= 64 * 1024 * 1024);
    }

    #[test]
    fn repeat_one_does_not_restart_a_track_that_never_played() {
        assert!(should_repeat_one(Repeat::One, 12.0));
        assert!(should_repeat_one(Repeat::One, 0.5));
        // A source that produced no audio at all: advance instead of spinning.
        assert!(!should_repeat_one(Repeat::One, 0.0));
        assert!(!should_repeat_one(Repeat::All, 30.0));
        assert!(!should_repeat_one(Repeat::Off, 30.0));
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
        let missing: Vec<char> =
            "▶▮◀■⇄↻1♪×".chars().filter(|c| !ctx.fonts(|fonts| fonts.has_glyph(&button, *c))).collect();
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

    fn local_entry(id: &str, title: &str, artist: &str, album: &str) -> DiskEntry {
        DiskEntry::Local(LocalTrack {
            id: format!("local:{id}.mp3"),
            path: std::path::PathBuf::from(format!("{id}.mp3")),
            rel: format!("{id}.mp3"),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration: 1.0,
            suffix: "mp3".into(),
            size: 1,
        })
    }

    #[test]
    fn labels_skip_missing_artist_and_album() {
        let bare = local_entry("x", "T", "", "");
        assert_eq!(bare.artist(), "");
        assert_eq!(entry_row_job(&bare, false).text, "T");
        assert_eq!(entry_hover(&bare), "T\nx.mp3");
        let tagged = DiskEntry::Cached(CachedTrack {
            id: "1".into(),
            path: "x".into(),
            title: "T".into(),
            artist: "A".into(),
            album: "B".into(),
            duration: 1.0,
            suffix: "mp3".into(),
            size: 1,
            format: "raw".into(),
        });
        assert_eq!(entry_row_job(&tagged, false).text, format!("T{ROW_SEP}A{ROW_SEP}B"));
        assert_eq!(entry_hover(&tagged), "T — A\nB");
        assert_eq!(texts_job("T", "", "", false).text, "T");
        assert_eq!(texts_job("T", "A", "", false).text, format!("T{ROW_SEP}A"));
        assert_eq!(texts_job("T", "", "B", false).text, format!("T{ROW_SEP}B"));
        assert_eq!(texts_hover("T", "A", "B"), "T — A\nB");
    }

    #[test]
    fn album_cards_wrap_into_rows_in_the_grid() {
        let ctx = egui::Context::default();
        theme::apply(&ctx, true);
        let mut row_bottoms: Vec<i32> = Vec::new();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 900.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    album_grid(ui, |ui| {
                        for index in 0..5 {
                            let album = api::Album {
                                id: format!("a{index}"),
                                name: format!("Album {index}"),
                                artist: "Artist".into(),
                                ..Default::default()
                            };
                            let _ = album_card(ui, &album, None, true);
                            row_bottoms.push((ui.min_rect().bottom() / 10.0).round() as i32);
                        }
                    });
                });
            },
        );
        row_bottoms.sort_unstable();
        row_bottoms.dedup();
        assert_eq!(row_bottoms.len(), 2, "cards did not wrap into rows: {row_bottoms:?}");
    }

    #[test]
    fn play_count_word_follows_russian_plurals() {
        for (count, expected) in [
            (1, "1 раз"),
            (2, "2 раза"),
            (4, "4 раза"),
            (5, "5 раз"),
            (11, "11 раз"),
            (12, "12 раз"),
            (21, "21 раз"),
            (22, "22 раза"),
            (25, "25 раз"),
            (101, "101 раз"),
        ] {
            assert_eq!(play_count_label(count), expected);
        }
    }

    #[test]
    fn local_files_form_albums_artists_and_search_hits() {
        let a1 = local_entry("a1", "One", "Band", "Album A");
        let a2 = local_entry("a2", "Two", "Band", "Album A");
        let b1 = local_entry("b1", "Solo", "Soloist", "Album B");
        let untagged = DiskEntry::Local(LocalTrack {
            id: "local:u.mp3".into(),
            path: std::path::PathBuf::from("u.mp3"),
            rel: "u.mp3".into(),
            title: "U".into(),
            artist: String::new(),
            album: String::new(),
            duration: 1.0,
            suffix: "mp3".into(),
            size: 1,
        });
        let entries = vec![a1, a2, b1, untagged];

        let albums = build_local_albums(&entries);
        assert_eq!(albums.len(), 3);
        let band = albums.iter().find(|album| album.artist == "Band").unwrap();
        assert_eq!(band.name, "Album A");
        assert_eq!(band.songs.iter().map(|song| song.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert!(band.id.starts_with(LOCAL_ALBUM_PREFIX));
        assert!(band.to_api_album().duration > 0);
        let unknown = albums.iter().find(|album| album.name == "без альбома").unwrap();
        assert_eq!(unknown.artist, "неизвестный артист");

        let artists = build_local_artists(&albums);
        assert_eq!(
            artists.iter().map(|artist| artist.name.as_str()).collect::<Vec<_>>(),
            ["Band", "Soloist", "неизвестный артист"]
        );
        assert!(artists[0].id.starts_with(LOCAL_ARTIST_PREFIX));
        assert_eq!(artists[0].album_count, 1);

        // Server-downloaded entries stay out of the local albums, but the
        // search can include them when there is no server to search instead.
        let all = [
            entries.clone(),
            vec![DiskEntry::Cached(CachedTrack {
                id: "s1".into(),
                path: "s1.mp3".into(),
                title: "Server Song".into(),
                artist: "Cloud".into(),
                album: "Remote".into(),
                duration: 1.0,
                suffix: "mp3".into(),
                size: 1,
                format: "raw".into(),
            })],
        ]
        .concat();
        assert_eq!(build_local_albums(&all).len(), 3);
        assert_eq!(search_local_matches(&entries, "album a", false, 10).len(), 2);
        assert_eq!(search_local_matches(&entries, "solo", false, 10).len(), 1);
        assert_eq!(search_local_matches(&entries, "", false, 10).len(), 0);
        assert_eq!(search_local_matches(&all, "server song", false, 10).len(), 0);
        assert_eq!(search_local_matches(&all, "server song", true, 10).len(), 1);
        assert_eq!(search_local_matches(&all, "a", true, 2).len(), 2, "limit not applied");
    }

    #[test]
    fn library_rows_merge_server_and_disk_without_duplicates() {
        let local = local_entry("l1", "Local", "L", "LA");
        let cached = DiskEntry::Cached(CachedTrack {
            id: "s1".into(),
            path: "s1.mp3".into(),
            title: "Cached".into(),
            artist: "C".into(),
            album: "CA".into(),
            duration: 1.0,
            suffix: "mp3".into(),
            size: 2,
            format: "raw".into(),
        });
        let server = vec![
            api::Song {
                id: "s1".into(),
                title: "Cached".into(),
                artist: "C".into(),
                album: "CA".into(),
                ..Default::default()
            },
            api::Song {
                id: "s2".into(),
                title: "Stream".into(),
                artist: "B".into(),
                album: "BA".into(),
                duration: 5.0,
                ..Default::default()
            },
        ];
        let rows = build_library_rows(&[local, cached], &server);
        assert_eq!(rows.len(), 3, "a cached server song must not be duplicated");
        assert_eq!(rows.iter().map(LibRow::id).collect::<Vec<_>>(), ["s2", "s1", "local:l1.mp3"]);
        let streamed = rows.iter().find(|row| row.id() == "s2").unwrap();
        assert!(streamed.is_server() && !streamed.is_local() && !streamed.is_cached());
        assert!(streamed.size().is_none(), "server rows show duration, not size");
        assert_eq!(streamed.duration(), 5.0);
        assert_eq!(rows.iter().find(|row| row.id() == "s1").unwrap().size(), Some(2));
    }

    #[test]
    fn the_play_button_is_not_a_pause_button_for_a_restored_track() {
        // Restored session: a track exists, but nothing is loaded yet.
        assert_eq!(transport_play_label(true, false, false), ("▶", "продолжить"));
        assert_eq!(transport_play_label(false, false, false), ("▶", "начать воспроизведение"));
        // Really playing and really paused.
        assert_eq!(transport_play_label(true, true, false), ("▮▮", "пауза"));
        assert_eq!(transport_play_label(true, true, true), ("▶", "продолжить"));
    }

    #[test]
    fn a_song_found_by_the_filter_keeps_its_place_in_the_library() {
        let rows = vec![
            LibRow::Server(api::Song { id: "a".into(), ..Default::default() }),
            LibRow::Server(api::Song { id: "b".into(), ..Default::default() }),
            LibRow::Server(api::Song { id: "c".into(), ..Default::default() }),
        ];
        assert_eq!(library_queue_start(&rows, "b"), Some(1), "next must continue in the library");
        assert_eq!(library_queue_start(&rows, "unknown"), None);
    }

    #[test]
    fn play_from_idle_uses_the_visible_library_list() {
        let rows = Arc::new(vec![LibRow::Server(api::Song { id: "a".into(), ..Default::default() })]);
        let filtered = Arc::new(vec![LibRow::Server(api::Song { id: "b".into(), ..Default::default() })]);
        assert!(Arc::ptr_eq(idle_start_rows(View::Library, &rows, &filtered), &filtered));
        let empty = Arc::new(Vec::new());
        assert!(
            Arc::ptr_eq(idle_start_rows(View::Library, &rows, &empty), &rows),
            "an empty filter must fall back to the full library"
        );
        assert!(Arc::ptr_eq(idle_start_rows(View::Albums, &rows, &filtered), &rows));
    }

    #[test]
    fn most_played_orders_by_count_and_honours_the_limit() {
        let dir = std::env::temp_dir().join(format!(
            "beat-top-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stats = stats::Stats::load_from(dir.join("play-stats.json"));
        let a = local_entry("a", "Alpha", "X", "");
        let b = local_entry("b", "Bravo", "X", "");
        let c = local_entry("c", "Charlie", "X", "");
        stats.increment("local:b.mp3");
        stats.increment("local:b.mp3");
        stats.increment("local:a.mp3");
        let list = top_played(&[a.clone(), b.clone(), c.clone()], &stats, 10);
        assert_eq!(list.iter().map(|(entry, _)| entry.id()).collect::<Vec<_>>(), ["local:b.mp3", "local:a.mp3"]);
        assert_eq!(list[0].1, 2);
        assert_eq!(top_played(&[a, b, c], &stats, 1).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_entry_roundtrips_into_a_song() {
        let entry = CachedTrack {
            id: "7".into(),
            title: "T".into(),
            artist: "A".into(),
            album: "B".into(),
            duration: 12.0,
            suffix: "flac".into(),
            path: "x".into(),
            size: 1,
            format: "raw".into(),
        };
        let song = cached_to_song(&entry);
        assert_eq!(song.id, "7");
        assert_eq!(song.suffix, "flac");
        assert_eq!(song.artist, "A");
    }
}
