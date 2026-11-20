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

/// The pointer on Previous, Play or Pause, or Next draws no box behind it:
/// the Mac's transport buttons are `.plain` (MainWindow.swift:504).
// covers: S450
#[test]
fn the_transport_takes_no_hover_box_as_on_the_mac() {
    use crate::now_playing_card::fake::{FakeControls, track};
    use i_slint_backend_testing::ElementHandle;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;
    let app = app_over(MockBackend::new());
    let fake = FakeControls::default();
    app.set_media_controls(fake.controls());
    app.main.show().expect("the main window shows headless");
    app.poll();
    fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
    app.poll();
    super::tests::draw_frames();
    let window = app.main.window();
    window.set_size(slint::LogicalSize::new(776.0, 680.0));
    slint::platform::update_timers_and_animations();
    let corner = |x: f32, y: f32| {
        let shot = window.take_snapshot().expect("drawn");
        let scale = window.scale_factor();
        let at = (((y + 3.0) * scale) as usize * shot.width() as usize
            + ((x + 3.0) * scale) as usize)
            * 4;
        <[u8; 4]>::try_from(&shot.as_bytes()[at..at + 4]).expect("a pixel")
    };
    for label in ["Previous Track", "Pause", "Next Track"] {
        let button = ElementHandle::find_by_accessible_label(&app.main, label)
            .next()
            .unwrap_or_else(|| panic!("{label}"));
        let (at, size) = (button.absolute_position(), button.size());
        let away = slint::LogicalPosition::new(2.0, 2.0);
        window.dispatch_event(WindowEvent::PointerMoved { position: away });
        super::tests::advance_clock(std::time::Duration::from_secs(1));
        super::tests::draw_frames();
        let before = corner(at.x, at.y);
        let over = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
        window.dispatch_event(WindowEvent::PointerMoved { position: over });
        super::tests::advance_clock(std::time::Duration::from_secs(1));
        super::tests::draw_frames();
        assert_eq!(corner(at.x, at.y), before, "{label} drew a hover box");
    }
}

/// Auto-connect's bolt is solid on every system, as the Mac's bolt.fill
/// (MainWindow.swift:2435), where the icon font's is an outline.
#[test]
fn the_auto_connect_bolt_is_solid_on_every_system() {
    let glyphs = component(include_str!("../../ui/widgets.slint"), "Glyphs");
    let drawn = &glyphs[glyphs
        .find("public pure function drawn-everywhere(")
        .expect("drawn-everywhere")..];
    assert!(drawn[..drawn.find('}').expect("its end")].contains("glyph == root.auto-connect"));
    let outline = &glyphs[glyphs
        .find("public pure function outline(")
        .expect("outline")
        ..glyphs.find("public pure function solid(").expect("solid")];
    assert!(
        !outline.contains("if glyph == root.auto-connect {"),
        "no stroked part"
    );
    assert_eq!(
        drawing(&glyphs, "solid", "auto-connect"),
        r#"return "M9.5 1.5 L3.5 9 H8 L6.5 14.5 L12.5 7 H8 Z";"#
    );
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
        assert!(
            body[at..at + 200].contains("font-weight: Fluent.weight-regular;"),
            "{row}"
        );
        assert!(
            !body.contains("root.item.plays ? Fluent.weight-semibold"),
            "{row}"
        );
    }
}

/// The block of what plays and the source is visibly grey on the white
/// window, as the Mac's on its own.
// covers: S450
#[test]
fn the_source_block_is_visibly_grey() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains("out property <brush> hero: Tokens.text.with-alpha(0.05);"));
}

/// Text takes the Mac's text style sizes 1:1 on every OS (BrandTokens.swift
/// Typography, the macOS styles), with no larger Windows scale.
// covers: S450
#[test]
fn text_sizes_are_the_macs_on_every_os() {
    let tokens = include_str!("../../ui/tokens.slint");
    for (style, size) in [
        ("title2", 17),
        ("title3", 15),
        ("headline", 13),
        ("body", 13),
        ("callout", 12),
        ("subheadline", 11),
        ("caption", 10),
        ("caption2", 10),
    ] {
        assert!(
            tokens.contains(&format!(
                "out property <length> font-size-{style}: {size}px;"
            )),
            "{style} {size}"
        );
    }
}

