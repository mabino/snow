//! Records sent from the web page (main thread) to the emulator worker
//!
//! The emulator's main loop never returns to the worker's JavaScript event
//! loop, so `postMessage` cannot be used to reach it. Instead the page
//! writes records into a ring buffer in a `SharedArrayBuffer`
//! (`www/snow-io.js`), and the worker glue (`src/web.js`) hands them to Rust
//! one at a time as `[type][payload]`. All multi-byte values are big endian.
//!
//! | type | payload                         | meaning                          |
//! |------|---------------------------------|----------------------------------|
//! | 0x00 | Ethernet frame                  | received from the bridge         |
//! | 0x01 | LToUDP datagram                 | received from the bridge         |
//! | 0x10 | scancode u8, down u8            | key event (M0115 scancode)       |
//! | 0x11 | x u16, y u16                    | absolute mouse position          |
//! | 0x12 | dx i16, dy i16                  | relative mouse movement          |
//! | 0x13 | down u8                         | mouse button                     |
//! | 0x14 | samples u32                     | audio drained (handled in JS)    |
//! | 0x15 | up u8, description (UTF-8)      | bridge link state changed        |
//!
//! The constants must match `www/snow-io.js`.

use snow_core::net::{TAG_ETHERNET, TAG_LOCALTALK};

pub const REC_KEY: u8 = 0x10;
pub const REC_MOUSE_ABS: u8 = 0x11;
pub const REC_MOUSE_REL: u8 = 0x12;
pub const REC_MOUSE_BUTTON: u8 = 0x13;
pub const REC_AUDIO_DRAINED: u8 = 0x14;
pub const REC_LINK: u8 = 0x15;

/// A decoded record
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoRecord {
    /// A packet for the net hub (tag, payload)
    Net(u8, Vec<u8>),
    Key { scancode: u8, down: bool },
    MouseAbs { x: u16, y: u16 },
    MouseRel { dx: i16, dy: i16 },
    MouseButton(bool),
    Link { up: bool, description: String },
}

impl IoRecord {
    /// Decode a `[type][payload]` record; `None` for malformed or unknown
    /// records (which are skipped)
    pub fn parse(rec: &[u8]) -> Option<Self> {
        let (&kind, p) = rec.split_first()?;
        let u16_at = |i: usize| Some(u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]));
        Some(match kind {
            TAG_ETHERNET | TAG_LOCALTALK => Self::Net(kind, p.to_vec()),
            REC_KEY => Self::Key {
                scancode: *p.first()?,
                down: *p.get(1)? != 0,
            },
            REC_MOUSE_ABS => Self::MouseAbs {
                x: u16_at(0)?,
                y: u16_at(2)?,
            },
            REC_MOUSE_REL => Self::MouseRel {
                dx: u16_at(0)? as i16,
                dy: u16_at(2)? as i16,
            },
            REC_MOUSE_BUTTON => Self::MouseButton(*p.first()? != 0),
            REC_LINK => Self::Link {
                up: *p.first()? != 0,
                description: String::from_utf8_lossy(&p[1..]).into_owned(),
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_records() {
        assert_eq!(
            IoRecord::parse(&[TAG_LOCALTALK, 1, 2, 3]),
            Some(IoRecord::Net(TAG_LOCALTALK, vec![1, 2, 3]))
        );
        assert_eq!(
            IoRecord::parse(&[REC_KEY, 0x31, 1]),
            Some(IoRecord::Key { scancode: 0x31, down: true })
        );
        assert_eq!(
            IoRecord::parse(&[REC_MOUSE_ABS, 0x01, 0x00, 0x00, 0xAB]),
            Some(IoRecord::MouseAbs { x: 256, y: 0xAB })
        );
        assert_eq!(
            IoRecord::parse(&[REC_MOUSE_REL, 0xFF, 0xFE, 0x00, 0x03]),
            Some(IoRecord::MouseRel { dx: -2, dy: 3 })
        );
        assert_eq!(
            IoRecord::parse(&[REC_MOUSE_BUTTON, 0]),
            Some(IoRecord::MouseButton(false))
        );
        assert_eq!(
            IoRecord::parse(b"\x15\x01ws://host:8080"),
            Some(IoRecord::Link { up: true, description: "ws://host:8080".into() })
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(IoRecord::parse(&[]), None);
        assert_eq!(IoRecord::parse(&[REC_KEY, 0x31]), None);
        assert_eq!(IoRecord::parse(&[REC_MOUSE_ABS, 0, 1, 2]), None);
        assert_eq!(IoRecord::parse(&[REC_LINK]), None);
        assert_eq!(IoRecord::parse(&[REC_AUDIO_DRAINED, 0, 0, 0, 1]), None);
        assert_eq!(IoRecord::parse(&[0x7F, 1]), None);
    }
}
