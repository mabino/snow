//! Host-side network bridge for the Snow WebAssembly port
//!
//! Browsers cannot create OS sockets, so the WebAssembly build of Snow
//! multiplexes its emulated network interfaces over a single WebSocket
//! connection to this bridge process. The bridge performs the real
//! networking on the host machine:
//!
//! - **Ethernet** (DaynaPORT adapter, channel tag 0): frames are fed into a
//!   per-client [`snow_nat::NatEngine`] and are run through the host's
//!   regular network stack, giving the guest internet access via MacTCP
//!   (with RARP/ICMP helpers enabled). Each client gets its own NAT domain
//!   (gateway 10.0.0.1/8, guest 10.0.0.2), just like a separately started
//!   native emulator.
//!
//! - **LocalTalk / AppleTalk** (channel tag 1): LToUDP datagrams are
//!   relayed between all connected clients, and to/from the LToUDP UDP
//!   multicast group (239.192.76.84:1954), so browser instances can do
//!   AppleTalk file sharing with each other and with real Macs on the same
//!   LAN (for example using Tashtalk).
//!
//! Wire protocol: one WebSocket message may contain one or more frames of
//! the form `[1 byte tag][2 byte big-endian length][payload]`, using the
//! same tags and framing as [`snow_core::net`] (the in-emulator transport).
//!
//! The bridge can also serve the web frontend itself (`--www <dir>`) on the
//! same port, with the cross-origin isolation headers the emulator needs
//! for `SharedArrayBuffer`. Requests carrying a WebSocket upgrade go to the
//! bridge, everything else to the static file server.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p snow_bridge -- --port 8080 --www frontend_web/www
//! # Then open http://localhost:8080/ in one or more browser tabs
//! ```

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use snow_core::mac::localtalk_bridge::{LTOUDP_MULTICAST, LTOUDP_PORT};
use snow_core::net::{TAG_ETHERNET, TAG_LOCALTALK, next_frame, push_frame};
use snow_nat::{NatEngine, Packet};

/// NAT gateway parameters (must match the NAT link in `snow_core`)
const NAT_GATEWAY_MAC: [u8; 6] = [0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55];
const NAT_GATEWAY_IP: [u8; 4] = [10, 0, 0, 1];
const NAT_GATEWAY_SUBNET: u8 = 8;

/// Size of the per-client packet queues
const QUEUE_SIZE: usize = 512;

/// Maximum size of a single WebSocket message emitted by the bridge
const MAX_WS_MESSAGE: usize = 64 * 1024;

const DEFAULT_ADDR: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8080;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = pico_args::Arguments::from_env();
    let addr: String = args
        .opt_value_from_str("--addr")
        .map_err(|e| anyhow::anyhow!("invalid --addr: {e}"))?
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let port: u16 = args
        .opt_value_from_str("--port")
        .map_err(|e| anyhow::anyhow!("invalid --port: {e}"))?
        .unwrap_or(DEFAULT_PORT);
    let www: Option<PathBuf> = args
        .opt_value_from_str("--www")
        .map_err(|e| anyhow::anyhow!("invalid --www: {e}"))?;
    let no_lan = args.contains("--no-lan");
    let https_stripping = args.contains("--https-stripping");
    if args.contains(["-h", "--help"]) {
        println!("snow-bridge: network bridge for the Snow WebAssembly port");
        println!("usage: snow-bridge [--addr <ip>] [--port <port>] [--www <dir>] [--no-lan]");
        println!("  --addr <ip>    interface to listen on (default {DEFAULT_ADDR})");
        println!("  --port <port>  port to listen on (default {DEFAULT_PORT})");
        println!("  --www <dir>    also serve the web frontend from <dir>");
        println!("  --no-lan       do not relay LocalTalk to the LToUDP multicast group");
        println!("  --https-stripping  let vintage browsers fetch https:// sites over http://");
        println!("  RUST_LOG       log level (e.g. RUST_LOG=snow_bridge=debug)");
        return Ok(());
    }
    if let Some(dir) = &www {
        anyhow::ensure!(dir.join("index.html").is_file(), "--www: no index.html in {}", dir.display());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let relay = Arc::new(LocalTalkRelay::new(&stop, !no_lan));
    relay.start_udp_pump();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let options = Options { www: www.map(Arc::new), https_stripping };
    rt.block_on(run(addr, port, &stop, &relay, options))?;
    Ok(())
}

