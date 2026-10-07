//! Shared rendering helpers.
use super::*;

/// Reserve the full scroll range, but lay out cards and request artwork only
/// for rows intersecting the viewport. Large local libraries stay responsive.
pub(super) fn visible_album_grid(
    ui: &mut egui::Ui,
    id: &str,
    albums: &[api::Album],
    mut card: impl FnMut(&mut egui::Ui, &api::Album),
) {
    if albums.is_empty() {
        return;
    }
    let gap = 12.0;
    let width = ui.available_width();
    let columns = ((width + gap) / (ALBUM_CARD_WIDTH + gap)).floor().max(1.0) as usize;
    let rows = albums.len().div_ceil(columns);
    let stride = ALBUM_CARD_HEIGHT + gap;
    let height = rows as f32 * stride - gap;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let clip = ui.clip_rect();
    let start = (((clip.top() - rect.top()) / stride).floor().max(0.0) as usize).min(rows);
    let end = (((clip.bottom() - rect.top()) / stride).ceil().max(0.0) as usize).min(rows);
    for row in start..end {
        let row_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(0.0, row as f32 * stride),
            egui::vec2(width, ALBUM_CARD_HEIGHT),
        );
        ui.scope_builder(egui::UiBuilder::new().id_salt((id, row)).max_rect(row_rect), |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                let start = row * columns;
                for album in &albums[start..(start + columns).min(albums.len())] {
                    card(ui, album);
                }
            });
        });
    }
}

/// Wrapped grid of album cards. `horizontal_wrapped` centers items on the
/// cross axis, so cards of different heights end up at different tops; this
/// top-aligned wrapped layout keeps every row straight and wraps into rows.
pub(super) fn album_grid(ui: &mut egui::Ui, add_cards: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true), |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
        add_cards(ui);
    });
}

pub(super) fn album_card(
    ui: &mut egui::Ui,
    album: &api::Album,
    cover: Option<egui::TextureHandle>,
    can_download: bool,
) -> AlbumCardAction {
    let mut action = AlbumCardAction::None;
    // A bare `Frame` never wraps in a wrapped grid: egui advances its cursor
    // without running the wrap decision, and the row runs off the panel.
    // Allocating a fixed-width vertical slot first gives the grid an item it
    // can wrap into rows; the frame then draws inside that slot.
    ui.allocate_ui_with_layout(
        egui::vec2(ALBUM_CARD_WIDTH, ALBUM_CARD_HEIGHT),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::NONE.stroke(egui::Stroke::new(1.0, theme::line())).inner_margin(egui::Margin::same(8)).show(
                ui,
                |ui| {
                    ui.set_width(168.0);
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(168.0, 168.0), egui::Sense::click());
                    match cover {
                        Some(texture) => {
                            ui.painter().image(
                                texture.id(),
                                rect,
                                square_cover_uv(texture.size()),
                                egui::Color32::WHITE,
                            );
                        }
                        None => {
                            ui.painter().rect_filled(rect, 0.0, theme::field());
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "♪",
                                egui::FontId::proportional(36.0),
                                theme::faint(),
                            );
                        }
                    }
                    if response.clicked() {
                        action = AlbumCardAction::Open;
                    }
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    ui.add_space(4.0);
                    // Fixed-height single-line rows: cards with different
                    // titles must keep the same height, or the wrapped grid
                    // turns into a staircase.
                    let text_row = |ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(168.0, size + 5.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).truncate());
                            },
                        );
                    };
                    text_row(ui, &clip(&album.name, 30), 12.0, theme::text());
                    text_row(ui, &clip(&album.artist, 30), 10.0, theme::faint());
                    ui.horizontal(|ui| {
                        if ui.button("▶").on_hover_text(crate::i18n::tr("слушать")).clicked() {
                            action = AlbumCardAction::Play;
                        }
                        if can_download && ui.button("↓").on_hover_text(crate::i18n::tr("скачать альбом")).clicked()
                        {
                            action = AlbumCardAction::Download;
                        }
                        if album.year > 0 {
                            ui.label(egui::RichText::new(format!("{}", album.year)).size(10.0).color(theme::faint()));
                        }
                    });
                },
            );
        },
    );
    action
}

