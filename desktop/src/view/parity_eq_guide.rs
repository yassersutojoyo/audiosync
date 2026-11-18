//! The equalizer and the Guide as the Mac's (EqualizerWindow.swift,
//! Localization.swift's helpText, AppDelegate+Windows.swift's Guide window).

use slint::ComponentHandle;
use slint::platform::Key;

use super::tests::{app_over, draw_frames, press};
use crate::ui_model::{Backend, MockBackend};

const EQUALIZER: &str = include_str!("../../ui/equalizer.slint");
const HELP: &str = include_str!("../../ui/help.slint");
const STRINGS: &str = include_str!("../../ui/strings.slint");
const VIEW: &str = include_str!("../view.rs");

// Mac EqualizerWindow.swift:189-197: the curve menu's items are plain.
#[test]
fn the_curve_menu_ticks_no_curve() {
    assert!(!EQUALIZER.contains("checks: true;"));
    assert!(!EQUALIZER.contains("checked: "));
}

// Mac EqualizerWindow.swift:25 (fixed size, close only) and :179 (bands
// 140 pt tall).
#[test]
fn the_equalizer_has_a_fixed_size_and_only_a_close_button() {
    assert!(EQUALIZER.contains("height: 140px;"), "bands 140 tall");
    assert!(!EQUALIZER.contains("min-height: 140px;"));
    assert!(!EQUALIZER.contains("min-height: 360px;"));
    assert!(EQUALIZER.contains("max-height: content.preferred-height;"));
    assert!(VIEW.contains("self.follow_dialog_frame(|app| app.equalizer.window(), \"equalizer\");"));
}

// Mac Equalizer.swift:224-226: the curve keeps the gains at the moment
// its name is confirmed.
#[test]
fn save_curve_keeps_a_band_moved_while_naming() {
    let app = app_over(MockBackend::new());
    app.equalizer.invoke_save_curve();
    app.equalizer.invoke_gain_changed(3, 6.0);
    app.equalizer.invoke_curve_named("Mine".into(), true);
    let curves = app.snapshot().equalizer_curves;
    let mine = curves.iter().find(|c| c.name == "Mine").expect("saved");
    assert_eq!(mine.gains_db[3], 6.0);
}

// Mac Localization.swift:1941-1942, GuideLayout.swift:83.
#[test]
fn the_guide_opens_with_its_title_and_byline() {
    assert!(STRINGS.contains("help-byline: @tr(\"By Yasser Sutojoyo Kusumo\");"));
    let intro = HELP.find("Strings.help-intro").expect("intro");
    let title = HELP.find("text: Strings.help-title;").expect("title in the page");
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
    assert!(more < delay && Some(delay) < calibration, "delay art in More Controls");
    let classes = HELP.find("guide/classes.svg").expect("classes art");
    let cast = HELP.find("title: Strings.help-cast-title;");
    let notices = HELP.find("title: Strings.help-notices-title;");
    assert!(cast < Some(classes) && Some(classes) < notices, "classes art opens Notices");
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
    assert!(!HELP.contains("Text {"), "every text can be selected");
    assert!(HELP.contains("read-only: true;"));
    assert!(!HELP.contains("Strings.guide-close"));
}

// Mac AppDelegate+Windows.swift:866-874: the Guide reopens where it was
// left, scroll included.
#[test]
fn the_guide_reopens_where_it_was_scrolled() {
    let app = app_over(MockBackend::new());
    let window = app.help.window();
    app.main.invoke_help_requested();
    draw_frames();
    press(window, Key::End);
    let bottom = app.help.get_scrolled();
    assert!(bottom > 0.0);
    press(window, Key::Escape);
    assert!(!window.is_visible());
    app.main.invoke_help_requested();
    draw_frames();
    assert_eq!(app.help.get_scrolled(), bottom);
}

// Mac AppDelegate+Windows.swift:940-945: place and size both kept across
// launches.
#[test]
fn the_guide_keeps_its_size_across_launches() {
    let app = app_over(MockBackend::new());
    let window = app.help.window();
    app.main.invoke_help_requested();
    draw_frames();
    window.set_size(slint::LogicalSize::new(700.0, 500.0));
    app.help.invoke_close_requested();
    app.remember_view();
    let saved = app.backend.borrow().view_settings();

    let mut backend = MockBackend::new();
    backend.set_view_settings(saved);
    let again = app_over(backend);
    again.main.invoke_help_requested();
    draw_frames();
    let help = again.help.window();
    let size = help.size().to_logical(help.scale_factor());
    assert_eq!((size.width, size.height), (700.0, 500.0));
}
