use super::*;

impl BeatApp {
    pub(super) fn cancel_catalog_scan(&mut self) {
        if let Some(scan) = self.catalog_scan.take() {
            scan.cancel.store(true, Ordering::Relaxed);
            // Dropping the receiver releases a worker blocked on the bounded
            // channel, even if the network request is still finishing.
            if scan.mode == catalog::Mode::Library {
                self.library_pending.clear();
            }
        }
    }
    pub(super) fn start_catalog_scan(&mut self, mode: catalog::Mode) {
        if self.catalog_scan.is_some() || (mode == catalog::Mode::Library && self.library_save_pending) {
            return;
        }
        let Some(client) = self.require_client() else { return };
        let (tx, rx) = sync_channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        catalog::spawn(client, self.cache.clone(), mode, tx, cancel.clone());
        self.catalog_scan = Some(CatalogScan { rx, cancel, mode, baseline: false, albums: 0, added: 0 });
        if mode == catalog::Mode::Library {
            self.library_pending = Vec::new();
            self.notice = Some(crate::i18n::tr("загружаю библиотеку Navidrome…").into());
        } else {
            self.notice = Some(crate::i18n::tr("проверяю библиотеку Navidrome…").into());
        }
    }
    pub(super) fn maybe_start_automatic_scan(&mut self) {
        if self.cfg.auto_cache_new
            && self.client.is_some()
            && self.catalog_scan.is_none()
            && std::time::Instant::now() >= self.next_catalog_check
        {
            self.start_catalog_scan(catalog::Mode::Automatic);
        }
    }
    /// Fills the unified library from the server; uses the on-disk cache when
    /// it is already there and `force` is false (opening the view).
    pub(super) fn refresh_server_library(&mut self, force: bool) {
        if self.client.is_none() || self.catalog_scan.is_some() {
            return;
        }
        if !force && !self.server_songs.is_empty() {
            return;
        }
        self.start_catalog_scan(catalog::Mode::Library);
    }
    /// A finished full server walk replaces the list and persists it, so the
    /// next launch opens instantly. A failed walk keeps the previous list.
    pub(super) fn finish_library_scan(&mut self, result: Result<(), String>, albums: usize) {
        match result {
            Ok(()) => {
                let songs = std::mem::take(&mut self.library_pending);
                let count = songs.len();
                self.set_server_songs(songs);
                if self.cfg.auto_cache_new {
                    // The walk just read everything; postpone the auto pass.
                    self.next_catalog_check = std::time::Instant::now() + catalog::POLL_EVERY;
                }
                self.notice = Some(crate::i18n::trf!("библиотека обновлена: {count} треков с сервера", count = count));
                if let Some(client) = self.client.clone() {
                    let songs = self.server_songs.clone();
                    let tx = self.lib_tx.clone();
                    self.library_save_pending = true;
                    std::thread::spawn(move || {
                        let result = catalog::save_library(&client, &songs);
                        let _ = tx.send(LibEvent::LibrarySaved(client.catalog_key(), result));
                    });
                }
            }
            Err(err) => {
                self.library_pending.clear();
                self.notice = Some(crate::i18n::trf!(
                    "не удалось обновить библиотеку после {albums} альбомов: {err}",
                    albums = albums,
                    err = err
                ));
            }
        }
    }
    pub(super) fn pump_catalog(&mut self) {
        let library_scan = self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Library);
        let mut events = Vec::new();
        let mut proposed = 0;
        while events.len() < 512 && (library_scan || self.download_queue.len() + proposed < catalog::MAX_QUEUED) {
            let Some(scan) = &self.catalog_scan else { break };
            match scan.rx.try_recv() {
                Ok(event) => {
                    let finished = matches!(event, catalog::Event::Finished(_));
                    if !library_scan && matches!(event, catalog::Event::Song(_)) {
                        proposed += 1;
                    }
                    events.push(event);
                    if finished {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    events.push(catalog::Event::Finished(Err(crate::i18n::tr(
                        "проверка библиотеки неожиданно прервалась",
                    )
                    .into())));
                    break;
                }
            }
        }
        for event in events {
            match event {
                catalog::Event::Started { baseline } => {
                    if let Some(scan) = &mut self.catalog_scan {
                        scan.baseline = baseline;
                    }
                }
                catalog::Event::AlbumScanned => {
                    if let Some(scan) = &mut self.catalog_scan {
                        scan.albums += 1;
                    }
                }
                catalog::Event::Song(song) => {
                    if library_scan {
                        // Metadata only: the list is filled when the walk ends.
                        self.library_pending.push(song);
                    } else {
                        let automatic =
                            self.catalog_scan.as_ref().is_some_and(|scan| scan.mode == catalog::Mode::Automatic);
                        let added = self.offer_download(song, automatic);
                        if added {
                            if let Some(scan) = &mut self.catalog_scan {
                                scan.added += 1;
                            }
                        }
                    }
                }
                catalog::Event::Finished(result) => {
                    if let Some(scan) = self.catalog_scan.take() {
                        if scan.mode == catalog::Mode::Library {
                            self.finish_library_scan(result, scan.albums);
                            continue;
                        }
                        self.next_catalog_check = std::time::Instant::now()
                            + if result.is_err() {
                                catalog::RETRY_AFTER
                            } else if scan.mode == catalog::Mode::All && self.cfg.auto_cache_new {
                                std::time::Duration::ZERO
                            } else {
                                catalog::POLL_EVERY
                            };
                        self.notice = Some(match result {
                            Ok(()) if scan.baseline => crate::i18n::trf!(
                                "запомнено альбомов: {}; новые песни будут скачиваться автоматически",
                                scan.albums
                            ),
                            Ok(()) => {
                                crate::i18n::trf!(
                                    "проверено альбомов: {}; добавлено в загрузки: {}",
                                    scan.albums,
                                    scan.added
                                )
                            }
                            Err(err) => crate::i18n::trf!(
                                "проверка библиотеки остановлена после {} альбомов (добавлено: {}): {err}",
                                scan.albums,
                                scan.added,
                                err = err
                            ),
                        });
                    }
                }
            }
        }
    }
}
