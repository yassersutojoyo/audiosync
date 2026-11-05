//! Which peers, `Host` headers and `Origin` headers the server accepts.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Whether `peer` is private (RFC 1918, RFC 4193), link-local or loopback.
pub fn is_private_peer(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// One address of this computer's interfaces, with its on-link prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalNet {
    pub address: IpAddr,
    pub prefix: u8,
    pub point_to_point: bool,
}

/// This computer's interface addresses now; none when the OS won't list them.
pub fn local_nets() -> Vec<LocalNet> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    interfaces
        .iter()
        .map(|interface| LocalNet {
            address: interface.ip(),
            prefix: match &interface.addr {
                if_addrs::IfAddr::V4(v4) => v4.prefixlen,
                if_addrs::IfAddr::V6(v6) => v6.prefixlen,
            },
            point_to_point: interface.is_p2p(),
        })
        .collect()
}

/// Whether a connection from `peer` to this machine's `local` address is
/// let in: a loopback peer, a peer on the subnet of one of `nets`, or a
/// tailnet peer (100.64/10 or fc00::/7) that reached a tailnet address of
/// `nets` held alone or on a point-to-point link, as Tailscale holds its
/// addresses on the Mac and on Windows.
pub fn admits_peer(peer: IpAddr, local: IpAddr, nets: &[LocalNet]) -> bool {
    let (peer, local) = (peer.to_canonical(), local.to_canonical());
    peer.is_loopback()
        || nets.iter().any(|net| {
            let own = net.address.to_canonical();
            let alone = net.prefix == if own.is_ipv4() { 32 } else { 128 };
            on_link(peer, own, net.prefix)
                || (own == local && (alone || net.point_to_point) && tailnet(peer) && tailnet(own))
        })
}

fn tailnet(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64,
        IpAddr::V6(v6) => v6.segments()[0] & 0xfe00 == 0xfc00,
    }
}

/// Whether `peer` shares the first `prefix` bits of `own`. Teredo
/// (2001::/32) and 6to4 (2002::/16) prefixes are built from a public IPv4
/// address that strangers on the internet share, so they are no link. A
/// prefix of 0 is an address the OS listed without a netmask, not a link
/// to the whole internet.
fn on_link(peer: IpAddr, own: IpAddr, prefix: u8) -> bool {
    if prefix == 0 {
        return false;
    }
    let (peer, own, width) = match (peer, own) {
        (IpAddr::V4(peer), IpAddr::V4(own)) => (peer.to_bits().into(), own.to_bits().into(), 32),
        (IpAddr::V6(peer), IpAddr::V6(own)) => {
            let segments = own.segments();
            if segments[..2] == [0x2001, 0] || segments[0] == 0x2002 {
                return false;
            }
            (peer.to_bits(), own.to_bits(), 128)
        }
        _ => return false,
    };
    let host_bits = width - u32::from(prefix).min(width);
    (peer ^ own).checked_shr(host_bits).unwrap_or(0) == 0
}

/// The peer that per-peer limits count against: the full address. Not an
/// IPv6 /64: every IPv6 device let in shares one (see [`admits_peer`]), so
/// one device would lock out the rest; the global limits cover a device
/// that rotates temporary addresses (RFC 8981).
pub fn peer_group(peer: IpAddr) -> IpAddr {
    peer.to_canonical()
}

/// Whether a host on the link can take as many addresses like `peer` as it
/// likes: any IPv6 address (RFC 4862, RFC 8981) or an IPv4 link-local one
/// (RFC 3927), unlike an IPv4 address the network hands out.
pub fn made_up_address(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(_) => true,
    }
}

