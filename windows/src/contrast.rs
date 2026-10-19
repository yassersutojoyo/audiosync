//! Accessibility checks on the interface sources: text contrast from the
//! design tokens, and the accessible labels and focus rings of the controls
//! in `ui/*.slint`. Tests only.

use crate::tokens::{COLORS, CONTRAST_PAIRS, ColorToken, OPACITIES};

/// WCAG 2.x AA for normal-size text.
const MIN_TEXT_CONTRAST: f64 = 4.5;
/// WCAG 2.x AA for focus indicators and other non-text graphics.
const MIN_GRAPHIC_CONTRAST: f64 = 3.0;

#[derive(Clone, Copy, Debug)]
enum Appearance {
    Light,
    Dark,
}

fn token(name: &str) -> &'static ColorToken {
    COLORS
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} is a color token"))
}

fn opacity(name: &str) -> f64 {
    OPACITIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, value)| *value)
        .unwrap_or_else(|| panic!("{name} is an opacity token"))
}

fn srgb(name: &str, appearance: Appearance) -> [f64; 3] {
    let token = token(name);
    let rgb = match appearance {
        Appearance::Light => token.light,
        Appearance::Dark => token.dark,
    };
    rgb.map(|c| f64::from(c) / 255.0)
}

/// A foreground (`color` or `color@opacity`) composited on an opaque
/// background, as the screen shows it.
fn composite(foreground: &str, background: [f64; 3], appearance: Appearance) -> [f64; 3] {
    let (name, alpha) = match foreground.split_once('@') {
        Some((name, opacity_name)) => (name, opacity(opacity_name)),
        None => (foreground, 1.0),
    };
    let color = srgb(name, appearance);
    [0, 1, 2].map(|i| color[i] * alpha + background[i] * (1.0 - alpha))
}

