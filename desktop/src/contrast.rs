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
            &["hero"],
            &["hero", "hover"],
            &["hero", "pressed"],
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

/// A `Fluent` color written `Tokens.dark ? #dark : #light` in widgets.slint,
/// as light and dark sRGB plus alpha.
fn fluent_color(name: &str) -> [[f64; 4]; 2] {
    let prefix = format!("out property <color> {name}: Tokens.dark ? ");
    let line = include_str!("../ui/widgets.slint")
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("widgets.slint defines Fluent.{name} for both themes"));
    let parse = |hex: &str| {
        let hex = hex.trim().trim_start_matches('#').trim_end_matches(';');
        let byte = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap()) / 255.0;
        [
            byte(0),
            byte(2),
            byte(4),
            if hex.len() == 8 { byte(6) } else { 1.0 },
        ]
    };
    let (dark, light) = line.split_once(" : ").expect("a dark and a light value");
    [parse(light), parse(dark)]
}

/// The focus ring is the Mac's teal line (Brand.swift:25-26,
/// HouseButtonStyle.swift:60, 2 px): it stands out from what it's drawn on
/// (3:1 for graphics) in both themes, the main window and its hero included.
// covers: S452
#[test]
fn the_teal_focus_ring_reaches_3_to_1() {
    let ring = include_str!("../ui/widgets.slint");
    let ring = &ring[ring.find("export component FocusRing").expect("FocusRing")..];
    let ring = &ring[..ring.find("\n}").expect("its end")];
    assert!(ring.contains("border-width: root.drawn ? 2px : 0px;"));
    assert!(ring.contains("border-color: Tokens.focus;"));
    assert_eq!(
        ring.matches("Rectangle {").count(),
        1,
        "one line, as the Mac's"
    );
    let window = fluent_color("main-window");
    for (index, appearance) in [Appearance::Light, Appearance::Dark]
        .into_iter()
        .enumerate()
    {
        let focus = srgb("focus", appearance);
        let [r, g, b, _] = window[index];
        let page = [r, g, b];
        let text = srgb("text", appearance);
        let hero_alpha = fluent_fill_alpha("hero");
        let hero = [0, 1, 2].map(|i| text[i] * hero_alpha + page[i] * (1.0 - hero_alpha));
        let mut unders = vec![("mainWindow", page), ("mainWindow+hero", hero)];
        for name in [
            "background",
            "surface",
            "playingRow",
            "notice",
            "settingsWindow",
            "settingsCard",
        ] {
            unders.push((name, srgb(name, appearance)));
        }
        for (name, under) in unders {
            let value = contrast(focus, under);
            assert!(
                value >= MIN_GRAPHIC_CONTRAST,
                "ring on {name} ({appearance:?}): {value:.2}:1"
            );
        }
    }
}

