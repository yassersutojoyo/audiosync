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
