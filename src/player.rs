//! Playback: one rodio `Player` on the default output device. Finished cache
//! entries play as plain files; in-flight downloads play through
//! `GrowingReader`, so the track starts while the rest is still arriving.

use crate::api::MAX_DURATION_SECS;
use crate::cache::{GrowingReader, Progress};
use rodio::cpal::traits::HostTrait;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player as Output};
use rodio::Decoder;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Player {
    /// Kept alive: dropping the device sink stops the output stream.
    _sink: MixerDeviceSink,
    output: Output,
    volume: f32,
    /// Cancel flag of the growing-file reader that feeds the current track, if
    /// it is a stream. rodio's `clear()`/`try_seek()` wait for the audio
    /// thread, which sits inside that reader while the download stalls: the
    /// flag frees it, so the UI thread never waits out the read timeout.
    stream_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Set by the OS when the output stream fails, which is what happens to
    /// a Bluetooth device that is switched off or carried out of range. A
    /// dead stream plays nothing and makes `try_seek` block forever, so the
    /// app rebuilds the output instead of using it.
    stream_error: Arc<AtomicBool>,
}

impl Player {
    pub fn new(volume: f32) -> Result<Self, String> {
        let stream_error = Arc::new(AtomicBool::new(false));
        let sink = open_sink(stream_error.clone())
            .map_err(|e| format!("аудиовыход недоступен: {e}"))?;
        let output = Output::connect_new(sink.mixer());
        output.set_volume(volume);
        Ok(Self { _sink: sink, output, volume, stream_cancel: Mutex::new(None), stream_error })
    }

    /// True when the OS reported a failure of the output stream (the device
    /// disappeared). The caller should rebuild the player.
    pub fn has_stream_error(&self) -> bool {
        self.stream_error.load(Ordering::SeqCst)
    }

    /// Releases the audio thread from the current stream's reader.
    fn cancel_stream(&self) {
        if let Some(flag) = self.stream_cancel.lock().unwrap().take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn play_file(&self, path: &Path) -> Result<(), String> {
        let decoder = open_file_decoder(path)?;
        self.cancel_stream();
        self.output.clear();
        self.output.append(decoder);
        self.output.play();
        Ok(())
    }

    pub fn play_streaming(&self, progress: Arc<Progress>, path: &Path, total: Option<u64>) -> Result<(), String> {
        let reader = GrowingReader::open(progress, path)?;
        let cancel = reader.cancel_handle();
        let startup = reader.startup_handle();
        startup.store(true, Ordering::SeqCst);
        let mut builder = Decoder::builder().with_data(reader);
        if let Some(total) = total {
            builder = builder.with_byte_len(total);
        }
        let decoder = guard_decoder(|| builder.build().map_err(|e| format!("декодер: {e}")))?;
        startup.store(false, Ordering::SeqCst);
        self.cancel_stream();
        self.output.clear();
        *self.stream_cancel.lock().unwrap() = Some(cancel);
        self.output.append(decoder);
        self.output.play();
        Ok(())
    }

    pub fn stop(&self) {
        self.cancel_stream();
        self.output.stop();
        self.output.clear();
    }

    pub fn pause(&self) { self.output.pause(); }
    pub fn resume(&self) { self.output.play(); }
    pub fn is_paused(&self) -> bool { self.output.is_paused() }

    /// True when nothing is queued/paying anymore (track finished).
    pub fn ended(&self) -> bool { self.output.empty() }

    pub fn position(&self) -> f64 {
        self.output.get_pos().as_secs_f64()
    }

    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        // `try_seek` on a stream whose device is gone blocks forever, so a
        // failed stream must never reach it.
        if self.has_stream_error() {
            return Err("аудиоустройство недоступно".into());
        }
        let target = seek_duration(seconds).ok_or_else(|| "некорректная позиция перемотки".to_string())?;
        self.output.try_seek(target)
            .map_err(|e| format!("перемотка недоступна: {e}"))
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.output.set_volume(self.volume);
    }

    pub fn volume(&self) -> f32 { self.volume }
}

/// Opens the default output device with an error callback that records stream
/// failures (Bluetooth unplugged, device disabled). Mirrors rodio's own
/// fallback: the default device first, then any other output device that can
/// take the configuration.
fn open_sink(flag: Arc<AtomicBool>) -> Result<MixerDeviceSink, String> {
    if let Ok(builder) = DeviceSinkBuilder::from_default_device() {
        if let Ok(sink) = builder.with_error_callback(flag_callback(flag.clone())).open_stream() {
            return Ok(silence_drop(sink));
        }
    }
    let devices = rodio::cpal::default_host().output_devices().map_err(|e| e.to_string())?;
    for device in devices {
        let Ok(builder) = DeviceSinkBuilder::from_device(device) else { continue };
        if let Ok(sink) = builder.with_error_callback(flag_callback(flag.clone())).open_sink_or_fallback() {
            return Ok(silence_drop(sink));
        }
    }
    Err("не найдено подходящее устройство вывода".into())
}

