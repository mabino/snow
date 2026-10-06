//! Audio output through the JavaScript Web Audio bridge
//!
//! The main thread owns the Web Audio graph. The worker posts chunks to it
//! and tracks how many interleaved samples are outstanding
//! (`audioQueued` in `web.js`), which the main thread reduces by posting
//! `audio-drained` messages as playback progresses.
//!
//! Backpressure is applied by *dropping* a chunk when the JavaScript
//! buffer is full, never by sleeping: the drain reports arrive from the
//! main thread as messages, and this single-threaded worker cannot process
//! messages while blocked, so a wait-loop here would stall the entire
//! machine (CPU, video, and network) whenever the main thread stops
//! draining (no AudioContext on the page, throttled tab, ...). Dropping a
//! chunk is a brief audio skip; the guest keeps running and buffering
//! resumes once playback catches up. The JavaScript side additionally
//! bounds its own queue, so memory stays bounded either way.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use snow_core::renderer::{
    null_audio_sink, AudioBuffer, AudioProvider, AudioSink, AUDIO_QUEUE_LEN,
};

use crate::js_api;

const SAMPLE_SIZE_BITS: u32 = 32;
const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

pub struct WebAudioProvider {
    stream_opened: bool,
}

impl WebAudioProvider {
    pub fn new() -> Self {
        Self {
            stream_opened: false,
        }
    }
}

impl AudioProvider for WebAudioProvider {
    fn create_stream(
        &mut self,
        freq: i32,
        channels: u8,
        samples: u16,
    ) -> Result<Box<dyn AudioSink>> {
        if self.stream_opened {
            log::info!(
                "Ignoring additional audio stream: sample_rate={} channels={} samples={}",
                freq,
                channels,
                samples
            );
            return Ok(null_audio_sink());
        }
        self.stream_opened = true;
        Ok(Box::new(WebAudioSink::new(freq, channels, samples)))
    }
}

struct WebAudioSink {
    /// Maximum number of bytes the JavaScript buffer holds
    max_js_buffer_bytes: usize,
    /// Buffers dropped because the JavaScript buffer was full
    dropped: AtomicUsize,
}

impl WebAudioSink {
    fn new(freq: i32, channels: u8, samples: u16) -> Self {
        let sample_rate = u32::try_from(freq).unwrap();
        let channels_usize = usize::from(channels);
        let max_js_buffer_bytes =
            usize::from(samples) * channels_usize * BYTES_PER_SAMPLE * AUDIO_QUEUE_LEN;

        js_api::did_open_audio(sample_rate, SAMPLE_SIZE_BITS, u32::from(channels));
        Self {
            max_js_buffer_bytes,
            dropped: AtomicUsize::new(0),
        }
    }
}

impl AudioSink for WebAudioSink {
    fn send(&self, buffer: AudioBuffer) -> Result<()> {
        let expected_len = buffer.len() * BYTES_PER_SAMPLE;
        let max_fill = self.max_js_buffer_bytes.saturating_sub(expected_len);
        let js_buffer_size = js_api::audio_buffer_size();
        if js_buffer_size >= 0 && (js_buffer_size as usize) > max_fill {
            // The JavaScript buffer cannot yet accept this chunk. Drop it
            // (a brief audio skip) instead of waiting for the main thread
            // to drain it; see the module docs for why waiting would
            // stall the guest. The JavaScript side bounds its queue, and
            // the main thread keeps reporting playback progress, so
            // buffering resumes once the queue drains.
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(200) {
                log::warn!(
                    "JavaScript audio buffer is full; dropped {dropped} buffer(s) \
                     until playback catches up"
                );
            }
            return Ok(());
        }

        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, expected_len) };
        js_api::enqueue_audio(bytes);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        js_api::audio_buffer_size() <= 0
    }

    fn is_full(&self) -> bool {
        let js_buffer_size = js_api::audio_buffer_size();
        js_buffer_size >= 0 && js_buffer_size as usize >= self.max_js_buffer_bytes
    }
}
