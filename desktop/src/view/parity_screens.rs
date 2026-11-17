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
    let eq_window = &app.equalizer;
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
    assert!(!app.zone_dialog.window().is_visible(), "no dialog");
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
    let place = |x, y| WindowPlace { x, y };
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

#[test]
fn the_guide_keeps_its_place_once_shown_and_none_before() {
    use super::tests::app_over;
    let app = app_over(crate::ui_model::MockBackend::new());
    app.save_settings();
    assert_eq!(app.backend.borrow().view_settings().guide, None);
    app.show_help();
    app.help.invoke_close_requested();
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
    let eq_window = &app.equalizer;
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
    eq_window.set_naming_curve(true);
    app.show_equalizer();
    assert!(!eq_window.get_naming_curve(), "it reopens with the menu");
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
        card.contains("height: 26px + 6px;"),
        "the transport's 26 high row 6 under the artist"
    );
    assert!(
        card.contains("y: 3px;"),
        "its 32 px buttons centred on that row"
    );
    assert!(card.contains("spacing: root.shuffle >= 0 || root.repeat >= 0 ? 6px : 22px;"));
    assert!(card.contains("glyph-size: 22px;"), "Play and Pause at 22");
    let window = component(&main, "MainWindow");
    assert_eq!(window.matches("- 28px;").count(), 2, "both covers");
    assert!(window.contains("padding-top: 16px;\n                        // The number ends"));
    assert!(window.contains(
        "padding-right: 1px;\n                            // 8 px clear of the knob at 100.\n                            number-gap: 16px;"
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
    assert!(window.contains("padding-top: title-row.height - 1px;"));
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
fn presets_equalizer_and_calibrate_are_36_high_rows_6_apart_with_no_border() {
    let source = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let quick = component(&source, "QuickRow");
    let equalizer = component(&source, "EqualizerCard");
    let presets = component(&source, "PresetsCard");
    let presets = &presets[..presets.find("list := FoldPane").unwrap()];
    for row in [quick, equalizer, presets] {
        assert!(row.contains("min-height: 36px;"));
        assert!(
            !row.lines().any(|line| line.starts_with("    border-width")),
            "no border"
        );
        assert!(
            !row.contains("padding-top: 6px;\n"),
            "both lines inside the 36"
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
