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

extension GuideLayout {
    /// One paragraph of the Guide, its source lines joined so the window wraps it at any width.
    struct Paragraph: Equatable {
        let text: String
        /// The list marker, such as "1. " or "- ", that the wrapped lines indent past; empty outside a list.
        let hang: String
    }

    /// The text's paragraphs, in order, with blank lines kept as empty ones. A line joins the
    /// paragraph above it unless it starts a list item, sits no deeper than the item above it,
    /// or follows a heading: a line whose words all start with a capital and that ends without punctuation.
    static func paragraphs(of text: String) -> [Paragraph] {
        var paragraphs: [Paragraph] = []
        var itemIndent: Int?
        var joinsNext = false
        for line in text.components(separatedBy: "\n") {
            let indent = line.prefix { $0 == " " }.count
            let body = String(line.dropFirst(indent))
            if body.isEmpty {
                paragraphs.append(Paragraph(text: "", hang: ""))
                itemIndent = nil
                joinsNext = false
            } else if let marker = listMarker(body) {
                paragraphs.append(Paragraph(text: body, hang: marker))
                itemIndent = indent
                joinsNext = true
            } else if joinsNext, itemIndent.map({ indent > $0 }) ?? true, let last = paragraphs.popLast() {
                paragraphs.append(Paragraph(text: last.text + " " + body, hang: last.hang))
            } else {
                paragraphs.append(Paragraph(text: body, hang: ""))
                itemIndent = nil
                joinsNext = !isHeading(body)
            }
        }
        return paragraphs
    }

    enum Role: Equatable {
        case title, heading, body
    }

    /// How the window sets each paragraph: the text's first one is the title,
    /// and a heading line that opens a block is a heading. One right under
    /// another heading-like line, such as the byline under the title, stays
    /// body text. `opensText` false: the paragraphs go on from an earlier
    /// piece, after an illustration, which opens a block like a blank line.
    static func roles(of paragraphs: [Paragraph], opensText: Bool = true) -> [Role] {
        var roles: [Role] = []
        var previous: Paragraph?
        for paragraph in paragraphs {
            let headingLike = !paragraph.text.isEmpty && paragraph.hang.isEmpty && isHeading(paragraph.text)
            if roles.isEmpty {
                roles.append(headingLike ? (opensText ? .title : .heading) : .body)
            } else {
                roles.append(headingLike && previous?.text.isEmpty == true ? .heading : .body)
            }
            previous = paragraph
        }
        return roles
    }

    private static func listMarker(_ line: String) -> String? {
        if line.hasPrefix("- ") { return "- " }
        let digits = line.prefix { $0.isASCII && $0.isNumber }
        guard !digits.isEmpty, line.dropFirst(digits.count).hasPrefix(". ") else { return nil }
        return digits + ". "
    }

    private static func isHeading(_ line: String) -> Bool {
        guard let last = line.last, !last.isPunctuation else { return false }
        return line.split(separator: " ").allSatisfy { word in
            word.first.map { !$0.isLetter || $0.isUppercase } ?? true
        }
    }
}
