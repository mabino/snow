//! Frontend-provided network transport ("net hub")
//!
//! Some frontends have no OS sockets - most notably the WebAssembly build,
//! which runs in a browser Web Worker. There, the emulated network
//! interfaces (the DaynaPORT Ethernet adapter and the LocalTalk serial
//! port) cannot talk to a UDP socket or a TAP device. Instead they exchange
//! packets with this process-wide hub, and the *frontend* moves the packets
//! between the hub and whatever transport it has (for the web frontend: a
//! WebSocket to the `snow-bridge` host process, owned by the page's main
//! thread).
//!
//! The hub is a pair of bounded queues per channel:
//!
//! ```text
//!  device --send()--> outbox --take_outgoing()--> frontend --> wire
//!  device <--recv()-- inbox  <------deliver()---- frontend <-- wire
//! ```
//!
//! Packets sent while the frontend reports the link as down are dropped,
//! just like on a real unplugged network.
//!
//! ## Wire framing
//!
//! Frontends that multiplex the channels over one byte stream use
//! [`push_frame`] / [`next_frame`]:
//!
//! ```text
//! [1 byte channel tag][2 byte length (big endian)][length bytes of payload]
//! ```
//!
//! Channel tags:
//! - [`TAG_ETHERNET`] - a layer 2 Ethernet frame (DaynaPORT adapter)
//! - [`TAG_LOCALTALK`] - a LocalTalk-over-UDP datagram (4 byte sender id +
//!   LLAP packet)

use std::collections::VecDeque;
use std::sync::Mutex;

/// Channel tag for layer 2 Ethernet frames
pub const TAG_ETHERNET: u8 = 0;
/// Channel tag for LocalTalk-over-UDP datagrams
pub const TAG_LOCALTALK: u8 = 1;
/// Number of channels
const CHANNELS: usize = 2;

/// Maximum payload size for a single frame
pub const MAX_FRAME_SIZE: usize = u16::MAX as usize;

/// Maximum number of packets queued per channel and direction
pub const QUEUE_LIMIT: usize = 512;

/// Append a framed packet (tag + length + payload) to a byte stream
///
/// Payloads longer than [`MAX_FRAME_SIZE`] are truncated.
pub fn push_frame(stream: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    let len = u16::try_from(payload.len()).unwrap_or(u16::MAX);
    stream.push(tag);
    stream.extend_from_slice(&len.to_be_bytes());
    stream.extend_from_slice(&payload[..len as usize]);
}

/// Extract the next complete frame from the head of a byte stream of
/// concatenated frames
///
/// Returns `None` if the stream does not (yet) hold a complete frame. The
/// consumed bytes are removed from the stream.
pub fn next_frame(stream: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if stream.len() < 3 {
        return None;
    }
    let len = u16::from_be_bytes([stream[1], stream[2]]) as usize;
    if stream.len() < 3 + len {
        return None;
    }
    let tag = stream[0];
    let payload = stream[3..3 + len].to_vec();
    stream.drain(..3 + len);
    Some((tag, payload))
}

/// Traffic counters of the hub
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetStats {
    /// Packets handed to the frontend for sending
    pub tx: u64,
    /// Packets delivered by the frontend
    pub rx: u64,
    /// Packets dropped (link down, queue full or unknown channel)
    pub dropped: u64,
}

#[derive(Default)]
struct Hub {
    /// Whether the frontend's link is up
    connected: bool,
    /// Human readable description of the link (for the UI)
    description: String,
    inbox: [VecDeque<Vec<u8>>; CHANNELS],
    outbox: VecDeque<(u8, Vec<u8>)>,
    stats: NetStats,
}

static HUB: Mutex<Option<Hub>> = Mutex::new(None);

fn with_hub<R>(f: impl FnOnce(&mut Hub) -> R) -> R {
    let mut guard = HUB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Hub::default))
}

fn channel(tag: u8) -> Option<usize> {
    let ch = usize::from(tag);
    (ch < CHANNELS).then_some(ch)
}

/// Frontend: report whether the link is up, with a description for the UI
///
/// Taking the link down discards all queued packets.
pub fn set_link(connected: bool, description: &str) {
    with_hub(|hub| {
        if hub.connected != connected {
            log::info!(
                "Network link {}: {description}",
                if connected { "up" } else { "down" }
            );
        }
        if !connected {
            hub.inbox.iter_mut().for_each(VecDeque::clear);
            hub.outbox.clear();
        }
        hub.connected = connected;
        hub.description = description.to_string();
    });
}

/// Whether the frontend's link is up
pub fn is_connected() -> bool {
    with_hub(|hub| hub.connected)
}

/// Device: queue a packet for sending (dropped if the link is down)
pub fn send(tag: u8, payload: &[u8]) {
    with_hub(|hub| {
        if !hub.connected || payload.is_empty() || hub.outbox.len() >= QUEUE_LIMIT * CHANNELS {
            hub.stats.dropped += 1;
            return;
        }
        hub.outbox.push_back((tag, payload.to_vec()));
        hub.stats.tx += 1;
    });
}

/// Device: receive the next packet for a channel
pub fn recv(tag: u8) -> Option<Vec<u8>> {
    let ch = channel(tag)?;
    with_hub(|hub| hub.inbox[ch].pop_front())
}

