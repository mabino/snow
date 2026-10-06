//! LocalTalk over UDP (LToUDP) bridge
//!
//! This module implements the LocalTalk-over-UDP protocol, allowing emulated
//! Macs to communicate with each other over a LAN using UDP multicast.
//!
//! Protocol specification: https://windswept.home.blog/2019/12/10/localtalk-over-udp/
//!
//! Key points:
//! - UDP port 1954, multicast group 239.192.76.84
//! - Packets are LLAP frames prefixed with a 4-byte sender ID
//! - RTS/CTS collision avoidance is handled locally (not sent over network)
//!
//! The wire transport is abstracted behind the [`LtopIo`] trait:
//! - Native builds use a local UDP socket joined to the multicast group.
//! - Web (Emscripten) builds exchange the datagrams with the frontend through
//!   the net hub ([`crate::net`], [`HubTransport`]); the web page relays them
//!   to the `snow-bridge` host process over a WebSocket, so browser instances
//!   can AppleTalk with each other and with real Macs on the LAN.

use std::collections::VecDeque;
use std::io;

#[cfg(not(target_os = "emscripten"))]
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
#[cfg(not(target_os = "emscripten"))]
use socket2::{Domain, Protocol, Socket, Type};

use log::*;

/// LocalTalk over UDP port
pub const LTOUDP_PORT: u16 = 1954;

/// LocalTalk over UDP multicast address (239.192.76.84)
pub const LTOUDP_MULTICAST: [u8; 4] = [239, 192, 76, 84];

/// Maximum number of received packets waiting for the SCC
const RX_QUEUE_LIMIT: usize = 64;

/// Maximum LLAP packet size (3 byte header + 597 byte data)
pub const MAX_LLAP_SIZE: usize = 600;

/// LLAP packet types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LlapType {
    /// DDP short header (data packet)
    DdpShort = 0x01,
    /// DDP long header (data packet)
    DdpLong = 0x02,
    /// Node ID probe during address acquisition
    LapEnq = 0x81,
    /// Response to ENQ (address collision)
    LapAck = 0x82,
    /// Request to send (collision avoidance)
    LapRts = 0x84,
    /// Clear to send (collision avoidance)
    LapCts = 0x85,
}

impl TryFrom<u8> for LlapType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::DdpShort),
            0x02 => Ok(Self::DdpLong),
            0x81 => Ok(Self::LapEnq),
            0x82 => Ok(Self::LapAck),
            0x84 => Ok(Self::LapRts),
            0x85 => Ok(Self::LapCts),
            _ => Err(value),
        }
    }
}

/// Status of the LocalTalk bridge
#[derive(Debug, Clone)]
pub struct LocalTalkStatus {
    /// Our node address (0 = not yet assigned)
    pub node_address: u8,
    /// Number of packets transmitted
    pub tx_packets: u64,
    /// Number of packets received
    pub rx_packets: u64,
}

impl std::fmt::Display for LocalTalkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LocalTalk (node {}, tx:{} rx:{})",
            if self.node_address == 0 {
                "?".to_string()
            } else {
                self.node_address.to_string()
            },
            self.tx_packets,
            self.rx_packets
        )
    }
}

/// Transport for LToUDP datagrams
///
/// A complete LToUDP datagram is a 4-byte (big endian) sender ID followed by
/// the LLAP packet, exactly as it appears on the LToUDP wire.
pub trait LtopIo: Send {
    /// Send a complete LToUDP datagram
    fn send(&mut self, datagram: &[u8]) -> io::Result<usize>;

    /// Non-blocking receive of one LToUDP datagram.
    ///
    /// Returns the number of bytes read, `0` when no datagram is available
    /// right now, or an error.
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Whether the transport is currently connected/usable
    fn is_connected(&self) -> bool;
}

/// UDP multicast transport (native platforms)
#[cfg(not(target_os = "emscripten"))]
struct UdpTransport {
    socket: UdpSocket,
}

#[cfg(not(target_os = "emscripten"))]
impl UdpTransport {
    /// Bind the LToUDP port and join the multicast group
    fn bind() -> io::Result<Self> {
        // Create UDP socket with socket2 so we can set options before binding
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Enable address reuse for multiple instances on same machine
        socket.set_reuse_address(true)?;
        #[cfg(not(target_os = "windows"))]
        if let Err(e) = socket.set_reuse_port(true) {
            warn!("SO_REUSEPORT failed: {}", e);
        }

        let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LTOUDP_PORT);
        socket.bind(&addr.into())?;

