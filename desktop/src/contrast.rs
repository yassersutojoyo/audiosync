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

/// The alpha of a `Fluent` fill (`hover`, `pressed`), read from the line in
/// widgets.slint that defines it as the text color at that alpha.
fn fluent_fill_alpha(name: &str) -> f64 {
    let prefix = format!("out property <brush> {name}: Tokens.text.with-alpha(");
    include_str!("../ui/widgets.slint")
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .and_then(|rest| rest.strip_suffix(");"))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("widgets.slint defines Fluent.{name} as text with an alpha"))
}

/// The fills the UI stacks on each background under the pointer. Speaker
/// rows on a card take the hover fill, and a hovered button inside a hovered
/// row adds its own; a playing row keeps its color, so only the button's
/// fill lands on it.
const HOVER_STACKS: &[(&str, &[&[&str]])] = &[
    ("background", &[&["hover"], &["pressed"]]),
    (
        "surface",
        &[
            &["hover"],
            &["pressed"],
            &["hover", "hover"],
            &["hover", "pressed"],
        ],
    ),
    ("playingRow", &[&["hover"], &["pressed"]]),
    ("notice", &[&["hover"], &["pressed"]]),
];

/// Every text pair keeps 4.5:1 under the hover and pressed fills the UI
/// stacks on its background, in light and dark.
#[test]
fn every_text_pair_reaches_4_5_to_1_under_hover_fills() {
    let mut failures = Vec::new();
    for appearance in [Appearance::Light, Appearance::Dark] {
        let text = srgb("text", appearance);
        for (foreground, background) in CONTRAST_PAIRS {
            let Some((_, stacks)) = HOVER_STACKS.iter().find(|(name, _)| name == background) else {
                continue;
            };
            for stack in *stacks {
                let filled = stack
                    .iter()
                    .fold(srgb(background, appearance), |under, fill| {
                        let alpha = fluent_fill_alpha(fill);
                        [0, 1, 2].map(|i| text[i] * alpha + under[i] * (1.0 - alpha))
                    });
                let value = contrast(composite(foreground, filled, appearance), filled);
                if value < MIN_TEXT_CONTRAST {
                    failures.push(format!(
                        "{foreground} on {background}+{} ({appearance:?}): {value:.2}:1",
                        stack.join("+")
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "below {MIN_TEXT_CONTRAST}:1: {failures:?}"
    );
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

/// The level meter's fill stands out from its track (3:1 for graphics),
/// and it is never a focusable control.
#[test]
fn the_level_meter_reaches_3_to_1_and_takes_no_focus() {
    for appearance in [Appearance::Light, Appearance::Dark] {
        let value = ratio("tealText", "separator", appearance);
        assert!(
            value >= MIN_GRAPHIC_CONTRAST,
            "meter fill on its track ({appearance:?}): {value:.2}:1"
        );
    }
    let body = component_body(include_str!("../ui/widgets.slint"), "LevelMeter");
    assert!(body.contains("background: Tokens.separator;"));
    assert!(body.contains("background: Tokens.teal-text;"));
    for focusable in ["FocusScope", "TouchArea", "forward-focus"] {
        assert!(!body.contains(focusable), "the meter has a {focusable}");
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
    ("help.slint", include_str!("../ui/help.slint")),
    ("remote.slint", include_str!("../ui/remote.slint")),
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
        "TextField",
        "CheckBox",
        "SpinBox",
    ];
    let own = ["IconButton", "SpeakerBadge", "VolumeSlider", "LinkButton"];
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
        (widgets, "LinkButton"),
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
/// or Image (set to `none`) in a layout or Rectangle, and the icon font (or,
/// off Windows, its drawn stand-in) is drawn only by Glyph.
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
    for component in [
        "Glyph",
        "SectionTitle",
        "CardTitle",
        "QuietText",
        "BrandMark",
    ] {
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
    for (file, source) in UI_FILES {
        if *file != "widgets.slint" {
            assert!(
                !source.contains("Glyphs.outline") && !source.contains("Glyphs.solid"),
                "{file}: icon drawings are drawn by Glyph"
            );
        }
    }
}

/// Segoe Fluent Icons ships only with Windows; elsewhere its code points
/// would render as empty boxes, so Glyph uses the font only on Windows and
/// every icon has a drawing of its own for the other systems.
#[test]
fn every_icon_has_a_drawing_off_windows() {
    let widgets = include_str!("../ui/widgets.slint");
    let glyph = component_body(widgets, "Glyph");
    let font = glyph
        .find("if Platform.os == OperatingSystemType.windows: Text {")
        .expect("the icon font draws only on Windows");
    assert!(glyph[font..].contains("font-family: \"Segoe Fluent Icons\";"));
    let drawing = glyph
        .find("if Platform.os != OperatingSystemType.windows && root.glyph != \"\": Rectangle {")
        .expect("the drawing stands in everywhere else");
    let drawing = &glyph[drawing..];
    assert!(drawing.contains("commands: Glyphs.outline(root.glyph);"));
    assert!(drawing.contains("stroke: root.color;"));
    assert!(drawing.contains("commands: Glyphs.solid(root.glyph);"));
    assert!(drawing.contains("fill: root.color;"));

    let start = widgets.find("export global Glyphs {").expect("Glyphs");
    let glyphs = &widgets[start..];
    let glyphs = &glyphs[..glyphs.find("\n}").expect("it ends")];
    let mut icons = 0;
    for line in glyphs.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("out property <string> ") else {
            continue;
        };
        let name = rest.split(':').next().unwrap_or_default();
        icons += 1;
        assert!(
            glyphs.contains(&format!("if glyph == root.{name} {{")),
            "Glyphs.{name} has no drawing"
        );
    }
    assert!(icons > 25, "the check found the icons ({icons})");

    let mut drawings = 0;
    for line in glyphs.lines().map(str::trim) {
        let Some(commands) = line
            .strip_prefix("return \"")
            .and_then(|rest| rest.strip_suffix("\";"))
        else {
            continue;
        };
        if commands.is_empty() {
            continue;
        }
        drawings += 1;
        assert!(
            commands.starts_with('M'),
            "a drawing starts with a move: `{commands}`"
        );
        for token in commands.split_whitespace() {
            let number = token.trim_start_matches(|c| "MLHVAQZ".contains(c));
            assert!(
                token.len() - number.len() <= 1
                    && (number.is_empty()
                        || number
                            .parse::<f32>()
                            .is_ok_and(|n| (0.0..=16.0).contains(&n))),
                "`{token}` is not a path command or a coordinate on the 16-unit grid"
            );
        }
    }
    assert!(
        drawings > icons,
        "the check found the drawings ({drawings})"
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
/// speaker kind, what a listed speaker can't do yet, the AirPlay delay field
/// (spec S181), which shows only while the AirPlay sender runs, the notice
/// that AirPlay speakers playing can't wait long enough for a Google Cast
/// speaker, with its short name, and the notice
/// that the sender stopped or couldn't start, with its Retry button and its
/// short name for the dismiss button, the notice that another app holds
/// the PTP ports, and the note that discovery has found
/// no speaker yet (alone, or with Google Cast's in one sentence), which
/// shows only while it runs. The note and row state for a speaker waiting
/// for the one AirPlay 2 slot show only behind the AirPlay 2 switch, whose
/// File menu item says it is experimental, as do the notice that turning
/// the item on or off applies at the next launch and its short name.
/// Under the AirPlay 2 flag, off by default, the item reads Prefer AirPlay 2
/// (spec S313).
/// The row note and notice for a speaker that plays only AirPlay 2 name
/// that item, and show only for a receiver the sender found; so does the
/// note on a zone's stereo pair or home theater that needs it.
/// The calibration results' AirPlay lines, a zone run's line on whom the
/// shared AirPlay delay moves included, show only when an AirPlay speaker
/// took part, so the sender ran. The live input note shows only while an
/// AirPlay speaker is on, and steers live sound away from AirPlay.
/// An empty Connected Speakers list otherwise says just that it's empty, or
/// that its virtual outputs are hidden. Seven strings of the AudioSync Guide
/// name AirPlay too, with the receivers the sender plays, the ones it can't
/// yet, and the track info it sends.
#[test]
fn no_text_promises_airplay() {
    let strings = include_str!("../ui/strings.slint");
    for line in strings.lines().filter(|line| line.contains("@tr(")) {
        assert!(
            !line.contains("AirPlay")
                || line.contains("@tr(\"AirPlay\")")
                || line.contains("@tr(\"AirPlay 2\")")
                || line.contains("couldn't save your AirPlay pairings,")
                || line.contains("couldn't read your AirPlay pairings when it started,")
                || line.contains("not supported yet")
                || line.contains("doesn't support yet")
                || line.contains("return @tr(\"{} plays only AirPlay 2, so it's off.")
                || line.contains("return @tr(\"AirPlay 2 is off\");")
                || line.contains("return @tr(\"AirPlay 2 is off until AudioSync reopens\");")
                || line.contains("property <string> airplay-delay-")
                || line
                    .contains("notice-airplay-stopped: @tr(\"AirPlay stopped or couldn't start.")
                || line.contains("notice-retry-label: @tr(\"Start AirPlay again\")")
                || line.contains("return @tr(\"AirPlay stopped\");")
                || line.contains(
                    "notice-airplay-ptp-ports-taken: @tr(\"Another app is using port 319 or 320,"
                )
                || line.contains(
                    "return @tr(\"AirPlay speakers can wait at most 20 s for a Google Cast speaker,"
                )
                || line.contains("return @tr(\"AirPlay adds a {} s delay to every speaker.")
                || line.contains(
                    "live-input-airplay-unknown: @tr(\"AirPlay adds a delay to every speaker."
                )
                || line.contains("return @tr(\"AirPlay ahead of {}\", name);")
                || line.contains("return @tr(\"AirPlay delay: was {}. New: {} ms")
                || line.contains("return @tr(\"The AirPlay speakers differ by {} ms.")
                || line.contains("return @tr(\"No AirPlay speaker was measured clearly,")
                || line.contains("@tr(\"AirPlay speakers share one delay, so a new AirPlay delay")
                || line.contains(
                    "airplay-none-found: root.public-network ? \
                     @tr(\"No AirPlay speakers found yet.",
                )
                || line.contains("plays up to {0} AirPlay 2 speakers at once, so {1} waits.")
                || line.contains("@tr(\"Waiting: too many AirPlay 2 speakers\")")
                || line.contains(
                    "waits while AudioSync plays to AirPlay 2 speakers on another network."
                )
                || line.contains("@tr(\"Waiting: AirPlay 2 speakers on another network\")")
                || line.contains("airplay2-experimental: @tr(\"AirPlay 2 (Experimental)\")")
                || line.contains("airplay2-prefer: @tr(\"Prefer AirPlay 2\")")
                || line.contains("needs-airplay2: @tr(\"Needs AirPlay 2 (Experimental)\")")
                || line.contains("property <string> notice-airplay2-on-next-launch: @tr(")
                || line.contains("property <string> notice-airplay2-off-next-launch: @tr(")
                || line.contains("property <string> notice-airplay2-prefer-on-next-launch: @tr(")
                || line.contains("property <string> notice-airplay2-prefer-off-next-launch: @tr(")
                || line.contains("return @tr(\"AirPlay 2 after restart\");")
                || line.contains(
                    "network-none-found: root.public-network ? \
                     @tr(\"No AirPlay or Google Cast speakers found yet.",
                )
                || [
                    "help-intro",
                    "help-start",
                    "help-start-linux",
                    "help-sync",
                    "help-sync-cast",
                    "help-art-classes",
                    "help-volume",
                    "help-volume-linux",
                    "help-more",
                    "help-more-linux",
                    "help-airplay-title",
                    "help-airplay",
                    "help-airplay-linux",
                    "help-airplay-prefer",
                    "help-airplay-linux-prefer",
                    "help-calibration",
                    "help-calibration-linux",
                ]
                .iter()
                .any(|name| line.contains(&format!("out property <string> {name}: @tr("))),
            "strings.slint: {}",
            line.trim()
        );
    }
    assert!(strings.contains("connected-none: @tr(\"No connected speakers\")"));
    assert!(
        include_str!("../ui/main.slint")
            .contains("root.connected.length > 0 || root.connected-zoned.length > 0 ? \"\" : root.connected-all-hidden ? Strings.all-speakers-hidden : root.airplay-searching ? Strings.looking-for-speakers : root.connected-virtual-hidden ? Strings.connected-none-virtual-hidden : Strings.connected-none;"),
        "the empty Connected Speakers list says it looks while AirPlay searches, then uses the plain note"
    );
    assert!(
        include_str!("../ui/main.slint").contains(
            "property <string> cast-note: self.both-none || self.list-note == Strings.looking-for-speakers ? \"\" : Strings.cast-list(root.cast-list);"
        ),
        "while the list says it looks, no Google Cast note follows it"
    );
    assert!(
        include_str!("../ui/main.slint").contains("note: root.airplay-none-found ? ("),
        "the note that no AirPlay speaker was found shows only when the backend says so"
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
    assert!(!has_private_use("Strings.dismiss-notice(notice.message)"));
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
        "guide-route-all-except",
        "guide-keep-all-except-unmuted",
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

/// What Linux shows in place of a Windows text (a `-linux` Guide text, or the
/// Linux branch of a string) names nothing only Windows has.
#[test]
fn linux_texts_name_no_windows_places() {
    let strings = include_str!("../ui/strings.slint");
    let mut linux: Vec<&str> = strings
        .lines()
        .filter(|line| line.contains("-linux: @tr(\""))
        .collect();
    for (at, _) in strings.match_indices("OperatingSystemType.linux") {
        let rest = &strings[at + "OperatingSystemType.linux".len()..];
        if rest.starts_with(" ?") {
            linux.push(&rest[..rest.find(" : @tr(").expect("a Windows text follows")]);
            continue;
        }
        // An `if` block: up to the brace that closes it.
        let mut depth = 0;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => return false,
                }
                depth == 0
            })
            .map(|(i, _)| i)
            .expect("a Linux branch ends");
        linux.push(&rest[..end]);
    }
    assert!(linux.len() >= 15, "{} Linux texts", linux.len());
    for text in linux {
        for windows_only in [
            "Windows",
            "Volume mixer",
            "Start menu",
            "notification area",
            "quick settings",
        ] {
            assert!(!text.contains(windows_only), "{windows_only} in: {text}");
        }
    }
}

/// Every Windows text that has a Linux twin reads as in windows_strings.txt,
/// one per line as written in strings.slint, so Linux wording never changes
/// what Windows shows.
#[test]
fn windows_texts_with_a_linux_twin_are_unchanged() {
    let strings = include_str!("../ui/strings.slint");
    let snapshot = include_str!("windows_strings.txt");
    assert!(snapshot.lines().count() >= 48);
    for text in snapshot.lines() {
        assert!(
            strings.contains(&format!("@tr(\"{text}\"")),
            "a Windows text changed: {text}"
        );
    }
}

/// Help > AudioSync Guide (F1) opens the Guide, which spells out no text of
/// its own, closes on Escape, and keeps every paragraph's full height.
#[test]
fn the_audiosync_guide_is_wired_and_worded_in_strings() {
    let main = include_str!("../ui/main.slint");
    let item = main
        .find("title: Strings.help-title;")
        .expect("the Help menu has the Guide");
    let item = &main[item..item + 200];
    assert!(item.contains("@keys(F1)") && item.contains("root.help-requested();"));
    assert!(main.contains("export { HelpWindow } from \"help.slint\";"));

    let help = include_str!("../ui/help.slint");
    assert!(!help.contains("text: \""), "help.slint spells out no text");
    assert!(help.contains("Key.Escape") && help.contains("root.close-requested();"));
    let wrapping = help.matches("wrap: word-wrap;").count();
    assert_eq!(
        help.matches("min-height: self.preferred-height;").count(),
        wrapping,
        "every wrapping text in the Guide keeps the height of all its lines"
    );
    // Each section is a group Narrator names by its heading.
    let sections = help.matches("HelpSection {").count();
    assert!(sections >= 8, "{sections} sections");
    assert_eq!(help.matches("title: Strings.help-").count(), sections + 1);

    let strings = include_str!("../ui/strings.slint");
    let lines: Vec<&str> = strings
        .lines()
        .filter(|line| line.contains("out property <string> help-"))
        .collect();
    assert!(lines.len() > sections * 2, "a title and a text per section");
    for line in lines {
        assert!(line.contains(": @tr(\""), "translatable: {line}");
        assert!(!line.contains('—'), "no em dash: {line}");
        let name = line
            .split("help-")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .unwrap_or_default();
        assert!(
            help.contains(&format!("Strings.help-{name};"))
                || help.contains(&format!("Strings.help-{name} :")),
            "help.slint shows help-{name}"
        );
    }
    // The Mac Guide's three illustrations, each in both themes with its
    // description for Narrator.
    for art in ["source", "delay", "classes"] {
        assert!(
            help.contains(&format!(
                "light: @image-url(\"../../brand/guide/{art}.svg\");"
            )) && help.contains(&format!(
                "dark: @image-url(\"../../brand/guide/{art}-dark.svg\");"
            )) && help.contains(&format!("description: Strings.help-art-{art};")),
            "the Guide shows the {art} illustration"
        );
    }
    assert!(help.contains("source: Tokens.dark ? root.dark : root.light;"));
}

/// The body of `function` in strings.slint, up to its closing brace.
fn strings_function(function: &str) -> &'static str {
    let strings = include_str!("../ui/strings.slint");
    let start = strings
        .find(&format!("public pure function {function}("))
        .unwrap_or_else(|| panic!("strings.slint has {function}"));
    let body = &strings[start..];
    &body[..body.find("\n    }").expect("the function ends")]
}

/// The Windows columns of the device-volume wording table in the spec
/// (S199): each case with its hint and its button, "n" standing for the
/// percent.
fn spec_device_volume_wording() -> Vec<(String, String, String)> {
    let spec = include_str!("../../docs/ENGINE_SPEC.md");
    let header = spec
        .find("| Case | Mac hint | Mac button | Windows hint | Windows button |")
        .expect("the spec has the wording table");
    spec[header..]
        .lines()
        .skip(2)
        .take_while(|line| line.trim_start().starts_with('|'))
        .map(|line| {
            let cells: Vec<&str> = line
                .trim()
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            (
                cells[0].to_owned(),
                cells[3].to_owned(),
                cells[4].to_owned(),
            )
        })
        .collect()
}

// covers: S199, S200 (the Windows host)
/// The device-volume hint under a speaker's volume slider (S198 to S200):
/// worded as the spec says, in strings.slint; one text element whose label
/// names the output; the fix a link-style button whose label starts with its
/// visible text and then names the output; both right after the slider in
/// reading order; the glyph decorative.
// covers: S199, S200
#[test]
fn the_hint_and_the_fix_name_the_output_for_narrator() {
    let wording = spec_device_volume_wording();
    let cases: Vec<&str> = wording.iter().map(|(case, _, _)| case.as_str()).collect();
    assert_eq!(cases, ["low", "muted", "muted and low"]);
    let hint = strings_function("device-volume-hint");
    let fix = strings_function("device-volume-fix");
    for (case, hint_text, fix_text) in &wording {
        // "n%" is the percent and "75%" the fix's percent: both come in as {}.
        let hint_text = hint_text.replace("n%", "{}%");
        let fix_text = fix_text.replace("75%", "{}%");
        assert!(
            hint.contains(&format!("@tr(\"{hint_text}\"")),
            "{case}: the hint reads {hint_text:?}"
        );
        assert!(
            fix.contains(&format!("@tr(\"{fix_text}\"")),
            "{case}: the button reads {fix_text:?}"
        );
    }
    let strings = include_str!("../ui/strings.slint");
    let fix_percent = (audiosync_core::policy::device_volume::FIX_VOLUME * 100.0).round();
    assert!(
        strings.contains(&format!(
            "out property <int> device-volume-fix-percent: {fix_percent};"
        )),
        "the buttons name the volume the fix sets"
    );
    assert!(fix.contains("root.device-volume-fix-percent"));

    // Narrator: the output's name, then the hint (S200).
    let hint_label = strings_function("device-volume-hint-label");
    for label in [
        "@tr(\"{0}: Windows volume is {1}%\", name, percent)",
        "@tr(\"{}: muted in Windows\", name)",
        "@tr(\"{0}: muted in Windows, volume {1}%\", name, percent)",
    ] {
        assert!(hint_label.contains(label), "the hint's label has {label}");
    }
    // The fix's label starts with what the button shows.
    let fix_label = strings_function("device-volume-fix-label");
    assert!(fix_label.contains("@tr(\"{0}, {1}\", root.device-volume-fix(hint), name)"));

    let widgets = include_str!("../ui/widgets.slint");
    let row = component_body(widgets, "SpeakerRow");
    let slider = row.find("VolumeSlider {").expect("the row's slider");
    let hint_at = row
        .find(
            "accessible-label: Strings.device-volume-hint-label(root.item.name, \
             root.item.device-volume, root.item.device-volume-percent);",
        )
        .expect("the hint names the output");
    let fix_at = row
        .find("label: Strings.device-volume-fix-label(root.item.name, root.item.device-volume);")
        .expect("the fix names the output");
    let options = row
        .find("options := IconButton {")
        .expect("the options button");
    assert!(
        slider < hint_at && hint_at < fix_at && !(slider..fix_at).contains(&options),
        "the hint and the fix come right after the slider"
    );
    let shown = row
        .find("if root.item.device-volume != DeviceVolumeHint.none:")
        .expect("the hint shows only with a hint to show");
    assert!(slider < shown && shown < hint_at);
    let sync = row
        .find("if root.item.has-delay && root.item.sync-open:")
        .expect("the sync controls");
    let part = &row[shown..sync];
    assert!(part.contains("accessible-role: text;"));
    assert!(part.contains(
        "text: Strings.device-volume-hint(root.item.device-volume, \
         root.item.device-volume-percent);"
    ));
    assert!(part.contains("text: Strings.device-volume-fix(root.item.device-volume);"));
    assert!(part.contains("root.device-volume-fix();"));
    assert!(part.contains("color: Tokens.secondary-text;"));
    assert!(
        part.contains("Glyph {")
            && part.contains("glyph: Glyphs.device-volume(root.item.device-volume);"),
        "the glyph is a decorative Glyph"
    );
    assert!(
        part.contains("LinkButton {"),
        "the fix is a link-style button"
    );
    let link = component_body(widgets, "LinkButton");
    for needle in [
        "accessible-role: button;",
        "accessible-label: root.label;",
        "accessible-action-default",
        "FocusScope",
        "FocusRing",
        "color: Tokens.teal-text;",
    ] {
        assert!(link.contains(needle), "LinkButton has {needle}");
    }
    assert!(
        !link.contains("border-width: 1"),
        "a link-style button is borderless"
    );
    // The click reaches the backend.
    assert!(include_str!("../ui/main.slint").contains("root.device-volume-fix(speaker.id);"));
    assert!(include_str!("view.rs").contains("b.fix_device_volume(&id)"));
}

/// The fix goes away with its hint once Windows reports the change, so a
/// fix pressed from the keyboard first hands the focus to the row's volume
/// slider, right before it in reading order; the focus stays in the row
/// rather than being lost (S198). A mouse click leaves the focus alone.
// covers: S198
#[test]
fn the_fix_keeps_the_keyboard_focus_in_the_row() {
    let widgets = include_str!("../ui/widgets.slint");
    let slider = component_body(widgets, "VolumeSlider");
    let function = slider
        .find("public function focus-slider() {")
        .expect("VolumeSlider can take the focus");
    assert!(slider[function..].contains("slider.focus();"));

    let row = component_body(widgets, "SpeakerRow");
    assert!(row.contains("volume-slider := VolumeSlider {"));
    let button = row
        .find("fix-button := LinkButton {")
        .expect("the fix button is named");
    let handler = &row[button..];
    let handler = &handler[handler.find("clicked => {").expect("its click")..];
    let focus = handler
        .find("if fix-button.keyboard-focused {")
        .expect("only a keyboard focus moves");
    let moved = handler
        .find("volume-slider.focus-slider();")
        .expect("the focus moves to the slider");
    let fixed = handler
        .find("root.device-volume-fix();")
        .expect("then the fix");
    assert!(
        focus < moved && moved < fixed,
        "the focus moves before the fix"
    );
    let link = component_body(widgets, "LinkButton");
    assert!(link.contains("out property <bool> keyboard-focused: focus.has-focus;"));
    assert!(link.contains("focus-on-click: false;"));
}

/// A zone card's speaker row shows why a zone click left it off ("Stayed
/// off, since one Cast speaker plays at a time"); its toggle hands Narrator
/// the same note, as the compact row under "In zones" does.
#[test]
fn a_zone_cards_row_reads_its_zone_note_to_narrator() {
    let widgets = include_str!("../ui/widgets.slint");
    for component in ["SpeakerRow", "ZoneMemberRow"] {
        let row = component_body(widgets, component);
        assert!(
            row.contains(
                "property <string> note: root.item.zone-note != \"\" ? root.item.zone-note"
            ),
            "{component} shows the zone note first"
        );
        let badge = &row[row
            .find("badge := SpeakerBadge {")
            .expect("the row's toggle")..];
        let description = badge
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("description:"))
            .expect("the toggle has a description");
        assert!(
            description.contains("root.item.zone-note") || description.contains("root.note"),
            "{component}'s toggle reads the zone note: {description}"
        );
    }
}

/// The tray flyout's row of a switched-on local output shows the state as a
/// small glyph in the secondary text color, with no button, and Narrator
/// hears the hint in the row's description (S198, S199).
// covers: S198
#[test]
fn the_flyout_row_shows_the_glyph_and_the_hint_in_its_description() {
    let flyout = include_str!("../ui/flyout.slint");
    let row = component_body(flyout, "FlyoutRow");
    assert!(row.contains("in property <DeviceVolumeHint> device-volume;"));
    let name = row.find("text: root.name;").expect("the row's name");
    let glyph = row
        .find("if root.device-volume != DeviceVolumeHint.none: Glyph {")
        .expect("the glyph shows only with a hint");
    assert!(name < glyph, "the glyph sits at the row's right");
    let glyph_part = &row[glyph..];
    assert!(glyph_part.contains("glyph: Glyphs.device-volume(root.device-volume);"));
    assert!(glyph_part.contains("color: Tokens.secondary-text;"));
    assert!(
        !row.contains("Button {"),
        "no button in the flyout row; the fix is in the main window"
    );
    assert!(flyout.contains("device-volume: speaker.device-volume;"));
    assert!(flyout.contains(
        "description: Strings.flyout-speaker-description(Strings.help(speaker.state), \
         Strings.device-volume-hint(speaker.device-volume, speaker.device-volume-percent));"
    ));
    let description = strings_function("flyout-speaker-description");
    assert!(description.contains("if hint == \"\" {"));
    assert!(description.contains("@tr(\"{0}. {1}\", hint, help)"));

    // Windows' own Mute glyph (a speaker with a cross: Segoe Fluent Icons
    // has no speaker with a slash) while muted, a speaker with one wave while
    // low.
    let widgets = include_str!("../ui/widgets.slint");
    assert!(widgets.contains("out property <string> volume-muted: \"\\u{E74F}\";"));
    assert!(widgets.contains("out property <string> volume-low: \"\\u{E993}\";"));
    let start = widgets
        .find("public pure function device-volume(hint: DeviceVolumeHint) -> string {")
        .expect("Glyphs.device-volume");
    let function = &widgets[start..];
    let function = &function[..function.find("\n    }").expect("it ends")];
    assert!(function.contains("return root.volume-muted;"));
    assert!(function.contains("return root.volume-low;"));
}

/// A tray flyout row shows its speaker's note on the name's line (a waiting
/// speaker's reason, for one, after a zone's count), as a main window row
/// does. The source is read
/// line by line: Windows CI checks it out with CRLF line endings.
#[test]
fn a_flyout_row_shows_the_speakers_note() {
    let row = component_body(include_str!("../ui/flyout.slint"), "FlyoutRow");
    let lines: Vec<&str> = row.lines().map(str::trim).collect();
    let note = lines
        .iter()
        .position(|line| {
            *line
                == "if Strings.speaker-caption(root.caption, Strings.row-note(root.state)) != \"\": QuietText {"
        })
        .expect("FlyoutRow shows a QuietText while its state has a note");
    assert_eq!(
        lines.get(note + 1).copied(),
        Some("text: Strings.speaker-caption(root.caption, Strings.row-note(root.state));"),
        "the QuietText shows the note"
    );
}

/// Selected text keeps 4.5:1: every text box is a TextField, which selects
/// in the "surface" on "tealText" pair, and none is the style's LineEdit,
/// whose accent selection measures 4.21:1 under white text.
#[test]
fn selected_text_in_a_text_box_reaches_4_5_to_1() {
    for (file, source) in UI_FILES {
        assert!(
            blocks(source, "LineEdit").is_empty(),
            "{file} uses LineEdit"
        );
    }
    let field = component_body(include_str!("../ui/widgets.slint"), "TextField");
    assert!(field.contains("selection-background-color: Tokens.teal-text;"));
    assert!(field.contains("selection-foreground-color: Tokens.surface;"));
    assert!(CONTRAST_PAIRS.contains(&("surface", "tealText")));
}

// covers: S308
/// The updater's notice and menu items: words from strings.slint, token
/// colors only, text at 4.5:1 on the notice in light and dark, and Narrator
/// hears the notice's text and each button's name.
#[test]
fn the_updater_notice_uses_strings_and_tokens() {
    let stack = component_body(include_str!("../ui/widgets.slint"), "NoticeStack");
    for line in stack.lines().map(str::trim) {
        if ["color:", "background:", "border-color:"]
            .iter()
            .any(|key| line.starts_with(key))
        {
            assert!(
                !line.contains('#') && !line.contains("rgb"),
                "a token, not a literal: {line}"
            );
        }
        if ["text:", "accessible-label:", "label:"]
            .iter()
            .any(|key| line.starts_with(key))
        {
            assert!(!line.contains('"'), "words from strings.slint: {line}");
        }
    }
    for needle in [
        "background: Tokens.notice;",
        "accessible-label: notice.message;",
        "text: Strings.update-button(button);",
        "accessible-label: Strings.update-button(button);",
        "label: Strings.dismiss-notice(notice.title);",
    ] {
        assert!(stack.contains(needle), "NoticeStack has {needle}");
    }
    // The buttons' own fill and label use Slint's Fluent palette, which no token reaches.
    for pair in [
        ("text", "notice"),
        ("text@secondaryText", "notice"),
        ("muted", "notice"),
    ] {
        assert!(CONTRAST_PAIRS.contains(&pair), "{pair:?} is checked");
        for appearance in [Appearance::Light, Appearance::Dark] {
            let value = ratio(pair.0, pair.1, appearance);
            assert!(
                value >= MIN_TEXT_CONTRAST,
                "{pair:?} ({appearance:?}): {value:.2}:1"
            );
        }
    }
    let main = include_str!("../ui/main.slint");
    for title in [
        "title: Strings.check-updates-automatically;",
        "title: Strings.menu-check-for-updates;",
    ] {
        assert!(main.contains(title), "main.slint has {title}");
    }
    let strings = include_str!("../ui/strings.slint");
    for label in [
        "Download and Install",
        "Not Now",
        "Cancel",
        "Try Again",
        "Open Release Page",
        "Restart Now",
    ] {
        assert!(
            strings.contains(&format!("@tr(\"{label}\")")),
            "strings.slint words {label}"
        );
    }
}

// covers: S268 (the Windows host)
/// The preset controls (spec S267, S268): token colors only, a focus ring,
/// Narrator's label, value and hint on each row, F2 for Rename, and the
/// Menu key and Shift+F10 through the row's context menu area.
#[test]
fn the_preset_controls_use_tokens_and_reach_the_keyboard() {
    let main = include_str!("../ui/main.slint");
    let flyout = include_str!("../ui/flyout.slint");
    let parts = [
        component_body(main, "PresetRow"),
        component_body(main, "PresetsCard"),
        component_body(flyout, "PresetPicker"),
    ];
    for part in parts {
        for line in part.lines().map(str::trim) {
            if line.starts_with("color:") || line.starts_with("background:") {
                assert!(
                    !line.contains('#') && !line.contains("rgb"),
                    "a token, not a literal: {line}"
                );
            }
        }
    }
    let [row, card, picker] = parts;
    for needle in [
        "accessible-role: button;",
        "accessible-label: root.item.name;",
        "accessible-value: root.item.current ? Strings.preset-current : \"\";",
        "accessible-description: Strings.preset-recall-hint;",
        "if event.text == Key.F2 {",
        "menu := ContextMenuArea {",
        "border-color: Tokens.focus;",
        "label: Strings.speaker-options(root.item.name);",
        "color: Tokens.teal-text;",
    ] {
        assert!(row.contains(needle), "PresetRow has {needle}");
    }
    assert!(card.contains("accessible-role: groupbox;"));
    assert!(card.contains("text: Strings.presets-empty;"));
    assert!(card.contains("text: Strings.save-preset;"));
    for needle in [
        "accessible-role: button;",
        "accessible-label: Strings.presets;",
        "border-color: Tokens.focus;",
        "if event.text == \" \" || event.text == Key.Return {",
    ] {
        assert!(picker.contains(needle), "PresetPicker has {needle}");
    }
    assert!(flyout.contains("if root.presets.length > 0: PresetPicker {"));
}

/// A zone header works like a speaker row's box: only its badge switches the
/// zone, and a click anywhere else on the header shows or hides the speakers
/// (on the name, once the double-click time has passed).
#[test]
fn only_a_zone_headers_badge_switches_the_zone() {
    let header = component_body(include_str!("../ui/widgets.slint"), "ZoneHeader");
    let hover = header
        .find("hover := TouchArea {")
        .expect("the header has its box's click");
    let click = &header[hover..hover + 120];
    assert!(click.contains("root.expanded-changed(!root.item.expanded);"));
    assert_eq!(header.matches("root.toggled();").count(), 1);
    let badge = header
        .find("badge := SpeakerBadge {")
        .expect("the header has its badge");
    assert!(header[badge..].find("root.toggled();").unwrap() < 800);
    assert_eq!(
        header.matches("TouchArea {").count(),
        3,
        "the box, the name and the arrow"
    );
    // A FocusScope that takes the focus on click accepts the press (Slint
    // 1.18), so the box's TouchArea would never see a click on the name.
    assert_eq!(
        header.matches("FocusScope {").count(),
        header.matches("focus-on-click: false;").count(),
        "every FocusScope in the header lets a click through to the box"
    );
    let wait = header
        .find("click-wait := Timer {")
        .expect("the name waits for a second click");
    assert!(header[wait..wait + 300].contains("root.expanded-changed(!root.item.expanded);"));
}

/// The flyout's zone rows follow the main window's header: only the badge
/// switches the zone, a click anywhere else shows or hides its speakers, and
/// a folded zone lists none.
#[test]
fn only_a_flyout_zone_badge_switches_the_zone() {
    let flyout = include_str!("../ui/flyout.slint");
    let header = component_body(flyout, "FlyoutZoneHeader");
    let hover = header
        .find("hover := TouchArea {")
        .expect("the row has its click");
    assert!(header[hover..hover + 120].contains("root.expanded-changed(!root.item.expanded);"));
    assert_eq!(header.matches("root.toggled();").count(), 1);
    let badge = header
        .find("SpeakerBadge {")
        .expect("the row has its badge");
    assert!(header[badge..].find("root.toggled();").unwrap() < 800);
    assert!(
        header.contains("expandable: true;"),
        "Left and Right fold it"
    );
    assert_eq!(header.matches("TouchArea {").count(), 1, "the row only");
    assert!(
        !header.contains("accessible-expanded:"),
        "UI Automation offers Invoke only on an arrow with no expanded state"
    );
    assert!(flyout.contains("for speaker in zone.expanded ? zone.members : []: FlyoutRow {"));
    assert!(flyout.contains("root.zone-expanded-changed(zone.id, open);"));
}

/// With the equalizer off, the gain values and band labels stay readable:
/// only the sliders fade, never the text around them.
#[test]
fn an_equalizer_turned_off_fades_only_the_sliders() {
    let source = include_str!("../ui/equalizer.slint");
    let sliders: usize = blocks(source, "Slider")
        .iter()
        .map(|block| block.matches("Tokens.disabled-opacity").count())
        .sum();
    assert!(
        sliders > 0,
        "the band's slider fades while the equalizer is off"
    );
    assert_eq!(
        source.matches("Tokens.disabled-opacity").count(),
        sliders,
        "nothing but the sliders fades in equalizer.slint"
    );
}
