use super::*;

impl BeatApp {
    pub(super) fn ui_search(&mut self, ui: &mut egui::Ui) {
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
                self.request(Command::Search);
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
            self.request(Command::OpenArtist(artist));
        }
        if let Some(album) = open_album {
            self.request(Command::BrowseAlbum(album));
        }
        if let Some(album) = play_album {
            self.request(Command::PlayAlbum(album));
        }
        if let Some(album) = download_album {
            self.request(Command::DownloadAlbum(album));
        }
        if let Some(song) = play_song {
            self.request(Command::PlayFromLibrary(song));
        }
        if let Some(song) = download {
            self.request(Command::DownloadSong(song));
        }
        if let Some(index) = play_local {
            self.request(Command::PlayFromLibrary(local[index].to_song()));
        }
    }
}