        let socket: UdpSocket = socket.into();
        socket.join_multicast_v4(&Ipv4Addr::from(LTOUDP_MULTICAST), &Ipv4Addr::UNSPECIFIED)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket })
    }
}

#[cfg(not(target_os = "emscripten"))]
impl LtopIo for UdpTransport {
    fn send(&mut self, datagram: &[u8]) -> io::Result<usize> {
        let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(LTOUDP_MULTICAST), LTOUDP_PORT));
        self.socket.send_to(datagram, dest)
    }

    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.socket.recv_from(buf) {
            Ok((n, _)) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn is_connected(&self) -> bool {
        true
    }
}

/// Net hub transport: datagrams are exchanged with the frontend through
/// [`crate::net`] (used by the web build, where the page relays them to the
/// host bridge process over a WebSocket)
pub struct HubTransport;

impl LtopIo for HubTransport {
    fn send(&mut self, datagram: &[u8]) -> io::Result<usize> {
        // The hub drops the datagram if the link is down, like an
        // unplugged LocalTalk cable
        crate::net::send(crate::net::TAG_LOCALTALK, datagram);
        Ok(datagram.len())
    }

    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match crate::net::recv(crate::net::TAG_LOCALTALK) {
            None => Ok(0),
            Some(dgram) => {
                let n = dgram.len().min(buf.len());
                buf[..n].copy_from_slice(&dgram[..n]);
                Ok(n)
            }
        }
    }

    fn is_connected(&self) -> bool {
        crate::net::is_connected()
    }
}

/// LocalTalk bridge
pub struct LocalTalkBridge {
    /// Wire transport (UDP multicast on native, the net hub on the web)
    transport: Box<dyn LtopIo>,
    /// Sender ID for loopback detection (unique per node)
    sender_id: u32,
    /// Our node address (set from SCC WR6, also learned from outgoing packets)
    node_address: u8,
    /// Z8530 SDLC address search mode (hardware destination filter)
    /// When ON: only accept packets for our address or broadcast (0xFF)
    /// When OFF: accept all packets (Mac firmware handles its own filtering)
    address_search_mode: bool,
    /// Queue of pending CTS responses to inject (dest, src)
    pending_cts: VecDeque<(u8, u8)>,
    /// Buffer for accumulating TX data from SCC
    tx_buffer: Vec<u8>,
    /// Buffer for received packets to inject into SCC
    rx_queue: Vec<Vec<u8>>,
    /// Statistics
    tx_packets: u64,
    rx_packets: u64,
}

impl LocalTalkBridge {
    /// Create a new LocalTalk bridge using the platform's default transport
    /// (UDP multicast on native platforms, the net hub on the web)
    pub fn new() -> io::Result<Self> {
        #[cfg(not(target_os = "emscripten"))]
        {
            Ok(Self::with_transport(
                Box::new(UdpTransport::bind()?),
                std::process::id(),
                "LToUDP",
            ))
        }

        #[cfg(target_os = "emscripten")]
        {
            // Random sender ID: separate browser tabs must not filter each
            // other's packets as loopback
            Ok(Self::with_transport(
                Box::new(HubTransport),
                rand::random(),
                "LToUDP via network bridge",
            ))
        }
    }

    /// Create a LocalTalk bridge on top of a specific transport
    pub fn with_transport(transport: Box<dyn LtopIo>, sender_id: u32, name: &str) -> Self {
        info!("LocalTalk bridge started ({name}), sender_id={sender_id:08X}");
        Self {
            transport,
            sender_id,
            node_address: 0,
            address_search_mode: false,
            pending_cts: VecDeque::new(),
            rx_queue: Vec::new(),
            tx_buffer: Vec::new(),
            tx_packets: 0,
            rx_packets: 0,
        }
    }

    /// Get current bridge status
    pub fn status(&self) -> LocalTalkStatus {
        LocalTalkStatus {
            node_address: self.node_address,
            tx_packets: self.tx_packets,
            rx_packets: self.rx_packets,
        }
    }

