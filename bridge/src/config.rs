//! Command line configuration
//!
//! Every default is the safe choice: listen on loopback only, no LAN relay,
//! no Ethernet/NAT, same-origin WebSockets only, and bounded resources.
//! Exposing more is an explicit opt-in.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};

pub const USAGE: &str = "\
snow-bridge: network bridge for the Snow WebAssembly port

usage: snow-bridge [options]

Listening:
  --addr <ip>               address to listen on (default 127.0.0.1)
  --port <port>             port to listen on (default 8080)
  --www <dir>               also serve the web frontend from <dir>
  --trust-proxy             take client addresses from X-Forwarded-For
                            (only behind a reverse proxy that sets it)

Origins (browser pages allowed to connect):
  --allow-origin <origin>   allow this origin, e.g. https://mac.example.com
                            (repeatable; default: same origin as the bridge)
  --allow-any-origin        allow every origin (development only)
  --require-origin          refuse clients that send no Origin header

Networking (all off by default):
  --lan                     relay the default room to the LocalTalk-over-UDP
                            multicast group on the local network
  --ethernet                give each client a NAT engine for Ethernet
                            (internet access for the emulated Mac)
  --egress-allow-private    let NAT reach loopback/private/link-local
                            addresses (default: public internet only)
  --egress-ports <list>     only allow these NAT destination ports, e.g. 80,443
  --https-stripping         let vintage browsers reach https:// via http://

Limits:
  --max-clients <n>         simultaneous clients (default 64)
  --max-clients-per-ip <n>  simultaneous clients per address (default 8)
  --max-rooms <n>           simultaneous rooms (default 32)
  --max-room-clients <n>    clients per room (default 16)
  --max-nat-flows <n>       NAT connections per client (default 64)
  --rate-frames <n>         frames per second per client (default 1000)
  --rate-bytes <n>          bytes per second per client (default 1048576)
  --idle-timeout <secs>     drop unresponsive clients (default 120)
  --max-session <secs>      maximum session length (default: unlimited)

Clients connect to ws://<host>/bridge/<room> (room: 1-64 of A-Z a-z 0-9 . _ -);
ws://<host>/bridge is the room \"default\".
";

/// Bridge configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub addr: IpAddr,
    pub port: u16,
    pub www: Option<PathBuf>,
    pub trust_proxy: bool,

    pub allowed_origins: Vec<String>,
    pub allow_any_origin: bool,
    pub require_origin: bool,

    pub lan: bool,
    pub ethernet: bool,
    pub egress_allow_private: bool,
    pub egress_ports: Option<Vec<u16>>,
    pub https_stripping: bool,

    pub max_clients: usize,
    pub max_clients_per_ip: usize,
    pub max_rooms: usize,
    pub max_room_clients: usize,
    pub max_nat_flows: usize,
    pub rate_frames: u32,
    pub rate_bytes: u32,
    pub idle_timeout: Duration,
    pub max_session: Option<Duration>,
}

/// Maximum size of a WebSocket message (one or more frames)
pub const MAX_MESSAGE: usize = 64 * 1024;
/// How often clients are pinged (keeps idle connections alive and detects
/// dead ones)
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Time allowed for the HTTP request / WebSocket handshake
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            www: None,
            trust_proxy: false,
            allowed_origins: Vec::new(),
            allow_any_origin: false,
            require_origin: false,
            lan: false,
            ethernet: false,
            egress_allow_private: false,
            egress_ports: None,
            https_stripping: false,
            max_clients: 64,
            max_clients_per_ip: 8,
            max_rooms: 32,
            max_room_clients: 16,
            max_nat_flows: 64,
            rate_frames: 1000,
            rate_bytes: 1024 * 1024,
            idle_timeout: Duration::from_secs(120),
            max_session: None,
        }
    }
}

fn positive<T: PartialOrd + Default>(name: &str, v: T) -> Result<T> {
    if v <= T::default() {
        bail!("{name} must be greater than 0");
    }
    Ok(v)
}

