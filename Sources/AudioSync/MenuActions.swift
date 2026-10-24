import AppKit

/// Runs a closure when its menu item is chosen.
@MainActor
final class MenuAction: NSObject {
    let run: () -> Void
    init(_ run: @escaping () -> Void) { self.run = run }
    @objc func perform(_ sender: Any?) { run() }

    /// A menu item that runs `run`, checked when `on` is given and true.
    static func item(_ title: String, on: Bool? = nil, _ run: @escaping () -> Void) -> NSMenuItem {
        let action = MenuAction(run)
        let item = NSMenuItem(title: title, action: #selector(perform(_:)), keyEquivalent: "")
        item.target = action
        item.representedObject = action   // the target is weak
        if let on { item.state = on ? .on : .off }
        return item
    }
}

@MainActor
extension NSMenu {
    /// Opens the menu under `view`, as a pull-down from a button.
    func popUp(under view: NSView) {
        keepUntilNextPopUp()
        let below = view.isFlipped ? view.bounds.maxY + 4 : view.bounds.minY - 4
        _ = popUp(positioning: nil, at: NSPoint(x: view.bounds.minX, y: below), in: view)
    }

    /// Holds the menu, its items and their targets until the next menu opens:
    /// AppKit can send the chosen item's action after the pop-up call returns.
    func keepUntilNextPopUp() { OpenMenu.current = self }
}

@MainActor
private enum OpenMenu {
    static var current: NSMenu?
}
