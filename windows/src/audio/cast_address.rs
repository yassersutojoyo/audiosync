//! A Google Cast speaker the user adds by address, for a network where
//! multicast DNS is blocked (spec C18): a host name or IPv4 address with an
//! optional port, 8009 when none is given.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use audiosync_cast::discovery::DeviceId;

/// The port a single Cast device listens on (spec C14).
pub const DEFAULT_PORT: u16 = 8009;

/// The most addresses the settings keep.
pub const MAX_ADDRESSES: usize = 64;

/// How long Add by Address… waits for an address to answer.
pub const PROBE_WAIT: Duration = Duration::from_secs(3);

/// How long the connection that checks an address may take.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// The longest host name DNS allows, without its trailing dot.
const MAX_HOST_LEN: usize = 253;

/// The longest label of a host name.
const MAX_LABEL_LEN: usize = 63;

/// An address the user typed, checked and normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastAddress {
    /// Lowercase, without a trailing dot; an IPv4 address in dotted form.
    host: String,
    port: u16,
}

impl CastAddress {
    /// `host`, `host:port`, `a.b.c.d` or `a.b.c.d:port`; `None` for anything
    /// else, IPv6 included.
    pub fn parse(text: &str) -> Option<CastAddress> {
        let text = text.trim();
        let (host, port) = match text.split_once(':') {
            Some((host, port)) => (host, parse_port(port)?),
            None => (text, DEFAULT_PORT),
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
                return None;
            }
            return Some(CastAddress {
                host: ip.to_string(),
                port,
            });
        }
        is_host_name(host).then(|| CastAddress {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Set when the host is an IPv4 address, which needs no lookup.
    pub fn socket_address(&self) -> Option<SocketAddr> {
        let ip: Ipv4Addr = self.host.parse().ok()?;
        Some(SocketAddr::from((ip, self.port)))
    }

    /// How the settings keep it and its row names it: the port only when it
    /// isn't 8009.
    pub fn text(&self) -> String {
        if self.port == DEFAULT_PORT {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The ID its session and offset are kept under while no browse has
    /// found the device's own (C18). It follows from [`CastAddress::text`],
    /// so it is the same at every launch.
    pub fn device_id(&self) -> DeviceId {
        let text = self.text();
        let mut bytes = [0; 16];
        for (half, basis) in [0xcbf2_9ce4_8422_2325_u64, 0x8422_2325_cbf2_9ce4]
            .into_iter()
            .enumerate()
        {
            // FNV-1a: stable across Rust releases, which std's hasher isn't.
            let mut hash = basis;
            for byte in b"cast-address:".iter().chain(text.as_bytes()) {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
            bytes[half * 8..half * 8 + 8].copy_from_slice(&hash.to_be_bytes());
        }
        DeviceId(bytes)
    }
}

fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|&port| port != 0)
}

/// Letters, digits and inner hyphens in labels of 1 to 63 characters; a
/// last label of only digits is a malformed IPv4 address, not a name.
fn is_host_name(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST_LEN {
        return false;
    }
    let labels_ok = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    let last_numeric = host
        .rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()));
    labels_ok && !last_numeric
}

/// Looks the host up, IPv4 only. It blocks, so it runs off the UI thread.
pub fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IPv4 address"))
}

/// Whether something answers at `address`: its host resolves and its port
/// takes a connection, within [`PROBE_WAIT`]. Both run on a thread of their
/// own, so a slow lookup costs no more than the wait.
pub fn answers(address: &CastAddress) -> bool {
    let (sender, receiver) = mpsc::channel();
    let host = address.host().to_owned();
    let port = address.port();
    let spawned = thread::Builder::new()
        .name("cast-probe".to_owned())
        .spawn(move || {
            let reached = resolve(&host, port)
                .and_then(|socket| TcpStream::connect_timeout(&socket, CONNECT_WAIT))
                .is_ok();
            let _ = sender.send(reached);
        });
    spawned.is_ok() && receiver.recv_timeout(PROBE_WAIT).unwrap_or(false)
}

/// Adds `address` to the kept list unless it is there; returns whether it
/// was added.
pub fn add(list: &mut Vec<String>, address: &CastAddress) -> bool {
    let text = address.text();
    if list.contains(&text) || list.len() >= MAX_ADDRESSES {
        return false;
    }
    list.push(text);
    true
}

/// Takes `address` out of the kept list; returns whether it was there.
pub fn remove(list: &mut Vec<String>, address: &CastAddress) -> bool {
    let text = address.text();
    let before = list.len();
    list.retain(|kept| *kept != text);
    list.len() != before
}

