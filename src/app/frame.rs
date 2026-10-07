//! Frame lifecycle: drain workers, draw, synchronize and schedule repaint.
use super::*;

impl eframe::App for BeatApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.demo.is_some() {
            return;
        }
        self.check_output_health();
        self.pump_media_keys();
        self.pump_library();
        self.pump_catalog();
        self.pump_covers(ctx);
        self.pump_downloads();
        self.maybe_start_automatic_scan();
        self.update_playback(ctx);
        self.sync_media_controls();
        self.schedule_repaint(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(snapshot) = &mut self.demo {
            if snapshot.tick(&ctx) {
                return;
            }
        }

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)) {
            self.request(Command::Search);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.settings_open {
                self.settings_open = false;
            } else if self.view != View::Albums {
                self.view = View::Albums;
            }
        }
        #[cfg(windows)]
        self.ui_title_bar(ui);

        egui::Panel::top("header")
            .exact_size(58.0)
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin::symmetric(16, 0)))
            .show_separator_line(true)
            .show(ui, |ui| self.ui_header(ui));

        egui::Panel::bottom("player")
            .exact_size(64.0)
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin::symmetric(14, 0)))
            .show_separator_line(true)
            .show(ui, |ui| self.ui_player_bar(ui));

        egui::Panel::bottom("statusbar")
            .exact_size(26.0)
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin::symmetric(10, 0)))
            .show_separator_line(false)
            .show(ui, |ui| self.ui_statusbar(ui));

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(238.0)
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin::symmetric(12, 14)))
            .show_separator_line(true)
            .show(ui, |ui| self.ui_sidebar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::bg()).inner_margin(egui::Margin::symmetric(14, 12)))
            .show(ui, |ui| self.ui_central(ui));

        self.ui_settings_modal(&ctx);
        self.ui_queue(&ctx);
        self.dispatch_commands(&ctx);
        self.sync_media_controls();

        #[cfg(windows)]
        resize_edges(&ctx);

        self.schedule_repaint(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.demo.is_some() {
            return;
        }
        // The play counters are debounced; the last listens must land on disk.
        self.invalidate_prefetch();
        self.stats.flush();
        self.save_session();
    }
}
