//! Speaker presets: contents, recall plan, the current-preset check, names
//! and the migration from groups (S251 to S254).

use audiosync_core::policy::presets::{
    LeftOff, Preset, PresetMember, PresetZone, RecallAction, RecallMember, RecallOther, RecallStep,
    Saved, Scene, SpeakerNow, current, from_stored, migrate, new_preset_name, presets_from_groups,
    recall_plan, recall_zone_volumes, renamed_preset,
};
use audiosync_core::policy::zones::{MemberKind, SavedGroup, Zone, Zones};

const GAINS: [f64; 10] = [3.0, 2.0, 1.0, 0.0, -1.0, -1.5, 0.0, 1.0, 2.0, 3.5];

fn on(key: &str, volume: Option<f64>) -> SpeakerNow {
    SpeakerNow {
        key: key.to_owned(),
        volume,
    }
}

fn zones() -> Zones {
    Zones::from_stored([
        Zone {
            id: "z1".to_owned(),
            name: "Downstairs".to_owned(),
            members: vec!["lc:desk".to_owned(), "ap:kitchen".to_owned()],
            volume: 70.0,
        },
        Zone {
            id: "z2".to_owned(),
            name: "Den".to_owned(),
            members: vec!["cc:den".to_owned()],
            volume: 40.0,
        },
    ])
}

fn keys(preset: &Preset) -> Vec<&str> {
    preset
        .members
        .iter()
        .map(|member| member.key.as_str())
        .collect()
}

fn present(kind: MemberKind) -> RecallMember {
    RecallMember {
        kind,
        found: true,
        hidden: false,
        on: false,
        busy: false,
        needs_airplay2: false,
        in_target: false,
        fixed_volume: false,
        has_volume: true,
    }
}

const OFF: RecallOther = RecallOther {
    on: false,
    reference: false,
};
const ON: RecallOther = RecallOther {
    on: true,
    reference: false,
};
const REFERENCE: RecallOther = RecallOther {
    on: true,
    reference: true,
};

// covers: S251
#[test]
fn a_preset_stores_on_set_volumes_master_and_gains_never_delays() {
    let zones = zones();
    let speakers = [
        on("ap:kitchen", Some(55.0)),
        on("lc:desk", Some(30.0)),
        on("ap:pair:5A1B", Some(80.0)),
    ];
    let scene = Scene {
        on: &speakers,
        zones: &zones,
        master: 90.0,
        gains_db: [0.3, 2.0, 1.0, 0.0, -1.0, -1.5, 0.0, 1.0, 2.0, 30.0],
    };
    let preset = Preset::capture("preset:1", "Evening", &scene);
    assert_eq!(
        preset,
        Preset {
            id: "preset:1".to_owned(),
            name: "Evening".to_owned(),
            members: vec![
                PresetMember {
                    key: "ap:kitchen".to_owned(),
                    volume: Some(55.0)
                },
                PresetMember {
                    key: "lc:desk".to_owned(),
                    volume: Some(30.0)
                },
                PresetMember {
                    key: "ap:pair:5A1B".to_owned(),
                    volume: Some(80.0)
                },
            ],
            zones: vec![PresetZone {
                id: "z1".to_owned(),
                volume: 70.0
            }],
            master: 90.0,
            gains_db: [0.5, 2.0, 1.0, 0.0, -1.0, -1.5, 0.0, 1.0, 2.0, 12.0],
        },
        "list order, one zone volume per zone, gains on the controls' steps"
    );

    let stored = Preset {
        id: "p".to_owned(),
        name: "Loud".to_owned(),
        members: vec![
            PresetMember {
                key: "lc:desk".to_owned(),
                volume: Some(140.0),
            },
            PresetMember {
                key: "bt:x".to_owned(),
                volume: Some(10.0),
            },
            PresetMember {
                key: "lc:desk".to_owned(),
                volume: Some(20.0),
            },
            PresetMember {
                key: "ap:".to_owned(),
                volume: None,
            },
            PresetMember {
                key: "ap:kitchen".to_owned(),
                volume: Some(f64::NAN),
            },
        ],
        zones: vec![
            PresetZone {
                id: "z1".to_owned(),
                volume: -5.0,
            },
            PresetZone {
                id: "z1".to_owned(),
                volume: 50.0,
            },
        ],
        master: f64::NAN,
        gains_db: [f64::NAN; 10],
    }
    .sanitized();
    assert_eq!(keys(&stored), ["lc:desk", "ap:kitchen"]);
    assert_eq!(
        stored
            .members
            .iter()
            .map(|member| member.volume)
            .collect::<Vec<_>>(),
        [Some(100.0), None]
    );
    assert_eq!(
        stored.zones,
        [PresetZone {
            id: "z1".to_owned(),
            volume: 0.0
        }]
    );
    assert_eq!((stored.master, stored.gains_db), (100.0, [0.0; 10]));
}

