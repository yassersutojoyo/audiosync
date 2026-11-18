//! Row padding and texts that read as the Mac's (audit FEAT-1117-20 and -21).

fn widgets() -> String {
    include_str!("../../ui/widgets.slint").replace("\r\n", "\n")
}

fn strings() -> String {
    include_str!("../../ui/strings.slint").replace("\r\n", "\n")
}

/// Rows take the Mac's 8 above and below (BrandTokens.swift:155, used at
/// MainWindow.swift:2057 for the zone header); a speaker row gives back
/// 3 px below so it keeps its 54 px step.
#[test]
fn rows_pad_8_as_the_macs() {
    let widgets = widgets();
    assert!(widgets.contains("out property <length> row-padding-y: 8px;"));
    assert!(widgets.contains(
        "padding-top: Fluent.row-padding-y;\n                padding-bottom: Fluent.row-padding-y - 3px;"
    ));
}

/// Localization.swift:1310, :499, :426-429, :1931 and :765.
#[test]
fn texts_read_as_the_macs() {
    let strings = strings();
    for text in [
        "in-zones: @tr(\"In a zone\");",
        "Turn on another speaker, or choose Include Anyway below.\");",
        "@tr(\"AudioSync couldn't open port {} on this PC, so the web remote can't start. Enter another port and select Use Port.\", port);",
    ] {
        assert!(strings.contains(text), "missing: {text}");
    }
    assert_eq!(
        strings
            .matches("and then on each other speaker that is on, one at a time")
            .count(),
        2
    );
    assert_eq!(
        strings
            .matches("@tr(\"All Apps Except {}\", names)")
            .count(),
        2
    );
    for old in [
        "doesn't let AudioSync use port",
        "then on every other speaker that is on",
        "@tr(\"all apps except {}\"",
    ] {
        assert!(!strings.contains(old), "still: {old}");
    }
}
