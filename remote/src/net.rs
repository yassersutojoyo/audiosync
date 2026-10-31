//! Which peers, `Host` headers and `Origin` headers the server accepts.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Whether `peer` is private (RFC 1918, RFC 4193), link-local or loopback.
/// [`admits_peer`] adds the peers let in beside these.
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
/// let in: a private peer, a carrier-grade NAT peer (RFC 6598, the range
/// Tailscale gives its devices), or an IPv6 peer in the same /64 as a native
/// global `local`, which on a home network is the phone next to it.
pub fn admits_peer(peer: IpAddr, local: IpAddr) -> bool {
    if is_private_peer(peer) {
        return true;
    }
    match (peer.to_canonical(), local.to_canonical()) {
        (IpAddr::V4(peer), _) => peer.octets()[0] == 100 && peer.octets()[1] & 0xc0 == 64,
        (IpAddr::V6(peer), IpAddr::V6(local)) => {
            native_global(local) && peer.segments()[..4] == local.segments()[..4]
        }
        _ => false,
    }
}

/// The peer that per-peer limits count against: the full address. Not an
/// IPv6 /64: every IPv6 device let in shares one (see [`admits_peer`]), so
/// one device would lock out the rest; the global limits cover a device
/// that rotates temporary addresses (RFC 8981).
pub fn peer_group(peer: IpAddr) -> IpAddr {
    peer.to_canonical()
}

/// Global unicast (2000::/3) whose /64 is a real link. Teredo (2001::/32)
/// and 6to4 (2002::/16) prefixes are built from a public IPv4 address that
/// strangers on the internet share.
fn native_global(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[0] & 0xe000 == 0x2000 && segments[..2] != [0x2001, 0] && segments[0] != 0x2002
}

