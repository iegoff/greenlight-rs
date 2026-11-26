//! Teredo (RFC 4380) address decoding.
//!
//! Xbox consoles frequently advertise Teredo IPv6 candidates. The real IPv4
//! endpoint is embedded (obfuscated) in the address; Greenlight adds extra
//! host candidates for it so home streaming works without IPv6.

use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeredoInfo {
    pub server: Ipv4Addr,
    pub client: Ipv4Addr,
    pub udp_port: u16,
}

/// Returns `true` if the address is in the Teredo prefix `2001:0000::/32`.
pub fn is_teredo(addr: &Ipv6Addr) -> bool {
    let s = addr.segments();
    s[0] == 0x2001 && s[1] == 0x0000
}

pub fn parse(address: &str) -> Option<TeredoInfo> {
    let addr: Ipv6Addr = address.trim().trim_matches(['[', ']']).parse().ok()?;
    if !is_teredo(&addr) {
        return None;
    }
    let s = addr.segments();
    let server = Ipv4Addr::new((s[2] >> 8) as u8, s[2] as u8, (s[3] >> 8) as u8, s[3] as u8);
    let udp_port = !s[5];
    let c6 = !s[6];
    let c7 = !s[7];
    let client = Ipv4Addr::new((c6 >> 8) as u8, c6 as u8, (c7 >> 8) as u8, c7 as u8);
    Some(TeredoInfo { server, client, udp_port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4380_example() {
        // Example from RFC 4380 / Wikipedia:
        // server 65.54.227.120, client 192.0.2.45, port 40000
        let info = parse("2001:0000:4136:e378:8000:63bf:3fff:fdd2").unwrap();
        assert_eq!(info.server, Ipv4Addr::new(65, 54, 227, 120));
        assert_eq!(info.client, Ipv4Addr::new(192, 0, 2, 45));
        assert_eq!(info.udp_port, 40000);
    }

    #[test]
    fn not_teredo() {
        assert!(parse("2001:db8::1").is_none());
        assert!(parse("fe80::1").is_none());
        assert!(parse("not an ip").is_none());
    }
}
