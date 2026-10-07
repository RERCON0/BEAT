//! Playback: one rodio `Player` on the default output device. Finished cache
//! entries play as plain files; in-flight downloads play through
//! `GrowingReader`, so the track starts while the rest is still arriving.

use crate::api::MAX_DURATION_SECS;
use crate::cache::{GrowingReader, Progress};
use rodio::cpal::traits::HostTrait;
use rodio::Decoder;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player as Output, Source};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    stream_startup: Mutex<Option<Arc<AtomicBool>>>,
    /// Set by the OS when the output stream fails, which is what happens to
    /// a Bluetooth device that is switched off or carried out of range. A
    /// dead stream plays nothing and makes `try_seek` block forever, so the
    /// app rebuilds the output instead of using it.
    stream_error: Arc<AtomicBool>,
    clock: Arc<crate::playback::Clock>,
    serial: AtomicU64,
    next: Mutex<Option<Arc<crate::playback::NextSlot>>>,
}

impl Player {
    pub fn new(volume: f32) -> Result<Self, String> {
        let stream_error = Arc::new(AtomicBool::new(false));
        let sink =
            open_sink(stream_error.clone()).map_err(|e| crate::i18n::trf!("аудиовыход недоступен: {e}", e = e))?;
        let output = Output::connect_new(sink.mixer());
        output.set_volume(volume);
        Ok(Self {
            _sink: sink,
            output,
            volume,
            stream_cancel: Mutex::new(None),
            stream_startup: Mutex::new(None),
            stream_error,
            clock: Arc::new(crate::playback::Clock::default()),
            serial: AtomicU64::new(1),
            next: Mutex::new(None),
        })
    }

    /// True when the OS reported a failure of the output stream (the device
    /// disappeared). The caller should rebuild the player.
    pub fn has_stream_error(&self) -> bool {
        self.stream_error.load(Ordering::SeqCst)
    }

