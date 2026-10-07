use super::*;

impl BeatApp {
    pub(super) fn ui_queue(&mut self, ctx: &egui::Context) {
        if !self.queue_open {
            return;
        }
        let mut open = self.queue_open;
        let mut action = None;
        egui::Window::new(crate::i18n::tr("[ ОЧЕРЕДЬ ]"))
            .open(&mut open)
            .default_width(620.0)
            .default_height(430.0)
            .show(ctx, |ui| {
                if ui.button(crate::i18n::tr("[ ОСТАВИТЬ ТЕКУЩИЙ ТРЕК ]")).clicked() {
                    action = Some(QueueAction::KeepCurrent);
                }
                if self.play_queue.is_empty() {
                    ui.label(crate::i18n::tr("Очередь пуста"));
                }
                egui::ScrollArea::vertical().show_rows(ui, 28.0, self.play_queue.len(), |ui, rows| {
                    for index in rows {
                        let song = &self.play_queue[index];
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button(if index == self.play_index { "●" } else { "▶" }).clicked() {
                                    action = Some(QueueAction::Play(index));
                                }
                                let text = format!("{} · {}", song.title, song.artist);
                                ui.add_sized(
                                    [(ui.available_width() - 104.0).max(80.0), 24.0],
                                    egui::Label::new(egui::RichText::new(text).color(if index == self.play_index {
                                        theme::accent()
                                    } else {
                                        theme::text()
                                    }))
                                    .truncate(),
                                );
                                if ui.add_enabled(index > 0, egui::Button::new("↑")).clicked() {
                                    action = Some(QueueAction::Move(index, index - 1));
                                }
                                if ui.add_enabled(index + 1 < self.play_queue.len(), egui::Button::new("↓")).clicked()
                                {
                                    action = Some(QueueAction::Move(index, index + 1));
                                }
                                if ui.button("×").on_hover_text(crate::i18n::tr("Убрать из очереди")).clicked()
                                {
                                    action = Some(QueueAction::Remove(index));
                                }
                            });
                        });
                    }
                });
            });
        self.queue_open = open;
        if let Some(action) = action {
            self.request(Command::Queue(action));
        }
    }
}
