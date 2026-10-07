//! One WebSocket client (one browser tab / emulated Mac)

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval, sleep_until, timeout};
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

use snow_core::net::{TAG_ETHERNET, TAG_LOCALTALK, next_frame, push_frame};

use crate::config::{Config, HANDSHAKE_TIMEOUT, MAX_MESSAGE, PING_INTERVAL};
use crate::frames::{valid_ethernet, valid_localtalk};
use crate::http::{client_ip, origin_allowed, room_from_path};
use crate::limits::{ConnectionLimiter, ConnectionSlot, RateLimit, Refusal};
use crate::rooms::{JoinError, Outgoing, Rooms};

/// Frames queued towards one client before newer ones are dropped
const CLIENT_QUEUE: usize = 512;
/// Malformed frames tolerated before a client is disconnected
const MAX_VIOLATIONS: u32 = 100;

/// State shared by all sessions
pub struct Shared {
    pub cfg: Config,
    pub rooms: Arc<Rooms>,
    pub limiter: ConnectionLimiter,
    next_id: AtomicU64,
}

impl Shared {
    pub fn new(cfg: Config, rooms: Arc<Rooms>) -> Self {
        let limiter = ConnectionLimiter::new(cfg.max_clients, cfg.max_clients_per_ip);
        Self {
            cfg,
            rooms,
            limiter,
            next_id: AtomicU64::new(1),
        }
    }
}

/// What the handshake checks granted
struct Admission {
    room: String,
    ip: IpAddr,
    _slot: ConnectionSlot,
}

fn refuse(status: StatusCode, why: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(why.to_string()));
    *r.status_mut() = status;
    r
}

/// Check a WebSocket upgrade request: path/room, Origin, connection limits
// ErrorResponse is tungstenite's handshake rejection type
#[allow(clippy::result_large_err)]
fn admit(req: &Request, peer: SocketAddr, shared: &Shared) -> Result<Admission, ErrorResponse> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let room = room_from_path(path)
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, "use /bridge or /bridge/<room>"))?;
    let ip = client_ip(peer, header("x-forwarded-for"), &shared.cfg);
    if !origin_allowed(header("origin"), header("host"), &shared.cfg) {
        warn!("refused {ip}: origin {:?} not allowed", header("origin"));
        return Err(refuse(StatusCode::FORBIDDEN, "origin not allowed"));
    }
    shared.rooms.has_room_for(&room).map_err(|e| match e {
        JoinError::RoomFull => refuse(StatusCode::SERVICE_UNAVAILABLE, "room is full"),
        JoinError::TooManyRooms => refuse(StatusCode::SERVICE_UNAVAILABLE, "too many rooms"),
    })?;
    let slot = shared.limiter.acquire(ip).map_err(|e| match e {
        Refusal::TooManyClients => refuse(StatusCode::SERVICE_UNAVAILABLE, "bridge is full"),
        Refusal::TooManyFromAddress => refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "too many connections from your address",
        ),
    })?;
    Ok(Admission {
        room,
        ip,
        _slot: slot,
    })
}

/// Handshake and run one client
pub async fn handle(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let admission: Arc<Mutex<Option<Admission>>> = Arc::default();
    let slot = Arc::clone(&admission);
    let check_shared = Arc::clone(&shared);
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let granted = admit(req, peer, &check_shared)?;
        *slot.lock().unwrap() = Some(granted);
        Ok(resp)
    };
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let ws = match timeout(
        HANDSHAKE_TIMEOUT,
        accept_hdr_async_with_config(stream, callback, Some(ws_config)),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            debug!("WebSocket handshake from {peer} failed: {e}");
            return;
        }
        Err(_) => {
            debug!("WebSocket handshake from {peer} timed out");
            return;
        }
    };
    let Some(admission) = admission.lock().unwrap().take() else {
        return;
    };
    run(ws, admission, &shared).await;
}

