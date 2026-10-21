//! SRP-6a, client side, for the Apple TV PIN pairing (P50) and the HomeKit
//! pairing of the AirPlay 2 path (P151).
//!
//! The group arithmetic follows RFC 5054 §2.5 and §2.6: k, x, u, A and the
//! premaster secret S, with the hash the group names (SHA-1 for the legacy
//! groups, SHA-512 for HomeKit's 3072-bit one). Modular exponentiation and
//! the other big integer operations come from `crypto-bigint` (constant time
//! in the secret values; the exponent's bit bound is public), hashing from
//! `sha1` and `sha2`. Every value is held in a 4096-bit integer, so each
//! RFC 5054 group, the 1024-bit one of the RFC's own test vector (appendix
//! B) included, runs through the same code.
//!
//! The legacy pairing's two departures from common SRP libraries (P50) live
//! in [`session_key`] and [`client_proof`]: K is 40 bytes, H(S | 00 00 00 00)
//! | H(S | 00 00 00 01), and M1 hashes g without padding. HomeKit keeps the
//! unpadded g and takes K = H(S) ([`hashed_session_key`]). In M1, M2 and K
//! the values A, B and S are hashed in their shortest big-endian form,
//! without leading zero bytes; only k and u pad them to the length of N.

use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U4096};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use zeroize::{Zeroize, Zeroizing};

/// Bytes of a SHA-1 output, the hash of the legacy pairing.
pub const HASH_BYTES: usize = 20;
/// Bytes of a SHA-512 output, the hash of the HomeKit pairing.
pub const SHA512_BYTES: usize = 64;
/// Bytes of the widest group this module handles.
const MAX_BYTES: usize = 512;
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

/// The 3072-bit prime of RFC 5054 appendix A.
const PRIME_3072: [u8; 384] = decode_hex(
    b"FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1 29024E08 \
      8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD EF9519B3 CD3A431B \
      302B0A6D F25F1437 4FE1356D 6D51C245 E485B576 625E7EC6 F44C42E9 \
      A637ED6B 0BFF5CB6 F406B7ED EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 \
      49286651 ECE45B3D C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8 \
      FD24CF5F 83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D \
      670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B E39E772C \
      180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9 DE2BCBF6 95581718 \
      3995497C EA956AE5 15D22618 98FA0510 15728E5A 8AAAC42D AD33170D \
      04507A33 A85521AB DF1CBA64 ECFB8504 58DBEF0A 8AEA7157 5D060C7D \
      B3970F85 A6E1E4C7 ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226 \
      1AD2EE6B F12FFA06 D98A0864 D8760273 3EC86A64 521F2B18 177B200C \
      BBE11757 7A615D6C 770988C0 BAD946E2 08E24FA0 74E5AB31 43DB5BFC \
      E0FD108E 4B82D120 A93AD2CA FFFFFFFF FFFFFFFF",
);

/// The hash an SRP group is used with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrpHash {
    Sha1,
    Sha512,
}

/// An SRP group: a safe prime N and a generator g (RFC 5054 appendix A),
/// with the hash H its pairing uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrpGroup {
    prime: &'static [u8],
    generator: u8,
    hash: SrpHash,
}

impl SrpGroup {
    /// The 1024-bit group, generator 2, with SHA-1: the group of RFC 5054's
    /// test vector.
    pub const RFC5054_1024: SrpGroup = SrpGroup {
        prime: &PRIME_1024,
        generator: 2,
        hash: SrpHash::Sha1,
    };
    /// The 2048-bit group, generator 2, with SHA-1: the group of the Apple
    /// TV pairing (P50).
    pub const RFC5054_2048: SrpGroup = SrpGroup {
        prime: &PRIME_2048,
        generator: 2,
        hash: SrpHash::Sha1,
    };
    /// The 3072-bit group, generator 5, with SHA-512: the group of the
    /// HomeKit pairing (P151).
    pub const RFC5054_3072: SrpGroup = SrpGroup {
        prime: &PRIME_3072,
        generator: 5,
        hash: SrpHash::Sha512,
    };