    /// Whether the underlying transport is connected
    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    /// Handle a complete LLAP packet from the SCC TX path
    pub fn handle_tx_packet(&mut self, llap: &[u8]) {
        if llap.len() < 3 {
            warn!("LocalTalk: TX packet too small ({} bytes)", llap.len());
            return;
        }

        let dest = llap[0];
        let src = llap[1];
        let ptype = llap[2];

        // Track our node address from outgoing packets
        if src != 0 && src != 0xFF {
            self.node_address = src;
        }

        match ptype {
            0x84 => {
                // lapRTS - Request to send
                // Don't send over network - synthesize the CTS response of a
                // directed RTS locally. A broadcast RTS gets no CTS: the
                // sender just waits for the line to stay idle and then sends
                // the data frame; a CTS arriving in that window looks like a
                // collision, so the driver would retry and finally give up
                // (no NBP lookups, RTMP etc. would ever be sent).
                if dest != 0xFF {
                    self.pending_cts.push_back((src, dest));
                }
            }
            0x85 => {
                // lapCTS - Clear to send
                // Don't send CTS over network
            }
            _ => {
                // All other packets (data, ENQ, ACK) are sent over the wire
                self.send_wire(llap);
            }
        }
    }

    /// Send an LLAP packet over the wire (with sender ID prefix)
    fn send_wire(&mut self, llap: &[u8]) {
        // Build datagram: 4-byte sender ID (big-endian) + LLAP data
        let mut packet = Vec::with_capacity(4 + llap.len());
        packet.extend_from_slice(&self.sender_id.to_be_bytes());
        packet.extend_from_slice(llap);

        match self.transport.send(&packet) {
            Ok(_) => {
                self.tx_packets += 1;
            }
            Err(e) => {
                warn!("LocalTalk: send error: {}", e);
            }
        }
    }

    /// Poll for incoming datagrams and state changes
    /// Returns true if there's data available for the SCC
    pub fn poll(&mut self) -> bool {
        let mut buf = [0u8; 4 + MAX_LLAP_SIZE + 64]; // Extra space for safety
        let mut received_any = false;

        // Receive all pending datagrams
        loop {
            match self.transport.recv(&mut buf) {
                Ok(0) => break, // No data available right now
                Ok(len) => {
                    if len < 4 + 3 {
                        // Too small: need at least sender_id (4) + LLAP header (3)
                        continue;
                    }

                    // Extract sender ID
                    let packet_sender_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);

                    // Loopback detection: skip packets from ourselves
                    if packet_sender_id == self.sender_id {
                        continue;
                    }

                    // Extract LLAP packet
                    let llap = &buf[4..len];
                    self.handle_rx_packet(llap);
                    received_any = true;
                }
                Err(e) => {
                    warn!("LocalTalk: recv error: {}", e);
                    break;
                }
            }
        }

