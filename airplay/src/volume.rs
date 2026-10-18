//! Speaker volume as AirPlay 1 carries it (P116, P117, P118, P122).

/// Lowest audible level the sender sends, in dB (P116).
pub const MIN_DB: f64 = -30.0;
/// Highest level, in dB (P116).
pub const MAX_DB: f64 = 0.0;
/// The value that means muted (P116).
pub const MUTED_DB: f64 = -144.0;

/// A speaker volume: a level from −30 to 0 dB, or muted (P118).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Volume {
    Muted,
    /// Always within `MIN_DB..=MAX_DB`; build it with [`Volume::from_db`].
    Db(f64),
}

impl Volume {
    /// A level from the host. Values outside −30 to 0 are clamped (infinite
    /// ones too); NaN is ignored, which gives `None` (P118).
    pub fn from_db(db: f64) -> Option<Volume> {
        if db.is_nan() {
            return None;
        }
        Some(Volume::Db(db.clamp(MIN_DB, MAX_DB)))
    }

    /// The number on the wire: −144 when muted (P116).
    pub fn wire_db(self) -> f64 {
        match self {
            Volume::Muted => MUTED_DB,
            Volume::Db(db) => db.clamp(MIN_DB, MAX_DB),
        }
    }

    /// The number in fixed point with six decimals and a period, whatever
    /// the system locale, never with an exponent (P117). Rust's formatting
    /// doesn't follow the locale. A value that rounds to zero is written
    /// without a minus sign.
    pub fn wire_text(self) -> String {
        let mut db = (self.wire_db() * 1e6).round() / 1e6;
        if db == 0.0 {
            db = 0.0;
        }
        format!("{db:.6}")
    }

    /// The `text/parameters` body of the volume SET_PARAMETER:
    /// `volume: <dB>` and CRLF (P116).
    pub fn parameter_body(self) -> String {
        format!("volume: {}\r\n", self.wire_text())
    }
}

/// The `text/parameters` body of the volume GET_PARAMETER (P122): the
/// parameter name on its own line.
pub const READ_BODY: &str = "volume\r\n";

/// The level in a GET_PARAMETER answer, `volume: <dB>` (P122). `None` when the
/// body has no finite `volume` value.
pub fn parse_reported(body: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(body).ok()?;
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("volume") {
            return None;
        }
        let db: f64 = value.trim().parse().ok()?;
        db.is_finite().then_some(db)
    })
}
