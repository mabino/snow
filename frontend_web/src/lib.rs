//! Platform-independent parts of the standalone web frontend
//!
//! Everything in here is plain Rust (no JavaScript imports), so it is unit
//! tested natively with `cargo test -p snow_frontend_web --lib`.

pub mod io_protocol;
pub mod media;
