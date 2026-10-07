use super::*;

impl BeatApp {
    pub(super) fn spawn_download(&mut self, song: api::Song) {
        let Some(client) = self.client.clone() else { return };
        let label = format_label(self.cfg.stream_format);
        let handle =
            cache::start_download((*client).clone(), self.cache.clone(), song.clone(), label, self.dl_tx.clone());
        self.downloads.insert(song.id, handle);
    }
    pub(super) fn enqueue_download(&mut self, song: api::Song) {
        if local::is_local_id(&song.id) {
            self.notice = Some(crate::i18n::trf!("файл уже лежит в папке кеша: {}", song.title));
            return;
        }
        if self.cache.contains(&song.id) {
            self.notice = Some(crate::i18n::trf!("уже в кеше: {}", song.title));
            return;
        }
        if self.offer_download(song, false) {
            self.notice = Some(crate::i18n::tr("добавлено в загрузки").into());
        }
    }
    pub(super) fn offer_download(&mut self, song: api::Song, automatic: bool) -> bool {
        if !automatic {
            self.auto_queued.remove(&song.id);
        }
        if song.id.is_empty()
            || local::is_local_id(&song.id)
            || self.cache.contains(&song.id)
            || self.downloads.contains_key(&song.id)
            || self.download_queue.iter().any(|queued| queued.id == song.id)
        {
            return false;
        }
        if automatic {
            self.auto_queued.insert(song.id.clone());
        }
        self.download_queue.push_back(song);
        true
    }
    pub(super) fn enqueue_album(&mut self, songs: &[api::Song]) {
        let mut added = 0;
        for song in songs {
            if self.offer_download(song.clone(), false) {
                added += 1;
            }
        }
        self.notice = Some(if added > 0 {
            crate::i18n::trf!("в загрузки добавлено треков: {added}", added = added)
        } else {
            crate::i18n::tr("все треки уже скачаны").into()
        });
    }
    pub(super) fn download_album(&mut self, album: api::Album) {
        if album.id.starts_with(LOCAL_ALBUM_PREFIX) {
            self.notice = Some(crate::i18n::tr("этот альбом уже лежит в папке кеша").into());
            return;
        }
        if self.album_open.as_ref().is_some_and(|(open, _)| open.id == album.id) {
            let songs = self.album_open.as_ref().map(|(_, songs)| songs.clone()).unwrap_or_default();
            self.enqueue_album(&songs);
        } else {
            self.pending_play = None;
            self.pending_download = Some(album.id.clone());
            self.open_album(album);
        }
    }
}
