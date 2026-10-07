use super::*;

impl BeatApp {
    pub(super) fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let wordmark = banner::BANNER.trim_matches(['\r', '\n']);
            let lines: Vec<&str> = wordmark.lines().collect();
            let width_at = |ui: &egui::Ui, size: f32| -> f32 {
                lines
                    .iter()
                    .map(|line| {
                        ui.fonts_mut(|f| {
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
                self.request(Command::Albums {
                    kind: ("newest").into(),
                    title: (crate::i18n::tr("НОВЫЕ АЛЬБОМЫ")).into(),
                });
            }
            if ui
                .add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("случайные альбомы")))
                .clicked()
            {
                self.request(Command::Albums {
                    kind: ("random").into(),
                    title: (crate::i18n::tr("СЛУЧАЙНЫЕ АЛЬБОМЫ")).into(),
                });
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("все артисты"))).clicked()
            {
                self.request(Command::Artists);
            }
            if ui.add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("библиотека"))).clicked()
            {
                self.view = View::Library;
                self.request(Command::RefreshDisk);
                self.request(Command::RefreshServer(false));
            }
            if ui
                .add_sized([ui.available_width(), 26.0], egui::Button::new(crate::i18n::tr("часто прослушиваемые")))
                .clicked()
            {
                self.view = View::Frequent;
                self.request(Command::FlushStats);
                self.request(Command::RefreshDisk);
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
                self.request(Command::Catalog(catalog::Mode::All));
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
                    self.request(Command::CancelCatalog);
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
                    self.request(Command::ClearDownloadQueue);
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
                self.request(Command::OpenCacheFolder);
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
                self.request(Command::ClearCache);
            }

            if let Some(notice) = &self.notice {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(notice).size(10.0).color(theme::warn()));
            }
        });
    }
}
