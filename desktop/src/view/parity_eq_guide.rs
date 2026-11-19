//! The equalizer and the Guide as the Mac's (EqualizerWindow.swift,
//! Localization.swift's helpText, AppDelegate+Windows.swift's Guide window).

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::ComponentHandle;
use slint::platform::Key;

use super::tests::{app_over, click, draw_frames, leaked, press};
use crate::ui_model::{Backend, MockBackend};

const EQUALIZER: &str = include_str!("../../ui/equalizer.slint");
const HELP: &str = include_str!("../../ui/help.slint");
const STRINGS: &str = include_str!("../../ui/strings.slint");
const VIEW: &str = include_str!("../view.rs");

// Mac EqualizerWindow.swift:189-197: the curve menu's items are plain.
#[test]
fn the_curve_menu_ticks_no_curve() {
    assert!(!EQUALIZER.contains("checks: true;"));
    assert!(!EQUALIZER.contains("== root.current;"));
}

// Mac EqualizerWindow.swift:196 is a native menu, all of whose items stay
// reachable. The window cuts a Slint popup at its edge and the full curve
// menu runs past it, so the popup stops inside the window and its rows
// scroll, the keys scrolling the picked row into view.
#[test]
fn every_curve_menu_item_is_reachable_inside_the_window() {
    let app = app_over(MockBackend::new());
    app.show_equalizer();
    let eq = app.equalizer();
    let button = ElementHandle::find_by_accessible_label(&eq, "Curve")
        .find(|element| element.accessible_role() == Some(AccessibleRole::Combobox))
        .expect("the curve menu");
    let presets = eq.get_preset_count() as f32;
    // MenuRoom.rows: 24 px rows, the divider before Save Curve…, 4 px padding at each end.
    let full = (presets + 1.0) * 24.0 + 9.0 + 8.0;
    let below = button.absolute_position().y + button.size().height + 4.0;
    let height = eq
        .window()
        .size()
        .to_logical(eq.window().scale_factor())
        .height;
    assert!(below + full > height, "the full menu runs past the window");

    let source = EQUALIZER.replace("\r\n", "\n");
    let menu = &source[source.find("component CurveMenu").unwrap()..];
    let menu = &menu[menu.find("menu := PopupWindow").unwrap()..];
    let menu = &menu[..menu.find("export component EqualizerWindow").unwrap()];
    assert!(
        menu.contains("height: MenuRoom.height > 0px ? min(panel.preferred-height,"),
        "the popup's height stops at the window's"
    );
    assert!(menu.contains("OverlayScrollView {"), "its rows scroll");
    assert!(
        menu.contains("changed current =>"),
        "a row the keys pick scrolls into view"
    );
    assert!(
        source.contains("MenuRoom.height = self.height;"),
        "the window gives its menus its height"
    );
}

// Mac EqualizerWindow.swift:159-160 is a native pop-up button filling its
// 170 pt frame. The 2x EQ-WINDOW screen captures of 19 Nov measure it 21 pt
// tall, white on light with the curve name in the text colour, and a 17 pt
// brand-teal tile holding the chevron 2 pt inside its right edge.
#[test]
fn the_curve_menu_is_a_pop_up_button_with_a_teal_chevron_tile() {
    let app = app_over(MockBackend::new());
    app.show_equalizer();
    let eq = app.equalizer();
    let button = ElementHandle::find_by_accessible_label(&eq, "Curve")
        .find(|element| element.accessible_role() == Some(AccessibleRole::Combobox))
        .expect("the curve menu");
    assert_eq!((button.size().width, button.size().height), (170.0, 21.0));

    let source = EQUALIZER.replace("\r\n", "\n");
    let menu = &source[source.find("component CurveMenu").unwrap()..];
    let menu = &menu[..menu.find("menu := PopupWindow").unwrap()];
    assert!(
        menu.contains("background: Fluent.native-control;"),
        "{menu}"
    );
    assert!(!menu.contains("Tokens.teal-text"), "{menu}");
    let tile = &menu[menu.find("tile := Rectangle {").expect("the chevron tile")..];
    assert!(tile.contains("width: 17px;"), "{tile}");
    assert!(tile.contains("height: 17px;"), "{tile}");
    assert!(tile.contains("background: Tokens.teal;"), "{tile}");
}

