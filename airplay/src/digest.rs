//! RFC 2617 digest authentication for password-protected receivers (P36,
//! P37, P38).
//!
//! This module computes; the retry and nonce rules live in the session. The
//! hash is the RustCrypto `md-5` crate.

use md5::{Digest, Md5};

use crate::rtsp::{Headers, Method};

/// The user name for the audio service (P36).
pub const USERNAME: &str = "iTunes";

/// A `WWW-Authenticate: Digest ...` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    /// Returned unchanged when present (RFC 2617 §3.2.2).
    pub opaque: Option<String>,
    /// The challenge offers `qop=auth` (P37).
    pub qop_auth: bool,
    /// The challenge carries a `qop` directive at all. With `qop_auth` false
    /// it asks only for options the sender doesn't implement, such as
    /// `auth-int`; see [`Challenge::needs_unsupported_qop`].
    pub qop_offered: bool,
    /// `stale=true`: only the nonce expired (P38).
    pub stale: bool,
}

impl Challenge {
    /// Reads one header value. `None` unless it is a Digest challenge with a
    /// realm and a nonce whose algorithm is MD5 or unnamed (MD5 is the
    /// default, RFC 2617 §3.2.1).
    pub fn parse(value: &str) -> Option<Challenge> {
        let value = value.trim_start();
        let scheme_end = value.find(char::is_whitespace).unwrap_or(value.len());
        if !value[..scheme_end].eq_ignore_ascii_case("Digest") {
            return None;
        }
        let mut realm = None;
        let mut nonce = None;
        let mut opaque = None;
        let mut qop_auth = false;
        let mut qop_offered = false;
        let mut stale = false;
        for (name, value) in auth_params(&value[scheme_end..])? {
            match name.as_str() {
                "realm" => realm = Some(value),
                "nonce" => nonce = Some(value),
                "opaque" => opaque = Some(value),
                "stale" => stale = value.eq_ignore_ascii_case("true"),
                "qop" => {
                    qop_offered = true;
                    qop_auth = value
                        .split(',')
                        .any(|option| option.trim().eq_ignore_ascii_case("auth"));
                }
                "algorithm" if !value.eq_ignore_ascii_case("MD5") => return None,
                _ => {}
            }
        }
        Some(Challenge {
            realm: realm?,
            nonce: nonce?,
            opaque,
            qop_auth,
            qop_offered,
            stale,
        })
    }

    /// A `qop` directive without `auth`: the receiver insists on a quality
    /// of protection the sender doesn't implement (such as `auth-int`, RFC
    /// 2617 §3.2.1), so no answer can satisfy it. Answering without `qop`
    /// would only be read as a wrong password, so the session reports a
    /// protocol error instead (P37, P140).
    pub fn needs_unsupported_qop(&self) -> bool {
        self.qop_offered && !self.qop_auth
    }

    /// The Digest challenge among a response's `WWW-Authenticate` headers:
    /// the first one the sender can answer, otherwise the first one.
    pub fn from_headers(headers: &Headers) -> Option<Challenge> {
        let mut first = None;
        for challenge in headers
            .get_all("WWW-Authenticate")
            .filter_map(Challenge::parse)
        {
            if !challenge.needs_unsupported_qop() {
                return Some(challenge);
            }
            first.get_or_insert(challenge);
        }
        first
    }
}

/// The comma-separated `name=value` list after the scheme; values are tokens
/// or quoted strings with backslash escapes. `None` when malformed.
fn auth_params(input: &str) -> Option<Vec<(String, String)>> {
    let mut params = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        if chars.peek().is_none() {
            return Some(params);
        }
        let mut name = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',' && !c.is_whitespace()) {
            name.push(c);
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next() != Some('=') || name.is_empty() {
            return None;
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',' && !c.is_whitespace()) {
                value.push(c);
            }
        }
        params.push((name.to_ascii_lowercase(), value));
    }
}

/// MD5 written as 32 lowercase hex digits (P37).
pub fn md5_hex(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter() {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    hex
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The `qop=auth` inputs: the nonce count and the client nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QopAuth<'a> {
    pub nc: u32,
    pub cnonce: &'a str,
}

/// The request digest of RFC 2617 §3.2.2 (P37). Without `qop` it is
/// MD5(HA1 ":" nonce ":" HA2); with `qop=auth` it is MD5(HA1 ":" nonce ":"
/// nc ":" cnonce ":" "auth" ":" HA2), where HA1 = MD5(username ":" realm ":"
/// password) and HA2 = MD5(method ":" uri).
pub fn response(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    qop: Option<QopAuth<'_>>,
) -> String {
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
        Some(QopAuth { nc, cnonce }) => {
            md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:auth:{ha2}"))
        }
    }
}

/// The `Authorization` value for one request (P36, P37): user name, realm,
/// nonce, URI and response, then `qop`, `nc` and `cnonce` when the challenge
/// offered `qop=auth`, then `opaque` when it carried one. `uri` is the
/// request URI exactly as on the request line.
pub fn authorization(
    challenge: &Challenge,
    password: &str,
    method: Method,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let qop = challenge.qop_auth.then_some(QopAuth { nc, cnonce });
    let hash = response(
        USERNAME,
        &challenge.realm,
        password,
        method.as_str(),
        uri,
        &challenge.nonce,
        qop,
    );
    let mut value = format!(
        "Digest username={}, realm={}, nonce={}, uri={}, response=\"{hash}\"",
        quote(USERNAME),
        quote(&challenge.realm),
        quote(&challenge.nonce),
        quote(uri),
    );
    if qop.is_some() {
        value.push_str(&format!(
            ", qop=auth, nc={nc:08x}, cnonce={}",
            quote(cnonce)
        ));
    }
    if let Some(opaque) = &challenge.opaque {
        value.push_str(&format!(", opaque={}", quote(opaque)));
    }
    value
}

/// A quoted string with `"` and `\` escaped.
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}
