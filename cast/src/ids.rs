//! Request ids (C29).

/// Hands out the `requestId`s of one channel: a random positive start, then
/// one more per request, never 0.
#[derive(Debug, Clone)]
pub struct RequestIds {
    next: u32,
}

/// Starts stay below 2^30, so more than a billion requests fit below 2^31
/// before a wrap: receivers parse the id as a JSON number, and 31 bits is
/// the range every reader holds as an integer.
const START_LIMIT: u32 = 1 << 30;
const WRAP: u32 = i32::MAX as u32;

impl RequestIds {
    /// A random start, from the operating system's generator.
    pub fn random() -> RequestIds {
        let mut bytes = [0u8; 4];
        // Without randomness the ids still work; only their start is fixed.
        let _ = getrandom::fill(&mut bytes);
        RequestIds::starting_at(u32::from_le_bytes(bytes) % START_LIMIT)
    }

    /// A fixed start, for tests. 0 becomes 1.
    pub fn starting_at(start: u32) -> RequestIds {
        RequestIds {
            next: start.clamp(1, WRAP),
        }
    }

    pub fn next_id(&mut self) -> u32 {
        let id = self.next;
        self.next = if id >= WRAP { 1 } else { id + 1 };
        id
    }
}