    /// Releases the audio thread from the current stream's reader.
    fn cancel_stream(&self) {
        crate::lock(&self.stream_startup).take();
        if let Some(flag) = crate::lock(&self.stream_cancel).take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn play_file(&self, path: &Path) -> Result<(), String> {
        self.play_file_at(path, 0.0)
    }

    pub fn play_file_at(&self, path: &Path, position: f64) -> Result<(), String> {
        if self.has_stream_error() {
            return Err(crate::i18n::tr("аудиоустройство недоступно").into());
        }
        let mut decoder = open_file_decoder(path)?;
        let position = crate::session::safe_position(position);
        let position = decoder.total_duration().map(|d| position.min(d.as_secs_f64())).unwrap_or(position);
        if position > 0.0 {
            guard_decoder(|| {
                decoder
                    .try_seek(Duration::from_secs_f64(crate::session::safe_position(position)))
                    .map_err(|e| e.to_string())
            })?;
        }
        self.cancel_stream();
        self.output.clear();
        self.append_current(decoder, position);
        self.output.play();
        Ok(())
    }

    pub fn play_streaming(&self, progress: Arc<Progress>, path: &Path, total: Option<u64>) -> Result<(), String> {
        if self.has_stream_error() {
            return Err(crate::i18n::tr("аудиоустройство недоступно").into());
        }
        let reader = GrowingReader::open(progress, path)?;
        let cancel = reader.cancel_handle();
        let startup = reader.startup_handle();
        startup.store(true, Ordering::SeqCst);
        let decoder = open_stream_decoder(reader, total)?;
        startup.store(false, Ordering::SeqCst);
        self.cancel_stream();
        self.output.clear();
        *crate::lock(&self.stream_cancel) = Some(cancel);
        *crate::lock(&self.stream_startup) = Some(startup);
        self.append_current(decoder, 0.0);
        self.output.play();
        Ok(())
    }

    pub fn stop(&self) {
        self.clear_next();
        self.cancel_stream();
        self.output.stop();
        if !self.has_stream_error() {
            self.output.clear();
        }
    }

    pub fn pause(&self) {
        self.output.pause();
    }
    pub fn resume(&self) {
        self.output.play();
    }
    pub fn is_paused(&self) -> bool {
        self.output.is_paused()
    }

    /// True when nothing is queued/paying anymore (track finished).
    pub fn ended(&self) -> bool {
        self.output.empty()
    }

    pub fn position(&self) -> f64 {
        self.clock.position()
    }
    pub fn snapshot(&self) -> (u64, f64) {
        self.clock.snapshot()
    }

    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        // `try_seek` on a stream whose device is gone blocks forever, so a
        // failed stream must never reach it.
        if self.has_stream_error() {
            return Err(crate::i18n::tr("аудиоустройство недоступно").into());
        }
        let target =
            seek_duration(seconds).ok_or_else(|| crate::i18n::tr("некорректная позиция перемотки").to_string())?;
        // The decoder seeks on the audio thread, while rodio waits for its
        // reply here. Interrupt an existing network wait and prevent new ones.
        let _mode = NonblockingSeek::new(crate::lock(&self.stream_startup).clone());
        self.clock.seek_token.store(self.token(), Ordering::Release);
        self.output.try_seek(target).map_err(|e| crate::i18n::trf!("перемотка недоступна: {e}", e = e))
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.output.set_volume(self.volume);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    fn append_current(&self, source: crate::opus::AudioSource, position: f64) {
        let channels = self._sink.config().channel_count();
        let rate = self._sink.config().sample_rate();
        let token = self.serial.fetch_add(1, Ordering::Relaxed);
        self.clock.reset_at(token, position);
        // Queue metadata in rodio can lag a source boundary. Normalise each
        // track to the actual device format before queuing, so switching
        // between mono/stereo and sample rates cannot corrupt the first span.
        self.output.append(rodio::source::UniformSourceIterator::new(
            crate::playback::Tracked::at(source, self.clock.clone(), token, position),
            channels,
            rate,
        ));
        self.append_future(channels, rate);
    }

    fn append_future(&self, channels: rodio::ChannelCount, rate: rodio::SampleRate) {
        let slot = crate::playback::NextSlot::new(channels, rate);
        self.output.append(crate::playback::FutureSource::new(slot.clone(), self.clock.clone()));
        *crate::lock(&self.next) = Some(slot);
    }

    pub fn token(&self) -> u64 {
        self.clock.token.load(Ordering::Acquire)
    }

    pub fn clear_next(&self) {
        if let Some(slot) = crate::lock(&self.next).as_ref() {
            slot.clear();
        }
    }

    pub fn queue_prepared(&self, source: crate::opus::AudioSource) -> Option<u64> {
        if self.has_stream_error() || self.ended() {
            return None;
        }
        let token = self.serial.fetch_add(1, Ordering::Relaxed);
        if crate::lock(&self.next).as_ref()?.set(token, source) {
            Some(token)
        } else {
            None
        }
    }

    /// The previous placeholder is now the current source; queue one new slot.
    pub fn advance_slot(&self) {
        self.cancel_stream();
        if !self.has_stream_error() && !self.ended() {
            self.append_future(self._sink.config().channel_count(), self._sink.config().sample_rate());
        }
    }
}

struct NonblockingSeek(Option<(Arc<AtomicBool>, bool)>);

impl NonblockingSeek {
    fn new(flag: Option<Arc<AtomicBool>>) -> Self {
        Self(flag.map(|flag| {
            let old = flag.swap(true, Ordering::SeqCst);
            (flag, old)
        }))
    }
}

impl Drop for NonblockingSeek {
    fn drop(&mut self) {
        if let Some((flag, old)) = &self.0 {
            flag.store(*old, Ordering::SeqCst);
        }
    }
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
    Err(crate::i18n::tr("не найдено подходящее устройство вывода").into())
}

fn flag_callback(flag: Arc<AtomicBool>) -> impl FnMut(rodio::cpal::StreamError) + Send + Clone + 'static {
    move |err: rodio::cpal::StreamError| {
        eprintln!("{}", crate::i18n::trf!("beat: аудиопоток: {err}", err = err));
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
pub(crate) fn open_file_decoder(path: &Path) -> Result<crate::opus::AudioSource, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| crate::i18n::trf!("не удалось открыть {}: {e}", path.display(), e = e))?;
    let len =
        file.metadata().map_err(|e| crate::i18n::trf!("не удалось прочитать {}: {e}", path.display(), e = e))?.len();
    open_decoder(std::io::BufReader::new(file), Some(len))
}

