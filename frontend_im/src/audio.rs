use std::cell::RefCell;
use std::time::Instant;

use anyhow::Result;
use snow_core::renderer::{
    null_audio_sink, AudioBuffer, AudioProvider, AudioSink, AUDIO_QUEUE_LEN,
};

use crate::js_api;

const SAMPLE_SIZE_BITS: u32 = 32;

const AUDIO_WORKLET_QUANTUM_FRAMES: u32 = 128;
const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

/// How far emulated audio may run ahead of the wall clock before the clock
/// pacer sleeps, and how far behind it may fall before it gives up catching
/// up and starts over.
const CLOCK_PACING_LEAD_SECONDS: f64 = 0.05;
const CLOCK_PACING_RESYNC_SECONDS: f64 = 0.25;

/// Paces audio production against the wall clock.
///
/// The worker drops audio while the page's AudioContext isn't running (until
/// the first user gesture, or when the browser refuses to play sound), so the
/// JS buffer never fills and can't apply back-pressure. Without this, the
/// "accurate" speed runs uncapped in that state: several times real time,
/// which, among other things, shrinks the double-click interval below what a
/// person can click.
#[derive(Default)]
struct ClockPacer {
    /// Wall-clock time and seconds of audio produced since then.
    origin: Option<(f64, f64)>,
}

impl ClockPacer {
    /// Accounts for `audio_seconds` of audio produced at wall-clock time
    /// `now` and returns how long to sleep to stay at real time.
    fn delay(&mut self, now: f64, audio_seconds: f64) -> f64 {
        let (start, produced) = self.origin.get_or_insert((now, 0.0));
        *produced += audio_seconds;
        let ahead = *produced - (now - *start);
        if ahead < -CLOCK_PACING_RESYNC_SECONDS {
            self.origin = Some((now, 0.0));
            return 0.0;
        }
        if ahead > CLOCK_PACING_LEAD_SECONDS {
            ahead - CLOCK_PACING_LEAD_SECONDS
        } else {
            0.0
        }
    }

    fn reset(&mut self) {
        self.origin = None;
    }
}

pub struct JsAudioProvider {
    stream_opened: bool,
}

impl JsAudioProvider {
    pub fn new() -> Self {
        Self {
            stream_opened: false,
        }
    }
}

impl AudioProvider for JsAudioProvider {
    fn create_stream(
        &mut self,
        freq: i32,
        channels: u8,
        samples: u16,
    ) -> Result<Box<dyn AudioSink>> {
        if self.stream_opened {
            log::info!(
                "Ignoring additional JS audio stream: sample_rate={} channels={} samples={}",
                freq,
                channels,
                samples
            );
            return Ok(null_audio_sink());
        }
        self.stream_opened = true;
        Ok(Box::new(JsAudioSink::new(freq, channels, samples)))
    }
}

struct JsAudioSink {
    bytes_per_second: usize,
    max_js_buffer_bytes: usize,
    audio_worklet_quantum_seconds: f64,
    epoch: Instant,
    clock_pacer: RefCell<ClockPacer>,
}

impl JsAudioSink {
    fn new(freq: i32, channels: u8, samples: u16) -> Self {
        let sample_rate = u32::try_from(freq).unwrap();
        let channels_usize = usize::from(channels);
        let bytes_per_second = sample_rate as usize * channels_usize * BYTES_PER_SAMPLE;
        let max_js_buffer_bytes =
            usize::from(samples) * channels_usize * BYTES_PER_SAMPLE * AUDIO_QUEUE_LEN;
        let audio_worklet_quantum_seconds =
            AUDIO_WORKLET_QUANTUM_FRAMES as f64 / f64::from(sample_rate);

        js_api::audio::did_open(sample_rate, SAMPLE_SIZE_BITS, u32::from(channels));
        Self {
            bytes_per_second,
            max_js_buffer_bytes,
            audio_worklet_quantum_seconds,
            epoch: Instant::now(),
            clock_pacer: RefCell::new(ClockPacer::default()),
        }
    }
}

impl AudioSink for JsAudioSink {
    fn send(&self, buffer: AudioBuffer) -> Result<()> {
        let expected_len = buffer.len() * BYTES_PER_SAMPLE;
        let max_fill = self.max_js_buffer_bytes.saturating_sub(expected_len);
        if js_api::audio::buffer_size() == 0 {
            // Nothing is draining the buffer (or it just ran dry): keep time
            // with the wall clock instead.
            let now = self.epoch.elapsed().as_secs_f64();
            let audio_seconds = expected_len as f64 / self.bytes_per_second as f64;
            let delay = self.clock_pacer.borrow_mut().delay(now, audio_seconds);
            if delay > 0.0 {
                js_api::runtime::sleep_seconds(delay);
            }
        } else {
            self.clock_pacer.borrow_mut().reset();
        }
        loop {
            let js_buffer_size = js_api::audio::buffer_size();
            if js_buffer_size < 0 || js_buffer_size as usize <= max_fill {
                break;
            }
            // Estimate how long it will take to drain the buffer so that we
            // can sleep instead of spinning. Leave some headroom for jitter,
            // and wait at most one audio worklet quantum.
            let wait_bytes = js_buffer_size as usize - max_fill;
            let wait_seconds = ((wait_bytes as f64 / self.bytes_per_second as f64) * 0.75)
                .clamp(0.0, self.audio_worklet_quantum_seconds);
            js_api::runtime::sleep_seconds(wait_seconds);
        }

        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, expected_len) };
        js_api::audio::enqueue(bytes);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        js_api::audio::buffer_size() <= 0
    }

    fn is_full(&self) -> bool {
        let js_buffer_size = js_api::audio::buffer_size();
        js_buffer_size >= 0 && (js_buffer_size as usize) >= self.max_js_buffer_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_pacer_holds_production_to_real_time() {
        let mut pacer = ClockPacer::default();
        // 10ms of audio produced every 1ms of wall time: 10x real time
        let mut slept = 0.0;
        for i in 0..100 {
            let now = f64::from(i).mul_add(0.001, slept);
            slept += pacer.delay(now, 0.01);
        }
        let wall = slept + 0.1;
        assert!(
            (wall - 1.0).abs() < CLOCK_PACING_LEAD_SECONDS + 0.01,
            "wall={wall}"
        );
    }

    #[test]
    fn clock_pacer_does_not_sleep_at_or_below_real_time() {
        let mut pacer = ClockPacer::default();
        for i in 0..100 {
            assert_eq!(pacer.delay(f64::from(i) * 0.02, 0.01), 0.0);
        }
    }

    #[test]
    fn clock_pacer_resyncs_instead_of_bursting_after_a_stall() {
        let mut pacer = ClockPacer::default();
        pacer.delay(0.0, 0.01);
        // A 2s stall: don't race to make up for it
        assert_eq!(pacer.delay(2.0, 0.01), 0.0);
        assert_eq!(pacer.delay(2.001, 0.01), 0.0);
        assert!(pacer.delay(2.002, 0.1) > 0.0);
    }
}
