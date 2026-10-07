//! Typed worker events and state updates; no UI drawing.
use super::*;

pub(super) enum LibEvent {
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

pub(super) type PreparedEvent = (u64, usize, Result<opus::AudioSource, String>);

pub(super) enum CoverEvent {
    Loaded(u64, String, Option<egui::ColorImage>),
}

impl BeatApp {
    pub(super) fn pump_library(&mut self) {
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
    pub(super) fn accept(&mut self, id: u64) -> bool {
        if self.loading == Some(id) {
            self.loading = None;
            true
        } else {
            false
        }
    }
    pub(super) fn pump_covers(&mut self, ctx: &egui::Context) {
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
    pub(super) fn pump_downloads(&mut self) {
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
}