/// The address a paired device's token is bound to: an IPv4 address whole,
/// an IPv6 address by its /64, where a phone's temporary address changes
/// at least daily (RFC 8981).
pub fn pairing_address(peer: IpAddr) -> IpAddr {
    match peer.to_canonical() {
        IpAddr::V6(address) => {
            IpAddr::V6(Ipv6Addr::from(address.to_bits() & !u128::from(u64::MAX)))
        }
        v4 => v4,
    }
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

    fn ula(subnet: u16, host: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0xfd00, subnet, 0, 0, 0, 0, 0, host))
    }

    // covers: S338 (a private peer outside every local subnet)
    #[test]
    fn a_private_peer_outside_every_local_subnet_is_refused() {
        let wifi = LocalNet {
            address: v4(192, 168, 1, 10),
            prefix: 24,
            point_to_point: false,
        };
        let local = wifi.address;
        assert!(admits_peer(v4(192, 168, 1, 20), local, &[wifi]));
        assert!(admits_peer(v4(127, 0, 0, 1), local, &[wifi]));
        assert!(admits_peer(v6("::1"), local, &[wifi]));
        for refused in [
            v4(192, 168, 2, 20),
            v4(10, 1, 2, 3),
            v4(172, 16, 0, 1),
            v4(169, 254, 10, 1),
            v4(100, 100, 1, 1),
            ula(0, 5),
            v6("fe80::1"),
        ] {
            assert!(!admits_peer(refused, local, &[wifi]), "{refused}");
        }
    }

    // covers: S338 (an interface address listed without a netmask)
    #[test]
    fn an_address_without_a_netmask_is_no_subnet() {
        let bare = net(v4(192, 168, 1, 10), 0);
        assert!(!admits_peer(v4(8, 8, 8, 8), bare.address, &[bare]));
        assert!(!admits_peer(v4(192, 168, 1, 20), bare.address, &[bare]));
        let bare6 = net(v6("2001:db8:1:2::10"), 0);
        assert!(!admits_peer(v6("2001:db9::5"), bare6.address, &[bare6]));
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

    fn net(address: IpAddr, prefix: u8) -> LocalNet {
        LocalNet {
            address,
            prefix,
            point_to_point: false,
        }
    }

    // covers: S338 (an IPv6 peer on the computer's own subnet)
    #[test]
    fn a_global_ipv6_peer_on_the_same_link_is_let_in() {
        // A phone that resolved the .local name to a global AAAA connects
        // from its own global address in the same /64.
        let local = v6("2001:db8:1:2::10");
        let nets = [net(local, 64), net(v6("fe80::10"), 64)];
        assert!(admits_peer(
            v6("2001:db8:1:2:aaaa:bbbb:cccc:dddd"),
            local,
            &nets
        ));
        assert!(admits_peer(v6("2001:db8:1:2::10"), local, &nets));
        assert!(
            !admits_peer(v6("2001:db8:1:3::5"), local, &nets),
            "another prefix"
        );
        assert!(
            !admits_peer(v6("2001:db9:1:2::5"), local, &nets),
            "another prefix"
        );
        assert!(admits_peer(v6("fe80::1"), local, &nets));
        assert!(!admits_peer(v6("::5"), v6("::"), &[]), "no link to share");
        let teredo = v6("2001:0:4136:e378:8000:63bf:3fff:fdd2");
        assert!(
            !admits_peer(v6("2001:0:4136:e378::1"), teredo, &[net(teredo, 64)]),
            "a Teredo /64 is the relay's, not a link"
        );
        let six_to_four = v6("2002:cb00:7101::10");
        assert!(
            !admits_peer(
                v6("2002:cb00:7101::20"),
                six_to_four,
                &[net(six_to_four, 64)]
            ),
            "a 6to4 /64 is a public IPv4 address's"
        );
        let native = v6("2001:db9::10");
        assert!(
            admits_peer(v6("2001:db9::5"), native, &[net(native, 64)]),
            "2001::/16 outside Teredo stays native"
        );
        let wide = net(v4(192, 168, 1, 10), 22);
        assert!(
            admits_peer(v4(192, 168, 3, 20), local, &[wide]),
            "the interface's own prefix"
        );
        assert!(!admits_peer(v4(192, 168, 4, 20), local, &[wide]));
    }

    // covers: S338 (a tailnet peer)
    #[test]
    fn a_tailnet_peer_is_let_in() {
        // Tailscale on the Mac holds its address alone on a point-to-point link.
        let tunnel = LocalNet {
            address: v4(100, 101, 102, 103),
            prefix: 32,
            point_to_point: true,
        };
        let wifi = net(v4(192, 168, 1, 10), 24);
        let nets = [wifi, tunnel];
        let mapped = IpAddr::V6(Ipv4Addr::new(100, 100, 1, 1).to_ipv6_mapped());
        for allowed in [v4(100, 64, 0, 1), v4(100, 127, 255, 255), mapped] {
            assert!(admits_peer(allowed, tunnel.address, &nets), "{allowed}");
        }
        for refused in [
            v4(100, 63, 255, 255),
            v4(100, 128, 0, 1),
            v4(192, 168, 2, 1),
        ] {
            assert!(!admits_peer(refused, tunnel.address, &nets), "{refused}");
        }
        assert!(
            !admits_peer(v4(100, 100, 1, 1), wifi.address, &nets),
            "only through the tailnet address"
        );
        let wide = net(v4(100, 101, 102, 103), 10);
        assert!(admits_peer(v4(100, 100, 1, 1), wide.address, &[wide]));
        // A network that hands out carrier-grade NAT addresses lets in its own subnet only.
        let hotel = net(v4(100, 70, 4, 9), 22);
        assert!(admits_peer(v4(100, 70, 5, 1), hotel.address, &[hotel]));
        assert!(!admits_peer(v4(100, 100, 1, 1), hotel.address, &[hotel]));
        let tunnel6 = LocalNet {
            address: ula(0xa, 1),
            prefix: 128,
            point_to_point: true,
        };
        assert!(admits_peer(ula(0xa, 2), tunnel6.address, &[tunnel6]));
        assert!(!admits_peer(v6("2001:db8::2"), tunnel6.address, &[tunnel6]));
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

    // covers: S369
    #[test]
    fn only_ipv6_and_ipv4_link_local_addresses_count_as_made_up() {
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped());
        assert!(!made_up_address(mapped));
        assert!(!made_up_address(v4(192, 168, 1, 20)));
        assert!(!made_up_address(v4(127, 0, 0, 1)));
        assert!(made_up_address(v4(169, 254, 3, 4)));
        assert!(made_up_address(v6("::1")));
        assert!(made_up_address(v6("fe80::1")));
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
