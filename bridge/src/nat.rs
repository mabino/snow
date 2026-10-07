//! Per-client NAT engine for Ethernet (only with `--ethernet`)
//!
//! Each client that sends Ethernet frames gets its own NAT domain (gateway
//! 10.0.0.1/8, guest 10.0.0.2 via RARP), like a native Snow instance. The
//! engine runs on its own thread (smoltcp state is not async), starts on the
//! client's first Ethernet frame and stops when the client goes away.
//! Outbound connections are limited by the egress policy and a flow limit;
//! traffic towards the client by the client's byte rate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use log::{info, warn};
use snow_nat::egress::EgressPolicy;
use snow_nat::{NatEngine, Packet};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::limits::TokenBucket;
use crate::rooms::Outgoing;

/// NAT gateway parameters (must match the NAT link in `snow_core`)
const NAT_GATEWAY_MAC: [u8; 6] = [0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55];
const NAT_GATEWAY_IP: [u8; 4] = [10, 0, 0, 1];
const NAT_GATEWAY_SUBNET: u8 = 8;
const QUEUE_SIZE: usize = 512;

pub struct NatClient {
    guest_tx: Sender<Packet>,
    stop: Arc<AtomicBool>,
}

impl NatClient {
    pub fn start(id: u64, cfg: &Config, out: mpsc::Sender<Outgoing>) -> Self {
        let (engine_tx, engine_rx) = bounded::<Packet>(QUEUE_SIZE);
        let (guest_tx, guest_rx) = bounded::<Packet>(QUEUE_SIZE);
        let mut engine = NatEngine::new(
            engine_tx,
            guest_rx,
            NAT_GATEWAY_MAC,
            NAT_GATEWAY_IP,
            NAT_GATEWAY_SUBNET,
            cfg.https_stripping,
        );
        engine.set_egress_policy(EgressPolicy {
            allow_non_public: cfg.egress_allow_private,
            allowed_ports: cfg.egress_ports.clone(),
        });
        engine.set_max_flows(Some(cfg.max_nat_flows));

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let mut download = TokenBucket::new(cfg.rate_bytes);
        std::thread::Builder::new()
            .name(format!("nat-client-{id}"))
            .spawn(move || {
                let mut last_error_log = Instant::now() - Duration::from_secs(1);
                while !thread_stop.load(Ordering::Relaxed) && !out.is_closed() {
                    // Blocks for at most ~100 ms waiting for guest frames
                    if let Err(e) = engine.process() {
                        // Socket errors can be transient; log at most once per second
                        if last_error_log.elapsed() >= Duration::from_secs(1) {
                            warn!("client {id}: NAT engine error: {e}");
                            last_error_log = Instant::now();
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    while let Ok(frame) = engine_rx.try_recv() {
                        let len = u32::try_from(frame.len()).unwrap_or(u32::MAX);
                        // Over the byte budget or the client is not keeping
                        // up: drop (TCP backs off)
                        if download.take(len) {
                            let _ = out.try_send((snow_core::net::TAG_ETHERNET, frame));
                        }
                    }
                }
                let stats = engine.stats();
                info!(
                    "client {id}: NAT engine stopped (rx {} / tx {} / refused {})",
                    stats.rx_packets.load(Ordering::Relaxed),
                    stats.tx_packets.load(Ordering::Relaxed),
                    stats.nat_denied.load(Ordering::Relaxed),
                );
            })
            .expect("failed to spawn NAT engine thread");
        Self { guest_tx, stop }
    }

    /// Hand a guest frame to the engine (dropped if its queue is full)
    pub fn send(&self, frame: Vec<u8>) {
        let _ = self.guest_tx.try_send(frame);
    }
}

impl Drop for NatClient {
    fn drop(&mut self) {
        // The thread notices within ~100 ms and exits on its own
        self.stop.store(true, Ordering::Relaxed);
    }
}
