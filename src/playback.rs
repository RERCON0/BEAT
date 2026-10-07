//! One prepared next track, consumed directly by rodio at the sample boundary.
//! No audio-device access here, so transitions can be tested deterministically.
use crate::opus::AudioSource;
use rodio::{ChannelCount, SampleRate, Source};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Clock {
    pub token: AtomicU64,
    micros: AtomicU64,
    boundary: Mutex<()>,
    pub seek_token: AtomicU64,
}

impl Clock {
    pub fn position(&self) -> f64 {
        self.micros.load(Ordering::Acquire) as f64 / 1_000_000.0
    }
    pub fn reset(&self, token: u64) {
        self.reset_at(token, 0.0);
    }
    pub fn reset_at(&self, token: u64, position: f64) {
        let _boundary = crate::lock(&self.boundary);
        self.micros.store((crate::session::safe_position(position) * 1_000_000.0) as u64, Ordering::Release);
        self.token.store(token, Ordering::Release);
    }
    pub fn snapshot(&self) -> (u64, f64) {
        let _boundary = crate::lock(&self.boundary);
        (self.token.load(Ordering::Acquire), self.position())
    }
}

pub struct Tracked {
    source: AudioSource,
    clock: Arc<Clock>,
    token: u64,
    samples: u64,
    started: bool,
}

impl Tracked {
    pub fn new(source: AudioSource, clock: Arc<Clock>, token: u64) -> Self {
        Self { source, clock, token, samples: 0, started: false }
    }
    pub fn at(source: AudioSource, clock: Arc<Clock>, token: u64, position: f64) -> Self {
        let samples =
            (position * f64::from(source.sample_rate().get())).round() as u64 * u64::from(source.channels().get());
        Self { source, clock, token, samples, started: true }
    }
    fn publish(&self) {
        let frames = self.samples / u64::from(self.source.channels().get());
        let micros = frames.saturating_mul(1_000_000) / u64::from(self.source.sample_rate().get());
        self.clock.micros.store(micros, Ordering::Release);
    }
}

impl Iterator for Tracked {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let sample = self.source.next();
        if sample.is_some() {
            if !self.started {
                self.clock.reset(self.token);
                self.started = true;
            }
            self.samples = self.samples.saturating_add(1);
            if self.samples & 511 == 0 {
                self.publish();
            }
        } else if self.started {
            self.publish();
        }
        sample
    }
}

impl Source for Tracked {
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.source.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.source.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
    fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
        if self.clock.seek_token.load(Ordering::Acquire) != self.token {
            return Err(rodio::source::SeekError::Other(Arc::new(std::io::Error::other("track changed during seek"))));
        }
        let position = self.source.total_duration().map(|duration| position.min(duration)).unwrap_or(position);
        self.source.try_seek(position)?;
        self.samples = (position.as_secs_f64() * f64::from(self.sample_rate().get())).round() as u64
            * u64::from(self.channels().get());
        self.publish();
        Ok(())
    }
}

struct State {
    prepared: Option<(u64, AudioSource)>,
    latched: bool,
}

/// A bounded replaceable slot, rather than accumulating cancelled sources.
pub struct NextSlot {
    state: Mutex<State>,
    channels: ChannelCount,
    rate: SampleRate,
}

impl NextSlot {
    pub fn new(channels: ChannelCount, rate: SampleRate) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State { prepared: None, latched: false }), channels, rate })
    }
    pub fn set(&self, token: u64, source: AudioSource) -> bool {
        let mut state = crate::lock(&self.state);
        if state.latched {
            return false;
        }
        state.prepared = Some((token, source));
        true
    }
    pub fn clear(&self) {
        crate::lock(&self.state).prepared = None;
    }
    #[cfg(test)]
    pub fn pending(&self) -> bool {
        crate::lock(&self.state).prepared.is_some()
    }
}

/// The queued placeholder freezes its metadata before its first sample. A late
/// worker must not change sample-rate/channels after rodio sets up conversion.
pub struct FutureSource {
    slot: Arc<NextSlot>,
    clock: Arc<Clock>,
    active: Option<rodio::source::UniformSourceIterator<Tracked>>,
    attempted: bool,
}

impl FutureSource {
    pub fn new(slot: Arc<NextSlot>, clock: Arc<Clock>) -> Self {
        Self { slot, clock, active: None, attempted: false }
    }
}

impl Iterator for FutureSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if !self.attempted {
            self.attempted = true;
            let mut state = crate::lock(&self.slot.state);
            state.latched = true;
            self.active = state.prepared.take().map(|(token, source)| {
                // Publish under the same lock as clear(): the UI can detect a
                // boundary that won a race with editing/cancelling the queue.
                rodio::source::UniformSourceIterator::new(
                    Tracked::new(source, self.clock.clone(), token),
                    self.slot.channels,
                    self.slot.rate,
                )
            });
            // Pull the first sample while still holding the cancellation lock:
            // empty sources cannot claim a transition, and the UI observes
            // a real transition which won before clear().
            return self.active.as_mut()?.next();
        }
        self.active.as_mut()?.next()
    }
}

