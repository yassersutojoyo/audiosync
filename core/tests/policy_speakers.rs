//! Speaker keys, group names, the device list, the layout and notices (S145,
//! S147, S149 to S153).

use std::cmp::Ordering;

use audiosync_core::policy::speakers::{
    Layout, ListedSpeaker, MAX_NOTICES, NoticeSubject, Notices, PrivacyPane, SpeakerKey, Transport,
    TypeTag, compare_names, group_name, layout, listed_devices, new_group_name, renamed_group,
};
use proptest::prelude::*;

fn group_label(n: usize) -> String {
    format!("Group {n}")
}

fn local(uid: &str, name: &str, transport: Transport) -> ListedSpeaker {
    ListedSpeaker {
        key: SpeakerKey::Local(uid.to_owned()),
        name: name.to_owned(),
        transport: Some(transport),
    }
}

fn airplay(id: &str, name: &str) -> ListedSpeaker {
    ListedSpeaker {
        key: SpeakerKey::AirPlay(id.to_owned()),
        name: name.to_owned(),
        transport: None,
    }
}

// covers: S149
#[test]
fn speaker_keys_have_an_airplay_or_local_prefix() {
    assert_eq!(
        SpeakerKey::parse("ap:42"),
        Some(SpeakerKey::AirPlay("42".to_owned()))
    );
    assert_eq!(SpeakerKey::AirPlay("42".to_owned()).to_string(), "ap:42");
    // Local parse keeps the colons of the UID.
    let key = SpeakerKey::parse("lc:AppleUSBAudioEngine:Vendor:Model:1:2").unwrap();
    assert_eq!(
        key,
        SpeakerKey::Local("AppleUSBAudioEngine:Vendor:Model:1:2".to_owned())
    );
    assert_eq!(key.id(), "AppleUSBAudioEngine:Vendor:Model:1:2");
    assert_eq!(SpeakerKey::parse(&key.to_string()), Some(key));
    // Unknown prefixes are rejected.
    assert_eq!(SpeakerKey::parse("bt:42"), None);
    assert_eq!(SpeakerKey::parse("AP:42"), None);
    assert_eq!(SpeakerKey::parse("42"), None);
}

// covers: S150
#[test]
fn group_names_are_trimmed_and_blank_is_no_name() {
    assert_eq!(group_name("  Living room \n"), Some("Living room"));
    assert_eq!(group_name(""), None);
    assert_eq!(group_name(" \t\n "), None);
    assert_eq!(
        new_group_name(" Upstairs ", &["Group 1"], group_label),
        "Upstairs",
        "a typed name is kept"
    );
}

// covers: S150
#[test]
fn a_new_group_without_a_name_counts_up_past_names_in_use() {
    let none: [&str; 0] = [];
    assert_eq!(
        new_group_name("  ", &none, group_label),
        "Group 1",
        "blank first group"
    );
    assert_eq!(
        new_group_name("", &["Kitchen"], group_label),
        "Group 2",
        "blank second group"
    );
    assert_eq!(
        new_group_name("", &["Group 2", "Kitchen"], group_label),
        "Group 3",
        "skips names in use"
    );
    assert_eq!(
        new_group_name("", &["Group 3", "Group 4", "x"], group_label),
        "Group 5"
    );
}

// covers: S150
#[test]
fn a_rename_is_trimmed_and_a_blank_one_keeps_the_name() {
    assert_eq!(renamed_group("Group 1", "  Den  "), "Den");
    assert_eq!(renamed_group("Group 1", "   "), "Group 1");
}

// covers: S151, S145
#[test]
fn virtual_aggregate_and_the_systems_airplay_output_are_hidden_by_default() {
    let devices = [
        ("builtin", Transport::BuiltIn, false),
        ("blackhole", Transport::Virtual, false),
        ("agg", Transport::Aggregate, false),
        ("auto", Transport::AutoAggregate, false),
        ("airplay", Transport::AirPlay, false),
        ("usb", Transport::Usb, false),
    ];
    let shown: Vec<&str> = listed_devices(&devices, false, |d| d.1, |d| d.2)
        .iter()
        .map(|d| d.0)
        .collect();
    assert_eq!(shown, ["builtin", "usb"]);
    let everything: Vec<&str> = listed_devices(&devices, true, |d| d.1, |d| d.2)
        .iter()
        .map(|d| d.0)
        .collect();
    assert_eq!(
        everything,
        ["builtin", "blackhole", "agg", "auto", "airplay", "usb"],
        "Show Virtual Devices"
    );
}

