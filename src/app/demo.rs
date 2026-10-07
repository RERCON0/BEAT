//! Application-owned README capture: synthetic metadata, no profile or audio.
use super::{egui, Config, DiskEntry, LocalTrack, View};
use crate::i18n::Language;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Snapshot {
    output: PathBuf,
    language: Language,
    dark: bool,
    frames: u32,
    started: Instant,
}

impl Snapshot {
    pub fn from_args() -> Result<Option<Self>, String> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.is_empty() {
            return Ok(None);
        }
        if args.len() != 4 || args[0] != "--screenshot-demo" {
            return Err("Usage: beat --screenshot-demo <absolute.png> <en|ru> <dark|light>".into());
        }
        let output = PathBuf::from(&args[1]);
        if !output.is_absolute() || output.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("png")) {
            return Err("Screenshot output must be an absolute PNG path".into());
        }
        let language = match args[2].to_str() {
            Some("en") => Language::En,
            Some("ru") => Language::Ru,
            _ => return Err("Expected en or ru".into()),
        };
        let dark = match args[3].to_str() {
            Some("dark") => true,
            Some("light") => false,
            _ => return Err("Expected dark or light".into()),
        };
        Ok(Some(Self { output, language, dark, frames: 0, started: Instant::now() }))
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self { output: PathBuf::new(), language: Language::Ru, dark: true, frames: 0, started: Instant::now() }
    }
    pub fn config(&self) -> Config {
        Config {
            language: self.language,
            dark_mode: self.dark,
            cache_dir: r"C:\Music\BEAT-demo".into(),
            ..Config::default()
        }
    }

    pub fn tick(&mut self, ctx: &egui::Context) -> bool {
        let image = ctx.input_mut(|input| {
            let capture = input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            });
            // A capture run has no interactive actions, even if clicked.
            input.events.clear();
            input.pointer = Default::default();
            input.keys_down.clear();
            input.modifiers = Default::default();
            capture
        });
        if let Some(image) = image {
            let pixels: Vec<u8> = image.pixels.iter().flat_map(|color| color.to_array()).collect();
            if let Err(error) = image::save_buffer_with_format(
                &self.output,
                &pixels,
                image.width() as u32,
                image.height() as u32,
                image::ColorType::Rgba8,
                image::ImageFormat::Png,
            ) {
                eprintln!("Screenshot failed: {error}");
                std::process::exit(2);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return true;
        }
        if self.started.elapsed() > Duration::from_secs(20) {
            eprintln!("Screenshot timed out");
            std::process::exit(2);
        }
        self.frames += 1;
        if self.frames == 5 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        false
    }
}

pub fn populate(app: &mut crate::BeatApp, ctx: &egui::Context) {
    let tracks = [
        ("Instant Crush", "Daft Punk", "Random Access Memories", 337.),
        ("Midnight City", "M83", "Hurry Up, We're Dreaming", 244.),
        ("Everything In Its Right Place", "Radiohead", "Kid A", 251.),
        ("Teardrop", "Massive Attack", "Mezzanine", 330.),
        ("Enjoy the Silence", "Depeche Mode", "Violator", 372.),
        ("Time", "Pink Floyd", "The Dark Side of the Moon", 413.),
        ("Dreams", "Fleetwood Mac", "Rumours", 257.),
        ("Feel Good Inc.", "Gorillaz", "Demon Days", 222.),
        ("Do I Wanna Know?", "Arctic Monkeys", "AM", 272.),
        ("Blinding Lights", "The Weeknd", "After Hours", 200.),
        ("Nightcall", "Kavinsky", "OutRun", 258.),
        ("Glory Box", "Portishead", "Dummy", 306.),
        ("Roads", "Portishead", "Dummy", 302.),
        ("Digital Love", "Daft Punk", "Discovery", 301.),
        ("Space Oddity", "David Bowie", "David Bowie", 315.),
        ("Take Five", "The Dave Brubeck Quartet", "Time Out", 324.),
    ];
    let mut entries = Vec::new();
    for (index, (title, artist, album, duration)) in tracks.iter().enumerate() {
        let rel = format!("demo-{index}.flac");
        let entry = DiskEntry::Local(LocalTrack {
            id: format!("local:{rel}"),
            path: app.cache.root().join(&rel),
            rel,
            title: (*title).into(),
            artist: (*artist).into(),
            album: (*album).into(),
            duration: *duration,
            suffix: "flac".into(),
            size: 0,
        });
        // Original geometric thumbnails, not artists' album artwork.
        let colors = [[25, 77, 68], [69, 47, 90], [42, 66, 98], [96, 54, 44]];
        let rgb = colors[index % colors.len()];
        let mut cover = egui::ColorImage::filled([96, 96], egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        for y in 0..96i32 {
            for x in 0..96i32 {
                let radius = (x - 48).pow(2) + (y - 48).pow(2);
                if (80..1600).contains(&radius) {
                    cover[(x as usize, y as usize)] =
                        if radius % 210 < 35 { egui::Color32::from_gray(47) } else { egui::Color32::from_gray(25) };
                }
                if radius < 70 {
                    cover[(x as usize, y as usize)] = egui::Color32::from_rgb(79, 190, 127);
                }
            }
        }
        let texture = ctx.load_texture(format!("demo-cover-{index}"), cover, egui::TextureOptions::LINEAR);
        for px in [super::ROW_COVER_PX, super::COVER_PX] {
            app.covers.insert(format!("{}#{px}#{}", entry.cover_key(), app.disk_cover_generation), texture.clone());
        }
        entries.push(entry);
    }
    app.play_queue = entries.iter().map(DiskEntry::to_song).collect();
    app.current = app.play_queue.first().cloned();
    app.resume_position = 104.0;
    app.local_stats = (entries.len(), 0);
    app.set_disk_entries(entries);
    app.view = View::Library;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_input_cannot_activate_an_application_button() {
        fn inject_click(capture: bool) -> bool {
            let ctx = egui::Context::default();
            let mut center = egui::Pos2::ZERO;
            let input = || egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 300.0))),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    center = ui.button("Save settings").rect.center();
                });
            });
            output.textures_delta.clear();
            let mut raw = input();
            raw.events = vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ];
            let mut clicked = false;
            let mut output = ctx.run_ui(raw, |ui| {
                if capture {
                    let ctx = ui.ctx().clone();
                    Snapshot {
                        output: PathBuf::new(),
                        language: Language::En,
                        dark: true,
                        frames: 0,
                        started: Instant::now(),
                    }
                    .tick(&ctx);
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    clicked = ui.button("Save settings").clicked();
                });
            });
            output.textures_delta.clear();
            clicked
        }
        assert!(inject_click(false), "the control click did not reach the widget");
        assert!(!inject_click(true), "a screenshot run accepted an interactive action");
    }
}
