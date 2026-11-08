//! The page, its manifest and icons, and the Content-Security-Policy that
//! lets in only the page's own inline script and style, by hash (CSP Level 3
//! §8.4: base64 of the SHA-256 of the element's text).

use sha2::{Digest, Sha256};

pub const PAGE: &str = include_str!("../page/index.html");
pub const MANIFEST: &str = include_str!("../page/manifest.webmanifest");
// Placeholders until the page's own icons: the site's icons, already in the asset ledger.
pub const ICON_180: &[u8] = include_bytes!("../../site/apple-touch-icon.png");
pub const ICON_512: &[u8] = include_bytes!("../../site/icon-512.png");

/// The page doesn't have exactly one plain `<script>` and one plain `<style>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageError(pub &'static str);

/// Standard base64 with padding (RFC 4648 §4).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((group >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A CSP hash source for an inline element's text.
pub fn hash_source(text: &str) -> String {
    format!("'sha256-{}'", base64(&Sha256::digest(text.as_bytes())))
}

/// The text of the one `<tag>` element, which must have no attributes.
fn only_element<'a>(page: &'a str, tag: &'static str) -> Result<&'a str, PageError> {
    let open = format!("<{tag}");
    if page.matches(open.as_str()).count() != 1 {
        return Err(PageError(tag));
    }
    let plain = format!("<{tag}>");
    let start = page.find(plain.as_str()).ok_or(PageError(tag))? + plain.len();
    let end = page[start..]
        .find(&format!("</{tag}>"))
        .ok_or(PageError(tag))?;
    Ok(&page[start..start + end])
}

