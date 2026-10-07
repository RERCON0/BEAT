//! User actions are collected during drawing and applied after the frame.
use super::*;

pub(super) enum Command {
    OpenSettings,
    SaveSettings,
    TestConnection(Box<Config>),
    SaveConfig,
    ToggleLanguage,
    ToggleTheme,
    Volume(f32),
    Albums { kind: String, title: String },
    Artists,
    OpenArtist(api::Artist),
    BrowseAlbum(api::Album),
    PlayAlbum(api::Album),
    DownloadAlbum(api::Album),
    DownloadSong(api::Song),
    DownloadSongs(Vec<api::Song>),
    PlayFromLibrary(api::Song),
    Play { song: api::Song, queue: Vec<api::Song>, index: usize },
    Search,
    RefreshDisk,
    RefreshServer(bool),
    OpenCacheFolder,
    PickCacheFolder,
    AddLibraryFolder,
    FlushStats,
    Catalog(catalog::Mode),
    CancelCatalog,
    ClearDownloadQueue,
    ClearCache,
    RemoveCached(String),
    Previous,
    Next,
    Stop,
    TogglePlay,
    ToggleShuffle,
    CycleRepeat,
    Seek { target: f64, duration: f64 },
    Queue(QueueAction),
}

impl BeatApp {
    pub(super) fn request(&mut self, command: Command) {
        self.commands.push(command);
    }
    pub(super) fn dispatch_commands(&mut self, ctx: &egui::Context) {
        let commands = std::mem::take(&mut self.commands);
        // Preview windows and discarded layout passes cannot execute side effects.
        if self.demo.is_some() || ctx.will_discard() {
            return;
        }
        if !commands.is_empty() {
            ctx.request_repaint();
        }
        for command in commands {
            match command {
                Command::OpenSettings => self.open_settings(),
                Command::SaveSettings => {
                    let old_profile = profile_key(&self.cfg);
                    self.save_settings();
                    // Remaining clicks were drawn for the previous account/cache.
                    if profile_key(&self.cfg) != old_profile {
                        break;
                    }
                }
                Command::TestConnection(config) => self.check_connection(&config),
                Command::SaveConfig => self.save(),
                Command::ToggleLanguage => {
                    self.cfg.language = self.cfg.language.toggle();
                    self.settings_draft.language = self.cfg.language;
                    i18n::set(self.cfg.language);
                    self.album_list_title = i18n::tr(&self.album_list_title).to_owned();
                    self.save();
                }
                Command::ToggleTheme => self.toggle_theme(ctx),
                Command::Volume(volume) => {
                    if volume.is_finite() {
                        if let Some(player) = &mut self.player {
                            player.set_volume(volume.clamp(0.0, 1.0));
                        }
                        self.cfg.volume = volume.clamp(0.0, 1.0);
                    }
                }
                Command::Albums { kind, title } => self.refresh_albums(&kind, &title),
                Command::Artists => self.fetch_artists(),
                Command::OpenArtist(artist) => self.open_artist(artist),
                Command::BrowseAlbum(album) => self.browse_album(album),
                Command::PlayAlbum(album) => self.play_album(album),
                Command::DownloadAlbum(album) => self.download_album(album),
                Command::DownloadSong(song) => self.enqueue_download(song),
                Command::DownloadSongs(songs) => self.enqueue_album(&songs),
                Command::PlayFromLibrary(song) => self.play_from_library(song),
                Command::Play { song, queue, index } => self.play_song(song, queue, index),
                Command::Search => self.run_search(),
                Command::RefreshDisk => self.refresh_disk(),
                Command::RefreshServer(force) => self.refresh_server_library(force),
                Command::OpenCacheFolder => self.open_cache_folder(),
                Command::PickCacheFolder => {
                    if let Some(dir) = pick_folder() {
                        self.settings_draft.cache_dir = dir;
                    }
                }
                Command::AddLibraryFolder => {
                    if self.settings_draft.library_dirs.len() < 16 {
                        if let Some(dir) = pick_folder() {
                            self.settings_draft.library_dirs.push(dir);
                        }
                    }
                }
                Command::FlushStats => {
                    self.stats.flush();
                }
                Command::Catalog(mode) => self.start_catalog_scan(mode),
                Command::CancelCatalog => {
                    self.cancel_catalog_scan();
                    self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    self.notice =
                        Some(crate::i18n::tr("поиск остановлен; уже добавленные загрузки продолжаются").into());
                }
                Command::ClearDownloadQueue => {
                    if self.catalog_scan.is_some() {
                        self.cancel_catalog_scan();
                        self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                    }
                    self.download_queue.clear();
                    self.auto_queued.clear();
                    self.notice =
                        Some(crate::i18n::tr("поиск и ожидающие загрузки отменены; начатые продолжаются").into());
                }
                Command::ClearCache => self.clear_cache(),
                Command::RemoveCached(id) => self.remove_cached(&id),
                Command::Previous => self.prev_track(),
                Command::Next => self.next_track(),
                Command::Stop => self.stop_playback(),
                Command::TogglePlay => self.toggle_play(),
                Command::ToggleShuffle => {
                    self.shuffle = !self.shuffle;
                    self.invalidate_prefetch();
                    self.refresh_shuffle();
                    self.save_session();
                }
                Command::CycleRepeat => {
                    self.invalidate_prefetch();
                    self.repeat = match self.repeat {
                        Repeat::Off => Repeat::All,
                        Repeat::All => Repeat::One,
                        Repeat::One => Repeat::Off,
                    };
                    self.save_session();
                }
                Command::Seek { target, duration } => self.seek_to(target, duration),
                Command::Queue(action) => self.edit_queue(action),
            }
        }
    }
    fn clear_cache(&mut self) {
        // Recheck at execution: an earlier command may have started a download.
        if !self.downloads.is_empty() || !self.download_queue.is_empty() {
            return;
        }
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
    fn remove_cached(&mut self, id: &str) {
        match self.cache.remove(id) {
            Ok(()) => {
                let kept: Vec<DiskEntry> = self.disk_entries.iter().filter(|entry| entry.id() != id).cloned().collect();
                self.set_disk_entries(kept);
                self.notice = Some(crate::i18n::tr("удалено из кеша").into());
            }
            Err(err) => self.notice = Some(err),
        }
    }
}
