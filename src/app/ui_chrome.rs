use super::*;

impl BeatApp {
    pub(super) fn ui_header(&mut self, ui: &mut egui::Ui) {
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
                    self.request(Command::OpenSettings);
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
    pub(super) fn ui_statusbar(&mut self, ui: &mut egui::Ui) {
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
    /// Borderless-window title bar, same as STRIKE/SNATCH.
    #[cfg(windows)]
    pub(super) fn ui_title_bar(&mut self, ui: &mut egui::Ui) {
        const THEME_BTN_WIDTH: f32 = 56.0;
        egui::Panel::top("app_titlebar")
            .exact_size(42.0)
            .show_separator_line(true)
            .frame(egui::Frame::NONE.fill(ui.visuals().window_fill).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| {
                let ctx = ui.ctx().clone();
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
                        self.request(Command::ToggleLanguage);
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
                        self.request(Command::ToggleTheme);
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
