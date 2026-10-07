//! Validation of frames received from clients
//!
//! Clients are untrusted: only well-formed frames are relayed or handed to
//! the NAT engine.

/// LLAP header: destination, source, type
const LLAP_HEADER: usize = 3;
/// LLAP data packet: header + up to 600 bytes of DDP
const LLAP_MAX: usize = LLAP_HEADER + 600;
/// LToUDP sender ID prefix
pub const SENDER_ID_LEN: usize = 4;

/// LLAP control packet types (lapENQ, lapACK, lapRTS, lapCTS)
const LLAP_CONTROL: [u8; 4] = [0x81, 0x82, 0x84, 0x85];

/// Smallest and largest Ethernet frame accepted (without FCS)
const ETHERNET_MIN: usize = 14;
const ETHERNET_MAX: usize = 1514;

/// Whether `dgram` is a plausible LToUDP datagram: a sender ID followed by
/// an LLAP packet (a 3 byte control packet or a data packet with a DDP
/// length that matches)
pub fn valid_localtalk(dgram: &[u8]) -> bool {
    if dgram.len() < SENDER_ID_LEN + LLAP_HEADER || dgram.len() > SENDER_ID_LEN + LLAP_MAX {
        return false;
    }
    let llap = &dgram[SENDER_ID_LEN..];
    let ptype = llap[2];
    if ptype >= 0x80 {
        return LLAP_CONTROL.contains(&ptype) && llap.len() == LLAP_HEADER;
    }
    // Data packet: types 1 (short DDP) and 2 (long DDP); the low 10 bits
    // of the first DDP word are the DDP length
    if !(ptype == 1 || ptype == 2) || llap.len() < LLAP_HEADER + 2 {
        return false;
    }
    let ddp_len = (usize::from(llap[3] & 0x03) << 8) | usize::from(llap[4]);
    ddp_len == llap.len() - LLAP_HEADER
}

/// Whether `frame` is a plausible Ethernet frame
pub fn valid_ethernet(frame: &[u8]) -> bool {
    (ETHERNET_MIN..=ETHERNET_MAX).contains(&frame.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localtalk_control_packets() {
        assert!(valid_localtalk(&[1, 2, 3, 4, 0x4F, 0x4F, 0x81]));
        assert!(valid_localtalk(&[1, 2, 3, 4, 0x20, 0x4F, 0x84]));
        assert!(
            !valid_localtalk(&[1, 2, 3, 4, 0x4F, 0x4F, 0x81, 0]),
            "control is 3 bytes"
        );
        assert!(
            !valid_localtalk(&[1, 2, 3, 4, 0x4F, 0x4F, 0x99]),
            "unknown type"
        );
        assert!(!valid_localtalk(&[1, 2, 3, 4, 0x4F]), "too short");
    }

    #[test]
    fn localtalk_data_packets() {
        // RTMP request as sent by System 6: short DDP, length 6
        assert!(valid_localtalk(&[
            0, 0, 0, 1, 0xFF, 0x21, 0x01, 0x00, 0x06, 0x01, 0x01, 0x05, 0x01
        ]));
        // Length field disagrees with the packet
        assert!(!valid_localtalk(&[
            0, 0, 0, 1, 0xFF, 0x21, 0x01, 0x00, 0x09, 0x01, 0x01, 0x05, 0x01
        ]));
        // Unknown LLAP type
        assert!(!valid_localtalk(&[
            0, 0, 0, 1, 0xFF, 0x21, 0x03, 0x00, 0x02
        ]));
        let mut big = vec![0, 0, 0, 1, 0xFF, 0x21, 0x02];
        big.extend(std::iter::repeat_n(0, 700));
        assert!(!valid_localtalk(&big), "oversized");
    }

    #[test]
    fn ethernet_sizes() {
        assert!(valid_ethernet(&[0; 60]));
        assert!(valid_ethernet(&[0; 1514]));
        assert!(!valid_ethernet(&[0; 13]));
        assert!(!valid_ethernet(&[0; 1515]));
    }
}
