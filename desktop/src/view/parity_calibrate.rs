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
