//! Device authentication (C119 to C128): the challenge the sender sends
//! right after TLS, and the judgement of the device's reply against the
//! Cast roots compiled in here.

use std::fmt;
use std::time::Duration;

use prost::Message;
use ring::signature::{self, UnparsedPublicKey, VerificationAlgorithm};
use sha2::{Digest, Sha256};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::oid_registry::{
    OID_PKCS1_RSAENCRYPTION, OID_PKCS1_SHA1WITHRSA, OID_PKCS1_SHA256WITHRSA,
    OID_PKCS1_SHA384WITHRSA, OID_PKCS1_SHA512WITHRSA, OID_X509_EXT_BASIC_CONSTRAINTS,
    OID_X509_EXT_KEY_USAGE,
};
use x509_parser::prelude::FromDer;
use x509_parser::public_key::PublicKey;

use crate::message::{
    BROADCAST, CastMessage, NS_DEVICE_AUTH, PLATFORM, PayloadType, ProtocolVersion, SENDER,
};

/// Cast Root CA (C126), from Open Screen, BSD-3-Clause.
pub const CAST_ROOT_CA: &[u8] = include_bytes!("../certs/cast_root_ca.der");
/// Eureka Root CA (C126), from Open Screen, BSD-3-Clause.
pub const EUREKA_ROOT_CA: &[u8] = include_bytes!("../certs/eureka_root_ca.der");
/// The trust anchors of every chain (C126).
pub const ROOTS: [&[u8]; 2] = [CAST_ROOT_CA, EUREKA_ROOT_CA];

/// The wait for a reply (C119).
pub const AUTH_WAIT: Duration = Duration::from_secs(5);
/// The wait for a device whose entry holds the mark (C119).
pub const MARKED_AUTH_WAIT: Duration = Duration::from_secs(1);
/// The most intermediates a reply may carry (C125).
pub const MAX_INTERMEDIATES: usize = 8;
/// The longest the device's TLS certificate may stay valid from now (C124).
const TLS_CERTIFICATE_SPAN: i64 = 96 * 3600;
/// The policy that marks a device audio-only (C125).
const AUDIO_ONLY_POLICY: &str = "1.3.6.1.4.1.11129.2.5.2";

pub const SIGNATURE_UNSPECIFIED: i32 = 0;
pub const SIGNATURE_RSASSA_PKCS1V15: i32 = 1;
pub const SIGNATURE_RSASSA_PSS: i32 = 2;
pub const HASH_SHA1: i32 = 0;
pub const HASH_SHA256: i32 = 1;

/// `DeviceAuthMessage` (C120).
#[derive(Clone, PartialEq, Message)]
pub struct DeviceAuthMessage {
    #[prost(message, optional, tag = "1")]
    pub challenge: Option<AuthChallenge>,
    #[prost(message, optional, tag = "2")]
    pub response: Option<AuthResponse>,
    #[prost(message, optional, tag = "3")]
    pub error: Option<AuthError>,
}

/// `AuthChallenge` (C120). Enums are kept as their numbers, so a value the
/// sender doesn't know still reaches the checks.
#[derive(Clone, PartialEq, Message)]
pub struct AuthChallenge {
    #[prost(int32, optional, tag = "1")]
    pub signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "3")]
    pub hash_algorithm: Option<i32>,
}

/// `AuthResponse` (C120).
#[derive(Clone, PartialEq, Message)]
pub struct AuthResponse {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub signature: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub client_auth_certificate: Option<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub intermediate_certificate: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "6")]
    pub hash_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub crl: Option<Vec<u8>>,
}

/// `AuthError` (C120).
#[derive(Clone, PartialEq, Message)]
pub struct AuthError {
    #[prost(int32, optional, tag = "1")]
    pub error_type: Option<i32>,
}

/// A fresh nonce from the operating system's secure source (C121).
pub fn new_nonce() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // A source that fails leaves zeros: the reply then fails or declines,
    // never verifies against a guessable nonce it could not have signed.
    let _ = getrandom::fill(&mut nonce);
    nonce
}

