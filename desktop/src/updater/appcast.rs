//! Reading the appcast (spec S295) and picking the update in it (S296).

use roxmltree::{Document, Node};

use super::version::Version;

const SPARKLE_NS: &str = "http://www.andymatuschak.org/xml-namespaces/sparkle";

/// The architecture the exe was built for, which an emulated x64 build on an
/// ARM64 PC keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    pub const fn own() -> Arch {
        if cfg!(target_arch = "aarch64") {
            Arch::Arm64
        } else {
            Arch::X64
        }
    }

    /// The `sparkle:os` value of this architecture's enclosure.
    pub const fn os_value(self) -> &'static str {
        match self {
            Arch::X64 => "windows-x64",
            Arch::Arm64 => "windows-arm64",
        }
    }

    /// The name in the user agent.
    pub const fn name(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }

    /// The PE header's machine field of an exe for this architecture.
    pub const fn pe_machine(self) -> u16 {
        match self {
            Arch::X64 => 0x8664,
            Arch::Arm64 => 0xAA64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enclosure {
    pub url: String,
    pub length: Option<u64>,
    pub os: Option<String>,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub short_version: Version,
    /// As the appcast writes it, for the notice.
    pub short_version_text: String,
    pub channel: Option<String>,
    pub minimum_system: Option<Version>,
    pub enclosures: Vec<Enclosure>,
}

#[derive(Debug, Default)]
pub struct Appcast {
    pub items: Vec<Item>,
    /// Why each skipped item was skipped, for the debug log.
    pub skipped: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AppcastError {
    /// A DOCTYPE, which is refused whole.
    Doctype,
    NotXml(String),
    NoChannel,
}

/// Reads the RSS document `text`; an item that is broken on its own is
/// skipped and named in [`Appcast::skipped`].
pub fn parse(text: &str) -> Result<Appcast, AppcastError> {
    let document = Document::parse(text).map_err(|error| match error {
        roxmltree::Error::DtdDetected => AppcastError::Doctype,
        other => AppcastError::NotXml(other.to_string()),
    })?;
    let channel = document
        .root_element()
        .children()
        .find(|node| node.has_tag_name("channel"))
        .ok_or(AppcastError::NoChannel)?;
    let mut appcast = Appcast::default();
    for (index, item) in channel
        .children()
        .filter(|node| node.has_tag_name("item"))
        .enumerate()
    {
        match read_item(item) {
            Ok(item) => appcast.items.push(item),
            Err(why) => appcast.skipped.push(format!("item {}: {why}", index + 1)),
        }
    }
    Ok(appcast)
}

fn sparkle_child<'a>(item: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    item.children()
        .find(|node| node.has_tag_name((SPARKLE_NS, name)))
        .map(|node| node.text().unwrap_or("").trim())
}

fn read_item(item: Node<'_, '_>) -> Result<Item, String> {
    let enclosures = item
        .children()
        .filter(|node| node.has_tag_name("enclosure"))
        .map(read_enclosure)
        .collect::<Result<Vec<_>, _>>()?;
    let from_enclosure = |name: &str| {
        item.children()
            .filter(|node| node.has_tag_name("enclosure"))
            .find_map(|node| node.attribute((SPARKLE_NS, name)))
            .map(str::trim)
    };
    // Sparkle's sparkle:version must be readable when given, though the
    // comparison uses the short version.
    if let Some(version) = sparkle_child(item, "version").or_else(|| from_enclosure("version"))
        && version.is_empty()
    {
        return Err("an empty sparkle:version".to_owned());
    }
    let short_text = sparkle_child(item, "shortVersionString")
        .or_else(|| from_enclosure("shortVersionString"))
        .ok_or("no sparkle:shortVersionString")?;
    let short_version = Version::parse(short_text)
        .ok_or_else(|| format!("short version {short_text:?} isn't dotted numbers"))?;
    let minimum_system = match sparkle_child(item, "minimumSystemVersion") {
        None => None,
        Some(text) => Some(
            Version::parse(text)
                .ok_or_else(|| format!("minimum system version {text:?} isn't dotted numbers"))?,
        ),
    };
    Ok(Item {
        short_version,
        short_version_text: short_text.to_owned(),
        channel: sparkle_child(item, "channel").map(str::to_owned),
        minimum_system,
        enclosures,
    })
}

fn read_enclosure(node: Node<'_, '_>) -> Result<Enclosure, String> {
    let url = node
        .attribute("url")
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or("an enclosure without a url")?;
    let length = match node.attribute("length").map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            text.parse::<u64>()
                .map_err(|_| format!("enclosure length {text:?} isn't a number"))?,
        ),
    };
    Ok(Enclosure {
        url: url.to_owned(),
        length,
        os: node
            .attribute((SPARKLE_NS, "os"))
            .map(|os| os.trim().to_owned()),
        signature: node
            .attribute((SPARKLE_NS, "edSignature"))
            .map(|signature| signature.trim().to_owned()),
    })
}