/// Cards round at the Mac's 12 (BrandTokens.swift Radius.card).
// covers: S450
#[test]
fn cards_take_the_macs_corner() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains("out property <length> radius-card: 12px;"));
}

/// The main window is white in light and the Mac's system window color in
/// dark (MainWindow.swift:32-36), #1e1e1e in the dark Mac photos of 18 Nov
/// 22.24, so its cards (5 % ink over it) come out at the Mac's #282828.
// covers: S450
#[test]
fn the_main_window_is_the_macs_window_color() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains("out property <color> main-window: Tokens.dark ? #1e1e1e : #ffffff;"));
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    assert!(
        main.contains("    background: Fluent.main-window;\n    default-font-family: Fluent.font;")
    );
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

/// Shrinking the window to its smallest with a song, the Presets list open
/// and the focus on the Presets row leaves the left column where it was, as
/// the Mac's scroll view does: the source drop-down stays in view.
// covers: S450
#[test]
fn shrinking_the_window_keeps_the_source_in_view_with_presets_open() {
    use crate::now_playing_card::fake::{FakeControls, track};
    use i_slint_backend_testing::ElementHandle;
    use slint::ComponentHandle;
    let app = app_over(MockBackend::new());
    let fake = FakeControls::default();
    app.set_media_controls(fake.controls());
    app.main.show().expect("the main window shows headless");
    app.poll();
    fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
    app.poll();
    app.main.set_presets_open(true);
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::tests::draw_frames();
    }
    app.main.set_focus_presets(true);
    slint::platform::update_timers_and_animations();
    app.main
        .window()
        .set_size(slint::LogicalSize::new(696.0, 440.0));
    slint::platform::update_timers_and_animations();
    let top = |id: &str| {
        ElementHandle::find_by_element_id(&app.main, id)
            .next()
            .map(|element| element.absolute_position().y)
            .unwrap_or_else(|| panic!("{id}"))
    };
    let column = top("MainWindow::left-column");
    let source = top("MainWindow::source");
    assert!(
        source >= column,
        "the source at {source}, the column at {column}"
    );
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

