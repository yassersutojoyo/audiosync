//! Zones, their click plan and the migration from groups (S150, S245 to S248).

use audiosync_core::policy::zones::{
    CastLeftOff, ClickAction, ClickPlan, Member, MemberKind, SavedGroup, Situation, Step, Zone,
    Zones, migrate, zones_from_groups,
};

fn zone_label(n: usize) -> String {
    format!("Zone {n}")
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|key| (*key).to_owned()).collect()
}

fn zone(id: &str, name: &str, members: &[&str]) -> Zone {
    Zone {
        id: id.to_owned(),
        name: name.to_owned(),
        members: keys(members),
        volume: 100.0,
        member_names: Default::default(),
    }
}

fn members_of(zones: &Zones) -> Vec<(String, Vec<String>)> {
    zones
        .list()
        .iter()
        .map(|zone| (zone.id.clone(), zone.members.clone()))
        .collect()
}

fn member(kind: MemberKind, on: bool) -> Member {
    Member {
        kind,
        listed: true,
        on,
        busy: false,
        needs_airplay2: false,
        in_target: false,
    }
}

fn playing() -> Situation {
    Situation {
        streaming: true,
        ..Situation::default()
    }
}

fn plan(members: &[Member], situation: Situation) -> ClickPlan {
    audiosync_core::policy::zones::click_plan(members, situation)
}

// covers: S150, S245
#[test]
fn zone_names_follow_the_group_name_rules() {
    let mut zones = Zones::default();
    assert_eq!(
        zones.new_zone("z1", "  Living Room ", zone_label).name,
        "Living Room"
    );
    assert_eq!(zones.new_zone("z2", " \t ", zone_label).name, "Zone 2");
    // "Zone 3" is in use, so the next unnamed zone counts past it.
    assert_eq!(zones.new_zone("z3", "Zone 4", zone_label).name, "Zone 4");
    assert_eq!(zones.new_zone("z4", "", zone_label).name, "Zone 5");
    assert!(zones.rename("z1", "   "));
    assert_eq!(zones.get("z1").unwrap().name, "Living Room");
    assert!(zones.rename("z1", " Den "));
    assert_eq!(zones.get("z1").unwrap().name, "Den");
    assert!(!zones.rename("nope", "Den"));
    // A new zone is empty at volume 100, and an ID in use adds nothing.
    let added = zones.get("z2").unwrap();
    assert!(added.members.is_empty() && added.volume == 100.0);
    assert_eq!(zones.new_zone("z1", "Other", zone_label).name, "Den");
    assert_eq!(zones.list().len(), 4);
    // A migrated group keeps its name as it is, "Group 2" included.
    let group = keys(&["lc:usb"]);
    let migrated = zones_from_groups([SavedGroup {
        id: "g2",
        name: "Group 2",
        members: &group,
    }]);
    assert_eq!(migrated.list()[0].name, "Group 2");
}

// covers: S245
#[test]
fn a_speaker_moved_to_another_zone_leaves_its_first() {
    let mut zones = Zones::from_stored([
        zone("a", "Kitchen", &["lc:usb", "ap:native:AABBCCDDEE01"]),
        zone("b", "Den", &["cc:Chromecast-1"]),
    ]);
    assert!(zones.move_speaker("lc:usb", "b"));
    assert_eq!(
        members_of(&zones),
        [
            ("a".to_owned(), keys(&["ap:native:AABBCCDDEE01"])),
            ("b".to_owned(), keys(&["cc:Chromecast-1", "lc:usb"])),
        ]
    );
    // Already there: it keeps its place.
    assert!(zones.move_speaker("cc:Chromecast-1", "b"));
    assert_eq!(zones.get("b").unwrap().members[0], "cc:Chromecast-1");
    // No such zone: nothing moves.
    assert!(!zones.move_speaker("lc:usb", "nope"));
    assert_eq!(zones.zone_of("lc:usb").unwrap().id, "b");
    // A Cast group and one of its members are different keys.
    assert!(zones.move_speaker("cc:Group-7", "a"));
    assert_eq!(zones.zone_of("cc:Chromecast-1").unwrap().id, "b");
    assert_eq!(zones.zone_of("cc:Group-7").unwrap().id, "a");
    // Stored zones that share a key keep it in the first one only.
    let stored = Zones::from_stored([
        zone("a", "A", &["lc:usb", "lc:usb", "ap:1"]),
        zone("b", "B", &["ap:1", "ap:2"]),
        zone("a", "A again", &["ap:3"]),
    ]);
    assert_eq!(
        members_of(&stored),
        [
            ("a".to_owned(), keys(&["lc:usb", "ap:1"])),
            ("b".to_owned(), keys(&["ap:2"])),
        ]
    );
    // The S204 key change: in place, unless a zone holds the new key.
    let mut renamed = Zones::from_stored([
        zone("a", "A", &["ap:1", "ap:native:X"]),
        zone("b", "B", &["ap:2"]),
    ]);
    renamed.rename_key("ap:2", "ap:native:Y");
    renamed.rename_key("ap:1", "ap:native:X");
    assert_eq!(
        members_of(&renamed),
        [
            ("a".to_owned(), keys(&["ap:native:X"])),
            ("b".to_owned(), keys(&["ap:native:Y"])),
        ]
    );
}