/// The policy for `page`: its own origin only, its inline script and style
/// by hash, no plugins, no framing, forms and the base URL kept to itself.
pub fn content_security_policy(page: &str) -> Result<String, PageError> {
    let script = hash_source(only_element(page, "script")?);
    let style = hash_source(only_element(page, "style")?);
    Ok(format!(
        "default-src 'self'; script-src {script}; style-src {style}; img-src 'self'; connect-src 'self'; manifest-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_follows_rfc_4648() {
        for (input, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), encoded);
        }
    }

    #[test]
    fn hash_sources_match_the_csp_example() {
        // The hash example in the CSP Level 3 specification.
        assert_eq!(
            hash_source("alert('Hello, world.');"),
            "'sha256-qznLcsROx4GACP2dm0UCKCzCG+HiZ1guq6ZZDob/Tng='"
        );
    }

    #[test]
    fn the_built_in_page_gets_a_policy_without_unsafe_inline() {
        let policy = content_security_policy(PAGE).unwrap();
        assert!(policy.starts_with("default-src 'self'; "));
        assert!(!policy.contains("unsafe-inline"));
        assert_eq!(policy.matches("'sha256-").count(), 2);
    }

    #[test]
    fn a_page_needs_one_plain_script_and_one_style() {
        let both = "<style>a</style><script>b</script>";
        assert!(content_security_policy(both).is_ok());
        assert!(content_security_policy("<style>a</style>").is_err());
        assert!(
            content_security_policy("<style>a</style><script>b</script><script>c</script>")
                .is_err()
        );
        assert!(content_security_policy("<style>a</style><script src=x>b</script>").is_err());
    }

    #[test]
    fn quick_taps_dont_zoom_and_pinch_zoom_stays() {
        let style = only_element(PAGE, "style").unwrap();
        let rule = style
            .lines()
            .find(|line| line.contains("touch-action"))
            .expect("a touch-action rule");
        assert!(rule.contains("touch-action: manipulation;"));
        for selector in ["html", "button", "input"] {
            assert!(
                rule.split('{').next().unwrap().contains(selector),
                "{selector}"
            );
        }
        let viewport = PAGE
            .lines()
            .find(|line| line.contains("name=\"viewport\""))
            .unwrap();
        for zoom_lock in ["maximum-scale", "user-scalable", "minimum-scale"] {
            assert!(!PAGE.contains(zoom_lock), "{zoom_lock}");
        }
        assert!(viewport.contains("width=device-width"));
        assert!(style.contains("font-size: max(16px, 1rem);"));
    }

    #[test]
    fn the_home_screen_steps_start_hidden_and_name_the_order() {
        // Hidden in the markup, so a Home Screen app never flashes them before the script runs.
        let card = PAGE
            .lines()
            .find(|line| line.contains("id=\"setup\""))
            .unwrap();
        assert!(card.ends_with(" hidden>"));
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("(display-mode: standalone)"));
        assert!(script.contains("localStorage.setItem(SETUP_HIDDEN_KEY"));
        assert!(script.contains("Do step 2 before you enter the code"));
    }

    // covers: S340
    #[test]
    fn a_paused_track_and_a_zone_with_no_member_here_read_as_on_the_mac() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("paused: \"Paused\","));
        assert!(script.contains("const paused = playing && playing.playing === false;"));
        assert!(script.contains("zoneNoneHere: (all) => `None of ${all} found`,"));
        assert!(script.contains("zoneEmpty: \"No speakers\","));
        assert!(script.contains("TEXT.zoneNoneHere(counts.all)"));
    }

    #[test]
    fn a_change_made_elsewhere_clears_the_error_it_settles() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("if (speaker.on !== was.on || (speaker.volume !== was.volume && !row.volumeControl.lateEcho(speaker.volume)) || speaker.delay_ms !== was.delay_ms) { showError(row.error, \"\"); }"));
        assert!(script.contains("if ((zone.volume !== row.zone.volume && !row.volumeControl.lateEcho(zone.volume)) || zone.counts.on !== row.zone.counts.on) { showError(row.error, \"\"); }"));
        assert!(script.contains("const send = sender((value) => write(value).then((reply) => {"));
        assert!(script.contains("delivered.push(value);"));
        assert!(script.contains("if (delivered.length > 8) { delivered.shift(); }"));
        assert!(script.contains("const LATE_ECHO_MS = 3000;"));
        assert!(script.contains("failedAt = Date.now();"));
        assert!(script.contains("lateEcho: (reported) => Date.now() - failedAt < LATE_ECHO_MS && delivered.includes(Math.round(reported)),"));
        assert!(script.contains("if ((button.getAttribute(\"aria-current\") === \"true\") !== (preset.current === true)) { showError(ui.presetError, \"\"); }"));
        assert!(script.contains("presetButtons.delete(id); showError(ui.presetError, \"\"); }"));
    }

    #[test]
    fn a_fixed_delay_hides_its_row_and_an_unpair_error_shows_under_the_button() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(
            script.contains("const fixedDelay = speaker.delay_min_ms === speaker.delay_max_ms;")
        );
        assert!(script.contains("row.delayLine.hidden = fixedDelay;"));
        let button = PAGE
            .lines()
            .position(|line| line.contains("id=\"unpair\""))
            .unwrap();
        let error = PAGE.lines().nth(button + 1).unwrap();
        assert!(error.contains("id=\"unpair-error\"") && error.ends_with(" hidden></p>"));
        assert!(script.contains("showError(ui.unpairError, problem.message);"));
    }

    #[test]
    fn focus_moves_to_the_master_volume_when_retry_or_the_pairing_form_hides() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("focusAfterPair = true;"));
        assert!(script.contains("ui.banner.contains(focused)"));
        assert!(script.contains("ui.master.focus();"));
    }

    #[test]
    fn a_replaced_stream_waits_for_retry_instead_of_reconnecting() {
        let script = only_element(PAGE, "script").unwrap();
        let start = script
            .find("events.addEventListener(\"replaced\"")
            .expect("a replaced handler");
        let handler = &script[start..start + script[start..].find("});").unwrap()];
        assert!(handler.contains("stopStream();"));
        assert!(handler.contains("setLost(true, TEXT.replaced);"));
        for retry in ["retryLater", "setTimeout", "connect("] {
            assert!(!handler.contains(retry), "{retry}");
        }
        assert!(script.contains("replaced: \"This remote is open in another tab or window."));
        // Coming back into view must not take the stream back either; Retry or pairing does.
        assert!(handler.contains("streamReplaced = true;"));
        let visible = &script[script.find("\"visibilitychange\"").unwrap()..];
        let visible = &visible[visible.find("if (").unwrap()..];
        let condition = &visible[..visible.find('{').unwrap()];
        assert!(condition.contains("!streamReplaced"), "{condition}");
        let connect = &script[script.find("function connect() {").unwrap()..];
        // A Windows checkout may end lines with CRLF, so match the closing brace without its newline.
        let connect = &connect[..connect.find("\n}").unwrap()];
        assert!(connect.contains("streamReplaced = false;"));
    }

    #[test]
    fn focus_stays_near_a_removed_speaker_and_moves_to_the_code_when_unpaired() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("focusNeighbor(order, focusedRow.root);"));
        assert!(script.contains("(row ? row.name : ui.title).focus();"));
        assert!(script.contains("if (message) { announce(message); ui.code.focus(); }"));
    }

    #[test]
    fn the_dark_switch_knob_stands_out_on_the_accent_track() {
        let style = only_element(PAGE, "style").unwrap();
        let dark = &style[style.find("prefers-color-scheme: dark").unwrap()..];
        assert!(dark.contains("--knob: #1E1E1E;"));
    }

    #[test]
    fn every_visible_string_in_the_markup_comes_from_the_text_table() {
        let start = PAGE.find("<main>").unwrap();
        let markup = &PAGE[start..PAGE.find("<script>").unwrap()];
        let mut text = String::new();
        let mut in_tag = false;
        for character in markup.chars() {
            match character {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => text.push(character),
                _ => {}
            }
        }
        // The lowercase wordmark is the only text written into the markup;
        // screen readers name the heading AudioSync.
        assert_eq!(text.split_whitespace().collect::<String>(), "audiosync");
        assert!(markup.contains(r#"<h1 id="title" tabindex="-1" aria-label="AudioSync">"#));

        let script = only_element(PAGE, "script").unwrap();
        let table = &script[script.find("const TEXT = {").unwrap()..];
        let table = &table[..table.find("\n};").unwrap()];
        let keys: Vec<&str> = markup
            .split("data-text=\"")
            .skip(1)
            .map(|rest| &rest[..rest.find('"').unwrap()])
            .collect();
        assert!(keys.len() >= 10);
        for key in keys {
            assert!(table.contains(&format!("\n  {key}: \"")), "{key}");
        }
        assert!(script.contains("element.textContent = TEXT[element.dataset.text];"));
    }

    /// The colors one `:root` block of the style sets, by custom property.
    fn theme(block: &str) -> Vec<(String, [u8; 3])> {
        block
            .lines()
            .filter_map(|line| {
                let (name, value) = line.trim().strip_prefix("--")?.split_once(':')?;
                let hex = value.trim().strip_prefix('#')?.strip_suffix(';')?;
                let byte = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
                Some((name.to_owned(), [byte(0)?, byte(2)?, byte(4)?]))
            })
            .collect()
    }

    /// The contrast ratio of two sRGB colors (WCAG 2.2, contrast ratio and relative luminance).
    fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
        let luminance = |color: [u8; 3]| {
            let linear = |channel: u8| {
                let c = f64::from(channel) / 255.0;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
        };
        let (light, dark) = (
            luminance(a).max(luminance(b)),
            luminance(a).min(luminance(b)),
        );
        (light + 0.05) / (dark + 0.05)
    }

    // covers: S258
    #[test]
    fn the_calibrating_line_and_disabled_controls_keep_4_5_to_1_in_both_themes() {
        let style = only_element(PAGE, "style").unwrap();
        let light = &style[style.find(":root {").unwrap()..];
        let light = &light[..light.find('}').unwrap()];
        let dark = &style[style.find("prefers-color-scheme: dark").unwrap()..];
        let dark = &dark[dark.find(":root {").unwrap()..];
        let dark = &dark[..dark.find('}').unwrap()];
        assert!(style.contains(".calibrating { background: var(--notice); color: var(--text);"));
        assert!(style.contains("button:disabled, input[type=text]:disabled, input[type=number]:disabled { color: var(--muted);"));
        assert!(style.contains("button.primary:disabled { background: var(--surface);"));
        let faded = style
            .lines()
            .find(|line| line.contains("opacity: 0.5"))
            .unwrap();
        assert!(
            !faded.contains("button") && !faded.contains("number"),
            "only controls without text fade: {faded}"
        );
        for block in [light, dark] {
            let colors = theme(block);
            let color = |name: &str| {
                colors
                    .iter()
                    .find(|(found, _)| found == name)
                    .unwrap_or_else(|| panic!("--{name}"))
                    .1
            };
            for (text, back) in [
                ("text", "notice"),
                ("muted", "surface"),
                ("on-accent", "accent"),
            ] {
                let ratio = contrast(color(text), color(back));
                assert!(ratio >= 4.5, "--{text} on --{back}: {ratio:.2}");
            }
        }
    }

    // covers: S258
    #[test]
    fn calibrating_disables_every_write_but_unpair_and_the_policy_takes_the_new_script() {
        let open = PAGE.find("<fieldset id=\"writes\">").unwrap();
        let close = open + PAGE[open..].find("</fieldset>").unwrap();
        let writes = &PAGE[open..close];
        for id in ["master", "presets", "zones", "speakers"] {
            assert!(writes.contains(&format!("id=\"{id}\"")), "{id}");
        }
        let footer = &PAGE[close..];
        assert!(footer.contains("id=\"unpair\""));
        let line = PAGE
            .lines()
            .find(|line| line.contains("id=\"calibrating\""))
            .unwrap();
        assert!(line.ends_with(" hidden></p>") && line.contains("tabindex=\"-1\""));
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("ui.writes.disabled = now;"));
        assert!(script.contains("showCalibrating(next.calibrating);"));
        assert!(script.contains("calibrating: \"AudioSync is calibrating."));
        let policy = content_security_policy(PAGE).unwrap();
        assert!(policy.contains(&hash_source(script)));
        assert!(policy.contains(&hash_source(only_element(PAGE, "style").unwrap())));
    }

    // covers: S343, S360
    #[test]
    fn an_empty_list_with_hidden_speakers_says_so_as_the_panels_do() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains(
            "allHidden: \"Every speaker is hidden. Open AudioSync and choose Hidden Speakers to show one again.\","
        ));
        assert!(script.contains(
            "ui.empty.textContent = TEXT[next.hidden_speakers > 0 ? \"allHidden\" : \"noSpeakers\"];"
        ));
    }

    // covers: S454
    #[test]
    fn the_apps_notices_show_above_now_playing_as_banners_without_a_button() {
        let remote = &PAGE[PAGE.find("<section id=\"remote\"").unwrap()..];
        let place = remote
            .find("<div id=\"notices\"></div>")
            .expect("a place for the notices");
        assert!(place < remote.find("<section class=\"playing card\"").unwrap());
        let style = only_element(PAGE, "style").unwrap();
        assert!(style.contains(".banner.app { padding: 12px 16px; }"));
        let script = only_element(PAGE, "script").unwrap();
        let start = script
            .find("function showNotices(")
            .expect("function showNotices");
        // A Windows checkout may end lines with CRLF, so match the closing brace without its newline.
        let show = &script[start..start + script[start..].find("\n}").unwrap()];
        assert!(show.contains("banner.className = \"banner app\";"));
        assert!(show.contains("line.textContent = text;"));
        assert!(show.contains("announce(fresh.join(\" \"));"));
        assert!(
            !show.contains("button"),
            "the phone never dismisses a notice"
        );
        assert!(
            script
                .lines()
                .any(|line| line.trim_end() == "  showNotices(next.notices);")
        );
        assert!(PAGE.contains("<p id=\"live\" class=\"sr-only\" aria-live=\"polite\"></p>"));
    }

    fn line_with<'a>(needle: &str) -> &'a str {
        PAGE.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle}"))
            .trim_end()
    }

    #[test]
    fn every_text_the_script_names_is_in_the_table() {
        let script = only_element(PAGE, "script").unwrap();
        let start = script.find("const TEXT = {").unwrap();
        let table = &script[start..start + script[start..].find("\n};").unwrap()];
        let rest = &script[start + table.len()..];
        let mut names = 0;
        for use_site in rest.split("TEXT.").skip(1) {
            let name: String = use_site
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            assert!(table.contains(&format!("\n  {name}: ")), "TEXT.{name}");
            names += 1;
        }
        assert!(names >= 30, "{names}");
    }

    #[test]
    fn the_source_picker_has_empty_loading_and_error_states() {
        for (id, state) in [
            ("source-empty", "data-text=\"noSources\""),
            ("source-status", ""),
            ("source-error", "class=\"error\""),
        ] {
            let line = line_with(&format!("id=\"{id}\""));
            assert!(line.ends_with(" hidden></p>"), "{id}");
            assert!(line.contains(state), "{id}");
        }
        let picker = line_with("<select id=\"source\"");
        assert!(picker.contains("aria-labelledby=\"source-title\""));
        assert!(picker.contains("aria-describedby=\"source-status source-error\""));
        let script = only_element(PAGE, "script").unwrap();
        // Empty: the list gives way to its text and focus stays in the card.
        assert!(script.contains("ui.sourceEmpty.hidden = !empty;"));
        assert!(script.contains("ui.sourceTitle.focus();"));
        // Loading: a pick waits for the app's next snapshot, busy and announced.
        assert!(script.contains("ui.source.setAttribute(\"aria-busy\", \"true\");"));
        assert!(script.contains("next.source_id === switchingTo"));
        assert!(script.contains("announce(TEXT.switched(name));"));
        // Error: the server's answer, or a pick the app never took.
        assert!(script.contains("endSwitch(problem ? problem.message : \"\");"));
        assert!(
            script
                .contains("window.setTimeout(() => endSwitch(TEXT.notSwitched), SWITCH_WAIT_MS);")
        );
        assert!(script.contains("showError(ui.sourceError, message);"));
        assert!(script.contains("post(\"/api/source\", { id })"));
    }

    #[test]
    fn a_sent_value_stays_until_a_snapshot_reports_it_or_the_settle_time_ends() {
        let script = only_element(PAGE, "script").unwrap();
        assert!(script.contains("if (!inFlight && reported === sentValue) { release(); }"));
        assert!(script.contains("if (pending === null) { onSettle(); }"));
        assert!(script.contains("}, () => { power.checked = row.speaker.on; });"));
        assert!(script.contains(
            "}, () => { if (document.activeElement !== row.delay) { showDelay(row, row.speaker.delay_ms); } });"
        ));
        assert!(script.contains("if (!held) { control.show(read()); }"));
        for check in [
            "row.powerSender.busy(speaker.on)",
            "row.volumeControl.held(speaker.volume)",
            "row.delaySender.busy(speaker.delay_ms)",
            "row.volumeControl.held(zone.volume)",
            "masterControl.held(next.master.volume)",
        ] {
            assert!(script.contains(check), "{check}");
        }
    }

    #[test]
    fn a_pick_never_leaves_a_stale_error_or_a_blank_picker() {
        let script = only_element(PAGE, "script").unwrap();
        let shown = &script[script.find("function showSources(next) {").unwrap()..];
        let shown = &shown[..shown.find("fillSources(list, listed);").unwrap()];
        // A switch that lands after the wait clears the error the wait left.
        assert!(
            shown.contains("} else if (switchingTo === null && next.source_id !== shownSource) {")
        );
        assert!(shown.contains("showError(ui.sourceError, \"\");"));
        assert!(shown.contains("shownSource = next.source_id;"));
        // A snapshot that drops the pick ends the wait at once.
        assert!(shown.contains(
            "} else if (switchingTo !== null && !list.some((source) => source.id === switchingTo)) {"
        ));
        assert!(shown.contains("endSwitch(TEXT.notSwitched);"));
        // Going back to the source in use waits for nothing.
        let picked = &script[script
            .find("ui.source.addEventListener(\"change\"")
            .unwrap()..];
        let back = picked.find("if (id === snapshot.source_id) {").unwrap();
        let timer = picked.find("startSwitch(id, sourceName(id));").unwrap();
        assert!(back < timer);
        let branch = &picked[back..timer];
        assert!(branch.contains("endSwitch(\"\");"));
        assert!(branch.contains("sourceSender.set(id);"));
        assert!(branch.contains("return;"));
    }

    // covers: S344
    #[test]
    fn picking_the_shown_source_again_sends_it_again() {
        // A Windows checkout may give the page CRLF; the multi-line checks below expect LF.
        let script = only_element(PAGE, "script").unwrap().replace("\r\n", "\n");
        // A native select fires change only for a pick that differs from the selection it opened
        // with, so it opens with none selected and leaving it shows the source again.
        assert!(script.contains(
            "ui.source.addEventListener(\"pointerdown\", () => { pickerOpen = true; ui.source.selectedIndex = -1; });"
        ));
        assert!(script.contains(
            "ui.source.addEventListener(\"blur\", () => { pickerOpen = false; showCurrentSource(); });"
        ));
        // A picker closed without a pick keeps the focus: Escape or a touch elsewhere ends it.
        assert!(script.contains(
            "ui.source.addEventListener(\"keydown\", (event) => { if (event.key === \"Escape\") { pickerOpen = false; showCurrentSource(); } });"
        ));
        assert!(script.contains(
            "document.addEventListener(\"pointerdown\", (event) => { if (pickerOpen && event.target !== ui.source) { pickerOpen = false; showCurrentSource(); } });"
        ));
        // While it is open, no snapshot, list rebuild or switch wait writes the shown value back.
        let shown = &script[script.find("function showCurrentSource() {").unwrap()..];
        assert!(
            shown.starts_with("function showCurrentSource() {\n  if (pickerOpen) { return; }\n")
        );
        assert!(script.contains(
            "  fillSources(list, listed);\n  if (pickerOpen) { ui.source.selectedIndex = -1; }\n  showCurrentSource();\n}"
        ));
        // The source in use is sent again, not dropped.
        let picked = &script[script
            .find("ui.source.addEventListener(\"change\"")
            .unwrap()..];
        let picked = &picked[..picked.find("\n});\n").unwrap()];
        assert!(picked.contains("  pickerOpen = false;\n"));
        assert!(picked.contains("if (!snapshot || id === \"\") { return; }"));
    }

    // covers: S353
    #[test]
    fn several_apps_lists_the_mix_apps_as_checkboxes_and_play_reports_as_a_pick() {
        let button = line_with("id=\"mix-open\"");
        assert!(button.contains("data-text=\"severalApps\""));
        assert!(button.contains("aria-expanded=\"false\" aria-controls=\"mix\""));
        assert!(button.ends_with(" hidden></button>"));
        assert!(line_with("id=\"mix-play\"").contains("data-text=\"play\" disabled>"));
        assert!(line_with("id=\"mix-cancel\"").contains("data-text=\"cancel\""));
        let open = PAGE.find("<fieldset id=\"writes\">").unwrap();
        assert!(
            PAGE[open..].find("id=\"mix\"").unwrap() < PAGE[open..].find("</fieldset>").unwrap()
        );

        // A Windows checkout may give the page CRLF; the multi-line checks below expect LF.
        let script = only_element(PAGE, "script").unwrap().replace("\r\n", "\n");
        assert!(script.contains("severalApps: \"Several Apps…\","));
        assert!(script.contains("appNotRunning: (name) => `${name} (not running)`,"));
        assert!(script.contains("play: \"Play\","));
        assert!(script.contains("cancel: \"Cancel\","));
        // Shown only while an app runs; one that doesn't is greyed and can't be checked.
        assert!(script.contains("const offered = apps.some((app) => app.running === true);"));
        assert!(script.contains("ui.mixOpen.hidden = !offered;"));
        assert!(script.contains("box.type = \"checkbox\";"));
        assert!(script.contains("box.disabled = app.running !== true;"));
        assert!(script.contains("box.disabled ? TEXT.appNotRunning(app.name) : app.name"));
        assert!(script.contains("ui.mixPlay.disabled = none;"));
        // Play posts {"apps": [id, …]} and reports through the source pick's switching, switched and notSwitched.
        assert!(script.contains("post(\"/api/mix\", { apps })"));
        assert!(script.contains("const apps = checkedApps();"));
        let sent = &script[script.find("const mixSender = sender(").unwrap()..];
        // Each sender ends only its own wait, and still shows its own failure.
        assert!(sent.starts_with("const mixSender = sender((apps) => post(\"/api/mix\", { apps }), (problem, settled) => {\n  if (settled && switchingTo === null) { endSwitch(problem ? problem.message : \"\"); }"));
        assert!(script.contains("const sourceSender = sender((id) => post(\"/api/source\", { id }), (problem, settled) => {\n  if (settled && mixing === null) { endSwitch(problem ? problem.message : \"\"); }"));
        // Playing the one app in use waits for nothing, as a pick of the source in use doesn't.
        let played = &script[script
            .find("ui.mixPlay.addEventListener(\"click\"")
            .unwrap()..];
        let in_use = played
            .find("if (apps.length === 1 && `app:${apps[0]}` === snapshot.source_id) {")
            .unwrap();
        let wait = played
            .find("startSwitch(null, names.join(\", \"));")
            .unwrap();
        assert!(played[in_use..wait].contains("endSwitch(\"\");"));
        assert!(played[wait..].contains("mixSender.set(apps);"));
        // Switched names the new source, or the checked apps when the list doesn't.
        assert!(script.contains("} else if (mixing !== null && next.source_id !== shownSource) {"));
        assert!(script.contains("const name = sourceName(next.source_id) || mixing;"));
        assert!(script.contains("mixing = id === null ? name : null;"));
    }

    #[test]
    fn device_names_reach_the_page_only_as_text() {
        let script = only_element(PAGE, "script").unwrap();
        for markup_sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
        ] {
            assert!(!script.contains(markup_sink), "{markup_sink}");
        }
        assert!(script.contains("option.textContent = reason ? TEXT.sourceWithNotice(source.name, reason) : source.name;"));
        assert!(script.contains("group.label = heading;"));
    }

    #[test]
    fn labels_and_the_empty_list_read_as_in_the_apps() {
        for line in [
            "  masterVolume: \"Master Volume\",",
            "  source: \"Audio Source\",",
            "  sourceInputs: \"Input Devices\",",
            "  noSpeakers: \"No speakers found\",",
        ] {
            assert_eq!(line_with(line.trim()), line);
        }
    }

    #[test]
    fn the_icons_are_png_of_their_size() {
        for (icon, size) in [(ICON_180, 180u32), (ICON_512, 512)] {
            assert_eq!(&icon[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(u32::from_be_bytes(icon[16..20].try_into().unwrap()), size);
            assert_eq!(u32::from_be_bytes(icon[20..24].try_into().unwrap()), size);
        }
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(manifest["display"], "standalone");
    }
}
