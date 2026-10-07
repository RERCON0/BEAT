use super::*;

/// Reads the index. A missing file is a fresh cache; an unreadable, oversized
/// or damaged one is set aside (never silently replaced by the next save), and
/// entries whose path leaves the cache folder are dropped: the index sits in a
/// folder that may be synced or shared, and its paths are used for deletion.
pub(super) fn load_index(root: &Path, stamp: u128) -> LoadedIndex {
    load_index_file(root, stamp, INDEX_FILE)
}

pub(super) fn profile_index_name(profile: &str) -> String {
    format!(".beat-index-{}.json", api::md5_hex(profile))
}

pub(super) fn load_index_file(root: &Path, stamp: u128, name: &str) -> LoadedIndex {
    let path = root.join(name);
    let parsed = match read_capped(&path, MAX_INDEX_BYTES) {
        Ok(None) => return LoadedIndex { tracks: HashMap::new(), warning: None, save_blocked: false },
        Ok(Some(raw)) => serde_json::from_slice::<IndexFile>(&raw).map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match parsed {
        Ok(file) if file.version == 1 => {
            let total = file.tracks.len();
            let tracks: HashMap<String, CachedTrack> = file
                .tracks
                .into_iter()
                .filter(|(id, entry)| {
                    id == &entry.id
                        && crate::api::remote_id(id)
                        && !matches!(checked_path(root, &entry.path), Err(PathError::Unsafe))
                })
                .collect();
            let dropped = total - tracks.len();
            let warning = (dropped > 0).then(|| {
                crate::i18n::trf!("в индексе кеша пропущено записей с небезопасным путём: {dropped}", dropped = dropped)
            });
            LoadedIndex { tracks, warning, save_blocked: false }
        }
        _ => match set_aside(&path, stamp) {
            Some(name) => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some(crate::i18n::trf!(
                    "индекс кеша повреждён и начат заново; старый файл сохранён как {name}",
                    name = name
                )),
                save_blocked: false,
            },
            None => LoadedIndex {
                tracks: HashMap::new(),
                warning: Some(
                    crate::i18n::tr(
                        "индекс кеша повреждён; копию сделать не удалось, сохранение индекса заблокировано",
                    )
                    .into(),
                ),
                save_blocked: true,
            },
        },
    }
}

/// Renames an unreadable index to `.beat-index.corrupt-<stamp>.bak`; `None`
/// when that failed or the name is taken.
pub(super) fn set_aside(path: &Path, stamp: u128) -> Option<String> {
    let backup = path.with_file_name(format!(".beat-index.corrupt-{stamp}.bak"));
    if backup.exists() {
        return None;
    }
    std::fs::rename(path, &backup).ok()?;
    backup.file_name().map(|name| name.to_string_lossy().into_owned())
}

/// Reads at most `cap` bytes; a bigger file is an error, checked before
/// anything is loaded into memory. `Ok(None)` = no such file.
pub(super) fn read_capped(path: &Path, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() > cap {
        return Err(std::io::Error::other(crate::i18n::tr("файл индекса слишком большой")));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(std::io::Error::other(crate::i18n::tr("файл индекса слишком большой")));
    }
    Ok(Some(bytes))
}
