use super::*;

impl BeatApp {
    pub(super) fn cover_texture(&mut self, cover_id: &str, px: u32) -> Option<egui::TextureHandle> {
        if cover_id.is_empty() {
            return None;
        }
        // The same artwork is drawn at two sizes (24 px list rows, 168 px album
        // cards), so the in-memory key carries the requested size: one texture
        // cannot serve both, and sharing the small one would upscale it.
        let key = if cover_id.starts_with(FILE_COVER_PREFIX) {
            format!("{cover_id}#{px}#{}", self.disk_cover_generation)
        } else {
            format!("{cover_id}#{px}")
        };
        if let Some(texture) = self.covers.get(&key) {
            return Some(texture.clone());
        }
        if self.demo.is_some() {
            return None;
        }
        if self.cover_pending.contains(&key) {
            return None;
        }
        // At the limit the cover is simply asked for again on a later frame.
        if self.cover_inflight >= MAX_COVER_JOBS {
            return None;
        }
        // Files on disk (local drops and finished downloads): the artwork is
        // extracted from the file itself, so this works offline. Extracted
        // thumbnails are cached on disk, so unchanged files are not re-read
        // on every launch.
        if let Some(rel) = cover_id.strip_prefix(FILE_COVER_PREFIX) {
            self.cover_pending.insert(key.clone());
            self.cover_inflight += 1;
            let cache = self.cache.clone();
            let roots = self.cfg.local_roots();
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            let rel = rel.to_owned();
            std::thread::spawn(move || {
                let image = local::resolve(&format!("{}{}", local::LOCAL_ID_PREFIX, rel), cache.root(), &roots)
                    .and_then(|path| {
                        let cache_key = crate::covers::file_key(&rel, &path, px);
                        if let Some(cache_key) = &cache_key {
                            if let Some(image) = crate::covers::load(cache_key) {
                                return Some(image);
                            }
                        }
                        let image = local::embedded_cover(&path).and_then(|bytes| decode_cover_sized(&bytes, px));
                        if let (Some(cache_key), Some(image)) = (&cache_key, &image) {
                            crate::covers::store(cache_key, image);
                        }
                        image
                    });
                let _ = tx.send(CoverEvent::Loaded(generation, key, image));
            });
            return None;
        }
        if let Some(client) = self.client.clone() {
            self.cover_pending.insert(key.clone());
            self.cover_inflight += 1;
            let tx = self.cover_tx.clone();
            let generation = self.cover_generation;
            let cover_id = cover_id.to_owned();
            std::thread::spawn(move || {
                let cache_key = crate::covers::server_key(&client.catalog_key(), &cover_id, px);
                let image = crate::covers::load(&cache_key).or_else(|| {
                    let image = client.cover_bytes(&cover_id, px).ok().and_then(|bytes| decode_cover_sized(&bytes, px));
                    if let Some(image) = &image {
                        crate::covers::store(&cache_key, image);
                    }
                    image
                });
                let _ = tx.send(CoverEvent::Loaded(generation, key, image));
            });
        }
        None
    }
}
/// A small file can describe a gigantic picture: cap the dimensions and the
/// memory before anything is allocated for it.
#[allow(clippy::field_reassign_with_default)]
pub(super) fn cover_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_COVER_SIDE);
    limits.max_image_height = Some(MAX_COVER_SIDE);
    limits.max_alloc = Some(MAX_COVER_DECODE_BYTES);
    limits
}

pub(super) fn decode_cover_sized(bytes: &[u8], size: u32) -> Option<egui::ColorImage> {
    pub(super) const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;
    if bytes.is_empty() || bytes.len() > MAX_COVER_BYTES {
        return None;
    }
    HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        reader.limits(cover_limits());
        let image = reader.decode().ok()?;
        // Crop a borrowed view BEFORE resizing. Resizing a very wide image to
        // fill a square first could allocate a huge intermediate bitmap.
        let (width, height) = (image.width(), image.height());
        let side = width.min(height);
        if side == 0 || size == 0 {
            return None;
        }
        let cropped = image::imageops::crop_imm(&image, (width - side) / 2, (height - side) / 2, side, side);
        let thumb = image::imageops::thumbnail(&*cropped, size, size);
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [thumb.width() as usize, thumb.height() as usize],
            thumb.as_raw(),
        ))
    }));
    HANDLED_PANIC.with(|handled| handled.set(false));
    result.ok().flatten()
}
