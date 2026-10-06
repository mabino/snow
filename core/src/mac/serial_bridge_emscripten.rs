//! Serial port bridge for Emscripten builds
//!
//! This module mirrors [`serial_bridge`](super::serial_bridge) for the web
//! target. The LocalTalk bridge is fully functional: it exchanges LocalTalk
//! (LToUDP) datagrams with the frontend through the net hub (see
//! [`crate::net`]); the web frontend relays them to the `snow-bridge` host
//! process, so browser instances can use AppleTalk with each other and with
//! LToUDP nodes on the LAN.
//!
//! The PTY and TCP serial bridges are not available on the web (they require
//! OS sockets) and act as inert stubs.

use std::io;
use std::path::PathBuf;

use log::warn;
use serde::{Deserialize, Serialize};

use super::localtalk_bridge::LocalTalkStatus;

/// Configuration for a serial bridge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SerialBridgeConfig {
    /// Create a PTY (pseudo-terminal) - Unix only, stub on the web
    Pty,
    /// Listen on a TCP port - stub on the web
    Tcp(u16),
    /// LocalTalk through the frontend's network bridge (net hub)
    LocalTalk,
}

impl std::fmt::Display for SerialBridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pty => write!(f, "PTY"),
            Self::Tcp(port) => write!(f, "TCP:{port}"),
            Self::LocalTalk => write!(f, "LocalTalk"),
        }
    }
}

/// Status of an active serial bridge
#[derive(Debug, Clone)]
pub enum SerialBridgeStatus {
    /// PTY bridge active, with path to slave device
    Pty(PathBuf),
    /// TCP bridge listening on a port
    TcpListening(u16),
    /// TCP bridge with connected client
    TcpConnected(u16, String),
    /// LocalTalk bridge active
    LocalTalk(LocalTalkStatus),
}

impl std::fmt::Display for SerialBridgeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pty(path) => write!(f, "PTY: {}", path.display()),
            Self::TcpListening(port) => write!(f, "TCP:{port} (listening)"),
            Self::TcpConnected(port, addr) => write!(f, "TCP:{port} ({addr})"),
            Self::LocalTalk(status) => write!(f, "{status}"),
        }
    }
}

/// A serial bridge that does nothing (PTY/TCP are not available on the web)
pub struct InertBridge {
    config: SerialBridgeConfig,
}

impl InertBridge {
    fn new(config: &SerialBridgeConfig) -> io::Result<Self> {
        warn!("Serial bridge {config} is not available on the web, creating inert bridge");
        Ok(Self {
            config: config.clone(),
        })
    }

    fn read_to_scc(&self) -> Vec<u8> {
        Vec::new()
    }

    fn write_from_scc(&self, _data: &[u8]) {}

    fn status(&self) -> SerialBridgeStatus {
        match &self.config {
            SerialBridgeConfig::Pty => {
                SerialBridgeStatus::Pty(PathBuf::from("pty (unavailable on web)"))
            }
            SerialBridgeConfig::Tcp(port) => SerialBridgeStatus::TcpListening(*port),
            SerialBridgeConfig::LocalTalk => unreachable!(),
        }
    }

    fn poll(&self) -> bool {
        false
    }
}

/// Unified bridge that can be either serial (stub) or LocalTalk
pub enum SccBridge {
    /// Serial bridge (inert on the web)
    Serial(InertBridge),
    /// LocalTalk over the network bridge
    LocalTalk(super::localtalk_bridge::LocalTalkBridge),
}

impl SccBridge {
    /// Create a new bridge with the given configuration
    pub fn new(config: &SerialBridgeConfig) -> io::Result<Self> {
        match config {
            SerialBridgeConfig::LocalTalk => Ok(Self::LocalTalk(
                super::localtalk_bridge::LocalTalkBridge::new()?,
            )),
            _ => Ok(Self::Serial(InertBridge::new(config)?)),
        }
    }

    /// Write data from the SCC TX queue to the bridge
    pub fn write_from_scc(&mut self, data: &[u8]) {
        match self {
            Self::Serial(bridge) => bridge.write_from_scc(data),
            Self::LocalTalk(bridge) => bridge.write_from_scc(data),
        }
    }

    /// Read data from the bridge to inject into SCC RX queue
    pub fn read_to_scc(&mut self) -> Vec<u8> {
        match self {
            Self::Serial(bridge) => bridge.read_to_scc(),
            Self::LocalTalk(bridge) => bridge.read_to_scc().unwrap_or_default(),
        }
    }

    /// Poll for state changes
    pub fn poll(&mut self) -> bool {
        match self {
            Self::Serial(bridge) => bridge.poll(),
            Self::LocalTalk(bridge) => bridge.poll(),
        }
    }

    /// Get current bridge status
    pub fn status(&self) -> SerialBridgeStatus {
        match self {
            Self::Serial(bridge) => bridge.status(),
            Self::LocalTalk(bridge) => SerialBridgeStatus::LocalTalk(bridge.status()),
        }
    }

    /// Check if this bridge is a LocalTalk bridge
    pub fn is_localtalk(&self) -> bool {
        matches!(self, Self::LocalTalk(_))
    }

    /// Set the node address (LocalTalk only)
    pub fn set_node_address(&mut self, addr: u8) {
        if let Self::LocalTalk(bridge) = self {
            bridge.set_node_address(addr);
        }
    }

    /// Set the SDLC address search mode (LocalTalk only)
    pub fn set_address_search_mode(&mut self, mode: bool) {
        if let Self::LocalTalk(bridge) = self {
            bridge.set_address_search_mode(mode);
        }
    }

    /// Send a complete SDLC frame (LocalTalk only)
    pub fn send_frame(&mut self, llap: &[u8]) {
        if let Self::LocalTalk(bridge) = self {
            bridge.send_frame(llap);
        }
    }
}