// Mac EqualizerWindow.swift:25 (fixed size, close only) and :179 (bands
// 140 pt tall).
#[test]
fn the_equalizer_has_a_fixed_size_and_only_a_close_button() {
    assert!(EQUALIZER.contains("height: 140px;"), "bands 140 tall");
    assert!(!EQUALIZER.contains("min-height: 140px;"));
    assert!(!EQUALIZER.contains("min-height: 360px;"));
    assert!(EQUALIZER.contains("max-height: content.preferred-height;"));
    assert!(VIEW.contains("self.follow_dialog_frame(&self.equalizer(), \"equalizer\");"));
}

// Mac Equalizer.swift:224-226: the curve keeps the gains at the moment
// its name is confirmed.
#[test]
fn save_curve_keeps_a_band_moved_while_naming() {
    let app = app_over(MockBackend::new());
    app.equalizer().invoke_save_curve();
    app.equalizer().invoke_gain_changed(3, 6.0);
    app.equalizer().invoke_curve_named("Mine".into(), true);
    let curves = app.snapshot().equalizer_curves;
    let mine = curves.iter().find(|c| c.name == "Mine").expect("saved");
    assert_eq!(mine.gains_db[3], 6.0);
}

// Mac Localization.swift:1941-1942, GuideLayout.swift:83.
#[test]
fn the_guide_opens_with_its_title_and_byline() {
    assert!(STRINGS.contains("help-byline: @tr(\"By Yasser Sutojoyo Kusumo\");"));
    let intro = HELP.find("Strings.help-intro").expect("intro");
    let title = HELP
        .find("text: Strings.help-title;")
        .expect("title in the page");
    let byline = HELP.find("text: Strings.help-byline;").expect("byline");
    assert!(title < byline && byline < intro);
    assert!(HELP.contains("font-size: Tokens.font-size-body + 4px;"));
}

// Mac Localization.swift:1949-2175: First Steps, More Controls,
// Calibration, AirPlay, Google Cast, Notices, Web Remote, Tips, Volume;
// Hearing Sound Twice, a Windows topic, last.
#[test]
fn the_guide_sections_follow_the_mac_order() {
    let titles: Vec<&str> = HELP
        .lines()
        .filter_map(|line| line.trim().strip_prefix("title: Strings.help-"))
        .filter_map(|rest| rest.strip_suffix("-title;"))
        .collect();
    assert_eq!(
        titles,
        [
            "start",
            "more",
            "calibration",
            "airplay",
            "cast",
            "notices",
            "remote",
            "tips",
            "volume",
            "double"
        ]
    );
    let delay = HELP.find("guide/delay.svg").expect("delay art");
    let more = HELP.find("title: Strings.help-more-title;").expect("more");
    let calibration = HELP.find("title: Strings.help-calibration-title;");
    assert!(
        more < delay && Some(delay) < calibration,
        "delay art in More Controls"
    );
    let classes = HELP.find("guide/classes.svg").expect("classes art");
    let cast = HELP.find("title: Strings.help-cast-title;");
    let notices = HELP.find("title: Strings.help-notices-title;");
    assert!(
        cast < Some(classes) && Some(classes) < notices,
        "classes art opens Notices"
    );
}

// Mac GuideLayout.swift:40-66: First Steps numbered, More Controls and
// Tips as bullets with a hanging indent.
#[test]
fn the_guide_sets_its_lists_as_lists() {
    assert!(HELP.contains("component HelpList "));
    assert_eq!(HELP.matches("numbered: true;").count(), 1, "First Steps");
    assert!(HELP.matches("HelpList {").count() >= 3);
}

// Mac Localization.swift:2040.
#[test]
fn the_guide_says_what_happens_when_the_source_app_quits() {
    assert!(STRINGS.contains(
        "help-more-source-quits: @tr(\"When the source app quits, AudioSync switches to All System Audio and keeps playing.\");"
    ));
    assert!(HELP.contains("Strings.help-more-source-quits"));
}