// covers: S151
#[test]
fn a_chosen_hidden_device_stays_listed_in_order() {
    let devices = [
        ("builtin", Transport::BuiltIn, false),
        ("blackhole", Transport::Virtual, true),
        ("usb", Transport::Usb, false),
        ("agg", Transport::Aggregate, false),
    ];
    let shown: Vec<&str> = listed_devices(&devices, false, |d| d.1, |d| d.2)
        .iter()
        .map(|d| d.0)
        .collect();
    assert_eq!(shown, ["builtin", "blackhole", "usb"]);
}

// covers: S151
#[test]
fn hardware_and_unknown_transports_are_never_hidden() {
    for transport in [
        Transport::BuiltIn,
        Transport::Usb,
        Transport::Bluetooth,
        Transport::BluetoothLe,
        Transport::Hdmi,
        Transport::DisplayPort,
        Transport::Thunderbolt,
        Transport::FireWire,
        Transport::Pci,
        Transport::Avb,
        Transport::Unknown,
    ] {
        assert!(!transport.hidden_by_default(), "{transport:?}");
    }
}

// covers: S152
#[test]
fn transport_types_map_to_their_tag_and_only_built_in_is_this_mac() {
    assert_eq!(Transport::Bluetooth.tag(), TypeTag::Bluetooth);
    assert_eq!(Transport::BluetoothLe.tag(), TypeTag::Bluetooth);
    assert_eq!(Transport::AutoAggregate.tag(), TypeTag::Aggregate);
    assert_eq!(Transport::Aggregate.tag(), TypeTag::Aggregate);
    assert_eq!(Transport::Unknown.tag(), TypeTag::Other);
    assert_eq!(Transport::AirPlay.tag(), TypeTag::AirPlay);
    assert_eq!(Transport::Usb.tag(), TypeTag::Usb);
    assert!(Transport::BuiltIn.is_this_computer());
    assert!(!Transport::Usb.is_this_computer());
}

// covers: S152
#[test]
fn layout_this_mac_keeps_the_os_order_and_everything_else_sorts_by_name() {
    let speakers = [
        local("speakers", "MacBook Speakers", Transport::BuiltIn),
        airplay("9", "kitchen"),
        local("usb", "USB DAC", Transport::Usb),
        local("headphones", "External Headphones", Transport::BuiltIn),
        airplay("3", "Bedroom 10"),
        airplay("4", "Bedroom 2"),
        local("bt", "AirPods", Transport::Bluetooth),
    ];
    let Layout {
        this_computer,
        connected,
    } = layout(&speakers);
    assert_eq!(
        this_computer,
        [
            SpeakerKey::Local("speakers".to_owned()),
            SpeakerKey::Local("headphones".to_owned())
        ]
    );
    let names: Vec<String> = connected.iter().map(|key| key.to_string()).collect();
    assert_eq!(
        names,
        ["lc:bt", "ap:4", "ap:3", "ap:9", "lc:usb"],
        "AirPlay and local share one list"
    );
    assert_eq!(layout(&[]), Layout::default(), "no speakers, empty lists");
}

// covers: S152, S147
#[test]
fn names_sort_ignoring_case_with_numbers_in_order_and_ties_stable() {
    assert_eq!(compare_names("Room 2", "Room 10"), Ordering::Less);
    assert_eq!(compare_names("room 2", "ROOM 2"), Ordering::Equal);
    assert_eq!(compare_names("Den", "den 1"), Ordering::Less);
    assert_eq!(compare_names("Speaker 007", "Speaker 7"), Ordering::Equal);
    assert_eq!(
        compare_names("a99999999999999999999999", "a100000000000000000000000"),
        Ordering::Less
    );
    assert_eq!(compare_names("Älv", "älv"), Ordering::Equal);
    let speakers = [
        airplay("1", "same"),
        airplay("2", "Same"),
        airplay("3", "SAME"),
    ];
    let ids: Vec<String> = layout(&speakers)
        .connected
        .iter()
        .map(|key| key.id().to_owned())
        .collect();
    assert_eq!(ids, ["1", "2", "3"], "ties keep their order");
}

