use super::*;

#[test]
fn queue_edits_preserve_the_current_occurrence_even_with_duplicate_ids() {
    let songs: Vec<_> =
        (0..6).map(|i| api::Song { id: "same-id".into(), title: i.to_string(), ..Default::default() }).collect();
    for current in 0..songs.len() {
        for from in 0..songs.len() {
            for to in 0..songs.len() {
                let mut queue = songs.clone();
                let mut index = current;
                move_queue_item(&mut queue, &mut index, from, to);
                assert_eq!(queue[index].title, songs[current].title);
                let mut titles: Vec<_> = queue.iter().map(|song| song.title.clone()).collect();
                titles.sort();
                assert_eq!(titles, (0..6).map(|i| i.to_string()).collect::<Vec<_>>());
            }
        }
    }
    for current in 0..songs.len() {
        for removed in 0..songs.len() {
            let mut queue = songs.clone();
            let mut index = current;
            remove_queue_item(&mut queue, &mut index, removed);
            assert_eq!(queue.len(), 5);
            if current != removed {
                assert_eq!(queue[index].title, songs[current].title);
            } else {
                assert_eq!(
                    queue[index].title,
                    songs[if removed + 1 < songs.len() { removed + 1 } else { removed - 1 }].title
                );
            }
        }
    }
    let mut empty = Vec::new();
    let mut index = 0;
    remove_queue_item(&mut empty, &mut index, usize::MAX);
    move_queue_item(&mut empty, &mut index, 0, usize::MAX);
    assert!(empty.is_empty());
}

#[test]
fn seeking_and_pausing_do_not_manufacture_listens() {
    let mut listened = 0.0;
    let mut previous = 0.0;
    for step in 1..=100 {
        let position = step as f64 * 30.0;
        listened += listen_advance(previous, position, 0.001, false);
        previous = position;
    }
    assert!(listened < MIN_LISTEN_SECS, "rapid seeks counted as a listen");
    listened += listen_advance(previous, previous + 10.0, 100.0, true);
    assert!(listened < MIN_LISTEN_SECS, "paused time counted as a listen");
    assert_eq!(listen_advance(previous, 0.0, 1.0, false), 0.0);
    assert_eq!(listen_advance(0.0, 2.0, 2.0, false), 2.0);
}

#[test]
fn a_large_album_grid_draws_only_visible_cards_and_keeps_the_scroll_range() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, true);
    let albums: Vec<_> =
        (0..5000).map(|id| api::Album { id: id.to_string(), name: "Album".into(), ..Default::default() }).collect();
    let mut drawn = 0;
    let mut max_height = 0.0f32;
    let mut content_height = 0.0;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 600.0))),
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let scroll = egui::ScrollArea::vertical().show(ui, |ui| {
                    visible_album_grid(ui, "test-albums", &albums, |ui, album| {
                        let start = ui.cursor().top();
                        album_card(ui, album, None, true);
                        max_height = max_height.max(ui.min_rect().bottom() - start);
                        drawn += 1;
                    });
                });
                content_height = scroll.content_size.y;
            });
        },
    );
    output.textures_delta.clear();
    assert!(drawn > 0 && drawn <= 12, "laid out {drawn} of 5000 cards");
    assert!(max_height <= ALBUM_CARD_HEIGHT + 1.0, "card height {max_height} exceeds its reserved row");
    assert!(content_height > 400_000.0, "offscreen rows disappeared: {content_height}");
}

#[test]
fn time_and_size_formatting() {
    assert_eq!(format_time(0.0), "0:00");
    assert_eq!(format_time(61.4), "1:01");
    assert_eq!(format_time(-3.0), "0:00");
    assert_eq!(human_size(4 * 1024), "4 КБ");
    assert!(human_size(5 * 1024 * 1024).contains("МБ"));
}

#[test]
fn the_ui_keeps_ticking_only_while_something_is_going_on() {
    assert_eq!(repaint_after(false, 0, 0), None, "an idle window must not redraw by itself");
    let playing = repaint_after(true, 0, 0).expect("a playing track needs its clock");
    assert!(repaint_after(false, 2, 0).is_some(), "downloads in progress");
    assert!(repaint_after(false, 0, 1).is_some(), "a request in flight");
    assert!(
        playing >= std::time::Duration::from_millis(50) && playing <= std::time::Duration::from_millis(250),
        "{playing:?}"
    );
}

