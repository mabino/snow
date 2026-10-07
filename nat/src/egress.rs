//! Egress policy: which destinations the NAT engine may connect to
//!
//! The NAT engine turns guest traffic into real connections made by the host.
//! When the host serves untrusted guests (for example a public web service),
//! those connections must not reach the host itself, its local network or
//! cloud metadata services. [`EgressPolicy::public_internet`] allows public
//! unicast addresses only.

use std::net::{Ipv4Addr, SocketAddrV4};

/// Transport protocol of an outbound flow
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressProtocol {
    Tcp,
    Udp,
}

/// Rules for outbound connections
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Allow non-public destinations (loopback, private, link-local, ...)
    pub allow_non_public: bool,
    /// If set, only these destination ports are allowed
    pub allowed_ports: Option<Vec<u16>>,
}

impl EgressPolicy {
    /// Everything allowed (the NAT engine's behaviour without a policy)
    pub fn allow_all() -> Self {
        Self {
            allow_non_public: true,
            allowed_ports: None,
        }
    }

    /// Public unicast IPv4 destinations only, any port
    pub fn public_internet() -> Self {
        Self::default()
    }

    /// Whether a connection to `dest` is allowed
    pub fn allows(&self, dest: SocketAddrV4, _proto: EgressProtocol) -> bool {
        if !self.allow_non_public && !is_public_ipv4(*dest.ip()) {
            return false;
        }
        match &self.allowed_ports {
            Some(ports) => ports.contains(&dest.port()),
            None => dest.port() != 0,
        }
    }
}

/// Whether an IPv4 address is a public unicast address
pub fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || a == 0                                   // 0.0.0.0/8 "this network"
        || ip.is_loopback()                         // 127.0.0.0/8
        || ip.is_private()                          // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()                       // 169.254/16 (cloud metadata)
        || (a == 100 && (b & 0xC0) == 64)           // 100.64/10 carrier-grade NAT
        || (a == 192 && b == 0 && c == 0)           // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)           // 192.0.2.0/24 TEST-NET-1
        || (a == 198 && (b & 0xFE) == 18)           // 198.18/15 benchmarking
        || (a == 198 && b == 51 && c == 100)        // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)         // TEST-NET-3
        || ip.is_multicast()                        // 224/4
        || a >= 240                                 // 240/4 reserved, broadcast
        || ip.is_broadcast())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(ip: [u8; 4], port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::from(ip), port)
    }

    #[test]
    fn public_internet_blocks_local_and_reserved() {
        let p = EgressPolicy::public_internet();
        for ip in [
            [127, 0, 0, 1],
            [10, 1, 2, 3],
            [172, 16, 0, 1],
            [172, 31, 255, 254],
            [192, 168, 1, 1],
            [169, 254, 169, 254],
            [100, 64, 0, 1],
            [100, 127, 255, 255],
            [0, 0, 0, 0],
            [0, 1, 2, 3],
            [224, 0, 0, 251],
            [239, 192, 76, 84],
            [240, 0, 0, 1],
            [255, 255, 255, 255],
            [192, 0, 2, 10],
            [198, 18, 0, 1],
            [203, 0, 113, 5],
        ] {
            assert!(
                !p.allows(dest(ip, 80), EgressProtocol::Tcp),
                "{ip:?} must be blocked"
            );
        }
    }

    #[test]
    fn public_internet_allows_public_unicast() {
        let p = EgressPolicy::public_internet();
        for ip in [
            [1, 1, 1, 1],
            [8, 8, 8, 8],
            [93, 184, 216, 34],
            [172, 32, 0, 1],
            [100, 128, 0, 1],
        ] {
            assert!(
                p.allows(dest(ip, 443), EgressProtocol::Tcp),
                "{ip:?} must be allowed"
            );
        }
        assert!(
            !p.allows(dest([1, 1, 1, 1], 0), EgressProtocol::Udp),
            "port 0"
        );
    }

    #[test]
    fn port_allowlist() {
        let p = EgressPolicy {
            allowed_ports: Some(vec![80, 443]),
            ..EgressPolicy::public_internet()
        };
        assert!(p.allows(dest([1, 1, 1, 1], 443), EgressProtocol::Tcp));
        assert!(!p.allows(dest([1, 1, 1, 1], 25), EgressProtocol::Tcp));
    }

    #[test]
    fn allow_all_allows_local() {
        assert!(EgressPolicy::allow_all().allows(dest([127, 0, 0, 1], 8080), EgressProtocol::Tcp));
    }
}
