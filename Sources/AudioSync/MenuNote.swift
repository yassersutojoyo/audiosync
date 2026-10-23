import AppKit

/// A row of small wrapping text under the menu item above it. A view row,
/// because neither `NSMenuItem.subtitle` set in `validateMenuItem` nor a
/// SwiftUI menu toggle's second `Text` showed on macOS 15 and later.
@MainActor
final class MenuNoteItem: NSMenuItem {
    /// Lines up with the titles of a menu that has a check mark column.
    private static let leading: CGFloat = 21
    private static let trailing: CGFloat = 14
    private static let textWidth: CGFloat = 260
    private let label = NSTextField(wrappingLabelWithString: "")

    init(note: String) {
        super.init(title: note, action: nil, keyEquivalent: "")
        isEnabled = false
        label.font = .menuFont(ofSize: NSFont.smallSystemFontSize)
        // The same 0.7 of the label color as Brand.secondaryText, which keeps
        // small text above 4.5:1 on the light and dark menu backgrounds.
        label.textColor = NSColor.labelColor.withAlphaComponent(BrandTokens.Opacity.secondaryText)
        label.isSelectable = false
        label.preferredMaxLayoutWidth = Self.textWidth
        let row = NSView()
        row.addSubview(label)
        view = row
        self.note = note
    }

    required init(coder: NSCoder) {
        fatalError("MenuNoteItem is built in code")
    }

    var note: String {
        get { label.stringValue }
        set {
            title = newValue
            label.stringValue = newValue
            let height = ceil(label.cell?.cellSize(forBounds: NSRect(x: 0, y: 0, width: Self.textWidth,
                                                                     height: .greatestFiniteMagnitude)).height ?? 0)
            label.frame = NSRect(x: Self.leading, y: 4, width: Self.textWidth, height: height)
            view?.frame = NSRect(x: 0, y: 0, width: Self.leading + Self.textWidth + Self.trailing, height: height + 6)
        }
    }

    /// The AirPlay 2 switch and its note, for the AudioSync menu and the gear menu.
    static func airPlay2Items(text t: UIStrings, on: Bool, onAtLaunch: Bool,
                              action: Selector?, target: AnyObject? = nil) -> [NSMenuItem] {
        let note = t.airPlay2Note(on: on, onAtLaunch: onAtLaunch)
        let toggle = NSMenuItem(title: t.airPlay2Toggle, action: action, keyEquivalent: "")
        toggle.target = target
        toggle.state = on ? .on : .off
        toggle.toolTip = note
        return [toggle, MenuNoteItem(note: note)]
    }

    /// Brings the note under the item with `action` in `menu` up to date.
    static func refresh(_ menu: NSMenu, below action: Selector, note: String) {
        guard let index = menu.items.firstIndex(where: { $0.action == action }),
              index + 1 < menu.items.count,
              let item = menu.items[index + 1] as? MenuNoteItem else { return }
        menu.items[index].toolTip = note
        if item.note != note { item.note = note }
    }
}