#[test]
fn the_disk_list_is_rescanned_at_most_every_few_seconds_during_downloads() {
    let secs = std::time::Duration::from_secs;
    assert!(!disk_refresh_due(false, false, false, secs(60)), "nothing changed");
    assert!(!disk_refresh_due(true, true, false, secs(60)), "a scan is already running");
    assert!(disk_refresh_due(true, false, false, secs(0)), "downloads finished: refresh at once");
    assert!(!disk_refresh_due(true, false, true, secs(1)), "still downloading, scanned a second ago");
    assert!(disk_refresh_due(true, false, true, secs(6)), "still downloading, but it has been a while");
}

#[test]
fn oversized_cover_dimensions_are_refused_before_decoding() {
    let png = |width: u32, height: u32| {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    };
    assert!(decode_cover_sized(&png(64, 64), 32).is_some(), "an ordinary cover must decode");
    assert!(decode_cover_sized(&png(9_000, 8), 32).is_none(), "9000 px wide accepted");
    assert!(decode_cover_sized(&png(8, 9_000), 32).is_none(), "9000 px tall accepted");
}

#[test]
fn rectangular_artwork_is_cropped_before_making_a_square_thumbnail() {
    for (width, height) in [(128, 72), (72, 128), (72, 72), (8192, 1)] {
        let side = width.min(height);
        let left = (width - side) / 2;
        let top = (height - side) / 2;
        let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
            if x >= left && x < left + side && y >= top && y < top + side {
                image::Rgba([40, 100, 160, 255])
            } else {
                image::Rgba([0, 0, 0, 255])
            }
        });
        let mut bytes = Vec::new();
        pixels.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        let cover = decode_cover_sized(&bytes, 32).unwrap();
        assert_eq!(cover.size, [32, 32], "{width} x {height}");
        assert!(
            cover.pixels.iter().all(|p| *p == egui::Color32::from_rgb(40, 100, 160)),
            "the outer bars must be cropped, not squeezed"
        );
        assert!(decode_cover_sized(&bytes, 0).is_none());
    }
}

#[test]
fn square_cover_uv_keeps_cached_rectangles_in_proportion() {
    for size in [[48, 27], [27, 48], [48, 48]] {
        let uv = square_cover_uv(size);
        let width = uv.width() * size[0] as f32;
        let height = uv.height() * size[1] as f32;
        assert!((width - height).abs() < 0.001);
        assert!((uv.center().x - 0.5).abs() < 0.001);
        assert!((uv.center().y - 0.5).abs() < 0.001);
    }
}

#[test]
fn a_decoder_attempt_waits_for_real_progress() {
    const FIRST: u64 = cache::PLAYBACK_BUFFER_BYTES;
    // Nothing has arrived yet.
    assert!(!buffering_attempt_ready(FIRST - 1, FIRST, 0, false));
    // The first attempt goes ahead as soon as the buffer is there.
    assert!(buffering_attempt_ready(FIRST, FIRST, 0, false));
    // Same bytes as the last failed attempt: retrying now would rebuild the
    // decoder every frame.
    assert!(!buffering_attempt_ready(2 * FIRST, 2 * FIRST, 2 * FIRST, false));
    assert!(!buffering_attempt_ready(2 * FIRST, FIRST, 2 * FIRST, false));
    // Enough new data, but not past the (grown) requirement yet.
    assert!(!buffering_attempt_ready(2 * FIRST, 2 * FIRST, FIRST, false));
    // Both conditions met.
    assert!(buffering_attempt_ready(3 * FIRST, 2 * FIRST, FIRST, false));
    // A finished download always gets a final attempt.
    assert!(buffering_attempt_ready(7, u64::MAX, 7, true));
    // Attempts stay logarithmic: a 64 MB track arriving in 64 KB chunks must
    // not be probed on every chunk, which is what the old per-frame gate did.
    let mut downloaded = 0u64;
    let mut attempted_at = 0u64;
    let mut needed = FIRST;
    let mut attempts = 0;
    while downloaded < 64 * 1024 * 1024 {
        downloaded += 64 * 1024;
        if buffering_attempt_ready(downloaded, needed, attempted_at, false) {
            attempts += 1;
            attempted_at = downloaded;
            needed = needed.saturating_mul(2);
        }
    }
    assert!((1..=16).contains(&attempts), "{attempts} decoder attempts for a 64 MB track is not logarithmic");
    assert!(downloaded >= 64 * 1024 * 1024);
}