// covers: S251
#[test]
fn a_preset_keeps_at_most_one_cast_key() {
    let zones = zones();
    let speakers = [
        on("cc:den", Some(25.0)),
        on("lc:desk", Some(30.0)),
        on("cc:attic", Some(60.0)),
        on("cc:fixed-tv", None),
    ];
    let preset = Preset::capture(
        "p",
        "Casts",
        &Scene {
            on: &speakers,
            zones: &zones,
            master: 100.0,
            gains_db: [0.0; 10],
        },
    );
    assert_eq!(keys(&preset), ["cc:den", "lc:desk"]);
    assert_eq!(preset.members[0].volume, Some(25.0), "a Cast own volume");
    let fixed = Preset::capture(
        "p",
        "Fixed",
        &Scene {
            on: &speakers[3..],
            zones: &zones,
            master: 100.0,
            gains_db: [0.0; 10],
        },
    );
    assert_eq!(fixed.members[0].volume, None, "a fixed Cast has none");
}

// covers: S252
#[test]
fn recall_switches_on_exactly_the_stored_set() {
    let members = [
        RecallMember {
            on: true,
            ..present(MemberKind::Local)
        },
        present(MemberKind::AirPlay),
    ];
    let plan = recall_plan(&members, &[ON, OFF, REFERENCE, ON], true);
    assert_eq!(
        plan.action,
        RecallAction::Apply {
            start_streaming: false
        }
    );
    assert_eq!(plan.steps, [RecallStep::On, RecallStep::On]);
    assert_eq!(
        plan.switch_off,
        [true, false, false, true],
        "every other speaker goes off but the reference output"
    );
    assert_eq!(plan.set_volume, [true, true]);
    assert!(!plan.needs_notice());
    let off = recall_plan(&members, &[ON], false);
    assert_eq!(
        off.action,
        RecallAction::Apply {
            start_streaming: true
        }
    );
}

// covers: S252
#[test]
fn recall_names_a_pair_member_that_needs_airplay_2() {
    let members = [
        RecallMember {
            found: false,
            needs_airplay2: true,
            ..present(MemberKind::AirPlay2)
        },
        RecallMember {
            in_target: true,
            ..present(MemberKind::AirPlay2)
        },
        present(MemberKind::AirPlay2),
        present(MemberKind::AirPlay2),
        RecallMember {
            on: true,
            ..present(MemberKind::AirPlay2)
        },
    ];
    let plan = recall_plan(&members, &[], true);
    assert_eq!(
        plan.steps,
        [
            RecallStep::LeftOff(LeftOff::NeedsAirPlay2),
            RecallStep::LeftOff(LeftOff::InTarget),
            RecallStep::Waiting,
            RecallStep::Waiting,
            RecallStep::On,
        ],
        "the AirPlay 2 member already playing keeps playing; the others wait"
    );
    assert!(plan.needs_notice());
    let stopped = recall_plan(&members, &[], false);
    assert_eq!(
        stopped.steps[2..],
        [RecallStep::On, RecallStep::Waiting, RecallStep::Waiting]
    );
}

// covers: S252
#[test]
fn recall_names_hidden_and_unlisted_members() {
    let members = [
        RecallMember {
            hidden: true,
            ..present(MemberKind::Local)
        },
        RecallMember {
            found: false,
            ..present(MemberKind::AirPlay)
        },
        present(MemberKind::Local),
    ];
    let plan = recall_plan(&members, &[], true);
    assert_eq!(
        plan.steps,
        [
            RecallStep::LeftOff(LeftOff::Hidden),
            RecallStep::LeftOff(LeftOff::NotFound),
            RecallStep::On,
        ]
    );
    assert_eq!(plan.set_volume, [false, false, true]);
    assert!(plan.needs_notice());
}

// covers: S252
#[test]
fn recall_turns_on_one_cast_or_notes_it_busy() {
    let plan = recall_plan(
        &[present(MemberKind::Cast), present(MemberKind::Cast)],
        &[ON],
        true,
    );
    assert_eq!(plan.steps, [RecallStep::On, RecallStep::Ignored]);
    assert_eq!(plan.switch_off, [true], "a Cast speaker outside goes off");
    let busy = recall_plan(
        &[
            RecallMember {
                busy: true,
                ..present(MemberKind::Cast)
            },
            present(MemberKind::Local),
        ],
        &[],
        true,
    );
    assert_eq!(
        busy.steps,
        [RecallStep::LeftOff(LeftOff::Busy), RecallStep::On]
    );
    assert!(busy.needs_notice());
}

