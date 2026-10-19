//! SRP-6a for the Apple TV PIN pairing (P50), client side.
//!
//! The group arithmetic follows RFC 5054 §2.5 and §2.6 with SHA-1: k, x, u,
//! A and the premaster secret S. Modular exponentiation and the other big
//! integer operations come from `crypto-bigint` (constant time in the
//! secret values; the exponent's bit bound is public), hashing from `sha1`.
//! Every value is held in a 2048-bit integer, so the RFC 5054 1024-bit group
//! of the RFC's own test vector (appendix B) runs through the same code.
//!
//! The pairing's two departures from common SRP libraries (P50) live in
//! [`session_key`] and [`client_proof`]: K is 40 bytes, H(S | 00 00 00 00) |
//! H(S | 00 00 00 01), and M1 hashes g without padding.

use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U2048};
use sha1::{Digest, Sha1};

/// Bytes of a SHA-1 output.
pub const HASH_BYTES: usize = 20;
/// Bytes of the widest group this module handles.
const MAX_BYTES: usize = 256;
/// The longest private value accepted, in bytes.
const MAX_PRIVATE_BYTES: usize = 64;

/// Decodes hex digits at compile time, skipping spaces.
const fn decode_hex<const N: usize>(hex: &[u8]) -> [u8; N] {
    const fn nibble(digit: u8) -> u8 {
        match digit {
            b'0'..=b'9' => digit - b'0',
            b'A'..=b'F' => digit - b'A' + 10,
            b'a'..=b'f' => digit - b'a' + 10,
            _ => panic!("not a hex digit"),
        }
    }
    let mut out = [0u8; N];
    let mut written = 0;
    let mut high: Option<u8> = None;
    let mut i = 0;
    while i < hex.len() {
        let digit = hex[i];
        i += 1;
        if digit == b' ' {
            continue;
        }
        let value = nibble(digit);
        match high {
            None => high = Some(value),
            Some(first) => {
                out[written] = (first << 4) | value;
                written += 1;
                high = None;
            }
        }
    }
    assert!(written == N && high.is_none(), "wrong number of hex digits");
    out
}

/// The 1024-bit prime of RFC 5054 appendix A.
const PRIME_1024: [u8; 128] = decode_hex(
    b"EEAF0AB9 ADB38DD6 9C33F80A FA8FC5E8 60726187 75FF3C0B 9EA2314C \
      9C256576 D674DF74 96EA81D3 383B4813 D692C6E0 E0D5D8E2 50B98BE4 \
      8E495C1D 6089DAD1 5DC7D7B4 6154D6B6 CE8EF4AD 69B15D49 82559B29 \
      7BCF1885 C529F566 660E57EC 68EDBC3C 05726CC0 2FD4CBF4 976EAA9A \
      FD5138FE 8376435B 9FC61D2F C0EB06E3",
);

/// The 2048-bit prime of RFC 5054 appendix A.
const PRIME_2048: [u8; 256] = decode_hex(
    b"AC6BDB41 324A9A9B F166DE5E 1389582F AF72B665 1987EE07 FC319294 \
      3DB56050 A37329CB B4A099ED 8193E075 7767A13D D52312AB 4B03310D \
      CD7F48A9 DA04FD50 E8083969 EDB767B0 CF609517 9A163AB3 661A05FB \
      D5FAAAE8 2918A996 2F0B93B8 55F97993 EC975EEA A80D740A DBF4FF74 \
      7359D041 D5C33EA7 1D281E44 6B14773B CA97B43A 23FB8016 76BD207A \
      436C6481 F1D2B907 8717461A 5B9D32E6 88F87748 544523B5 24B0D57D \
      5EA77A27 75D2ECFA 032CFBDB F52FB378 61602790 04E57AE6 AF874E73 \
      03CE5329 9CCC041C 7BC308D8 2A5698F3 A8D0C382 71AE35F8 E9DBFBB6 \
      94B5C803 D89F7AE4 35DE236D 525F5475 9B65E372 FCD68EF2 0FA7111F \
      9E4AFF73",
);

/// An SRP group: a safe prime N and a generator g (RFC 5054 appendix A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrpGroup {
    prime: &'static [u8],
    generator: u8,
}

impl SrpGroup {
    /// The 1024-bit group, generator 2: the group of RFC 5054's test vector.
    pub const RFC5054_1024: SrpGroup = SrpGroup {
        prime: &PRIME_1024,
        generator: 2,
    };
    /// The 2048-bit group, generator 2: the group of the Apple TV pairing
    /// (P50).
    pub const RFC5054_2048: SrpGroup = SrpGroup {
        prime: &PRIME_2048,
        generator: 2,
    };

