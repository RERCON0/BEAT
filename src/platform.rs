//! Native window and Windows integration helpers.
use eframe::egui;

/// Stable id of the current default output device; `None` when there is no
/// device or the id cannot be read. Used to notice that the OS switched
/// devices (Bluetooth connected or switched off) while BEAT runs.
pub(crate) fn current_output_device_id() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let device = rodio::cpal::default_host().default_output_device()?;
    device.id().ok().map(|id| id.to_string())
}

#[cfg(windows)]
pub(crate) fn open_in_explorer(path: &str) {
    let explorer = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("explorer.exe");
    let _ = std::process::Command::new(explorer).arg(path).spawn();
}

#[cfg(not(windows))]
pub(crate) fn open_in_explorer(_path: &str) {}

#[cfg(windows)]
pub(crate) fn pick_folder() -> Option<String> {
    rfd::FileDialog::new().pick_folder().map(|path| path.to_string_lossy().into_owned())
}

#[cfg(not(windows))]
pub(crate) fn pick_folder() -> Option<String> {
    None
}

const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/beat-256.png"));

pub(crate) fn app_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let rgba = img.into_rgba8();
    Some(egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

pub(crate) fn app_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let rgba = img.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    Some(ctx.load_texture(
        "app-icon",
        egui::ColorImage::from_rgba_unmultiplied(size, &pixels),
        egui::TextureOptions::LINEAR,
    ))
}

/// Borderless windows have no native resize border; begin the OS resize on
/// mouse-down so an outward drag is not lost.
#[cfg(windows)]
pub(crate) fn resize_edges(ctx: &egui::Context) {
    let screen = ctx.viewport_rect();
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let e = 10.0;
    let origin = screen.min;
    let w = screen.width();
    let h = screen.height();
    let handles = [
        (origin, egui::vec2(e, e), egui::ResizeDirection::NorthWest),
        (origin + egui::vec2(e, 0.0), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::North),
        (origin + egui::vec2(w - e, 0.0), egui::vec2(e, e), egui::ResizeDirection::NorthEast),
        (origin + egui::vec2(0.0, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::West),
        (origin + egui::vec2(w - e, e), egui::vec2(e, h - 2.0 * e), egui::ResizeDirection::East),
        (origin + egui::vec2(0.0, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthWest),
        (origin + egui::vec2(e, h - e), egui::vec2(w - 2.0 * e, e), egui::ResizeDirection::South),
        (origin + egui::vec2(w - e, h - e), egui::vec2(e, e), egui::ResizeDirection::SouthEast),
    ];
    for (i, (pos, size, direction)) in handles.into_iter().enumerate() {
        egui::Area::new(egui::Id::new(("window_resize", i))).order(egui::Order::Foreground).fixed_pos(pos).show(
            ctx,
            |ui| {
                let (_, response) = ui.allocate_exact_size(size, egui::Sense::drag());
                if response.hovered() {
                    let icon = match direction {
                        egui::ResizeDirection::North | egui::ResizeDirection::South => egui::CursorIcon::ResizeVertical,
                        egui::ResizeDirection::East | egui::ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
                        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
                            egui::CursorIcon::ResizeNwSe
                        }
                        egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
                            egui::CursorIcon::ResizeNeSw
                        }
                    };
                    ctx.set_cursor_icon(icon);
                }
                if response.is_pointer_button_down_on() && ctx.input(|input| input.pointer.primary_pressed()) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                }
            },
        );
    }
}

/// A windows-subsystem exe has no console: a startup failure must be shown in
/// a MessageBox or the process dies silently.
#[cfg(windows)]
pub(crate) fn fatal_dialog(title: &str, text: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        pub(crate) fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, mb_type: u32) -> i32;
    }
    pub(crate) const MB_ICONERROR: u32 = 0x10;
    let wide = |s: &str| -> Vec<u16> { std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect() };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), MB_ICONERROR);
    }
}

#[cfg(not(windows))]
pub(crate) fn fatal_dialog(_title: &str, _text: &str) {}

/// Two copies would race on the cache index and config, so a named mutex
/// refuses the second window. The name is `Global\`, not `Local\`: a
/// per-session name would let another session (RDP, fast user switching) start
/// its own copy over the same cache folder and config.
#[cfg(windows)]
pub(crate) fn acquire_single_instance(name: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        pub(crate) fn CreateMutexW(attrs: *const c_void, initially_owned: i32, name: *const u16) -> *mut c_void;
        pub(crate) fn GetLastError() -> u32;
    }
    pub(crate) const ERROR_ALREADY_EXISTS: u32 = 183;
    let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(crate::i18n::trf!("не удалось создать мьютекс: {}", std::io::Error::last_os_error()));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(crate::i18n::tr("BEAT уже запущен").into());
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn acquire_single_instance(_name: &str) -> Result<(), String> {
    Ok(())
}
