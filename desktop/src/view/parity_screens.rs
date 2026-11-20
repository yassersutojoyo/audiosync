//! The secondary windows laid out as the Mac's: the equalizer, Add by
//! Address, About, the Guide and the tray panel's footer.

use super::{GUIDE_CHROME, guide_place_to_show};
use crate::placement::Bounds;
use crate::ui_model::WindowPlace;

/// The text of component `name` in `source`, up to the next component.
fn component<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("component {name} "))
        .unwrap_or_else(|| panic!("{name}"));
    let rest = &source[start..];
    let end = ["\ncomponent ", "\nexport component "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn the_equalizer_is_520_wide_with_the_macs_spacing_and_no_card() {
    let source = include_str!("../../ui/equalizer.slint");
    let window = component(source, "EqualizerWindow");
    assert!(window.contains("min-width: 520px;") && window.contains("max-width: 520px;"));
    assert!(window.contains("padding: 18px;") && window.contains("spacing: 14px;"));
    assert!(!source.contains("Card {"), "the bands sit on the window");
    assert!(window.contains("spacing: 6px;"), "6 between the bands");
}

#[test]
fn an_equalizer_turned_off_dims_its_sliders_at_once() {
    let source = include_str!("../../ui/equalizer.slint");
    assert!(!source.contains("animate opacity"));
}

#[test]
fn save_rename_and_delete_are_items_of_the_curve_menu() {
    let source = include_str!("../../ui/equalizer.slint");
    let window = component(source, "EqualizerWindow");
    assert!(!source.contains("ComboBox"), "a menu, not a combo box");
    assert_eq!(
        window.matches("Button {").count(),
        1,
        "the bottom row has Reset only"
    );
    for item in [
        "text: Strings.equalizer-save-curve;",
        "text: Strings.equalizer-rename-curve(",
        "text: Strings.equalizer-delete-curve(",
    ] {
        assert!(source.contains(item), "{item}");
    }
    // A Windows checkout has CRLF line ends.
    let strings = include_str!("../../ui/strings.slint").replace("\r\n", "\n");
    for text in [
        "equalizer-save-curve: @tr(\"Save Curve…\");",
        "function equalizer-rename-curve(name: string) -> string {\n        return @tr(\"Rename “{}”…\", name);",
        "function equalizer-delete-curve(name: string) -> string {\n        return @tr(\"Delete “{}”\", name);",
    ] {
        assert!(strings.contains(text), "{text}");
    }
}

#[test]
fn add_by_address_reads_as_the_macs_sheet() {
    let dialog = component(include_str!("../../ui/dialogs.slint"), "CastAddressSheet");
    let at = |text: &str| dialog.find(text).unwrap_or_else(|| panic!("{text}"));
    assert!(dialog.contains("padding: 20px;"));
    assert!(dialog.contains("text: Strings.cast-add-address-title;"));
    assert!(dialog.contains("font-size: Tokens.font-size-title3;"));
    assert!(dialog.contains("font-size: Tokens.font-size-callout;"));
    assert!(
        dialog.contains("text: Strings.cast-address-label;"),
        "a visible label"
    );
    assert!(dialog.contains("placeholder-text: Strings.cast-address-label;"));
    assert!(
        !dialog.contains("Tokens.notice"),
        "the reason is red text, not a box"
    );
    assert!(
        at("TextField {") < at("color: Tokens.error-text;"),
        "under the field"
    );
    assert!(dialog.contains("Spinner {"), "a spinner while it checks");
    assert!(
        at("text: Strings.cancel;") < at("Strings.add;"),
        "Cancel, then Add"
    );
    assert!(dialog.contains("boxed: true;"), "a boxed field");
}

#[test]
fn about_names_the_license_before_the_copyright() {
    let about = include_str!("../../ui/about.slint");
    let license = about.find("Strings.about-license").expect("license");
    let copyright = about.find("Strings.about-copyright").expect("copyright");
    assert!(license < copyright);
}

#[test]
fn the_guide_keeps_440_by_300_and_insets_its_text_18_by_16() {
    let help = include_str!("../../ui/help.slint");
    assert!(help.contains("min-width: 440px;") && help.contains("min-height: 300px;"));
    assert!(help.contains("padding-left: 18px;") && help.contains("padding-top: 16px;"));
}

#[test]
fn the_tray_footer_buttons_are_grey_24_high_with_a_7_percent_hover() {
    let footer = component(include_str!("../../ui/flyout.slint"), "FooterButton");
    assert!(footer.contains("color: Tokens.secondary-text;"));
    assert!(footer.contains("height: 24px;"));
    assert!(footer.contains("padding-left: 6px;"));
    assert!(footer.contains("Tokens.text.with-alpha(0.07)"));
}

#[test]
fn save_curve_names_the_curve_in_place_of_the_menu() {
    use super::tests::{app_over, draw_frames, press, type_text};
    use slint::ComponentHandle;
    use slint::platform::Key;
    let app = app_over(crate::ui_model::MockBackend::new());
    let eq_window = &app.equalizer();
    eq_window.show().expect("the equalizer shows headless");
    let window = eq_window.window();
    let names = || {
        let mut names: Vec<_> = app
            .snapshot()
            .equalizer_curves
            .iter()
            .map(|curve| curve.name.clone())
            .collect();
        names.sort();
        names
    };
    eq_window.invoke_save_curve();
    draw_frames();
    assert!(!app.zone_dialog.is_visible(), "no dialog");
    type_text(window, "Desk");
    press(window, Key::Return);
    assert_eq!(names(), ["Desk"]);

    eq_window.invoke_gain_changed(0, 6.0);
    eq_window.invoke_save_curve();
    draw_frames();
    press(window, Key::Escape);
    assert!(window.is_visible(), "Escape drops the name, not the window");
    assert_eq!(names(), ["Desk"]);

    eq_window.invoke_save_curve();
    draw_frames();
    press(window, Key::Return);
    assert_eq!(names(), ["Curve 2", "Desk"], "Return saves the suggestion");
}

#[test]
fn the_guide_opens_where_it_was_kept_on_a_monitor() {
    let work = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1040.0,
    };
    let size = (640.0, 600.0);
    let (width, height) = (size.0 + GUIDE_CHROME.0, size.1 + GUIDE_CHROME.1);
    let place = |x, y| WindowPlace { x, y, size: None };
    assert_eq!(
        guide_place_to_show(place(100, 80), size, true, Some((work, 1.0))),
        Some(place(100, 80)),
        "where it was"
    );
    assert_eq!(
        guide_place_to_show(place(1800, 900), size, true, Some((work, 1.0))),
        Some(place((1920.0 - width) as i32, (1040.0 - height) as i32)),
        "pulled back inside the work area"
    );
    assert_eq!(
        guide_place_to_show(place(-5000, 4000), size, false, Some((work, 1.0))),
        Some(place(
            ((1920.0 - width) / 2.0).round() as i32,
            ((1040.0 - height) / 2.0).round() as i32
        )),
        "off every monitor, centered on the primary one"
    );
    assert_eq!(
        guide_place_to_show(place(100, 80), size, true, None),
        Some(place(100, 80))
    );
    assert_eq!(
        guide_place_to_show(place(-5000, 0), size, false, None),
        None
    );
}