// covers: S245
#[test]
fn deleting_a_zone_leaves_its_members_in_no_zone() {
    let mut zones = Zones::from_stored([
        zone("a", "Kitchen", &["lc:usb"]),
        zone("b", "Den", &["ap:1", "ap:2"]),
    ]);
    assert!(zones.set_volume("a", 140.0));
    assert!(zones.delete("b"));
    assert!(zones.zone_of("ap:1").is_none() && zones.zone_of("ap:2").is_none());
    assert_eq!(
        zones.list(),
        [Zone {
            volume: 100.0,
            ..zone("a", "Kitchen", &["lc:usb"])
        }]
    );
    assert!(!zones.delete("b"));
    // The last member forgotten: the zone stays, empty, until deleted.
    assert!(zones.remove_speaker("lc:usb"));
    assert_eq!(zones.list().len(), 1);
    assert!(zones.list()[0].members.is_empty());
    assert!(!zones.remove_speaker("lc:usb"));
    // Volumes stay within 0 to 100; one that isn't a number is 100.
    assert!(zones.set_volume("a", -5.0));
    assert_eq!(zones.list()[0].volume, 0.0);
    assert!(zones.set_volume("a", f64::NAN));
    assert_eq!(zones.list()[0].volume, 100.0);
    assert!(!zones.set_volume("b", 50.0));
}

// covers: S286
#[test]
fn an_absent_members_last_listed_name_survives_a_reload() {
    let mut zones = Zones::from_stored([zone("a", "Kitchen", &["ap:native:X", "lc:usb"])]);
    assert!(zones.remember_name("ap:native:X", "Kitchen HomePod"));
    assert!(!zones.remember_name("ap:native:X", "Kitchen HomePod"));
    // A blank name keeps the last one; a key in no zone keeps none.
    assert!(!zones.remember_name("ap:native:X", "  "));
    assert!(!zones.remember_name("ap:native:Y", "Porch"));
    assert_eq!(zones.member_name("ap:native:Y"), None);
    let reloaded = Zones::from_stored(zones.into_list());
    assert_eq!(reloaded.member_name("ap:native:X"), Some("Kitchen HomePod"));
    assert_eq!(reloaded.member_name("lc:usb"), None);
}

// covers: S286
#[test]
fn a_members_name_follows_it_and_leaves_with_it() {
    let mut zones = Zones::from_stored([zone("a", "A", &["ap:1", "ap:2"]), zone("b", "B", &[])]);
    assert!(zones.remember_name("ap:1", "Den"));
    assert!(zones.remember_name("ap:2", "Hall"));
    assert!(zones.move_speaker("ap:1", "b"));
    assert_eq!(zones.member_name("ap:1"), Some("Den"));
    assert!(!zones.get("a").unwrap().member_names.contains_key("ap:1"));
    // The S204 key change carries the name; a key a zone holds already keeps its own.
    zones.rename_key("ap:1", "ap:native:D");
    assert_eq!(zones.member_name("ap:native:D"), Some("Den"));
    assert_eq!(zones.member_name("ap:1"), None);
    assert!(zones.remove_speaker("ap:2"));
    assert!(zones.get("a").unwrap().member_names.is_empty());
    // A stored name without its member is dropped on load.
    let mut stray = zone("c", "C", &["lc:usb"]);
    stray
        .member_names
        .insert("ap:9".to_owned(), "Gone".to_owned());
    stray
        .member_names
        .insert("lc:usb".to_owned(), "USB DAC".to_owned());
    let loaded = Zones::from_stored([stray]);
    assert_eq!(loaded.member_name("ap:9"), None);
    assert_eq!(loaded.member_name("lc:usb"), Some("USB DAC"));
}