// Mac AppDelegate+Windows.swift:924 (selectable text) and :908-946 (no
// Close button).
#[test]
fn the_guide_text_is_selectable_and_has_no_close_button() {
    // Only the list markers, which Narrator skips, are plain text.
    let plain = HELP
        .lines()
        .filter(|line| line.trim_end().ends_with(" Text {") || line.trim() == "Text {")
        .count();
    assert_eq!(plain, 1, "every other text can be selected");
    assert!(HELP.contains("text-cursor-width: 0px;"), "no caret");
    assert!(HELP.contains("read-only: true;"));
    assert!(!HELP.contains("Strings.guide-close"));
}

// Mac AppDelegate+Windows.swift:866-874: the Guide reopens where it was
// left, scroll included.
#[test]
fn the_guide_reopens_where_it_was_scrolled() {
    let app = app_over(MockBackend::new());
    let window = leaked(app.help());
    app.main.invoke_help_requested();
    draw_frames();
    press(window, Key::End);
    let bottom = app.help().get_scrolled();
    assert!(bottom > 0.0);
    press(window, Key::Escape);
    assert!(!window.is_visible());
    app.drop_closed_windows();
    assert!(app.help.built().is_none(), "the closed Guide is dropped");
    app.main.invoke_help_requested();
    draw_frames();
    assert_eq!(app.help().get_scrolled(), bottom);
}

// Mac AppDelegate+Windows.swift:940-945: place and size both kept across
// launches.
#[test]
fn the_guide_keeps_its_size_across_launches() {
    let app = app_over(MockBackend::new());
    let window = leaked(app.help());
    app.main.invoke_help_requested();
    draw_frames();
    window.set_size(slint::LogicalSize::new(700.0, 500.0));
    app.help().invoke_close_requested();
    app.remember_view();
    let saved = app.backend.borrow().view_settings();
    assert_eq!(
        saved.guide.and_then(|place| place.size),
        Some((700.0, 500.0))
    );

    let mut backend = MockBackend::new();
    backend.set_view_settings(saved);
    let again = app_over(backend);
    // No draw_frames: it gives every headless window one size.
    again.main.invoke_help_requested();
    let help = leaked(again.help());
    let size = help.size().to_logical(help.scale_factor());
    assert_eq!((size.width, size.height), (700.0, 500.0));
}

// Mac AppDelegate+Windows.swift:966: the frame autosave also keeps a Guide
// still open when AudioSync quits. The test backend keeps no window position.
#[test]
fn the_guide_keeps_its_size_when_audiosync_quits_with_it_open() {
    let app = app_over(MockBackend::new());
    let window = leaked(app.help());
    app.main.invoke_help_requested();
    draw_frames();
    window.set_size(slint::LogicalSize::new(820.0, 520.0));
    app.main.invoke_exit_requested();
    app.at_quit();
    let saved = app.backend.borrow().view_settings();
    let place = saved.guide.expect("the Guide's place is saved");
    assert_eq!(place.size, Some((820.0, 520.0)));

    let mut backend = MockBackend::new();
    backend.set_view_settings(saved);
    let again = app_over(backend);
    again.main.invoke_help_requested();
    let help = leaked(again.help());
    let size = help.size().to_logical(help.scale_factor());
    assert_eq!((size.width, size.height), (820.0, 520.0));
}

// A click in the text, which takes the focus for Ctrl+C, leaves the arrow
// keys scrolling, and Tab skips the paragraphs.
#[test]
fn the_guide_scrolls_by_keyboard_after_a_click_and_tab_skips_its_text() {
    let app = app_over(MockBackend::new());
    let window = leaked(app.help());
    app.main.invoke_help_requested();
    draw_frames();
    click(window, 200.0, 120.0);
    draw_frames();
    press(window, Key::DownArrow);
    let after_click = app.help().get_scrolled();
    assert!(after_click > 0.0, "Down scrolls after a click in the text");
    for _ in 0..3 {
        press(window, Key::Tab);
    }
    press(window, Key::DownArrow);
    assert!(
        app.help().get_scrolled() > after_click,
        "Down scrolls after Tab"
    );
    press(window, Key::Escape);
    assert!(!window.is_visible(), "Escape closes it after a click");
}