/// The challenge of C120 with `nonce`.
pub fn challenge(nonce: &[u8; 16]) -> CastMessage {
    let message = DeviceAuthMessage {
        challenge: Some(AuthChallenge {
            signature_algorithm: None,
            sender_nonce: Some(nonce.to_vec()),
            hash_algorithm: Some(HASH_SHA256),
        }),
        response: None,
        error: None,
    };
    CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: SENDER.to_owned(),
        destination_id: PLATFORM.to_owned(),
        namespace: NS_DEVICE_AUTH.to_owned(),
        payload_type: PayloadType::Binary as i32,
        payload_utf8: None,
        payload_binary: Some(message.encode_to_vec()),
    }
}

/// Whether `message` is the reply to a challenge (C122).
pub fn is_reply(message: &CastMessage) -> bool {
    message.namespace == NS_DEVICE_AUTH
        && (message.destination_id == SENDER || message.destination_id == BROADCAST)
}

/// The SHA-256 of a device certificate's SubjectPublicKeyInfo DER (C129).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPin(pub [u8; 32]);

impl KeyPin {
    pub fn of_spki(der: &[u8]) -> KeyPin {
        KeyPin(Sha256::digest(der).into())
    }

    /// The first 16 hex digits, as the log shows a pin (C132).
    pub fn short(&self) -> String {
        self.to_string()[..16].to_owned()
    }
}

impl fmt::Display for KeyPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for KeyPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Why a reply failed its checks (C122, C132).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Malformed,
    Nonce,
    SignatureAlgorithm,
    Signature,
    DeviceKey,
    NoPeerCertificate,
    TlsCertificateWindow,
    TooManyIntermediates,
    CertificateParse,
    Chain,
    CriticalExtension,
}

impl Failure {
    pub fn reason(self) -> &'static str {
        match self {
            Failure::Malformed => "malformed",
            Failure::Nonce => "nonce",
            Failure::SignatureAlgorithm => "signature algorithm",
            Failure::Signature => "signature",
            Failure::DeviceKey => "device key",
            Failure::NoPeerCertificate => "no peer certificate",
            Failure::TlsCertificateWindow => "TLS certificate window",
            Failure::TooManyIntermediates => "too many intermediates",
            Failure::CertificateParse => "certificate parse",
            Failure::Chain => "chain",
            Failure::CriticalExtension => "critical extension",
        }
    }
}

/// A reply that passed every check (C122).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub key: KeyPin,
    /// The device signed with SHA-1 (C123).
    pub sha1: bool,
    pub nonce_echoed: bool,
    pub audio_only: bool,
    /// The length of the reply's `crl`, which is never parsed (C127).
    pub crl: Option<usize>,
}

/// The four outcomes of a challenge (C122).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Verified(Verified),
    Failed(Failure),
    /// The device answered with `error`, of this `error_type`.
    Declined(i32),
    /// No reply within this wait.
    NoAnswer(Duration),
}

impl fmt::Display for Outcome {
    /// The outcome as C132 logs it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Verified(verified) => {
                let digest = if verified.sha1 { "sha1" } else { "sha256" };
                let nonce = if verified.nonce_echoed {
                    "echoed"
                } else {
                    "missing"
                };
                let policy = if verified.audio_only {
                    "audio-only"
                } else {
                    "none"
                };
                write!(
                    f,
                    "verified, {digest}, nonce {nonce}, policy {policy}, crl "
                )?;
                match verified.crl {
                    Some(length) => write!(f, "{length} bytes")?,
                    None => f.write_str("none")?,
                }
                write!(f, ", key {}", verified.key.short())
            }
            Outcome::Failed(failure) => write!(f, "failed, {}", failure.reason()),
            Outcome::Declined(error_type) => match error_type {
                0 => f.write_str("declined, INTERNAL_ERROR"),
                1 => f.write_str("declined, NO_TLS"),
                2 => f.write_str("declined, SIGNATURE_ALGORITHM_UNAVAILABLE"),
                other => write!(f, "declined, {other}"),
            },
            Outcome::NoAnswer(wait) => write!(f, "no answer in {} s", wait.as_secs()),
        }
    }
}

