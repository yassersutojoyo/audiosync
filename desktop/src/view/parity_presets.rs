//! Presets, Add by Address… and Hidden Speakers against the Mac's
//! (MainWindow.swift PresetsRow, AppDelegate+Presets.swift,
//! CastAddressSheet.swift, SettingsWindow.swift).

use slint::{ComponentHandle, Model};

use super::tests::{app_over, main_window_over, press, settle_cast_check, type_text};
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
    let source = source.replace("\r\n", "\n");
    let start = source.find(from).unwrap_or_else(|| panic!("{from}"));
    let rest = &source[start..];
    rest[..rest.find(to).unwrap_or_else(|| panic!("{to}"))].to_owned()
}

fn all_speakers_off(app: &super::App) {
    for speaker in app.snapshot().speakers {
        app.update(|b| b.set_speaker_enabled(&speaker.id, false));
    }
}

/// The Mac shows the caption only while `needsSpeakers && !presetsCanSave`
/// (MainWindow.swift:1026), so it goes once a speaker is on.
#[test]
fn the_turn_on_speakers_caption_goes_once_a_speaker_is_on() {
    let app = main_window_over(MockBackend::new());
    let first = app.snapshot().speakers[0].id.clone();
    all_speakers_off(&app);
    app.ask_save_preset();
    assert_ne!(app.main.get_preset_save_note(), "");
    app.update(|b| b.set_speaker_enabled(&first, true));
    assert_eq!(app.main.get_preset_save_note(), "");
}

/// AppDelegate+Presets.swift:191: an informational alert titled Save Preset.
#[test]
fn return_after_the_last_speaker_went_off_says_so_in_a_save_preset_alert() {
    let app = main_window_over(MockBackend::new());
    all_speakers_off(&app);
    app.main.invoke_preset_save_named("Evening".into());
    assert!(app.zone_dialog().window().is_visible());
    assert_eq!(app.zone_dialog().get_heading(), "Save Preset");
    assert_eq!(
        app.zone_dialog().get_message(),
        "Turn on the speakers you want in the preset first, then save it."
    );
    assert_eq!(app.main.get_presets().row_count(), 0);
}

/// MainWindow.swift:1156: only spaces count as blank, so the focus leaving
/// saves nothing; Return still saves "Preset n".
#[test]
fn the_focus_leaving_a_new_name_of_only_spaces_saves_nothing() {
    let card = component(include_str!("../../ui/main.slint"), "PresetsCard");
    let left = between(
        &card,
        "changed has-focus => {",
        "if root.note != \"\": HorizontalLayout",
    );
    assert!(left.contains("if !root.blank(self.text) {"), "{left}");
}

/// MainWindow.swift:1076-1081, 1108: no checked state; the picked preset
/// reads "In use, <summary>". MainWindow.swift:1064: the rename field's
/// placeholder is the preset's name.
#[test]
fn a_preset_row_reads_in_use_when_picked_and_renames_over_its_own_name() {
    let row = component(include_str!("../../ui/main.slint"), "PresetListRow");
    assert!(!row.contains("accessible-checkable"), "no checked state");
    assert!(!row.contains("accessible-checked"), "no checked state");
    assert!(row.contains(
        "accessible-value: root.item.picked ? Strings.preset-in-use(Strings.preset-summary(root.item.speakers, root.item.master)) : Strings.preset-summary(root.item.speakers, root.item.master);"
    ));
    assert!(row.contains("placeholder-text: root.item.name;"));
}

/// MainWindow.swift:932: "Choose a Preset" while none is picked, also with none saved.
#[test]
fn the_presets_row_says_choose_a_preset_with_none_saved() {
    let card = component(include_str!("../../ui/main.slint"), "PresetsCard");
    assert!(card.contains(
        "accessible-value: root.current != \"\" ? root.current : Strings.choose-preset;"
    ));
}

/// CastAddressSheet.swift:37, 69: typing clears the reason, and so does Add.
#[test]
fn add_by_address_clears_the_reason_on_typing_and_when_add_starts() {
    let app = app_over(MockBackend::new());
    app.main.invoke_cast_add_address_requested();
    let window = app.main.window();
    type_text(window, "not an address");
    press(window, slint::platform::Key::Return);
    assert_ne!(app.cast_sheet().get_error(), "");
    type_text(window, "x");
    assert_eq!(app.cast_sheet().get_error(), "", "typing clears it");

    app.cast_sheet().set_address("speaker.invalid".into());
    press(window, slint::platform::Key::Return);
    settle_cast_check(&app);
    assert_ne!(app.cast_sheet().get_error(), "");
    press(window, slint::platform::Key::Return);
    assert!(app.cast_sheet().get_checking());
    assert_eq!(app.cast_sheet().get_error(), "", "Add clears it");
    settle_cast_check(&app);
}

/// CastAddressSheet.swift:38, 71-73: Return on an empty field runs Add,
/// which says the address can't be read.
#[test]
fn return_on_an_empty_address_says_it_isnt_one() {
    let app = app_over(MockBackend::new());
    app.main.invoke_cast_add_address_requested();
    press(app.main.window(), slint::platform::Key::Return);
    assert_eq!(
        app.cast_sheet().get_error(),
        "That isn't an IP address or host name. Check it and try again."
    );
}

/// CastAddressSheet.swift:83: the cursor goes back in the field after a
/// speaker isn't found, with the text not selected.
#[test]
fn a_speaker_not_found_leaves_the_address_unselected() {
    let app = app_over(MockBackend::new());
    app.main.invoke_cast_add_address_requested();
    let window = app.main.window();
    type_text(window, "speaker.invalid");
    press(window, slint::platform::Key::Return);
    settle_cast_check(&app);
    type_text(window, "x");
    assert_eq!(app.cast_sheet().get_address(), "speaker.invalidx");
}

/// CastAddressSheet.swift:45-46: the spinner sits at the row's far left,
/// a spacer between it and Cancel.
#[test]
fn the_checking_spinner_sits_at_the_far_left_of_the_buttons() {
    let row = between(
        include_str!("../../ui/dialogs.slint"),
        "if CastSheet.error != \"\": Text {",
        "text: Strings.cancel;",
    );
    assert!(!row.contains("alignment: end;"), "{row}");
    let after_spinner = &row[row.find("Spinner {").expect("the spinner")..];
    assert!(after_spinner.contains("Rectangle {\n                horizontal-stretch: 1;"));
}

/// SettingsWindow.swift:211-212: the Hidden Speakers box springs over
/// 0.62 s, linear 0.15 s with less motion.
#[test]
fn the_hidden_speakers_list_moves_as_the_macs() {
    let list = between(
        include_str!("../../ui/settings.slint"),
        "text: Strings.hidden-speakers;",
        "if root.updater-available",
    );
    assert!(
        list.contains("duration: Motion.on ? 620ms : 150ms;"),
        "{list}"
    );
    assert!(list.contains("easing: Motion.on ? cubic-bezier(0.2, 0.9, 0.3, 1) : linear;"));
}
