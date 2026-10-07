//! Opus sources for rodio, using the same Symphonia demuxers as tag probing.
//! libopus is linked statically: playback does not need an external codec.

use rodio::{ChannelCount, SampleRate, Source};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CodecParameters, CodecRegistry, Decoder, DecoderOptions, CODEC_TYPE_OPUS},
    errors::{Error, Result},
    formats::{FormatOptions, FormatReader, Packet, SeekMode, SeekTo},
    io::{MediaSource, MediaSourceStream},
    probe::Hint,
    units::TimeBase,
};

pub type AudioSource = Box<dyn Source + Send>;

/// Inspect the actual container, including Opus with an `.ogg` extension.
pub fn container(reader: &mut (impl Read + Seek)) -> std::io::Result<bool> {
    let result = (|| {
        let mut header = [0u8; 27];
        reader.read_exact(&mut header[..4])?;
        if header[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
            return Ok(true); // WebM/Matroska: select the Opus audio track.
        }
        if &header[..4] != b"OggS" {
            return Ok(false);
        }
        reader.read_exact(&mut header[4..])?;
        if header[4] != 0 || header[26] == 0 {
            return Ok(false);
        }
        reader.seek(SeekFrom::Start(27 + u64::from(header[26])))?;
        let mut packet = [0; 8];
        reader.read_exact(&mut packet)?;
        Ok(&packet == b"OpusHead")
    })();
    reader.seek(SeekFrom::Start(0))?;
    result
}

pub fn decode<R: Read + Seek + Send + Sync + 'static>(
    mut reader: R,
    len: Option<u64>,
    streaming: bool,
) -> Result<OpusSource> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic)?;
    reader.seek(SeekFrom::Start(0))?;
    let ogg = &magic == b"OggS";
    // Ogg records its physical end range during construction, even if probing
    // the unavailable tail fails. Do not change it from forward-only later:
    // Symphonia 0.5's Ogg seeker assumes that range exists for a seekable input.
    // Matroska instead must skip its tail/Cues scan on a partial download.
    let seekable = Arc::new(AtomicBool::new(len.is_some() && (!streaming || ogg)));
    let stream =
        MediaSourceStream::new(Box::new(Input { reader, len, seekable: seekable.clone() }), Default::default());
    let probed = symphonia::default::get_probe().format(
        &Hint::new(),
        stream,
        &FormatOptions { enable_gapless: true, ..Default::default() },
        &Default::default(),
    )?;
    let track = probed
        .format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec == CODEC_TYPE_OPUS)
        .ok_or(Error::Unsupported(crate::i18n::tr("контейнер не содержит поддерживаемый Opus-трек")))?;
    let params = track.codec_params.clone();
    let track_id = track.id;
    let time_base = params.time_base.ok_or(Error::Unsupported(crate::i18n::tr("Opus без временной шкалы")))?;
    let rate = SampleRate::new(params.sample_rate.unwrap_or(48_000))
        .ok_or(Error::Unsupported(crate::i18n::tr("частота Opus")))?;
    let channels = params
        .channels
        .or_else(|| params.channel_layout.map(|layout| layout.into_channels()))
        .map(|channels| channels.count())
        .unwrap_or(0);
    if !(1..=2).contains(&channels) {
        return Err(Error::Unsupported(crate::i18n::tr("Opus: поддерживаются моно и стерео")));
    }
    let channels = ChannelCount::new(channels as u16).ok_or(Error::Unsupported(crate::i18n::tr("каналы Opus")))?;
    let gain = params
        .extra_data
        .as_deref()
        .filter(|header| header.starts_with(b"OpusHead"))
        .and_then(|header| header.get(16..18))
        .map(|gain| 10.0f32.powf(f32::from(i16::from_le_bytes([gain[0], gain[1]])) / (256.0 * 20.0)))
        .unwrap_or(1.0);
    let pre_skip = params
        .extra_data
        .as_deref()
        .filter(|header| header.starts_with(b"OpusHead"))
        .and_then(|header| header.get(10..12))
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .unwrap_or(0);
    let decoder = make_decoder(&params)?;
    let duration = duration_from_params(&params);
    let mut source = OpusSource {
        format: probed.format,
        decoder,
        track_id,
        time_base,
        channels,
        rate,
        duration,
        samples: Vec::new(),
        offset: 0,
        gain,
        params,
        pre_skip,
        ogg,
        position_samples: 0,
    };
    source.refill()?;
    seekable.store(len.is_some(), Ordering::SeqCst);
    Ok(source)
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let mut registry = CodecRegistry::new();
    registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
    registry.make(params, &DecoderOptions::default())
}

pub(crate) fn duration_from_params(params: &CodecParameters) -> Option<Duration> {
    let pre_skip = params
        .extra_data
        .as_deref()
        .filter(|header| header.starts_with(b"OpusHead"))
        .and_then(|header| header.get(10..12))
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .unwrap_or(0);
    let time = params.time_base?.calc_time(params.n_frames?);
    let seconds =
        (time.seconds as f64 + time.frac - f64::from(pre_skip) / 48_000.0).clamp(0.0, crate::api::MAX_DURATION_SECS);
    Some(Duration::from_secs_f64(seconds))
}

