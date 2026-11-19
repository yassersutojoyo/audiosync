//! Calibrate All's run and results in a sheet over the main window, as the
//! Mac's, and the speaker volume's number while its slider is held.

use super::MainControl;
use super::tests::{app_over, draw_frames, press};
use crate::ui_model::MockBackend;
use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;
use slint::platform::Key;
use std::cell::Cell;
use std::rc::Rc;

// covers: S450
#[test]
fn calibrate_all_runs_in_a_600_wide_sheet_that_holds_the_focus() {
    let main = include_str!("../../ui/main.slint");
    assert!(
        main.find("component CalibrateAllSheet ")
            .is_some_and(|at| main[at..].contains("padding: 20px;\n        spacing: 14px;")),
        "the sheet's margin and spacing are the Mac's"
    );

    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    draw_frames();
    let cancels = Rc::new(Cell::new(0));
    let counted = cancels.clone();
    app.main
        .on_cancel_calibrate_all_requested(move || counted.set(counted.get() + 1));
    let sheet = || ElementHandle::find_by_element_id(&app.main, "CalibrateAllSheet::sheet").next();
    assert!(sheet().is_none(), "no sheet before a run");

    app.main.set_calibrate_all_can_start(true);
    app.main
        .set_calibrate_all_status("Measuring Desk Speakers…".into());
    app.main.set_calibrate_all_status_is_hint(false);
    app.main.set_calibrate_all_running(true);
    draw_frames();
    let shown = sheet();
    assert!(shown.is_some(), "a run shows its sheet");
    assert_eq!(shown.map(|sheet| sheet.size().width), Some(600.0));

    let window = app.main.window();
    press(window, " ");
    assert_eq!(cancels.get(), 1, "the focus lands on the sheet's Cancel");
    press(window, Key::Escape);
    assert_eq!(cancels.get(), 2, "Escape cancels the run");

    app.main.set_calibrate_all_running(false);
    draw_frames();
    assert!(sheet().is_none(), "a cancelled run closes the sheet");
    assert_eq!(
        app.main_focus.borrow().control,
        Some(MainControl::Calibration),
        "the focus is back on Calibrate All"
    );
}

/// As on the Mac: the run's progress has the music caption in its group,
/// Undo shows only while there is something to undo, Apply only while the
/// results are not applied, and Apply takes the focus as a run ends.
// covers: S450
#[test]
fn the_sheet_shows_undo_and_apply_only_when_they_act_and_says_the_music_comes_back() {
    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    draw_frames();
    let applies = Rc::new(Cell::new(0));
    let counted = applies.clone();
    app.main
        .on_calibrate_all_apply_requested(move || counted.set(counted.get() + 1));
    let shown = |id: &str| {
        ElementHandle::find_by_element_id(&app.main, &format!("CalibrateAllSheet::{id}")).next()
    };

    app.main
        .set_calibrate_all_status("Measuring Desk Speakers…".into());
    app.main.set_calibrate_all_running(true);
    draw_frames();
    let progress = shown("progress").and_then(|group| group.accessible_label());
    assert_eq!(
        progress.as_deref(),
        Some("Measuring Desk Speakers… The music fades back in when it's done.")
    );

    app.main.set_calibrate_all_results_shown(true);
    app.main.set_calibrate_all_can_apply(true);
    app.main.set_calibrate_all_running(false);
    draw_frames();
    assert!(shown("undo-button").is_none(), "nothing to undo yet");
    assert!(shown("apply-button").is_some());
    press(app.main.window(), " ");
    assert_eq!(applies.get(), 1, "Apply has the focus as the run ends");

    app.main.set_calibrate_all_applied(true);
    app.main.set_calibrate_all_can_apply(false);
    app.main.set_calibrate_all_can_undo(true);
    draw_frames();
    assert!(shown("apply-button").is_none(), "applied");
    assert!(shown("undo-button").is_some());
}

/// A speaker volume's number turns primary from the press on its slider,
/// as the Mac's does while the slider is held, not only once it moves.
#[test]
fn a_speaker_volumes_number_is_primary_while_its_slider_is_held() {
    let widgets = include_str!("../../ui/widgets.slint");
    assert!(widgets.contains("out property <bool> held: touch.pressed;"));
    assert!(
        widgets.contains(
            "color: root.is-editing || slider.held ? Tokens.text : Tokens.secondary-text;"
        )
    );
}

