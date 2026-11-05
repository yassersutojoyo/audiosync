//! Device authentication: the challenge, the checks on the reply and the
//! embedded roots (C119 to C128), against chains made in each test under a
//! test root.

use std::sync::OnceLock;
use std::time::Duration;

use audiosync_cast::auth::{
    self, AuthChallenge, AuthError, AuthResponse, CAST_ROOT_CA, DeviceAuthMessage, EUREKA_ROOT_CA,
    Failure, HASH_SHA1, HASH_SHA256, KeyPin, Outcome, SIGNATURE_RSASSA_PKCS1V15,
    SIGNATURE_RSASSA_PSS, SIGNATURE_UNSPECIFIED, Verified,
};
use audiosync_cast::message::{
    CastMessage, NS_DEVICE_AUTH, PLATFORM, PayloadType, ProtocolVersion, SENDER,
};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, PKCS_RSA_SHA256,
};
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use rustls::pki_types::PrivatePkcs8KeyDer;
use sha2::{Digest, Sha256};

/// 2030-01-02 00:00 UTC, inside every test certificate's dates.
const NOW: i64 = 1_893_542_400;
const NONCE: [u8; 16] = [7; 16];
const SHA256_PREFIX: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];
const SHA1_PREFIX: [u8; 15] = [
    0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
];
/// certificatePolicies with the audio-only policy 1.3.6.1.4.1.11129.2.5.2.
const AUDIO_ONLY_POLICIES: [u8; 16] = [
    0x30, 0x0e, 0x30, 0x0c, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0xd6, 0x79, 0x02, 0x05, 0x02,
];

fn rsa_key(bits: usize) -> (RsaPrivateKey, Vec<u8>) {
    let key = RsaPrivateKey::new(&mut rand_core::OsRng, bits).expect("RSA key");
    let pkcs8 = key.to_pkcs8_der().expect("PKCS#8").as_bytes().to_vec();
    (key, pkcs8)
}

/// One RSA 2048 key for every certificate of the binary: the checks look at
/// names and signatures, not at distinct keys.
fn key() -> &'static (RsaPrivateKey, Vec<u8>) {
    static KEY: OnceLock<(RsaPrivateKey, Vec<u8>)> = OnceLock::new();
    KEY.get_or_init(|| rsa_key(2048))
}

fn signing_key(pkcs8: &[u8]) -> KeyPair {
    KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(pkcs8.to_vec()),
        &PKCS_RSA_SHA256,
    )
    .expect("RSA signing key")
}

fn named(name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, name);
    params.distinguished_name = subject;
    params
}

fn ca(name: &str, limit: Option<u8>) -> CertificateParams {
    let mut params = named(name);
    params.is_ca = IsCa::Ca(limit.map_or(
        BasicConstraints::Unconstrained,
        BasicConstraints::Constrained,
    ));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

fn device_params() -> CertificateParams {
    let mut params = named("Test Device");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params
}

/// A test root, one intermediate and a device certificate, all valid at
/// [`NOW`], and a TLS certificate valid for two days around it.
struct Chain {
    root: Vec<u8>,
    intermediates: Vec<Vec<u8>>,
    device: Vec<u8>,
    tls: Vec<u8>,
}

fn sign_der(params: &CertificateParams, issuer: &CertificateParams) -> Vec<u8> {
    let subject_key = signing_key(&key().1);
    let issuer = Issuer::from_params(issuer, signing_key(&key().1));
    params
        .signed_by(&subject_key, &issuer)
        .expect("signed")
        .der()
        .to_vec()
}

fn tls_certificate(from: (i32, u8, u8), until: (i32, u8, u8)) -> Vec<u8> {
    let mut params = named("tls");
    params.not_before = rcgen::date_time_ymd(from.0, from.1, from.2);
    params.not_after = rcgen::date_time_ymd(until.0, until.1, until.2);
    params
        .self_signed(&signing_key(&key().1))
        .expect("TLS certificate")
        .der()
        .to_vec()
}

fn chain_with(
    root: CertificateParams,
    intermediate: CertificateParams,
    device: CertificateParams,
) -> Chain {
    Chain {
        root: root
            .self_signed(&signing_key(&key().1))
            .expect("root")
            .der()
            .to_vec(),
        intermediates: vec![sign_der(&intermediate, &root)],
        device: sign_der(&device, &intermediate),
        tls: tls_certificate((2030, 1, 1), (2030, 1, 3)),
    }
}

fn chain() -> Chain {
    chain_with(
        ca("Test Root", None),
        ca("Test Intermediate", None),
        device_params(),
    )
}

fn sign(key: &RsaPrivateKey, sha1: bool, message: &[u8]) -> Vec<u8> {
    let mut digest_info = Vec::new();
    if sha1 {
        digest_info.extend_from_slice(&SHA1_PREFIX);
        let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, message);
        digest_info.extend_from_slice(hash.as_ref());
    } else {
        digest_info.extend_from_slice(&SHA256_PREFIX);
        digest_info.extend_from_slice(&Sha256::digest(message));
    }
    key.sign(Pkcs1v15Sign::new_unprefixed(), &digest_info)
        .expect("signature")
}

