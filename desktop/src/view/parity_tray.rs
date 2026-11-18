//! The tray flyout's speaker rows, header and backing against the Mac's
//! menu bar panel (MenuBar.swift, PanelColors.swift).

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
    assert_eq!(flyout().matches("cast-state: speaker.cast-state;").count(), 3);
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
    assert!(disc.contains("duration: Motion.on ? 250ms : 150ms;"), "{disc}");
    assert!(disc.contains("easing: Motion.on ? ease-in-out : linear;"), "{disc}");
    assert!(disc.contains("(0.6 + 0.4 * root.fill)"), "{disc}");
}

/// MenuBar.swift header: the old and the new status cross-fade in one slot.
#[test]
fn the_header_status_cross_fades() {
    let source = flyout();
    assert!(source.contains("changed status => {"));
    assert!(source.contains("text: root.old-status;"));
}

/// MenuBar.swift AvailabilityNote: a small lock (needs a code, a code
/// every time) or a warning triangle leads the note.
#[test]
fn the_availability_note_leads_with_a_lock_or_a_triangle() {
    let row = component("FlyoutRow");
    assert!(row.contains("Glyphs.lock : Glyphs.warning"), "{row}");
    assert_eq!(flyout().matches("availability: speaker.availability;").count(), 3);
    assert!(include_str!("../../ui/widgets.slint").contains("out property <string> lock: \"\\u{E72E}\";"));
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
