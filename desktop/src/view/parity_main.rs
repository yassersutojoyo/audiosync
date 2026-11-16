//! The main window against the Mac's: the transport's filled glyphs, the
//! Calibrate All row with streaming off, the speaker rows, the source block,
//! the open Presets list and the text font.

use super::tests::app_over;
use super::{AllPhase, calibrate_all_card};
use crate::audio::calibration::{Failure, Missing};
use crate::ui_model::MockBackend;

// covers: S450
#[test]
fn with_streaming_off_calibrate_all_is_only_dimmed_as_on_the_mac() {
    let app = app_over(MockBackend::new());
    let strings = app.strings();
    let mut state = app.snapshot();
    state.calibrate_all.phase = AllPhase::Idle;
    state.calibrate_all.other_running = false;
    let mut card = |blocked| {
        state.calibrate_all.blocked = blocked;
        calibrate_all_card(&strings, &state)
    };
    let off = card(Some(Failure::NotPlaying(Missing::STREAMING)));
    assert!(!off.can_start, "the row is dimmed");
    assert!(off.status_is_hint, "no line under it, only its tooltip");
    assert_eq!(off.status, "Turn on streaming to calibrate.");
    let reference = card(Some(Failure::NotPlaying(Missing::REFERENCE)));
    assert!(!reference.status_is_hint, "another reason still shows");
}

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

/// The line after `if glyph == root.<glyph> {` in `function`.
fn drawing(glyphs: &str, function: &str, glyph: &str) -> String {
    let body = &glyphs[glyphs
        .find(&format!("public pure function {function}("))
        .unwrap_or_else(|| panic!("{function}"))..];
    let at = body
        .find(&format!("if glyph == root.{glyph} {{"))
        .unwrap_or_else(|| panic!("{function} {glyph}"));
    body[at..]
        .lines()
        .nth(1)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

// covers: S450
#[test]
fn the_transport_draws_the_macs_filled_glyphs_on_every_system() {
    let glyphs = component(include_str!("../../ui/widgets.slint"), "Glyphs");
    let drawn = glyphs
        .find("public pure function drawn-everywhere(")
        .map(|at| &glyphs[at..at + 400])
        .expect("drawn-everywhere");
    let outline = &glyphs[glyphs
        .find("public pure function outline(")
        .expect("outline")
        ..glyphs.find("public pure function solid(").expect("solid")];
    for glyph in ["previous", "next", "play", "pause"] {
        assert!(drawn.contains(&format!("glyph == root.{glyph}")), "{glyph}");
        assert!(
            !outline.contains(&format!("if glyph == root.{glyph} {{")),
            "{glyph} has no stroked part"
        );
    }
    // Two triangles each, nose to tail, across the 16-unit box.
    assert_eq!(
        drawing(&glyphs, "solid", "previous"),
        r#"return "M8 3.45 L0 8 L8 12.55 Z M16 3.45 L8 8 L16 12.55 Z";"#
    );
    assert_eq!(
        drawing(&glyphs, "solid", "next"),
        r#"return "M0 3.45 L8 8 L0 12.55 Z M8 3.45 L16 8 L8 12.55 Z";"#
    );
    let card = component(include_str!("../../ui/main.slint"), "NowPlayingCard");
    for glyph in ["Glyphs.previous;", "Glyphs.next;"] {
        let at = card.find(&format!("glyph: {glyph}")).expect(glyph);
        assert!(card[at..at + 80].contains("glyph-size: 21px;"), "{glyph}");
    }
}

/// A speaker's name is regular whether it plays or not, as the Mac's; the
/// round mark says it plays.
// covers: S450
#[test]
fn a_playing_speakers_name_keeps_the_regular_weight() {
    let widgets = include_str!("../../ui/widgets.slint");
    for row in ["SpeakerRow", "ZoneMemberRow"] {
        let body = component(widgets, row);
        let at = body.find("text: root.item.name;").expect(row);
        assert!(body[at..at + 200].contains("font-weight: 400;"), "{row}");
        assert!(!body.contains("root.item.plays ? 600"), "{row}");
    }
}

/// The block of what plays and the source is visibly grey on the white
/// window, as the Mac's on its own.
// covers: S450
#[test]
fn the_source_block_is_visibly_grey() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains("out property <brush> hero: Tokens.text.with-alpha(0.06);"));
}