/// The response a genuine device with `chain` sends to [`NONCE`].
fn response(chain: &Chain) -> AuthResponse {
    let mut signed = NONCE.to_vec();
    signed.extend_from_slice(&chain.tls);
    AuthResponse {
        signature: Some(sign(&key().0, false, &signed)),
        client_auth_certificate: Some(chain.device.clone()),
        intermediate_certificate: chain.intermediates.clone(),
        signature_algorithm: Some(SIGNATURE_RSASSA_PKCS1V15),
        sender_nonce: Some(NONCE.to_vec()),
        hash_algorithm: Some(HASH_SHA256),
        crl: None,
    }
}

fn binary(payload: Vec<u8>) -> CastMessage {
    CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: PLATFORM.to_owned(),
        destination_id: SENDER.to_owned(),
        namespace: NS_DEVICE_AUTH.to_owned(),
        payload_type: PayloadType::Binary as i32,
        payload_utf8: None,
        payload_binary: Some(payload),
    }
}

fn reply(response: AuthResponse) -> CastMessage {
    let message = DeviceAuthMessage {
        challenge: None,
        response: Some(response),
        error: None,
    };
    binary(message.encode_to_vec())
}

fn judge_at(chain: &Chain, response: AuthResponse, now: i64) -> Outcome {
    auth::judge(
        &reply(response),
        &NONCE,
        Some(&chain.tls),
        now,
        &[&chain.root],
    )
}

fn judge(chain: &Chain, response: AuthResponse) -> Outcome {
    judge_at(chain, response, NOW)
}

fn device_pin(chain: &Chain) -> KeyPin {
    let (_, device) = x509_parser::parse_x509_certificate(&chain.device).expect("device");
    KeyPin::of_spki(device.public_key().raw)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// covers: C126
#[test]
fn the_embedded_roots_are_the_recorded_cast_and_eureka_roots() {
    assert_eq!(
        hex(&Sha256::digest(CAST_ROOT_CA)),
        "809af14700b3fe2611ad597eb1584d6354313b64ccdb3390f097fa3e5826d6ca"
    );
    assert_eq!(
        hex(&Sha256::digest(EUREKA_ROOT_CA)),
        "caf6d1e37b532203e01a76bf07187bb731ccd38801565ab2211a2c0ab7f3bc46"
    );
    for (der, name, serial) in [
        (CAST_ROOT_CA, "Cast Root CA", 2u8),
        (EUREKA_ROOT_CA, "Eureka Root CA", 1),
    ] {
        let (_, root) = x509_parser::parse_x509_certificate(der).expect("root");
        assert!(root.subject().to_string().contains(&format!("CN={name}")));
        assert_eq!(root.subject(), root.issuer());
        assert_eq!(root.tbs_certificate.serial, serial.into());
    }
    assert_eq!(auth::ROOTS, [CAST_ROOT_CA, EUREKA_ROOT_CA]);
}

// covers: C120, C121
#[test]
fn the_challenge_asks_for_sha256_with_a_fresh_16_byte_nonce() {
    let nonce = auth::new_nonce().expect("random");
    assert_ne!(
        nonce,
        auth::new_nonce().expect("random"),
        "a fresh nonce per challenge"
    );
    let message = auth::challenge(&nonce);
    assert_eq!(message.source_id, SENDER);
    assert_eq!(message.destination_id, PLATFORM);
    assert_eq!(message.namespace, NS_DEVICE_AUTH);
    assert_eq!(message.payload_type, PayloadType::Binary as i32);
    assert_eq!(message.payload_utf8, None);
    let decoded = DeviceAuthMessage::decode(message.payload_binary.as_deref().expect("binary"))
        .expect("decodes");
    assert_eq!(
        decoded,
        DeviceAuthMessage {
            challenge: Some(AuthChallenge {
                signature_algorithm: None,
                sender_nonce: Some(nonce.to_vec()),
                hash_algorithm: Some(HASH_SHA256),
            }),
            response: None,
            error: None,
        }
    );
}

// covers: C122, C123, C128
#[test]
fn a_reply_signed_over_the_nonce_and_this_tls_certificate_verifies() {
    let chain = chain();
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Verified(Verified {
            key: device_pin(&chain),
            sha1: false,
            nonce_echoed: true,
            audio_only: false,
            crl: None,
        })
    );
    let other_tls = tls_certificate((2030, 1, 1), (2030, 1, 4));
    let outcome = auth::judge(
        &reply(response(&chain)),
        &NONCE,
        Some(&other_tls),
        NOW,
        &[&chain.root],
    );
    assert_eq!(
        outcome,
        Outcome::Failed(Failure::Signature),
        "signed over another connection's certificate"
    );
}

