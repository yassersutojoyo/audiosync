//! Which peers, `Host` headers and `Origin` headers the server accepts.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Whether `peer` may connect at all: private (RFC 1918, RFC 4193),
/// link-local or loopback. Everyone else is closed at accept.
pub fn is_private_peer(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// Whether a connection from `peer` to this machine's `local` address is
/// let in: a private peer, or an IPv6 peer in the same /64 as `local`,
/// which on a home network with global IPv6 is the phone next to it.
pub fn admits_peer(peer: IpAddr, local: IpAddr) -> bool {
    if is_private_peer(peer) {
        return true;
    }
    match (peer.to_canonical(), local.to_canonical()) {
        (IpAddr::V6(peer), IpAddr::V6(local)) => {
            !local.is_unspecified()
                && !local.is_loopback()
                && !local.is_multicast()
                && peer.segments()[..4] == local.segments()[..4]
        }
        _ => false,
    }
}

/// Splits `host[:port]` (RFC 9110 §7.2) into the host, without brackets
/// around an IPv6 literal, and the port.
fn split_host(value: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return match after {
            "" => Some((host, None)),
            _ => Some((host, Some(after.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match value.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port.parse().ok()?))),
        None => Some((value, None)),
    }
}

fn literal(host: &str) -> Option<IpAddr> {
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Some(IpAddr::V4(v4));
    }
    // A zone (RFC 6874, `%25en0` or `%en0`) names the interface, not the address.
    let address = host.split('%').next()?;
    address.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

/// Whether a request's `Host` names this server: the address the peer
/// connected to, or `<local_name>.local`, either with the listener's port
/// (which a browser leaves out only for port 80). Anything else is DNS
/// rebinding or a wrong address.
pub fn host_allowed(host: &str, local: SocketAddr, local_name: Option<&str>) -> bool {
    let Some((name, port)) = split_host(host.trim()) else {
        return false;
    };
    let port_matches = match port {
        Some(port) => port == local.port(),
        None => local.port() == 80,
    };
    if !port_matches || name.is_empty() {
        return false;
    }
    if let Some(address) = literal(name) {
        return address.to_canonical() == local.ip().to_canonical();
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    local_name.is_some_and(|own| {
        let own = own.strip_suffix(".local").unwrap_or(own);
        !own.is_empty() && name.eq_ignore_ascii_case(&format!("{own}.local"))
    })
}

/// Whether a `POST`'s `Origin` is this page's: `http://` and the request's
/// own (already accepted) `Host`.
pub fn origin_matches(origin: &str, host: &str) -> bool {
    origin
        .trim()
        .strip_prefix("http://")
        .is_some_and(|rest| rest.eq_ignore_ascii_case(host.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Private addresses are built from their parts: published files never spell one.
    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_private_link_local_and_loopback_peers_connect() {
        let unique_local = IpAddr::V6(Ipv6Addr::new(0xfdab, 0, 0, 0, 0, 0, 0, 5));
        let mapped_home = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped());
        for allowed in [
            v4(10, 1, 2, 3),
            v4(172, 16, 0, 1),
            v4(172, 31, 255, 255),
            v4(192, 168, 1, 20),
            v4(169, 254, 10, 1),
            v4(127, 0, 0, 1),
            v4(127, 8, 9, 10),
            v6("::1"),
            v6("fc00::1"),
            unique_local,
            v6("fe80::1"),
            v6("febf::1"),
            mapped_home,
        ] {
            assert!(is_private_peer(allowed), "{allowed}");
        }
        for refused in [
            v4(8, 8, 8, 8),
            v4(172, 32, 0, 1),
            v4(100, 64, 0, 1),
            v4(192, 169, 0, 1),
            v4(0, 0, 0, 0),
            v6("2001:db8::1"),
            v6("fec0::1"),
            v6("::ffff:8.8.8.8"),
            v6("::"),
        ] {
            assert!(!is_private_peer(refused), "{refused}");
        }
    }

    #[test]
    fn a_global_ipv6_peer_on_the_same_link_is_let_in() {
        // A phone that resolved the .local name to a global AAAA connects
        // from its own global address in the same /64.
        let local = v6("2001:db8:1:2::10");
        assert!(admits_peer(v6("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), local));
        assert!(admits_peer(v6("2001:db8:1:2::10"), local));
        assert!(!admits_peer(v6("2001:db8:1:3::5"), local), "another prefix");
        assert!(!admits_peer(v6("2001:db9:1:2::5"), local), "another prefix");
        assert!(!admits_peer(v6("2001:db8:1:2::5"), v4(127, 0, 0, 1)));
        assert!(!admits_peer(v4(8, 8, 8, 8), v6("::ffff:8.8.8.1")));
        assert!(!admits_peer(v6("::5"), v6("::")), "no link to share");
        assert!(admits_peer(v4(192, 168, 1, 20), local), "private stays in");
        assert!(admits_peer(v6("fe80::1"), local));
    }

    #[test]
    fn host_must_name_this_server_with_its_port() {
        let home = v4(192, 168, 1, 5);
        let local = SocketAddr::new(home, 7655);
        let name = Some("Kitchen");
        let at = |port: u16| format!("{home}:{port}");
        assert!(host_allowed(&at(7655), local, name));
        assert!(host_allowed("kitchen.local:7655", local, name));
        assert!(host_allowed("KITCHEN.LOCAL.:7655", local, name));
        assert!(host_allowed(
            "kitchen.local:7655",
            local,
            Some("kitchen.local")
        ));
        assert!(!host_allowed(&home.to_string(), local, name));
        assert!(!host_allowed(&at(80), local, name));
        assert!(!host_allowed(
            &format!("{}:7655", v4(192, 168, 1, 6)),
            local,
            name
        ));
        assert!(!host_allowed("evil.example:7655", local, name));
        assert!(!host_allowed("kitchen.local:7655", local, None));
        assert!(!host_allowed("den.local:7655", local, name));
        assert!(!host_allowed(":7655", local, name));
        assert!(!host_allowed(&format!("{}x", at(7655)), local, name));

        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::new(192, 168, 1, 5).to_ipv6_mapped()),
            7655,
        );
        assert!(host_allowed(&at(7655), mapped, None));
        let link: SocketAddr = "[fe80::1%4]:7655".parse().unwrap();
        assert!(host_allowed("[fe80::1]:7655", link, None));
        assert!(host_allowed("[fe80::1%25en0]:7655", link, None));
        assert!(!host_allowed("[fe80::2]:7655", link, None));
        assert!(!host_allowed("[fe80::1]", link, None));

        let web = SocketAddr::new(v4(10, 0, 0, 2), 80);
        assert!(host_allowed(&v4(10, 0, 0, 2).to_string(), web, None));
    }

    #[test]
    fn origin_must_be_the_pages_own() {
        let host = format!("{}:7655", v4(192, 168, 1, 5));
        assert!(origin_matches(&format!("http://{host}"), &host));
        assert!(origin_matches(
            "http://Kitchen.local:7655",
            "kitchen.local:7655"
        ));
        assert!(!origin_matches(&format!("https://{host}"), &host));
        assert!(!origin_matches(&format!("http://{host}1"), &host));
        assert!(!origin_matches("null", &host));
        assert!(!origin_matches("http://evil.example", &host));
    }
}