#[test]
fn repeat_one_does_not_restart_a_track_that_never_played() {
    assert!(should_repeat_one(Repeat::One, 12.0));
    assert!(should_repeat_one(Repeat::One, 0.5));
    // A source that produced no audio at all: advance instead of spinning.
    assert!(!should_repeat_one(Repeat::One, 0.0));
    assert!(!should_repeat_one(Repeat::All, 30.0));
    assert!(!should_repeat_one(Repeat::Off, 30.0));
}

#[test]
fn a_slider_seek_is_sent_only_after_the_pointer_is_released() {
    assert_eq!(due_seek(Some(42.0), true), None, "still dragging");
    assert_eq!(due_seek(Some(42.0), false), Some(42.0));
    assert_eq!(due_seek(None, false), None);
    assert_eq!(due_seek(None, true), None);
}

#[test]
fn clipping_respects_char_boundaries() {
    assert_eq!(clip("привет", 4), "при…");
    assert_eq!(clip("ok", 4), "ok");
}

#[test]
fn button_font_has_every_transport_glyph() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, true);
    ctx.begin_pass(egui::RawInput::default());
    // Inspect the charmap: egui 0.36 has_glyph compares faces, so valid
    // glyphs from the replacement glyph's own face report false.
    let mut missing = Vec::new();
    for (family, glyphs) in [("button", "▶▮◀■⇄↻1♪×"), ("symbols", "⇄↻1")] {
        let font = egui::FontId::new(12.5, egui::FontFamily::Name(family.into()));
        for glyph in glyphs.chars() {
            if !ctx.fonts_mut(|fonts| fonts.fonts.font(&font.family).characters().contains_key(&glyph)) {
                missing.push((family, glyph));
            }
        }
    }
    ctx.end_pass().textures_delta.clear();
    assert!(missing.is_empty(), "missing button glyphs: {missing:?}");
}

#[test]
fn shuffle_order_is_a_permutation_that_contains_the_current_track() {
    let (order, pos) = shuffled_order(8, 3);
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..8).collect::<Vec<_>>());
    assert_eq!(order[pos], 3);
    let (order, pos) = shuffled_order(1, 0);
    assert_eq!(order, vec![0]);
    assert_eq!(pos, 0);
    assert!(shuffled_order(0, 0).0.is_empty());
}

fn local_entry(id: &str, title: &str, artist: &str, album: &str) -> DiskEntry {
    DiskEntry::Local(LocalTrack {
        id: format!("local:{id}.mp3"),
        path: std::path::PathBuf::from(format!("{id}.mp3")),
        rel: format!("{id}.mp3"),
        title: title.into(),
        artist: artist.into(),
        album: album.into(),
        duration: 1.0,
        suffix: "mp3".into(),
        size: 1,
    })
}

#[test]
fn labels_skip_missing_artist_and_album() {
    let bare = local_entry("x", "T", "", "");
    assert_eq!(bare.artist(), "");
    assert_eq!(entry_row_job(&bare, false).text, "T");
    assert_eq!(entry_hover(&bare), "T\nx.mp3");
    let tagged = DiskEntry::Cached(CachedTrack {
        id: "1".into(),
        path: "x".into(),
        title: "T".into(),
        artist: "A".into(),
        album: "B".into(),
        duration: 1.0,
        suffix: "mp3".into(),
        size: 1,
        format: "raw".into(),
    });
    assert_eq!(entry_row_job(&tagged, false).text, format!("T{ROW_SEP}A{ROW_SEP}B"));
    assert_eq!(entry_hover(&tagged), "T — A\nB");
    assert_eq!(texts_job("T", "", "", false).text, "T");
    assert_eq!(texts_job("T", "A", "", false).text, format!("T{ROW_SEP}A"));
    assert_eq!(texts_job("T", "", "B", false).text, format!("T{ROW_SEP}B"));
    assert_eq!(texts_hover("T", "A", "B"), "T — A\nB");
}

