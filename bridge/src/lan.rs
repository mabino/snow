//! Link between the default room and the LocalTalk-over-UDP multicast group
//! on the local network (only with `--lan`)

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{debug, warn};
use snow_core::mac::localtalk_bridge::{LTOUDP_MULTICAST, LTOUDP_PORT};

use crate::frames::valid_localtalk;
use crate::rooms::{LanSink, Rooms};

struct Multicast {
    socket: UdpSocket,
    group: SocketAddr,
}

impl LanSink for Multicast {
    fn send(&self, dgram: &[u8]) {
        if let Err(e) = self.socket.send_to(dgram, self.group) {
            debug!("LToUDP send failed: {e}");
        }
    }
}

/// Join the LToUDP multicast group and link it to the default room
pub fn start(rooms: &Arc<Rooms>) -> Result<()> {
    let group_ip = Ipv4Addr::from(LTOUDP_MULTICAST);
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LTOUDP_PORT))
        .with_context(|| format!("--lan: cannot bind UDP port {LTOUDP_PORT}"))?;
    socket
        .join_multicast_v4(&group_ip, &Ipv4Addr::UNSPECIFIED)
        .context("--lan: cannot join the LToUDP multicast group")?;
    socket.set_nonblocking(true)?;

    let receiver = tokio::net::UdpSocket::from_std(socket.try_clone()?)?;
    rooms.set_lan(Arc::new(Multicast {
        socket,
        group: SocketAddr::V4(SocketAddrV4::new(group_ip, LTOUDP_PORT)),
    }));

    let rooms = Arc::clone(rooms);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            match receiver.recv_from(&mut buf).await {
                Ok((len, _)) if valid_localtalk(&buf[..len]) => rooms.relay_from_lan(&buf[..len]),
                Ok(_) => debug!("dropped malformed LToUDP datagram from the LAN"),
                Err(e) => {
                    warn!("LToUDP socket error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}