// covers: C123
#[test]
fn sha1_is_accepted_when_the_reply_names_it_or_leaves_it_out() {
    let chain = chain();
    let mut signed = NONCE.to_vec();
    signed.extend_from_slice(&chain.tls);
    for hash in [Some(HASH_SHA1), None] {
        let reply = AuthResponse {
            signature: Some(sign(&key().0, true, &signed)),
            hash_algorithm: hash,
            ..response(&chain)
        };
        let Outcome::Verified(verified) = judge(&chain, reply) else {
            panic!("SHA-1 verifies");
        };
        assert!(verified.sha1);
    }
    let unknown = AuthResponse {
        hash_algorithm: Some(7),
        ..response(&chain)
    };
    assert_eq!(
        judge(&chain, unknown),
        Outcome::Failed(Failure::SignatureAlgorithm)
    );
}

// covers: C121
#[test]
fn a_different_nonce_fails_and_a_missing_one_is_judged_on() {
    let chain = chain();
    let different = AuthResponse {
        sender_nonce: Some(vec![8; 16]),
        ..response(&chain)
    };
    assert_eq!(judge(&chain, different), Outcome::Failed(Failure::Nonce));
    // Without an echoed nonce the device signs the TLS certificate alone.
    let missing = AuthResponse {
        sender_nonce: None,
        signature: Some(sign(&key().0, false, &chain.tls)),
        ..response(&chain)
    };
    let Outcome::Verified(verified) = judge(&chain, missing) else {
        panic!("a missing nonce is judged on");
    };
    assert!(!verified.nonce_echoed);
}

// covers: C123
#[test]
fn pss_unspecified_and_unknown_signature_algorithms_fail() {
    let chain = chain();
    for algorithm in [SIGNATURE_RSASSA_PSS, SIGNATURE_UNSPECIFIED, 9] {
        let reply = AuthResponse {
            signature_algorithm: Some(algorithm),
            ..response(&chain)
        };
        assert_eq!(
            judge(&chain, reply),
            Outcome::Failed(Failure::SignatureAlgorithm)
        );
    }
    let absent = AuthResponse {
        signature_algorithm: None,
        ..response(&chain)
    };
    assert!(matches!(judge(&chain, absent), Outcome::Verified(_)));
}

// covers: C123
#[test]
fn an_empty_or_wrong_signature_and_a_missing_peer_certificate_fail() {
    let chain = chain();
    for signature in [None, Some(Vec::new()), Some(vec![1; 256])] {
        let reply = AuthResponse {
            signature,
            ..response(&chain)
        };
        assert_eq!(judge(&chain, reply), Outcome::Failed(Failure::Signature));
    }
    let outcome = auth::judge(&reply(response(&chain)), &NONCE, None, NOW, &[&chain.root]);
    assert_eq!(outcome, Outcome::Failed(Failure::NoPeerCertificate));
}

/// An RSA public key that ring would not load as a signing key.
struct SmallRsa(Vec<u8>);

impl rcgen::PublicKeyData for SmallRsa {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &PKCS_RSA_SHA256
    }
}

// covers: C123
#[test]
fn a_device_key_that_is_not_rsa_of_2048_bits_fails() {
    let root = ca("Test Root", None);
    let intermediate = ca("Test Intermediate", None);
    let issuer = Issuer::from_params(&intermediate, signing_key(&key().1));
    let ec = KeyPair::generate().expect("EC key");
    let (small, _) = rsa_key(1024);
    let small = SmallRsa(
        small
            .to_public_key()
            .to_pkcs1_der()
            .expect("PKCS#1")
            .as_bytes()
            .to_vec(),
    );
    let devices = [
        device_params().signed_by(&ec, &issuer),
        device_params().signed_by(&small, &issuer),
    ];
    for device in devices {
        let mut chain = chain_with(root.clone(), intermediate.clone(), device_params());
        chain.device = device.expect("device").der().to_vec();
        assert_eq!(
            judge(&chain, response(&chain)),
            Outcome::Failed(Failure::DeviceKey)
        );
    }
}