/// Judges `reply` to the challenge with `nonce` (C122 to C125). `peer` is
/// the DER of the certificate this connection's TLS handshake presented,
/// `now` the host's UTC time in Unix seconds, `roots` the trust anchors.
pub fn judge(
    reply: &CastMessage,
    nonce: &[u8; 16],
    peer: Option<&[u8]>,
    now: i64,
    roots: &[&[u8]],
) -> Outcome {
    let Some(payload) = reply
        .payload_binary
        .as_deref()
        .filter(|_| reply.payload_type == PayloadType::Binary as i32)
    else {
        return Outcome::Failed(Failure::Malformed);
    };
    let Ok(message) = DeviceAuthMessage::decode(payload) else {
        return Outcome::Failed(Failure::Malformed);
    };
    if let Some(error) = message.error {
        return Outcome::Declined(error.error_type.unwrap_or(0));
    }
    let Some(response) = message.response else {
        return Outcome::Failed(Failure::Malformed);
    };
    match check(&response, nonce, peer, now, roots) {
        Ok(verified) => Outcome::Verified(verified),
        Err(failure) => Outcome::Failed(failure),
    }
}

fn check(
    response: &AuthResponse,
    nonce: &[u8; 16],
    peer: Option<&[u8]>,
    now: i64,
    roots: &[&[u8]],
) -> Result<Verified, Failure> {
    if response
        .sender_nonce
        .as_deref()
        .is_some_and(|echoed| echoed != nonce)
    {
        return Err(Failure::Nonce);
    }
    if response
        .signature_algorithm
        .is_some_and(|algorithm| algorithm != SIGNATURE_RSASSA_PKCS1V15)
    {
        return Err(Failure::SignatureAlgorithm);
    }
    let sha1 = match response.hash_algorithm {
        None | Some(HASH_SHA1) => true,
        Some(HASH_SHA256) => false,
        Some(_) => return Err(Failure::SignatureAlgorithm),
    };
    let signature = response.signature.as_deref().unwrap_or_default();
    if signature.is_empty() {
        return Err(Failure::Signature);
    }
    let peer = peer.ok_or(Failure::NoPeerCertificate)?;
    let tls = parse(peer)?;
    let validity = tls.validity();
    let (from, until) = (
        validity.not_before.timestamp(),
        validity.not_after.timestamp(),
    );
    if from > now || until < now || until - now > TLS_CERTIFICATE_SPAN {
        return Err(Failure::TlsCertificateWindow);
    }
    if response.intermediate_certificate.len() > MAX_INTERMEDIATES {
        return Err(Failure::TooManyIntermediates);
    }
    let device_der = response
        .client_auth_certificate
        .as_deref()
        .ok_or(Failure::CertificateParse)?;
    let device = parse(device_der)?;
    let intermediates = (response.intermediate_certificate.iter())
        .map(|der| parse(der))
        .collect::<Result<Vec<_>, _>>()?;
    let roots = (roots.iter())
        .map(|der| parse(der).map_err(|_| Failure::Chain))
        .collect::<Result<Vec<_>, _>>()?;
    let device_key = rsa_key(&device).ok_or(Failure::DeviceKey)?;
    let mut signed = response.sender_nonce.clone().unwrap_or_default();
    signed.extend_from_slice(peer);
    let digest: &dyn VerificationAlgorithm = if sha1 {
        &signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY
    } else {
        &signature::RSA_PKCS1_2048_8192_SHA256
    };
    UnparsedPublicKey::new(digest, device_key)
        .verify(&signed, signature)
        .map_err(|_| Failure::Signature)?;
    let audio_only = chain(&device, &intermediates, &roots, now)?;
    Ok(Verified {
        key: KeyPin::of_spki(device.public_key().raw),
        sha1,
        nonce_echoed: response.sender_nonce.is_some(),
        audio_only,
        crl: response.crl.as_ref().map(Vec::len),
    })
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, Failure> {
    match X509Certificate::from_der(der) {
        Ok(([], certificate)) => Ok(certificate),
        _ => Err(Failure::CertificateParse),
    }
}

/// The certificate's key as ring takes it, when it is RSA of 2048 bits or
/// more (C123, C125).
fn rsa_key<'a>(certificate: &'a X509Certificate<'_>) -> Option<&'a [u8]> {
    let info = certificate.public_key();
    if info.algorithm.algorithm != OID_PKCS1_RSAENCRYPTION {
        return None;
    }
    match info.parsed() {
        Ok(PublicKey::RSA(key)) if key.key_size() >= 2048 => {
            Some(info.subject_public_key.data.as_ref())
        }
        _ => None,
    }
}

/// Builds the path from `device` to a root by issuer and subject names and
/// checks it (C125); whether a certificate on it marks the device
/// audio-only.
fn chain(
    device: &X509Certificate<'_>,
    intermediates: &[X509Certificate<'_>],
    roots: &[X509Certificate<'_>],
    now: i64,
) -> Result<bool, Failure> {
    let mut path = vec![device];
    let mut used = vec![false; intermediates.len()];
    loop {
        let issuer = path[path.len() - 1].issuer().as_raw();
        if let Some(root) = roots.iter().find(|root| root.subject().as_raw() == issuer) {
            path.push(root);
            break;
        }
        let next = (intermediates.iter().enumerate())
            .find(|(index, candidate)| !used[*index] && candidate.subject().as_raw() == issuer);
        let Some((index, intermediate)) = next else {
            return Err(Failure::Chain);
        };
        used[index] = true;
        path.push(intermediate);
    }
    let mut audio_only = false;
    for (index, certificate) in path.iter().enumerate() {
        let is_root = index == path.len() - 1;
        for extension in certificate.extensions() {
            if extension.critical
                && extension.oid != OID_X509_EXT_BASIC_CONSTRAINTS
                && extension.oid != OID_X509_EXT_KEY_USAGE
            {
                return Err(Failure::CriticalExtension);
            }
            if let ParsedExtension::CertificatePolicies(policies) = extension.parsed_extension()
                && (policies.iter())
                    .any(|policy| policy.policy_id.to_id_string() == AUDIO_ONLY_POLICY)
            {
                audio_only = true;
            }
        }
        let key = rsa_key(certificate).ok_or(Failure::Chain)?;
        let constraints = certificate
            .basic_constraints()
            .map_err(|_| Failure::Chain)?;
        let usage = certificate.key_usage().map_err(|_| Failure::Chain)?;
        if index > 0 {
            let Some(constraints) = constraints.filter(|found| found.value.ca) else {
                return Err(Failure::Chain);
            };
            if usage.is_some_and(|usage| !usage.value.key_cert_sign()) {
                return Err(Failure::Chain);
            }
            // The intermediates below this issuer, the device not counted.
            let below = (index - 1) as u32;
            if (constraints.value.path_len_constraint).is_some_and(|limit| below > limit) {
                return Err(Failure::Chain);
            }
            signed_by(path[index - 1], key)?;
        } else if !usage.is_some_and(|usage| usage.value.digital_signature()) {
            return Err(Failure::Chain);
        }
        if !is_root {
            let validity = certificate.validity();
            if validity.not_before.timestamp() > now || validity.not_after.timestamp() < now {
                return Err(Failure::Chain);
            }
        }
    }
    Ok(audio_only)
}

/// Checks `certificate`'s signature with its issuer's `key` (C125).
fn signed_by(certificate: &X509Certificate<'_>, key: &[u8]) -> Result<(), Failure> {
    let algorithm = &certificate.signature_algorithm;
    if *algorithm != certificate.tbs_certificate.signature {
        return Err(Failure::Chain);
    }
    let verification: &dyn VerificationAlgorithm = match &algorithm.algorithm {
        oid if *oid == OID_PKCS1_SHA1WITHRSA => {
            &signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY
        }
        oid if *oid == OID_PKCS1_SHA256WITHRSA => &signature::RSA_PKCS1_2048_8192_SHA256,
        oid if *oid == OID_PKCS1_SHA384WITHRSA => &signature::RSA_PKCS1_2048_8192_SHA384,
        oid if *oid == OID_PKCS1_SHA512WITHRSA => &signature::RSA_PKCS1_2048_8192_SHA512,
        _ => return Err(Failure::Chain),
    };
    UnparsedPublicKey::new(verification, key)
        .verify(
            certificate.tbs_certificate.as_ref(),
            &certificate.signature_value.data,
        )
        .map_err(|_| Failure::Chain)
}
