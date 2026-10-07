//! Host-side network bridge for the Snow WebAssembly port
//!
//! Browsers cannot create OS sockets, so the WebAssembly build of Snow
//! multiplexes its emulated network interfaces over one WebSocket per
//! browser tab to this bridge, framed as `[tag u8][length u16 BE][payload]`
//! (see [`snow_core::net`]):
//!
//! - **LocalTalk / AppleTalk** (tag 1): LToUDP datagrams are relayed between
//!   the clients of a *room* (`/bridge/<room>`); every room is an isolated
//!   AppleTalk network. With `--lan`, the default room is also linked to
//!   the LToUDP multicast group on the local network.
//! - **Ethernet** (tag 0, only with `--ethernet` and the `nat` feature): each
//!   client gets a NAT engine with an egress policy (public internet only by
//!   default), a flow limit and a bandwidth cap.
//!
//! With `--www <dir>` the bridge also serves the web frontend on the same
//! port, with cross-origin isolation and a strict Content Security Policy.
//!
//! Defaults are safe for running on your own machine: loopback only, no
//! LAN, no NAT, same-origin WebSockets, bounded resources. See
//! `docs/src/manual/network/web.md` for running it as a public service.

mod config;
mod frames;
mod http;
mod lan;
mod limits;
#[cfg(feature = "nat")]
mod nat;
mod rooms;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::TcpListener;

use crate::config::Config;
use crate::rooms::Rooms;
use crate::session::Shared;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let Some(cfg) = Config::from_args(pico_args::Arguments::from_env())? else {
        return Ok(());
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg))
}

fn describe(cfg: &Config, bind: SocketAddr) {
    info!("snow-bridge listening on {bind}; clients connect to /bridge/<room>");
    if let Some(dir) = &cfg.www {
        info!("  serving the web frontend from {}", dir.display());
    }
    info!(
        "  rooms: up to {} with {} clients each; {} clients total, {} per address",
        cfg.max_rooms, cfg.max_room_clients, cfg.max_clients, cfg.max_clients_per_ip
    );
    info!(
        "  origins: {}",
        if cfg.allow_any_origin {
            "any (--allow-any-origin)".to_string()
        } else if cfg.allowed_origins.is_empty() {
            "same origin only".to_string()
        } else {
            cfg.allowed_origins.join(", ")
        }
    );
    info!(
        "  LAN relay: {}",
        if cfg.lan {
            "default room <-> LToUDP multicast"
        } else {
            "off"
        }
    );
    if cfg.ethernet {
        info!(
            "  Ethernet NAT: on ({}{}, {} flows per client{})",
            if cfg.egress_allow_private {
                "any destination"
            } else {
                "public internet only"
            },
            cfg.egress_ports
                .as_ref()
                .map(|p| format!(", ports {p:?}"))
                .unwrap_or_default(),
            cfg.max_nat_flows,
            if cfg.https_stripping {
                ", HTTPS stripping"
            } else {
                ""
            },
        );
    } else {
        info!("  Ethernet NAT: off");
    }
    if cfg.is_exposed() {
        warn!("listening on a non-loopback address: other machines can connect");
        if cfg.allow_any_origin {
            warn!("--allow-any-origin: any web page can use this bridge");
        }
        if cfg.ethernet {
            warn!("--ethernet on an exposed bridge: clients make connections from this host");
        }
        if cfg.egress_allow_private {
            warn!("--egress-allow-private: clients can reach this host and its local network");
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let bind = SocketAddr::new(cfg.addr, cfg.port);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    describe(&cfg, bind);

    let rooms = Arc::new(Rooms::new(cfg.max_rooms, cfg.max_room_clients));
    if cfg.lan {
        lan::start(&rooms)?;
    }
    let www = cfg.www.clone().map(Arc::new);
    let shared = Arc::new(Shared::new(cfg, rooms));

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received Ctrl-C, shutting down");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("accept failed: {e}");
                        continue;
                    }
                };
                let shared = Arc::clone(&shared);
                let www = www.clone();
                tokio::spawn(async move {
                    if http::is_websocket_upgrade(&stream).await {
                        session::handle(stream, peer, shared).await;
                    } else if let Some(dir) = www
                        && let Err(e) = http::serve_static(stream, &dir).await
                    {
                        log::debug!("static file request from {peer} failed: {e}");
                    }
                });
            }
        }
    }
}