/// Frontend: take all packets queued for sending
pub fn take_outgoing() -> Vec<(u8, Vec<u8>)> {
    with_hub(|hub| hub.outbox.drain(..).collect())
}

/// Frontend: deliver a received packet to a channel
pub fn deliver(tag: u8, payload: Vec<u8>) {
    with_hub(|hub| {
        let Some(ch) = channel(tag) else {
            hub.stats.dropped += 1;
            return;
        };
        if hub.inbox[ch].len() >= QUEUE_LIMIT {
            hub.stats.dropped += 1;
            return;
        }
        hub.inbox[ch].push_back(payload);
        hub.stats.rx += 1;
    });
}

/// Traffic counters
pub fn stats() -> NetStats {
    with_hub(|hub| hub.stats)
}

/// A human readable status string for UI display
pub fn status() -> String {
    with_hub(|hub| {
        format!(
            "{} ({}) [rx {} / tx {} / dropped {}]",
            if hub.connected { "connected" } else { "disconnected" },
            if hub.description.is_empty() {
                "no transport"
            } else {
                &hub.description
            },
            hub.stats.rx,
            hub.stats.tx,
            hub.stats.dropped
        )
    })
}

/// Reset the hub to its initial state (link down, queues and counters
/// cleared)
pub fn reset() {
    *HUB.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The hub is process-global; tests that use it must not run in
    /// parallel with each other
    pub static HUB_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn frame_roundtrip() {
        let mut stream = Vec::new();
        push_frame(&mut stream, TAG_ETHERNET, b"hello world");
        push_frame(&mut stream, TAG_LOCALTALK, b"lt");

        let (tag, payload) = next_frame(&mut stream).unwrap();
        assert_eq!(tag, TAG_ETHERNET);
        assert_eq!(payload, b"hello world");

        let (tag, payload) = next_frame(&mut stream).unwrap();
        assert_eq!(tag, TAG_LOCALTALK);
        assert_eq!(payload, b"lt");

        assert!(next_frame(&mut stream).is_none());
        assert!(stream.is_empty());
    }

    #[test]
    fn frame_incomplete() {
        let mut stream = Vec::new();
        push_frame(&mut stream, TAG_ETHERNET, &[1, 2, 3, 4, 5]);
        stream.truncate(stream.len() - 2);
        assert!(next_frame(&mut stream).is_none());
        stream.extend_from_slice(&[4, 5]);
        let (tag, payload) = next_frame(&mut stream).unwrap();
        assert_eq!(tag, TAG_ETHERNET);
        assert_eq!(payload, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn frame_header_only() {
        let mut stream = vec![TAG_LOCALTALK, 0];
        assert!(next_frame(&mut stream).is_none());
        stream.push(0);
        assert_eq!(next_frame(&mut stream), Some((TAG_LOCALTALK, vec![])));
    }

    #[test]
    fn frame_truncates_oversize() {
        let mut stream = Vec::new();
        push_frame(&mut stream, TAG_ETHERNET, &vec![0xAB; MAX_FRAME_SIZE + 10]);
        let (_, payload) = next_frame(&mut stream).unwrap();
        assert_eq!(payload.len(), MAX_FRAME_SIZE);
    }

    #[test]
    fn hub_drops_while_down() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        reset();
        send(TAG_LOCALTALK, b"x");
        assert!(take_outgoing().is_empty());
        assert_eq!(stats().dropped, 1);
        assert!(!is_connected());
    }

    #[test]
    fn hub_keeps_channels_separate() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        reset();
        set_link(true, "test");
        deliver(TAG_ETHERNET, b"eth1".to_vec());
        deliver(TAG_LOCALTALK, b"lt1".to_vec());
        deliver(TAG_ETHERNET, b"eth2".to_vec());
        deliver(TAG_LOCALTALK, b"lt2".to_vec());

        // Draining one channel must not lose the other channel's packets
        assert_eq!(recv(TAG_LOCALTALK).unwrap(), b"lt1");
        assert_eq!(recv(TAG_LOCALTALK).unwrap(), b"lt2");
        assert!(recv(TAG_LOCALTALK).is_none());
        assert_eq!(recv(TAG_ETHERNET).unwrap(), b"eth1");
        assert_eq!(recv(TAG_ETHERNET).unwrap(), b"eth2");
        assert!(recv(TAG_ETHERNET).is_none());

        send(TAG_LOCALTALK, b"out");
        send(TAG_ETHERNET, b"out2");
        assert_eq!(
            take_outgoing(),
            vec![(TAG_LOCALTALK, b"out".to_vec()), (TAG_ETHERNET, b"out2".to_vec())]
        );
        assert_eq!(stats(), NetStats { tx: 2, rx: 4, dropped: 0 });
        reset();
    }

    #[test]
    fn hub_bounds_queues() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        reset();
        set_link(true, "test");
        for _ in 0..QUEUE_LIMIT + 5 {
            deliver(TAG_LOCALTALK, vec![1]);
        }
        deliver(9, vec![1]); // unknown channel
        assert_eq!(stats().dropped, 6);
        set_link(false, "gone");
        assert!(recv(TAG_LOCALTALK).is_none(), "link down clears queues");
        reset();
    }
}
