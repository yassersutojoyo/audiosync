//! TLV8, the encoding of the HomeKit pairing bodies (P62, P150): a type
//! byte, a length byte and the value, with values over 255 bytes split
//! into consecutive items of the same type.

use zeroize::Zeroize;

pub const METHOD: u8 = 0;
pub const IDENTIFIER: u8 = 1;
pub const SALT: u8 = 2;
pub const PUBLIC_KEY: u8 = 3;
pub const PROOF: u8 = 4;
pub const ENCRYPTED_DATA: u8 = 5;
pub const STATE: u8 = 6;
pub const ERROR: u8 = 7;
pub const BACK_OFF: u8 = 8;
pub const SIGNATURE: u8 = 10;
pub const NAME: u8 = 17;
pub const FLAGS: u8 = 19;

/// The longest value one item holds.
const MAX_ITEM: usize = 255;

/// Decoded items in their order, consecutive fragments joined. Values are
/// wiped when dropped, since some carry keys or proofs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Tlv8 {
    items: Vec<(u8, Vec<u8>)>,
}

impl std::fmt::Debug for Tlv8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shape: Vec<(u8, usize)> = self.items.iter().map(|(t, v)| (*t, v.len())).collect();
        f.debug_tuple("Tlv8").field(&shape).finish()
    }
}

impl Drop for Tlv8 {
    fn drop(&mut self) {
        for (_, value) in &mut self.items {
            value.zeroize();
        }
    }
}

/// A body that ends inside an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncated;

impl Tlv8 {
    pub fn new() -> Tlv8 {
        Tlv8::default()
    }

    /// Adds an item; chainable.
    pub fn with(mut self, kind: u8, value: &[u8]) -> Tlv8 {
        self.push(kind, value);
        self
    }

    pub fn push(&mut self, kind: u8, value: &[u8]) {
        self.items.push((kind, value.to_vec()));
    }

    /// The first value of this type.
    pub fn get(&self, kind: u8) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(t, _)| *t == kind)
            .map(|(_, value)| value.as_slice())
    }

    /// The first value of this type when it is exactly one byte.
    pub fn byte(&self, kind: u8) -> Option<u8> {
        match self.get(kind)? {
            [value] => Some(*value),
            _ => None,
        }
    }

    pub fn items(&self) -> impl Iterator<Item = (u8, &[u8])> {
        self.items.iter().map(|(t, value)| (*t, value.as_slice()))
    }

    /// The bytes: each value in items of at most 255 bytes, an empty value
    /// as one item of length 0.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (kind, value) in &self.items {
            if value.is_empty() {
                out.extend_from_slice(&[*kind, 0]);
            }
            for chunk in value.chunks(MAX_ITEM) {
                out.push(*kind);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out
    }

    /// Reads items, joining an item to the one before it when that one has
    /// the same type and is 255 bytes long (a split value).
    pub fn decode(bytes: &[u8]) -> Result<Tlv8, Truncated> {
        // Built in place, so what was read is wiped on an early return too.
        let mut tlv = Tlv8::new();
        let mut last_full = false;
        let mut rest = bytes;
        while let [kind, len, tail @ ..] = rest {
            let len = usize::from(*len);
            let value = tail.get(..len).ok_or(Truncated)?;
            match tlv.items.last_mut() {
                Some((previous, joined)) if last_full && *previous == *kind => {
                    joined.extend_from_slice(value);
                }
                _ => tlv.items.push((*kind, value.to_vec())),
            }
            last_full = len == MAX_ITEM;
            rest = &tail[len..];
        }
        if !rest.is_empty() {
            return Err(Truncated);
        }
        Ok(tlv)
    }
}