/// As on the Mac, Calibrate All Speakers first opens its sheet while an
/// output sits out: the left-out list with Include Anyway, then Close and
/// the sheet's own Calibrate All Speakers, which starts the run
/// (AppDelegate+CalibrateAll.swift:37-45; CalibrateAllViews.swift:117-131,
/// 433-478, 503-512). The left column keeps only the row (:6-54).
// covers: S362
#[test]
fn calibrate_all_opens_its_sheet_with_the_left_out_before_any_run() {
    use crate::audio::backend::OutputKind;
    use crate::audio::outputs::DeviceTraits;
    use crate::ui_model::{AllPhase, Backend as _};
    let mut audio = crate::audio::mock::MockBackend::empty();
    audio.connect_output_as("speakers", "Speakers", OutputKind::BuiltIn);
    audio.connect_output_as("usb", "USB Speakers", OutputKind::UsbSpeakers);
    audio.connect_output_as("phones", "Earbuds", OutputKind::Bluetooth);
    audio.set_output_traits(
        "phones",
        DeviceTraits {
            form_factor: Some(3),
            enumerator: Some("BTHENUM".to_owned()),
        },
    );
    let mut adapter = crate::adapter::AudioAdapter::new(
        audio,
        || "09.41".to_owned(),
        crate::debug_log::DebugLog::default(),
    );
    for id in ["lc:speakers", "lc:usb", "lc:phones"] {
        adapter.set_speaker_enabled(id, true);
    }
    adapter.set_running(true);
    let app = app_over(adapter);
    app.main.show().expect("the main window shows headless");
    draw_frames();
    let strings = app.strings();
    let left_out = strings.get_calibrate_all_left_out_title();
    let include = strings.invoke_include_anyway_label("Earbuds".into());
    let count = |label: &str| ElementHandle::find_by_accessible_label(&app.main, label).count();
    let sheet = || ElementHandle::find_by_element_id(&app.main, "CalibrateAllSheet::sheet").next();
    let start =
        || ElementHandle::find_by_element_id(&app.main, "CalibrateAllSheet::start-button").next();
    assert_eq!(count(&left_out), 0, "no left-out list in the left column");
    assert_eq!(count(&include), 0, "no Include Anyway in the left column");

    app.main.invoke_calibrate_all_requested();
    app.poll();
    draw_frames();
    assert_eq!(
        app.snapshot().calibrate_all.phase,
        AllPhase::Idle,
        "no run starts yet"
    );
    assert!(sheet().is_some(), "the sheet opens");
    assert_eq!(count(&left_out), 1, "the sheet lists the left out");
    assert_eq!(count(&include), 1, "with Include Anyway");
    let button = start().expect("the sheet's own Calibrate All Speakers");
    assert_eq!(
        button.accessible_label().as_deref(),
        Some(strings.get_calibrate_all().as_str())
    );

    // The mock has no microphone: the run stops at once and the sheet stays,
    // its button ready for another try.
    button.invoke_accessible_default_action();
    app.poll();
    draw_frames();
    assert!(
        matches!(app.snapshot().calibrate_all.phase, AllPhase::Failed(_)),
        "the sheet's button starts the run"
    );
    assert!(sheet().is_some(), "the sheet stays with the run's line");
    assert!(start().is_some(), "Calibrate All Speakers stays");

    press(app.main.window(), Key::Escape);
    app.poll();
    draw_frames();
    assert!(sheet().is_none(), "Escape closes the sheet");
}

/// The sheet's cards are the Mac's controlBackgroundColor, the sheet's own colour, with
/// a separatorColor edge (CalibrateAllViews.swift:210-212, :319-320, :407-408; its photos
/// read #e6e6e6 on light and #343434 on dark), and Set to 75% keeps its own width (:233-243).
// covers: S450
#[test]
fn the_calibrate_sheets_cards_are_outlined_and_the_fix_button_compact() {
    let main = include_str!("../../ui/main.slint").replace("\r\n", "\n");
    let start = main
        .find("component CalibrateAllSheet ")
        .expect("the sheet");
    let sheet = &main[start..];
    let sheet = &sheet[..sheet.find("\ncomponent ").unwrap_or(sheet.len())];
    assert!(
        !sheet.contains("background: Tokens.surface;"),
        "no filled card"
    );
    assert_eq!(sheet.matches("background: root.card-fill;").count(), 3);
    assert_eq!(sheet.matches("border-color: root.card-edge;").count(), 3);
    assert!(sheet.contains("property <brush> card-fill: sheet.background;"));
    assert!(sheet.contains("property <brush> card-edge: Tokens.dark ? #ffffff1a : #0000001a;"));
    let fix = &sheet[..sheet.find("volume-fix := SmallButton").expect("Set to 75%")];
    let wrapper = &fix[fix.rfind("VerticalLayout {").expect("its column")..];
    assert!(wrapper.contains("horizontal-stretch: 0;"), "{wrapper}");
}
