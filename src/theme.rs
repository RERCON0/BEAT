//! Same "Terminal Native" system as SNATCH and STRIKE (square corners,
//! hairline ghost buttons, one accent used sparingly), in a day/night pair:
//! every color is a getter over the active mode instead of a constant, so the
//! whole UI flips with one call. The palette is shared with those two apps, so
//! a few entries BEAT does not draw yet are kept and marked individually rather
//! than silencing the whole module.

use eframe::egui;
use egui::Color32;
use std::sync::atomic::{AtomicBool, Ordering};

static DARK: AtomicBool = AtomicBool::new(true);

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

pub fn set_dark(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub fn bg() -> Color32 {
    if is_dark() {
        rgb(0x0b, 0x0b, 0x0e)
    } else {
        rgb(0xf3, 0xf3, 0xf1)
    }
}

pub fn lift() -> Color32 {
    if is_dark() {
        rgb(0x0e, 0x0e, 0x11)
    } else {
        rgb(0xff, 0xff, 0xff)
    }
}

pub fn field() -> Color32 {
    if is_dark() {
        rgb(0x11, 0x11, 0x14)
    } else {
        rgb(0xfb, 0xfb, 0xfa)
    }
}

pub fn line() -> Color32 {
    if is_dark() {
        rgb(0x1e, 0x1e, 0x24)
    } else {
        rgb(0xda, 0xda, 0xd6)
    }
}

/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn line2() -> Color32 {
    if is_dark() {
        rgb(0x33, 0x33, 0x3c)
    } else {
        rgb(0xb5, 0xb5, 0xb0)
    }
}

pub fn text() -> Color32 {
    if is_dark() {
        rgb(0xd6, 0xd6, 0xda)
    } else {
        rgb(0x1c, 0x1c, 0x1e)
    }
}

pub fn dim() -> Color32 {
    if is_dark() {
        rgb(0x83, 0x83, 0x8c)
    } else {
        rgb(0x5c, 0x5c, 0x62)
    }
}

pub fn faint() -> Color32 {
    if is_dark() {
        rgb(0x53, 0x53, 0x5c)
    } else {
        rgb(0x9a, 0x9a, 0x9f)
    }
}

pub fn accent() -> Color32 {
    if is_dark() {
        rgb(0x59, 0xd6, 0x8c)
    } else {
        rgb(0x1a, 0x7f, 0x37)
    }
}

pub fn warn() -> Color32 {
    if is_dark() {
        rgb(0xe0, 0xb3, 0x4d)
    } else {
        rgb(0x9a, 0x67, 0x00)
    }
}

pub fn err() -> Color32 {
    if is_dark() {
        rgb(0xe0, 0x65, 0x5c)
    } else {
        rgb(0xcf, 0x22, 0x2e)
    }
}

/// Streaming text color (mockup's .output default `#c9c9ce`).
/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn output() -> Color32 {
    if is_dark() {
        rgb(0xc9, 0xc9, 0xce)
    } else {
        rgb(0x2b, 0x2b, 0x2e)
    }
}

/// Empty-cards placeholder text.
/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn placeholder() -> Color32 {
    if is_dark() {
        rgb(0x4e, 0x4e, 0x56)
    } else {
        rgb(0xa8, 0xa8, 0xad)
    }
}

/// Statusbar separator dot.
pub fn status_sep() -> Color32 {
    if is_dark() {
        rgb(0x2e, 0x2e, 0x35)
    } else {
        rgb(0xc9, 0xc9, 0xc4)
    }
}

// SIL OFL 1.1 (c) Microsoft Corporation, see fonts/OFL-notice.txt
const BUNDLED_MONO: &[u8] = include_bytes!("../fonts/CascadiaMono-Light.ttf");

/// See SNATCH's setup_theme for the full reasoning behind the per-family
/// glyph nudges; STRIKE reuses the exact same four families.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let body = egui::FontData::from_static(BUNDLED_MONO)
        .tweak(egui::FontTweak { y_offset_factor: 0.15, ..Default::default() });
    fonts.font_data.insert("mono-system".to_owned(), body.into());
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "mono-system".to_owned());
        }
    }
    // Buttons/title/fields: no glyph nudge (see SNATCH: body's +0.15em shift
    // reads fine in free-standing labels but sits low inside boxed controls).
    // Cascadia lacks some transport/mode glyphs (⇄ ↻ …); the default egui
    // fonts stay on as fallbacks, otherwise buttons would draw replacement
    // squares.
    let fallbacks = ["Hack", "Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"];
    let family = |name: &str| {
        let mut list = vec![name.to_owned()];
        list.extend(fallbacks.iter().map(|font| (*font).to_owned()));
        list
    };
    fonts.font_data.insert(
        "mono-button".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak { y_offset_factor: 0.0, ..Default::default() })
            .into(),
    );
    fonts.families.insert(egui::FontFamily::Name("button".into()), family("mono-button"));
    fonts.font_data.insert(
        "mono-title".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak { y_offset_factor: -0.08, ..Default::default() })
            .into(),
    );
    fonts.families.insert(egui::FontFamily::Name("title".into()), family("mono-title"));
    fonts.font_data.insert(
        "mono-field".to_owned(),
        egui::FontData::from_static(BUNDLED_MONO)
            .tweak(egui::FontTweak { y_offset_factor: 0.0, ..Default::default() })
            .into(),
    );
    fonts.families.insert(egui::FontFamily::Name("field".into()), family("mono-field"));
    // Use the symbol face directly: recent egui shaping can clip fallback
    // arrows when the primary face uses different metrics.
    fonts
        .families
        .insert(egui::FontFamily::Name("symbols".into()), fallbacks.iter().map(|name| (*name).into()).collect());
    ctx.set_fonts(fonts);
}

