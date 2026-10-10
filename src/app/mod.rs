//! Application state; controllers and rendering live in focused child modules.
mod audio_state;
mod browse;
mod catalog_controller;
mod commands;
mod covers_controller;
pub(crate) mod demo;
mod downloads;
mod events;
use events::{CoverEvent, LibEvent, PreparedEvent};
mod frame;
use commands::Command;
mod init;
mod local_library;
mod models;
mod output;
mod persistence;
mod playback_controller;
mod prefetch;
mod settings;
#[cfg(test)]
mod tests;
mod transport;
mod ui_browse;
mod ui_chrome;
mod ui_common;
mod ui_library;
mod ui_player;
mod ui_queue;
mod ui_search;
mod ui_settings;
mod ui_sidebar;

use crate::{
    api, banner, cache, catalog, config, i18n, local, media_keys, opus, platform, player, session, stats, theme,
    HANDLED_PANIC,
};
use cache::{Cache, CachedTrack, DlEvent, DlHandle};
use config::{Config, StreamFormat};
#[cfg(test)]
use covers_controller::decode_cover_sized;
use eframe::egui;
use local::LocalTrack;
use models::*;
use platform::*;
use session::Repeat;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use ui_common::*;
// The layout tests render the settings footer directly.
#[cfg(test)]
use ui_settings::settings_footer;

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

pub(crate) struct BeatApp {
    commands: Vec<Command>,
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

/// Outer width of one album card; fixed so the grid packs whole rows.
const ALBUM_CARD_WIDTH: f32 = 184.0;

const ALBUM_CARD_HEIGHT: f32 = 280.0;

const LOCAL_ALBUM_PREFIX: &str = "local-album:";

const LOCAL_ARTIST_PREFIX: &str = "local-artist:";

/// Dim separator between the title, artist and album of a list row.
const ROW_SEP: &str = "  ·  ";