// covers: S147
#[test]
fn long_unicode_names_are_display_only() {
    let long = "🔊 Wohnzimmer – Lautsprecher ".repeat(500);
    let speakers = [
        airplay("a", &long),
        airplay("b", "Bad"),
        local("c", &long, Transport::Usb),
    ];
    let order: Vec<String> = layout(&speakers)
        .connected
        .iter()
        .map(|key| key.to_string())
        .collect();
    // Keys, not names, identify speakers: equal names stay distinct.
    assert_eq!(order, ["ap:b", "ap:a", "lc:c"]);
}

// covers: S153
#[test]
fn notices_are_newest_first_one_per_subject_and_capped_at_four() {
    let mut notices: Notices<NoticeSubject, &str> = Notices::new();
    assert!(notices.is_empty(), "start empty");
    notices.post(NoticeSubject::Start, "first");
    notices.post(NoticeSubject::Source, "second");
    let subjects: Vec<&NoticeSubject> = notices.iter().map(|(subject, _)| subject).collect();
    assert_eq!(
        subjects,
        [&NoticeSubject::Source, &NoticeSubject::Start],
        "newest first, both kept"
    );
    notices.post(NoticeSubject::Start, "again");
    let order: Vec<&str> = notices.iter().map(|(_, text)| *text).collect();
    assert_eq!(
        order,
        ["again", "second"],
        "same subject replaces the old one and moves to the top"
    );
    notices.post(NoticeSubject::Capture, "c");
    notices.post(NoticeSubject::LocalNetwork, "d");
    notices.post(
        NoticeSubject::Speaker(SpeakerKey::AirPlay("7".to_owned())),
        "e",
    );
    assert_eq!(notices.len(), MAX_NOTICES, "capped");
    assert_eq!(
        notices.get(&NoticeSubject::Source),
        None,
        "the oldest drop off"
    );
}

// covers: S153
#[test]
fn clearing_removes_only_the_given_subjects() {
    let mut notices: Notices<NoticeSubject, &str> = Notices::new();
    notices.post(NoticeSubject::Start, "a");
    notices.post(NoticeSubject::Source, "b");
    assert!(notices.clear(&[NoticeSubject::Start]));
    assert_eq!(notices.len(), 1);
    assert!(
        !notices.clear(&[NoticeSubject::Capture]),
        "clearing an absent subject changes nothing"
    );
    assert!(notices.clear_all());
    assert!(!notices.clear_all());
}

// covers: S153
#[test]
fn automation_notices_are_per_player_and_carry_their_pane() {
    let mut notices: Notices<NoticeSubject, Option<PrivacyPane>> = Notices::new();
    let spotify = NoticeSubject::Automation("com.spotify.client".to_owned());
    let music = NoticeSubject::Automation("com.apple.Music".to_owned());
    notices.post(spotify.clone(), Some(PrivacyPane::Automation));
    notices.post(music.clone(), Some(PrivacyPane::Automation));
    notices.post(NoticeSubject::Start, None);
    assert_eq!(notices.len(), 3);
    assert_eq!(
        notices.get(&NoticeSubject::Start),
        Some(&None),
        "no settings button unless given"
    );
    assert!(
        notices.clear(std::slice::from_ref(&spotify)),
        "allowing one player clears only its notice"
    );
    assert_eq!(notices.get(&music), Some(&Some(PrivacyPane::Automation)));
}

proptest! {
    // covers: S149
    #[test]
    fn speaker_keys_round_trip(id in proptest::collection::vec(any::<char>(), 0..40), airplay_key in any::<bool>()) {
        let id: String = id.into_iter().collect();
        let key = if airplay_key { SpeakerKey::AirPlay(id) } else { SpeakerKey::Local(id) };
        prop_assert_eq!(SpeakerKey::parse(&key.to_string()), Some(key));
    }

    // covers: S152
    #[test]
    fn name_order_is_a_total_order(a in "[a-cA-C0-9 ]{0,6}", b in "[a-cA-C0-9 ]{0,6}", c in "[a-cA-C0-9 ]{0,6}") {
        prop_assert_eq!(compare_names(&a, &b), compare_names(&b, &a).reverse());
        if compare_names(&a, &b) != Ordering::Greater && compare_names(&b, &c) != Ordering::Greater {
            prop_assert_ne!(compare_names(&a, &c), Ordering::Greater);
        }
    }
}
