//! The notices, the speaker list's notes and In zones block, the options
//! glyph, Master Volume's number and the equalizer's switch, laid out as
//! the Mac's.

/// The text of component `name` in `source`, up to the next component.
fn component(source: &str, name: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let start = ["component", "global"]
        .iter()
        .find_map(|kind| source.find(&format!("{kind} {name} ")))
        .unwrap_or_else(|| panic!("{name}"));
    let rest = &source[start..];
    let end = ["\ncomponent ", "\nexport component ", "\nexport global "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

fn notice_stack() -> String {
    component(include_str!("../../ui/widgets.slint"), "NoticeStack")
}

/// The opening line of the element that sets `property` in `body`.
fn element_of<'a>(body: &'a str, property: &str) -> &'a str {
    let at = body.find(property).unwrap_or_else(|| panic!("{property}"));
    body[..at]
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.ends_with('{'))
        .expect("an element")
}

#[test]
fn a_warning_notice_is_the_macs_orange_box_and_an_info_notice_teal() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(
        fluent.contains("out property <color> warn-background: Tokens.dark ? #3a2c12 : #fff4dd;")
    );
    let stack = notice_stack();
    assert!(!stack.contains("background: Tokens.notice;"));
    assert!(stack.contains(
        "background: root.compact ? transparent : notice.info ? Tokens.teal.with-alpha(0.12) : Fluent.warn-background;"
    ));
    assert!(stack.contains(
        "border-color: root.compact || notice.info ? transparent : Tokens.warn.with-alpha(0.28);"
    ));
    assert!(stack.contains("border-radius: Tokens.radius-card;"));
}

#[test]
fn a_notices_icon_is_warn_orange_or_teal() {
    assert!(notice_stack().contains("color: notice.info ? Tokens.teal-text : Tokens.warn;"));
}

#[test]
fn a_notice_is_padded_10_with_8_to_its_icon() {
    let stack = notice_stack();
    assert!(stack.contains("padding: root.compact ? 0px : 10px;"));
    assert!(!stack.contains("padding-right: 8px;"));
    assert!(!stack.contains("spacing: 12px;"));
}

#[test]
fn a_notice_with_a_heading_shows_it_semibold_over_its_text() {
    let types = include_str!("../../ui/types.slint").replace("\r\n", "\n");
    assert!(types.contains("    heading: string,\n"));
    let stack = notice_stack();
    let at = stack
        .find("text: notice.heading;")
        .expect("the heading line");
    let heading = &stack[at..at + 200];
    assert!(heading.contains("font-weight: Fluent.weight-semibold;"));
    assert!(
        stack
            .find("text: notice.message;")
            .is_some_and(|message| message > at)
    );
}

#[test]
fn a_notices_actions_and_dismiss_all_are_links_in_the_text_color() {
    let stack = notice_stack();
    for text in [
        "text: Strings.notice-show-how;",
        "text: Strings.notice-retry;",
        "text: Strings.notice-open-privacy;",
        "text: Strings.dismiss-all-notices;",
    ] {
        assert!(element_of(&stack, text).ends_with("LinkButton {"), "{text}");
    }
    assert_eq!(stack.matches("text-color: Tokens.text;").count(), 4);
    let link = component(include_str!("../../ui/widgets.slint"), "LinkButton");
    assert!(link.contains("in property <color> text-color: Tokens.teal-text;"));
    assert!(link.contains("color: root.text-color;"));
}

