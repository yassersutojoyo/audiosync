//! The tray flyout's speaker rows, header and backing against the Mac's
//! menu bar panel (MenuBar.swift, PanelColors.swift).

use std::time::Duration;

use super::App;
use super::tests::{advance_clock, app_over, draw_frames, hold_clock};
use crate::ui_model::{MockBackend, NoticeMessage};
use slint::{ComponentHandle, Model};

fn flyout() -> String {
    include_str!("../../ui/flyout.slint").replace("\r\n", "\n")
}

/// The text of component `name` in the flyout, up to the next component.
fn component(name: &str) -> String {
    let source = flyout();
    let start = source
        .find(&format!("component {name} "))
        .unwrap_or_else(|| panic!("{name}"));
    let rest = &source[start..];
    let end = ["\ncomponent ", "\nexport component "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// MenuBar.swift SpeakerBadge/ConnectingArc: an on speaker that connects is
/// a hairline grey ring with a quarter amber arc turning every 0.9 s, still
/// with motion off, and its note turns amber (Brand.warn).
#[test]
fn a_connecting_speaker_turns_an_amber_arc_and_its_note_amber() {
    let disc = component("RowDisc");
    assert!(disc.contains("in property <bool> connecting;"), "{disc}");
    assert!(disc.contains("stroke: Tokens.warn;"), "{disc}");
    assert!(disc.contains("mod(animation-tick() / 1ms, 900)"), "{disc}");
    let row = component("FlyoutRow");
    assert!(row.contains("connecting: root.connecting;"), "{row}");
    assert!(
        row.contains("color: root.connecting ? Tokens.warn : Tokens.secondary-text;"),
        "{row}"
    );
    assert_eq!(
        flyout().matches("cast-state: speaker.cast-state;").count(),
        3
    );
}

/// MenuBar.swift ScrollingTitle(text: name).font(.body): regular in every
/// state; too long, it scrolls (cut only with motion off).
#[test]
fn a_row_name_is_regular_and_scrolls_when_too_long() {
    let row = component("FlyoutRow");
    assert!(!row.contains("weight-semibold"), "{row}");
    assert!(row.contains("name-label := MarqueeText {"), "{row}");
}

/// MenuBar.swift SpeakerBadge: no hover or press scale; the teal fill
/// grows from 0.6 to full on PowerMotion's easeInOut 0.25 s.
#[test]
fn the_row_disc_has_no_hover_scale_and_grows_from_0_6_in_250_ms() {
    assert!(!component("FlyoutRow").contains("transform-scale"));
    let disc = component("RowDisc");
    assert!(
        disc.contains("duration: Motion.on ? 250ms : 150ms;"),
        "{disc}"
    );
    assert!(
        disc.contains("easing: Motion.on ? ease-in-out : linear;"),
        "{disc}"
    );
    assert!(disc.contains("(0.6 + 0.4 * root.fill)"), "{disc}");
}

/// MenuBar.swift header: the old and the new status cross-fade in one slot.
#[test]
fn the_header_status_cross_fades() {
    let source = flyout();
    assert!(source.contains("changed status => {"));
    assert!(source.contains("text: root.status-b;"));
}

/// MenuBar.swift AvailabilityNote: a small lock (needs a code, a code
/// every time) or a warning triangle leads the note.
#[test]
fn the_availability_note_leads_with_a_lock_or_a_triangle() {
    let row = component("FlyoutRow");
    assert!(row.contains("Glyphs.lock : Glyphs.warning"), "{row}");
    assert_eq!(
        flyout()
            .matches("availability: speaker.availability;")
            .count(),
        3
    );
    assert!(
        include_str!("../../ui/widgets.slint")
            .contains("out property <string> lock: \"\\u{E72E}\";")
    );
}

/// PanelColors.swift audioSyncPanelBacking: the surface light, the window
/// background dark.
#[test]
fn the_flyout_backing_is_the_window_background_on_dark() {
    assert!(flyout().contains("background: Tokens.dark ? Tokens.background : Tokens.surface;"));
}

/// MenuBar.swift SpeakerRow/PanelButton, BrandTokens.Radius: rows corner 9
/// with a 5% hover, footer buttons corner 7.
#[test]
fn rows_round_9_with_a_5_percent_hover_and_footer_buttons_7() {
    let row = component("FlyoutRow");
    assert!(row.contains("border-radius: Tokens.radius-row;"), "{row}");
    assert!(row.contains("Fluent.row-hover"), "{row}");
    assert!(component("FooterButton").contains("border-radius: Tokens.radius-control;"));
}

/// MenuBar.swift body: this computer's rows show in full above the divider,
/// and the panel's parts stand 10 apart (VStack spacing 10).
#[test]
fn this_pcs_rows_show_in_full_and_parts_stand_10_apart() {
    let source = flyout();
    assert!(!source.contains("min(this-pc-list.preferred-height, 144px)"));
    assert!(source.contains("padding-bottom: 8px;\n            spacing: 10px;"));
}

/// MenuBar.swift:1754-1760, 2098-2116 AirPlayProblemView(compact): while
/// AirPlay has stopped, a red triangle and the message sit under the speaker
/// list with an underlined Retry, in Brand.errorText; it goes with the notice.
#[test]
fn an_airplay_stop_shows_in_red_under_the_list_with_retry() {
    let mut backend = MockBackend::new();
    backend.post("airplay", NoticeMessage::AirPlayStopped, "09.40");
    let app = app_over(backend);
    assert_eq!(
        app.flyout.get_airplay_problem(),
        app.strings().get_notice_airplay_stopped()
    );
    let id = app.snapshot().notices[0].id;
    app.dismiss_notice(id);
    assert_eq!(app.flyout.get_airplay_problem(), "");

    let row = component("AirPlayProblemRow");
    assert!(row.contains("glyph: Glyphs.warning;"), "{row}");
    assert_eq!(row.matches("color: Tokens.error-text;").count(), 3, "{row}");
    assert!(row.contains("text: Strings.notice-retry;"), "{row}");
    assert!(flyout().contains("root.notice-retry-requested(-1);"));
}

/// MenuBar.swift:1961-1965 rowTransition, WindowRules.swift:857-862: a row
/// that comes or goes fades and moves 6 px over 0.35 s ease-out, 35 ms after
/// the row before it, and a leaving one stays listed until it has faded.
#[test]
fn a_speaker_leaving_the_flyout_fades_out_before_it_goes() {
    let ease = component("RowEase");
    assert!(
        ease.contains("delay: Motion.on ? root.index * 35ms : 0ms;"),
        "{ease}"
    );
    assert!(ease.contains("y: (root.shown - 1) * 6px;"), "{ease}");
    assert!(
        ease.contains("duration: Motion.on ? 350ms : 0ms;"),
        "{ease}"
    );
    assert_eq!(flyout().matches("for speaker[index] in root.").count(), 2);

    let app = app_over(MockBackend::new());
    app.rows_ease.set(true);
    app.prepare_flyout_open(true);
    app.flyout.show().expect("the flyout shows headless");
    hold_clock();
    draw_frames();
    let listed = |app: &App, id: &str| {
        app.flyout
            .get_speakers()
            .iter()
            .chain(app.flyout.get_this_pc().iter())
            .find(|row| row.id == id)
            .map(|row| row.leaving)
    };
    let id = app.flyout.get_speakers().row_data(0).expect("a speaker").id;
    app.hide_speaker(&id);
    assert_eq!(listed(&app, &id), Some(true), "it stays while it fades");
    advance_clock(Duration::from_millis(2000));
    draw_frames();
    assert_eq!(listed(&app, &id), None);

    app.hide_flyout();
    let id = app.flyout.get_speakers().row_data(0).expect("a speaker").id;
    app.hide_speaker(&id);
    assert_eq!(listed(&app, &id), None, "hidden, it goes at once");
}

/// A hidden flyout runs no timer: a long name's marquee rests.
#[test]
fn a_long_names_timer_rests_while_the_flyout_is_hidden() {
    assert!(
        include_str!("../../ui/widgets.slint").contains("running: root.live && root.excess > 0;")
    );
    assert!(component("FlyoutRow").contains("live: root.live;"));
    assert_eq!(flyout().matches("live: root.shown;").count(), 3);
}

/// The lock or warning sits 3 px before its note at the row's end, as the
/// Mac's AvailabilityNote after its Spacer (MenuBar.swift:2042, :2313-2318),
/// not at the start of the room the note may take.
#[test]
fn the_availability_icon_stays_beside_its_note() {
    let row = component("FlyoutRow");
    let at = row
        .find("// The Mac's AvailabilityNote leads with")
        .expect("the note");
    let note = &row[at..at + 900];
    assert!(note.contains("alignment: end;"), "{note}");
    let caption = &note[note.find("QuietText {").expect("the caption")..];
    assert!(!caption.contains("horizontal-stretch: 1;"), "{caption}");
}

/// The Mac panel's Speakers row puts its device volume glyph at the row's
/// right edge (its 18 Nov photos): the glyph takes no share of the row's
/// spare width.
// covers: S450
#[test]
fn a_rows_device_volume_glyph_sits_at_the_right_edge() {
    let source = flyout();
    let at = source
        .find("if root.device-volume != DeviceVolumeHint.none: Glyph {")
        .expect("the glyph");
    let glyph = &source[at..at + 300];
    assert!(glyph.contains("horizontal-stretch: 0;"), "{glyph}");
}

/// The Mac panel's source tile is a 48 pt square with only a 10 % ink edge
/// and no fill, its speaker an unresized symbol in a 22 pt frame
/// (MenuBar.swift:2372-2383, :2509-2522), 17 pt wide in its photos.
// covers: S450
#[test]
fn the_source_tile_is_the_macs_unfilled_square() {
    let source = flyout();
    let at = source.find("cover-tile := Rectangle {").expect("the tile");
    let tile = &source[at..at + 2000];
    assert!(
        !tile.contains("background: Tokens.text.with-alpha(0.06);"),
        "{tile}"
    );
    let glyph = &tile[tile.find("Glyphs.volume-high;").expect("the glyph")..];
    assert!(glyph.contains("font-size: 17px;"), "{glyph}");
}

/// The Mac panel's "Looking for speakers…" is its text alone, flush left
/// (MenuBar.swift:1857-1867).
// covers: S450
#[test]
fn looking_for_speakers_has_no_dot_before_it() {
    assert!(!flyout().contains("a dot that beats"));
}

/// The Mac panel ends 8 pt under its buttons, its line 8 under the rows
/// (MenuBar.swift:1765, :1779), and its brand row has no inset above.
// covers: S450
#[test]
fn the_flyout_is_as_tall_as_the_macs_panel() {
    let source = flyout();
    assert!(source.contains("fit-height: root.footer-bottom + 8px;"));
    assert!(source.contains("padding-top: -2px;\n\n                Divider { }"));
    let brand = &source[source.find("brand := Rectangle {").expect("brand")..];
    let brand = &brand[..brand.find("brand-mark := BrandMark").expect("mark")];
    assert!(!brand.contains("padding-top: 2px;"), "{brand}");
}

/// The text of the flyout from `start` up to the first `end` after it.
fn between(start: &str, end: &str) -> String {
    let source = flyout();
    let from = &source[source.find(start).unwrap_or_else(|| panic!("{start}"))..];
    from[..from.find(end).unwrap_or_else(|| panic!("{end}"))].to_owned()
}

/// MenuBar.swift:2186: a speaker that is on and waits gets a 2.5 pt ring in
/// Brand.tealText (mint on dark, deep teal on light), not the brand teal.
#[test]
fn a_waiting_speakers_ring_is_the_text_teal() {
    let disc = component("RowDisc");
    assert!(
        disc.contains(
            "border-color: root.on && !root.connecting ? Tokens.teal-text : Tokens.secondary-text;"
        ),
        "{disc}"
    );
}

/// MenuBar.swift:1779 and :1803-1815: the panel's 14 pt padding puts the
/// mark 14 pt from the edge, and the status under the wordmark is
/// .subheadline (11 pt), 1 pt under it.
#[test]
fn the_flyout_header_sits_as_the_macs() {
    let header = between("header-part := IntroPart {", "brand := Rectangle {");
    assert!(header.contains("padding-left: -6px;"), "{header}");
    let words = between("brand-mark := BrandMark {", "FocusRing {");
    assert!(words.contains("spacing: 1px;"), "{words}");
    assert_eq!(
        words
            .matches("font-size: Tokens.font-size-subheadline;")
            .count(),
        2,
        "{words}"
    );
}

/// MenuBar.swift:1766-1773 and :2091: the footer row is padded -6 pt each
/// side and each button pads its text 6 pt, so the words sit on the
/// panel's 14 pt edge.
#[test]
fn the_flyout_footer_words_sit_on_the_panels_edge() {
    let footer = between("footer := IntroPart {", "FooterButton {");
    assert!(footer.contains("padding-left: -6px;"), "{footer}");
    assert!(footer.contains("padding-right: -6px;"), "{footer}");
}

/// MenuBar.swift:1868 and WindowRules.swift:1137: the empty list's note
/// starts dividerToList (-3 pt) plus the 10 pt spacing under the line,
/// padded 2 pt above and below.
#[test]
fn the_empty_lists_note_sits_under_the_line_as_the_macs() {
    let note = between("none-note := HorizontalLayout {", "SecondaryText {");
    assert!(note.contains("padding-top: -4px;"), "{note}");
    assert!(note.contains("padding-bottom: 2px;"), "{note}");
}

/// MenuBar.swift:1833-1846 and WindowRules.swift:1113, :1139, :1595: the
/// speaker in its 26 pt icon box, 9 pt to the track, 9 pt to the 25 pt
/// number and 15 pt from the edge. The 15 pt symbol (MainWindow.swift:1177)
/// inks 19 x 13.5 pt in TRAY-IDLE-terang-mac.png, the 16 px glyph 14.5 x 11.5.
#[test]
fn the_flyout_volume_row_sits_as_the_macs() {
    let row = between("MasterVolume {", "mute-toggled =>");
    for want in [
        "button-size: 26px;",
        "row-height: 32px;",
        "glyph-size: 20px;",
        "spacing: 1px;",
        "number-gap: 17px;",
        "padding-right: 15px;",
    ] {
        assert!(row.contains(want), "{want}\n{row}");
    }
}

/// MenuBar.swift:2797-2834: the panel's notice is HStack(spacing: 8) { icon,
/// text, Spacer(minLength: 0), dismiss }, which wraps the text in
/// TRAY-NOTICE-*-mac.png after "streaming", a column of 150 pt.
#[test]
fn the_tray_notice_wraps_in_the_macs_column() {
    let source = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let stack = &source[source
        .find("export component NoticeStack ")
        .expect("NoticeStack")..];
    assert!(
        stack.contains("max-width: root.compact ? 150px : 100000px;"),
        "the text column"
    );
    assert!(
        stack.contains("if root.compact: Rectangle { }"),
        "the spacer"
    );
}
