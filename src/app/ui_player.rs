use super::*;

impl BeatApp {
    /// Footer: now playing and seek on the left, transport in the centre of
    /// the whole bar, volume on the right.
    pub(super) fn ui_player_bar(&mut self, ui: &mut egui::Ui) {
        const CONTROL_W: f32 = 40.0;
        const CONTROL_COUNT: usize = 6;
        let bar = ui.available_rect_before_wrap();
        let spacing = ui.spacing().item_spacing.x;
        let controls_w = CONTROL_W * CONTROL_COUNT as f32 + spacing * (CONTROL_COUNT as f32 - 1.0);
        // Centred on the whole bar, not on the space left by the side
        // sections, so it never drifts when the window resizes.
        let center = egui::Rect::from_center_size(bar.center(), egui::vec2(controls_w, bar.height()));
        let left_rect = egui::Rect::from_min_max(bar.min, egui::pos2((center.left() - 16.0).max(bar.min.x), bar.max.y));
        let right_rect =
            egui::Rect::from_min_max(egui::pos2((center.right() + 16.0).min(bar.max.x), bar.min.y), bar.max);

        ui.scope_builder(
            egui::UiBuilder::new().max_rect(left_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| self.ui_now_playing(ui),
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(center).layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| self.ui_transport(ui, CONTROL_W),
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(right_rect).layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| self.ui_volume(ui),
        );
    }
    pub(super) fn ui_now_playing(&mut self, ui: &mut egui::Ui) {
        let has_player = self.player.is_some() || self.demo.is_some();
        // Now-playing cover: taken from the file on disk (local or cached
        // download); streams without a finished file get the empty slot.
        let cover_rel: Option<String> = match &self.current {
            Some(song) => match song.id.strip_prefix(local::LOCAL_ID_PREFIX) {
                Some(rel) => Some(rel.to_owned()),
                None => self.cache.indexed_entry(&song.id).map(|entry| entry.path),
            },
            None => None,
        };
        let cover = match cover_rel {
            Some(rel) => self.cover_texture_of(&format!("{FILE_COVER_PREFIX}{rel}"), ROW_COVER_PX),
            None => None,
        };
        let _ = row_cover(ui, 38.0, cover, egui::Sense::hover());
        ui.add_space(2.0);

        let width = (ui.available_width() - 4.0).max(120.0);
        ui.vertical(|ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 3.0;
            let (title, artist, album) = match &self.current {
                Some(song) => (song.title.clone(), song.artist.clone(), song.album.clone()),
                None => ("—".into(), crate::i18n::tr("ничего не играет").into(), String::new()),
            };
            let name_w = (ui.available_width() - 8.0).max(80.0);
            let name = ui
                .allocate_ui_with_layout(
                    egui::vec2(name_w, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.add(egui::Label::new(texts_job(&title, &artist, &album, false)).truncate()),
                )
                .inner;
            let hover = texts_hover(&title, &artist, &album);
            if !hover.trim().is_empty() {
                name.on_hover_text(hover);
            }

            let duration = self.current.as_ref().map(|song| song.duration).unwrap_or(0.0);
            let position = self.playback_position().min(duration);
            // While the slider is being dragged it shows the picked position.
            let shown = self.pending_seek.unwrap_or(position);
            let mut seek = shown;
            let time = format!("{} / {}", format_time(shown), format_time(duration));
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 92.0).max(80.0);
                let slider = ui.add_enabled(
                    has_player && self.current.is_some(),
                    egui::Slider::new(&mut seek, 0.0..=duration.max(1.0)).show_value(false),
                );
                if slider.changed() {
                    self.pending_seek = Some(seek);
                }
                if let Some(target) = due_seek(self.pending_seek, ui.input(|i| i.pointer.any_down())) {
                    self.pending_seek = None;
                    self.request(Command::Seek { target, duration });
                }
                ui.label(egui::RichText::new(time).size(10.0).color(theme::faint()));
            });
        });
    }
    pub(super) fn ui_transport(&mut self, ui: &mut egui::Ui, button_w: f32) {
        let has_player = self.player.is_some() || self.demo.is_some();
        let paused = self.player.as_ref().map(|p| p.is_paused()).unwrap_or(false);
        // «Loaded» means the track is actually in the output: a restored
        // session has a current song but nothing loaded yet, and the button
        // must read «play», not «pause».
        let loaded = !matches!(self.play_state, PlayState::Idle);
        let size = egui::vec2(button_w, 30.0);
        if ui
            .add_enabled(has_player, egui::Button::new("◀◀").min_size(size))
            .on_hover_text(crate::i18n::tr("предыдущий"))
            .clicked()
        {
            self.request(Command::Previous);
        }
        // Fixed width: the pause glyph is wider than play, and the row
        // must not jump when toggling. ▮ (U+25AE) exists in Cascadia;
        // the old ❚ (U+275A) was not and fell back to replacement boxes.
        let (play_label, play_hover) = transport_play_label(self.current.is_some(), loaded, paused);
        if ui.add_enabled(has_player, egui::Button::new(play_label).min_size(size)).on_hover_text(play_hover).clicked()
        {
            self.request(Command::TogglePlay);
        }
        if ui
            .add_enabled(has_player, egui::Button::new("▶▶").min_size(size))
            .on_hover_text(crate::i18n::tr("следующий"))
            .clicked()
        {
            self.request(Command::Next);
        }
        if ui
            .add_enabled(has_player, egui::Button::new("■").min_size(size))
            .on_hover_text(crate::i18n::tr("стоп"))
            .clicked()
        {
            self.request(Command::Stop);
        }
        if mode_button(ui, "⇄", self.shuffle, crate::i18n::tr("случайный порядок")) {
            self.request(Command::ToggleShuffle);
        }
        let (repeat_label, repeat_hover) = match self.repeat {
            Repeat::Off => ("↻", crate::i18n::tr("повтор выключен — нажмите: весь список")),
            Repeat::All => ("↻", crate::i18n::tr("повтор всего списка — нажмите: одна песня")),
            Repeat::One => ("↻1", crate::i18n::tr("повтор одной песни — нажмите: выключить")),
        };
        if mode_button(ui, repeat_label, self.repeat != Repeat::Off, repeat_hover) {
            self.request(Command::CycleRepeat);
        }
    }
    pub(super) fn ui_volume(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = 110.0;
        let mut volume = self.player.as_ref().map(|p| p.volume()).unwrap_or(self.cfg.volume);
        let volume_response =
            ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false).text(crate::i18n::tr("громкость")));
        if volume_response.changed() {
            self.request(Command::Volume(volume));
        }
        if volume_response.drag_stopped() {
            self.request(Command::SaveConfig);
        }
    }
}
