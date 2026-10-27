import Foundation

/// How an AirPlay speaker controls access, and why AudioSync switched one
/// off. AudioSync's own sender decides both (`NativeReceiver.access`,
/// `NativeAirPlayModel`); the texts for them are in `UIStrings`.
enum AirPlayPairing {

    /// How a speaker controls access, from what discovery reports.
    enum Access: Equatable {
        /// Nothing AudioSync can answer: it just couldn't connect.
        case open
        /// Wants a PIN shown on its screen, or a password; AudioSync asks for
        /// it when the user turns the speaker on.
        case code(password: Bool)
    }

    /// Why a speaker was switched off, for the message.
    enum Reason: Equatable {
        case unreachable
        case needsPin           // asked for a PIN after an automatic turn-on
        case needsPassword      // asked for a password after an automatic turn-on
        case cancelled          // the user cancelled the prompt
        case stillNeedsCode     // paired, but asked for a code again
    }
}