impl Config {
    /// Parse the command line; `Ok(None)` means help was printed
    pub fn from_args(mut args: pico_args::Arguments) -> Result<Option<Self>> {
        if args.contains(["-h", "--help"]) {
            print!("{USAGE}");
            return Ok(None);
        }
        let d = Self::default();
        let opt = |e: pico_args::Error| anyhow::anyhow!("{e}");
        let ports = |s: &str| -> Result<Vec<u16>> {
            s.split(',')
                .map(|p| {
                    p.trim()
                        .parse::<u16>()
                        .map_err(|e| anyhow::anyhow!("{p}: {e}"))
                })
                .collect()
        };
        let secs = |s: &str| s.parse::<u64>().map(Duration::from_secs);

        let cfg = Self {
            addr: args
                .opt_value_from_str("--addr")
                .map_err(opt)?
                .unwrap_or(d.addr),
            port: args
                .opt_value_from_str("--port")
                .map_err(opt)?
                .unwrap_or(d.port),
            www: args.opt_value_from_str("--www").map_err(opt)?,
            trust_proxy: args.contains("--trust-proxy"),
            allowed_origins: args
                .values_from_str::<_, String>("--allow-origin")
                .map_err(opt)?
                .into_iter()
                .map(|o| o.trim_end_matches('/').to_ascii_lowercase())
                .collect(),
            allow_any_origin: args.contains("--allow-any-origin"),
            require_origin: args.contains("--require-origin"),
            lan: args.contains("--lan"),
            ethernet: args.contains("--ethernet"),
            egress_allow_private: args.contains("--egress-allow-private"),
            egress_ports: match args
                .opt_value_from_str::<_, String>("--egress-ports")
                .map_err(opt)?
            {
                Some(s) => Some(ports(&s)?),
                None => None,
            },
            https_stripping: args.contains("--https-stripping"),
            max_clients: positive(
                "--max-clients",
                args.opt_value_from_str("--max-clients")
                    .map_err(opt)?
                    .unwrap_or(d.max_clients),
            )?,
            max_clients_per_ip: positive(
                "--max-clients-per-ip",
                args.opt_value_from_str("--max-clients-per-ip")
                    .map_err(opt)?
                    .unwrap_or(d.max_clients_per_ip),
            )?,
            max_rooms: positive(
                "--max-rooms",
                args.opt_value_from_str("--max-rooms")
                    .map_err(opt)?
                    .unwrap_or(d.max_rooms),
            )?,
            max_room_clients: positive(
                "--max-room-clients",
                args.opt_value_from_str("--max-room-clients")
                    .map_err(opt)?
                    .unwrap_or(d.max_room_clients),
            )?,
            max_nat_flows: positive(
                "--max-nat-flows",
                args.opt_value_from_str("--max-nat-flows")
                    .map_err(opt)?
                    .unwrap_or(d.max_nat_flows),
            )?,
            rate_frames: positive(
                "--rate-frames",
                args.opt_value_from_str("--rate-frames")
                    .map_err(opt)?
                    .unwrap_or(d.rate_frames),
            )?,
            rate_bytes: positive(
                "--rate-bytes",
                args.opt_value_from_str("--rate-bytes")
                    .map_err(opt)?
                    .unwrap_or(d.rate_bytes),
            )?,
            idle_timeout: positive(
                "--idle-timeout",
                args.opt_value_from_fn("--idle-timeout", secs)
                    .map_err(opt)?
                    .unwrap_or(d.idle_timeout),
            )?,
            max_session: args.opt_value_from_fn("--max-session", secs).map_err(opt)?,
        };

        let rest = args.finish();
        if !rest.is_empty() {
            bail!("unknown argument(s): {rest:?} (see --help)");
        }
        cfg.validate()?;
        Ok(Some(cfg))
    }

    fn validate(&self) -> Result<()> {
        if self.https_stripping && !self.ethernet {
            bail!("--https-stripping requires --ethernet");
        }
        if (self.egress_allow_private || self.egress_ports.is_some()) && !self.ethernet {
            bail!("--egress-* options require --ethernet");
        }
        if self.ethernet && !cfg!(feature = "nat") {
            bail!("--ethernet: this snow-bridge was built without NAT support (feature \"nat\")");
        }
        if let Some(dir) = &self.www
            && !dir.join("index.html").is_file()
        {
            bail!("--www: no index.html in {}", dir.display());
        }
        Ok(())
    }

    /// Whether the bridge is reachable from other machines
    pub fn is_exposed(&self) -> bool {
        !self.addr.is_loopback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config> {
        let args = pico_args::Arguments::from_vec(args.iter().map(Into::into).collect());
        Config::from_args(args).map(|c| c.unwrap())
    }

    #[test]
    fn defaults_are_safe() {
        let c = parse(&[]).unwrap();
        assert!(c.addr.is_loopback(), "loopback only by default");
        assert!(!c.lan, "no LAN relay by default");
        assert!(!c.ethernet, "no NAT by default");
        assert!(!c.allow_any_origin);
        assert!(c.allowed_origins.is_empty(), "same-origin by default");
        assert!(!c.trust_proxy);
        assert!(!c.https_stripping);
        assert!(!c.is_exposed());
    }

    #[test]
    fn parses_options() {
        let c = parse(&[
            "--addr",
            "0.0.0.0",
            "--port",
            "9000",
            "--allow-origin",
            "https://Mac.Example.com/",
            "--max-room-clients",
            "4",
            "--idle-timeout",
            "30",
            "--max-session",
            "3600",
        ])
        .unwrap();
        assert!(c.is_exposed());
        assert_eq!(c.port, 9000);
        assert_eq!(c.allowed_origins, vec!["https://mac.example.com"]);
        assert_eq!(c.max_room_clients, 4);
        assert_eq!(c.idle_timeout, Duration::from_secs(30));
        assert_eq!(c.max_session, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn rejects_unknown_and_inconsistent_options() {
        assert!(parse(&["--no-such-flag"]).is_err());
        assert!(parse(&["--https-stripping"]).is_err(), "needs --ethernet");
        assert!(
            parse(&["--egress-ports", "80"]).is_err(),
            "needs --ethernet"
        );
        assert!(parse(&["--max-clients", "0"]).is_err());
        assert!(parse(&["--www", "/nonexistent-snow-www"]).is_err());
    }

    #[cfg(feature = "nat")]
    #[test]
    fn ethernet_options() {
        let c = parse(&[
            "--ethernet",
            "--egress-ports",
            "80, 443",
            "--https-stripping",
        ])
        .unwrap();
        assert!(c.ethernet && c.https_stripping);
        assert_eq!(c.egress_ports, Some(vec![80, 443]));
        assert!(!c.egress_allow_private);
    }

    #[cfg(not(feature = "nat"))]
    #[test]
    fn ethernet_needs_nat_feature() {
        assert!(parse(&["--ethernet"]).is_err());
    }
}