#[test]
fn album_cards_wrap_into_rows_in_the_grid() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, true);
    let mut row_bottoms: Vec<i32> = Vec::new();
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 900.0))),
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                album_grid(ui, |ui| {
                    for index in 0..5 {
                        let album = api::Album {
                            id: format!("a{index}"),
                            name: format!("Album {index}"),
                            artist: "Artist".into(),
                            ..Default::default()
                        };
                        let _ = album_card(ui, &album, None, true);
                        row_bottoms.push((ui.min_rect().bottom() / 10.0).round() as i32);
                    }
                });
            });
        },
    );
    output.textures_delta.clear();
    row_bottoms.sort_unstable();
    row_bottoms.dedup();
    assert_eq!(row_bottoms.len(), 2, "cards did not wrap into rows: {row_bottoms:?}");
}

#[test]
fn play_count_word_follows_russian_plurals() {
    for (count, expected) in [
        (1, "1 раз"),
        (2, "2 раза"),
        (4, "4 раза"),
        (5, "5 раз"),
        (11, "11 раз"),
        (12, "12 раз"),
        (21, "21 раз"),
        (22, "22 раза"),
        (25, "25 раз"),
        (101, "101 раз"),
    ] {
        assert_eq!(play_count_label(count), expected);
    }
}

#[test]
fn local_files_form_albums_artists_and_search_hits() {
    let a1 = local_entry("a1", "One", "Band", "Album A");
    let a2 = local_entry("a2", "Two", "Band", "Album A");
    let b1 = local_entry("b1", "Solo", "Soloist", "Album B");
    let untagged = DiskEntry::Local(LocalTrack {
        id: "local:u.mp3".into(),
        path: std::path::PathBuf::from("u.mp3"),
        rel: "u.mp3".into(),
        title: "U".into(),
        artist: String::new(),
        album: String::new(),
        duration: 1.0,
        suffix: "mp3".into(),
        size: 1,
    });
    let entries = vec![a1, a2, b1, untagged];

    let albums = build_local_albums(&entries);
    assert_eq!(albums.len(), 3);
    let band = albums.iter().find(|album| album.artist == "Band").unwrap();
    assert_eq!(band.name, "Album A");
    assert_eq!(band.songs.iter().map(|song| song.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
    assert!(band.id.starts_with(LOCAL_ALBUM_PREFIX));
    assert!(band.to_api_album().duration > 0);
    let unknown = albums.iter().find(|album| album.name == "без альбома").unwrap();
    assert_eq!(unknown.artist, "неизвестный артист");

    let artists = build_local_artists(&albums);
    assert_eq!(
        artists.iter().map(|artist| artist.name.as_str()).collect::<Vec<_>>(),
        ["Band", "Soloist", "неизвестный артист"]
    );
    assert!(artists[0].id.starts_with(LOCAL_ARTIST_PREFIX));
    assert_eq!(artists[0].album_count, 1);

    // Server-downloaded entries stay out of the local albums, but the
    // search can include them when there is no server to search instead.
    let all = [
        entries.clone(),
        vec![DiskEntry::Cached(CachedTrack {
            id: "s1".into(),
            path: "s1.mp3".into(),
            title: "Server Song".into(),
            artist: "Cloud".into(),
            album: "Remote".into(),
            duration: 1.0,
            suffix: "mp3".into(),
            size: 1,
            format: "raw".into(),
        })],
    ]
    .concat();
    assert_eq!(build_local_albums(&all).len(), 3);
    assert_eq!(search_local_matches(&entries, "album a", false, 10).len(), 2);
    assert_eq!(search_local_matches(&entries, "solo", false, 10).len(), 1);
    assert_eq!(search_local_matches(&entries, "", false, 10).len(), 0);
    assert_eq!(search_local_matches(&all, "server song", false, 10).len(), 0);
    assert_eq!(search_local_matches(&all, "server song", true, 10).len(), 1);
    assert_eq!(search_local_matches(&all, "a", true, 2).len(), 2, "limit not applied");
}

#[test]
fn library_rows_merge_server_and_disk_without_duplicates() {
    let local = local_entry("l1", "Local", "L", "LA");
    let cached = DiskEntry::Cached(CachedTrack {
        id: "s1".into(),
        path: "s1.mp3".into(),
        title: "Cached".into(),
        artist: "C".into(),
        album: "CA".into(),
        duration: 1.0,
        suffix: "mp3".into(),
        size: 2,
        format: "raw".into(),
    });
    let server = vec![
        api::Song {
            id: "s1".into(),
            title: "Cached".into(),
            artist: "C".into(),
            album: "CA".into(),
            ..Default::default()
        },
        api::Song {
            id: "s2".into(),
            title: "Stream".into(),
            artist: "B".into(),
            album: "BA".into(),
            duration: 5.0,
            ..Default::default()
        },
    ];
    let rows = build_library_rows(&[local, cached], &server);
    assert_eq!(rows.len(), 3, "a cached server song must not be duplicated");
    assert_eq!(rows.iter().map(LibRow::id).collect::<Vec<_>>(), ["s2", "s1", "local:l1.mp3"]);
    let streamed = rows.iter().find(|row| row.id() == "s2").unwrap();
    assert!(streamed.is_server() && !streamed.is_local() && !streamed.is_cached());
    assert!(streamed.size().is_none(), "server rows show duration, not size");
    assert_eq!(streamed.duration(), 5.0);
    assert_eq!(rows.iter().find(|row| row.id() == "s1").unwrap().size(), Some(2));
}

#[test]
fn the_play_button_is_not_a_pause_button_for_a_restored_track() {
    // Restored session: a track exists, but nothing is loaded yet.
    assert_eq!(transport_play_label(true, false, false), ("▶", "продолжить"));
    assert_eq!(transport_play_label(false, false, false), ("▶", "начать воспроизведение"));
    // Really playing and really paused.
    assert_eq!(transport_play_label(true, true, false), ("▮▮", "пауза"));
    assert_eq!(transport_play_label(true, true, true), ("▶", "продолжить"));
}

#[test]
fn a_song_found_by_the_filter_keeps_its_place_in_the_library() {
    let rows = vec![
        LibRow::Server(api::Song { id: "a".into(), ..Default::default() }),
        LibRow::Server(api::Song { id: "b".into(), ..Default::default() }),
        LibRow::Server(api::Song { id: "c".into(), ..Default::default() }),
    ];
    assert_eq!(library_queue_start(&rows, "b"), Some(1), "next must continue in the library");
    assert_eq!(library_queue_start(&rows, "unknown"), None);
}

#[test]
fn play_from_idle_uses_the_visible_library_list() {
    let rows = Arc::new(vec![LibRow::Server(api::Song { id: "a".into(), ..Default::default() })]);
    let filtered = Arc::new(vec![LibRow::Server(api::Song { id: "b".into(), ..Default::default() })]);
    assert!(Arc::ptr_eq(idle_start_rows(View::Library, &rows, &filtered), &filtered));
    let empty = Arc::new(Vec::new());
    assert!(
        Arc::ptr_eq(idle_start_rows(View::Library, &rows, &empty), &rows),
        "an empty filter must fall back to the full library"
    );
    assert!(Arc::ptr_eq(idle_start_rows(View::Albums, &rows, &filtered), &rows));
}

#[test]
fn most_played_orders_by_count_and_honours_the_limit() {
    let dir = std::env::temp_dir().join(format!(
        "beat-top-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let stats = stats::Stats::load_from(dir.join("play-stats.json"));
    let a = local_entry("a", "Alpha", "X", "");
    let b = local_entry("b", "Bravo", "X", "");
    let c = local_entry("c", "Charlie", "X", "");
    stats.increment("local:b.mp3");
    stats.increment("local:b.mp3");
    stats.increment("local:a.mp3");
    let list = top_played(&[a.clone(), b.clone(), c.clone()], &stats, 10);
    assert_eq!(list.iter().map(|(entry, _)| entry.id()).collect::<Vec<_>>(), ["local:b.mp3", "local:a.mp3"]);
    assert_eq!(list[0].1, 2);
    assert_eq!(top_played(&[a, b, c], &stats, 1).len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cached_entry_roundtrips_into_a_song() {
    let entry = CachedTrack {
        id: "7".into(),
        title: "T".into(),
        artist: "A".into(),
        album: "B".into(),
        duration: 12.0,
        suffix: "flac".into(),
        path: "x".into(),
        size: 1,
        format: "raw".into(),
    };
    let song = cached_to_song(&entry);
    assert_eq!(song.id, "7");
    assert_eq!(song.suffix, "flac");
    assert_eq!(song.artist, "A");
}

#[test]
fn clicking_download_album_collects_a_command_without_starting_work_while_drawing() {
    let ctx = egui::Context::default();
    let mut app = BeatApp::preview(&ctx);
    let albums = Arc::new(vec![api::Album { id: "server-album".into(), name: "Album".into(), ..Default::default() }]);
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 600.0))),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input(), |ui| {
        app.ui_album_grid(ui, "", Some(albums.clone()));
    });
    let center = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "↓" => Some(text.visual_bounding_rect().center()),
            _ => None,
        })
        .expect("download widget was not rendered");
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
    let mut output = ctx.run_ui(raw, |ui| {
        app.ui_album_grid(ui, "", Some(albums.clone()));
    });
    output.textures_delta.clear();
    assert!(matches!(app.commands.as_slice(), [Command::DownloadAlbum(album)] if album.id == "server-album"));
    assert!(app.pending_download.is_none());
    assert!(app.downloads.is_empty() && app.download_queue.is_empty());
    app.dispatch_commands(&ctx);
    assert!(app.commands.is_empty(), "preview left a command pending");
    assert!(app.pending_download.is_none(), "preview launched a real request");
}