/// At the smallest window with a song and the Presets list open, the source
/// drop-down at the top of the left column stays in view.
// covers: S450
#[test]
fn the_source_stays_in_view_with_presets_open_in_the_smallest_window() {
    use crate::now_playing_card::fake::{FakeControls, track};
    use crate::now_playing_card::{PlayModes, Repeat};
    use i_slint_backend_testing::ElementHandle;
    use slint::ComponentHandle;
    let app = app_over(MockBackend::new());
    let fake = FakeControls::default();
    app.set_media_controls(fake.controls());
    app.main.show().expect("the main window shows headless");
    app.poll();
    let mut song = track("Spotify.exe", "Song", "Band", true);
    song.modes = PlayModes {
        shuffle: Some(false),
        repeat: Some(Repeat::Off),
    };
    fake.read(Some(song));
    app.poll();
    let top = |id: &str| {
        ElementHandle::find_by_element_id(&app.main, id)
            .next()
            .map(|element| element.absolute_position().y)
            .unwrap_or_else(|| panic!("{id}"))
    };
    app.main.global::<crate::Motion>().set_on(true);
    for open in [false, true] {
        app.main.set_presets_open(open);
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            super::tests::draw_frames();
        }
        app.main
            .window()
            .set_size(slint::LogicalSize::new(712.0, 680.0));
        slint::platform::update_timers_and_animations();
        let column = top("MainWindow::left-column");
        let source = top("MainWindow::source");
        assert!(
            source >= column,
            "open {open}: the source at {source}, the column at {column}"
        );
    }
}

/// Text is set in the system font on Windows, as the Mac sets its own, and
/// in the bundled Inter elsewhere; every window takes it from Fluent.
// covers: S450
#[test]
fn text_is_segoe_ui_variable_on_windows_and_inter_elsewhere() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains(
        r#"out property <string> font: Platform.os == OperatingSystemType.windows ? "Segoe UI Variable Text" : "Inter";"#
    ));
    for (name, source) in [
        ("main", include_str!("../../ui/main.slint")),
        ("flyout", include_str!("../../ui/flyout.slint")),
        ("equalizer", include_str!("../../ui/equalizer.slint")),
        ("settings", include_str!("../../ui/settings.slint")),
    ] {
        assert!(
            source.contains("default-font-family: Fluent.font;"),
            "{name}"
        );
        assert!(!source.contains("font-family: \"Inter\""), "{name}");
    }
}

/// In the open Presets list with none saved, No Presets, the caption and
/// Save as Preset… start on the Presets label's column, as the Mac's
/// PresetListLayout puts them.
// covers: S450
#[test]
fn the_empty_presets_list_starts_on_the_presets_labels_column() {
    use i_slint_backend_testing::ElementHandle;
    use slint::ComponentHandle;
    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    app.main.set_presets_open(true);
    super::tests::draw_frames();
    let x = |id: &str| {
        ElementHandle::find_by_element_id(&app.main, id)
            .next()
            .map(|element| element.absolute_position().x)
            .unwrap_or_else(|| panic!("{id}"))
    };
    let label = x("PresetsCard::label");
    assert_eq!(
        [
            x("PresetsCard::name"),
            x("PresetsCard::empty-note"),
            x("PresetsCard::save-label"),
        ],
        [label; 3],
        "No Presets, the caption and Save as Preset… under Presets"
    );
}

/// At 712 x 680 with a song, Windows' larger text overlaps no other text:
/// every line stays inside its row.
// covers: S450
#[test]
fn no_text_overlaps_another_in_the_default_window() {
    use crate::now_playing_card::fake::{FakeControls, track};
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    let app = app_over(MockBackend::new());
    let fake = FakeControls::default();
    app.set_media_controls(fake.controls());
    app.main.show().expect("the main window shows headless");
    app.poll();
    fake.read(Some(track("Spotify.exe", "World Is Mine", "Lane 8", true)));
    app.poll();
    super::tests::draw_frames();
    app.main
        .window()
        .set_size(slint::LogicalSize::new(712.0, 680.0));
    slint::platform::update_timers_and_animations();
    let lines: Vec<(f32, f32, f32, f32)> = ElementQuery::from_root(&app.main)
        .match_descendants()
        .match_inherits("Text")
        .find_all()
        .into_iter()
        .map(|text| {
            let (at, size) = (text.absolute_position(), text.size());
            (at.x, at.y, at.x + size.width, at.y + size.height)
        })
        .filter(|&(x, y, right, bottom)| right > x && bottom > y && y < 680.0 && x < 712.0)
        .collect();
    assert!(lines.len() > 20, "{lines:?}");
    for (n, a) in lines.iter().enumerate() {
        for b in &lines[n + 1..] {
            // Under a pixel is the wordmark's fractional line, not a clash.
            let apart =
                a.2 <= b.0 + 1.0 || b.2 <= a.0 + 1.0 || a.3 <= b.1 + 1.0 || b.3 <= a.1 + 1.0;
            // Icons stack glyphs in one box.
            let inside = |a: &(f32, f32, f32, f32), b: &(f32, f32, f32, f32)| {
                a.0 >= b.0 && a.1 >= b.1 && a.2 <= b.2 && a.3 <= b.3
            };
            assert!(
                apart || inside(a, b) || inside(b, a),
                "{a:?} overlaps {b:?}"
            );
        }
    }
}