#[test]
fn a_notice_opens_and_closes_on_the_macs_320_ms_with_no_slide() {
    let stack = notice_stack();
    let lines: Vec<&str> = stack.lines().map(str::trim).collect();
    let after = |head: &str| {
        let at = lines
            .iter()
            .position(|line| *line == head)
            .unwrap_or_else(|| panic!("{head}"));
        lines[at + 1..at + 4].to_vec()
    };
    assert_eq!(
        after("animate open {"),
        [
            "duration: Motion.on ? 240ms : 0ms;",
            "delay: Motion.on && self.closing ? 80ms : 0ms;",
            "easing: cubic-bezier(0.25, 0, 0.75, 1);",
        ]
    );
    assert_eq!(
        after("animate shown {"),
        [
            "duration: Motion.on ? (self.closing ? 160ms : 180ms) : 150ms;",
            "delay: Motion.on && !self.closing ? 60ms : 0ms;",
            "easing: cubic-bezier(0.25, 0, 0.75, 1);",
        ]
    );
    assert!(!stack.contains("6px * (1 - slot.shown)"), "no slide");
    let view = include_str!("../view.rs").replace("\r\n", "\n");
    assert!(
        view.contains("get_on() {\n            320\n"),
        "dismissed after the room shuts"
    );
}

#[test]
fn the_tray_notice_is_text_on_the_panel_with_link_actions() {
    let flyout = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    for text in [
        "text: Strings.more-notices(",
        "text: Strings.dismiss-all-notices;",
    ] {
        assert!(
            element_of(&flyout, text).ends_with("LinkButton {"),
            "{text}"
        );
    }
}

#[test]
fn add_by_address_is_a_small_bordered_button_on_the_badge_column() {
    let widgets = include_str!("../../ui/widgets.slint");
    assert!(widgets.contains("export component SmallButton "));
    let small = component(include_str!("../../ui/widgets.slint"), "SmallButton");
    assert!(small.contains("height: 24px;"));
    assert!(small.contains("border-color: Tokens.teal.with-alpha(0.3);"));
    let section = component(include_str!("../../ui/main.slint"), "SpeakerSection");
    assert!(element_of(&section, "text: Strings.cast-add-address;").ends_with("SmallButton {"));
    let at = section.find("text: Strings.cast-add-address;").unwrap();
    let before = &section[at.saturating_sub(300)..at];
    assert!(before.contains("padding-left: Fluent.row-padding-x;"));
    // 10 px under the line, as the Mac's (MainWindow.swift:664-665 on the rows-tidy commit).
    assert!(before.contains("padding-top: 10px;") && before.contains("padding-bottom: 6px;"));
}

#[test]
fn open_network_settings_sits_in_the_notes_padding_8_under_its_text() {
    let section = component(include_str!("../../ui/main.slint"), "SpeakerSection");
    assert!(
        element_of(&section, "text: Strings.open-network-settings;").ends_with("SmallButton {")
    );
    let note = section.find("text: root.note;").expect("the note");
    let before = &section[note.saturating_sub(200)..note];
    assert!(before.contains("padding: 10px;") && before.contains("spacing: 8px;"));
    assert!(!section.contains("padding-bottom: 12px;"));
}

#[test]
fn the_options_button_is_a_circled_ellipsis_on_every_system() {
    let glyphs = component(include_str!("../../ui/widgets.slint"), "Glyphs");
    let drawn = glyphs
        .find("public pure function drawn-everywhere(")
        .map(|at| &glyphs[at..at + 200])
        .expect("drawn-everywhere");
    assert!(drawn.contains("glyph == root.more"));
    let outline = glyphs
        .find("public pure function outline(")
        .map(|at| &glyphs[at..])
        .expect("outline");
    assert!(outline.contains("if glyph == root.more {"), "the circle");
}

#[test]
fn master_volumes_number_keeps_1_px_inside_the_rows_edge() {
    let window = component(include_str!("../../ui/main.slint"), "MainWindow");
    let at = window.find("MasterVolume {").expect("Master Volume");
    assert!(window[at..at + 200].contains("padding-right: 1px;"));
}

#[test]
fn the_equalizer_switch_reads_equalizer_as_a_headline() {
    let switch = component(include_str!("../../ui/widgets.slint"), "ToggleSwitch");
    assert!(switch.contains("in property <bool> headline;"));
    assert!(
        switch.contains(
            "font-weight: root.headline ? Fluent.weight-semibold : Fluent.weight-regular;"
        )
    );
    let eq = include_str!("../../ui/equalizer.slint");
    let at = eq.find("power := ToggleSwitch {").expect("the switch");
    assert!(eq[at..at + 200].contains("headline: true;"));
}

