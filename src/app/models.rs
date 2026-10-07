//! Pure library, queue and playback calculations.
use super::*;

/// Playback waiting for its first decoder attempt: the initial buffer size, no
/// attempt made yet.
pub(super) fn new_buffering(handle: DlHandle) -> PlayState {
    PlayState::Buffering { handle, needed: cache::PLAYBACK_BUFFER_BYTES, attempted_at: 0 }
}

/// Whether building the decoder again is worth it.
///
/// `needed` bytes must have arrived, and at least `needed` *more* since the last
/// attempt. Without the second condition the requirement grows past the file
/// size and every following frame would rebuild the decoder on the UI thread
/// until the download ends. A finished download always gets a last attempt.
pub(super) fn buffering_attempt_ready(downloaded: u64, needed: u64, attempted_at: u64, finished: bool) -> bool {
    finished || (downloaded >= needed && downloaded >= attempted_at.saturating_add(needed))
}

/// Repeat-one restarts the track, but only one that actually played: a valid
/// header over zero samples never advances, and restarting it on every tick
/// would spin the UI thread forever.
pub(super) fn should_repeat_one(repeat: Repeat, played_secs: f64) -> bool {
    repeat == Repeat::One && played_secs > 0.0
}

pub(super) fn move_queue_item(queue: &mut Vec<api::Song>, current: &mut usize, from: usize, to: usize) {
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

pub(super) fn remove_queue_item(queue: &mut Vec<api::Song>, current: &mut usize, index: usize) {
    if index >= queue.len() {
        return;
    }
    queue.remove(index);
    if index < *current {
        *current -= 1;
    }
    *current = (*current).min(queue.len().saturating_sub(1));
}

/// One row of the on-disk list: a downloaded cache entry or a hand-dropped
/// local file found in the cache folder.
#[derive(Clone)]
pub(super) enum DiskEntry {
    Cached(CachedTrack),
    Local(LocalTrack),
}

impl DiskEntry {
    pub(super) fn id(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.id,
            Self::Local(track) => &track.id,
        }
    }
    pub(super) fn title(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.title,
            Self::Local(track) => &track.title,
        }
    }
    pub(super) fn artist(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.artist,
            Self::Local(track) => &track.artist,
        }
    }
    pub(super) fn album(&self) -> &str {
        match self {
            Self::Cached(entry) => &entry.album,
            Self::Local(track) => &track.album,
        }
    }
    pub(super) fn size(&self) -> u64 {
        match self {
            Self::Cached(entry) => entry.size,
            Self::Local(track) => track.size,
        }
    }
    pub(super) fn duration(&self) -> f64 {
        match self {
            Self::Cached(entry) => entry.duration,
            Self::Local(track) => track.duration,
        }
    }
    pub(super) fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
    pub(super) fn local_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Cached(_) => None,
            Self::Local(track) => Some(&track.path),
        }
    }
    /// Cover key for the artwork embedded in this row's file on disk.
    pub(super) fn cover_key(&self) -> String {
        let rel = match self {
            Self::Cached(entry) => &entry.path,
            Self::Local(track) => track.id.strip_prefix(local::LOCAL_ID_PREFIX).unwrap_or(&track.rel),
        };
        format!("{FILE_COVER_PREFIX}{rel}")
    }
    pub(super) fn to_song(&self) -> api::Song {
        match self {
            Self::Cached(entry) => cached_to_song(entry),
            Self::Local(track) => track.to_song(),
        }
    }
}

/// One row of the unified library: a file on disk or a server-only song.
#[derive(Clone)]
pub(super) enum LibRow {
    Disk(DiskEntry),
    Server(api::Song),
}

impl LibRow {
    pub(super) fn id(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.id(),
            Self::Server(song) => &song.id,
        }
    }
    pub(super) fn title(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.title(),
            Self::Server(song) => &song.title,
        }
    }
    pub(super) fn artist(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.artist(),
            Self::Server(song) => &song.artist,
        }
    }
    pub(super) fn album(&self) -> &str {
        match self {
            Self::Disk(entry) => entry.album(),
            Self::Server(song) => &song.album,
        }
    }
    pub(super) fn duration(&self) -> f64 {
        match self {
            Self::Disk(entry) => entry.duration(),
            Self::Server(song) => song.duration,
        }
    }
    /// Human size of the file; server-only songs have none to show.
    pub(super) fn size(&self) -> Option<u64> {
        match self {
            Self::Disk(entry) => Some(entry.size()),
            Self::Server(_) => None,
        }
    }
    pub(super) fn is_local(&self) -> bool {
        matches!(self, Self::Disk(entry) if entry.is_local())
    }
    pub(super) fn is_cached(&self) -> bool {
        matches!(self, Self::Disk(entry) if !entry.is_local())
    }
    pub(super) fn is_server(&self) -> bool {
        matches!(self, Self::Server(_))
    }
    /// Cover key: `<rel>`-based file path for disk rows, cover id for server.
    pub(super) fn cover_key(&self) -> String {
        match self {
            Self::Disk(entry) => entry.cover_key(),
            Self::Server(song) => song.cover_id.clone(),
        }
    }
    pub(super) fn local_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Disk(entry) => entry.local_path(),
            Self::Server(_) => None,
        }
    }
    pub(super) fn to_song(&self) -> api::Song {
        match self {
            Self::Disk(entry) => entry.to_song(),
            Self::Server(song) => song.clone(),
        }
    }
}

