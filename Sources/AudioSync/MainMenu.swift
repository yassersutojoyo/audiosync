import AppKit

/// The app's menu bar menus. Items without a target go up the responder chain,
/// which ends at the app delegate, so AppDelegate's actions and its
/// `validateMenuItem` handle the AudioSync-specific ones.
@MainActor
enum MainMenu {

    static func make(text t: UIStrings) -> NSMenu {
        let main = NSMenu()

        let app = submenu(of: main, title: t.appName)
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
        let airPlay2 = item(t.airPlay2Toggle, #selector(AppDelegate.toggleAirPlay2(_:)))
        airPlay2.subtitle = t.airPlay2Note
        airPlay2.toolTip = t.airPlay2Note
        app.addItem(airPlay2)
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
