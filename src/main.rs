// GUI exe: no console window when launched from Explorer. Panics are routed
// to a MessageBox in main() (windows-subsystem exes die silently otherwise).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod app;
mod banner;
mod cache;
mod catalog;
mod config;
mod covers;
mod i18n;
mod local;
mod media;
mod media_keys;
mod opus;
mod platform;
mod playback;
mod player;
mod session;
mod stats;
mod theme;

use app::{demo, BeatApp};
use eframe::egui;
use platform::{acquire_single_instance, app_icon, fatal_dialog};
use std::sync::Mutex;

/// Lock that recovers from poisoning. Every mutex in this app guards a plain
/// collection, so a panic elsewhere must not turn each later frame into
/// another panic: the data behind the flag is still consistent enough to keep
/// the app running, which is what a media player should do.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

thread_local! {
    /// Set while a panic is expected and handled locally (image and audio
    /// decoding, local tag probing), so the global panic dialog does not pop
    /// up for one broken file.
    pub(crate) static HANDLED_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn main() -> eframe::Result {
    let demo = demo::Snapshot::from_args().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2)
    });
    if demo.is_none() {
        if let Err(err) = acquire_single_instance("Global\\beat-single-instance") {
            fatal_dialog(
                "BEAT",
                &crate::i18n::trf!("{err}.\n\nЗакройте уже открытое окно BEAT и запустите это снова.", err = err),
            );
            std::process::exit(1);
        }
    }
    std::panic::set_hook(Box::new(|info| {
        if HANDLED_PANIC.with(|handled| handled.get()) {
            return;
        }
        fatal_dialog(
            crate::i18n::tr("BEAT — внутренняя ошибка"),
            &crate::i18n::trf!("BEAT не смог продолжить работу.\n\n{info}", info = info),
        );
    }));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([1000.0, 640.0])
        .with_resizable(true)
        .with_title("BEAT — by rercon prod.");
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    if demo.is_some() {
        viewport = viewport.with_inner_size([1280.0, 800.0]).with_resizable(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native("BEAT", options, Box::new(move |cc| Ok(Box::new(BeatApp::new(cc, demo)))));
    if let Err(e) = &result {
        fatal_dialog(
            crate::i18n::tr("BEAT — не удалось открыть окно"),
            &crate::i18n::trf!("Причина: {e}\n\nДля запуска нужен OpenGL 2.1+. Включите 3D-ускорение в настройках ВМ либо запустите программу на обычном рабочем столе.", e = e),
        );
    }
    result
}