/// The reported layout bug: the connection error was drawn inside the button
/// row, so a sentence-long message ran under the save and cancel buttons that
/// the right-aligned block placed on top of it. It now wraps on its own rows
/// above the buttons.
///
/// The footer is rendered directly rather than through the settings window:
/// a headless pass sizes a window from the first frame it ever sees, which
/// clips the modal and would hide the very rows under test.
#[test]
fn a_long_settings_error_never_overlaps_the_settings_buttons() {
    let error = "адрес сервера должен быть HTTPS (HTTP допустим только для localhost и адресов локальной сети) \
                  и без логина, параметров или фрагмента";
    // The width the settings window gives its content, and a narrower one.
    for width in [620.0_f32, 420.0] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, true);
        let failed = Err(error.to_owned());
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 900.0))),
                ..Default::default()
            },
            |ui| {
                ui.set_width(width);
                settings_footer(ui, false, Some(&failed), None);
            },
        );
        output.textures_delta.clear();
        let texts: Vec<(String, egui::Rect, usize)> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => {
                    Some((text.galley.text().to_owned(), text.visual_bounding_rect(), text.galley.rows.len()))
                }
                _ => None,
            })
            .collect();
        let (error_rect, rows) = texts
            .iter()
            .find(|(text, _, _)| text == error)
            .map(|(_, rect, rows)| (*rect, *rows))
            .unwrap_or_else(|| panic!("width {width}: the error was not rendered"));
        // A sentence this long cannot fit on one row of the settings window.
        assert!(rows > 1, "width {width}: the error did not wrap: {error_rect:?}");
        for button in ["[ ПРОВЕРИТЬ СВЯЗЬ ]", "[ СОХРАНИТЬ ]", "[ ОТМЕНА ]"] {
            let rect = texts
                .iter()
                .find(|(text, _, _)| text == button)
                .map(|(_, rect, _)| *rect)
                .unwrap_or_else(|| panic!("width {width}: {button} was not rendered"));
            assert!(
                error_rect.max.y <= rect.min.y,
                "width {width}: {button} at {rect:?} sits on the error at {error_rect:?}"
            );
        }
    }
}