// covers: S252
#[test]
fn recall_skips_the_volume_of_a_fixed_cast() {
    let plan = recall_plan(
        &[
            RecallMember {
                fixed_volume: true,
                ..present(MemberKind::Cast)
            },
            RecallMember {
                has_volume: false,
                ..present(MemberKind::Local)
            },
            present(MemberKind::AirPlay),
        ],
        &[],
        true,
    );
    assert_eq!(plan.steps, [RecallStep::On; 3]);
    assert_eq!(plan.set_volume, [false, false, true]);
}

// covers: S252
#[test]
fn recall_with_no_member_present_changes_nothing() {
    let members = [
        RecallMember {
            found: false,
            ..present(MemberKind::Local)
        },
        RecallMember {
            busy: true,
            ..present(MemberKind::Cast)
        },
    ];
    let plan = recall_plan(&members, &[ON, ON], true);
    assert_eq!(plan.action, RecallAction::Nothing);
    assert_eq!(plan.switch_off, [false, false]);
    assert_eq!(plan.set_volume, [false, false]);
    assert!(plan.needs_notice(), "only the notice");
    assert_eq!(recall_plan(&[], &[ON], false).action, RecallAction::Nothing);

    // A deleted zone is skipped (P2).
    let preset = Preset {
        id: "p".to_owned(),
        name: "P".to_owned(),
        members: Vec::new(),
        zones: vec![
            PresetZone {
                id: "gone".to_owned(),
                volume: 10.0,
            },
            PresetZone {
                id: "z2".to_owned(),
                volume: 15.0,
            },
        ],
        master: 100.0,
        gains_db: [0.0; 10],
    };
    assert_eq!(recall_zone_volumes(&preset, &zones()), [("z2", 15.0)]);
}

fn evening(zones: &Zones) -> (Vec<SpeakerNow>, Preset) {
    let speakers = vec![on("lc:desk", Some(30.0)), on("cc:den", None)];
    let preset = Preset::capture(
        "preset:1",
        "Evening",
        &Scene {
            on: &speakers,
            zones,
            master: 80.0,
            gains_db: GAINS,
        },
    );
    (speakers, preset)
}

// covers: S253
#[test]
fn any_change_breaks_the_match() {
    let zones = zones();
    let (speakers, preset) = evening(&zones);
    let scene = |on: &[SpeakerNow], zones: &Zones, master: f64, gains: [f64; 10]| {
        preset.is_current(&Scene {
            on,
            zones,
            master,
            gains_db: gains,
        })
    };
    assert!(scene(&speakers, &zones, 80.0, GAINS));
    let reversed: Vec<SpeakerNow> = speakers.iter().rev().cloned().collect();
    assert!(scene(&reversed, &zones, 80.0, GAINS), "order is ignored");

    let mut more = speakers.clone();
    more.push(on("ap:kitchen", Some(50.0)));
    assert!(!scene(&more, &zones, 80.0, GAINS), "a speaker switched on");
    assert!(
        !scene(&speakers[..1], &zones, 80.0, GAINS),
        "one switched off"
    );
    let mut slider = speakers.clone();
    slider[0].volume = Some(30.5);
    assert!(!scene(&slider, &zones, 80.0, GAINS), "an own volume");
    assert!(!scene(&speakers, &zones, 79.0, GAINS), "the master");
    let mut band = GAINS;
    band[4] += 0.5;
    assert!(!scene(&speakers, &zones, 80.0, band), "a band");
    let mut zone_moved = zones.clone();
    zone_moved.set_volume("z1", 71.0);
    assert!(!scene(&speakers, &zone_moved, 80.0, GAINS), "a zone volume");
    let mut zone_gone = zones.clone();
    zone_gone.delete("z2");
    zone_gone.delete("z1");
    assert!(
        scene(&speakers, &zone_gone, 80.0, GAINS),
        "a deleted zone is ignored (P2)"
    );
}