fn flag_callback(flag: Arc<AtomicBool>) -> impl FnMut(rodio::cpal::StreamError) + Send + Clone + 'static {
    move |err: rodio::cpal::StreamError| {
        eprintln!("beat: аудиопоток: {err}");
        flag.store(true, Ordering::SeqCst);
    }
}

/// rodio prints a notice when a sink is dropped; rebuilding the output on
/// every device change would fill the log with it.
fn silence_drop(mut sink: MixerDeviceSink) -> MixerDeviceSink {
    sink.log_on_drop(false);
    sink
}

/// Where a seek request may really go: never past the end, never past what
/// has been downloaded (`limit`; `None` = seeking is unavailable right now),
/// and never a value `Duration` cannot represent.
pub fn seek_position(target: f64, duration: f64, limit: Option<f64>) -> Option<f64> {
    let mut cap = limit?.min(MAX_DURATION_SECS);
    if duration.is_finite() && duration > 0.0 {
        cap = cap.min(duration);
    }
    let target = if target.is_finite() { target.max(0.0) } else { 0.0 };
    Some(target.min(cap))
}

/// `Duration::from_secs_f64` panics on NaN, infinity and overflow.
fn seek_duration(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() {
        return None;
    }
    Duration::try_from_secs_f64(seconds.clamp(0.0, MAX_DURATION_SECS)).ok()
}

/// Decoder over a finished file on disk. The byte length is what makes rodio
/// mark the source seekable: without it symphonia refuses every backward seek
/// (slider, "previous track" restart) and mp3 has no total duration.
fn open_file_decoder(path: &Path) -> Result<Decoder<std::io::BufReader<std::fs::File>>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("не удалось открыть {}: {e}", path.display()))?;
    let len = file.metadata()
        .map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?
        .len();
    guard_decoder(|| {
        Decoder::builder()
            .with_data(std::io::BufReader::new(file))
            .with_byte_len(len)
            .build()
            .map_err(|e| format!("декодер: {e}"))
    })
}

fn guard_decoder<T>(build: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    crate::HANDLED_PANIC.with(|handled| handled.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
    crate::HANDLED_PANIC.with(|handled| handled.set(false));
    result.unwrap_or_else(|_| Err("декодер не смог прочитать повреждённый файл".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source;

    /// Minimal 8-bit mono PCM WAV, `seconds` long.
    fn write_wav(path: &Path, seconds: u32) {
        let sample_rate = 8000u32;
        let samples = sample_rate * seconds;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + samples).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&samples.to_le_bytes());
        bytes.extend(std::iter::repeat_n(128u8, samples as usize));
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn seek_positions_are_clamped_to_what_can_be_played() {
        // Past the end, or a bogus number from the server: the end of the track.
        assert_eq!(seek_position(1e300, 200.0, Some(200.0)), Some(200.0));
        assert_eq!(seek_position(500.0, 200.0, Some(200.0)), Some(200.0));
        // Not past the downloaded part of a stream.
        assert_eq!(seek_position(150.0, 200.0, Some(100.0)), Some(100.0));
        assert_eq!(seek_position(60.0, 200.0, Some(100.0)), Some(60.0));
        // Garbage in, start of the track out.
        assert_eq!(seek_position(f64::NAN, 200.0, Some(200.0)), Some(0.0));
        assert_eq!(seek_position(-5.0, 200.0, Some(200.0)), Some(0.0));
        // Length unknown: bounded by the limit, and by a sane maximum.
        assert_eq!(seek_position(90.0, 0.0, Some(f64::INFINITY)), Some(90.0));
        assert_eq!(seek_position(1e300, 0.0, Some(f64::INFINITY)), Some(crate::api::MAX_DURATION_SECS));
        // Seeking unavailable (stream of unknown size still downloading).
        assert_eq!(seek_position(50.0, 200.0, None), None);
    }

    #[test]
    fn a_seek_duration_never_panics() {
        assert_eq!(seek_duration(f64::NAN), None);
        assert_eq!(seek_duration(f64::INFINITY), None);
        assert_eq!(seek_duration(-1.0), Some(Duration::ZERO));
        assert_eq!(seek_duration(1e300), Some(Duration::from_secs_f64(crate::api::MAX_DURATION_SECS)));
        assert_eq!(seek_duration(12.5), Some(Duration::from_millis(12_500)));
    }

    #[test]
    fn a_file_decoder_seeks_backwards_as_well_as_forwards() {
        let path = std::env::temp_dir().join(format!("beat-seek-{}-{}.wav", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        write_wav(&path, 3);
        let mut decoder = open_file_decoder(&path).unwrap();
        // Play a little, like the player does before the user touches the slider.
        assert!(decoder.by_ref().take(8000).count() > 0);
        let forward = decoder.try_seek(Duration::from_secs(2));
        let backward = decoder.try_seek(Duration::from_secs(1));
        let restart = decoder.try_seek(Duration::ZERO);
        let _ = std::fs::remove_file(&path);
        assert!(forward.is_ok(), "seek forward failed: {forward:?}");
        assert!(backward.is_ok(), "seek backward failed: {backward:?}");
        assert!(restart.is_ok(), "seek to the start failed: {restart:?}");
    }
}