// covers: C124
#[test]
fn the_tls_certificate_must_be_valid_now_and_for_at_most_96_hours() {
    let mut chain = chain();
    let cases = [
        ((2030, 1, 1), (2030, 1, 6), true),
        ((2030, 1, 1), (2030, 1, 7), false),
        ((2030, 1, 3), (2030, 1, 4), false),
        ((2029, 12, 30), (2030, 1, 1), false),
        ((1975, 1, 1), (4096, 1, 1), false),
    ];
    for (from, until, valid) in cases {
        chain.tls = tls_certificate(from, until);
        let outcome = judge(&chain, response(&chain));
        if valid {
            assert!(
                matches!(outcome, Outcome::Verified(_)),
                "{from:?} to {until:?}"
            );
        } else {
            assert_eq!(
                outcome,
                Outcome::Failed(Failure::TlsCertificateWindow),
                "{from:?} to {until:?}"
            );
        }
    }
}

// covers: C125
#[test]
fn the_chain_must_end_at_a_trusted_root() {
    let chain = chain();
    let outcome = auth::judge(
        &reply(response(&chain)),
        &NONCE,
        Some(&chain.tls),
        NOW,
        &auth::ROOTS,
    );
    assert_eq!(outcome, Outcome::Failed(Failure::Chain));
    let no_intermediate = AuthResponse {
        intermediate_certificate: Vec::new(),
        ..response(&chain)
    };
    assert_eq!(
        judge(&chain, no_intermediate),
        Outcome::Failed(Failure::Chain)
    );
}

// covers: C125
#[test]
fn unused_intermediates_are_ignored_and_more_than_eight_fail() {
    let chain = chain();
    let other = sign_der(&ca("Unrelated", None), &ca("Test Root", None));
    let mut extra = response(&chain);
    extra.intermediate_certificate.insert(0, other.clone());
    assert!(matches!(judge(&chain, extra), Outcome::Verified(_)));
    let mut nine = response(&chain);
    nine.intermediate_certificate.extend(vec![other; 8]);
    assert_eq!(
        judge(&chain, nine),
        Outcome::Failed(Failure::TooManyIntermediates)
    );
}

// covers: C125
#[test]
fn a_certificate_that_does_not_parse_fails() {
    let chain = chain();
    let mut garbage = response(&chain);
    garbage
        .intermediate_certificate
        .push(vec![0x30, 0x03, 1, 2, 3]);
    assert_eq!(
        judge(&chain, garbage),
        Outcome::Failed(Failure::CertificateParse)
    );
    let no_device = AuthResponse {
        client_auth_certificate: Some(vec![0x30]),
        ..response(&chain)
    };
    assert_eq!(
        judge(&chain, no_device),
        Outcome::Failed(Failure::CertificateParse)
    );
}

// covers: C125
#[test]
fn issuers_must_be_cas_within_their_path_length() {
    let mut not_ca = named("Test Intermediate");
    not_ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let chain = chain_with(ca("Test Root", None), not_ca, device_params());
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::Chain)
    );

    let chain = chain_with(
        ca("Test Root", Some(0)),
        ca("Test Intermediate", None),
        device_params(),
    );
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::Chain)
    );
    let chain = chain_with(
        ca("Test Root", Some(1)),
        ca("Test Intermediate", Some(0)),
        device_params(),
    );
    assert!(matches!(
        judge(&chain, response(&chain)),
        Outcome::Verified(_)
    ));

    let mut no_sign = ca("Test Intermediate", None);
    no_sign.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let chain = chain_with(ca("Test Root", None), no_sign, device_params());
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::Chain)
    );
}

// covers: C125
#[test]
fn the_device_certificate_needs_the_digital_signature_usage() {
    for usages in [Vec::new(), vec![KeyUsagePurpose::KeyEncipherment]] {
        let mut device = device_params();
        device.key_usages = usages;
        let chain = chain_with(ca("Test Root", None), ca("Test Intermediate", None), device);
        assert_eq!(
            judge(&chain, response(&chain)),
            Outcome::Failed(Failure::Chain)
        );
    }
}