/// A list from the settings file as it can be used: each entry that parses,
/// normalized and once, at most [`MAX_ADDRESSES`].
pub fn clamped(list: Vec<String>) -> Vec<String> {
    let mut kept = Vec::new();
    for address in list.iter().filter_map(|text| CastAddress::parse(text)) {
        add(&mut kept, &address);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Option<(String, u16)> {
        CastAddress::parse(text).map(|address| (address.host().to_owned(), address.port()))
    }

    #[test]
    fn a_host_name_or_ipv4_address_takes_port_8009_unless_one_is_given() {
        assert_eq!(
            parsed("speaker.local"),
            Some(("speaker.local".into(), 8009))
        );
        assert_eq!(
            parsed("  Speaker.Local.  "),
            Some(("speaker.local".into(), 8009))
        );
        assert_eq!(parsed("kitchen"), Some(("kitchen".into(), 8009)));
        assert_eq!(parsed("192.0.2.10"), Some(("192.0.2.10".into(), 8009)));
        assert_eq!(
            parsed("speaker.local:8010"),
            Some(("speaker.local".into(), 8010))
        );
        assert_eq!(
            parsed("192.0.2.10:32123"),
            Some(("192.0.2.10".into(), 32123))
        );
        assert_eq!(
            parsed("192.0.2.10:65535"),
            Some(("192.0.2.10".into(), 65535))
        );
    }

    #[test]
    fn a_bad_port_or_host_or_an_empty_field_is_refused() {
        for text in [
            "",
            "   ",
            ":8009",
            "speaker.local:",
            "speaker.local:0",
            "speaker.local:65536",
            "speaker.local:123456",
            "speaker.local:80a",
            "speaker.local:+80",
            "speaker.local:80:81",
            "2001:db8::1",
            "[2001:db8::1]:8009",
            "spe aker.local",
            "-speaker.local",
            "speaker-.local",
            "speaker..local",
            "speaker_1.local",
            "http://speaker.local",
            "192.0.2",
            "192.0.2.256",
            "192.0.2.010",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.251",
        ] {
            assert_eq!(CastAddress::parse(text), None, "{text:?}");
        }
        let long_label = format!("{}.local", "a".repeat(64));
        assert_eq!(CastAddress::parse(&long_label), None);
        let long_name = ["abcdefghi"; 26].join(".");
        assert!(long_name.len() > MAX_HOST_LEN);
        assert_eq!(CastAddress::parse(&long_name), None);
    }

    #[test]
    fn an_ipv4_address_needs_no_lookup_and_a_name_does() {
        let ip = CastAddress::parse("192.0.2.10:8010").unwrap();
        assert_eq!(
            ip.socket_address(),
            Some(SocketAddr::from(([192, 0, 2, 10], 8010)))
        );
        assert_eq!(
            CastAddress::parse("speaker.local")
                .unwrap()
                .socket_address(),
            None
        );
    }

    #[test]
    fn the_text_names_the_port_only_when_it_isnt_8009() {
        let text = |input: &str| CastAddress::parse(input).unwrap().text();
        assert_eq!(text("Speaker.Local:8009"), "speaker.local");
        assert_eq!(text("speaker.local:8010"), "speaker.local:8010");
        assert_eq!(text("192.0.2.10"), "192.0.2.10");
    }

    #[test]
    fn the_device_id_follows_the_address_and_lasts() {
        let id = |input: &str| CastAddress::parse(input).unwrap().device_id();
        assert_eq!(id("speaker.local"), id("SPEAKER.local:8009"));
        assert_ne!(id("speaker.local"), id("speaker.local:8010"));
        assert_ne!(id("192.0.2.10"), id("192.0.2.11"));
        // Kept in the speakers' file, so it never changes between builds.
        assert_eq!(
            id("speaker.local").to_string(),
            "dcb5ee8cc72b855deeb006df3efeef18"
        );
    }

    #[test]
    fn an_address_answers_only_where_a_port_takes_a_connection() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let open = CastAddress::parse(&format!("127.0.0.1:{port}")).unwrap();
        assert!(answers(&open));
        drop(listener);
        assert!(!answers(&open), "closed now");
    }

    #[test]
    fn the_kept_list_adds_once_and_removes() {
        let address = |input: &str| CastAddress::parse(input).unwrap();
        let mut list = Vec::new();
        assert!(add(&mut list, &address("speaker.local")));
        assert!(!add(&mut list, &address("Speaker.local:8009")), "dedupe");
        assert!(add(&mut list, &address("192.0.2.10:8010")));
        assert_eq!(list, ["speaker.local", "192.0.2.10:8010"]);
        assert!(remove(&mut list, &address("speaker.local")));
        assert!(!remove(&mut list, &address("speaker.local")));
        assert_eq!(list, ["192.0.2.10:8010"]);
    }

    #[test]
    fn a_list_from_the_file_keeps_what_parses_once_and_at_most_the_limit() {
        let list = vec![
            "Speaker.Local".to_owned(),
            "speaker.local:8009".to_owned(),
            "not an address".to_owned(),
            "192.0.2.10".to_owned(),
        ];
        assert_eq!(clamped(list), ["speaker.local", "192.0.2.10"]);
        let many: Vec<String> = (0..200).map(|n| format!("speaker-{n}.local")).collect();
        assert_eq!(clamped(many).len(), MAX_ADDRESSES);
        let mut full: Vec<String> = (0..MAX_ADDRESSES).map(|n| format!("s{n}.local")).collect();
        assert!(!add(
            &mut full,
            &CastAddress::parse("one-more.local").unwrap()
        ));
    }
}
