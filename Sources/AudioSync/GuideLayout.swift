/// Where the Guide's illustrations go in its text.
enum GuideLayout {
    struct Piece: Equatable {
        let text: String
        /// The illustration shown after this piece of text, if any.
        let art: GuideArt?
    }

    /// The text cut into pieces, in order: each illustration closes the piece before the
    /// first line that starts with its anchor (leading spaces ignored). An illustration
    /// whose anchor starts no line is left out; the pieces always join back into `text`.
    static func pieces(of text: String, anchor: (GuideArt) -> String) -> [Piece] {
        var pieces: [Piece] = []
        var waiting = GuideArt.allCases
        var current = ""
        let lines = text.components(separatedBy: "\n")
        for (index, line) in lines.enumerated() {
            let start = line.drop { $0 == " " }
            if !current.isEmpty, let found = waiting.firstIndex(where: { start.hasPrefix(anchor($0)) }) {
                pieces.append(Piece(text: current, art: waiting.remove(at: found)))
                current = ""
            }
            current += index < lines.count - 1 ? line + "\n" : line
        }
        pieces.append(Piece(text: current, art: nil))
        return pieces
    }
}
