//! The device-volume hint, the notices, the source switch after a quit and
//! the rename field against the Mac's (DeviceVolumeViews.swift,
//! NoticeMotion.swift, MenuBar.swift NoticeStack, Localization.swift,
//! MainWindow.swift).

use super::notice_item;
use super::tests::app_over;
use crate::ui_model::MockBackend;

/// The text of component `name` in `source`, up to the next component.
fn component(source: &str, name: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let start = source
        .find(&format!("component {name} "))
        .unwrap_or_else(|| panic!("{name}"));
    let rest = &source[start..];
    let end = ["\ncomponent ", "\nexport component ", "\nexport global "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// The part of `source` from `from` up to `to`.
fn between(source: &str, from: &str, to: &str) -> String {
    let start = source.find(from).unwrap_or_else(|| panic!("{from}"));
    let rest = &source[start..];
    let end = rest.find(to).unwrap_or_else(|| panic!("{to}"));
    rest[..end].to_owned()
}

#[test]
fn the_device_volume_hint_explains_itself_in_a_tooltip() {
    let row = component(include_str!("../../ui/widgets.slint"), "SpeakerRow");
    let hint = between(&row, "hint-line := HorizontalLayout {", "fix-button :=");
    // DeviceVolumeViews.swift:26 .help(shown.help)
    assert!(hint.contains("Tooltip {"), "{hint}");
    assert!(hint.contains("text: Strings.device-volume-help;"), "{hint}");
    let strings = include_str!("../../ui/strings.slint");
    assert!(strings.contains(
        "\"Windows has its own volume for each output, and it limits AudioSync's slider.\""
    ));
    assert!(strings.contains(
        "\"PipeWire has its own volume for each output, and it limits AudioSync's slider.\""
    ));
}

#[test]
fn the_tray_rows_device_volume_glyph_shows_the_hint_on_hover() {
    let row = component(include_str!("../../ui/flyout.slint"), "FlyoutRow");
    let glyph = between(
        &row,
        "if root.device-volume != DeviceVolumeHint.none:",
        "FocusRing {",
    );
    // DeviceVolumeViews.swift:57 .help(glyph.tooltip), the hint's own text.
    assert!(glyph.contains("Tooltip {"), "{glyph}");
    assert!(glyph.contains("text: root.device-volume-tip;"), "{glyph}");
    let flyout = include_str!("../../ui/flyout.slint");
    assert_eq!(
        flyout
            .matches("device-volume-tip: Strings.device-volume-hint(speaker.device-volume, speaker.device-volume-percent);")
            .count(),
        3,
        "every speaker row passes it"
    );
}

#[test]
fn dismiss_all_comes_and_goes_on_the_notices_own_motion() {
    let stack = component(include_str!("../../ui/widgets.slint"), "NoticeStack");
    let all = &stack[stack.find("all-slot := Rectangle {").expect("the row")..];
    // MenuBar.swift:2760 gives the slot the notices' NoticeReveal.
    for line in [
        "delay: Motion.on && self.closing ? 80ms : 0ms;",
        "duration: Motion.on ? (self.closing ? 160ms : 180ms) : 150ms;",
        "opacity: all-slot.shown;",
    ] {
        assert!(all.contains(line), "{line}\n{all}");
    }
    assert!(!all.contains("cubic-bezier(0.2, 0.8, 0.2, 1)"), "{all}");
}

#[test]
fn the_preset_notice_has_the_macs_heading() {
    let app = app_over(MockBackend::new());
    app.update(|b| b.set_speaker_enabled("local:desk", true));
    app.save_preset("Evening");
    app.update(|b| b.hide_speaker("local:desk", "USB"));
    let id = super::tests::preset_ids(&app)[0].clone();
    app.recall_preset(&id);
    let notices = app.snapshot().notices;
    // AppDelegate+Presets.swift:150, Localization.swift:1365.
    assert_eq!(
        notice_item(&app.strings(), &notices[0]).heading,
        "Some speakers stayed off"
    );
}

#[test]
fn the_tray_notices_footer_sits_under_the_notice_text() {
    let flyout = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    let part = between(&flyout, "NoticeStack {", "text: Strings.more-notices(");
    // MenuBar.swift:2804-2833: the footer is in the banner's text column,
    // 6 pt under the text, past the glyph (18 + 8) and short of the X (16 + 8).
    assert!(!part.contains("IntroPart"), "{part}");
    assert!(part.contains("padding-left: 26px;"), "{part}");
    assert!(part.contains("padding-right: 24px;"), "{part}");
    let column = between(
        &flyout,
        "if root.notices.length > 0: IntroPart {",
        "NoticeStack {",
    );
    assert!(column.contains("spacing: 6px;"), "{column}");
}