    /// N, big-endian, without leading zero bytes.
    pub fn prime(&self) -> &'static [u8] {
        self.prime
    }

    pub fn generator(&self) -> u8 {
        self.generator
    }

    /// The length of N in bytes: the width PAD widens values to.
    pub fn len(&self) -> usize {
        self.prime.len()
    }

    /// Always false: a group has a prime.
    pub fn is_empty(&self) -> bool {
        self.prime.is_empty()
    }

    /// PAD(value): `value` (big-endian, at most as long as N once leading
    /// zeros are dropped) widened to the length of N.
    pub fn pad(&self, value: &[u8]) -> Option<Vec<u8>> {
        let value = trim_leading_zeros(value);
        let width = self.len();
        if value.len() > width {
            return None;
        }
        let mut out = vec![0u8; width - value.len()];
        out.extend_from_slice(value);
        Some(out)
    }

    fn params(&self) -> FixedMontyParams<{ U2048::LIMBS }> {
        // Both primes are odd, and a group is only made from those
        // constants, so this never fails.
        let modulus = Odd::new(widen(self.prime)).expect("an odd prime");
        FixedMontyParams::new_vartime(modulus)
    }
}

/// Why the client values can't be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrpError {
    /// B is empty, longer than N, or a multiple of N (RFC 5054 §2.5.4).
    InvalidServerValue,
    /// The private value is empty or longer than 64 bytes.
    InvalidPrivateValue,
    /// u came out zero (RFC 2945 §3): the client draws a new private value.
    ZeroScrambler,
}

/// What the client computes from the server's salt and B (RFC 5054 §2.5,
/// §2.6). A and S are padded to the length of N.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientValues {
    pub k: [u8; HASH_BYTES],
    pub x: [u8; HASH_BYTES],
    pub u: [u8; HASH_BYTES],
    /// PAD(A), A = g^a mod N.
    pub public: Vec<u8>,
    /// PAD(S), S = (B − k·g^x)^(a + u·x) mod N: the premaster secret.
    pub premaster: Vec<u8>,
}

impl std::fmt::Debug for ClientValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // x and S are secrets.
        f.debug_struct("ClientValues")
            .field("u", &self.u)
            .field("public", &self.public.len())
            .finish_non_exhaustive()
    }
}

impl Drop for ClientValues {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.x.zeroize();
        self.premaster.zeroize();
    }
}

fn trim_leading_zeros(value: &[u8]) -> &[u8] {
    let first = value
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(value.len());
    &value[first..]
}

/// A big-endian value of at most 256 bytes in a 2048-bit integer.
fn widen(value: &[u8]) -> U2048 {
    let value = trim_leading_zeros(value);
    let mut bytes = [0u8; MAX_BYTES];
    bytes[MAX_BYTES - value.len()..].copy_from_slice(value);
    U2048::from_be_slice(&bytes)
}

fn sha1(parts: &[&[u8]]) -> [u8; HASH_BYTES] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Checks the server's B as the client must before using it: not empty,
/// not longer than N, and B mod N not zero (RFC 5054 §2.5.4).
pub fn check_server_value(group: &SrpGroup, b_pub: &[u8]) -> Result<(), SrpError> {
    let trimmed = trim_leading_zeros(b_pub);
    if b_pub.is_empty() || trimmed.len() > group.len() {
        return Err(SrpError::InvalidServerValue);
    }
    let reduced = FixedMontyForm::new(&widen(trimmed), &group.params()).retrieve();
    if reduced.is_zero_vartime() {
        return Err(SrpError::InvalidServerValue);
    }
    Ok(())
}

