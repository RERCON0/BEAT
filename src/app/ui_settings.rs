use super::*;

/// What the settings footer asks the caller to do.
pub(super) enum FooterAction {
    Check,
    Save,
    Cancel,
}

/// The settings messages and the button row under them.
///
/// A server error is a full sentence and can be long, so it gets its own
/// wrapped row. Inside the button row it ran under the buttons drawn after it.
pub(super) fn settings_footer(
    ui: &mut egui::Ui,
    checking: bool,
    check_result: Option<&Result<(), String>>,
    save_error: Option<&str>,
) -> Option<FooterAction> {
    ui.add_space(8.0);
    if let Some(result) = check_result {
        let (text, color) = match result {
            Ok(()) => (crate::i18n::tr("связь есть").into(), theme::accent()),
            Err(err) => (err.clone(), theme::err()),
        };
        ui.add(egui::Label::new(egui::RichText::new(text).size(11.0).color(color)).wrap());
    }
    if let Some(err) = save_error {
        ui.add(
            egui::Label::new(
                egui::RichText::new(crate::i18n::trf!("конфиг не сохранён: {err}", err = err))
                    .size(10.0)
                    .color(theme::err()),
            )
            .wrap(),
        );
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(!checking, egui::Button::new(crate::i18n::tr("[ ПРОВЕРИТЬ СВЯЗЬ ]"))).clicked()
        {
            return Some(FooterAction::Check);
        }
        if checking {
            ui.label(egui::RichText::new(crate::i18n::tr("проверяю…")).size(11.0).color(theme::faint()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(theme::accent_button(crate::i18n::tr("[ СОХРАНИТЬ ]"))).clicked() {
                return Some(FooterAction::Save);
            }
            if ui.add(egui::Button::new(crate::i18n::tr("[ ОТМЕНА ]"))).clicked() {
                return Some(FooterAction::Cancel);
            }
            None
        })
        .inner
    })
    .inner
}

impl BeatApp {
    pub(super) fn ui_settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut save = false;
        let mut cancel = false;
        let mut check = false;
        let mut pick_dir = false;
        let mut add_library_dir = false;
        let mut remove_library_dir = None;
        let save_error = self.save_error.clone();
        let checking = self.settings_checking;
        let check_result = self.settings_check.clone();
        let mut show_password = self.show_password;
        {
            let draft = &mut self.settings_draft;
            egui::Window::new(theme::window_title(crate::i18n::tr("[ НАСТРОЙКИ ]")))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .max_height((ctx.viewport_rect().height() - 60.0).max(300.0))
                .vscroll(true)
                .show(ctx, |ui| {
                    ui.set_width(620.0);
                    field_label(ui, crate::i18n::tr("> адрес Navidrome (https://…)"));
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.server_url).font(theme::field_font())
                            .hint_text("https://music.example.com"));
                    field_label(ui, crate::i18n::tr("> логин"));
                    ui.add_sized([ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut draft.user).font(theme::field_font()));
                    field_label(ui, crate::i18n::tr("> пароль"));
                    ui.horizontal(|ui| {
                        let field = ui.add_sized([420.0, 30.0],
                            egui::TextEdit::singleline(&mut draft.password).font(theme::field_font())
                                .password(!show_password));
                        if field.changed() {
                            draft.forget_unreadable_password();
                        }
                        if ui.add(egui::Button::new(if show_password { crate::i18n::tr("[ скрыть ]") } else { crate::i18n::tr("[ показать ]") })).clicked() {
                            show_password = !show_password;
                        }
                    });
                    field_label(ui, crate::i18n::tr("> папка кеша"));
                    ui.horizontal(|ui| {
                        let dir = draft.cache_dir.clone();
                        let hint = if dir.trim().is_empty() {
                            crate::i18n::trf!("по умолчанию: {}", Config::default().cache_root().to_string_lossy())
                        } else { dir };
                        ui.add_sized([420.0, 30.0], egui::Label::new(
                            egui::RichText::new(hint).size(11.0).color(theme::dim())));
                        if ui.button(crate::i18n::tr("[ выбрать ]")).clicked() { pick_dir = true; }
                        if ui.button(crate::i18n::tr("[ сбросить ]")).clicked() { draft.cache_dir.clear(); }
                    });
                    field_label(ui, crate::i18n::tr("> музыкальные папки"));
                    egui::ScrollArea::vertical().id_salt("music-roots").max_height(110.0).show(ui, |ui| {
                        for (index, dir) in draft.library_dirs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_sized([530.0, 24.0], egui::Label::new(dir).truncate()).on_hover_text(dir);
                                if ui.button("×").on_hover_text(crate::i18n::tr("Убрать папку из библиотеки")).clicked() { remove_library_dir = Some(index); }
                            });
                        }
                    });
                    if ui.add_enabled(draft.library_dirs.len() < 16, egui::Button::new(crate::i18n::tr("[ ДОБАВИТЬ ПАПКУ ]"))).clicked() { add_library_dir = true; }
                    ui.label(egui::RichText::new(crate::i18n::tr("Эти папки читаются напрямую; очистка кеша их не затрагивает.")).size(10.0).color(theme::dim()));
                    field_label(ui, crate::i18n::tr("> формат загрузки"));
                    egui::ComboBox::from_id_salt("format")
                        .selected_text(egui::RichText::new(draft.stream_format.label()).size(12.0))
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Raw, crate::i18n::tr("оригинал"));
                            ui.selectable_value(&mut draft.stream_format, StreamFormat::Mp3, crate::i18n::tr("mp3 (транскод сервером)"));
                        });
                    if draft.stream_format == StreamFormat::Mp3 {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(crate::i18n::tr("битрейт, kbps")).size(11.0).color(theme::dim()));
                            ui.add(egui::DragValue::new(&mut draft.bit_rate).speed(16.0).range(64..=320));
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(crate::i18n::tr("параллельных загрузок")).size(11.0).color(theme::dim()));
                        ui.add(egui::DragValue::new(&mut draft.parallel_downloads).speed(1.0).range(1..=3));
                    });
                    ui.add_space(6.0);
                    ui.checkbox(&mut draft.auto_cache_new, crate::i18n::tr("автоматически кешировать новые песни"));
                    ui.label(egui::RichText::new(crate::i18n::tr("При первом включении запоминает текущие треки; затем проверяет сервер каждые 10 минут, пока BEAT открыт."))
                        .size(10.0).color(theme::dim()));
                    match settings_footer(ui, checking, check_result.as_ref(), save_error.as_deref()) {
                        Some(FooterAction::Check) => check = true,
                        Some(FooterAction::Save) => save = true,
                        Some(FooterAction::Cancel) => cancel = true,
                        None => {}
                    }
                });
        }
        if pick_dir {
            self.request(Command::PickCacheFolder);
        }
        if add_library_dir {
            self.request(Command::AddLibraryFolder);
        }
        if let Some(index) = remove_library_dir {
            self.settings_draft.library_dirs.remove(index);
        }

        self.show_password = show_password;
        if check {
            let draft = self.settings_draft.clone();
            self.request(Command::TestConnection(Box::new(draft)));
        }
        if save {
            self.request(Command::SaveSettings);
        } else if cancel {
            self.settings_open = false;
        }
    }
}