// covers: S253
#[test]
fn preamp_and_a_fixed_cast_volume_do_not_break_the_match() {
    let zones = zones();
    let (mut speakers, preset) = evening(&zones);
    speakers[1].volume = Some(12.0);
    assert!(preset.is_current(&Scene {
        on: &speakers,
        zones: &zones,
        master: 80.0,
        gains_db: GAINS,
    }));

    let quiet = Preset {
        id: "preset:2".to_owned(),
        name: "Same".to_owned(),
        ..preset.clone()
    };
    let list = [preset.clone(), quiet];
    let scene = Scene {
        on: &speakers,
        zones: &zones,
        master: 80.0,
        gains_db: GAINS,
    };
    assert_eq!(
        current(&list, &scene).map(|found| found.id.as_str()),
        Some("preset:1"),
        "two matches: the first in list order"
    );
    assert_eq!(current(&list[..0], &scene), None);
}

// covers: S254, S150
#[test]
fn preset_names_follow_the_group_name_rules() {
    let label = |n: usize| format!("Preset {n}");
    assert_eq!(
        new_preset_name("  Evening ", &["Preset 1"], label),
        "Evening"
    );
    assert_eq!(new_preset_name("", &[] as &[&str], label), "Preset 1");
    assert_eq!(new_preset_name(" ", &["Morning"], label), "Preset 2");
    assert_eq!(new_preset_name("", &["A", "Preset 3"], label), "Preset 4");
    assert_eq!(renamed_preset("Evening", "  "), "Evening");
    assert_eq!(renamed_preset("Evening", " Night "), "Night");
}

fn group<'a>(id: &'a str, name: &'a str, members: &'a [String]) -> SavedGroup<'a> {
    SavedGroup { id, name, members }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|key| (*key).to_owned()).collect()
}

fn volume(key: &str) -> Option<f64> {
    match key {
        "lc:desk" => Some(30.0),
        "ap:kitchen" => Some(55.0),
        _ => None,
    }
}

// covers: S254, S248
#[test]
fn an_overlapping_group_becomes_a_preset_with_its_full_members() {
    let first = strings(&["lc:desk", "ap:kitchen"]);
    let second = strings(&["ap:kitchen", "cc:den", "cc:attic", "lc:usb"]);
    let alone = strings(&["ap:office"]);
    let groups = [
        group("g1", "Downstairs", &first),
        group("g2", "Group 2", &second),
        group("g3", "Office", &alone),
        group("g4", "Empty", &[]),
        group("", "No zone", &[]),
    ];
    let zones = zones();
    let saved = Saved {
        zones: &zones,
        volume: &volume,
        master: 75.0,
        gains_db: GAINS,
    };
    let presets = presets_from_groups(&groups, &saved);
    assert_eq!(
        presets.len(),
        1,
        "only the group that lost a member, never an empty one"
    );
    let preset = &presets[0];
    assert_eq!(
        (preset.id.as_str(), preset.name.as_str()),
        ("g2", "Group 2")
    );
    assert_eq!(
        keys(preset),
        ["ap:kitchen", "cc:den", "lc:usb"],
        "group order, the first Cast key only"
    );
    assert_eq!(
        preset
            .members
            .iter()
            .map(|member| member.volume)
            .collect::<Vec<_>>(),
        [Some(55.0), None, None]
    );
    assert_eq!(
        preset.zones,
        [
            PresetZone {
                id: "z1".to_owned(),
                volume: 70.0
            },
            PresetZone {
                id: "z2".to_owned(),
                volume: 40.0
            },
        ]
    );
    assert_eq!((preset.master, preset.gains_db), (75.0, GAINS));
}

// covers: S254
#[test]
fn migrating_presets_twice_changes_nothing() {
    let first = strings(&["lc:desk"]);
    let second = strings(&["lc:desk", "ap:kitchen"]);
    let groups = [group("g1", "A", &first), group("g2", "B", &second)];
    let zones = zones();
    let saved = Saved {
        zones: &zones,
        volume: &volume,
        master: 100.0,
        gains_db: [0.0; 10],
    };
    let once = migrate(false, &groups, &saved).expect("not marked yet");
    assert_eq!(once.len(), 1);
    assert_eq!(
        migrate(false, &groups, &saved),
        Some(once.clone()),
        "cut short before the mark: the same result"
    );
    assert_eq!(migrate(true, &groups, &saved), None);
    assert_eq!(from_stored(once.clone()), once);
    assert_eq!(from_stored([once[0].clone(), once[0].clone()]).len(), 1);

    let mut renamed = once[0].clone();
    renamed.rename_key("ap:kitchen", "ap:native:AABBCCDDEE01");
    assert_eq!(keys(&renamed), ["lc:desk", "ap:native:AABBCCDDEE01"]);
    renamed.rename_key("lc:desk", "ap:native:AABBCCDDEE01");
    assert_eq!(keys(&renamed), ["ap:native:AABBCCDDEE01"]);
}