pub(super) fn profile_row(ui: &mut egui::Ui, name: &str, count: usize, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let title = clip_to_width(ui, name, egui::FontId::monospace(13.0), (rect.width() - 44.0).max(10.0));
    let painter = ui.painter();
    if active || response.hovered() {
        painter.rect_filled(rect, 0.0, theme::lift());
    }
    painter.text(
        rect.left_center() + egui::vec2(7.0, 0.0),
        egui::Align2::LEFT_CENTER,
        if active { "●" } else { "○" },
        egui::FontId::monospace(11.0),
        if active { theme::accent() } else { theme::faint() },
    );
    painter.text(
        rect.left_center() + egui::vec2(23.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::monospace(13.0),
        if active { theme::text() } else { theme::dim() },
    );
    if count > 0 {
        painter.text(
            rect.right_center() - egui::vec2(7.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{count:02}"),
            egui::FontId::monospace(10.0),
            theme::faint(),
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// Glyph and hover of the transport play/pause button. It offers «pause»
/// only while the track is really playing; a track restored from the last
/// session (or one that failed to start) shows «play».
pub(super) fn transport_play_label(has_current: bool, loaded: bool, paused: bool) -> (&'static str, &'static str) {
    if loaded && !paused {
        ("▮▮", crate::i18n::tr("пауза"))
    } else if has_current {
        ("▶", crate::i18n::tr("продолжить"))
    } else {
        ("▶", crate::i18n::tr("начать воспроизведение"))
    }
}

/// Fixed-width toggle for the player modes (shuffle/repeat): accent when on,
/// faint when off, so the transport row never shifts.
pub(super) fn mode_button(ui: &mut egui::Ui, glyph: &str, active: bool, hover: &str) -> bool {
    let color = if active { theme::accent() } else { theme::faint() };
    let stroke = if active { theme::accent() } else { theme::line() };
    // Matches the transport buttons, so the centred block keeps its width.
    ui.add(
        egui::Button::new(egui::RichText::new(glyph).family(egui::FontFamily::Name("symbols".into())).color(color))
            .stroke(egui::Stroke::new(1.0, stroke))
            .min_size(egui::vec2(40.0, 30.0)),
    )
    .on_hover_text(hover)
    .clicked()
}

/// Square cover thumbnail with the "♪" placeholder used across the app. The
/// response lets callers make the cover itself clickable (list rows).
pub(super) fn row_cover(
    ui: &mut egui::Ui,
    size: f32,
    texture: Option<egui::TextureHandle>,
    sense: egui::Sense,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
    match texture {
        Some(texture) => {
            ui.painter().image(texture.id(), rect, square_cover_uv(texture.size()), egui::Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, theme::field());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "♪",
                egui::FontId::proportional(size * 0.5),
                theme::faint(),
            );
        }
    }
    response
}

/// Fill the square cover slot without stretching rectangular textures. New
/// thumbnails are square; this also handles an older/manually replaced cache.
pub(super) fn square_cover_uv([width, height]: [usize; 2]) -> egui::Rect {
    let full = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    if width == 0 || height == 0 {
        return full;
    }
    if width > height {
        let margin = (1.0 - height as f32 / width as f32) * 0.5;
        egui::Rect::from_min_max(egui::pos2(margin, 0.0), egui::pos2(1.0 - margin, 1.0))
    } else {
        let margin = (1.0 - width as f32 / height as f32) * 0.5;
        egui::Rect::from_min_max(egui::pos2(0.0, margin), egui::pos2(1.0, 1.0 - margin))
    }
}

/// Vector "local file" mark: a small folder glyph drawn with the painter
/// (no SVG-renderer dependency), meaning "dropped into the cache folder by
/// hand, not downloaded from the server".
pub(super) fn local_marker(ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    let color = theme::accent();
    let x0 = rect.left() + 1.0;
    let x1 = rect.right() - 1.0;
    let tab = egui::Rect::from_min_max(egui::pos2(x0, rect.top() + 3.0), egui::pos2(x0 + 6.0, rect.top() + 6.0));
    let body = egui::Rect::from_min_max(egui::pos2(x0, rect.top() + 5.0), egui::pos2(x1, rect.bottom() - 3.0));
    painter.rect_filled(tab, 0.0, color);
    painter.rect_filled(body, 1.0, color);
    let _ = response.on_hover_text(crate::i18n::tr("локальный файл — лежит в папке кеша, не скачан с сервера"));
}

pub(super) fn status_sep(ui: &mut egui::Ui) {
    ui.label(egui::RichText::new("│").size(10.0).color(theme::status_sep()));
}

pub(super) fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text).size(11.0).color(theme::dim()));
    ui.add_space(3.0);
}

pub(super) fn format_label(format: StreamFormat) -> String {
    match format {
        StreamFormat::Raw => "raw".into(),
        StreamFormat::Mp3 => "mp3".into(),
    }
}

/// List row as one coloured line: title, then a dim artist, then a fainter
/// album. Missing parts leave no dangling separators, and a long row is cut
/// off by the label's truncation (the full text stays in the hover).
pub(super) fn texts_job(title: &str, artist: &str, album: &str, playing: bool) -> egui::text::LayoutJob {
    let format = |size: f32, color: egui::Color32| egui::TextFormat {
        font_id: egui::FontId::new(size, egui::FontFamily::Proportional),
        color,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(title, 0.0, format(12.0, if playing { theme::accent() } else { theme::text() }));
    if !artist.trim().is_empty() {
        job.append(ROW_SEP, 0.0, format(12.0, theme::faint()));
        job.append(artist, 0.0, format(11.5, theme::dim()));
    }
    if !album.trim().is_empty() {
        job.append(ROW_SEP, 0.0, format(12.0, theme::faint()));
        job.append(album, 0.0, format(11.5, theme::faint()));
    }
    job
}

pub(super) fn entry_row_job(entry: &DiskEntry, playing: bool) -> egui::text::LayoutJob {
    texts_job(entry.title(), entry.artist(), entry.album(), playing)
}

/// Full, untruncated row text for the hover.
pub(super) fn texts_hover(title: &str, artist: &str, album: &str) -> String {
    let mut out = title.trim().to_owned();
    if !artist.trim().is_empty() {
        out.push_str(" — ");
        out.push_str(artist.trim());
    }
    if !album.trim().is_empty() {
        out.push('\n');
        out.push_str(album.trim());
    }
    out
}

/// `texts_hover` plus the file path for rows that have one on disk.
pub(super) fn entry_hover(entry: &DiskEntry) -> String {
    let mut out = texts_hover(entry.title(), entry.artist(), entry.album());
    if let Some(path) = entry.local_path() {
        out.push('\n');
        out.push_str(&path.display().to_string());
    }
    out
}

/// «1 раз», «2 раза», «5 раз» — Russian plural for the listen counter.
pub(super) fn play_count_label(count: u64) -> String {
    if i18n::current() == i18n::Language::En {
        return format!("{count} {}", if count == 1 { "play" } else { "plays" });
    }
    let word = if (11..=14).contains(&(count % 100)) {
        crate::i18n::tr("раз")
    } else {
        match count % 10 {
            2..=4 => crate::i18n::tr("раза"),
            _ => crate::i18n::tr("раз"),
        }
    };
    format!("{count} {word}")
}

pub(super) fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub(super) fn clip_to_width(ui: &egui::Ui, text: &str, font: egui::FontId, max_w: f32) -> String {
    let width =
        |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE).rect.width());
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&cand) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return "…".to_owned();
    }
    chars[..lo].iter().collect::<String>() + "…"
}

pub(super) fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00".into();
    }
    let seconds = seconds as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub(super) fn human_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        crate::i18n::trf!("{kb:.0} КБ", kb = kb)
    } else if kb < 1024.0 * 1024.0 {
        crate::i18n::trf!("{:.1} МБ", kb / 1024.0)
    } else {
        crate::i18n::trf!("{:.2} ГБ", kb / 1024.0 / 1024.0)
    }
}
