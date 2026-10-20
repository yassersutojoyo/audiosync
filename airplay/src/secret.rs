//! Text the host hands over that must not outlive its use: a speaker's
//! password (P39) or the PIN of a pairing (P50).
//!
//! A [`Secret`] owns its buffer, wipes it when dropped (spare capacity
//! included) and never prints it: `Debug` shows `Secret(<hidden>)`. Its text
//! is reachable only through [`Secret::expose`], so every read is easy to
//! find. Each clone owns and wipes a buffer of its own.

use std::fmt;

use zeroize::{ZeroizeOnDrop, Zeroizing};

/// A password or PIN, wiped from memory when dropped and hidden from
/// `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Takes `text` over without copying it, so no unwiped copy is left
    /// behind.
    pub fn new(text: String) -> Secret {
        Secret(Zeroizing::new(text))
    }

    /// The text, for the computation that needs it. Copying it into a
    /// plain `String` would leave that copy unwiped; don't.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(text: String) -> Secret {
        Secret::new(text)
    }
}

/// Copies `text` into a buffer the new `Secret` wipes; the caller's own
/// copy stays the caller's to wipe.
impl From<&str> for Secret {
    fn from(text: &str) -> Secret {
        Secret::new(text.to_owned())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}

/// The one field is a `Zeroizing<String>`, which wipes itself when dropped.
impl ZeroizeOnDrop for Secret {}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiped_on_drop<T: ZeroizeOnDrop>() {}

    #[test]
    fn a_secret_is_wiped_on_drop_and_debug_never_prints_it() {
        wiped_on_drop::<Secret>();
        let secret = Secret::from("hunter2");
        assert_eq!(secret.expose(), "hunter2");
        let shown = format!("{secret:?} {:?}", Some(secret.clone()));
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "Secret(<hidden>) Some(Secret(<hidden>))");
    }

    #[test]
    fn every_password_the_crate_keeps_is_a_secret_and_never_printed() {
        use crate::sender::SpeakerOptions;
        use crate::session::SessionConfig;
        use crate::volume::Volume;

        let password = Some(Secret::from("hunter2"));
        let mut options = SpeakerOptions::new(Volume::Db(-20.0));
        options.password = password.clone();
        let mut config = SessionConfig::new(Vec::new(), Volume::Db(-20.0));
        config.password = password;
        let shown = format!("{options:?} {config:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn a_secret_takes_its_string_over_without_a_copy() {
        let text = String::from("1234");
        let buffer = text.as_ptr();
        let secret = Secret::from(text);
        assert_eq!(secret.expose().as_ptr(), buffer, "the same buffer");
        assert_eq!(secret, Secret::from("1234"));
        assert_ne!(secret, Secret::from("4321"));
    }
}
