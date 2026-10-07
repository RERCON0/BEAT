use super::*;

impl BeatApp {
    /// Rescans the cache folder on a worker thread: indexed downloads plus
    /// hand-dropped local files (tags and durations are read there, so the UI
    /// thread never touches files).
    pub(super) fn refresh_disk(&mut self) {
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
    pub(super) fn refresh_local_stats(&mut self) {
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
    pub(super) fn open_cache_folder(&mut self) {
        let root = self.cache.root();
        if let Err(err) = std::fs::create_dir_all(root) {
            self.notice = Some(crate::i18n::trf!("не удалось создать {}: {err}", root.display(), err = err));
            return;
        }
        open_in_explorer(&root.to_string_lossy());
    }
    /// Replaces the on-disk list and refreshes everything derived from it:
    /// the merged library rows now, the frequent list on its next visit.
    pub(super) fn set_disk_entries(&mut self, entries: Vec<DiskEntry>) {
        self.disk_entries = Arc::new(entries);
        self.frequent_dirty = true;
        self.rebuild_local_library();
        self.rebuild_library_rows();
    }
    /// Replaces the full server song list (fresh scan or the on-disk cache).
    pub(super) fn set_server_songs(&mut self, songs: Vec<api::Song>) {
        self.server_songs = Arc::new(songs);
        self.rebuild_library_rows();
    }
    pub(super) fn rebuild_library_rows(&mut self) {
        self.library_rows = Arc::new(build_library_rows(&self.disk_entries, &self.server_songs));
        self.rebuild_library_filter();
    }
    pub(super) fn rebuild_library_filter(&mut self) {
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
    pub(super) fn rebuild_local_library(&mut self) {
        let albums = build_local_albums(&self.disk_entries);
        self.local_artists = Arc::new(build_local_artists(&albums));
        self.local_album_cards = Arc::new(albums.iter().map(LocalAlbum::to_api_album).collect());
        self.local_albums = Arc::new(albums);
        self.rebuild_artists_view();
    }
    /// Server artists first, then local ones: both open through `open_artist`.
    pub(super) fn rebuild_artists_view(&mut self) {
        let mut list = self.artists_server.clone();
        list.extend(self.local_artists.iter().cloned());
        self.artists = list;
    }
    pub(super) fn rebuild_frequent(&mut self) {
        self.frequent_entries = Arc::new(top_played(&self.disk_entries, &self.stats, MAX_FREQUENT));
        self.frequent_dirty = false;
    }
}
