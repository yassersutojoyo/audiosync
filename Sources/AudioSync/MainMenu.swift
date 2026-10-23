import AppKit

/// The app's menu bar menus. Items without a target go up the responder chain,
/// which ends at the app delegate, so AppDelegate's actions and its
/// `validateMenuItem` handle the AudioSync-specific ones.
@MainActor
enum MainMenu {

    /// `appMenuDelegate` keeps the AirPlay 2 note under its switch current.
    static func make(text t: UIStrings, airPlay2: Bool, airPlay2AtLaunch: Bool,
                     appMenuDelegate: NSMenuDelegate) -> NSMenu {
        let main = NSMenu()

        let app = submenu(of: main, title: t.appName)
        app.delegate = appMenuDelegate
        app.addItem(AboutPanel.menuItem(text: t))
        app.addItem(.separator())
        let appearance = NSMenu(title: t.appearance)
        for choice in AppAppearance.allCases {
            let entry = item(t.appearanceName(choice), #selector(AppDelegate.chooseAppearance(_:)))
            entry.representedObject = choice.rawValue
            appearance.addItem(entry)
        }
        app.addItem(withTitle: t.appearance, action: nil, keyEquivalent: "").submenu = appearance
        app.addItem(item(t.launchAtLogin, #selector(AppDelegate.toggleLaunchAtLogin(_:))))
        app.addItem(item(t.switchToSystemWhenSourceQuits, #selector(AppDelegate.toggleSwitchWhenSourceQuits(_:))))
        MenuNoteItem.airPlay2Items(text: t, on: airPlay2, onAtLaunch: airPlay2AtLaunch,
                                   action: #selector(AppDelegate.toggleAirPlay2(_:))).forEach(app.addItem)
        app.addItem(.separator())
        let services = NSMenu(title: t.menuServices)
        app.addItem(withTitle: t.menuServices, action: nil, keyEquivalent: "").submenu = services
        NSApp.servicesMenu = services
        app.addItem(.separator())
        app.addItem(item(t.menuHide, #selector(NSApplication.hide(_:)), "h"))
        app.addItem(item(t.menuHideOthers, #selector(NSApplication.hideOtherApplications(_:)), "h",
                         [.command, .option]))
        app.addItem(item(t.menuShowAll, #selector(NSApplication.unhideAllApplications(_:))))
        app.addItem(.separator())
        app.addItem(item(t.quitApp, #selector(NSApplication.terminate(_:)), "q"))

        let file = submenu(of: main, title: t.menuFile)
        file.addItem(item(t.menuApplyDelay, #selector(AppDelegate.applyTypedDelay(_:)), "s"))
        file.addItem(item(t.menuCloseWindow, #selector(NSWindow.performClose(_:)), "w"))

        // Text fields (the sync delay) need the standard editing commands.
        let edit = submenu(of: main, title: t.menuEdit)
        edit.addItem(item(t.menuUndo, Selector(("undo:")), "z"))
        edit.addItem(item(t.menuRedo, Selector(("redo:")), "z", [.command, .shift]))
        edit.addItem(.separator())
        edit.addItem(item(t.menuCut, #selector(NSText.cut(_:)), "x"))
        edit.addItem(item(t.menuCopy, #selector(NSText.copy(_:)), "c"))
        edit.addItem(item(t.menuPaste, #selector(NSText.paste(_:)), "v"))
        edit.addItem(item(t.menuSelectAll, #selector(NSText.selectAll(_:)), "a"))

        let speakers = submenu(of: main, title: t.menuSpeakers)
        speakers.addItem(item(t.menuTurnStreamingOn, #selector(AppDelegate.toggleStreaming(_:))))
        speakers.addItem(.separator())
        speakers.addItem(item(t.newGroup, #selector(AppDelegate.newGroup(_:)), "n"))
        speakers.addItem(item(t.refreshDevices, #selector(AppDelegate.refreshDevices(_:)), "r"))
        let hidden = NSMenu(title: t.hiddenSpeakers)
        hidden.delegate = HiddenSpeakersMenu.shared   // filled each time it opens
        speakers.addItem(withTitle: t.hiddenSpeakers, action: nil, keyEquivalent: "").submenu = hidden

        let window = submenu(of: main, title: t.menuWindow)
        window.addItem(item(t.menuMinimize, #selector(NSWindow.performMiniaturize(_:)), "m"))
        window.addItem(item(t.menuZoom, #selector(NSWindow.performZoom(_:))))
        window.addItem(.separator())
        window.addItem(item(t.mainWindowTitle, #selector(AppDelegate.showMainWindow(_:)), "0"))
        window.addItem(item(t.equalizer, #selector(AppDelegate.showEqualizer(_:)), "e", [.command, .option]))
        window.addItem(item(t.webRemoteMenu, #selector(AppDelegate.showWebRemote(_:))))
        window.addItem(.separator())
        window.addItem(item(t.menuBringAllToFront, #selector(NSApplication.arrangeInFront(_:))))
        NSApp.windowsMenu = window

        let help = submenu(of: main, title: t.menuHelp)
        help.addItem(item(t.guideTitle, #selector(AppDelegate.showGuide(_:)), "?"))
        NSApp.helpMenu = help

        return main
    }

    private static func submenu(of main: NSMenu, title: String) -> NSMenu {
        let menu = NSMenu(title: title)
        main.addItem(withTitle: title, action: nil, keyEquivalent: "").submenu = menu
        return menu
    }

    private static func item(_ title: String, _ action: Selector, _ key: String = "",
                             _ modifiers: NSEvent.ModifierFlags = .command) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.keyEquivalentModifierMask = modifiers
        return item
    }
}