// covers: S246
#[test]
fn a_zone_click_switches_only_present_members_and_hidden_ones_are_absent() {
    let hidden = Member {
        listed: false,
        ..member(MemberKind::Local, false)
    };
    let members = [
        member(MemberKind::Local, false),
        hidden,
        member(MemberKind::AirPlay, true),
    ];
    let on = plan(&members, Situation::default());
    assert_eq!(
        on.action,
        ClickAction::TurnOn {
            start_streaming: true
        }
    );
    assert_eq!(on.steps, [Step::On, Step::Absent, Step::On]);
    // Every present member on while streaming: the click switches them off.
    let all_on = [
        member(MemberKind::Local, true),
        hidden,
        member(MemberKind::AirPlay, true),
    ];
    let off = plan(&all_on, playing());
    assert_eq!(off.action, ClickAction::TurnOff);
    assert_eq!(off.steps, [Step::Off, Step::Absent, Step::Off]);
    // Streaming off: the click starts it, even with every member on.
    assert_eq!(
        plan(&all_on, Situation::default()).action,
        ClickAction::TurnOn {
            start_streaming: true
        }
    );
    // One present member off: the click switches the rest on.
    let some = plan(&members, playing());
    assert_eq!(
        some.action,
        ClickAction::TurnOn {
            start_streaming: false
        }
    );
    // No member present: nothing.
    let none = plan(&[hidden, hidden], playing());
    assert_eq!(none.action, ClickAction::Nothing);
    assert_eq!(none.steps, [Step::Absent, Step::Absent]);
    assert_eq!(plan(&[], playing()).action, ClickAction::Nothing);
}

// covers: S246
#[test]
fn a_zone_click_turns_on_one_cast_member_the_first_not_busy() {
    let busy = Member {
        busy: true,
        ..member(MemberKind::Cast, false)
    };
    let members = [
        busy,
        member(MemberKind::Cast, false),
        member(MemberKind::Local, false),
        member(MemberKind::Cast, false),
    ];
    let on = plan(&members, playing());
    assert_eq!(
        on.action,
        ClickAction::TurnOn {
            start_streaming: false
        }
    );
    assert_eq!(
        on.steps,
        [
            Step::CastLeftOff(CastLeftOff::OneCastAtATime),
            Step::On,
            Step::On,
            Step::CastLeftOff(CastLeftOff::OneCastAtATime),
        ]
    );
    // A Cast member already on is the one; the ones left off count toward
    // neither on nor off, so the zone reads on and the next click is off.
    let chosen = [
        member(MemberKind::Cast, false),
        member(MemberKind::Cast, true),
        member(MemberKind::Local, true),
    ];
    let off = plan(&chosen, playing());
    assert_eq!(off.action, ClickAction::TurnOff);
    assert_eq!(off.steps, [Step::Off, Step::Off, Step::Off]);
    // Every Cast member busy: none goes on, each says why.
    let all_busy = plan(&[busy, member(MemberKind::Local, false)], playing());
    assert_eq!(
        all_busy.steps,
        [Step::CastLeftOff(CastLeftOff::Busy), Step::On]
    );
    let only_busy = plan(&[busy], playing());
    assert_eq!(only_busy.action, ClickAction::Nothing);
    assert_eq!(only_busy.steps, [Step::CastLeftOff(CastLeftOff::Busy)]);
}

// covers: S246
#[test]
fn a_zone_click_never_switches_off_a_cast_outside_its_zone() {
    let elsewhere = Situation {
        cast_on_elsewhere: true,
        ..playing()
    };
    let members = [
        member(MemberKind::Cast, false),
        member(MemberKind::Local, false),
    ];
    let on = plan(&members, elsewhere);
    assert_eq!(
        on.action,
        ClickAction::TurnOn {
            start_streaming: false
        }
    );
    assert_eq!(
        on.steps,
        [Step::CastLeftOff(CastLeftOff::AnotherCastPlays), Step::On]
    );
    // With the local member on, the zone reads on: the next click turns
    // it off and still leaves the other Cast alone.
    let on_already = [
        member(MemberKind::Cast, false),
        member(MemberKind::Local, true),
    ];
    assert_eq!(plan(&on_already, elsewhere).action, ClickAction::TurnOff);
    let cast_only = plan(&[member(MemberKind::Cast, false)], elsewhere);
    assert_eq!(cast_only.action, ClickAction::Nothing);
}

// covers: S246
#[test]
fn two_airplay_2_members_both_switch_on_and_the_second_waits() {
    let members = [
        member(MemberKind::AirPlay, false),
        member(MemberKind::AirPlay2, false),
        member(MemberKind::AirPlay2, false),
    ];
    let on = plan(&members, playing());
    assert_eq!(on.steps, [Step::On, Step::On, Step::Waiting]);
    // The one already playing keeps playing; the other waits.
    let second_plays = [
        member(MemberKind::AirPlay2, false),
        member(MemberKind::AirPlay2, true),
    ];
    assert_eq!(
        plan(&second_plays, playing()).steps,
        [Step::Waiting, Step::On]
    );
    // Another AirPlay 2 speaker plays outside the zone: both wait.
    let elsewhere = Situation {
        airplay2_playing_elsewhere: true,
        ..playing()
    };
    assert_eq!(
        plan(&members, elsewhere).steps,
        [Step::On, Step::Waiting, Step::Waiting]
    );
    // Waiting counts as on: every member on means the next click is off.
    let all_on = [
        member(MemberKind::AirPlay2, true),
        member(MemberKind::AirPlay2, true),
    ];
    assert_eq!(plan(&all_on, playing()).action, ClickAction::TurnOff);
}

