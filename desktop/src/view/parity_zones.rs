//! A zone's header in the main window and the tray panel against the Mac's
//! (MainWindow.swift ZoneHeader, PanelZoneHeader.swift).

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

/// The block that opens with `from` in `source`, up to its closing brace.
fn block(source: &str, from: &str) -> String {
    let start = source.find(from).unwrap_or_else(|| panic!("{from}"));
    let mut depth = 0;
    for (at, c) in source[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=start + at].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("{from} has no end");
}

fn main_header() -> String {
    component(include_str!("../../ui/widgets.slint"), "ZoneHeader")
}

fn panel_header() -> String {
    component(include_str!("../../ui/flyout.slint"), "FlyoutZoneHeader")
}

/// MainWindow.swift:2048, 2055: the zone's options are grey, teal being kept
/// for state, and show under the pointer or their own focus, as a speaker's.
#[test]
fn a_zone_s_options_are_grey_and_show_on_hover_or_focus() {
    let options = block(&main_header(), "options := IconButton {");
    assert!(
        options.contains("glyph-color: Tokens.secondary-text;"),
        "{options}"
    );
    assert!(options.contains("opacity: root.reveal;"), "{options}");
}

/// MainWindow.swift:2015 and PanelZoneHeader.swift:64-65: the zone's name
/// keeps the body weight while the zone plays.
#[test]
fn a_zone_s_name_keeps_its_weight_while_it_plays() {
    let main = block(&main_header(), "name-text := MarqueeText {");
    assert!(!main.contains("weight-semibold"), "{main}");
    let panel = block(&panel_header(), "zone-name := Text {");
    assert!(!panel.contains("weight-semibold"), "{panel}");
}

/// MainWindow.swift:792-793, 2015: the tooltip is the zone's name, and only
/// while the name is cut.
#[test]
fn a_zone_s_name_tip_is_the_name_only_while_it_is_cut() {
    let header = main_header();
    assert!(!header.contains("speaker-name-tooltip"), "{header}");
    assert!(
        header.contains(
            "if root.tip-allowed && name-text.preferred-width > name-text.width: Tooltip {"
        ),
        "{header}"
    );
}

/// PanelZoneHeader.swift:66: the name dims with the icon while none of the
/// zone's speakers is present.
#[test]
fn a_panel_zone_s_name_dims_while_none_of_its_speakers_is_present() {
    let name = block(&panel_header(), "zone-name := Text {");
    assert!(
        name.contains("opacity: root.live ? 1 : Tokens.disabled-opacity;"),
        "{name}"
    );
}

/// PanelZoneHeader.swift:31: the panel's chevron turns a quarter with the
/// fold (FoldTurn), as the main window's does.
#[test]
fn a_panel_zone_s_chevron_turns_with_the_fold() {
    let header = panel_header();
    assert!(!header.contains("Glyphs.chevron-down"), "{header}");
    assert!(
        header.contains("transform-rotation: root.item.expanded ? 90deg : 0deg;"),
        "{header}"
    );
}