/// Merged library: every disk file plus server songs that are not cached
/// yet, sorted artist/album/title. Cached ids are skipped so a downloaded
/// song does not appear twice.
pub(super) fn build_library_rows(disk: &[DiskEntry], server: &[api::Song]) -> Vec<LibRow> {
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
pub(super) fn library_queue_start(rows: &[LibRow], song_id: &str) -> Option<usize> {
    rows.iter().position(|row| row.id() == song_id)
}

/// What a bare «play» press queues when nothing is playing: the visible
/// (filtered) library list while the library screen is open, else all rows.
pub(super) fn idle_start_rows<'a>(
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
pub(super) struct LocalAlbum {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) artist: String,
    /// `file:<rel>` cover key of one of the tracks (may resolve to none).
    pub(super) cover_id: String,
    pub(super) songs: Vec<api::Song>,
    pub(super) duration: f64,
}

impl LocalAlbum {
    pub(super) fn to_api_album(&self) -> api::Album {
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

pub(super) fn local_album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1}{album}")
}

/// Groups hand-dropped files into albums (artist + album tag, with folder
/// fallbacks already applied by the scanner) and stable ids.
pub(super) fn build_local_albums(entries: &[DiskEntry]) -> Vec<LocalAlbum> {
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
pub(super) fn build_local_artists(albums: &[LocalAlbum]) -> Vec<api::Artist> {
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
pub(super) fn search_local_matches(
    entries: &[DiskEntry],
    needle: &str,
    include_cached: bool,
    limit: usize,
) -> Vec<DiskEntry> {
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

/// Top tracks by locally counted listens, title as the tie-break. Counts are
/// the only source: a track never played in BEAT has nothing to show yet.
pub(super) fn top_played(entries: &[DiskEntry], stats: &stats::Stats, limit: usize) -> Vec<(DiskEntry, u64)> {
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
pub(super) fn shuffle_albums(albums: &mut Vec<api::Album>) {
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
pub(super) fn shuffled_order(len: usize, current: usize) -> (Vec<usize>, usize) {
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
pub(super) fn merge_disk_entries(indexed: Vec<CachedTrack>, local: Vec<LocalTrack>) -> Vec<DiskEntry> {
    let mut entries: Vec<DiskEntry> =
        indexed.into_iter().map(DiskEntry::Cached).chain(local.into_iter().map(DiskEntry::Local)).collect();
    // One lowercase per row instead of one per comparison.
    entries.sort_by_cached_key(|entry| {
        (entry.artist().to_lowercase(), entry.album().to_lowercase(), entry.title().to_lowercase())
    });
    entries
}

pub(super) fn cached_to_song(entry: &CachedTrack) -> api::Song {
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

pub(super) fn profile_key(cfg: &Config) -> String {
    let server = api::Server::from_config(cfg).catalog_key().unwrap_or_default();
    api::md5_hex(&format!("{server}\0{}", cfg.cache_root().to_string_lossy()))
}

/// Seeking changes source position instantly; it must not manufacture minutes
/// of listening. Paused time and backward seeks add nothing to the counter.
pub(super) fn listen_advance(previous: f64, position: f64, elapsed: f64, paused: bool) -> f64 {
    if paused || !previous.is_finite() || !position.is_finite() || !elapsed.is_finite() {
        return 0.0;
    }
    (position - previous).max(0.0).min(elapsed.max(0.0))
}

/// How soon the UI must run again without any input. Workers, downloads and
/// the player change state on their own, and eframe redraws only on input or
/// a repaint request: without this a finished track never advances and the
/// clock stands still until the mouse moves.
pub(super) fn repaint_after(playing: bool, transfers: usize, requests: usize) -> Option<std::time::Duration> {
    (playing || transfers > 0 || requests > 0).then_some(UI_TICK)
}

/// Whether the on-disk list should be rescanned now. Every finished download
/// marks it stale, but a rescan reads the whole folder: during an album
/// download it runs at most every few seconds instead of once per track.
pub(super) fn disk_refresh_due(
    stale: bool,
    scanning: bool,
    downloading: bool,
    since_last: std::time::Duration,
) -> bool {
    stale && !scanning && (!downloading || since_last >= DISK_REFRESH_EVERY)
}

/// A position picked on the seek slider is sent once the pointer is released,
/// not on every frame of the drag (each seek resets the decoder and, in a
/// download in progress, may wait for the network).
pub(super) fn due_seek(pending: Option<f64>, pointer_down: bool) -> Option<f64> {
    if pointer_down {
        None
    } else {
        pending
    }
}
