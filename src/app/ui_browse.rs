use super::*;

impl BeatApp {
    pub(super) fn ui_central(&mut self, ui: &mut egui::Ui) {
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
    pub(super) fn ui_artists(&mut self, ui: &mut egui::Ui) {
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
            self.request(Command::OpenArtist(artist));
        }
    }
    pub(super) fn ui_artist_albums(&mut self, ui: &mut egui::Ui) {
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
    pub(super) fn ui_album_grid(&mut self, ui: &mut egui::Ui, title: &str, albums: Option<Arc<Vec<api::Album>>>) {
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
                    self.request(Command::OpenSettings);
                }
                if ui.button(crate::i18n::tr("[ БИБЛИОТЕКА ]")).clicked() {
                    self.view = View::Library;
                    self.request(Command::RefreshDisk);
                }
                if ui.button(crate::i18n::tr("[ ОТКРЫТЬ ПАПКУ КЕША ]")).clicked() {
                    self.request(Command::OpenCacheFolder);
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
            self.request(Command::BrowseAlbum(album));
        }
        if let Some(album) = play {
            self.request(Command::PlayAlbum(album));
        }
        if let Some(album) = download {
            self.request(Command::DownloadAlbum(album));
        }
    }
    /// Cover lookup that does not borrow the whole app inside UI closures.
    pub(super) fn cover_texture_of(&mut self, cover_id: &str, px: u32) -> Option<egui::TextureHandle> {
        self.cover_texture(cover_id, px)
    }
    pub(super) fn ui_album(&mut self, ui: &mut egui::Ui) {
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
                    self.request(Command::Play { song: song.clone(), queue: songs.as_ref().clone(), index: 0 });
                }
            }
            if !album.id.starts_with(LOCAL_ALBUM_PREFIX) && ui.button(crate::i18n::tr("[ СКАЧАТЬ АЛЬБОМ ]")).clicked()
            {
                self.request(Command::DownloadSongs(songs.to_vec()));
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
                        self.request(Command::Play { song: song.clone(), queue: songs.as_ref().clone(), index });
                    }
                    if !local_song
                        && ui
                            .add_enabled(!cached, egui::Button::new("↓"))
                            .on_hover_text(crate::i18n::tr("скачать в кеш"))
                            .clicked()
                    {
                        self.request(Command::DownloadSong(song.clone()));
                    }
                });
            }
        });
    }
}
