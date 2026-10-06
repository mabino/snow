//! JavaScript glue for the standalone web frontend
//!
//! All functions in this module are implemented by `web.js` (linked via
//! `--js-library`). Names use the `web_js_` prefix so they do not collide
//! with the `js_` symbols used by `frontend_im` (both js-libraries are
//! linked into every Snow wasm build through the shared cargo config, but
//! Emscripten only includes a js-library function when it is referenced
//! from the compiled code).
//!
//! The emulator's main loop never returns to the worker's event loop, so
//! nothing can be delivered to it with `postMessage`. Input events and
//! received network packets instead arrive through a ring buffer in a
//! `SharedArrayBuffer` written by the page (see `www/snow-io.js` and
//! [`crate::io_protocol`]); Rust pulls them with [`poll_io`]. Output (video,
//! audio, outgoing packets) is posted to the page, which works fine from a
//! busy worker. JavaScript never calls back into Rust.

use std::ffi::CString;
use std::os::raw::c_char;

unsafe extern "C" {
    /// Initialize the glue (attaches to the page's I/O ring).
    /// Must be called before anything else.
    fn web_js_init();

    /// Block the worker for `seconds` seconds (speed governor)
    fn web_js_sleep(seconds: f64);

    /// Pop the next record from the page's I/O ring into `ptr` (capacity
    /// `cap`). Returns the record length, 0 if the ring is empty, or -1 if
    /// the record did not fit (it is discarded).
    fn web_js_poll_io(ptr: *mut u8, cap: u32) -> i32;

    /// Post framed outgoing network packets to the page
    fn web_js_net_send(ptr: *const u8, len: u32);

    /// Report a fatal user-visible error to the web page
    fn web_js_report_error(message: *const c_char);

    /// The video size changed (a new canvas/ImageData is set up)
    fn web_js_did_open_video(width: u32, height: u32);

    /// Blit an RGBA frame to the canvas
    fn web_js_blit(ptr: *const u8, size: u32);

    /// The audio stream parameters (sample rate, bits per sample, channels)
    fn web_js_did_open_audio(sample_rate: u32, sample_bits: u32, channels: u32);

    /// Bytes of audio currently queued in the JavaScript buffer
    /// (negative if audio is not initialized)
    fn web_js_audio_buffer_size() -> i32;

    /// Enqueue interleaved 32-bit float audio samples into the JS buffer
    fn web_js_enqueue_audio(ptr: *const u8, size: u32);

    /// Set the host (browser) clipboard
    fn web_js_set_clipboard_text(text: *const c_char);
}

// ------------------------------------------------------------------ video

pub fn did_open_video(width: u32, height: u32) {
    unsafe {
        web_js_did_open_video(width, height);
    }
}

pub fn blit(frame: &[u8]) {
    if frame.is_empty() {
        return;
    }
    let len = u32::try_from(frame.len()).unwrap_or(u32::MAX);
    unsafe {
        web_js_blit(frame.as_ptr(), len);
    }
}

// ------------------------------------------------------------------ audio

pub fn did_open_audio(sample_rate: u32, sample_bits: u32, channels: u32) {
    unsafe {
        web_js_did_open_audio(sample_rate, sample_bits, channels);
    }
}

/// Bytes of audio currently queued (negative if audio is not initialized)
pub fn audio_buffer_size() -> i32 {
    unsafe { web_js_audio_buffer_size() }
}

pub fn enqueue_audio(buffer: &[u8]) {
    if buffer.is_empty() {
        return;
    }
    let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    unsafe {
        web_js_enqueue_audio(buffer.as_ptr(), len);
    }
}

// -------------------------------------------------------------------- I/O

/// Largest record the page sends (an Ethernet frame plus slack)
const IO_RECORD_MAX: usize = 4096;

/// Pop the next record from the page's I/O ring (see
/// [`crate::io_protocol`]); `None` when the ring is empty
pub fn poll_io() -> Option<Vec<u8>> {
    let mut buf = vec![0u8; IO_RECORD_MAX];
    loop {
        let len = unsafe { web_js_poll_io(buf.as_mut_ptr(), IO_RECORD_MAX as u32) };
        match len {
            0 => return None,
            n if n < 0 => {
                log::warn!("Skipping oversized I/O record");
            }
            n => {
                buf.truncate(n as usize);
                return Some(buf);
            }
        }
    }
}

/// Post framed outgoing network packets (see [`snow_core::net::push_frame`])
pub fn net_send(frames: &[u8]) {
    if frames.is_empty() {
        return;
    }
    unsafe {
        web_js_net_send(frames.as_ptr(), u32::try_from(frames.len()).unwrap_or(u32::MAX));
    }
}

// ---------------------------------------------------------------- runtime

/// Initialize the web glue
pub fn init() {
    unsafe {
        web_js_init();
    }
}

/// Block for `seconds` seconds (speed governor)
pub fn sleep_seconds(seconds: f64) {
    unsafe {
        web_js_sleep(seconds);
    }
}

pub fn report_error(message: &str) {
    let Ok(message) = CString::new(message) else {
        log::warn!("Skipping emulator error containing an interior NUL byte");
        return;
    };
    unsafe {
        web_js_report_error(message.as_ptr());
    }
}

pub fn set_clipboard_text(text: &str) {
    let Ok(text) = CString::new(text) else {
        return;
    };
    unsafe {
        web_js_set_clipboard_text(text.as_ptr());
    }
}
