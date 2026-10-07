use super::*;

impl BeatApp {
    /// The main screen: every song in one list — server library, cached
    /// downloads and hand-dropped files, with a filter and per-row actions.
    pub(super) fn ui_library(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title(crate::i18n::tr("[ БИБЛИОТЕКА ]")));
            if ui.button(crate::i18n::tr("обновить")).clicked() {
                self.request(Command::RefreshDisk);
                self.request(Command::RefreshServer(true));
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
                self.request(Command::OpenCacheFolder);
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
                    self.request(Command::CancelCatalog);
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
                        self.request(Command::OpenCacheFolder);
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
            self.request(Command::PlayFromLibrary(song));
        }
        if let Some(id) = remove {
            self.request(Command::RemoveCached(id));
        }
        if let Some(song) = download {
            self.request(Command::DownloadSong(song));
        }
    }
    /// Top tracks by locally counted listens, so «most played» also works for
    /// hand-dropped files without any server.
    pub(super) fn ui_frequent(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::window_title(crate::i18n::tr("[ ЧАСТО ПРОСЛУШИВАЕМЫЕ ]")));
            if ui.button(crate::i18n::tr("обновить")).clicked() {
                self.request(Command::RefreshDisk);
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
            self.request(Command::Play { song, queue: play_queue, index });
        }
    }
}