        received_any || !self.pending_cts.is_empty() || !self.rx_queue.is_empty()
    }

    /// Handle a received LLAP packet from the network
    fn handle_rx_packet(&mut self, llap: &[u8]) {
        if llap.len() < 3 {
            return;
        }

        let dest = llap[0];

        // Destination filtering (Z8530 SDLC address search mode) is done by
        // the SCC when it picks up the frame: the station address and search
        // mode can change between now and then (LLAP address acquisition
        // toggles them around every transmission), so filtering here with a
        // stale view would let foreign frames through or drop wanted ones.

        let ptype = llap[2];
        let src = llap[1];

        // If we have a settled address and someone ENQs for it, respond with ACK
        // ("that address is taken"). Still queue the ENQ for the SCC as well.
        if ptype == 0x81
            && dest == self.node_address
            && self.node_address != 0
            && !self.address_search_mode
        {
            let ack = vec![src, self.node_address, 0x82];
            self.send_wire(&ack);
        }

        // Queue the packet for injection into SCC (bounded: a guest that
        // keeps its receiver off must not make the queue grow forever)
        if self.rx_queue.len() >= RX_QUEUE_LIMIT {
            self.rx_queue.remove(0);
        }
        self.rx_queue.push(llap.to_vec());
        self.rx_packets += 1;
    }

    /// Set the node address (from SCC WR6 / SDLC station address)
    pub fn set_node_address(&mut self, addr: u8) {
        if addr != 0 {
            self.node_address = addr;
        }
    }

    /// Set the SDLC address search mode (from SCC WR3 bit 2)
    pub fn set_address_search_mode(&mut self, mode: bool) {
        self.address_search_mode = mode;
    }

    /// Send a complete LLAP frame (from SDLC TX frame boundary detection)
    pub fn send_frame(&mut self, llap: &[u8]) {
        self.handle_tx_packet(llap);
    }

    /// Read data to inject into the SCC RX path
    /// Returns LLAP packet data (one packet at a time)
    pub fn read_to_scc(&mut self) -> Option<Vec<u8>> {
        // First, return any pending CTS response
        if let Some((dest, src)) = self.pending_cts.pop_front() {
            let cts = vec![dest, src, 0x85]; // CTS packet
            return Some(cts);
        }

        // Then return queued packets from the network
        if !self.rx_queue.is_empty() {
            return Some(self.rx_queue.remove(0));
        }

        None
    }

    /// Get the number of packets waiting in the RX queue
    pub fn rx_queue_len(&self) -> usize {
        self.rx_queue.len()
    }

    /// Write data from the SCC TX path
    /// Accumulates bytes and extracts LLAP packets
    pub fn write_from_scc(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        self.tx_buffer.extend_from_slice(data);
        self.try_extract_packets();
    }

    /// Try to extract complete LLAP packets from the TX buffer
    fn try_extract_packets(&mut self) {
        while self.tx_buffer.len() >= 3 {
            let ptype = self.tx_buffer[2];

            // Determine expected packet length
            let packet_len = if ptype >= 0x80 {
                // Control packet (RTS, CTS, ENQ, ACK) - always 3 bytes
                3
            } else {
                // Data packet - need to parse DDP length
                // Check if we have enough for DDP header
                if self.tx_buffer.len() < 5 {
                    break;
                }

                // DDP length is in the first 10 bits of the 2-byte field at offset 3
                let ddp_len =
                    (((self.tx_buffer[3] as usize) & 0x03) << 8) | (self.tx_buffer[4] as usize);

                // Total packet = 3 (LLAP) + ddp_len
                if ddp_len == 0 || ddp_len > MAX_LLAP_SIZE - 3 {
                    // Invalid length - skip byte and retry
                    self.tx_buffer.remove(0);
                    continue;
                }

                3 + ddp_len
            };

            // Check if we have the complete packet
            if self.tx_buffer.len() < packet_len {
                break;
            }

            // Extract the packet
            let packet: Vec<u8> = self.tx_buffer.drain(..packet_len).collect();
            self.handle_tx_packet(&packet);
        }

        // If buffer gets too large without extracting packets, something is wrong
        if self.tx_buffer.len() > MAX_LLAP_SIZE * 2 {
            warn!(
                "LocalTalk: TX buffer overflow ({} bytes), clearing",
                self.tx_buffer.len()
            );
            self.tx_buffer.clear();
        }
    }

    /// Flush any pending TX data (called on frame boundary)
    pub fn flush_tx(&mut self) {
        if !self.tx_buffer.is_empty() {
            let packet = std::mem::take(&mut self.tx_buffer);
            if packet.len() >= 3 {
                self.handle_tx_packet(&packet);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net;
    use crate::net::tests::HUB_TEST_LOCK;

    const SENDER: u32 = 0x1234_5678;

    /// Create a bridge on the net hub transport (no sockets needed)
    fn test_bridge() -> Option<LocalTalkBridge> {
        Some(LocalTalkBridge::with_transport(
            Box::new(HubTransport),
            SENDER,
            "test",
        ))
    }

    #[test]
    fn test_hub_tx_prefixes_sender_id() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        net::reset();
        net::set_link(true, "test");
        let mut bridge = test_bridge().unwrap();

        // ENQ for node 0x4F (what System 6 sends while acquiring an address)
        bridge.send_frame(&[0x4F, 0x4F, 0x81]);
        // RTS/CTS are handled locally and never reach the wire
        bridge.send_frame(&[0x10, 0x4F, 0x84]);

        let out = net::take_outgoing();
        assert_eq!(
            out,
            vec![(net::TAG_LOCALTALK, vec![0x12, 0x34, 0x56, 0x78, 0x4F, 0x4F, 0x81])]
        );
        assert_eq!(bridge.status().tx_packets, 1);
        net::reset();
    }

    #[test]
    fn test_hub_rx_filters_own_datagrams() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        net::reset();
        net::set_link(true, "test");
        let mut bridge = test_bridge().unwrap();

        // Our own datagram echoed back (multicast loopback) is ignored
        net::deliver(net::TAG_LOCALTALK, vec![0x12, 0x34, 0x56, 0x78, 0xFF, 0x4F, 0x01]);
        // Another node's broadcast, plus a runt, plus a second datagram
        net::deliver(net::TAG_LOCALTALK, vec![0, 0, 0, 1, 0xFF, 0x20, 0x01, 0xAA]);
        net::deliver(net::TAG_LOCALTALK, vec![0, 0, 0, 1, 0xFF]);
        net::deliver(net::TAG_LOCALTALK, vec![0, 0, 0, 1, 0xFF, 0x21, 0x01, 0xBB]);

        assert!(bridge.poll());
        assert_eq!(bridge.read_to_scc().unwrap(), [0xFF, 0x20, 0x01, 0xAA]);
        assert_eq!(bridge.read_to_scc().unwrap(), [0xFF, 0x21, 0x01, 0xBB]);
        assert!(bridge.read_to_scc().is_none());
        net::reset();
    }

    #[test]
    fn test_hub_answers_enq_for_our_address() {
        let _guard = HUB_TEST_LOCK.lock().unwrap();
        net::reset();
        net::set_link(true, "test");
        let mut bridge = test_bridge().unwrap();
        bridge.set_node_address(0x4F);

        // Another node probes for our address: we must ACK ("taken")
        net::deliver(net::TAG_LOCALTALK, vec![0, 0, 0, 9, 0x4F, 0x4F, 0x81]);
        bridge.poll();
        assert_eq!(
            net::take_outgoing(),
            vec![(net::TAG_LOCALTALK, vec![0x12, 0x34, 0x56, 0x78, 0x4F, 0x4F, 0x82])]
        );
        net::reset();
    }

    #[test]
    fn test_llap_type_conversion() {
        assert_eq!(LlapType::try_from(0x81), Ok(LlapType::LapEnq));
        assert_eq!(LlapType::try_from(0x84), Ok(LlapType::LapRts));
        assert!(LlapType::try_from(0x99).is_err());
    }

    #[test]
    fn test_rx_queues_everything_for_the_scc() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };

        // Destination filtering is the SCC's job (at frame pickup time)
        bridge.set_node_address(42);
        bridge.set_address_search_mode(true);
        bridge.handle_rx_packet(&[42, 10, 0x01]);
        bridge.handle_rx_packet(&[0xFF, 10, 0x01]);
        bridge.handle_rx_packet(&[99, 10, 0x01]);
        assert_eq!(bridge.rx_queue.len(), 3);
    }

    #[test]
    fn test_rx_queue_is_bounded() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };
        for i in 0..RX_QUEUE_LIMIT + 10 {
            bridge.handle_rx_packet(&[0xFF, i as u8, 0x01]);
        }
        assert_eq!(bridge.rx_queue.len(), RX_QUEUE_LIMIT);
        // The oldest packets were dropped
        assert_eq!(bridge.read_to_scc().unwrap()[1], 10);
    }

    #[test]
    fn test_rx_too_small_packet() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };

        // Packets smaller than 3 bytes should be dropped
        bridge.handle_rx_packet(&[42, 10]);
        assert_eq!(bridge.rx_queue.len(), 0);

        bridge.handle_rx_packet(&[]);
        assert_eq!(bridge.rx_queue.len(), 0);
    }

    #[test]
    fn test_set_node_address_ignores_zero() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };

        bridge.set_node_address(42);
        assert_eq!(bridge.node_address, 42);

        // Setting to 0 should be ignored
        bridge.set_node_address(0);
        assert_eq!(bridge.node_address, 42);
    }

    #[test]
    fn test_tx_broadcast_rts_gets_no_cts() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };

        bridge.handle_tx_packet(&[0xFF, 42, 0x84]);
        assert!(bridge.pending_cts.is_empty());
    }

    #[test]
    fn test_tx_rts_generates_cts() {
        let Some(mut bridge) = test_bridge() else {
            return;
        };

        // RTS from node 42 to node 10 should generate CTS
        bridge.handle_tx_packet(&[10, 42, 0x84]);
        assert_eq!(bridge.pending_cts.len(), 1);
        let (dest, src) = bridge.pending_cts[0];
        assert_eq!(dest, 42); // CTS goes back to sender
        assert_eq!(src, 10);
    }
}
