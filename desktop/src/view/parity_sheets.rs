//! The sheets, the delay panel's buttons and the equalizer's bands sized and
//! placed as the Mac's photos measure them (LAYAR comb A7.4).

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::ComponentHandle;

use super::tests::{app_over, draw_frames};
use crate::ui_model::MockBackend;

const MAIN: &str = include_str!("../../ui/main.slint");
const WIDGETS: &str = include_str!("../../ui/widgets.slint");
const EQUALIZER: &str = include_str!("../../ui/equalizer.slint");

fn height(app: &super::App, id: &str) -> f32 {
    ElementHandle::find_by_element_id(&app.main, id)
        .next()
        .unwrap_or_else(|| panic!("{id}"))
        .size()
        .height
}

// CalibrateAllViews.swift:487-519: the Mac's sheet sits right under the title
// row over an undimmed window, its push buttons 24 pt high.
#[test]
fn the_calibrate_all_sheet_sits_under_the_title_row_with_mac_sized_buttons() {
    assert!(!MAIN.contains("background: #00000033;"), "no scrim");
    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    draw_frames();
    app.main.set_calibrate_all_shown(true);
    draw_frames();
    let sheet = ElementHandle::find_by_element_id(&app.main, "CalibrateAllSheet::sheet")
        .next()
        .expect("the sheet");
    // Its popup starts under the 32 px title row; the sheet sits at the popup's top.
    assert_eq!(
        sheet.absolute_position().y,
        0.0,
        "right under the title row"
    );
    assert_eq!(height(&app, "CalibrateAllSheet::close-button"), 24.0);
    assert_eq!(height(&app, "CalibrateAllSheet::start-button"), 24.0);
}

// CastAddressSheet.swift:30-35, HouseButtonStyle.swift:58: the sheet under
// the title row, its field 22 pt high.
#[test]
fn add_by_address_sits_under_the_title_row_with_a_22_pt_field() {
    assert!(!MAIN.contains("#00000066 : #00000030"), "no scrim");
    let app = app_over(MockBackend::new());
    app.main.show().expect("the main window shows headless");
    draw_frames();
    app.main.invoke_cast_add_address_requested();
    draw_frames();
    let field = ElementHandle::find_by_accessible_label(&app.main, "Address")
        .find(|element| element.accessible_role() == Some(AccessibleRole::TextInput))
        .expect("the address field");
    assert_eq!(field.size().height, 22.0);
    assert!(
        field.absolute_position().y < 160.0,
        "the field at {} sits in a sheet at the window's top",
        field.absolute_position().y
    );
}

// MainWindow.swift:3500-3516 and CalibrationViews.swift:53-56: 11 pt steps,
// and Calibrate with Microphone the Mac's small grey push button.
#[test]
fn the_delay_panel_buttons_are_the_macs_small_ones() {
    let step = WIDGETS
        .find("component DelayStep ")
        .map(|at| &WIDGETS[at..at + 2500])
        .expect("DelayStep");
    assert!(step.contains("font-size: 11px;"));
    assert!(step.contains("min-width: label.preferred-width + 22px;"));
    let calibrate = WIDGETS
        .find("text: root.calibrating ? Strings.calibration-cancel")
        .map(|at| &WIDGETS[at - 300..at])
        .expect("Calibrate with Microphone");
    assert!(calibrate.contains("SmallButton {") && calibrate.contains("height: 20px;"));
}

// EqualizerWindow.swift:160, 318-341: a content-wide 24 pt curve chip, a
// 12 pt zero tick and a plain white 16 pt knob.
#[test]
fn the_equalizer_has_the_macs_curve_chip_tick_and_knob() {
    let app = app_over(MockBackend::new());
    app.show_equalizer();
    let eq = app.equalizer();
    let curve = ElementHandle::find_by_accessible_label(&eq, "Curve")
        .find(|element| element.accessible_role() == Some(AccessibleRole::Combobox))
        .expect("the curve menu");
    assert_eq!(curve.size().height, 24.0);
    assert!(curve.size().width < 170.0, "as wide as the curve name");
    assert!(EQUALIZER.contains("width: 12px;\n                        height: 1px;"));
    assert!(EQUALIZER.contains("width: 16px;\n                        height: 16px;"));
    assert!(!EQUALIZER.contains("Fluent.thumb"));
}

// The Mac's system switch (EqualizerWindow.swift:150) measures 54 x 24 pt on
// its 2x photo (EQ-WINDOW), and its curve chip 106 pt for Loudness: 13 + 11 pt
// beside the name and chevron, 19 pt between them.
#[test]
fn the_equalizer_switch_and_curve_chip_are_the_macs_size() {
    let app = app_over(MockBackend::new());
    app.show_equalizer();
    let eq = app.equalizer();
    let size = |id: &str| {
        ElementHandle::find_by_element_id(&eq, id)
            .next()
            .unwrap_or_else(|| panic!("{id}"))
            .size()
    };
    let track = size("ToggleSwitch::track");
    assert_eq!((track.width, track.height), (54.0, 24.0));
    let chip = size("CurveMenu::touch").width;
    let around = chip - size("CurveMenu::shown-text").width - size("CurveMenu::chevron").width;
    assert_eq!(around, 41.0);
    assert!(EQUALIZER.contains("// One line, as the Mac's (EqualizerWindow.swift:179)"));
    let at = EQUALIZER
        .find("text: Strings.equalizer-caption(")
        .expect("the caption");
    assert!(EQUALIZER[at..at + 400].contains("overflow: elide;"));
}

// EqualizerWindow.swift:145-187: 18 pt padding, 14 pt spacing and a band row
// only as tall as its gain, 140 pt slider and label; the Mac's photo
// (EQ-WINDOW, 584 px at 2x) is 292 pt high.
#[test]
fn the_equalizer_window_is_the_macs_292_pt_high() {
    let app = app_over(MockBackend::new());
    app.show_equalizer();
    assert_eq!(app.equalizer().get_fit_height(), 292.0);
}