struct Input<R> {
    reader: R,
    len: Option<u64>,
    seekable: Arc<AtomicBool>,
}

impl<R: Read> Read for Input<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(bytes)
    }
}

impl<R: Seek> Seek for Input<R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.reader.seek(from)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for Input<R> {
    fn is_seekable(&self) -> bool {
        self.seekable.load(Ordering::SeqCst)
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

pub struct OpusSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: TimeBase,
    channels: ChannelCount,
    rate: SampleRate,
    duration: Option<Duration>,
    samples: Vec<f32>,
    offset: usize,
    gain: f32,
    params: CodecParameters,
    pre_skip: u16,
    ogg: bool,
    position_samples: u64,
}

impl OpusSource {
    fn refill(&mut self) -> Result<()> {
        guarded(|| self.refill_inner())
    }

    fn refill_inner(&mut self) -> Result<()> {
        // Bound a run of invalid/irrelevant packets before returning to rodio.
        for _ in 0..256 {
            let packet = self.format.next_packet()?;
            if packet.track_id() != self.track_id {
                continue;
            }
            if let Some(params) =
                self.format.tracks().iter().find(|track| track.id == self.track_id).map(|track| &track.codec_params)
            {
                self.duration = duration_from_params(params).or(self.duration);
            }
            // With short Opus streams the 0.5 Ogg demuxer can infer tail
            // padding as start padding. Use OpusHead pre-skip exactly once,
            // preserve end trimming, and cap output at the EOS granule duration.
            let packet = if self.ogg {
                Packet::new_trimmed_from_boxed_slice(
                    packet.track_id(),
                    packet.ts(),
                    packet.dur(),
                    0,
                    packet.trim_end(),
                    packet.data,
                )
            } else {
                packet
            };
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) if decoded.frames() > 0 => decoded,
                Ok(_) | Err(Error::DecodeError(_)) => continue,
                Err(error) => return Err(error),
            };
            let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            samples.copy_interleaved_ref(decoded);
            self.samples.clear();
            self.samples.extend(samples.samples().iter().map(|sample| *sample * self.gain));
            self.offset = 0;
            return Ok(());
        }
        Err(Error::DecodeError(crate::i18n::tr("Opus: слишком много повреждённых пакетов")))
    }
}

impl Iterator for OpusSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.duration.is_some_and(|duration| {
            self.position_samples
                >= (duration.as_secs_f64() * f64::from(self.rate.get())).round() as u64 * u64::from(self.channels.get())
        }) {
            return None;
        }
        if self.offset >= self.samples.len() {
            self.refill().ok()?;
        }
        let sample = self.samples[self.offset];
        self.offset += 1;
        self.position_samples += 1;
        Some(sample)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.samples.len().saturating_sub(self.offset))
    }
    fn channels(&self) -> ChannelCount {
        self.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
    fn try_seek(&mut self, position: Duration) -> std::result::Result<(), rodio::source::SeekError> {
        let result = guarded(|| -> Result<()> {
            let position = self.duration.map(|duration| position.min(duration)).unwrap_or(position);
            let delay = Duration::from_secs_f64(f64::from(self.pre_skip) / 48_000.0);
            let seek_target =
                position.saturating_sub(Duration::from_millis(80)) + if self.ogg { delay } else { Duration::ZERO };
            let seeked = self
                .format
                .seek(SeekMode::Accurate, SeekTo::Time { time: seek_target.into(), track_id: Some(self.track_id) })?;
            let mut params = self.params.clone();
            if seeked.actual_ts != 0 {
                params.extra_data = None;
            }
            self.decoder = make_decoder(&params)?;
            self.samples.clear();
            self.offset = 0;
            let target = position + if self.ogg { delay } else { Duration::ZERO };
            let desired_ts = self.time_base.calc_timestamp(target.into());
            let time = self.time_base.calc_time(desired_ts.saturating_sub(seeked.actual_ts));
            if time.seconds > 60 {
                return Err(Error::LimitError(crate::i18n::tr("Opus: слишком большая дистанция точной перемотки")));
            }
            let mut remaining = ((time.seconds as f64 + time.frac) * f64::from(self.rate.get())).round() as u64;
            if seeked.actual_ts == 0 && self.ogg {
                remaining = remaining.saturating_sub(u64::from(self.pre_skip));
            }
            while remaining > 0 {
                self.refill()?;
                let frames = (self.samples.len() / usize::from(self.channels.get())) as u64;
                let skipped = remaining.min(frames);
                self.offset = skipped as usize * usize::from(self.channels.get());
                remaining -= skipped;
            }
            self.position_samples =
                (position.as_secs_f64() * f64::from(self.rate.get())).round() as u64 * u64::from(self.channels.get());
            Ok(())
        });
        result.map_err(|error| rodio::source::SeekError::Other(Arc::new(error)))
    }
}

fn guarded<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let old = crate::HANDLED_PANIC.with(|handled| handled.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
    crate::HANDLED_PANIC.with(|handled| handled.set(old));
    result.unwrap_or(Err(Error::DecodeError(crate::i18n::tr("Opus: повреждённый поток"))))
}