/// The same footer with a saved-but-refused config, which reports its own
/// second error next to the connection result.
#[test]
fn both_settings_messages_wrap_above_the_button_row() {
    let error = "соединение закрыто: ошибка ввода-вывода";
    let save_error = "дождитесь окончания загрузок перед сменой сервера, формата или папки кеша";
    let ctx = egui::Context::default();
    theme::apply(&ctx, true);
    let failed = Err(error.to_owned());
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(620.0, 900.0))),
            ..Default::default()
        },
        |ui| {
            ui.set_width(620.0);
            settings_footer(ui, false, Some(&failed), Some(save_error));
        },
    );
    output.textures_delta.clear();
    let texts: Vec<(String, egui::Rect)> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some((text.galley.text().to_owned(), text.visual_bounding_rect())),
            _ => None,
        })
        .collect();
    let rect_of = |want: &str| {
        texts
            .iter()
            .find(|(text, _)| text == want)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("{want} was not rendered"))
    };
    let save = rect_of("[ СОХРАНИТЬ ]");
    assert!(rect_of(error).max.y <= save.min.y, "the connection error reaches into the buttons: {save:?}");
    let note = rect_of(&format!("конфиг не сохранён: {save_error}"));
    assert!(note.max.y <= save.min.y, "the save error reaches into the buttons: {save:?}");
    // Both messages stack above each other, not on one row.
    assert!(note.min.y >= rect_of(error).max.y, "the two messages share a row");
}