/// The update offered: the newest candidate above the running version.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: Version,
    pub version_text: String,
    pub enclosure: Enclosure,
}

/// The update for `arch` above `running`, on Windows `system`, or `None`.
pub fn pick(appcast: &Appcast, arch: Arch, running: &Version, system: &Version) -> Option<Update> {
    let mut best: Option<Update> = None;
    for item in &appcast.items {
        if item.channel.is_some() {
            continue;
        }
        if item.minimum_system.as_ref().is_some_and(|min| min > system) {
            continue;
        }
        if item.short_version <= *running
            || best
                .as_ref()
                .is_some_and(|best| item.short_version <= best.version)
        {
            continue;
        }
        let Some(enclosure) = item
            .enclosures
            .iter()
            .find(|enclosure| enclosure.os.as_deref() == Some(arch.os_value()))
        else {
            continue;
        };
        best = Some(Update {
            version: item.short_version.clone(),
            version_text: item.short_version_text.clone(),
            enclosure: enclosure.clone(),
        });
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(items: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="{SPARKLE_NS}">
<channel><title>AudioSync</title>{items}</channel></rss>"#
        )
    }

    fn windows_item(version: &str, os: &str) -> String {
        format!(
            r#"<item><title>{version}</title>
<sparkle:version>{version}</sparkle:version>
<sparkle:shortVersionString>{version}</sparkle:shortVersionString>
<enclosure url="https://example.invalid/{os}-{version}.zip" length="10" type="application/octet-stream"
 sparkle:os="{os}" sparkle:edSignature="c2ln"/></item>"#
        )
    }

    fn v(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    fn win11() -> Version {
        v("10.0.22631")
    }

    #[test]
    fn an_appcast_with_a_doctype_is_refused_and_a_broken_item_is_skipped() {
        let doctype = format!(
            "<?xml version=\"1.0\"?><!DOCTYPE rss [<!ENTITY a \"aaaa\">]>{}",
            feed("").trim_start_matches(r#"<?xml version="1.0" encoding="utf-8"?>"#)
        );
        assert_eq!(parse(&doctype).unwrap_err(), AppcastError::Doctype);

        let text = feed(&format!(
            "{}<item><sparkle:shortVersionString>0.9.x</sparkle:shortVersionString></item>\
             <item><title>no version</title></item>\
             <item><sparkle:shortVersionString>0.9</sparkle:shortVersionString>\
             <enclosure url=\"https://e.invalid/a.zip\" length=\"ten\" sparkle:os=\"windows-x64\"/></item>",
            windows_item("0.8.0", "windows-x64")
        ));
        let appcast = parse(&text).expect("the feed reads");
        assert_eq!(appcast.items.len(), 1);
        assert_eq!(appcast.skipped.len(), 3, "{:?}", appcast.skipped);
        assert_eq!(appcast.items[0].short_version, v("0.8"));
    }

    #[test]
    fn version_elements_win_over_enclosure_attributes() {
        let text = feed(
            r#"<item><sparkle:shortVersionString>0.9.1</sparkle:shortVersionString>
<enclosure url="https://e.invalid/a.zip" sparkle:shortVersionString="0.5" sparkle:os="windows-x64"/></item>
<item><enclosure url="https://e.invalid/b.zip" sparkle:version="7" sparkle:shortVersionString="0.6.2" sparkle:os="windows-arm64"/></item>"#,
        );
        let appcast = parse(&text).expect("the feed reads");
        assert_eq!(appcast.items[0].short_version, v("0.9.1"));
        assert_eq!(appcast.items[1].short_version, v("0.6.2"));
    }

    #[test]
    fn versions_compare_as_dotted_numbers_and_a_non_numeric_part_skips_the_item() {
        assert!(v("0.7.10") > v("0.7.9"));
        assert_eq!(v("0.8"), v("0.8.0"));
        assert!(v("1") > v("0.99.99"));
        for bad in ["", "0.8-beta", "0..8", "v1", "0.8.", " . "] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        let text = feed(&format!(
            "{}{}",
            windows_item("0.9-rc1", "windows-x64"),
            windows_item("0.8.1", "windows-x64")
        ));
        let appcast = parse(&text).expect("the feed reads");
        let update = pick(&appcast, Arch::X64, &v("0.8"), &win11()).expect("an update");
        assert_eq!(update.version_text, "0.8.1");
    }

    #[test]
    fn channel_items_plain_windows_and_a_minimum_system_version_above_the_running_one_are_skipped()
    {
        let text = feed(&format!(
            "<item><sparkle:channel>beta</sparkle:channel>\
             <sparkle:shortVersionString>2.0</sparkle:shortVersionString>\
             <enclosure url=\"https://e.invalid/beta.zip\" sparkle:os=\"windows-x64\"/></item>\
             <item><sparkle:minimumSystemVersion>10.0.99999</sparkle:minimumSystemVersion>\
             <sparkle:shortVersionString>1.9</sparkle:shortVersionString>\
             <enclosure url=\"https://e.invalid/future.zip\" sparkle:os=\"windows-x64\"/></item>\
             {}{}{}{}\
             <item><sparkle:shortVersionString>1.5</sparkle:shortVersionString>\
             <enclosure url=\"https://e.invalid/mac.zip\"/></item>\
             <item><sparkle:minimumSystemVersion>10.0.22000</sparkle:minimumSystemVersion>\
             <sparkle:shortVersionString>1.1</sparkle:shortVersionString>\
             <enclosure url=\"https://e.invalid/ok.zip\" sparkle:os=\"windows-x64\"/></item>",
            windows_item("1.8", "windows"),
            windows_item("1.7", "windows-x86"),
            windows_item("1.6", "macos"),
            windows_item("1.0", "windows-arm64"),
        ));
        let appcast = parse(&text).expect("the feed reads");
        let update = pick(&appcast, Arch::X64, &v("0.7.0"), &win11()).expect("an update");
        assert_eq!(update.enclosure.url, "https://e.invalid/ok.zip");
        let arm = pick(&appcast, Arch::Arm64, &v("0.7.0"), &win11()).expect("an update");
        assert_eq!(arm.version_text, "1.0");
    }

    #[test]
    fn the_newest_above_the_running_version_wins_and_the_same_or_older_offers_nothing() {
        let text = feed(&format!(
            "{}{}{}",
            windows_item("0.8.0", "windows-x64"),
            windows_item("0.9.0", "windows-x64"),
            windows_item("0.8.5", "windows-x64")
        ));
        let appcast = parse(&text).expect("the feed reads");
        let update = pick(&appcast, Arch::X64, &v("0.7.0"), &win11()).expect("an update");
        assert_eq!(update.version_text, "0.9.0");
        assert_eq!(pick(&appcast, Arch::X64, &v("0.9"), &win11()), None);
        assert_eq!(pick(&appcast, Arch::X64, &v("1.0"), &win11()), None);
        assert_eq!(pick(&appcast, Arch::Arm64, &v("0.1"), &win11()), None);
    }
}