/// The accept loop; exits on Ctrl-C
async fn run(
    addr: String,
    port: u16,
    stop: &Arc<AtomicBool>,
    relay: &Arc<LocalTalkRelay>,
    options: Options,
) -> Result<()> {
    let Options { www, https_stripping } = options;
    let bind_addr = SocketAddr::V4(SocketAddrV4::new(
        addr.parse().context("invalid --addr")?,
        port,
    ));
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    let lto = Ipv4Addr::from(LTOUDP_MULTICAST);
    info!("snow-bridge listening on ws://{bind_addr}");
    info!("  Ethernet frames -> per-client NAT engine (guest gets 10.0.0.2 via RARP)");
    let lto_note = if relay.udp().is_some() {
        format!(" and LToUDP multicast {lto}:{LTOUDP_PORT}")
    } else {
        String::from(" (no LAN relay)")
    };
    info!("  LocalTalk datagrams -> other clients{lto_note}");
    if let Some(dir) = &www {
        info!("  serving the web frontend from {} at http://{bind_addr}/", dir.display());
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received Ctrl-C, shutting down");
                break;
            }
            accepted = listener.accept() => {
                let (stream, _peer) = accepted.context("failed to accept connection")?;
                let relay = Arc::clone(relay);
                let www = www.clone();
                tokio::spawn(async move {
                    match (is_websocket_upgrade(&stream).await, www) {
                        (true, _) | (false, None) => {
                            handle_client(stream, &relay, https_stripping).await;
                        }
                        (false, Some(dir)) => {
                            if let Err(e) = serve_static(stream, &dir).await {
                                debug!("static file request failed: {e}");
                            }
                        }
                    }
                });
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

/// Command line options passed to the accept loop
struct Options {
    /// Directory of the web frontend to serve, if any
    www: Option<Arc<PathBuf>>,
    /// Enable the NAT engine's HTTPS stripping for every client
    https_stripping: bool,
}

/// Fans LocalTalk datagrams out between clients and with the LToUDP
/// multicast group
struct LocalTalkRelay {
    /// Per-client datagram queues (tag 1 frames)
    clients: RwLock<HashMap<u64, Sender<Vec<u8>>>>,
    /// LToUDP sender ID -> client id, used to suppress multicast loopback
    /// echo of a client's own datagrams (mirrors the sender ID check that
    /// every LToUDP implementation performs)
    owners: RwLock<HashMap<u32, u64>>,
    /// Multicast socket for the LAN relay (None if unavailable)
    udp: Option<Arc<Mutex<UdpSocket>>>,
    /// Flipped when the bridge is shutting down
    stop: Arc<AtomicBool>,
}

impl LocalTalkRelay {
    fn new(stop: &Arc<AtomicBool>, lan: bool) -> Self {
        let udp = if lan { Self::bind_multicast() } else { None };
        Self {
            clients: RwLock::new(HashMap::new()),
            owners: RwLock::new(HashMap::new()),
            udp,
            stop: Arc::clone(stop),
        }
    }

    /// Bind the LToUDP multicast socket; None (with a warning) on failure
    fn bind_multicast() -> Option<Arc<Mutex<UdpSocket>>> {
        let socket = UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            LTOUDP_PORT,
        )))
        .ok()?;
        let group_ip = Ipv4Addr::from(LTOUDP_MULTICAST);
        let group = SocketAddrV4::new(group_ip, LTOUDP_PORT);
        if let Err(e) = socket.join_multicast_v4(group.ip(), &Ipv4Addr::UNSPECIFIED) {
            warn!(
                "failed to join LToUDP multicast group {group}: {e} \
                 (LAN relay disabled, client-to-client relay still works)"
            );
        }
        if let Err(e) = socket.set_nonblocking(true) {
            warn!("failed to make LToUDP socket non-blocking: {e}");
        }
        Some(Arc::new(Mutex::new(socket)))
    }

    fn udp(&self) -> &Option<Arc<Mutex<UdpSocket>>> {
        &self.udp
    }

    /// Start the pump thread that receives from the multicast group and
    /// fans datagrams out to all connected clients
    fn start_udp_pump(self: &Arc<Self>) {
        let Some(udp) = self.udp.clone() else {
            return;
        };
        let relay = Arc::clone(self);
        std::thread::Builder::new()
            .name("lto-udp-pump".to_string())
            .spawn(move || {
                let mut buf = vec![0u8; 2048];
                loop {
                    if relay.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // The socket is non-blocking; the lock is held only around
                    // recv_from (never across the idle sleep, so that sends
                    // from async tasks are not blocked)
                    let received = {
                        let guard = udp.lock().unwrap();
                        match guard.recv_from(&mut buf) {
                            Ok((len, _)) => Some(buf[..len].to_vec()),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
                            Err(e) => {
                                warn!("LToUDP socket error: {e}");
                                return;
                            }
                        }
                    };
                    if let Some(dgram) = received {
                        // Suppress the loopback echo of a client's own
                        // datagram (the guest dedupes by sender ID too)
                        let owner = dgram
                            .first_chunk::<4>()
                            .map(|sid| u32::from_be_bytes(*sid))
                            .and_then(|sid| relay.owners.read().unwrap().get(&sid).copied());
                        relay.fanout_to_clients(&dgram, owner);
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })
            .expect("failed to spawn LToUDP pump thread");
    }

    fn client_count(&self) -> usize {
        self.clients.read().unwrap().len()
    }

    /// Deliver a datagram to all clients except `exclude`
    fn fanout_to_clients(&self, dgram: &[u8], exclude: Option<u64>) {
        let mut dropped = 0;
        for (id, tx) in self.clients.read().unwrap().iter() {
            if Some(*id) == exclude {
                continue;
            }
            if tx.try_send(dgram.to_vec()).is_err() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            warn!("dropped {dropped} LocalTalk datagram(s), client queue(s) full");
        }
    }

    /// A datagram from a client: relay to the other clients and the LAN
    fn handle_client_dgram(&self, id: u64, dgram: &[u8]) {
        if dgram.len() >= 4 {
            let sid = u32::from_be_bytes([dgram[0], dgram[1], dgram[2], dgram[3]]);
            self.owners.write().unwrap().insert(sid, id);
        }
        self.fanout_to_clients(dgram, Some(id));
        if let Some(udp) = self.udp.as_ref() {
            let addr =
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(LTOUDP_MULTICAST), LTOUDP_PORT));
            let guard = udp.lock().unwrap();
            if let Err(e) = guard.send_to(dgram, addr) {
                debug!("failed to send LocalTalk datagram to LToUDP group: {e}");
            }
        }
    }

    /// Register a client's datagram queue
    fn add_client(&self, id: u64, tx: Sender<Vec<u8>>) {
        self.clients.write().unwrap().insert(id, tx);
    }

    /// Remove a client's datagram queue and its sender id ownership
    fn remove_client(&self, id: u64) {
        self.clients.write().unwrap().remove(&id);
        let mut owners = self.owners.write().unwrap();
        owners.retain(|_, owner| *owner != id);
    }
}

