//! LocalTalk (AppleTalk on the printer port) over Infinite Mac's zone relay
//!
//! The zone relay carries Ethernet frames between the browsers in a zone.
//! The emulated LocalTalk port exchanges LToUDP datagrams (sender ID + LLAP
//! packet) through the net hub ([`snow_core::net`]); this module wraps each
//! datagram in a broadcast Ethernet frame with an IEEE "local experimental"
//! EtherType, so it reaches every Snow in the zone and is ignored by
//! EtherTalk machines (Basilisk II, SheepShaver) that might share the zone.
//!
//! System 6 only opens its LocalTalk driver when AppleTalk is active in
//! PRAM, so without a PRAM file a PRAM image with AppleTalk active and a
//! random node address hint is generated (identical PRAM would make every
//! Snow pick the same addresses).

use snow_core::net::{self, TAG_LOCALTALK};

use crate::js_api::ethernet;

/// EtherType for LToUDP datagrams on the zone (IEEE 802 local experimental)
pub const ETHERTYPE_LTOUDP: u16 = 0x88B5;
const ETHERNET_HEADER: usize = 14;
const BROADCAST: [u8; 6] = [0xFF; 6];

pub struct LocalTalkLink {
    mac: [u8; 6],
    buf: Vec<u8>,
}

impl LocalTalkLink {
    /// Attach to the zone and mark the link as up
    pub fn start() -> Self {
        let mut mac = random_bytes::<6>();
        mac[0] = (mac[0] & 0xFE) | 0x02; // locally administered, unicast
        let mac_str = mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        ethernet::init(&mac_str);
        net::set_link(true, "Infinite Mac AppleTalk zone");
        log::info!("LocalTalk on the zone relay as {mac_str}");
        Self {
            mac,
            buf: vec![0; 2048],
        }
    }

    /// Move datagrams between the net hub and the zone
    pub fn tick(&mut self) {
        for (tag, dgram) in net::take_outgoing() {
            if tag == TAG_LOCALTALK {
                ethernet::write("*", &wrap(&self.mac, &dgram));
            }
        }
        loop {
            let n = ethernet::read(&mut self.buf);
            if n == 0 {
                break;
            }
            if let Some(dgram) = unwrap(&self.buf[..n]) {
                net::deliver(TAG_LOCALTALK, dgram.to_vec());
            }
        }
    }
}

/// Wrap an LToUDP datagram in a broadcast Ethernet frame
pub fn wrap(src: &[u8; 6], dgram: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETHERNET_HEADER + dgram.len());
    frame.extend_from_slice(&BROADCAST);
    frame.extend_from_slice(src);
    frame.extend_from_slice(&ETHERTYPE_LTOUDP.to_be_bytes());
    frame.extend_from_slice(dgram);
    frame
}

/// The LToUDP datagram in an Ethernet frame, if it carries one
pub fn unwrap(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() <= ETHERNET_HEADER
        || u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_LTOUDP
    {
        return None;
    }
    Some(&frame[ETHERNET_HEADER..])
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        // RandomState is seeded from the OS random source (on the web:
        // crypto.getRandomValues)
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u8(0);
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
    out
}

/// PRAM address of SPConfig (serial port use: low nibble = port B)
const PRAM_SPCONFIG: usize = 0x13;
/// PRAM address of SPATalkB (LocalTalk node address hint for port B)
const PRAM_NODE_HINT_B: usize = 0x12;

/// A 256 byte PRAM image with AppleTalk active on the printer port and a
/// random node address hint in the workstation range (1-127)
///
/// Holds the defaults a Macintosh SE ROM writes on first boot.
pub fn appletalk_pram() -> Vec<u8> {
    let mut pram = vec![0u8; 256];
    for &(addr, val) in &[
        (0x08, 0x03),
        (0x09, 0x88),
        (0x0B, 0x4C),
        (0x0C, b'B'),
        (0x0D, b'u'),
        (0x0E, b'g'),
        (0x0F, b's'),
        (0x10, 0xA8), // SPValid
        (0x14, 0xCC), // SPPortA
        (0x15, 0x0A),
        (0x16, 0xCC), // SPPortB
        (0x17, 0x0A),
        (0x1D, 0x02), // SPKbd
        (0x1E, 0x63), // SPPrint
    ] {
        pram[addr] = val;
    }
    pram[PRAM_SPCONFIG] = 0x01; // useATalk on port B
    pram[PRAM_NODE_HINT_B] = random_bytes::<1>()[0] % 127 + 1;
    pram
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_roundtrip() {
        let mac = [0x02, 1, 2, 3, 4, 5];
        let dgram = [0, 0, 0, 7, 0x4F, 0x4F, 0x81];
        let frame = wrap(&mac, &dgram);
        assert_eq!(&frame[..6], &BROADCAST);
        assert_eq!(&frame[6..12], &mac);
        assert_eq!(unwrap(&frame), Some(&dgram[..]));
    }

    #[test]
    fn unwrap_ignores_other_traffic() {
        let mut ethertalk = wrap(&[2; 6], &[1, 2, 3]);
        ethertalk[12..14].copy_from_slice(&0x809Bu16.to_be_bytes());
        assert_eq!(unwrap(&ethertalk), None);
        assert_eq!(unwrap(&[0u8; 10]), None);
    }

    #[test]
    fn pram_has_appletalk_active() {
        let pram = appletalk_pram();
        assert_eq!(pram.len(), 256);
        assert_eq!(pram[0x10], 0xA8);
        assert_eq!(pram[PRAM_SPCONFIG] & 0x0F, 1);
        assert!((1..=127).contains(&pram[PRAM_NODE_HINT_B]));
    }
}