/// The Add by Address sheet's boxed field shows its edge at 3:1 before it
/// has the focus (a control's boundary), and the focus outline after.
#[test]
fn the_boxed_fields_edge_reaches_3_to_1() {
    for appearance in [Appearance::Light, Appearance::Dark] {
        let value = ratio("text@secondaryText", "surface", appearance);
        assert!(
            value >= MIN_GRAPHIC_CONTRAST,
            "edge on the sheet ({appearance:?}): {value:.2}:1"
        );
    }
    let field = component_body(include_str!("../ui/widgets.slint"), "TextField");
    assert!(field.contains(
        "border-color: root.boxed ? (input.has-focus ? Tokens.focus : Tokens.secondary-text) : root.compact && input.has-focus ? Tokens.teal : root.raised && !Tokens.dark ? #00000026 : Fluent.stroke;"
    ));
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

/// An opening notice in the main window, as on the Mac (WindowRules.swift
/// `NoticeMotion.opacity`): its room opens over 240 ms and it fades in over
/// 180 ms from 60 ms, opaque as the room lands; a closing one fades over
/// 160 ms. With animation effects off it cross-fades over 150 ms. Read line
/// by line: a Windows checkout may have CRLF line ends.
// covers: S450
#[test]
fn an_opening_notice_fades_in_as_its_room_lands() {
    let stack = component_body(include_str!("../ui/widgets.slint"), "NoticeStack");
    let lines: Vec<&str> = stack.lines().map(str::trim).collect();
    let after = |head: &str| {
        let at = lines
            .iter()
            .position(|line| *line == head)
            .unwrap_or_else(|| panic!("{head}"));
        (lines[at + 1], lines[at + 2])
    };
    assert_eq!(
        after("animate open {").0,
        "duration: Motion.on ? 240ms : 0ms;"
    );
    assert_eq!(
        after("animate shown {"),
        (
            "duration: Motion.on ? (self.closing ? 160ms : 180ms) : 150ms;",
            "delay: Motion.on && !self.closing ? 60ms : 0ms;"
        )
    );
}

/// A warning notice's orange box, an info notice's teal one and the small
/// button's tint, as the Mac's: the text on each at 4.5:1 and the notice's
/// icon at 3:1, on the window and on a card, in light and dark.
#[test]
fn the_notice_boxes_and_the_small_button_keep_their_text_readable() {
    let opaque = |rgba: [f64; 4]| [rgba[0], rgba[1], rgba[2]];
    for (index, appearance) in [Appearance::Light, Appearance::Dark]
        .into_iter()
        .enumerate()
    {
        let warn_box = opaque(fluent_color("warn-background")[index]);
        let ink = opaque(fluent_color("house-text")[index]);
        let pressed = if index == 0 { 0.28 } else { 0.30 };
        for under in ["background", "surface"] {
            let below = srgb(under, appearance);
            let teal = srgb("teal", appearance);
            let tint = |alpha: f64| [0, 1, 2].map(|i| teal[i] * alpha + below[i] * (1.0 - alpha));
            for (name, fill, icon) in [
                ("warning", warn_box, "warn"),
                ("info", tint(0.12), "tealText"),
            ] {
                for text in ["text", "text@secondaryText"] {
                    let value = contrast(composite(text, fill, appearance), fill);
                    assert!(
                        value >= MIN_TEXT_CONTRAST,
                        "{text} on the {name} box over {under} ({appearance:?}): {value:.2}:1"
                    );
                }
                let value = contrast(srgb(icon, appearance), fill);
                assert!(
                    value >= MIN_GRAPHIC_CONTRAST,
                    "{icon} icon on the {name} box over {under} ({appearance:?}): {value:.2}:1"
                );
            }
            let value = contrast(ink, tint(pressed));
            assert!(
                value >= MIN_TEXT_CONTRAST,
                "small button pressed over {under} ({appearance:?}): {value:.2}:1"
            );
        }
    }
}

/// The master volume rows as on the Mac (MenuBar.swift `MasterSlider`): the
/// speaker with one, two or three waves by the level, crossed out at 0, in a
/// fixed slot; the track and the knob's edge in the
/// secondary text color, 3:1 on the window and the tray panel; the number
/// in the row titles' size and color.
// covers: S450
#[test]
fn the_master_volume_draws_its_levels_waves_and_a_3_to_1_track() {
    for appearance in [Appearance::Light, Appearance::Dark] {
        for background in ["background", "surface"] {
            let value = ratio("text@secondaryText", background, appearance);
            assert!(
                value >= MIN_GRAPHIC_CONTRAST,
                "track on {background} ({appearance:?}): {value:.2}:1"
            );
        }
    }
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    let start = widgets
        .find("public pure function master-volume(waves: int) -> string {")
        .expect("Glyphs.master-volume");
    let function = &widgets[start..];
    let function = &function[..function.find("\n    }").expect("it ends")];
    assert!(function.contains(
        "if waves <= 0 {\n            return root.volume-muted;\n        }\n        \
         if waves == 1 {\n            return root.volume-low;\n        }\n        \
         if waves == 2 {\n            return root.volume-mid;\n        }\n        \
         return root.volume-high;"
    ));
    let master = component_body(&widgets, "MasterVolume");
    assert!(
        master.contains(
            "width: 16px;\n                color: root.icon-color;\n                \
             glyph: Glyphs.master-volume(root.waves);"
        ),
        "one slot for every wave count, so the slider never moves"
    );
    assert!(master.contains("track: Tokens.secondary-text;"));
    assert!(
        !master.contains("Tokens.disabled-opacity"),
        "no faint waves"
    );
    assert!(master.contains("font-size: Tokens.font-size-body;"));
    assert!(master.contains("color: Tokens.text;"));
    let slider = component_body(&widgets, "LevelSlider");
    assert!(
        slider.contains("background: root.small ? (piece == 0 && root.idle ? (Tokens.dark ? #ffffff25 : #00000025) : (Tokens.dark ? #ffffff19 : #00000019)) : root.track;")
    );
    assert!(slider.contains("border-width: root.small && Tokens.dark ? 0px : 1px;"));
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
    ("setup.slint", include_str!("../ui/setup.slint")),
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

/// Dark body text is the Mac's label color as its window shows it
/// (Color.primary, Brand.swift:54: white at 84.7% over #323232 is 223.6,
/// which rounds to E0, 224),
/// not the wordmark's off-white; secondary text follows at 0.7.
#[test]
fn dark_body_text_is_the_macs_label_color() {
    assert_eq!(token("text").dark, [0xe0, 0xe0, 0xe0]);
}

/// An off speaker's toggle glyph is the Mac's `.secondary`
/// (MenuBar.swift:2199-2205): black at 50% on light, white at 55% on dark.
#[test]
fn an_off_speakers_glyph_is_the_macs_secondary() {
    let line = include_str!("../ui/widgets.slint")
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("property <color> ink: root.plays ?"))
        .expect("the toggle's ink");
    let off = &line[line
        .find("RowState.off ? (Tokens.dark ? ")
        .expect("an off branch")
        + 30..];
    let rgba = |hex: &str| -> [u8; 4] {
        [0, 2, 4, 6].map(|i| u8::from_str_radix(&hex[i + 1..i + 3], 16).expect("a hex colour"))
    };
    let (dark, light) = (rgba(&off[..9]), rgba(&off[12..21]));
    assert_eq!(&dark[..3], &[255, 255, 255], "{line}");
    assert!((f64::from(dark[3]) / 255.0 - 0.55).abs() < 0.005, "{line}");
    assert_eq!(&light[..3], &[0, 0, 0], "{line}");
    assert!((f64::from(light[3]) / 255.0 - 0.5).abs() < 0.005, "{line}");
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
        (main, "QuickRow"),
        (main, "SourceMenu"),
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

/// The light Appearance track looks like the Mac's AppKit control: white
/// with a soft shadow in light, a grey fill in dark that keeps the text at
/// 4.5:1 on the window and on a card.
#[test]
fn the_appearance_track_looks_like_the_macs_native_control() {
    let settings = include_str!("../ui/settings.slint");
    let track = settings
        .split("accessible-label: Strings.appearance;")
        .next()
        .and_then(|before| before.rsplit("Rectangle {").next())
        .expect("the Appearance track");
    for needle in [
        "Fluent.native-control",
        "drop-shadow-color: Fluent.native-control-shadow",
    ] {
        assert!(track.contains(needle), "the Appearance track has {needle}");
    }
    let prefix = "out property <brush> native-control: Tokens.dark ? Tokens.text.with-alpha(";
    let alpha: f64 = include_str!("../ui/widgets.slint")
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .and_then(|rest| rest.split(')').next())
        .and_then(|value| value.parse().ok())
        .expect("Fluent.native-control is the text color over the window in dark");
    let text = srgb("text", Appearance::Dark);
    for under in ["surface", "settingsCard"] {
        let below = srgb(under, Appearance::Dark);
        let fill = [0, 1, 2].map(|i| text[i] * alpha + below[i] * (1.0 - alpha));
        let value = contrast(text, fill);
        assert!(
            value >= MIN_TEXT_CONTRAST,
            "text on the grey control over {under}: {value:.2}:1"
        );
    }
}

/// The delay steps are the Mac's small bordered buttons as its 2x render
/// draws them (MainWindow.swift:3500-3515): a faint grey pill, the text color
/// at 5% (#f4f4f4 on white, #282828 on #1e1e1e), with no border or shadow,
/// so they show on a white card. Both delay fields have the Mac field's
/// #d9d9d9 edge in light.
#[test]
fn delay_steps_are_grey_pills_and_fields_have_an_edge_in_light() {
    let widgets = include_str!("../ui/widgets.slint");
    let step = component_body(widgets, "DelayStep");
    assert!(
        step.contains(
            "background: Tokens.text.with-alpha(touch.pressed ? (Tokens.dark ? 0.3 : 0.1) : 0.05);"
        ),
        "a delay step is the text color at 5%"
    );
    for needle in ["border-width", "drop-shadow"] {
        assert!(!step.contains(needle), "a delay step draws no {needle}");
    }
    for appearance in [Appearance::Light, Appearance::Dark] {
        let text = srgb("text", appearance);
        let below = srgb("surface", appearance);
        let fill = [0, 1, 2].map(|i| text[i] * 0.05 + below[i] * 0.95);
        let value = contrast(text, fill);
        assert!(
            value >= MIN_TEXT_CONTRAST,
            "a step's digits on its fill: {value:.2}:1"
        );
    }
    let field = component_body(widgets, "TextField");
    for needle in [
        "border-width: root.boxed && input.has-focus ? 2px : 1px;",
        "root.raised && !Tokens.dark ? #00000026 : Fluent.stroke;",
    ] {
        assert!(field.contains(needle), "the text field has {needle}");
    }
    assert_eq!(
        widgets.matches("raised: true;").count(),
        2,
        "both delay fields are raised"
    );
}

/// The calibration Cancel is the Mac's small push button
/// (CalibrationViews.swift:53-56): the delay steps' faint grey, its text
/// readable at rest and pressed on the window and on a card.
#[test]
fn the_calibration_cancel_keeps_its_text_readable() {
    let widgets = include_str!("../ui/widgets.slint");
    let button = component_body(widgets, "SmallButton");
    assert!(button.contains(
        "(root.grey ? Tokens.text : Tokens.teal).with-alpha(touch.pressed ? (Tokens.dark ? 0.3 : 0.1) : 0.05)"
    ));
    assert!(button.contains("color: root.dimmed ? Tokens.text.with-alpha(0.5)"));
    let controls = component_body(widgets, "SyncControls");
    let cancel = blocks(controls, "SmallButton");
    assert!(
        cancel
            .iter()
            .any(|b| b.contains("Strings.calibration-cancel") && b.contains("grey: true;")),
        "Cancel is the small grey push button"
    );
    for (appearance, pressed) in [(Appearance::Light, 0.1), (Appearance::Dark, 0.3)] {
        let text = srgb("text", appearance);
        for under in ["background", "surface"] {
            let below = srgb(under, appearance);
            for alpha in [0.05, pressed] {
                let fill = [0, 1, 2].map(|i| text[i] * alpha + below[i] * (1.0 - alpha));
                let value = contrast(text, fill);
                assert!(
                    value >= MIN_TEXT_CONTRAST,
                    "Cancel at {alpha} over {under} ({appearance:?}): {value:.2}:1"
                );
            }
        }
    }
}

/// A zone header's "2 of 3 on" is in the readable teal while a member is on,
/// its waiting part and note grey, as the Mac's (MainWindow.swift:2042-2048);
/// "tealText" keeps 4.5:1 through its pairs on the window, card and row.
#[test]
fn a_zones_on_count_is_teal_and_its_waiting_part_grey() {
    let widgets = include_str!("../ui/widgets.slint");
    assert!(
        widgets.contains("count-text := ZoneCount {"),
        "the zone header draws its count in two runs"
    );
    let count = component_body(widgets, "ZoneCount");
    for needle in [
        "color: root.on > 0 ? Tokens.teal-text : Tokens.secondary-text;",
        "text: root.rest;",
        "color: Tokens.secondary-text;",
    ] {
        assert!(count.contains(needle), "the zone count has {needle}");
    }
    assert!(CONTRAST_PAIRS.contains(&("tealText", "surface")));
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
    assert!(toggle.contains("track := PowerSwitch"));
    assert!(toggle.contains("accessible-role: button;"));
    for name in ["SpeakerBadge", "PowerSwitch"] {
        let body = component_body(widgets, name);
        for needle in [
            "accessible-role: button;",
            "accessible-checkable: true;",
            "accessible-checked:",
            "FocusScope",
            "event.text == \" \" || event.text == Key.Return",
            "FocusRing",
            "visible-focus: focus.has-focus;",
        ] {
            assert!(body.contains(needle), "{name} has {needle}");
        }
    }
    let count = |needle: &str| {
        UI_FILES
            .iter()
            .map(|(_, source)| blocks(source, needle).len())
            .sum::<usize>()
    };
    assert_eq!(count("power := ToggleSwitch"), 1, "the equalizer");
    assert_eq!(count("power := PowerSwitch"), 2, "main window and flyout");
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
        .find("if Platform.os == OperatingSystemType.windows && !Glyphs.drawn-everywhere(root.glyph): Text {")
        .expect("the icon font draws only on Windows");
    assert!(glyph[font..].contains("font-family: \"Segoe Fluent Icons\";"));
    let drawing = glyph
        .find("if (Platform.os != OperatingSystemType.windows || Glyphs.drawn-everywhere(root.glyph)) && root.glyph != \"\": Rectangle {")
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
    // Shuffle and Repeat are drawn on Glyphs.bold's 20-unit box.
    let mut grid = 16.0;
    for line in glyphs.lines().map(str::trim) {
        if let Some(test) = line.strip_prefix("if glyph == root.") {
            grid = if test.starts_with("shuffle") || test.starts_with("repeat-") {
                20.0
            } else {
                16.0
            };
        }
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
            let number = token.trim_start_matches(|c| "MLHVACQZ".contains(c));
            assert!(
                token.len() - number.len() <= 1
                    && (number.is_empty()
                        || number
                            .parse::<f32>()
                            .is_ok_and(|n| (0.0..=grid).contains(&n))),
                "`{token}` is not a path command or a coordinate on the {grid}-unit grid"
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
    for keys in ["\"Ctrl+N\";", "\"F5\";", "\"Ctrl+,\";", "\"Ctrl+Q\";"] {
        assert!(main.contains(keys), "the gear's menu shows {keys}");
    }
    for (file, source) in UI_FILES {
        if source.contains("inherits Window") && !source.contains("public function shortcut(") {
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
/// shows only while it runs, as does the note that it still looks. The note and row state for a speaker waiting
/// for the one AirPlay 2 slot show only while the sender's AirPlay 2 path
/// is on. The row note and notice for a speaker that plays only AirPlay 2
/// show only for a receiver the sender found; so does the note on a zone's
/// stereo pair or home theater that needs it.
/// The calibration results' AirPlay lines, a zone run's line on whom the
/// shared AirPlay delay moves included, show only when an AirPlay speaker
/// took part, so the sender ran. The live input note shows only while an
/// AirPlay speaker is on, and steers live sound away from AirPlay; the main
/// window's state line names the AirPlay delay only while one streams.
/// An empty Connected Speakers list otherwise says just that it's empty, or
/// that its virtual outputs are hidden. Seven strings of the AudioSync Guide
/// name AirPlay too, with the receivers the sender plays, the ones it can't
/// yet, and the track info it sends. Check Setup's Network type row says only
/// that AudioSync finds AirPlay speakers on the network.
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
                || line.contains("return @tr(\"{} needs AirPlay 2, which this launch of AudioSync")
                || line.contains("return @tr(\"AirPlay 2 is off\");")
                || line.contains("property <string> airplay-delay-")
                || line
                    .contains("notice-airplay-stopped: @tr(\"AirPlay stopped or couldn't start.")
                || line.contains("notice-retry-label: @tr(\"Retry, AirPlay\")")
                || line.contains("return @tr(\"AirPlay stopped\");")
                || line.contains(
                    "notice-airplay-ptp-ports-taken: @tr(\"Another app is using port 319 or 320,"
                )
                || line.contains(
                    "return @tr(\"AirPlay speakers can wait at most 20 s for a Google Cast speaker,"
                )
                || line.contains("return @tr(\"AirPlay adds a {} s delay to every speaker.")
                || line.contains("return @tr(\"{0} · AirPlay delay {1} s\", status, seconds);")
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
                || line.contains(
                    "looking-for-airplay-speakers: @tr(\"Looking for AirPlay speakers…\")"
                )
                || line.contains("plays up to {0} AirPlay 2 speakers at once, so {1} waits.")
                || line.contains("@tr(\"Waiting: too many AirPlay 2 speakers\")")
                || line.contains(
                    "waits while AudioSync plays to AirPlay 2 speakers on another network."
                )
                || line.contains("@tr(\"Waiting: AirPlay 2 speakers on another network\")")
                || line.contains("needs-airplay2: @tr(\"Needs AirPlay 2\")")
                || line.contains(
                    "setup-network-detail: @tr(\"AudioSync finds AirPlay and Google Cast speakers on your network.\")"
                )
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
    // Localization.swift virtualDevicesHidden and connectedEmpty: the switch
    // lives in Settings, and the network note follows on its own line.
    assert!(strings.contains(
        "connected-none-virtual-hidden: @tr(\"Virtual devices are hidden. To list them, choose Settings… in the gear menu and turn on Show Virtual Devices.\");"
    ));
    assert!(
        include_str!("../ui/main.slint").contains(
            "root.connected-virtual-hidden ? Strings.connected-none-virtual-hidden + \"\\n\" + self.none-found : self.none-found"
        ),
        "the network note follows the hidden virtual devices' note on its own line"
    );
    assert!(
        include_str!("../ui/main.slint")
            .contains("root.connected.length > 0 || root.connected-zoned.length > 0 ? \"\" : root.connected-all-hidden ? Strings.all-speakers-hidden : root.airplay-searching ? (root.cast-list == CastList.none-found || root.cast-list == CastList.failed ? Strings.looking-for-airplay-speakers : Strings.looking-for-speakers) : root.connected-virtual-hidden ? Strings.connected-none-virtual-hidden + \"\\n\" + Strings.connected-none : Strings.connected-none;"),
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

/// "Looking for speakers…" and the other list notes in the Mac window's
/// style: callout text on a 10 px inset, 8 px over its button
/// (MainWindow.swift, `emptyNote`).
// covers: S393
#[test]
fn the_speaker_list_note_is_callout_text_as_on_the_mac() {
    // A Windows checkout may turn the file's line ends into CRLF.
    let main = include_str!("../ui/main.slint").replace("\r\n", "\n");
    assert!(main.contains(
        "                    padding: 10px;
                    spacing: 8px;
                    SecondaryText {
                        text: root.note;
                        font-size: Tokens.font-size-callout;"
    ));
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

/// The English text inside the first `@tr("…")` of string or function `name`.
fn tr_text<'a>(strings: &'a str, name: &str) -> &'a str {
    let start = strings
        .find(&format!("out property <string> {name}: @tr(\""))
        .or_else(|| strings.find(&format!("function {name}(")))
        .unwrap_or_else(|| panic!("strings.slint has {name}"));
    let rest = &strings[start..];
    let open = rest.find("@tr(\"").expect("a translatable string") + "@tr(\"".len();
    let len = rest[open..].find('"').expect("a closed string");
    &rest[open..open + len]
}

/// Whether `line` names `Strings.{name}` itself, not a longer name it prefixes.
fn names_string(line: &str, name: &str) -> bool {
    let pattern = format!("Strings.{name}");
    line.match_indices(&pattern).any(|(i, _)| {
        !line[i + pattern.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Voice Access clicks a button by the words it shows, so a button's Narrator
/// name starts with its visible text, and every button showing that text
/// takes its Narrator name from the matching label.
#[test]
fn accessible_names_start_with_the_visible_text() {
    let strings = include_str!("../ui/strings.slint");
    let mut buttons = 0;
    for (text, label) in [
        ("notice-retry", "notice-retry-label"),
        ("notice-open-privacy", "notice-open-privacy-label"),
        ("leave-out", "leave-out-label"),
        ("include-anyway", "include-anyway-label"),
        ("calibrate-all-again", "calibrate-all-again-label"),
        ("calibrate-with-microphone", "calibrate-label"),
    ] {
        for (file, source) in UI_FILES {
            let small = blocks(source, "SmallButton");
            for block in blocks(source, "Button").into_iter().chain(small) {
                let lines: Vec<&str> = block.lines().map(str::trim).collect();
                if !lines
                    .iter()
                    .any(|l| l.starts_with("text:") && names_string(l, text))
                {
                    continue;
                }
                buttons += 1;
                assert!(
                    lines
                        .iter()
                        .any(|l| l.starts_with("accessible-label:") && names_string(l, label)),
                    "{file}: a {text} button without accessible-label {label}:\n{block}"
                );
            }
        }
        let (text, label) = (tr_text(strings, text), tr_text(strings, label));
        assert!(label.starts_with(text), "`{label}` starts with `{text}`");
    }
    assert_eq!(buttons, 4, "the check found the buttons");
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
    assert!(include_str!("view.rs").contains("self.guide().set_settings_failed(!opened);"));
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
            "tray panel",
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

/// Windows moves or holds back the doubled source itself (S469 to S471) and
/// shows no double-audio notice, so its Help asks for no change by hand.
#[test]
fn windows_help_on_hearing_twice_asks_for_no_change() {
    let text = tr_text(include_str!("../ui/strings.slint"), "help-double");
    for wording in ["Sound settings", "shows a notice", "Show How", "row says"] {
        assert!(!text.contains(wording), "{wording} in: {text}");
    }
    for sentence in text.split('.') {
        if sentence.contains("Volume mixer") {
            let sentence = sentence.trim_start().trim_start_matches("\\n");
            assert!(sentence.starts_with("Don't mute"), "asks: {sentence}");
        }
    }
}

/// A notice that leaves the user stuck says what to do next, and Resync
/// names the Cast offset it restores.
#[test]
fn stuck_notices_say_what_to_do_next() {
    let strings = include_str!("../ui/strings.slint");
    for text in [
        "AudioSync couldn't save your settings, so changes you make now won't be kept after AudioSync quits. If the disk is full, free up some space.",
        "AudioSync couldn't save your AirPlay pairings, so pairings you add or forget now won't be kept after AudioSync quits. If the disk is full, free up some space.",
        "The download is broken, so nothing was installed. The running version stays. You can download the new version from the release page.",
        "There isn't enough disk space for the update, so nothing was installed. The running version stays. Free up some space, then try again.",
        "{} needs PTP timing, which isn't available, so it's off. If another app is using port 319 or 320, close it, then turn the speaker on again.",
        "Streaming, the speakers or their delays changed during the measurement. Leave them as they are while it measures, and try again.",
        "The two speakers were too far apart to measure. Make sure the microphone can hear both speakers, and try again.",
        "Restarts the stream, with a short gap, so this speaker plays at its offset again",
    ] {
        assert!(
            strings.contains(&format!("@tr(\"{text}\"")),
            "missing: {text}"
        );
    }
}

/// A text that asks the user to start AudioSync again says so in the words
/// of the gear's Exit, never quit and reopen or restart.
#[test]
fn texts_say_exit_to_start_audiosync_again() {
    let strings = include_str!("../ui/strings.slint");
    for line in strings.lines().filter(|line| line.contains("@tr(")) {
        for wording in [
            "quit and reopen",
            "estart AudioSync",
            "after AudioSync restarts. Select",
        ] {
            assert!(!line.contains(wording), "strings.slint: `{}`", line.trim());
        }
    }
}

/// Pairing texts ask for the code a speaker shows, the word its row uses,
/// never a PIN, as on the Mac.
// covers: S422
#[test]
fn no_text_asks_for_a_pin() {
    let strings = include_str!("../ui/strings.slint");
    for line in strings.lines().filter(|line| line.contains("@tr(")) {
        assert!(!line.contains("PIN"), "strings.slint: `{}`", line.trim());
    }
}

/// AudioSync Guide in the gear's menu (F1) opens the Guide, which spells out no text of
/// its own, closes on Escape, and keeps every paragraph's full height.
#[test]
fn the_audiosync_guide_is_wired_and_worded_in_strings() {
    let main = include_str!("../ui/main.slint");
    let item = main
        .find("text: Strings.help-title;\n                keys: \"F1\";")
        .expect("the gear's menu has the Guide, with F1");
    let item = &main[item..item + 400];
    assert!(item.contains("root.guide();"));
    assert!(main.contains("root.help-requested();"));
    // A Windows checkout may end the lines of main.slint with CRLF.
    assert!(main.replace("\r\n", "\n").contains(
        "if text == Key.F1 {
                root.help-requested();"
    ));
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

/// The Windows and Linux Guides say where Several Apps is and describe
/// the Now Playing card, as the Mac Guide does.
// covers: S416
#[test]
fn the_guide_describes_several_apps_and_now_playing_on_both_hosts() {
    let strings = include_str!("../ui/strings.slint");
    let text = |name: &str| {
        strings
            .lines()
            .find(|line| line.contains(&format!("out property <string> {name}: @tr(\"")))
            .unwrap_or_else(|| panic!("strings.slint has {name}"))
    };
    for suffix in ["", "-linux"] {
        assert!(
            text(&format!("help-start{suffix}"))
                .contains("Several Apps, at the end of the source menu")
        );
        assert!(text(&format!("help-more{suffix}")).contains("the Now Playing card"));
    }
}

// covers: S416
#[test]
fn the_guide_has_the_ten_topics_the_spec_names() {
    let help = include_str!("../ui/help.slint");
    let strings = include_str!("../ui/strings.slint");
    let topics = [
        ("start", "First Steps"),
        ("cast", "Google Cast"),
        ("calibration", "Calibration"),
        ("volume", "Volume"),
        ("tips", "Tips"),
        ("double", "Hearing Sound Twice"),
        ("more", "More Controls"),
        ("airplay", "AirPlay"),
        ("remote", "Web Remote"),
        ("notices", "Notices"),
    ];
    for (key, title) in topics {
        assert!(
            help.contains(&format!("title: Strings.help-{key}-title;")),
            "the Guide shows the {title} topic"
        );
        assert!(
            strings.contains(&format!(
                "out property <string> help-{key}-title: @tr(\"{title}\");"
            )),
            "the {key} topic is titled {title}"
        );
    }
    let shown = help
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("title: Strings.help-")
                && line.contains("-title;")
                && !line.contains("Strings.help-title;")
        })
        .count();
    assert_eq!(shown, topics.len(), "the Guide shows no other topic");
}

/// The Windows and Linux Guides name a Cast group's members, a zone's
/// double-click rename, AirPlay 2 code pairing and the AirPlay volume
/// scale, as the Mac Guide does.
// covers: S416
#[test]
fn the_guide_matches_the_mac_guide_on_groups_zones_codes_and_volume() {
    let strings = include_str!("../ui/strings.slint");
    let text = |name: &str| {
        strings
            .lines()
            .find(|line| line.contains(&format!("out property <string> {name}: @tr(\"")))
            .unwrap_or_else(|| panic!("strings.slint has {name}"))
    };
    assert!(
        text("help-sync-cast")
            .contains("its note names the speakers in it once the group reports them")
    );
    for suffix in ["", "-linux"] {
        assert!(
            text(&format!("help-more{suffix}")).contains("To rename a zone, double-click its name")
        );
        assert!(
            text(&format!("help-volume{suffix}"))
                .contains("AirPlay speakers use the AirPlay volume scale.")
        );
    }
    for name in ["help-airplay", "help-airplay-linux"] {
        assert!(
            text(name)
                .contains("An Apple TV, or a speaker reached over AirPlay 2, that asks for a code")
        );
    }
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
            "accessible-label: hint-box.open ? Strings.device-volume-hint-label(\
             root.item.name, hint-box.hint, hint-box.percent) : \"\";",
        )
        .expect("the hint names the output, and a shutting hint says nothing");
    let fix_at = row
        .find("label: Strings.device-volume-fix-label(root.item.name, hint-box.hint);")
        .expect("the fix names the output");
    let options = row
        .find("options := IconButton {")
        .expect("the options button");
    assert!(
        slider < hint_at && hint_at < fix_at && !(slider..fix_at).contains(&options),
        "the hint and the fix come right after the slider"
    );
    let shown = row
        .find("property <bool> open: root.item.device-volume != DeviceVolumeHint.none;")
        .expect("the hint shows only with a hint to show");
    assert!(slider < shown && shown < hint_at);
    let sync = row
        .find("sync-pane := FoldPane {")
        .expect("the sync controls");
    let part = &row[shown..sync];
    assert!(part.contains("accessible-role: text;"));
    assert!(part.contains("text: Strings.device-volume-hint(hint-box.hint, hint-box.percent);"));
    assert!(part.contains("text: Strings.device-volume-fix(hint-box.hint);"));
    assert!(part.contains("root.device-volume-fix();"));
    assert!(part.contains("color: Tokens.secondary-text;"));
    assert!(
        part.contains("Glyph {") && part.contains("glyph: Glyphs.device-volume(hint-box.hint);"),
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
            *line == "if Strings.speaker-caption(root.caption, root.note) != \"\": QuietText {"
        })
        .expect("FlyoutRow shows a QuietText while its state has a note");
    assert_eq!(
        lines.get(note + 1).copied(),
        Some("text: Strings.speaker-caption(root.caption, root.note);"),
        "the QuietText shows the note"
    );
    assert_eq!(
        include_str!("../ui/flyout.slint")
            .matches("note: Strings.speaker-note(speaker);")
            .count(),
        3,
        "every speaker row takes the main window's note"
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
        "background: root.compact ? transparent : notice.info ? Tokens.teal.with-alpha(0.12) : Fluent.warn-background;",
        "accessible-label: Strings.notice-spoken(notice.heading, notice.message);",
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
    let settings = include_str!("../ui/settings.slint");
    for title in [
        "text: Strings.check-updates-automatically;",
        "text: Strings.menu-check-for-updates;",
    ] {
        assert!(settings.contains(title), "settings.slint has {title}");
    }
    let strings = include_str!("../ui/strings.slint");
    for label in [
        "Install Update",
        "Remind Me Later",
        "Skip This Version",
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
/// Narrator's label and value on the Presets row, and the Menu key and
/// Shift+F10 through each listed preset's menu.
#[test]
fn the_preset_controls_use_tokens_and_reach_the_keyboard() {
    let main = include_str!("../ui/main.slint");
    let flyout = include_str!("../ui/flyout.slint");
    let row = component_body(main, "PresetListRow");
    let parts = [
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
    for line in row.lines().map(str::trim) {
        if line.starts_with("color:") || line.starts_with("background:") {
            assert!(!line.contains('#') && !line.contains("rgb"), "{line}");
        }
    }
    let [card, picker] = parts;
    for needle in [
        "Strings.no-presets",
        "accessible-label: Strings.save-preset;",
    ] {
        assert!(card.contains(needle), "PresetsCard has {needle}");
    }
    for needle in [
        "menu := PopupWindow {",
        "if MenuKey.opens(event) {",
        "text: Strings.rename-preset;",
        "enabled: !root.item.current;",
        "text: Strings.delete-preset;",
        "FocusRing {",
        "|| ScreenReader.on ? 1 : 0;",
    ] {
        assert!(row.contains(needle), "PresetListRow has {needle}");
    }
    for needle in [
        "accessible-role: button;",
        "accessible-label: Strings.presets;",
        "FocusRing {",
        "if event.text == \" \" || event.text == Key.Return {",
    ] {
        assert!(card.contains(needle), "PresetsCard has {needle}");
        assert!(picker.contains(needle), "PresetPicker has {needle}");
    }
    assert!(flyout.contains("if root.presets.length > 0: IntroPart {"));
}

/// A preset row's grey 11 px second line keeps 4.5:1 under the row's hover
/// and pressed fills over the open list's tint, on the window's own
/// background in light and dark.
// covers: S259
#[test]
fn a_preset_rows_second_line_reaches_4_5_to_1_under_its_fills() {
    let main = include_str!("../ui/main.slint");
    let row = component_body(main, "PresetListRow");
    assert!(row.contains(
        "background: hover.pressed ? Fluent.pressed : hover.has-hover ? Fluent.row-hover : transparent;"
    ));
    let second_line = row
        .find("font-size: Tokens.font-size-subheadline;")
        .expect("an 11 px second line");
    assert_eq!(
        row[second_line..].lines().nth(1).map(str::trim),
        Some("color: Tokens.secondary-text;")
    );
    assert!(
        main.contains("    background: Fluent.main-window;\n    default-font-family: Fluent.font;")
    );
    let page = fluent_color("main-window");
    for (index, appearance) in [Appearance::Light, Appearance::Dark]
        .into_iter()
        .enumerate()
    {
        let window = "main-window";
        let [r, g, b, _] = page[index];
        let under = [r, g, b];
        let text = srgb("text", appearance);
        // Over the open list's own tint.
        let open = fluent_fill_alpha("open-row");
        for fill in ["row-hover", "pressed"] {
            let alpha = 1.0 - (1.0 - open) * (1.0 - fluent_fill_alpha(fill));
            let filled = [0, 1, 2].map(|i| text[i] * alpha + under[i] * (1.0 - alpha));
            let value = contrast(composite("text@secondaryText", filled, appearance), filled);
            assert!(
                value >= MIN_TEXT_CONTRAST,
                "secondary text on {window}+{fill} ({appearance:?}): {value:.2}:1"
            );
        }
    }
}

/// The Presets and Equalizer rows name their choice on a grey 11 px second
/// line under the label, as the Mac's do (`ChoiceLineView`): it folds open
/// and shut on the 0.25 s fold, a name that changes fades over 0.2 s, at
/// once with animation effects off, and no dot marks it.
// covers: S264
#[test]
fn the_preset_and_equalizer_rows_name_their_choice_on_a_second_line() {
    let widgets = include_str!("../ui/widgets.slint");
    let fade = component_body(widgets, "FadeName");
    assert!(fade.contains("animate opacity { duration: Motion.on ? 200ms : 0ms; }"));
    let line = component_body(widgets, "ChoiceLine");
    for needle in [
        "FoldPane {",
        "FadeName {",
        "text-color: Tokens.secondary-text;",
        "font-size: Tokens.font-size-subheadline;",
    ] {
        assert!(line.contains(needle), "ChoiceLine has {needle}");
    }
    let main = include_str!("../ui/main.slint");
    for (part, text) in [
        (component_body(main, "PresetsCard"), "text: root.current;"),
        (
            component_body(main, "EqualizerCard"),
            "text: root.on ? root.curve : \"\";",
        ),
        (
            component_body(include_str!("../ui/flyout.slint"), "PresetPicker"),
            "text: root.current;",
        ),
    ] {
        assert!(part.contains("ChoiceLine {"), "{part}");
        assert!(part.contains(text), "{part}");
        assert!(!part.contains("Tokens.teal"), "no dot: {part}");
    }
}

/// A zone header is filled only under the pointer, as on the Mac: a playing
/// zone shows it in its badge, not with a teal header.
#[test]
fn a_zone_header_fills_only_on_hover() {
    let header = component_body(include_str!("../ui/widgets.slint"), "ZoneHeader");
    assert!(
        header.contains("        background: hover.has-hover ? Fluent.row-hover : transparent;"),
        "the header's fill follows only the hover"
    );
}

/// A zone header's volume is a bare slider: the Mac's zone Slider has no
/// value text beside it (MainWindow.swift:1993-2000).
#[test]
fn a_zone_headers_volume_shows_no_number() {
    let header = component_body(include_str!("../ui/widgets.slint"), "ZoneHeader");
    let slider = header
        .find("volume-slider := VolumeSlider {")
        .expect("the header has its volume");
    let end = slider + header[slider..].find("changed(value)").unwrap();
    assert!(
        header[slider..end].contains("shows-value: false;"),
        "no number beside the zone's volume"
    );
}

/// A zone header shows its volume only while one of its speakers is on, as
/// the Mac's `if zone.on > 0` (MainWindow.swift:1992); hidden, it takes no room.
#[test]
fn a_zone_headers_volume_shows_only_while_a_speaker_is_on() {
    let header = component_body(include_str!("../ui/widgets.slint"), "ZoneHeader");
    let slider = header
        .find("volume-slider := VolumeSlider {")
        .expect("the header has its volume");
    let wrap = header[..slider]
        .rfind("Rectangle {")
        .expect("the volume sits in its own box");
    let gate = &header[wrap..slider];
    assert!(gate.contains("visible: root.item.on > 0;"), "{gate}");
    assert!(
        gate.contains("min-width: self.visible ? 100px : 0px;"),
        "{gate}"
    );
    assert!(
        gate.contains("max-width: self.visible ? 140px : 0px;"),
        "{gate}"
    );
}

/// A hidden zone volume leaves no gap either: the Mac's HStack drops the
/// slider's spacing with it (MainWindow.swift:1992), so the name keeps the room.
#[test]
fn a_hidden_zone_volume_leaves_no_gap() {
    let header = component_body(include_str!("../ui/widgets.slint"), "ZoneHeader");
    let slider = header
        .find("volume-slider := VolumeSlider {")
        .expect("the header has its volume");
    let group = header[..slider]
        .rfind("HorizontalLayout {")
        .expect("the volume and options share a row");
    assert!(
        header[group..slider].contains("spacing: root.item.on > 0 ? 8px : 0px;"),
        "the gap before the options goes with the volume"
    );
}

/// A zone header's hover fill stops 4 px inside each edge, as the Mac's
/// highlight inset does.
#[test]
fn a_zone_headers_hover_fill_stops_4_px_inside_each_edge() {
    // CI checks the .slint files out with CRLF line ends on Windows.
    let header =
        component_body(include_str!("../ui/widgets.slint"), "ZoneHeader").replace("\r\n", "\n");
    let fill = header
        .find("hover-fill := Rectangle {")
        .map(|at| &header[at..at + header[at..].find("    }\n").expect("it ends")])
        .expect("a hover fill of its own");
    assert!(fill.contains("x: 4px;"), "{fill}");
    assert!(fill.contains("width: parent.width - 8px;"), "{fill}");
}

/// Speaker list rows as the Mac draws them (Sources/AudioSync on this
/// branch): Radius.row is 9 (BrandTokens.swift:161); a speaker row's fill
/// is its whole box (MainWindow.swift:2494), not the zone header's 4 px
/// inset (:2021); a compact in-zone row has no fill (:2177-2180); the rule
/// between rows runs from nameColumn to the edge (:619, :624).
#[test]
fn speaker_list_rows_fill_and_rule_as_on_the_mac() {
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    assert!(widgets.contains("out property <length> radius-row: 9px;"));
    let row = component_body(&widgets, "SpeakerRow");
    assert!(!row.contains("hover-fill :="), "the whole row fills");
    assert!(row.contains(
        "\n    border-radius: Fluent.radius-row;\n    background: !root.grouped && root.plated"
    ));
    let member = component_body(&widgets, "ZoneMemberRow");
    assert!(
        !member.contains("background: hover.has-hover"),
        "no hover fill"
    );
    let main = include_str!("../ui/main.slint").replace("\r\n", "\n");
    assert!(
        component_body(&main, "SpeakerRows")
            .contains("padding-left: Fluent.name-inset;\n                Divider { }")
    );
}

/// A hovered speaker row takes primary at 5 % in both themes, as the Mac's
/// (MainWindow.swift:2495), not more on dark.
#[test]
fn a_speaker_rows_hover_fill_is_the_same_in_both_themes() {
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    assert!(!widgets.contains("row-hover-dark"));
    assert!(
        component_body(&widgets, "SpeakerRow")
            .contains("background: !root.grouped && root.plated ? Fluent.row-hover : transparent;")
    );
}

/// The badge stays beside the name and the slider while a hint line grows
/// the row: its centre sits in the middle of the 5 px gap between them, as
/// the Mac's badgePin (MainWindow.swift:2331, :2336, :2439 on the rows-tidy
/// commit), instead of the middle of the whole row, on a whole pixel.
#[test]
fn a_speaker_rows_badge_stays_at_the_name_line() {
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    let row = component_body(&widgets, "SpeakerRow");
    assert!(row.contains("name-line := HorizontalLayout {"));
    assert!(row.contains(
        "y: floor((name-line.y + name-line.height + 5px / 2 - self.height / 2) / 1px) * 1px;"
    ));
}

/// Both sliders of the reference's row start and end together: the Windows
/// volume slider fills its line as the speaker slider does (the Mac indents
/// the panel to the slider, MainWindow.swift:2479), and the
/// speaker's value takes the panel's 34 px column (:2435, :2842).
#[test]
fn the_windows_volume_slider_lines_up_with_the_speaker_slider() {
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    let row = component_body(&widgets, "SpeakerRow");
    for slider in [
        "volume-slider := VolumeSlider {",
        "system-slider := VolumeSlider {",
    ] {
        let body = &row[row.find(slider).expect(slider)..];
        assert!(body[..body.find("height:").unwrap()].contains("width: parent.width;"));
    }
    assert!(row.contains("value-width: 34px;"));
    assert!(component_body(&widgets, "VolumeSlider").contains("width: root.value-width;"));
}

/// Under the network note, the line above Add by Address keeps to the note's
/// 10 px text inset, with 10 px under it and 6 px under the button, as the
/// Mac's (MainWindow.swift:655, :664-665 on the rows-tidy commit).
#[test]
fn the_line_above_add_by_address_keeps_to_the_note() {
    let main = include_str!("../ui/main.slint").replace("\r\n", "\n");
    let section = component_body(&main, "SpeakerSection");
    assert!(section.contains("padding-left: root.note != \"\" ? 10px : 0px;"));
    assert!(section.contains("padding-right: root.note != \"\" ? 10px : 0px;"));
    assert!(section.contains("padding-top: 10px;\n                padding-bottom: 6px;"));
}

/// An off speaker's slider is drawn dimmed as the Mac's, measured on its
/// window at 2x on a plain and a hovered row: no fill, a see-through track (white at 5 % on
/// dark, black at 3 % on light) and knob (white at 13 % and 56 %), which
/// the track stops at. It still takes no drag, as the Mac's .disabled
/// slider (MainWindow.swift:2424).
#[test]
fn an_off_speakers_slider_is_dimmed_as_the_macs() {
    let widgets = include_str!("../ui/widgets.slint").replace(
        "
", "
",
    );
    let slider = component_body(&widgets, "LevelSlider");
    for look in [
        "piece == 0 && root.idle ? (Tokens.dark ? #ffffff25 : #00000025)",
        // An on speaker's fill is the ink the window tints its sliders
        // (MainWindow.swift:350-351): near white on dark, as the Mac's.
        "background: root.idle ? transparent : root.small ? (Tokens.dark ? #f4f4f4 : #000000f5) : Tokens.text;",
        "width: root.vertical ? root.thickness : max(0px, piece == 0 ? root.span * root.ratio : root.span * (1 - root.ratio));",
    ] {
        assert!(slider.contains(look), "{look}");
    }
    assert!(!slider.contains("opacity: root.idle && Tokens.dark ? 0.25"));
    assert!(slider.contains("enabled: root.enabled;"));
}

/// Calibrate All's left-out list as the Mac's, in the sheet
/// (CalibrateAllViews.swift:430-478): a small "Left out" heading, then each
/// output's name in body text over its reason, with its button after them.
#[test]
fn the_left_out_list_reads_as_the_macs() {
    let main = include_str!("../ui/main.slint").replace(
        "
", "
",
    );
    let sheet = component_body(&main, "CalibrateAllSheet");
    let list = &sheet[sheet
        .find("if root.left-out.length > 0: VerticalLayout")
        .expect("the left-out list")..];
    assert!(list.contains("text: Strings.calibrate-all-left-out-title;"));
    let name = list.find("text: item.name;").expect("the name");
    let reason = list.find("text: item.reason;").expect("the reason");
    let button = list.find("Button {").expect("the button");
    assert!(
        name < reason && reason < button,
        "name over reason, then the button"
    );
    assert!(!list[name..reason].contains("weight-semibold"));
    assert!(
        !component_body(&main, "CalibrationCard").contains("left-out"),
        "none in the left column"
    );
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
        4,
        "the box, the name, the count line and the arrow"
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
    assert!(flyout.contains("open: zone.expanded;"));
    assert!(flyout.contains("for speaker in zone-members.shown ? zone.members : []: FlyoutRow {"));
    assert!(flyout.contains("root.zone-expanded-changed(zone.id, open);"));
}

/// With the equalizer off, the gain values and band labels stay readable:
/// only the sliders fade, never the text around them.
#[test]
fn an_equalizer_turned_off_fades_only_the_sliders() {
    let source = include_str!("../ui/equalizer.slint");
    let sliders: usize = blocks(source, "track := Rectangle")
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

/// Narrator calls the calibration results' Undo button "Undo the new
/// delays", the label VoiceOver reads on the Mac; the button still shows
/// Undo.
// covers: S238
#[test]
fn the_calibration_undo_button_is_labeled_as_on_the_mac() {
    let strings = include_str!("../ui/strings.slint");
    assert!(strings.contains(r#"calibrate-all-undo-label: @tr("Undo the new delays");"#));
    let main = include_str!("../ui/main.slint");
    let start = main
        .find("undo-button := SmallButton {")
        .expect("the Undo button");
    let button = &main[start..start + 600];
    assert!(button.contains("text: Strings.calibrate-all-undo;"));
    assert!(button.contains("accessible-label: Strings.calibrate-all-undo-label;"));
}

/// The microphone notice and the calibration name the same Windows switches:
/// the desktop apps one, and the master one, which blocks it when off.
// covers: S355
#[test]
fn both_microphone_fixes_name_the_same_windows_switches() {
    let strings = include_str!("../ui/strings.slint");
    let notice = &strings[strings
        .find("function notice-microphone-blocked(")
        .expect("the notice")..];
    let notice = &notice[..notice.find("\n    }").expect("the notice ends")];
    let calibration = strings
        .lines()
        .find(|line| line.contains("calibration-microphone-denied:"))
        .expect("the calibration text");
    for text in [notice, calibration] {
        assert!(
            text.contains("“Let desktop apps access your microphone” and “Microphone access”"),
            "{text}"
        );
    }
}

/// The file a `DeviceIcons` function returns for `kind`: the first `if` that
/// names it, else the closing fallback.
fn device_icon_for<'a>(function: &'a str, kind: &str) -> &'a str {
    let named = format!("SpeakerKind.{kind}");
    let mut lines = function.lines().map(str::trim);
    let mut fallback = "";
    while let Some(line) = lines.next() {
        let file = |line: &'a str| {
            line.split("@image-url(\"../../brand/icons/")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("an icon file in {line}"))
        };
        if line.starts_with("if ") {
            let names = line
                .split(|c: char| c.is_whitespace() || c == '{')
                .any(|word| word == named);
            let target = file(lines.next().expect("the if returns"));
            if names {
                return target;
            }
        } else if line.starts_with("return @image-url(") {
            fallback = file(line);
        }
    }
    fallback
}

/// Every speaker kind gets AudioSync's own drawing from brand/icons in its
/// round toggle and in the badge after its name, never an icon font glyph.
#[test]
fn every_speaker_kind_has_its_own_device_icon() {
    let types = include_str!("../ui/types.slint");
    let kinds: Vec<&str> = types[types.find("enum SpeakerKind {").expect("the kinds")..]
        .split(['{', '}'])
        .nth(1)
        .expect("the kind list")
        .split(',')
        .map(str::trim)
        .collect();
    let expected = [
        ("airplay", "smart-speaker.svg", "badge-airplay.svg"),
        ("airplay2", "smart-speaker.svg", "badge-airplay.svg"),
        ("cast", "speaker.svg", "badge-cast.svg"),
        ("built-in", "speaker.svg", "badge-laptop.svg"),
        ("headphones", "headphones.svg", "badge-headphones.svg"),
        ("bluetooth", "bluetooth.svg", "badge-bluetooth.svg"),
        ("usb", "usb.svg", "badge-usb.svg"),
        ("usb-headphones", "headphones.svg", "badge-usb.svg"),
        ("usb-speakers", "speaker.svg", "badge-usb.svg"),
        ("hdmi", "monitor.svg", "badge-display.svg"),
        ("display-port", "monitor.svg", "badge-display.svg"),
        ("virtual", "device.svg", "badge-speaker.svg"),
        ("other", "device.svg", "badge-speaker.svg"),
    ];
    assert_eq!(
        kinds,
        expected.iter().map(|(kind, ..)| *kind).collect::<Vec<_>>(),
        "every kind has a row here"
    );
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    let icons = &widgets[widgets
        .find("export global DeviceIcons {")
        .expect("DeviceIcons")..];
    let function = |name: &str| {
        let body = &icons[icons
            .find(&format!("function {name}(kind: SpeakerKind) -> image {{"))
            .unwrap_or_else(|| panic!("DeviceIcons.{name}"))..];
        &body[..body.find("\n    }").expect("the function ends")]
    };
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../brand/icons");
    for (kind, glyph, badge) in expected {
        assert_eq!(device_icon_for(function("glyph"), kind), glyph, "{kind}");
        for size in ["16", "14"] {
            assert_eq!(
                device_icon_for(function(&format!("glyph{size}")), kind),
                format!("{size}/{glyph}"),
                "{kind}"
            );
        }
        assert_eq!(device_icon_for(function("badge"), kind), badge, "{kind}");
    }
    for file in icons.split("@image-url(\"../../brand/icons/").skip(1) {
        let file = file.split('"').next().expect("a file name");
        assert!(folder.join(file).is_file(), "brand/icons/{file} exists");
    }
    for (file, source) in UI_FILES {
        for font_glyph in [
            "Glyphs.speaker(",
            "Glyphs.kind-icon(",
            "glyph: Glyphs.zone;",
        ] {
            assert!(!source.contains(font_glyph), "{file} uses {font_glyph}");
        }
    }
    for (component, icon) in [
        ("SpeakerRow", "icon: DeviceIcons.glyph(root.item.kind);"),
        (
            "ZoneMemberRow",
            "icon: DeviceIcons.glyph14(root.item.kind);",
        ),
        ("ZoneHeader", "icon: DeviceIcons.zone;"),
    ] {
        assert!(
            component_body(&widgets, component).contains(icon),
            "{component}"
        );
    }
    let flyout = include_str!("../ui/flyout.slint");
    assert_eq!(
        flyout
            .matches("icon: DeviceIcons.glyph16(speaker.kind);")
            .count(),
        3
    );
    assert!(flyout.contains("icon: DeviceIcons.zone16;"));
    assert!(!flyout.contains("Glyphs.speaker("));
}

/// The 18 px glyph sits 7 px into its 32 px circle, the 14 px one 5 px into
/// a zone speaker's 24 px circle, the 16 px one 6 px into the tray panel's
/// 26 px circle, and the 12 px badge is drawn at its own
/// size, so at 100% every edge lands on a whole pixel; the 32 px circle's
/// waiting ring is 2 px.
#[test]
fn device_icons_sit_on_whole_pixels() {
    let widgets = include_str!("../ui/widgets.slint").replace("\r\n", "\n");
    let flyout = include_str!("../ui/flyout.slint").replace("\r\n", "\n");
    let badge = component_body(&widgets, "SpeakerBadge");
    assert!(
        badge.contains("in property <length> size: 32px;"),
        "{badge}"
    );
    assert!(badge.contains(
        "if !root.bare: Image {\n        x: (root.size - self.width) / 2;\n        y: self.x;\n        width: root.size == 32px ? 18px : 14px;\n        height: self.width;"
    ));
    assert!(badge.contains("border-width: root.size == 32px ? 2px :"));
    let disc = component_body(&flyout, "RowDisc");
    assert!(disc.contains("width: 26px;\n    height: 26px;"), "{disc}");
    assert!(disc.contains("x: 5px;\n        y: 5px;\n        width: 16px;\n        height: 16px;"));
    assert!(!disc.contains("Glyph {"), "{disc}");
    let badge = component_body(&widgets, "KindIcon");
    assert!(
        badge.contains("width: 12px;\n        height: 12px;"),
        "{badge}"
    );
    assert!(!badge.contains("Glyph {"), "{badge}");
}

/// Shuffle and Repeat are one drawing on every system: the Mac's ModeGlyph
/// holds the same paths, and both draw them on a 20-unit box at 21 px with
/// 2-unit strokes (2.1 px).
#[test]
fn shuffle_and_repeat_match_the_mac() {
    let widgets = include_str!("../ui/widgets.slint");
    let swift = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../Sources/AudioSync/MainWindow.swift"
    ))
    .expect("the Mac's main window")
    .replace("\r\n", "\n");
    let mac = &swift[swift.find("struct ModeGlyph: View {").expect("ModeGlyph")..];
    for name in [
        "shuffleLines",
        "shuffleHeads",
        "repeatLines",
        "repeatHeads",
        "repeatOneDigit",
    ] {
        let line = &mac[mac.find(&format!("static let {name} = \"")).expect(name)..];
        let path = line.split('"').nth(1).expect(name);
        assert!(
            widgets.contains(&format!("return \"{path}\";")),
            "{name} differs between widgets.slint and MainWindow.swift"
        );
    }
    assert!(mac.contains("static let side: CGFloat = 21"));
    assert!(mac.contains("lineWidth: 2 * unit"));
    // The on dot hangs from the glyph's bottom 7 px down (MainWindow.swift:507-508),
    // so its 4 px sit 3 px clear under the 21 px glyph.
    assert!(swift.contains(
        ".overlay(alignment: .bottom) {\n                    Circle().frame(width: 4, height: 4).offset(y: 7)"
    ));
    let button = component_body(widgets, "IconButton");
    assert!(
        button.contains("y: (root.height + root.glyph-size) / 2 + 3px;"),
        "{button}"
    );
    let glyph = component_body(widgets, "Glyph");
    assert!(glyph.contains("viewbox-width: Glyphs.bold(root.glyph) ? 20 : 16;"));
    assert!(glyph.contains("Glyphs.bold(root.glyph) ? root.font-size / 10"));
    let main = include_str!("../ui/main.slint");
    for glyph in [
        "glyph: Glyphs.shuffle;",
        "Glyphs.repeat-one : Glyphs.repeat-all;",
    ] {
        let at = main.find(glyph).expect(glyph);
        assert!(
            main[at..]
                .lines()
                .nth(1)
                .unwrap()
                .contains("glyph-size: 21px;")
        );
    }
}