/// Splits `host[:port]` (RFC 9110 §7.2) into the host, without brackets
/// around an IPv6 literal, and the port. Brackets hold only an IPv6 literal,
/// an IPv6 literal needs them, and the port is bare ASCII digits.
pub(crate) fn split_host(value: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = match value.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            if !matches!(literal(host), Some(IpAddr::V6(_))) {
                return None;
            }
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':')?)),
            }
        }
        None => match value.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        },
    };
    if host.is_empty() || (!value.starts_with('[') && host.contains(':')) {
        return None;
    }
    match port {
        Some(port) if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) => None,
        Some(port) => Some((host, Some(port.parse().ok()?))),
        None => Some((host, None)),
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

    // covers: S338 (private, link-local, loopback and IPv4-mapped peers)
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

    // covers: S338 (an IPv6 peer in the computer's own /64)
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
        let teredo = v6("2001:0:4136:e378:8000:63bf:3fff:fdd2");
        assert!(
            !admits_peer(v6("2001:0:4136:e378::1"), teredo),
            "a Teredo /64 is the relay's, not a link"
        );
        let six_to_four = v6("2002:cb00:7101::10");
        assert!(
            !admits_peer(v6("2002:cb00:7101::20"), six_to_four),
            "a 6to4 /64 is a public IPv4 address's"
        );
        assert!(!admits_peer(v6("ff02::1:2"), v6("ff02::1")), "multicast");
        assert!(
            admits_peer(v6("2001:db9::5"), v6("2001:db9::10")),
            "2001::/16 outside Teredo stays native"
        );
        assert!(admits_peer(v4(192, 168, 1, 20), local), "private stays in");
        assert!(admits_peer(v6("fe80::1"), local));
    }

    // covers: S338
    #[test]
    fn a_carrier_nat_peer_is_let_in() {
        let loopback = v4(127, 0, 0, 1);
        for allowed in [
            v4(100, 64, 0, 1),
            v4(100, 100, 1, 1),
            v4(100, 127, 255, 255),
        ] {
            assert!(admits_peer(allowed, loopback), "{allowed}");
        }
        let mapped = IpAddr::V6(Ipv4Addr::new(100, 100, 1, 1).to_ipv6_mapped());
        assert!(admits_peer(mapped, loopback));
        for refused in [v4(100, 63, 255, 255), v4(100, 128, 0, 1)] {
            assert!(!admits_peer(refused, loopback), "{refused}");
        }
        // The address shown for the phone to open stays a home network's.
        assert!(!is_private_peer(v4(100, 100, 1, 1)));
    }

    // covers: S369, S378
    #[test]
    fn every_address_is_a_peer_of_its_own() {
        let phone = v6("2001:db8:1:2:aaaa:bbbb:cccc:dddd");
        let laptop = v6("2001:db8:1:2:1111:2222:3333:4444");
        assert_ne!(peer_group(phone), peer_group(laptop));
        assert_eq!(peer_group(phone), phone);
        let unique_local = IpAddr::V6(Ipv6Addr::new(0xfdab, 0, 0, 7, 1, 2, 3, 4));
        let unique_local_next = IpAddr::V6(Ipv6Addr::new(0xfdab, 0, 0, 7, 1, 2, 3, 5));
        assert_ne!(peer_group(unique_local), peer_group(unique_local_next));
        assert_ne!(peer_group(v6("fe80::1")), peer_group(v6("fe80::2")));
        assert_eq!(peer_group(v6("::1")), v6("::1"));
        assert_ne!(
            peer_group(v4(192, 168, 1, 20)),
            peer_group(v4(192, 168, 1, 21))
        );
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped());
        assert_eq!(peer_group(mapped), v4(192, 168, 1, 20));
    }

    // covers: S373
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
        assert!(!host_allowed(&format!("[{home}]:7655"), local, name));
        assert!(!host_allowed("[kitchen.local]:7655", local, name));
        assert!(!host_allowed("kitchen.local:+7655", local, name));
        assert!(!host_allowed(&format!("{home}:+7655"), local, name));
        assert!(!host_allowed("kitchen.local:", local, name));
        assert!(!host_allowed(&format!("{home}:"), local, name));
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
        assert!(!host_allowed("fe80::1:7655", link, None));
        assert!(!host_allowed("[fe80::1]:+7655", link, None));
        assert!(!host_allowed("[fe80::1]:", link, None));

        let web = SocketAddr::new(v4(10, 0, 0, 2), 80);
        assert!(host_allowed(&v4(10, 0, 0, 2).to_string(), web, None));
    }

    // covers: S373
    #[test]
    fn a_blank_local_name_or_a_lookalike_host_never_matches() {
        let local = SocketAddr::new(v4(192, 168, 1, 5), 7655);
        for blank in ["", ".local"] {
            assert!(
                !host_allowed(".local:7655", local, Some(blank)),
                "{blank:?}"
            );
            assert!(!host_allowed("local:7655", local, Some(blank)), "{blank:?}");
        }
        let name = Some("kitchen");
        for lookalike in [
            "kitchen.local.evil.example:7655",
            "evil.kitchen.local:7655",
            "kitchen:7655",
            "user@kitchen.local:7655",
            "kitchen.local:7655:7655",
            "kitchen.local:99999",
            "kitchen.local:",
            "[kitchen.local]:7655",
            "kitchen.local:+7655",
            "kitchen.local: 7655",
            "kitchen.local:7655 x",
            "kitchen.local:\u{ff17}655",
        ] {
            assert!(!host_allowed(lookalike, local, name), "{lookalike}");
        }
    }

    // covers: S373
    #[test]
    fn an_origin_with_a_path_or_credentials_is_not_the_pages() {
        let host = "kitchen.local:7655";
        assert!(origin_matches(" http://kitchen.local:7655 ", host));
        for origin in [
            "http://kitchen.local:7655/",
            "http://kitchen.local:7655/api/pair",
            "http://user@kitchen.local:7655",
            "http://kitchen.local",
            "HTTP://kitchen.local:7655",
            "",
        ] {
            assert!(!origin_matches(origin, host), "{origin}");
        }
    }

    // covers: S373
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