/// Whether a new connection is a WebSocket upgrade request (peeks at the
/// request headers without consuming them)
async fn is_websocket_upgrade(stream: &TcpStream) -> bool {
    let mut buf = [0u8; 4096];
    for _ in 0..50 {
        let Ok(n) = stream.peek(&mut buf).await else {
            return false;
        };
        let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
        if head.contains("\r\n\r\n") || n == buf.len() || n == 0 {
            return head.contains("upgrade: websocket");
        }
        // Headers still arriving
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Content type by file extension
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" => "application/json",
        "css" => "text/css",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

/// Map a request path onto a file below `root` (no traversal outside it)
fn resolve_static(root: &Path, request_path: &str) -> Option<PathBuf> {
    let path = request_path.split(['?', '#']).next().unwrap_or("/");
    let mut file = root.to_path_buf();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if part == ".." || part == "." || part.contains('\\') {
            return None;
        }
        file.push(part);
    }
    if file.is_dir() {
        file.push("index.html");
    }
    file.is_file().then_some(file)
}

/// Minimal static file server for the web frontend. Sends the COOP/COEP
/// headers that make the page cross-origin isolated (`SharedArrayBuffer`).
async fn serve_static(mut stream: TcpStream, root: &Path) -> Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        anyhow::ensure!(n > 0 && head.len() < 64 * 1024, "bad request");
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut request = head.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (request.next().unwrap_or(""), request.next().unwrap_or("/"));

    let found = (method == "GET" || method == "HEAD")
        .then(|| resolve_static(root, path))
        .flatten();
    let (status, ctype, body) = match found {
        Some(file) => ("200 OK", content_type(&file), tokio::fs::read(&file).await?),
        None => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Cross-Origin-Embedder-Policy: require-corp\r\n\
         Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(&body).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

/// Run the NAT engine for one client until `stop` is set, returning its
/// statistics
fn nat_engine_loop(
    id: u64,
    mut engine: NatEngine,
    stop: &Arc<AtomicBool>,
) -> Arc<snow_nat::NatEngineStats> {
    let mut last_error_log = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // process() blocks at most ~100 ms (channel timeout)
        if let Err(e) = engine.process() {
            // Socket errors can be transient (e.g. a reset connection);
            // keep the engine alive and log at most once per second
            if last_error_log.elapsed() > Duration::from_secs(1) {
                warn!("client {id}: NAT engine error: {e}");
                last_error_log = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    engine.stats()
}

/// Handle a single bridge client (one browser tab / emulated Mac)
async fn handle_client(stream: TcpStream, relay: &Arc<LocalTalkRelay>, https_stripping: bool) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());

    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!("client from {peer}: WebSocket handshake failed: {e}");
            return;
        }
    };

    let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    info!("client {id} connected from {peer}");

    // NAT engine channels
    let (engine_tx, engine_rx) = bounded::<Packet>(QUEUE_SIZE);
    let (guest_tx, guest_rx) = bounded::<Packet>(QUEUE_SIZE);
    // LocalTalk datagram queue fed by the relay
    let (lt_tx, lt_rx) = bounded::<Vec<u8>>(QUEUE_SIZE);
    relay.add_client(id, lt_tx);

    let stop = Arc::new(AtomicBool::new(false));

    // The NAT engine on a dedicated thread (smoltcp state cannot be shared)
    let engine = NatEngine::new(
        engine_tx,
        guest_rx,
        NAT_GATEWAY_MAC,
        NAT_GATEWAY_IP,
        NAT_GATEWAY_SUBNET,
        https_stripping,
    );
    let engine_stop = Arc::clone(&stop);
    let engine_thread = std::thread::Builder::new()
        .name(format!("nat-client-{id}"))
        .spawn(move || {
            let stats = nat_engine_loop(id, engine, &engine_stop);
            info!(
                "client {id}: NAT engine stopped (rx {} / tx {} / active tcp {} / udp {})",
                stats.rx_packets.load(Ordering::Relaxed),
                stats.tx_packets.load(Ordering::Relaxed),
                stats.nat_active_tcp.load(Ordering::Relaxed),
                stats.nat_active_udp.load(Ordering::Relaxed)
            );
        })
        .expect("failed to spawn NAT engine thread");

    // Merge thread: NAT output + relay LocalTalk -> WebSocket messages
    let (ws_out_tx, mut ws_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pump_stop = Arc::clone(&stop);
    let pump_thread = std::thread::Builder::new()
        .name(format!("pump-client-{id}"))
        .spawn(move || {
            let mut batch = Vec::new();
            loop {
                if pump_stop.load(Ordering::Relaxed) {
                    break;
                }
                batch.clear();
                while let Ok(frame) = engine_rx.try_recv() {
                    if batch.len() + 3 + frame.len() > MAX_WS_MESSAGE {
                        break;
                    }
                    push_frame(&mut batch, TAG_ETHERNET, &frame);
                }
                while let Ok(dgram) = lt_rx.try_recv() {
                    if batch.len() + 3 + dgram.len() > MAX_WS_MESSAGE {
                        break;
                    }
                    push_frame(&mut batch, TAG_LOCALTALK, &dgram);
                }
                if batch.is_empty() {
                    // Wait for something to arrive
                    let first = match engine_rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(frame) => Some((TAG_ETHERNET, frame)),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => match lt_rx.try_recv() {
                            Ok(dgram) => Some((TAG_LOCALTALK, dgram)),
                            Err(_) => None,
                        },
                    };
                    if let Some((tag, payload)) = first {
                        push_frame(&mut batch, tag, &payload);
                    }
                }
                if batch.is_empty() {
                    continue;
                }
                if ws_out_tx.send(std::mem::take(&mut batch)).is_err() {
                    debug!("client {id}: pump exiting (mpsc closed)");
                    break; // writer is gone
                }
            }
        })
        .expect("failed to spawn pump thread");

    let (mut ws_tx, mut ws_rx) = ws.split();

    // Notify the reader loop when the writer finishes
    let (write_done_tx, mut write_done_rx) = tokio::sync::oneshot::channel::<()>();
    let mut write_task = tokio::spawn(async move {
        loop {
            let mut msg = match ws_out_rx.recv().await {
                Some(m) => m,
                None => {
                    debug!("client {id}: writer exiting (pump gone)");
                    break;
                }
            };
            // Coalesce everything that is already queued into one WS message
            while let Ok(more) = ws_out_rx.try_recv() {
                msg.extend_from_slice(&more);
            }
            if ws_tx.send(Message::Binary(msg.into())).await.is_err() {
                warn!("client {id}: WebSocket write failed, closing");
                break;
            }
        }
        let _ = write_done_tx.send(());
    });

    // Read loop: WebSocket frames -> NAT engine / LocalTalk relay
    let mut carry = Vec::new();
    loop {
        tokio::select! {
            result = ws_rx.next() => {
                let Some(result) = result else {
                    debug!("client {id}: reader exiting (stream end)");
                    break; // stream closed
                };
                let msg = match result {
                    Ok(msg) => msg,
                    Err(e) => {
                        debug!("client {id}: WebSocket read error: {e}");
                        break;
                    }
                };
                match msg {
                    Message::Binary(bytes) => {
                        carry.extend_from_slice(&bytes);
                        while let Some((tag, payload)) = next_frame(&mut carry) {
                            match tag {
                                TAG_ETHERNET => {
                                    if guest_tx.try_send(payload).is_err() {
                                        warn!(
                                            "client {id}: dropped Ethernet frame \
                                             (NAT queue full)"
                                        );
                                    }
                                }
                                TAG_LOCALTALK => relay.handle_client_dgram(id, &payload),
                                _ => debug!(
                                    "client {id}: ignoring frame with unknown tag {tag}"
                                ),
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {
                        // Ping (auto-answered by tungstenite), Pong, Text
                    }
                }
            }
            _ = &mut write_done_rx => {
                info!("client {id}: writer finished, closing");
                break;
            }
        }
    }

    // Teardown
    stop.store(true, Ordering::Relaxed);
    relay.remove_client(id); // also drops lt_tx
    drop(guest_tx); // unblocks the NAT engine's channel
    // The pump exits within ~20 ms, dropping its mpsc sender; the writer then exits
    tokio::select! {
        _ = &mut write_task => {}
        _ = tokio::time::sleep(Duration::from_millis(500)) => {
            write_task.abort();
        }
    }
    let _ = engine_thread.join();
    let _ = pump_thread.join();
    info!("client {id} disconnected ({} client(s) remain)", relay.client_count());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_paths_stay_inside_root() {
        let root = std::env::temp_dir().join(format!("snow-bridge-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("index.html"), "x").unwrap();
        std::fs::write(root.join("sub/a.js"), "y").unwrap();

        assert_eq!(resolve_static(&root, "/"), Some(root.join("index.html")));
        assert_eq!(resolve_static(&root, "/?autostart=1"), Some(root.join("index.html")));
        assert_eq!(resolve_static(&root, "/sub/a.js"), Some(root.join("sub/a.js")));
        assert_eq!(resolve_static(&root, "/../etc/passwd"), None);
        assert_eq!(resolve_static(&root, "/sub/../index.html"), None);
        assert_eq!(resolve_static(&root, "/missing.js"), None);
        assert_eq!(content_type(Path::new("x.wasm")), "application/wasm");
        std::fs::remove_dir_all(root).unwrap();
    }
}