/// Windows sets the Mac's regular, medium and semibold heavier, so its text
/// carries the ink the Mac's does at 1:1; Inter keeps the Mac's numbers, and
/// every weight and window default comes from Fluent.
// covers: S450
#[test]
fn text_weights_are_heavier_on_windows_and_come_from_fluent() {
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    for (role, windows, elsewhere) in [
        ("regular", 480, 400),
        ("medium", 560, 500),
        ("semibold", 640, 600),
    ] {
        assert!(fluent.contains(&format!(
            "out property <int> weight-{role}: Platform.os == OperatingSystemType.windows ? {windows} : {elsewhere};"
        )));
    }
    for (name, source) in [
        ("main", include_str!("../../ui/main.slint")),
        ("flyout", include_str!("../../ui/flyout.slint")),
        ("widgets", include_str!("../../ui/widgets.slint")),
        ("equalizer", include_str!("../../ui/equalizer.slint")),
        ("settings", include_str!("../../ui/settings.slint")),
        ("dialogs", include_str!("../../ui/dialogs.slint")),
        ("setup", include_str!("../../ui/setup.slint")),
        ("remote", include_str!("../../ui/remote.slint")),
    ] {
        for literal in ["400", "500", "600", "700"] {
            assert!(
                !source.contains(&format!("font-weight: {literal}")),
                "{name}: {literal}"
            );
            let outside_fluent = source.replace(fluent.as_str(), "");
            assert!(
                !outside_fluent.contains(&format!("? {literal} :")),
                "{name}"
            );
        }
        assert_eq!(
            source.matches("default-font-family: Fluent.font;").count(),
            source
                .matches("default-font-weight: Fluent.weight-regular;")
                .count(),
            "{name}"
        );
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

/// The source button shows the chosen app's own icon, as the Mac's does, and
/// keeps the speaker or microphone glyph for anything without one; Presets
/// shows the stacked squares.
// covers: S450
#[test]
fn the_source_button_shows_the_chosen_apps_icon_and_presets_the_stack() {
    let menu = component(include_str!("../../ui/main.slint"), "SourceMenu");
    assert!(menu.contains("if root.looks[root.index].icon.width > 0: Rectangle {"));
    assert!(menu.contains("source: root.looks[root.index].icon;"));
    assert!(menu.contains("if root.looks[root.index].icon.width == 0: Glyph {"));
    let glyphs = component(include_str!("../../ui/widgets.slint"), "Glyphs");
    assert!(glyphs.contains(r#"out property <string> presets: "\u{E81E}";"#));
}

/// The header sits 4 pt lower than a pixel into the title row, as 21 of the
/// Mac's 23 main window 2x photos measure it (logo top 78 px), and the columns
/// start the Mac's 14 px windowGap under it (MenuBar.swift:2668, MainWindow.swift:330).
// covers: S450
#[test]
fn the_header_sits_as_high_as_the_macs_and_the_columns_stay() {
    use i_slint_backend_testing::ElementHandle;
    use slint::ComponentHandle;
    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    app.poll();
    let top = |id: &str| {
        ElementHandle::find_by_element_id(&app.main, id)
            .next()
            .map(|element| element.absolute_position().y)
            .unwrap_or_else(|| panic!("{id}"))
    };
    let gear = top("MainWindow::gear");
    let column = top("MainWindow::left-column");
    assert_eq!((gear, column), (37.0, 85.0), "gear, then the left column");
}

/// A row or zone that comes or goes opens or shuts its room and fades with it
/// over the Mac's row move, 0.35 s ease-out, and at once with animation effects
/// off, as with Reduce Motion (MainWindow.swift:305-307, WindowRules.swift:82
/// and 213).
// covers: S450
#[test]
fn rows_and_zones_open_and_shut_over_the_macs_row_move() {
    let main = include_str!("../../ui/main.slint");
    let ease = component(main, "ListEase");
    for line in [
        "animate open {",
        "duration: Motion.on ? 350ms : 0ms;",
        "easing: ease-out;",
        "opacity: root.open;",
    ] {
        assert!(ease.contains(line), "{line}");
    }
    let rows = component(main, "SpeakerRows");
    assert!(rows.contains("in root.speakers: ListEase {"));
    assert!(main.contains("in root.zones: ListEase {"));
}

/// A row or zone on its way out takes no click or hover, and the keyboard
/// focus passes over it to a neighbour.
// covers: S450
#[test]
fn a_leaving_row_or_zone_takes_no_pointer_or_keyboard_focus() {
    let main = include_str!("../../ui/main.slint");
    let ease = component(main, "ListEase");
    assert!(ease.contains("if root.leaving: TouchArea { }"));
    let rows = component(main, "SpeakerRows");
    assert!(rows.contains("if speaker.leaving && part"));
    assert!(main.contains("if zone.leaving && part"));
}

/// A leaving row or zone hands the keyboard focus to its nearest neighbour
/// that stays, never to one leaving too, so the focus doesn't bounce.
// covers: S450
#[test]
fn a_leaving_row_or_zone_hands_the_focus_to_a_neighbour_that_stays() {
    let main = include_str!("../../ui/main.slint");
    for blind in ["root.speakers[index + 1].id", "root.zones[index + 1].id"] {
        assert!(!main.contains(blind), "{blind}");
    }
    let rows = component(main, "SpeakerRows");
    assert!(rows.contains("root.focus-id = speaker.hand-off;"));
    assert_eq!(
        main.matches("root.focus-zone-id = zone.hand-off;").count(),
        2
    );

    let row = |id: &str| super::SpeakerItem {
        id: id.into(),
        ..super::SpeakerItem::default()
    };
    let hand_offs = |rows: &[super::SpeakerItem]| -> Vec<String> {
        rows.iter().map(|row| row.hand_off.to_string()).collect()
    };
    let (rows, _) = super::eased_rows(
        &[row("a"), row("b"), row("c"), row("d")],
        vec![row("a"), row("d")],
    );
    assert_eq!(
        hand_offs(&rows),
        ["", "a", "d", ""],
        "the nearest, the next one first"
    );
    let (rows, _) = super::eased_rows(&rows, vec![row("a")]);
    assert_eq!(hand_offs(&rows), ["", "a", "a", "a"], "the one before");
    let (rows, _) = super::eased_rows(&rows, Vec::new());
    assert_eq!(hand_offs(&rows), ["", "", "", ""], "none stays");
}

/// While rows ease, an arriving row is marked fresh and a row that left stays
/// where it was, marked leaving, and is reported once.
// covers: S450
#[test]
fn rows_that_come_or_go_are_marked_and_a_leaving_one_keeps_its_place() {
    let row = |id: &str| super::SpeakerItem {
        id: id.into(),
        ..super::SpeakerItem::default()
    };
    let marks = |rows: &[super::SpeakerItem]| -> Vec<(String, bool, bool)> {
        rows.iter()
            .map(|row| (row.id.to_string(), row.fresh, row.leaving))
            .collect()
    };
    let (rows, left) = super::eased_rows(
        &[row("a"), row("b"), row("c")],
        vec![row("a"), row("c"), row("d")],
    );
    assert_eq!(
        marks(&rows),
        [
            ("a".to_owned(), false, false),
            ("b".into(), false, true),
            ("c".into(), false, false),
            ("d".into(), true, false),
        ]
    );
    assert_eq!(left, ["b"]);
    let (again, left) = super::eased_rows(&rows, vec![row("a"), row("c"), row("d")]);
    assert_eq!(marks(&again)[1], ("b".to_owned(), false, true));
    assert!(left.is_empty(), "reported once");
    let (back, _) = super::eased_rows(&rows, vec![row("a"), row("b"), row("c"), row("d")]);
    assert_eq!(
        marks(&back),
        [
            ("a".to_owned(), false, false),
            ("b".into(), false, false),
            ("c".into(), false, false),
            ("d".into(), true, false),
        ],
        "a row back before it went stays put; d keeps coming until its move ends"
    );
}

/// A hidden speaker's row fades out for the Mac's 0.35 s before it leaves the
/// list, and comes back fresh, so its room opens again.
// covers: S450
#[test]
fn a_hidden_speakers_row_stays_leaving_for_the_macs_row_move_then_goes() {
    use slint::{ComponentHandle, Model};
    let app = app_over(MockBackend::new());
    app.rows_ease.set(true);
    app.main.global::<crate::Motion>().set_on(true);
    app.main.show().expect("the main window shows headless");
    app.refresh();
    let row = |id: &str| app.models.connected.iter().find(|row| row.id == id);
    let marks = || row("local:desk").map(|row| (row.fresh, row.leaving));
    assert_eq!(marks(), Some((false, false)));

    app.main.invoke_speaker_hide_requested("local:desk".into());
    assert_eq!(marks(), Some((false, true)), "it fades out first");
    std::thread::sleep(std::time::Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    assert_eq!(marks(), None, "then it leaves the list");

    app.main
        .invoke_speaker_unhide_requested("local:desk".into());
    assert_eq!(marks(), Some((true, false)), "it comes back fresh");
}

/// A row hidden, shown and hidden again within the row move stays for the
/// whole move of its last hiding: the first hiding's timer leaves it alone.
// covers: S450
#[test]
fn a_row_hidden_again_within_the_row_move_stays_for_its_whole_move() {
    use slint::{ComponentHandle, Model};
    use std::time::Duration;
    let app = app_over(MockBackend::new());
    app.rows_ease.set(true);
    app.main.global::<crate::Motion>().set_on(true);
    app.main.show().expect("the main window shows headless");
    app.refresh();
    let marks = || {
        app.models
            .connected
            .iter()
            .find(|row| row.id == "local:desk")
            .map(|row| (row.fresh, row.leaving))
    };

    app.main.invoke_speaker_hide_requested("local:desk".into());
    std::thread::sleep(Duration::from_millis(50));
    app.main
        .invoke_speaker_unhide_requested("local:desk".into());
    std::thread::sleep(Duration::from_millis(200));
    app.main.invoke_speaker_hide_requested("local:desk".into());
    std::thread::sleep(Duration::from_millis(150));
    slint::platform::update_timers_and_animations();
    assert_eq!(
        marks(),
        Some((false, true)),
        "the first hiding's 350 ms are up, the second's are not"
    );
    std::thread::sleep(Duration::from_millis(300));
    slint::platform::update_timers_and_animations();
    assert_eq!(marks(), None, "it goes when its last move ends");
}

/// Zones come and go as the speaker rows do: the Mac's row move animates on
/// the zones' IDs too (MainWindow.swift:307).
// covers: S450
#[test]
fn a_deleted_zone_stays_leaving_for_the_macs_row_move_and_a_new_one_comes_fresh() {
    use slint::{ComponentHandle, Model};
    let app = app_over(MockBackend::zones_preview());
    app.rows_ease.set(true);
    app.main.global::<crate::Motion>().set_on(true);
    app.main.show().expect("the main window shows headless");
    app.refresh();
    let office = || {
        app.models
            .zones
            .iter()
            .find(|zone| zone.id == "zone:office")
            .map(|zone| (zone.fresh, zone.leaving))
    };
    assert_eq!(office(), Some((false, false)));

    app.main.invoke_zone_deleted("zone:office".into());
    assert_eq!(office(), Some((false, true)), "it fades out first");
    std::thread::sleep(std::time::Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    assert_eq!(office(), None, "then it leaves the list");

    app.main
        .invoke_speaker_new_zone_requested("local:headphones".into());
    let fresh = app.models.zones.iter().filter(|zone| zone.fresh).count();
    assert_eq!(fresh, 1, "a new zone comes fresh");
}

/// The header's streaming switch shows no tooltip (maintainer 2026-11-16,
/// "tooltip buat apaan sih? remove aja"), as the tray panel's.
#[test]
fn the_main_window_streaming_switch_has_no_tooltip() {
    let switch = component(include_str!("../../ui/widgets.slint"), "PowerSwitch");
    assert!(!switch.contains("Tooltip"), "none in the switch");
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let at = main
        .find("power := PowerSwitch {")
        .expect("the header switch");
    let end = main[at..]
        .find("if root.notices.length > 0: NoticeStack {")
        .expect("the header's end");
    assert!(!main[at..at + end].contains("Tooltip"), "none around it");
}

/// Each card's corner is the Mac's: the panel hero 12 (MenuBar.swift:2348,
/// BrandTokens.swift:160), the setup list 8 (SetupWindow.swift:150).
#[test]
fn the_cards_have_the_macs_corners() {
    let tokens = component(include_str!("../../ui/tokens.slint"), "Tokens");
    assert!(tokens.contains("out property <length> radius-card: 12px;"));
    let flyout = include_str!("../../ui/flyout.slint").replace("\r\n", "\n");
    let hero = &flyout[flyout.find("hero-card := Rectangle {").expect("the hero")..];
    assert_eq!(
        hero.lines().nth(1).map(str::trim),
        Some("border-radius: Tokens.radius-card;"),
        "the panel hero"
    );
    let fluent = component(include_str!("../../ui/widgets.slint"), "Fluent");
    assert!(fluent.contains("out property <length> radius-card: 12px;"));
    let setup = include_str!("../../ui/setup.slint").replace("\r\n", "\n");
    assert!(
        setup.contains(
            "background: Tokens.surface;\n                        // The Mac's setup list (SetupWindow.swift:150).\n                        border-radius: 8px;"
        ),
        "the setup list"
    );
}

/// A row's small slider as the Mac's 18 Nov photos at 2x show it (a native
/// .controlSize(.small) Slider, MainWindow.swift:2462-2473): an 18 x 14 knob,
/// #dddddd on dark whether the row is on or off, a track of ink at 10 %
/// past the knob and 14.5 % before it on an off row, a #f4f4f4 fill on dark.
// covers: S450
#[test]
fn a_rows_slider_is_the_macs_small_slider() {
    let widgets = include_str!("../../ui/widgets.slint");
    let slider = component(widgets, "LevelSlider");
    assert!(slider.contains("property <length> knob: root.small ? 18px : 16px;"));
    assert!(slider.contains("property <length> knob-across: root.small ? 14px : 16px;"));
    assert!(slider.contains("height: root.knob-across;"));
    assert!(slider.contains(
        "root.small ? (piece == 0 && root.idle ? (Tokens.dark ? #ffffff25 : #00000025) : (Tokens.dark ? #ffffff19 : #00000019)) : root.track;"
    ));
    assert!(slider.contains("root.small ? (Tokens.dark ? #f4f4f4 : #000000f5) : Tokens.text;"));
    assert!(slider.contains(
        "background: !root.small ? #ffffff : Tokens.dark ? #dddddd : touch.pressed ? #ebebeb : #ffffff;"
    ));
    assert!(!slider.contains("#959595"), "no grey knob on dark");
}

/// The Mac's main window fills an on speaker's badge while streaming is on,
/// also while it reconnects or is off the network: its row state is only
/// on and streaming (MenuBar.swift:505-507, SpeakerBadge :2191-2200).
// covers: S450
#[test]
fn an_on_speaker_that_reconnects_keeps_the_macs_filled_badge() {
    use crate::ui_model::Waiting;
    let mut state = app_over(MockBackend::new()).snapshot();
    state.running = true;
    for waiting in [Waiting::Reconnecting, Waiting::NotOnNetwork] {
        let mut speaker = state.speakers[0].clone();
        speaker.enabled = true;
        speaker.waiting = Some(waiting);
        assert!(
            super::speaker_item(&state, &speaker, true, false, None).plays,
            "{waiting:?}"
        );
    }
}

/// The Mac's warning is exclamationmark.triangle.fill everywhere
/// (MenuBar.swift:2115, :2316, :2798): a solid triangle with the mark cut
/// out, drawn on Windows too, as the icon font's warning is an outline.
// covers: S450
#[test]
fn the_warning_is_the_macs_solid_triangle() {
    let widgets = include_str!("../../ui/widgets.slint").replace(
        "
", "
",
    );
    let glyphs = component(&widgets, "Glyphs");
    let everywhere = &glyphs[glyphs
        .find("public pure function drawn-everywhere(")
        .expect("drawn-everywhere")..];
    let everywhere = &everywhere[..everywhere
        .find(
            "
    }",
        )
        .expect("it ends")];
    assert!(everywhere.contains("glyph == root.warning"), "{everywhere}");
    let outline = &glyphs[glyphs
        .find("public pure function outline(")
        .expect("outline")
        ..glyphs.find("public pure function solid(").expect("solid")];
    assert!(
        !outline.contains("root.warning"),
        "no stroked triangle over it"
    );
    assert!(drawing(&glyphs, "solid", "warning").starts_with("return \"M8 1.5 L15 14 H1 Z"));
}

/// The text between `start` and `end` in a UI file.
fn between(source: &str, start: &str, end: &str) -> String {
    let from = source.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &source[from..];
    rest[..rest.find(end).unwrap_or_else(|| panic!("{end}"))].to_owned()
}

/// The Mac's running calibration (CalibrationViews.swift:41-60): a grey
/// small spinner, the step, and Cancel as a small grey push button; the
/// delay steps' digits 11 px in its 2x photos.
// covers: S450
#[test]
fn a_running_calibration_shows_a_grey_spinner_and_a_small_cancel() {
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let sync = between(
        &widgets,
        "export component SyncControls ",
        "export component AirPlayDelayControls ",
    );
    assert!(!sync.contains("Spinner {"), "no teal arc");
    assert!(sync.contains("ProgressSpokes {"), "the Mac's grey spokes");
    assert!(
        sync.contains("SmallButton {\n                grey: true;"),
        "a small grey Cancel"
    );
    let step = between(&widgets, "component DelayStep ", "component DraftOutline ");
    assert!(step.contains("font-size: 11px;"), "{step}");
}

/// The Mac's Presets and Equalizer rows keep 6 pt above the name and under
/// its line (MainWindow.swift:1234, :569, :915), so a plain row stays 28 high.
// covers: S450
#[test]
fn the_presets_and_equalizer_rows_pad_their_text_as_the_mac() {
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    assert_eq!(
        main.matches("padding-top: (28px - label.preferred-height) / 2;")
            .count(),
        2
    );
}

/// The Mac's source menu draws its chevron right after the name
/// (MenuBar.swift:2470-2484, a borderless menu's indicator), not at the edge.
// covers: S450
#[test]
fn the_source_menus_chevron_follows_the_name() {
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let menu = between(&main, "component SourceMenu ", "menu := PopupWindow");
    let name = between(&menu, "text: root.sources[root.index];", "}");
    assert!(!name.contains("horizontal-stretch: 1;"), "{name}");
}

/// A horizontal slider's track runs the whole box with the knob inside it, as
/// the Mac's small Slider (MainWindow.swift:2461-2486, photos at 2x) and its
/// MasterSlider (MenuBar.swift:3009-3012, WindowRules.swift:1597-1600): a
/// row's slider sits flush under the name and ends 8 px before its value.
// covers: S450
#[test]
fn a_sliders_track_runs_its_whole_box_as_on_the_mac() {
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let slider = component(&widgets, "LevelSlider");
    assert!(slider.contains("piece == 0 ? 0px : root.span * root.ratio + root.knob;"));
    assert!(slider.contains("piece == 0 ? root.span * root.ratio : root.span * (1 - root.ratio)"));
    assert!(slider.contains(
        "width: root.vertical ? root.thickness : root.span * root.ratio + root.knob / 2;"
    ));
    let row = component(&widgets, "SpeakerRow");
    assert!(
        !row.contains("x: -Fluent.slider-inset;"),
        "no row slider moved left"
    );
    let master = component(&widgets, "MasterVolume");
    assert!(
        master.contains("padding-left: 8px;"),
        "the track starts where it did"
    );
}

/// The auto-connect bolt as large as the Mac's bolt.fill at .caption2
/// (MainWindow.swift:2434-2436): 11.5 pt of ink in its 2x photos, which the
/// drawn path (13/16 of its box) reaches at 14 px.
// covers: S450
#[test]
fn the_auto_connect_bolt_is_the_macs_size() {
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let row = component(&widgets, "SpeakerRow");
    let at = row.find("glyph: Glyphs.auto-connect;").expect("bolt");
    let bolt = &row[row[..at].rfind("Rectangle {").unwrap()..at + 120];
    assert!(bolt.contains("width: 14px;"), "{bolt}");
    assert!(bolt.contains("font-size: 14px;"), "{bolt}");
}

/// A row with no delay field keeps no delay column, as the Mac's AirPlay rows
/// (MainWindow.swift:2438-2445): only the Spacer's 50 px before the options
/// stays (MAIN-ROW-PROBLEM at 2x), so a long note wraps where the Mac's does.
// covers: S450
#[test]
fn a_row_without_a_delay_gives_its_note_the_delay_column() {
    let widgets = include_str!("../../ui/widgets.slint").replace("\r\n", "\n");
    let row = component(&widgets, "SpeakerRow");
    assert!(row.contains(
        "width: root.item.has-delay || root.item.system-volume >= 0 ? Fluent.delay-column + 3px : 50px;"
    ));
}

/// The window's source button is the Mac's borderless menu (MenuBar.swift:2469-2484):
/// the name semibold, the speaker glyph 16 in the 20 pt icon frame (:2510, `large`),
/// and the menu's indicator 4 after the name, as its photo measures it.
// covers: S450
#[test]
fn the_source_button_draws_the_macs_semibold_name_and_small_glyph() {
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let menu = between(&main, "component SourceMenu ", "menu := PopupWindow");
    let glyph = between(&menu, "icon.width == 0: Glyph {", "}");
    assert!(
        glyph.contains("width: 20px;") && glyph.contains("font-size: 16px;"),
        "{glyph}"
    );
    let name = between(&menu, "text: root.sources[root.index];", "}");
    assert!(
        name.contains("font-weight: Fluent.weight-semibold;"),
        "{name}"
    );
    let label = between(&menu, "name-row := HorizontalLayout {", "Glyph {");
    assert!(label.contains("spacing: 4px;"), "{label}");
}