/// One-time startup setup: fonts and the palette for the persisted mode.
pub fn apply(ctx: &egui::Context, dark: bool) {
    set_dark(dark);
    setup_fonts(ctx);
    // Both buckets (dark_style/light_style): whichever one egui's theme()
    // resolves to later must carry our palette, not just the active one now.
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 8.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = egui::Margin::symmetric(10, 15);
        style.spacing.scroll = egui::style::ScrollStyle { foreground_color: true, ..egui::style::ScrollStyle::solid() };
        let mut ts = style.text_styles.clone();
        ts.insert(egui::TextStyle::Body, egui::FontId::new(13.0, egui::FontFamily::Proportional));
        ts.insert(egui::TextStyle::Button, egui::FontId::new(12.5, egui::FontFamily::Name("button".into())));
        ts.insert(egui::TextStyle::Small, egui::FontId::new(13.0, egui::FontFamily::Proportional));
        ts.insert(egui::TextStyle::Monospace, egui::FontId::new(13.0, egui::FontFamily::Monospace));
        style.text_styles = ts;
    });
}

/// Runtime flip (title-bar день/ночь): swaps the palette in both style
/// buckets and updates the palette getters' mode.
pub fn set_mode(ctx: &egui::Context, dark: bool) {
    set_dark(dark);
    ctx.all_styles_mut(|style| {
        style.visuals = terminal_visuals(dark);
    });
}

fn terminal_visuals(dark: bool) -> egui::Visuals {
    let (bg, lift, field, line, line2, text, dim, accent, warn, err) = if dark {
        (
            rgb(0x0b, 0x0b, 0x0e),
            rgb(0x0e, 0x0e, 0x11),
            rgb(0x11, 0x11, 0x14),
            rgb(0x1e, 0x1e, 0x24),
            rgb(0x33, 0x33, 0x3c),
            rgb(0xd6, 0xd6, 0xda),
            rgb(0x83, 0x83, 0x8c),
            rgb(0x59, 0xd6, 0x8c),
            rgb(0xe0, 0xb3, 0x4d),
            rgb(0xe0, 0x65, 0x5c),
        )
    } else {
        (
            rgb(0xf3, 0xf3, 0xf1),
            rgb(0xff, 0xff, 0xff),
            rgb(0xfb, 0xfb, 0xfa),
            rgb(0xda, 0xda, 0xd6),
            rgb(0xb5, 0xb5, 0xb0),
            rgb(0x1c, 0x1c, 0x1e),
            rgb(0x5c, 0x5c, 0x62),
            rgb(0x1a, 0x7f, 0x37),
            rgb(0x9a, 0x67, 0x00),
            rgb(0xcf, 0x22, 0x2e),
        )
    };

    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.window_corner_radius = egui::CornerRadius::ZERO;
    v.menu_corner_radius = egui::CornerRadius::ZERO;
    v.window_fill = lift;
    v.window_stroke = egui::Stroke::new(1.0, line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.panel_fill = bg;
    v.extreme_bg_color = field;
    v.faint_bg_color = field;
    v.code_bg_color = field;
    v.hyperlink_color = accent;
    v.warn_fg_color = warn;
    v.error_fg_color = err;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 45);
    v.selection.stroke = egui::Stroke::new(1.0, accent);

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::ZERO;
        w.bg_fill = field;
    }
    v.widgets.noninteractive.weak_bg_fill = bg;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, dim);

    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, line);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, dim);

    v.widgets.hovered.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, line2);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.weak_bg_fill = field;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, text);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0, line2);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, text);
    v
}

pub fn accent_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(accent()))
        .stroke(egui::Stroke::new(1.0, accent()))
        .fill(Color32::TRANSPARENT)
}

/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn danger_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(err()))
        .stroke(egui::Stroke::new(1.0, line2()))
        .fill(Color32::TRANSPARENT)
}

pub fn window_title(text: &str) -> egui::RichText {
    egui::RichText::new(text).font(egui::FontId::new(13.0, egui::FontFamily::Name("title".into())))
}

/// Input-field text: body size with the boxed controls' rendering, so text
/// sits centred inside the field's margins.
pub fn field_font() -> egui::FontSelection {
    egui::FontSelection::FontId(egui::FontId::new(13.0, egui::FontFamily::Name("field".into())))
}

/// Bordered `[prefix value]` chip used by the composer row.
/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn tag(ui: &mut egui::Ui, prefix: &str, value: &str, value_color: Color32) {
    egui::Frame::NONE.stroke(egui::Stroke::new(1.0, line())).inner_margin(egui::Margin::symmetric(7, 4)).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(egui::RichText::new(prefix).size(11.0).color(dim()));
            ui.label(egui::RichText::new(value).size(11.0).color(value_color));
        },
    );
}

/// `[ SECTION ]` sidebar caption.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(egui::RichText::new(format!("[ {text} ]")).size(11.0).color(faint()));
    ui.add_space(2.0);
}

/// Hairline row separator that owns exactly 1px of height.
/// Part of the shared palette; BEAT does not draw it yet.
#[allow(dead_code)]
pub fn hline(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, line());
}

/// `label` + right-aligned value row (sidebar "system"/"session" lists).
pub fn kv_row(ui: &mut egui::Ui, label: &str, value: &str, value_color: Color32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(11.0).color(dim()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).size(11.0).color(value_color));
        });
    });
}