async fn run(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    admission: Admission,
    shared: &Shared,
) {
    let cfg = &shared.cfg;
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let room = admission.room.clone();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(CLIENT_QUEUE);
    let (mut ws_tx, mut ws_rx) = ws.split();

    let sender_id = match shared.rooms.join(&room, id, out_tx.clone()) {
        Ok(s) => s,
        Err(e) => {
            let frame = CloseFrame {
                code: CloseCode::Again,
                reason: format!("{e:?}").into(),
            };
            let _ = ws_tx.send(Message::Close(Some(frame))).await;
            return;
        }
    };
    info!(
        "client {id} ({}) joined room {room:?} as LToUDP sender {sender_id:08X}",
        admission.ip
    );

    // Writer: batches queued frames into WebSocket messages and pings
    let (close_tx, close_rx) = oneshot::channel::<Option<CloseFrame>>();
    let writer = tokio::spawn(async move {
        let mut close_rx = close_rx;
        let mut ping = interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                frame = out_rx.recv() => {
                    let Some((tag, payload)) = frame else { break };
                    let mut batch = Vec::new();
                    push_frame(&mut batch, tag, &payload);
                    while batch.len() < MAX_MESSAGE / 2 {
                        let Ok((tag, payload)) = out_rx.try_recv() else { break };
                        push_frame(&mut batch, tag, &payload);
                    }
                    if ws_tx.send(Message::Binary(batch.into())).await.is_err() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                frame = &mut close_rx => {
                    let _ = ws_tx.send(Message::Close(frame.ok().flatten())).await;
                    break;
                }
            }
        }
    });

    let deadline = cfg.max_session.map(|d| Instant::now() + d);
    let mut rate = RateLimit::new(cfg.rate_frames, cfg.rate_bytes);
    let mut carry = Vec::new();
    let (mut violations, mut dropped) = (0u32, 0u64);
    #[cfg(feature = "nat")]
    let mut nat: Option<crate::nat::NatClient> = None;
    let mut ethernet_noted = false;

    let close = loop {
        let next = timeout(cfg.idle_timeout, ws_rx.next());
        let msg = match deadline {
            Some(at) => tokio::select! {
                r = next => r,
                () = sleep_until(at) => break Some((CloseCode::Policy, "session time limit reached")),
            },
            None => next.await,
        };
        let msg = match msg {
            Err(_) => break Some((CloseCode::Away, "idle timeout")),
            Ok(None) => break None,
            Ok(Some(Err(e))) => {
                debug!("client {id}: {e}");
                break Some((CloseCode::Policy, "protocol error or message too large"));
            }
            Ok(Some(Ok(msg))) => msg,
        };
        let bytes = match msg {
            Message::Binary(bytes) => bytes,
            Message::Close(_) => break None,
            Message::Text(_) => {
                violations += 1;
                continue;
            }
            // Ping/pong: the activity alone keeps the session alive
            _ => continue,
        };

        carry.extend_from_slice(&bytes);
        if carry.len() > 2 * MAX_MESSAGE {
            break Some((CloseCode::Policy, "incomplete frame too large"));
        }
        while let Some((tag, payload)) = next_frame(&mut carry) {
            if !rate.allow(payload.len()) {
                dropped += 1;
                continue;
            }
            match tag {
                TAG_LOCALTALK if valid_localtalk(&payload) => {
                    shared.rooms.relay_from_client(&room, id, &payload);
                }
                TAG_ETHERNET if !cfg.ethernet => {
                    if !ethernet_noted {
                        info!(
                            "client {id}: Ethernet frames ignored (bridge started without --ethernet)"
                        );
                        ethernet_noted = true;
                    }
                }
                #[cfg(feature = "nat")]
                TAG_ETHERNET if valid_ethernet(&payload) => {
                    nat.get_or_insert_with(|| {
                        crate::nat::NatClient::start(id, cfg, out_tx.clone())
                    })
                    .send(payload);
                }
                _ => violations += 1,
            }
        }
        if violations > MAX_VIOLATIONS {
            break Some((CloseCode::Policy, "too many malformed frames"));
        }
    };
    #[cfg(not(feature = "nat"))]
    let _ = valid_ethernet;

    // Teardown: leave the room, stop NAT, close the socket
    shared.rooms.leave(&room, id);
    #[cfg(feature = "nat")]
    drop(nat);
    drop(out_tx);
    let frame = close.map(|(code, reason)| CloseFrame {
        code,
        reason: reason.into(),
    });
    if let Some(f) = &frame {
        info!("client {id}: closing ({})", f.reason);
    }
    let _ = close_tx.send(frame);
    let _ = timeout(std::time::Duration::from_secs(1), writer).await;
    info!(
        "client {id} left room {room:?} ({} client(s) in {} room(s) remain, {dropped} frame(s) over the rate limit)",
        shared.limiter.total() - 1,
        shared.rooms.room_count()
    );
}