impl Source for FutureSource {
    fn current_span_len(&self) -> Option<usize> {
        if let Some(active) = &self.active {
            return active.current_span_len();
        }
        let mut state = crate::lock(&self.slot.state);
        state.latched = true;
        if state.prepared.is_some() {
            None
        } else {
            Some(0)
        }
    }
    fn channels(&self) -> ChannelCount {
        self.slot.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.slot.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
    fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
        self.active
            .as_mut()
            .ok_or(rodio::source::SeekError::NotSupported { underlying_source: "next track" })?
            .try_seek(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(value: f32, samples: usize, channels: u16, rate: u32) -> AudioSource {
        Box::new(rodio::buffer::SamplesBuffer::new(
            ChannelCount::new(channels).unwrap(),
            SampleRate::new(rate).unwrap(),
            vec![value; samples],
        ))
    }
    #[test]
    fn the_audio_queue_transitions_without_a_ui_tick_or_silence() {
        let clock = Arc::new(Clock::default());
        let slot = NextSlot::new(ChannelCount::new(2).unwrap(), SampleRate::new(48_000).unwrap());
        assert!(slot.set(2, tone(0.75, 4800, 2, 48_000)));
        let (tx, rx) = rodio::queue::queue(false);
        tx.append(Tracked::new(tone(0.25, 9600, 2, 48_000), clock.clone(), 1));
        tx.append(FutureSource::new(slot, clock.clone()));
        let decoded: Vec<_> = rx.collect();
        assert_eq!(decoded.len(), 14400);
        assert!(decoded[..9600].iter().all(|v| *v == 0.25));
        assert!(decoded[9600..].iter().all(|v| *v == 0.75));
        assert_eq!(clock.token.load(Ordering::Acquire), 2);
        assert!((clock.position() - 0.05).abs() < 0.006, "position {}", clock.position());
    }
    #[test]
    fn cancelled_next_tracks_are_replaced_in_one_slot_and_late_results_are_refused() {
        let slot = NextSlot::new(ChannelCount::new(1).unwrap(), SampleRate::new(48_000).unwrap());
        assert!(slot.set(2, tone(0.2, 100, 1, 48_000)));
        slot.clear();
        assert!(!slot.pending());
        assert!(slot.set(3, tone(0.3, 100, 1, 48_000)));
        let mut future = FutureSource::new(slot.clone(), Arc::new(Clock::default()));
        assert_eq!(future.current_span_len(), None);
        assert!(!slot.set(4, tone(0.4, 100, 1, 48_000)));
        assert_eq!(future.next(), Some(0.3));
    }

    #[test]
    fn a_real_rodio_player_accepts_late_preparation_and_converts_mixed_formats() {
        let clock = Arc::new(Clock::default());
        let slot = NextSlot::new(ChannelCount::new(2).unwrap(), SampleRate::new(48_000).unwrap());
        let (output, source) = rodio::Player::new();
        output.append(Tracked::new(tone(0.25, 9600, 2, 48_000), clock.clone(), 1));
        output.append(FutureSource::new(slot.clone(), clock.clone()));
        assert!(slot.set(2, tone(0.75, 2205, 1, 44_100)), "append latched the empty slot too early");
        let mut converted = rodio::source::UniformSourceIterator::new(
            source,
            ChannelCount::new(2).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        let samples: Vec<_> = converted.by_ref().take(14400).collect();
        assert_eq!(samples.len(), 14400);
        assert!(samples[..9600].iter().all(|v| (*v - 0.25).abs() < 0.0001));
        assert!(
            samples[9610..14380].iter().all(|v| (*v - 0.75).abs() < 0.0001),
            "mixed-rate transition was padded or corrupted"
        );
        assert_eq!(clock.token.load(Ordering::Acquire), 2);
        assert!((clock.position() - 0.05).abs() < 0.006, "position {}", clock.position());
    }

    #[test]
    fn cancelling_a_latched_slot_does_not_publish_a_phantom_track() {
        let clock = Arc::new(Clock::default());
        clock.reset(1);
        let slot = NextSlot::new(ChannelCount::new(2).unwrap(), SampleRate::new(48_000).unwrap());
        assert!(slot.set(2, tone(0.5, 960, 2, 48_000)));
        let mut future = FutureSource::new(slot.clone(), clock.clone());
        assert_eq!(future.current_span_len(), None);
        slot.clear();
        assert_eq!(future.next(), None);
        assert_eq!(clock.token.load(Ordering::Acquire), 1);
    }

    #[test]
    fn position_and_seek_are_bound_to_the_current_track() {
        let clock = Arc::new(Clock::default());
        let mut tracked = Tracked::at(tone(0.5, 96_000, 2, 48_000), clock.clone(), 2, 0.0);
        clock.reset(2);
        clock.seek_token.store(1, Ordering::Release);
        assert!(tracked.try_seek(Duration::from_millis(250)).is_err());
        assert_eq!(clock.position(), 0.0);
        clock.seek_token.store(2, Ordering::Release);
        tracked.try_seek(Duration::from_millis(250)).unwrap();
        assert!((clock.position() - 0.25).abs() < 0.001);
        for _ in 0..48_000 {
            tracked.next().unwrap();
        }
        assert!((clock.position() - 0.75).abs() < 0.01);
    }

    #[test]
    fn an_empty_prepared_track_does_not_change_the_song_or_position() {
        let clock = Arc::new(Clock::default());
        clock.reset_at(1, 7.0);
        let slot = NextSlot::new(ChannelCount::new(2).unwrap(), SampleRate::new(48_000).unwrap());
        assert!(slot.set(2, tone(0.5, 0, 2, 48_000)));
        let mut future = FutureSource::new(slot, clock.clone());
        assert_eq!(future.next(), None);
        assert_eq!(clock.token.load(Ordering::Acquire), 1);
        assert_eq!(clock.position(), 7.0);
    }
}