// Mac AppDelegate+Windows.swift:966 restores the Guide's frame through
// AppKit, which keeps it on the screen; :936 sets its 440 x 300 minimum.
#[test]
fn a_saved_guide_larger_than_the_work_area_opens_fitted_to_it() {
    let work = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1280.0,
        height: 720.0,
    };
    let place = WindowPlace {
        x: 40,
        y: 30,
        size: Some((1800.0, 1100.0)),
    };
    let shown = guide_place_to_show(place, (1800.0, 1100.0), true, Some((work, 1.0)));
    assert_eq!(
        shown,
        Some(WindowPlace {
            x: 0,
            y: 0,
            size: Some((1280.0 - GUIDE_CHROME.0, 720.0 - GUIDE_CHROME.1)),
        })
    );
}

#[test]
fn the_guide_keeps_its_place_once_shown_and_none_before() {
    use super::tests::app_over;
    let app = app_over(crate::ui_model::MockBackend::new());
    app.save_settings();
    assert_eq!(app.backend.borrow().view_settings().guide, None);
    app.show_help();
    app.help().invoke_close_requested();
    app.save_settings();
    assert!(app.backend.borrow().view_settings().guide.is_some());
}

#[test]
fn a_settings_file_without_the_guides_place_or_with_a_bad_one_reads_as_none() {
    let stored = |text: &str| -> serde_json::Value {
        let (read, _) = crate::settings::Settings::from_json(text).expect("reads");
        serde_json::from_str(&read.to_json()).expect("written as JSON")
    };
    for text in [
        r#"{"version": 3}"#,
        r#"{"version": 3, "guide_window": "x"}"#,
        r#"{"version": 3, "guide_window": {"x": 1}}"#,
    ] {
        let read = stored(text);
        assert!(
            read.get("guide_window")
                .is_none_or(serde_json::Value::is_null),
            "{text}"
        );
    }
    let read = stored(r#"{"version": 3, "guide_window": {"x": -1500, "y": 120}}"#);
    assert_eq!(
        read["guide_window"],
        serde_json::json!({"x": -1500, "y": 120})
    );
}

#[test]
fn closing_the_equalizer_drops_a_curve_name_being_typed() {
    use super::tests::{app_over, draw_frames, type_text};
    use slint::ComponentHandle;
    let app = app_over(crate::ui_model::MockBackend::new());
    let eq_window = &app.equalizer();
    let window = eq_window.window();
    let closes: [fn(&crate::EqualizerWindow); 2] = [
        |eq| eq.invoke_close_requested(),
        |eq| {
            eq.window()
                .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        },
    ];
    for close in closes {
        app.show_equalizer();
        eq_window.invoke_save_curve();
        draw_frames();
        type_text(window, "Late");
        close(eq_window);
        draw_frames();
        assert!(!eq_window.get_naming_curve(), "the field is gone");
        assert!(app.snapshot().equalizer_curves.is_empty(), "nothing saved");
    }
    app.equalizer().set_naming_curve(true);
    app.show_equalizer();
    assert!(
        !app.equalizer().get_naming_curve(),
        "it reopens with the menu"
    );
}

/// The main window's left column takes the Mac's spacing: the Now Playing
/// stack 4 apart with the art 22 high and 3 clear, the transport 6 below,
/// Master Volume 16 under the block, the hairline 11 above and 5 below,
/// and the rows 6 apart, with no scroll bar on either column.
// covers: S450
#[test]
fn the_left_column_takes_the_macs_spacing() {
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let card = component(&main, "NowPlayingCard");
    assert!(card.contains("padding-top: 2px;\n        spacing: 4px;"));
    assert!(card.contains(
        "padding-top: 3px;\n            padding-bottom: 3px;\n\n            AmbientArt {\n                height: 22px;"
    ));
    assert!(
        card.contains("height: 22px + 6px;"),
        "the transport's row as high as the Mac's 22 pt Pause symbol, 6 under the artist"
    );
    assert!(
        card.contains("y: 1px;"),
        "its 32 px buttons 2 px above centred on that row"
    );
    assert!(card.contains("spacing: root.shuffle >= 0 || root.repeat >= 0 ? 2px : 18px;"));
    assert!(card.contains("glyph-size: 22px;"), "Play and Pause at 22");
    let window = component(&main, "MainWindow");
    assert_eq!(window.matches("- 28px;").count(), 2, "both covers");
    assert!(window.contains("padding-top: 16px;\n                        // The number ends"));
    assert!(window.contains(
        "padding-right: 1px;\n                            // The Mac's 6 from the knob at 100 to the number (MainWindow.swift:545).\n                            number-gap: 14px;"
    ));
    assert!(window.contains(
        "padding-top: 11px;\n                        padding-bottom: 5px;\n\n                        Divider { }"
    ));
    assert!(window.contains(
        "VerticalLayout {\n                        spacing: 6px;\n                        padding-left: -8px;\n                        padding-right: -8px;\n\n                        presets-card := PresetsCard {"
    ));
    assert!(window.contains("left-column := FadeScrollView {"));
    assert!(window.contains("scroll := FadeScrollView {"));
    assert!(window.contains("property <length> scroll-gutter: 0px;"));
}

#[test]
fn the_main_window_has_no_menu_row_and_a_gear_left_of_the_switch() {
    let source = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let window = component(&source, "MainWindow");
    let title_row = &window[window.find("title-row := HorizontalLayout {").unwrap()..];
    let title_row = &title_row[..title_row.find("\n    }\n").unwrap()];
    assert!(
        !title_row.contains("ContextMenuArea"),
        "no menus in the top row"
    );
    // The MASTER-MUTED Mac photo has a shorter title bar (smaller window
    // buttons); the other 21 main window photos put the header 3 pt lower.
    assert!(window.contains("padding-top: title-row.height + 3px;"));
    let gear = window.find("gear := GearButton {").expect("the gear");
    assert!(gear < window.find("power := PowerSwitch {").unwrap());
    assert!(window[gear..].contains("glyph-size: 16px;"));
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let button = component(&widgets, "IconButton");
    assert!(button.contains("width: 32px;") && button.contains("height: 32px;"));
    let gear = component(&source, "GearButton");
    assert!(
        gear.contains("label: Strings.more-options;"),
        "Narrator names it"
    );
}

#[test]
fn settings_is_460_wide_with_the_macs_boxes_and_rows() {
    let source = include_str!("../../ui/settings.slint").replace("\r\n", "\n");
    let window = component(&source, "SettingsWindow");
    assert!(window.contains("width: 460px;"));
    assert!(window.contains("padding: 20px;") && window.contains("spacing: 16px;"));
    assert!(component(&source, "SettingsRow").contains("min-height: 48px;"));
    for title in [
        "Strings.settings-general",
        "Strings.menu-speakers",
        "Strings.settings-updates",
        "Strings.setup-title",
    ] {
        assert!(window.contains(&format!("title: {title};")), "{title}");
    }
}

#[test]
fn the_tray_panel_is_300_wide_with_the_macs_padding_and_rows() {
    let source = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    let panel = component(&source, "TrayFlyout");
    assert!(panel.contains("width: 300px;"));
    assert!(panel.contains("padding: 14px;\n            padding-bottom: 8px;"));
    assert!(component(&source, "FlyoutRow").contains("min-height: 36px;"));
    assert!(panel.contains("max(6 * 36px, list.preferred-height - root.list-cut)"));
}

#[test]
fn presets_equalizer_and_calibrate_are_28_high_rows_6_apart_with_no_border() {
    let source = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let quick = component(&source, "QuickRow");
    let equalizer = component(&source, "EqualizerCard");
    let presets = component(&source, "PresetsCard");
    let presets = &presets[..presets.find("list := FoldPane").unwrap()];
    // The Mac's QuickRow.height (MainWindow.swift:1175).
    for row in [quick, equalizer, presets] {
        assert!(row.contains("min-height: 28px;"));
        assert!(
            !row.lines().any(|line| line.starts_with("    border-width")),
            "no border"
        );
        assert!(
            !row.contains("padding-top: 6px;\n"),
            "both lines inside the row"
        );
    }
    assert!(quick.contains("border-radius: Fluent.radius-control;"));
    assert!(equalizer.contains("border-radius: Fluent.radius-control;"));
    // As the Mac's: no second line until a preset is picked.
    assert!(presets.contains("text: root.current;"));
    let window = component(&source, "MainWindow");
    let rows = &window[window.find("PresetsCard {").unwrap() - 240..];
    assert!(rows.contains("spacing: 6px;"));
    assert!(window.contains("button-size: 24px;"));
}

#[test]
fn the_device_volume_fix_reads_in_the_hints_secondary_colour() {
    let source = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let row = component(&source, "SpeakerRow");
    let fix = &row[row.find("fix-button := LinkButton {").expect("the fix")..];
    let fix = &fix[..fix.find("clicked =>").unwrap()];
    // The Mac's .primary (DeviceVolumeViews.swift:34) is the primary level of the
    // hint's Brand.secondaryText (:43), drawn grey in the Mac render.
    assert!(fix.contains("text-color: Tokens.secondary-text;"), "{fix}");
}

#[test]
fn the_tray_panels_empty_note_is_set_at_the_macs_subheadline() {
    let source = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    let note = &source[source
        .find("none-note := HorizontalLayout")
        .expect("the note")..];
    let text = &note[note.find("SecondaryText {").unwrap()..];
    let text = &text[..text.find('}').unwrap()];
    // The Mac panel's .subheadline (MenuBar.swift:1865), not the caption size.
    assert!(
        text.contains("font-size: Tokens.font-size-subheadline;"),
        "{text}"
    );
}

#[test]
fn the_equalizer_fills_its_bands_in_the_brand_teal() {
    let source = include_str!("../../ui/equalizer.slint");
    // The Mac's .tint(Brand.teal) (EqualizerWindow.swift:122), not the system accent.
    assert!(!source.contains("Palette.accent-background"));
    assert!(source.contains("background: Tokens.teal;"));
}

#[test]
fn the_equalizers_reset_is_the_macs_house_button() {
    let source = include_str!("../../ui/equalizer.slint");
    let at = source
        .find("text: Strings.equalizer-reset;")
        .expect("Reset");
    let start = source[..at].rfind("Button {").expect("its button");
    let button = &source[start - 5..at + source[at..].find("clicked =>").unwrap()];
    // HouseButtonStyle (EqualizerWindow.swift:183): the teal tint, 13 pt, 26 pt high
    // (HouseButtonStyle.swift:31-34), as the Add by Address sheet's buttons.
    assert!(button.starts_with("Small"), "{button}");
    assert!(button.contains("height: 26px;"), "{button}");
    assert!(
        button.contains("font-size: Tokens.font-size-body;"),
        "{button}"
    );
}

#[test]
fn the_off_power_switch_is_drawn_in_the_macs_secondary() {
    let source = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let switch = component(&source, "PowerSwitch");
    // Color.secondary and its 15 % fill (MenuBar.swift:2934-2935): black at 50 % on
    // light and white at 55 % on dark, as the off toggle glyph's.
    assert!(
        switch.contains(": Tokens.dark ? #ffffff15 : #00000013;"),
        "{switch}"
    );
    assert!(
        switch.contains("(root.system ? #ffffff3d : #ffffff8c) : #00000080;"),
        "{switch}"
    );
}

#[test]
fn the_panels_mute_button_is_drawn_in_the_secondary_text() {
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let volume = component(&widgets, "MasterVolume");
    assert!(volume.contains("color: root.icon-color;"), "{volume}");
    // The panel's mute glyph is Brand.secondaryText, the window's .primary (MenuBar.swift:2978).
    let flyout = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    let row = &flyout[flyout
        .find("MasterVolume {")
        .expect("the flyout's master volume")..];
    assert!(
        row[..row.find("mute-toggled").unwrap()].contains("icon-color: Tokens.secondary-text;"),
        "{row}"
    );
}

/// SettingsWindow.swift:355-360: the system switch, which MAIN-SETTINGS-*-mac.png
/// draws 38 x 22 pt with a filled track and no ring: black at 10 % on light,
/// white at 10 % with a white 24 % hairline and a #c6c6c6 knob on dark.
/// SettingsWindow.swift:219-229: Check for Updates and Check Setup are
/// HouseButtonStyle(), 13 pt, 26 pt high, padded 12 pt (HouseButtonStyle.swift:31-35).
#[test]
fn settings_switches_and_buttons_are_the_macs() {
    let settings = include_str!("../../ui/settings.slint").replace("\r\n", "\n");
    let row = &settings[settings.find("switch := PowerSwitch {").expect("switch")..];
    assert!(row[..row.find("toggled =>").unwrap()].contains("system: true;"));
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let switch = component(&widgets, "PowerSwitch");
    assert!(
        switch.contains("root.system ? (Tokens.dark ? #ffffff1a : #0000001a)"),
        "{switch}"
    );
    assert!(switch.contains("#c6c6c6"), "{switch}");
    for text in ["Strings.menu-check-for-updates", "Strings.menu-check-setup"] {
        let at = settings.find(&format!("text: {text};")).expect(text);
        let start = settings[..at].rfind("SmallButton {").expect("its button");
        let button = &settings[start..at];
        for want in [
            "height: 26px;",
            "font-size: Tokens.font-size-body;",
            "side-padding: 12px;",
        ] {
            assert!(button.contains(want), "{text}: {want}");
        }
    }
}