// covers: S246
#[test]
fn a_pair_member_with_airplay_2_off_needs_airplay_2_and_is_absent() {
    // Not listed, since targets are listed only while AirPlay 2 is on.
    let pair = Member {
        listed: false,
        needs_airplay2: true,
        ..member(MemberKind::AirPlay2, false)
    };
    let on = plan(&[pair, member(MemberKind::Local, true)], playing());
    assert_eq!(on.action, ClickAction::TurnOff);
    assert_eq!(on.steps, [Step::NeedsAirPlay2, Step::Off]);
    let alone = plan(&[pair], playing());
    assert_eq!(alone.action, ClickAction::Nothing);
    assert_eq!(alone.steps, [Step::NeedsAirPlay2]);
}

// covers: S247
#[test]
fn a_member_playing_as_part_of_a_target_is_skipped_by_the_click() {
    let in_pair = Member {
        in_target: true,
        ..member(MemberKind::AirPlay2, false)
    };
    let members = [
        in_pair,
        member(MemberKind::Local, true),
        member(MemberKind::AirPlay2, false),
    ];
    let on = plan(&members, playing());
    assert_eq!(
        on.action,
        ClickAction::TurnOn {
            start_streaming: false
        }
    );
    // The skipped member never takes the AirPlay 2 place either.
    assert_eq!(on.steps, [Step::InTarget, Step::On, Step::On]);
    let all_on = [in_pair, member(MemberKind::Local, true)];
    let off = plan(&all_on, playing());
    assert_eq!(off.action, ClickAction::TurnOff);
    assert_eq!(off.steps, [Step::InTarget, Step::Off]);
    assert_eq!(plan(&[in_pair], playing()).action, ClickAction::Nothing);
}

// covers: S248
#[test]
fn disjoint_groups_become_zones_unchanged() {
    let living = keys(&["lc:BuiltInSpeakerDevice", "ap:native:AABBCCDDEE01"]);
    let den = keys(&["cc:Chromecast-1", "ap:native:pair:5"]);
    let zones = zones_from_groups([
        SavedGroup {
            id: "g1",
            name: "Living Room",
            members: &living,
        },
        SavedGroup {
            id: "g2",
            name: "Den",
            members: &den,
        },
    ]);
    assert_eq!(
        zones.list(),
        [
            zone(
                "g1",
                "Living Room",
                &["lc:BuiltInSpeakerDevice", "ap:native:AABBCCDDEE01"]
            ),
            zone("g2", "Den", &["cc:Chromecast-1", "ap:native:pair:5"]),
        ]
    );
}

// covers: S248
#[test]
fn overlapping_groups_keep_each_speaker_in_its_first_zone() {
    let first = keys(&["lc:usb", "ap:1"]);
    let second = keys(&["ap:1", "ap:2"]);
    let only_shared = keys(&["lc:usb"]);
    let zones = zones_from_groups([
        SavedGroup {
            id: "g1",
            name: "First",
            members: &first,
        },
        SavedGroup {
            id: "g2",
            name: "Second",
            members: &second,
        },
        SavedGroup {
            id: "g3",
            name: "Third",
            members: &only_shared,
        },
    ]);
    // A group left with no member of its own is still a zone, empty.
    assert_eq!(
        zones.list(),
        [
            zone("g1", "First", &["lc:usb", "ap:1"]),
            zone("g2", "Second", &["ap:2"]),
            zone("g3", "Third", &[]),
        ]
    );
}

// covers: S248
#[test]
fn migrating_twice_changes_nothing() {
    let first = keys(&["lc:usb", "ap:1"]);
    let second = keys(&["ap:1", "ap:2"]);
    let groups = || {
        [
            SavedGroup {
                id: "g1",
                name: "First",
                members: &first,
            },
            SavedGroup {
                id: "g2",
                name: "Second",
                members: &second,
            },
        ]
    };
    // Cut short before the mark: the next run gives the same zones.
    let once = migrate(false, groups()).unwrap();
    assert_eq!(migrate(false, groups()), Some(once.clone()));
    // Once marked, nothing changes, whatever the groups are now.
    assert_eq!(migrate(true, groups()), None);
    assert_eq!(migrate(true, []), None);
    assert_eq!(migrate(false, []), Some(Zones::default()));
}