#[test]
fn in_zones_has_a_rule_that_turns_down_into_each_zones_bar() {
    let section = component(include_str!("../../ui/main.slint"), "SpeakerSection");
    let at = section
        .find("text: Strings.in-zones.to-uppercase();")
        .expect("the label");
    let before = &section[at.saturating_sub(600)..at];
    assert!(before.contains("padding-top: 9px;") && before.contains("padding-bottom: 5px;"));
    assert!(section.contains("border-top-left-radius: 6px;"), "the join");
    assert!(
        section.contains("root.zoned[index - 1].zone-id != speaker.zone-id"),
        "a gap per zone"
    );
    assert!(section.contains("height: 4px;"));
    let row = component(include_str!("../../ui/widgets.slint"), "ZoneMemberRow");
    assert!(row.contains("// The zone's bar on the group's left edge."));
}

#[test]
fn a_click_leaves_the_small_button_without_a_focus_ring() {
    let small = component(include_str!("../../ui/widgets.slint"), "SmallButton");
    assert!(small.contains("focus-on-click: false;"));
}

#[test]
fn narrator_reads_a_notices_heading_before_its_text() {
    let stack = notice_stack();
    assert!(
        stack.contains("accessible-label: Strings.notice-spoken(notice.heading, notice.message);")
    );
    let strings = include_str!("../../ui/strings.slint");
    assert!(strings.contains("return @tr(\"{}. {}\", heading, message);"));
}

/// SettingsWindow.swift:184-186 is a native segmented control: on the Mac's
/// 2x MAIN-SETTINGS photos a filled 24 pt track (#ebebeb light, #565656
/// dark) with no edge, and a 1 x 14 pt divider between two segments that
/// are not chosen (#b4b4b4 light, #494949 dark).
#[test]
fn the_appearance_control_is_a_filled_track_with_dividers() {
    let settings = include_str!("../../ui/settings.slint").replace("\r\n", "\n");
    let at = settings
        .find("accessible-label: Strings.appearance;")
        .expect("the track");
    let track = &settings[settings[..at].rfind("Rectangle {").expect("track")..];
    let track = &track[..track
        .find("\n                        Divider { }")
        .expect("end")];
    assert!(
        track.contains("background: Tokens.dark ? #565656 : #ebebeb;"),
        "{track}"
    );
    assert!(!track.contains("border-width"), "{track}");
    assert!(!track.contains("drop-shadow"), "{track}");
    assert_eq!(track.matches("SegmentDivider {").count(), 2, "{track}");
    let divider = component(&settings, "SegmentDivider");
    assert!(divider.contains("height: 14px;"), "{divider}");
    assert!(
        divider.contains("background: Tokens.dark ? #494949 : #b4b4b4;"),
        "{divider}"
    );
}

/// EqualizerWindow.swift:251 and :261 set a band's gain and label in
/// .caption2, which macOS draws at 10 pt medium; Segoe sets 560 as its
/// regular, so the semibold face carries the Mac's ink (EQ-WINDOW photos).
/// :168 makes the preamp's divider 170 pt, at the bottom of the bands.
#[test]
fn the_equalizer_band_text_is_semibold_and_its_divider_170_tall() {
    let eq = include_str!("../../ui/equalizer.slint").replace("\r\n", "\n");
    let text = component(&eq, "BandText");
    assert!(
        text.contains("font-size: Tokens.font-size-caption2;"),
        "{text}"
    );
    assert!(
        text.contains("font-weight: Fluent.weight-semibold;"),
        "{text}"
    );
    let band = component(&eq, "Band");
    assert_eq!(band.matches("BandText {").count(), 2, "{band}");
    assert!(!band.contains("QuietText {"), "{band}");
    let divider = &eq[eq.find("root.preamp-changed(value);").expect("preamp")..];
    let divider = &divider[..divider.find("for gain[index]").expect("bands")];
    assert!(divider.contains("height: 170px;"), "{divider}");
    assert!(!divider.contains("padding-top: 8px;"), "{divider}");
}