/// WCAG relative luminance of an sRGB color.
fn luminance(rgb: [f64; 3]) -> f64 {
    let linear = rgb.map(|c| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}

/// WCAG contrast ratio, 1 to 21.
fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn ratio(foreground: &str, background: &str, appearance: Appearance) -> f64 {
    let background = srgb(background, appearance);
    contrast(composite(foreground, background, appearance), background)
}

#[test]
fn the_contrast_formula_matches_wcag() {
    assert!((contrast([0.0; 3], [1.0; 3]) - 21.0).abs() < 1e-9);
    assert!((contrast([1.0; 3], [1.0; 3]) - 1.0).abs() < 1e-9);
    // Brand.swift: the brand teal is 3.27:1 on white, tealText 5.85:1.
    let white = [1.0; 3];
    let teal = [28.0, 159.0, 145.0].map(|c: f64| c / 255.0);
    let teal_text = [20.0, 113.0, 103.0].map(|c: f64| c / 255.0);
    assert!((contrast(teal, white) - 3.27).abs() < 0.01);
    assert!((contrast(teal_text, white) - 5.85).abs() < 0.01);
}

/// Every text color on every background the UI draws it on, in light and
/// dark, reaches 4.5:1.
#[test]
fn every_text_pair_reaches_4_5_to_1_in_light_and_dark() {
    let mut failures = Vec::new();
    for appearance in [Appearance::Light, Appearance::Dark] {
        for (foreground, background) in CONTRAST_PAIRS {
            let value = ratio(foreground, background, appearance);
            if value < MIN_TEXT_CONTRAST {
                failures.push(format!(
                    "{foreground} on {background} ({appearance:?}): {value:.2}:1"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "below {MIN_TEXT_CONTRAST}:1: {failures:?}"
    );
}

/// The focus ring stands out from what it's drawn on (3:1 for graphics).
#[test]
fn the_focus_ring_reaches_3_to_1() {
    for appearance in [Appearance::Light, Appearance::Dark] {
        for background in ["background", "surface", "playingRow"] {
            let value = ratio("focus", background, appearance);
            assert!(
                value >= MIN_GRAPHIC_CONTRAST,
                "focus on {background} ({appearance:?}): {value:.2}:1"
            );
        }
    }
}

/// The interface sources, for the checks below.
const UI_FILES: &[(&str, &str)] = &[
    ("main.slint", include_str!("../ui/main.slint")),
    ("widgets.slint", include_str!("../ui/widgets.slint")),
    ("equalizer.slint", include_str!("../ui/equalizer.slint")),
    ("flyout.slint", include_str!("../ui/flyout.slint")),
    ("dialogs.slint", include_str!("../ui/dialogs.slint")),
    ("about.slint", include_str!("../ui/about.slint")),
    ("guide.slint", include_str!("../ui/guide.slint")),
];

/// Token colors that are not text on a token background: the wordmark's
/// "sync" is part of the logo, which WCAG exempts.
const NOT_TEXT_COLORS: &[&str] = &["logo"];

fn camel_case(kebab: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in kebab.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Every token color the UI draws text or icons in is covered by the
/// contrast pairs, so a new text color can't skip the check above.
#[test]
fn every_token_text_color_is_in_the_contrast_pairs() {
    let foregrounds: Vec<&str> = CONTRAST_PAIRS.iter().map(|(fg, _)| *fg).collect();
    for (file, source) in UI_FILES {
        for line in source.lines().map(str::trim) {
            let Some(value) = line.strip_prefix("color:") else {
                continue;
            };
            for (index, _) in value.match_indices("Tokens.") {
                let name: String = value[index + "Tokens.".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                let name = camel_case(&name);
                if NOT_TEXT_COLORS.contains(&name.as_str()) {
                    continue;
                }
                let as_foreground = match name.as_str() {
                    "secondaryText" => "text@secondaryText".to_string(),
                    other => other.to_string(),
                };
                assert!(
                    foregrounds.contains(&as_foreground.as_str()),
                    "{file}: `{line}` uses {as_foreground}, which no contrast pair covers"
                );
            }
        }
    }
}

/// The elements of `source` named `element` (`Button {`, `x := Slider {`),
/// each with the text of its block.
fn blocks<'a>(source: &'a str, element: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let pattern = format!("{element} {{");
    let mut from = 0;
    while let Some(offset) = source[from..].find(&pattern) {
        let start = from + offset;
        from = start + pattern.len();
        // A whole word: `Button {`, not `IconButton {` or `component Button`.
        let before = source[..start].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
        if source[line_start..start].contains("component ") {
            continue;
        }
        let mut depth = 0;
        let mut end = from;
        for (i, c) in source[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        found.push(&source[start..end]);
    }
    found
}

/// Every control has a label Narrator reads. Standard widgets get an
/// explicit `accessible-label`; AudioSync's own controls take a `label`
/// that their root turns into one.
#[test]
fn every_control_has_an_accessible_label() {
    let standard = [
        "Button",
        "Switch",
        "ToggleSwitch",
        "Slider",
        "ComboBox",
        "LineEdit",
        "CheckBox",
        "SpinBox",
    ];
    let own = ["IconButton", "SpeakerBadge", "VolumeSlider"];
    let mut checked = 0;
    for (file, source) in UI_FILES {
        for element in standard {
            for block in blocks(source, element) {
                checked += 1;
                assert!(
                    block.contains("accessible-label:"),
                    "{file}: a {element} without accessible-label:\n{block}"
                );
            }
        }
        for element in own {
            for block in blocks(source, element) {
                checked += 1;
                assert!(
                    block.contains("label:"),
                    "{file}: an {element} without a label:\n{block}"
                );
            }
        }
    }
    assert!(checked > 20, "the check found the controls ({checked})");
}

/// AudioSync's own controls (anything built on a TouchArea) have a role, a
/// label, a default action for Narrator and a keyboard focus ring.
#[test]
fn custom_controls_are_accessible_and_show_focus() {
    let widgets = include_str!("../ui/widgets.slint");
    let main = include_str!("../ui/main.slint");
    for (source, component) in [
        (widgets, "IconButton"),
        (widgets, "SpeakerBadge"),
        (main, "EqualizerCard"),
    ] {
        let start = source
            .find(&format!("component {component} "))
            .unwrap_or_else(|| panic!("{component} exists"));
        // Up to the next component definition.
        let rest = &source[start..];
        let end = ["\ncomponent ", "\nexport component "]
            .iter()
            .filter_map(|next| rest.find(next))
            .min()
            .unwrap_or(rest.len());
        let body = &rest[..end];
        for needle in [
            "accessible-role:",
            "accessible-label:",
            "accessible-action-default",
            "FocusScope",
            "has-focus",
        ] {
            assert!(body.contains(needle), "{component} has {needle}");
        }
    }
    // The delay button in a speaker row is built the same way.
    let row = blocks(widgets, "if root.item.has-delay: Rectangle")
        .into_iter()
        .next()
        .expect("the delay button");
    for needle in [
        "accessible-role: button",
        "accessible-label:",
        "accessible-value:",
        "FocusRing",
    ] {
        assert!(row.contains(needle), "the delay button has {needle}");
    }
}

/// The text of `component`'s definition in `source`, up to the next one.
fn component_body<'a>(source: &'a str, component: &str) -> &'a str {
    let start = source
        .find(&format!("component {component} "))
        .unwrap_or_else(|| panic!("{component} exists"));
    let rest = &source[start..];
    let end = ["\ncomponent ", "\nexport component ", "\n// "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// The on/off toggles (streaming, the equalizer, each speaker and group) are
/// keyboard focusable for Narrator too. Slint 1.18 tells Windows that only
/// some roles take keyboard focus, and `switch` is not one of them, so a
/// toggle is a button that is checkable: Windows reads it as a toggle
/// button that Tab reaches, and Space or Enter flips it.
#[test]
fn toggles_report_keyboard_focus_to_narrator() {
    for (file, source) in UI_FILES {
        assert!(
            !source.contains("accessible-role: switch"),
            "{file}: a switch role reads as not focusable"
        );
        assert!(
            blocks(source, "Switch").is_empty(),
            "{file}: use ToggleSwitch, not the standard Switch"
        );
    }
    let widgets = include_str!("../ui/widgets.slint");
    let toggle = component_body(widgets, "ToggleSwitch");
    assert!(toggle.contains("inherits Switch"));
    assert!(toggle.contains("accessible-role: button;"));
    let badge = component_body(widgets, "SpeakerBadge");
    for needle in [
        "accessible-role: button;",
        "accessible-checkable: true;",
        "accessible-checked:",
        "FocusScope",
        "event.text == \" \" || event.text == Key.Return",
        "FocusRing",
        "visible-focus: focus.has-focus;",
    ] {
        assert!(badge.contains(needle), "SpeakerBadge has {needle}");
    }
    let mut toggles = 0;
    for (_, source) in UI_FILES {
        toggles += blocks(source, "power := ToggleSwitch").len();
    }
    assert_eq!(toggles, 3, "main window, flyout and equalizer");
}

/// Icons and other decorative pieces stay out of Narrator's way. Slint 1.18
/// exposes every instance of a component whose root is a Text or Image,
/// whatever `accessible-role` says, so a decorative component wraps its Text
/// or Image (set to `none`) in a layout or Rectangle, and the icon font is
/// drawn only by Glyph.
#[test]
fn decorative_pieces_are_hidden_from_narrator() {
    let widgets = include_str!("../ui/widgets.slint");
    for (file, source) in UI_FILES {
        let mut from = 0;
        while let Some(offset) = source[from..].find("component ") {
            let start = from + offset;
            from = start + "component ".len();
            let header = source[start..].lines().next().unwrap_or_default();
            let Some(name) = header.split_whitespace().nth(1) else {
                continue;
            };
            if !(header.contains("inherits Text ") || header.contains("inherits Image ")) {
                continue;
            }
            let body = component_body(source, name);
            assert!(
                !body.contains("accessible-role: none"),
                "{file}: {name} inherits a Text or Image, so `none` is ignored; wrap it"
            );
            for (other_file, other) in UI_FILES {
                for block in blocks(other, name) {
                    assert!(
                        !block.contains("accessible-role: none"),
                        "{other_file}: a {name} set to `none`, which Slint ignores:\n{block}"
                    );
                }
            }
        }
        if *file != "widgets.slint" {
            assert!(
                !source.contains("Segoe Fluent Icons"),
                "{file}: icons are drawn by Glyph"
            );
        }
    }
    for component in ["Glyph", "SectionTitle", "QuietText", "BrandMark"] {
        let body = component_body(widgets, component);
        assert!(
            body.contains("inherits HorizontalLayout") || body.contains("inherits Rectangle"),
            "{component} wraps its content"
        );
        assert!(
            body.contains("accessible-role: none;"),
            "{component} hides its content"
        );
    }
    assert_eq!(
        widgets
            .matches("font-family: \"Segoe Fluent Icons\"")
            .count(),
        1,
        "only Glyph draws the icon font"
    );
}

/// Every window answers Escape or has a close button of its own, and the
/// main window's commands have Ctrl shortcuts.
#[test]
fn keyboard_shortcuts_are_declared() {
    let main = include_str!("../ui/main.slint");
    for keys in [
        "@keys(Control + W)",
        "@keys(Control + N)",
        "@keys(Control + E)",
        "@keys(F5)",
    ] {
        assert!(main.contains(keys), "the menu bar has {keys}");
    }
    for (file, source) in UI_FILES {
        if source.contains("inherits Window") && !source.contains("MenuBar {") {
            assert!(
                source.contains("Key.Escape"),
                "{file}: Escape closes the window"
            );
        }
    }
}

/// AirPlay on Windows is new and not yet checked against a real receiver,
/// so no text promises it: the translatable strings naming AirPlay are the
/// speaker kind, what a listed speaker can't do yet, and the notice that it
/// stopped after an internal error with its Retry button; an empty
/// Connected Speakers list says just that it's empty.
#[test]
fn no_text_promises_airplay() {
    let strings = include_str!("../ui/strings.slint");
    for line in strings.lines().filter(|line| line.contains("@tr(")) {
        assert!(
            !line.contains("AirPlay")
                || line.contains("@tr(\"AirPlay\")")
                || line.contains("not supported yet")
                || line.contains("doesn't support yet")
                || line.contains(
                    "notice-airplay-stopped: @tr(\"AirPlay stopped after an internal error"
                )
                || line.contains("notice-retry-label: @tr(\"Start AirPlay again\")"),
            "strings.slint: {}",
            line.trim()
        );
    }
    assert!(strings.contains("connected-none: @tr(\"No connected speakers\")"));
    assert!(
        include_str!("../ui/main.slint")
            .contains("root.connected.length == 0 ? Strings.connected-none"),
        "the empty Connected Speakers list uses the plain note"
    );
}

/// Whether `text` holds a private-use character (the icon font's glyphs), a
/// Slint escape for one, or a glyph by name, which Narrator would read as
/// noise.
fn has_private_use(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text.chars().any(|c| ('\u{E000}'..='\u{F8FF}').contains(&c))
        || ["\\u{e", "\\u{f"]
            .iter()
            .any(|escape| lower.contains(escape))
        || text.contains("Glyphs.")
}

/// No accessible name carries an icon glyph: every `accessible-label` and
/// every label of AudioSync's own controls is plain text, and the strings
/// file has no private-use characters at all.
#[test]
fn accessible_names_have_no_private_use_characters() {
    assert!(has_private_use("Glyphs.dismiss") && has_private_use("\"\\u{E711}\""));
    assert!(!has_private_use("Strings.dismiss-notice"));
    let mut checked = 0;
    for (file, source) in UI_FILES {
        for line in source.lines().map(str::trim) {
            if line.starts_with("accessible-label:") || line.starts_with("label:") {
                checked += 1;
                assert!(!has_private_use(line), "{file}: `{line}`");
            }
        }
    }
    assert!(checked > 20, "the check found the labels ({checked})");
    let strings = include_str!("../ui/strings.slint");
    for line in strings.lines() {
        assert!(!has_private_use(line), "strings.slint: `{}`", line.trim());
    }
}

/// Retry on the notice that AirPlay stopped: the button shows only on a
/// notice that offers it, both windows pass it on to the backend, and its
/// words are translatable strings.
#[test]
fn the_airplay_retry_button_is_wired_and_worded_in_strings() {
    let widgets = include_str!("../ui/widgets.slint");
    let stack = component_body(widgets, "NoticeStack");
    assert!(
        stack.contains("if notice.has-retry:"),
        "Retry only when offered"
    );
    assert!(stack.contains("root.retry-requested(notice.id);"));
    for (file, source) in [
        ("main.slint", include_str!("../ui/main.slint")),
        ("flyout.slint", include_str!("../ui/flyout.slint")),
    ] {
        assert!(
            source.contains("root.notice-retry-requested(id);"),
            "{file} passes Retry on"
        );
    }
    let view = include_str!("view.rs");
    assert_eq!(
        view.matches("_retry_requested(move |_| {").count(),
        2,
        "both windows' Retry reaches the backend"
    );
    let strings = include_str!("../ui/strings.slint");
    for name in [
        "notice-retry",
        "notice-retry-label",
        "notice-airplay-stopped",
    ] {
        let line = strings
            .lines()
            .find(|line| line.contains(&format!("out property <string> {name}:")))
            .unwrap_or_else(|| panic!("strings.slint has {name}"));
        assert!(line.contains("@tr(\""), "{name} is translatable");
    }
}

/// The double-audio guide: the notice's Show How button opens it from the
/// main window and the flyout, every text it shows is in strings.slint
/// (translatable, none spelled out in guide.slint), and its buttons only ask
/// for the Settings pages the Rust side knows.
#[test]
fn the_double_audio_guide_is_wired_and_worded_in_strings() {
    let widgets = include_str!("../ui/widgets.slint");
    let stack = component_body(widgets, "NoticeStack");
    assert!(
        stack.contains("if notice.has-guide:"),
        "Show How only with a guide"
    );
    assert!(stack.contains("root.guide-requested(notice.id);"));
    for (file, source) in [
        ("main.slint", include_str!("../ui/main.slint")),
        ("flyout.slint", include_str!("../ui/flyout.slint")),
    ] {
        assert!(
            source.contains("root.notice-guide-requested(id);"),
            "{file} passes Show How on"
        );
    }

    let guide = include_str!("../ui/guide.slint");
    assert!(
        !guide.contains("text: \""),
        "guide.slint spells out no text"
    );
    assert!(guide.contains("Key.Escape") && guide.contains("root.close-requested();"));
    assert!(guide.contains("root.page-requested(step.page);"));
    // A button that opened nothing says where Settings is, and the log says why.
    assert!(guide.contains("if root.settings-failed: Text {"));
    assert!(include_str!("view.rs").contains("self.guide.set_settings_failed(!opened);"));
    let wrapping =
        guide.matches("wrap: word-wrap;").count() + guide.matches("SecondaryText {").count();
    assert_eq!(
        guide.matches("min-height: self.preferred-height;").count(),
        wrapping,
        "every wrapping text in the guide keeps the height of all its lines"
    );

    let strings = include_str!("../ui/strings.slint");
    for name in [
        "notice-show-how",
        "notice-show-how-label",
        "guide-title",
        "guide-steps",
        "guide-footer",
        "guide-close",
        "guide-settings-failed",
    ] {
        let line = strings
            .lines()
            .find(|line| line.contains(&format!("out property <string> {name}:")))
            .unwrap_or_else(|| panic!("strings.slint has {name}"));
        assert!(line.contains("@tr(\""), "{name} is translatable");
    }
    for function in [
        "guide-intro-app",
        "guide-intro-system",
        "guide-route-app",
        "guide-change-default",
        "guide-route-other-apps",
        "guide-mute-outputs",
        "guide-shared-outputs",
        "guide-keep-app-unmuted",
        "guide-open",
        "list-and",
    ] {
        let start = strings
            .find(&format!("public pure function {function}("))
            .unwrap_or_else(|| panic!("strings.slint has {function}"));
        let body = &strings[start..];
        let body = &body[..body.find("\n    }").expect("the function ends")];
        assert!(body.contains("@tr(\""), "{function} is translatable");
    }
}
