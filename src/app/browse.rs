use super::*;

impl BeatApp {
    pub(super) fn refresh_albums(&mut self, kind: &str, title: &str) {
        if self.client.is_none() {
            // Local-only mode: show the albums assembled from the cache folder.
            let mut albums: Vec<api::Album> = self.local_albums.iter().map(LocalAlbum::to_api_album).collect();
            let local_title = if kind == "random" {
                shuffle_albums(&mut albums);
                crate::i18n::tr("СЛУЧАЙНЫЕ ЛОКАЛЬНЫЕ АЛЬБОМЫ")
            } else {
                crate::i18n::tr("ЛОКАЛЬНЫЕ АЛЬБОМЫ")
            };
            self.album_list = Arc::new(albums);
            self.album_list_title = local_title.to_owned();
            self.view = View::Albums;
            self.loading = None;
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.album_list_title = title.to_owned();
        let tx = self.lib_tx.clone();
        let kind = kind.to_owned();
        std::thread::spawn(move || {
            let result = client.album_list(&kind, 60, 0);
            let _ = tx.send(LibEvent::AlbumList(id, result));
        });
    }
    pub(super) fn fetch_artists(&mut self) {
        if self.client.is_none() {
            // Local-only mode: the combined list already holds local artists.
            self.view = View::Artists;
            return;
        }
        let Some(client) = self.client.clone() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.artists();
            let _ = tx.send(LibEvent::Artists(id, result));
        });
    }
    pub(super) fn open_artist(&mut self, artist: api::Artist) {
        if artist.id.starts_with(LOCAL_ARTIST_PREFIX) {
            let albums: Vec<api::Album> = self
                .local_albums
                .iter()
                .filter(|local| local.artist == artist.name)
                .map(LocalAlbum::to_api_album)
                .collect();
            self.loading = None;
            self.view = View::Artist;
            self.artist_open = Some((artist, Arc::new(albums)));
            return;
        }
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Artist;
        self.artist_open = Some((artist.clone(), Arc::new(Vec::new())));
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.artist(&artist.id);
            let _ = tx.send(LibEvent::Artist(id, result));
        });
    }
    /// Opens an album on the user's click. An auto-play/download requested for
    /// an album that was never opened must not fire when it is opened later.
    pub(super) fn browse_album(&mut self, album: api::Album) {
        self.pending_play = None;
        self.pending_download = None;
        if self.open_local_album(&album) {
            return;
        }
        self.open_album(album);
    }
    /// Opens an album assembled from hand-dropped files; `false` means the id
    /// belongs to the server and the caller must fetch it.
    pub(super) fn open_local_album(&mut self, album: &api::Album) -> bool {
        if !album.id.starts_with(LOCAL_ALBUM_PREFIX) {
            return false;
        }
        match self.local_albums.iter().find(|local| local.id == album.id) {
            Some(local) => {
                self.view = View::Album;
                self.loading = None;
                self.album_open = Some((album.clone(), Arc::new(local.songs.clone())));
            }
            None => self.notice = Some(crate::i18n::tr("альбом не найден — обновите список на диске").into()),
        }
        true
    }
    pub(super) fn open_album(&mut self, album: api::Album) {
        let Some(client) = self.require_client() else { return };
        let id = self.request_id();
        self.loading = Some(id);
        self.view = View::Album;
        self.album_open = Some((album.clone(), Arc::new(Vec::new())));
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.album(&album.id);
            let _ = tx.send(LibEvent::Album(id, result));
        });
    }
    pub(super) fn run_search(&mut self) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.view = View::Search;
        let Some(client) = self.client.clone() else {
            // Local-only mode: matches are computed live from the disk list.
            self.search_result = None;
            return;
        };
        let id = self.request_id();
        self.loading = Some(id);
        let tx = self.lib_tx.clone();
        std::thread::spawn(move || {
            let result = client.search(&query);
            let _ = tx.send(LibEvent::Search(id, result));
        });
    }
}
