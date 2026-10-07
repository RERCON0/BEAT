use super::*;

/// A `/`-separated relative path that stays inside the cache root: no empty,
/// `.` or `..` parts, no drive/absolute forms, no characters Windows rejects,
/// no trailing dot or space (Windows strips those, so the name would differ).
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !is_device_name(part)
                && !part.to_ascii_lowercase().starts_with(".beat-index")
                && !part.to_ascii_lowercase().ends_with(".part")
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        })
}

/// Refuse existing symlinks and Windows junctions at every level: a lexical
/// path inside the cache can otherwise resolve outside it when read or deleted.
pub(crate) fn safe_path(root: &Path, rel: &str) -> Option<PathBuf> {
    checked_path(root, rel).ok()
}

pub(super) fn checked_path(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    if !is_safe_rel(rel) {
        return Err(PathError::Unsafe);
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(PathError::Unsafe);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err(PathError::Unsafe);
                    } // FILE_ATTRIBUTE_REPARSE_POINT
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PathError::Unavailable),
        }
    }
    Ok(path)
}

pub fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(".part");
    path.with_file_name(name)
}

/// Cheap "is this audio at all" probe on the first bytes of a finished
/// download. Only container magic is looked for, so an unsupported codec may
/// still be cached, while an error page or a `200 OK` carrying "Bad Gateway"
/// is refused. The window is
/// a few kilobytes because an ID3v2 tag or a small amount of leading junk can
/// sit before the first real frame.
pub(super) fn looks_like_audio(path: &Path) -> Result<(), String> {
    use std::io::Read;
    pub(super) const WINDOW: usize = 4096;
    let mut file = std::fs::File::open(path).map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    crate::media::validate_metadata(&mut file).map_err(|e| format!("метаданные: {e}"))?;
    let mut head = vec![0u8; WINDOW];
    let read = file.read(&mut head).map_err(|e| format!("не удалось прочитать кеш: {e}"))?;
    let head = &head[..read];
    let textual =
        head.windows(3).any(|w| w == b"ID3") || head.windows(4).any(|w| w == b"fLaC" || w == b"OggS" || w == b"ftyp");
    let riff = head.windows(12).any(|w| &w[..4] == b"RIFF" && &w[8..12] == b"WAVE");
    // Matroska/WebM, and the MPEG frame / ADTS sync word.
    let binary = head.windows(4).any(|w| w == [0x1a, 0x45, 0xdf, 0xa3])
        || head.windows(2).any(|w| w[0] == 0xff && w[1] & 0xe0 == 0xe0);
    if textual || riff || binary {
        Ok(())
    } else {
        Err("сервер прислал не аудиофайл".into())
    }
}

/// Windows counts a path in UTF-16 units, so the budget has to be counted the
/// same way: an emoji is two units, not one.
pub(super) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Cuts `text` to at most `max` UTF-16 units, never splitting a character.
pub(super) fn truncate_utf16(text: &str, max: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if used + width > max {
            return &text[..index];
        }
        used += width;
    }
    text
}

/// `Artist/Album/NN - Title.ext`, sanitized for Windows file names and short
/// enough that root + this stays inside `MAX_ABSOLUTE_PATH`. Tags are attacker
/// -controlled data: three 200-character components would otherwise exceed
/// `MAX_PATH` and fail the download with a confusing "cannot create" error.
pub(super) fn relative_path(song: &Song, suffix: &str, budget: usize) -> PathBuf {
    let suffix = suffix.trim_start_matches('.');
    let track_prefix = if song.track > 0 { format!("{:02} - ", song.track) } else { String::new() };
    // Everything that is not one of the three free-text components.
    let overhead = utf16_len(&track_prefix) + 1 /* '.' */ + utf16_len(suffix)
        + 2 /* artist/album separators */ + PATH_HEADROOM;
    let room = budget.saturating_sub(overhead);
    let mut artist = sanitize_component(&song.artist, "Неизвестный артист", MAX_COMPONENT);
    let mut album = sanitize_component(&song.album, "Без альбома", MAX_COMPONENT);
    let mut title = sanitize_component(&song.title, "Трек", MAX_COMPONENT);
    // Shrink the longest component until the three fit the room; the titles
    // that need shortening are exactly the ones the user would recognise.
    let mut excess =
        [utf16_len(&artist), utf16_len(&album), utf16_len(&title)].into_iter().sum::<usize>().saturating_sub(room);
    while excess > 0 {
        let longest = [utf16_len(&artist), utf16_len(&album), utf16_len(&title)]
            .iter()
            .enumerate()
            .max_by_key(|(_, len)| **len)
            .map(|(index, _)| index);
        let Some(index) = longest else { break };
        let slot = match index {
            0 => &mut artist,
            1 => &mut album,
            _ => &mut title,
        };
        let current = utf16_len(slot);
        if current == 0 {
            // Nothing left to cut: the fixed parts alone exceed the budget.
            // Truncating further would produce an empty component, and
            // `sanitize_component` already replaced those with a fallback.
            break;
        }
        let keep = current.saturating_sub(excess);
        *slot = truncate_utf16(slot, keep).trim_end().to_owned();
        excess = excess.saturating_sub(current - utf16_len(slot));
    }
    let title = if track_prefix.is_empty() { title } else { format!("{track_prefix}{title}") };
    PathBuf::from(artist).join(album).join(format!("{title}.{suffix}"))
}

pub(super) fn numbered_path(rel: &str, counter: u32) -> String {
    let path = std::path::Path::new(rel);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match path.extension().map(|s| s.to_string_lossy().into_owned()) {
        Some(ext) => {
            let parent = path.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if parent.is_empty() {
                format!("{stem} ({counter}).{ext}")
            } else {
                format!("{parent}/{stem} ({counter}).{ext}")
            }
        }
        None => format!("{rel} ({counter})"),
    }
}

/// One path component from a server tag: Windows-safe, never empty, and at most
/// `max` UTF-16 units long (NTFS allows 255; the rest is room for the collision
/// suffix and the `.part` file that are appended later).
pub fn sanitize_component(raw: &str, fallback: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
        if utf16_len(&out) >= max {
            break;
        }
    }
    let mut out = truncate_utf16(&out, max).trim().trim_end_matches(['.', ' ']).to_owned();
    if is_device_name(&out) {
        out.insert(0, '_');
    }
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    }
}

/// Windows reserves these names (with any extension) for devices; a file or
/// folder called like that cannot be created.
pub(super) fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")).is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        })
}
