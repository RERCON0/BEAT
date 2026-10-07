//! Desktop SMTC commands cross a bounded mailbox; callbacks never mutate the UI.
use crate::api::Song;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    Seek(f64),
}

pub struct Controls {
    #[cfg(windows)]
    inner: souvlaki::MediaControls,
    receiver: std::sync::mpsc::Receiver<Command>,
    song: Option<String>,
    state: Option<(bool, bool, u64)>,
}

impl Controls {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, String> {
        #[cfg(windows)]
        {
            use raw_window_handle::HasWindowHandle;
            let raw_window_handle::RawWindowHandle::Win32(window) =
                cc.window_handle().map_err(|e| e.to_string())?.as_raw()
            else {
                return Err("SMTC: unsupported window handle".into());
            };
            Self::for_window(window.hwnd.get() as *mut std::ffi::c_void, cc.egui_ctx.clone())
        }
        #[cfg(not(windows))]
        {
            let _ = cc;
            Err("Windows media controls are unavailable".into())
        }
    }

    #[cfg(windows)]
    fn for_window(hwnd: *mut std::ffi::c_void, ctx: eframe::egui::Context) -> Result<Self, String> {
        let mut inner = souvlaki::MediaControls::new(souvlaki::PlatformConfig {
            display_name: "BEAT",
            dbus_name: "beat",
            hwnd: Some(hwnd),
        })
        .map_err(|e| e.to_string())?;
        let (tx, receiver) = std::sync::mpsc::sync_channel(32);
        inner
            .attach(move |event| {
                if let Some(command) = command(event) {
                    if tx.try_send(command).is_ok() {
                        ctx.request_repaint();
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, receiver, song: None, state: None })
    }

    pub fn poll(&self) -> Option<Command> {
        self.receiver.try_recv().ok()
    }

    pub fn update(&mut self, song: Option<&Song>, loaded: bool, playing: bool, position: f64) -> Result<(), String> {
        #[cfg(windows)]
        {
            let key = song.map(|song| song.id.clone());
            if key != self.song {
                let metadata = song
                    .map(|song| souvlaki::MediaMetadata {
                        title: Some(&song.title),
                        artist: Some(&song.artist),
                        album: Some(&song.album),
                        duration: duration(song.duration),
                        cover_url: None,
                    })
                    .unwrap_or_else(|| souvlaki::MediaMetadata {
                        title: Some(""),
                        artist: Some(""),
                        album: Some(""),
                        ..Default::default()
                    });
                self.inner.set_metadata(metadata).map_err(|e| e.to_string())?;
                self.song = key;
            }
            let position = duration(position).unwrap_or_default();
            let state = (loaded, playing, position.as_secs());
            if self.state != Some(state) {
                let progress = Some(souvlaki::MediaPosition(position));
                let playback = if !loaded {
                    souvlaki::MediaPlayback::Stopped
                } else if playing {
                    souvlaki::MediaPlayback::Playing { progress }
                } else {
                    souvlaki::MediaPlayback::Paused { progress }
                };
                self.inner.set_playback(playback).map_err(|e| e.to_string())?;
                self.state = Some(state);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (song, loaded, playing, position, &self.song, &self.state);
        }
        Ok(())
    }
}

fn duration(seconds: f64) -> Option<Duration> {
    if seconds.is_finite() {
        Duration::try_from_secs_f64(seconds.clamp(0.0, crate::api::MAX_DURATION_SECS)).ok()
    } else {
        None
    }
}

#[cfg(windows)]
fn command(event: souvlaki::MediaControlEvent) -> Option<Command> {
    use souvlaki::MediaControlEvent as Event;
    match event {
        Event::Play => Some(Command::Play),
        Event::Pause => Some(Command::Pause),
        Event::Toggle => Some(Command::Toggle),
        Event::Next => Some(Command::Next),
        Event::Previous => Some(Command::Previous),
        Event::Stop => Some(Command::Stop),
        Event::SetPosition(souvlaki::MediaPosition(position)) => Some(Command::Seek(position.as_secs_f64())),
        // No URI opening or process control through a system media callback.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    #[ignore = "Requires an interactive Windows desktop, not a hosted CI session"]
    fn native_smtc_registers_updates_and_detaches_without_audio_or_a_profile() {
        #[link(name = "RuntimeObject")]
        unsafe extern "system" {
            fn RoInitialize(kind: u32) -> i32;
            fn RoUninitialize();
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn CreateWindowExW(
                ex_style: u32,
                class: *const u16,
                title: *const u16,
                style: u32,
                x: i32,
                y: i32,
                width: i32,
                height: i32,
                parent: isize,
                menu: isize,
                instance: isize,
                param: *const std::ffi::c_void,
            ) -> isize;
            fn DestroyWindow(hwnd: isize) -> i32;
        }
        struct Desktop(isize);
        impl Drop for Desktop {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                    RoUninitialize();
                }
            }
        }
        assert!(unsafe { RoInitialize(0) } >= 0);
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let hwnd =
            unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 100, 100, 0, 0, 0, std::ptr::null()) };
        let _desktop = Desktop(hwnd);
        assert_ne!(hwnd, 0);
        let mut controls =
            Controls::for_window(hwnd as *mut std::ffi::c_void, eframe::egui::Context::default()).unwrap();
        let song = Song {
            id: "native-smoke".into(),
            title: "Demo".into(),
            artist: "Test".into(),
            duration: 30.0,
            ..Default::default()
        };
        controls.update(Some(&song), true, false, 5.0).unwrap();
        controls.update(Some(&song), true, false, 6.0).unwrap();
        controls.update(None, false, false, 0.0).unwrap();
        assert!(controls.poll().is_none());
        drop(controls);
    }
    #[cfg(windows)]
    #[test]
    fn media_commands_do_not_accept_uris_or_process_control() {
        assert_eq!(command(souvlaki::MediaControlEvent::Next), Some(Command::Next));
        assert_eq!(command(souvlaki::MediaControlEvent::OpenUri("file:///private".into())), None);
        assert_eq!(command(souvlaki::MediaControlEvent::Quit), None);
        assert_eq!(
            command(souvlaki::MediaControlEvent::SetPosition(souvlaki::MediaPosition(Duration::from_secs(7)))),
            Some(Command::Seek(7.0))
        );
    }
    #[test]
    fn hostile_media_times_are_bounded() {
        assert_eq!(duration(f64::NAN), None);
        assert_eq!(duration(-1.0), Some(Duration::ZERO));
        assert_eq!(duration(1e300), Some(Duration::from_secs_f64(crate::api::MAX_DURATION_SECS)));
    }
}