/// The client's values for identity I, password P, salt s, the server's B
/// and the private value a (big-endian), as RFC 5054 defines them:
/// k = H(N | PAD(g)), x = H(s | H(I | ":" | P)), u = H(PAD(A) | PAD(B)),
/// A = g^a mod N and S = (B − k·g^x)^(a + u·x) mod N.
pub fn client_values(
    group: &SrpGroup,
    identity: &[u8],
    password: &[u8],
    salt: &[u8],
    b_pub: &[u8],
    private: &[u8],
) -> Result<ClientValues, SrpError> {
    check_server_value(group, b_pub)?;
    if private.is_empty() || private.len() > MAX_PRIVATE_BYTES {
        return Err(SrpError::InvalidPrivateValue);
    }
    let params = group.params();
    let pad = |value: &[u8]| group.pad(value).ok_or(SrpError::InvalidServerValue);
    let generator = FixedMontyForm::new(&U2048::from_u8(group.generator), &params);

    let k = sha1(&[group.prime, &pad(&[group.generator])?]);
    let inner = sha1(&[identity, b":", password]);
    let x = sha1(&[salt, &inner]);

    let a = widen(private);
    let private_bits = 8 * private.len() as u32;
    let public_int = generator.pow_bounded_exp(&a, private_bits).retrieve();
    let public = pad(public_int.to_be_bytes().as_ref())?;
    let b_padded = pad(b_pub)?;
    let u = sha1(&[&public, &b_padded]);
    if u.iter().all(|&byte| byte == 0) {
        return Err(SrpError::ZeroScrambler);
    }

    let x_int = widen(&x);
    let u_int = widen(&u);
    let k_form = FixedMontyForm::new(&widen(&k), &params);
    let b_form = FixedMontyForm::new(&widen(b_pub), &params);
    let g_x = generator.pow_bounded_exp(&x_int, 8 * HASH_BYTES as u32);
    let base = b_form.sub(&k_form.mul(&g_x));
    // a + u·x: u and x have 160 bits each, so the sum has at most
    // max(8·|a|, 320) + 1 bits, far below 2048.
    let exponent = a.wrapping_add(&u_int.wrapping_mul(&x_int));
    let exponent_bits = private_bits.max(2 * 8 * HASH_BYTES as u32) + 1;
    let premaster_int = base.pow_bounded_exp(&exponent, exponent_bits).retrieve();
    let premaster = pad(premaster_int.to_be_bytes().as_ref())?;
    Ok(ClientValues {
        k,
        x,
        u,
        public,
        premaster,
    })
}

/// The pairing's session key (P50): K = H(S | 00 00 00 00) |
/// H(S | 00 00 00 01), 40 bytes, with S as given (padded to N).
pub fn session_key(premaster: &[u8]) -> [u8; 2 * HASH_BYTES] {
    let mut key = [0u8; 2 * HASH_BYTES];
    key[..HASH_BYTES].copy_from_slice(&sha1(&[premaster, &[0, 0, 0, 0]]));
    key[HASH_BYTES..].copy_from_slice(&sha1(&[premaster, &[0, 0, 0, 1]]));
    key
}

/// The client's proof (P50): M1 = H(H(N) XOR H(g) | H(I) | s | PAD(A) |
/// PAD(B) | K), with g hashed as its single byte, not padded.
pub fn client_proof(
    group: &SrpGroup,
    identity: &[u8],
    salt: &[u8],
    a_padded: &[u8],
    b_padded: &[u8],
    key: &[u8],
) -> [u8; HASH_BYTES] {
    let hash_n = sha1(&[group.prime]);
    let hash_g = sha1(&[&[group.generator]]);
    let mut mixed = [0u8; HASH_BYTES];
    for (out, (n, g)) in mixed.iter_mut().zip(hash_n.iter().zip(hash_g.iter())) {
        *out = n ^ g;
    }
    sha1(&[&mixed, &sha1(&[identity]), salt, a_padded, b_padded, key])
}

/// The server's proof as RFC 2945 §3 defines it, M2 = H(A | M1 | K), with A
/// padded to N (P52).
pub fn server_proof(a_padded: &[u8], client_proof: &[u8], key: &[u8]) -> [u8; HASH_BYTES] {
    sha1(&[a_padded, client_proof, key])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_primes_are_the_rfc_lengths_without_leading_zeros() {
        assert_eq!(SrpGroup::RFC5054_1024.len(), 128);
        assert_eq!(SrpGroup::RFC5054_2048.len(), 256);
        assert_ne!(PRIME_1024[0], 0);
        assert_ne!(PRIME_2048[0], 0);
        assert_eq!(PRIME_2048[255], 0x73);
    }

    #[test]
    fn b_must_not_be_a_multiple_of_n() {
        let group = SrpGroup::RFC5054_1024;
        assert_eq!(
            check_server_value(&group, group.prime()),
            Err(SrpError::InvalidServerValue)
        );
        assert_eq!(
            check_server_value(&group, &[0, 0]),
            Err(SrpError::InvalidServerValue)
        );
        assert_eq!(
            check_server_value(&group, &[1; 129]),
            Err(SrpError::InvalidServerValue)
        );
        assert_eq!(check_server_value(&group, &[1]), Ok(()));
    }
}