// covers: C125
#[test]
fn the_device_and_intermediates_must_be_valid_now_but_the_root_need_not() {
    let mut old_root = ca("Test Root", None);
    old_root.not_before = rcgen::date_time_ymd(2000, 1, 1);
    old_root.not_after = rcgen::date_time_ymd(2001, 1, 1);
    let chain = chain_with(old_root, ca("Test Intermediate", None), device_params());
    assert!(matches!(
        judge(&chain, response(&chain)),
        Outcome::Verified(_)
    ));

    let mut expired = ca("Test Intermediate", None);
    expired.not_after = rcgen::date_time_ymd(2029, 1, 1);
    let chain = chain_with(ca("Test Root", None), expired, device_params());
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::Chain)
    );

    let mut early = device_params();
    early.not_before = rcgen::date_time_ymd(2031, 1, 1);
    let chain = chain_with(ca("Test Root", None), ca("Test Intermediate", None), early);
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::Chain)
    );
}

// covers: C125
#[test]
fn a_critical_extension_beyond_constraints_and_usage_fails() {
    let mut device = device_params();
    let mut extension = CustomExtension::from_oid_content(&[1, 2, 3, 4], vec![0x05, 0x00]);
    extension.set_criticality(true);
    device.custom_extensions = vec![extension];
    let chain = chain_with(ca("Test Root", None), ca("Test Intermediate", None), device);
    assert_eq!(
        judge(&chain, response(&chain)),
        Outcome::Failed(Failure::CriticalExtension)
    );
}

// covers: C125
#[test]
fn the_audio_only_policy_on_the_path_marks_the_device() {
    let mut intermediate = ca("Test Intermediate", None);
    intermediate.custom_extensions = vec![CustomExtension::from_oid_content(
        &[2, 5, 29, 32],
        AUDIO_ONLY_POLICIES.to_vec(),
    )];
    let chain = chain_with(ca("Test Root", None), intermediate, device_params());
    let Outcome::Verified(verified) = judge(&chain, response(&chain)) else {
        panic!("verifies");
    };
    assert!(verified.audio_only);
}

// covers: C122
#[test]
fn an_error_declines_and_a_reply_without_error_or_response_is_malformed() {
    let chain = chain();
    let declined = DeviceAuthMessage {
        challenge: None,
        response: Some(response(&chain)),
        error: Some(AuthError {
            error_type: Some(1),
        }),
    };
    let outcome = auth::judge(
        &binary(declined.encode_to_vec()),
        &NONCE,
        Some(&chain.tls),
        NOW,
        &[&chain.root],
    );
    assert_eq!(outcome, Outcome::Declined(1));
    let empty = binary(DeviceAuthMessage::default().encode_to_vec());
    let garbage = binary(vec![0xff, 0xff, 0xff]);
    let mut text = reply(response(&chain));
    text.payload_type = PayloadType::String as i32;
    for message in [empty, garbage, text] {
        assert_eq!(
            auth::judge(&message, &NONCE, Some(&chain.tls), NOW, &[&chain.root]),
            Outcome::Failed(Failure::Malformed)
        );
    }
    assert!(auth::is_reply(&reply(response(&chain))));
    let mut elsewhere = reply(response(&chain));
    elsewhere.destination_id = "sender-7".to_owned();
    assert!(!auth::is_reply(&elsewhere));
}

// covers: C127
#[test]
fn the_crl_is_never_parsed_and_only_its_length_is_kept() {
    let chain = chain();
    let reply = AuthResponse {
        crl: Some(vec![0xde, 0xad, 0xbe, 0xef]),
        ..response(&chain)
    };
    let Outcome::Verified(verified) = judge(&chain, reply) else {
        panic!("a CRL that doesn't parse changes nothing");
    };
    assert_eq!(verified.crl, Some(4));
}

// covers: C132
#[test]
fn each_outcome_reads_as_the_log_line_says() {
    let key = KeyPin([0xab; 32]);
    let verified = Outcome::Verified(Verified {
        key,
        sha1: true,
        nonce_echoed: false,
        audio_only: true,
        crl: Some(12),
    });
    assert_eq!(
        verified.to_string(),
        "verified, sha1, nonce missing, policy audio-only, crl 12 bytes, key abababababababab"
    );
    assert_eq!(
        Outcome::Failed(Failure::TlsCertificateWindow).to_string(),
        "failed, TLS certificate window"
    );
    assert_eq!(Outcome::Declined(1).to_string(), "declined, NO_TLS");
    assert_eq!(
        Outcome::NoAnswer(Duration::from_secs(1)).to_string(),
        "no answer in 1 s"
    );
    assert_eq!(key.short(), "abababababababab");
}