    /// N, big-endian, without leading zero bytes.
    pub fn prime(&self) -> &'static [u8] {
        self.prime
    }

    pub fn generator(&self) -> u8 {
        self.generator
    }

    pub fn hash(&self) -> SrpHash {
        self.hash
    }

    /// H(parts...), with the group's hash.
    pub fn digest(&self, parts: &[&[u8]]) -> Vec<u8> {
        match self.hash {
            SrpHash::Sha1 => sha1(parts).to_vec(),
            SrpHash::Sha512 => sha512(parts).to_vec(),
        }
    }

    /// Bytes of the group's hash output.
    pub fn hash_len(&self) -> usize {
        match self.hash {
            SrpHash::Sha1 => HASH_BYTES,
            SrpHash::Sha512 => SHA512_BYTES,
        }
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
        // Sized once, so a secret value is never left behind in a buffer a
        // reallocation gave up.
        let mut out = Vec::with_capacity(width);
        out.resize(width - value.len(), 0);
        out.extend_from_slice(value);
        Some(out)
    }

    fn params(&self) -> FixedMontyParams<{ U4096::LIMBS }> {
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
/// §2.6). k, x and u are hash outputs; A and S are padded to the length of
/// N.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientValues {
    pub k: Vec<u8>,
    pub x: Vec<u8>,
    pub u: Vec<u8>,
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

/// A big-endian value of at most 512 bytes in a 4096-bit integer.
fn widen(value: &[u8]) -> U4096 {
    let value = trim_leading_zeros(value);
    let mut bytes = [0u8; MAX_BYTES];
    bytes[MAX_BYTES - value.len()..].copy_from_slice(value);
    let wide = U4096::from_be_slice(&bytes);
    // The private value passes through here too.
    bytes.zeroize();
    wide
}

fn sha1(parts: &[&[u8]]) -> [u8; HASH_BYTES] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn sha512(parts: &[&[u8]]) -> [u8; SHA512_BYTES] {
    let mut hasher = Sha512::new();
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
    let generator = FixedMontyForm::new(&U4096::from_u8(group.generator), &params);
    let hash_bits = 8 * group.hash_len() as u32;

    // Every value derived from the password or a is wiped when dropped.
    let k = group.digest(&[group.prime, &pad(&[group.generator])?]);
    let inner = Zeroizing::new(group.digest(&[identity, b":", password]));
    let mut x = group.digest(&[salt, &inner[..]]);

    let a = Zeroizing::new(widen(private));
    let private_bits = 8 * private.len() as u32;
    let public_int = generator.pow_bounded_exp(&a, private_bits).retrieve();
    let public = pad(public_int.to_be_bytes().as_ref())?;
    let b_padded = pad(b_pub)?;
    let u = group.digest(&[&public, &b_padded]);
    if u.iter().all(|&byte| byte == 0) {
        x.zeroize();
        return Err(SrpError::ZeroScrambler);
    }

    let x_int = Zeroizing::new(widen(&x));
    let u_int = widen(&u);
    let k_form = FixedMontyForm::new(&widen(&k), &params);
    let b_form = FixedMontyForm::new(&widen(b_pub), &params);
    let g_x = Zeroizing::new(generator.pow_bounded_exp(&x_int, hash_bits));
    let base = Zeroizing::new(b_form.sub(&k_form.mul(&g_x)));
    // a + u·x: u and x are hash outputs of at most 512 bits each, so the sum
    // has at most max(8·|a|, 1024) + 1 bits, far below 4096.
    let exponent = Zeroizing::new(a.wrapping_add(&u_int.wrapping_mul(&x_int)));
    let exponent_bits = private_bits.max(2 * hash_bits) + 1;
    let premaster_int = Zeroizing::new(base.pow_bounded_exp(&exponent, exponent_bits).retrieve());
    let mut encoded = premaster_int.to_be_bytes();
    let premaster = pad(encoded.as_ref());
    encoded.as_mut().zeroize();
    let premaster = match premaster {
        Ok(premaster) => premaster,
        Err(error) => {
            x.zeroize();
            return Err(error);
        }
    };
    Ok(ClientValues {
        k,
        x,
        u,
        public,
        premaster,
    })
}

/// The pairing's session key (P50): K = H(S | 00 00 00 00) |
/// H(S | 00 00 00 01), 40 bytes, with S in its shortest big-endian form
/// (leading zero bytes dropped, so a padded S gives the same K).
pub fn session_key(premaster: &[u8]) -> [u8; 2 * HASH_BYTES] {
    let premaster = trim_leading_zeros(premaster);
    let mut key = [0u8; 2 * HASH_BYTES];
    key[..HASH_BYTES].copy_from_slice(&sha1(&[premaster, &[0, 0, 0, 0]]));
    key[HASH_BYTES..].copy_from_slice(&sha1(&[premaster, &[0, 0, 0, 1]]));
    key
}

/// HomeKit's session key (P151): K = H(S), with S in its shortest
/// big-endian form; 64 bytes with SHA-512.
pub fn hashed_session_key(group: &SrpGroup, premaster: &[u8]) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(group.digest(&[trim_leading_zeros(premaster)]))
}

/// The client's proof (P50, P151): M1 = H(H(N) XOR H(g) | H(I) | s | A | B |
/// K), with g hashed as its single byte, A and B in their shortest
/// big-endian form (leading zero bytes dropped, so padded values give the
/// same M1) and s as received.
pub fn client_proof(
    group: &SrpGroup,
    identity: &[u8],
    salt: &[u8],
    client_public: &[u8],
    server_public: &[u8],
    key: &[u8],
) -> Vec<u8> {
    let hash_n = group.digest(&[group.prime]);
    let hash_g = group.digest(&[&[group.generator]]);
    let mixed: Vec<u8> = hash_n.iter().zip(&hash_g).map(|(n, g)| n ^ g).collect();
    group.digest(&[
        &mixed,
        &group.digest(&[identity]),
        salt,
        trim_leading_zeros(client_public),
        trim_leading_zeros(server_public),
        key,
    ])
}

/// The server's proof as RFC 2945 §3 defines it, M2 = H(A | M1 | K), with A
/// in its shortest big-endian form (P52, P151).
pub fn server_proof(
    group: &SrpGroup,
    client_public: &[u8],
    client_proof: &[u8],
    key: &[u8],
) -> Vec<u8> {
    group.digest(&[trim_leading_zeros(client_public), client_proof, key])
}

/// The length of a big-endian value without its leading zero bytes.
pub fn significant_len(value: &[u8]) -> usize {
    trim_leading_zeros(value).len()
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
        assert_eq!(SrpGroup::RFC5054_3072.len(), 384);
        assert_eq!(SrpGroup::RFC5054_3072.hash_len(), 64);
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