pub(crate) fn open_decoder<R: std::io::Read + std::io::Seek + Send + Sync + 'static>(
    reader: R,
    len: Option<u64>,
) -> Result<crate::opus::AudioSource, String> {
    decode_input(reader, len, false)
}

pub(crate) fn open_stream_decoder(reader: GrowingReader, len: Option<u64>) -> Result<crate::opus::AudioSource, String> {
    decode_input(reader, len, true)
}

fn decode_input<R: std::io::Read + std::io::Seek + Send + Sync + 'static>(
    mut reader: R,
    len: Option<u64>,
    streaming: bool,
) -> Result<crate::opus::AudioSource, String> {
    crate::media::validate_metadata(&mut reader).map_err(|e| crate::i18n::trf!("метаданные: {e}", e = e))?;
    guard_decoder(|| {
        if crate::opus::container(&mut reader).map_err(|e| e.to_string())? {
            return crate::opus::decode(reader, len, streaming)
                .map(|source| Box::new(source) as crate::opus::AudioSource)
                .map_err(|e| format!("Opus: {e}"));
        }
        let mut builder = Decoder::builder().with_data(reader).with_gapless(true);
        if let Some(len) = len {
            builder = builder.with_byte_len(len);
        }
        builder
            .build()
            .map(|decoder| Box::new(decoder) as crate::opus::AudioSource)
            .map_err(|e| crate::i18n::trf!("декодер: {e}", e = e))
    })
}

fn guard_decoder<T>(build: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let old = crate::HANDLED_PANIC.with(|handled| handled.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
    crate::HANDLED_PANIC.with(|handled| handled.set(old));
    result.unwrap_or_else(|_| Err(crate::i18n::tr("декодер не смог прочитать повреждённый файл").into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source;

    #[test]
    fn opus_in_ogg_and_webm_decodes_and_seeks_without_duplicate_pre_skip() {
        for (bytes, channels, milliseconds_total, tolerance) in [
            (include_bytes!("../tests/fixtures/tone.opus").as_slice(), 2, 3000, 0),
            (include_bytes!("../tests/fixtures/tone.webm").as_slice(), 1, 3000, 96),
            (include_bytes!("../tests/fixtures/short.opus").as_slice(), 2, 250, 0),
        ] {
            let open = || open_decoder(std::io::Cursor::new(bytes), Some(bytes.len() as u64)).unwrap();
            let mut decoder = open();
            assert_eq!(decoder.channels().get(), channels);
            assert_eq!(decoder.sample_rate().get(), 48_000);
            let samples: Vec<_> = decoder.by_ref().collect();
            let expected = milliseconds_total * 48 * usize::from(channels);
            assert!(
                samples.len().abs_diff(expected) <= tolerance,
                "Opus padding/pre-skip: {} vs {expected}",
                samples.len()
            );
            assert!(samples.iter().all(|sample| sample.is_finite()));
            assert!(samples.iter().any(|sample| sample.abs() > 0.01));
            for milliseconds in [milliseconds_total * 7 / 10, milliseconds_total / 5, 0] {
                let mut decoder = open();
                decoder.try_seek(Duration::from_millis(milliseconds as u64)).unwrap();
                let remaining = decoder.count();
                let expected = expected - milliseconds * 48 * usize::from(channels);
                assert!(
                    remaining.abs_diff(expected) <= tolerance + usize::from(channels),
                    "seek to {milliseconds}: {remaining} vs {expected}"
                );
            }
            decoder.try_seek(Duration::ZERO).unwrap();
            assert!(decoder.take(4800).any(|sample| sample.abs() > 0.01), "rewinding an ended Opus source failed");
        }
    }

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
        let path = std::env::temp_dir().join(format!(
            "beat-seek-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
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
