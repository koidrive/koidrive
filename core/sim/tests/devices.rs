// SPDX-License-Identifier: Apache-2.0

//! Two or three devices share one reference server while their users edit, effects fail,
//! messages are lost, repeated or reordered, commits are refused and devices crash between store
//! transactions or in the middle of an effect (docs/design/sync-engine.md section 7.3).
//! `koidrive_sim::world` describes the harness and its two kinds of failure.
//!
//! `devices_lose_no_write` fails when a write is lost or when the engine, the store or the server
//! refuses a state. `devices_converge_once_the_faults_stop` also asks that every disk reaches the
//! server's tree and that no commit makes a change the user of its device never made.

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::restore::RestoreIntent;
use koidrive_engine::tree::Content;
use koidrive_proto::v1::error;
use koidrive_sim::world::disk::Place;
use koidrive_sim::world::{
    Action, Cut, Failure, NAMES, Outcome, PENDING_NAME, Reply, Step, TEXTS, World,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{RngSeed, TestRunner};

#[path = "devices/restore_write_history.rs"]
mod restore_write_history;

fn action() -> impl Strategy<Value = Action> {
    (
        prop_oneof![1 => Just(0u8), 3 => Just(1u8), 2 => Just(2u8), 2 => Just(3u8), 1 => Just(4u8)],
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        0..TEXTS,
    )
        .prop_map(|(kind, target, parent, name, text)| Action {
            kind,
            target,
            parent,
            name,
            text,
        })
}

#[derive(Clone, Copy)]
enum Losses {
    None,
    Restores,
    Rewinds,
}

fn step(losses: Losses) -> BoxedStrategy<Step> {
    let cut = prop_oneof![Just(Cut::Quarantined), Just(Cut::Placed), Just(Cut::Done)];
    let outcome = prop_oneof![
        5 => Just(Outcome::Run),
        1 => Just(Outcome::Fail),
        4 => cut.prop_map(Outcome::Crash),
    ];
    let refused = prop_oneof![
        Just(error::Code::RetryLater),
        Just(error::Code::LimitExceeded),
    ];
    let reply = prop_oneof![
        8 => Just(Reply::Served),
        1 => refused.prop_map(Reply::Refused),
        1 => Just(Reply::NoChunks),
    ];
    let steps = prop_oneof![
        8 => action().prop_map(Step::User),
        12 => any::<usize>().prop_map(Step::Work),
        2 => Just(Step::Scan),
        4 => outcome.prop_map(Step::Effect),
        4 => reply.prop_map(Step::Commit),
        2 => Just(Step::Connect),
        3 => any::<usize>().prop_map(Step::Deliver),
        1 => any::<usize>().prop_map(Step::Duplicate),
        1 => Just(Step::Drop),
        1 => Just(Step::Crash),
    ]
    .boxed();
    match losses {
        Losses::None => steps,
        Losses::Restores => prop_oneof![
            19 => steps,
            1 => prop_oneof![Just(Step::Backup), Just(Step::Restore)],
        ]
        .boxed(),
        Losses::Rewinds => prop_oneof![
            60 => steps,
            2 => Just(Step::Backup),
            1 => Just(Step::Rewind),
            3 => Just(Step::CopyStore),
            2 => Just(Step::RollBack),
            4 => (0..4usize).prop_map(Step::Answer),
        ]
        .boxed(),
    }
}

fn phases(losses: Losses) -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    prop::collection::vec(
        prop::collection::vec((0..3usize, step(losses)), 0..48),
        1..=3,
    )
}

fn replaced_folder() -> impl Strategy<Value = (usize, Vec<Vec<(usize, Step)>>)> {
    let name = || prop::sample::select(NAMES);
    (
        2..=3usize,
        (name(), name(), name()),
        0..4usize,
        phases(Losses::None),
        prop::collection::vec(any::<prop::sample::Index>(), 4),
    )
        .prop_map(
            |(devices, (renamed, made, inside), ids_minted_before, mut phases, at)| {
                let race = [
                    vec![(0, rename(0, 0, renamed)), (0, SCAN), (0, COMMIT)],
                    vec![
                        (1, delete(0)),
                        (1, folder(made)),
                        (1, user(1, 0, 1, inside)),
                        (1, SCAN),
                        (1, COMMIT),
                    ],
                    vec![(0, COMMIT)],
                    vec![(1, COMMIT)],
                ];
                let first = &mut phases[0];
                let mut places: Vec<usize> =
                    at.iter().map(|at| at.index(first.len() + 1)).collect();
                places.sort_unstable();
                for (place, block) in places.into_iter().zip(race).rev() {
                    first.splice(place..place, block);
                }
                let mut all = vec![vec![(0, folder("b"))]];
                all.extend(ids_minted_by_device_1(ids_minted_before, 1));
                all.extend(phases);
                (devices, all)
            },
        )
}

fn owing_a_rename(losses: Losses) -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    let change = prop_oneof![
        Just(delete(1)),
        Just(rename(1, 0, "b")),
        Just(rename(1, 0, "A"))
    ];
    (phases(losses), any::<prop::sample::Index>(), change).prop_map(|(mut phases, at, change)| {
        let first = &mut phases[0];
        first.insert(at.index(first.len() + 1), (1, change));
        first.splice(0..0, owing());
        phases.insert(0, vec![(0, file("a"))]);
        phases
    })
}

fn owing() -> Vec<(usize, Step)> {
    let mut owing = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    owing.extend([(0, Step::Work(0)); 5]);
    owing.extend([(1, Step::Work(0)); 8]);
    owing
}

fn reviving_an_owed_rename() -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    let owed_reply = prop_oneof![
        Just(Reply::Served),
        Just(Reply::Refused(error::Code::LimitExceeded)),
        Just(Reply::Refused(error::Code::RetryLater)),
    ];
    let revival = prop_oneof![Just(user(1, 0, 1, "b")), Just(user(0, 0, 1, "b"))];
    (owing_a_rename(Losses::None), owed_reply, revival).prop_map(
        |(mut phases, owed_reply, revival)| {
            let mut block = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
            block.extend([(0, Step::Work(0)); 5]);
            block.extend([(1, Step::Work(0)); 5]);
            block.push((0, Step::Commit(owed_reply)));
            block.push((1, delete(1)));
            block.extend(commit(1));
            block.push((0, revival));
            block.extend(commit(0));
            phases[1].splice(0..owing().len(), block);
            phases
        },
    )
}

/// Devices that print every step with the state it leaves when `KOIDRIVE_SIM_TRACE` is set.
fn world(devices: usize) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    world
}

/// Runs every phase and stops at the first failure that `stops` names. A phase that fails in
/// another way leaves the devices as they are for the next one.
fn run(
    devices: usize,
    phases: &[Vec<(usize, Step)>],
    stops: fn(&Failure) -> bool,
) -> Result<World, Failure> {
    let mut world = world(devices);
    for (phase, schedule) in phases.iter().enumerate() {
        match world.phase(phase, schedule) {
            Err(failure) if stops(&failure) => return Err(failure),
            _ => {}
        }
    }
    Ok(world)
}

/// Stops at the first failure of any kind.
fn converge(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    run(devices, phases, |_| true)
}

/// Stops only when a write is lost or a state is refused.
fn keep(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    run(devices, phases, |failure| {
        matches!(failure, Failure::Broken(_))
    })
}

const REWIND_SEED: u64 = 501;

macro_rules! test_name {
    ($test:ident) => {
        concat!(module_path!(), "::", stringify!($test))
    };
}

fn converges_from(test: &'static str, seed: u64, cases: u32) {
    converges_with(ProptestConfig {
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        test_name: Some(test),
        ..ProptestConfig::with_cases(cases)
    });
}

fn converges_from_saved_cases(test: &'static str, file: &'static str) {
    converges_with(ProptestConfig {
        test_name: Some(test),
        ..koidrive_test_support::regression_config(file, 0)
    });
}

fn converges_with(config: ProptestConfig) {
    let mut runner = TestRunner::new(config);
    let result = runner.run(
        &(2..=3usize, phases(Losses::Rewinds)),
        |(devices, phases)| {
            converge(devices, &phases)
                .map(drop)
                .map_err(|failure| TestCaseError::fail(failure.to_string()))
        },
    );
    if let Err(error) = result {
        panic!("{error}");
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/devices.proptest-regressions"),
        256,
    ))]

    #[test]
    fn devices_lose_no_write(devices in 2..=3usize, phases in phases(Losses::None)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_once_the_faults_stop(devices in 2..=3usize, phases in phases(Losses::None)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_around_a_folder_another_device_replaces((devices, phases) in replaced_folder()) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_around_an_owed_rename(devices in 2..=3usize, phases in owing_a_rename(Losses::None)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_when_a_folder_owing_its_name_is_deleted_and_brought_back(phases in reviving_an_owed_rename()) {
        converge(3, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    #[ignore = "after a server restore, a device sends its lost creates again under a folder another device deleted (GUN-700)"]
    fn devices_lose_no_write_around_an_owed_rename_across_server_restores(devices in 2..=3usize, phases in owing_a_rename(Losses::Restores)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_lose_no_write_across_server_restores(devices in 2..=3usize, phases in phases(Losses::Restores)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_across_server_restores(devices in 2..=3usize, phases in phases(Losses::Restores)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(REWIND_SEED),
        ..koidrive_test_support::regression_config(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/devices.proptest-regressions"),
            256,
        )
    })]

    #[test]
    fn devices_lose_no_write_across_rewinds_and_rollbacks(devices in 2..=3usize, phases in phases(Losses::Rewinds)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_across_rewinds_and_rollbacks(devices in 2..=3usize, phases in phases(Losses::Rewinds)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

#[test]
fn rewinds_converge_from_seed_122() {
    converges_from(test_name!(rewinds_converge_from_seed_122), 122, 4096);
}

#[test]
#[ignore = "a store rollback republishes a download at a name the server has since moved it from (GUN-557)"]
fn rewinds_converge_from_seed_605() {
    converges_from(test_name!(rewinds_converge_from_seed_605), 605, 4096);
}

#[test]
fn rewinds_converge_from_seed_301() {
    converges_from(test_name!(rewinds_converge_from_seed_301), 301, 4096);
}

#[test]
fn rewinds_converge_from_seed_313() {
    converges_from(test_name!(rewinds_converge_from_seed_313), 313, 4096);
}

#[test]
fn rewinds_converge_from_seed_319() {
    converges_from(test_name!(rewinds_converge_from_seed_319), 319, 4096);
}

#[test]
#[ignore = "a device goes quiet out of sync after a rewind (GUN-551)"]
fn rewinds_converge_from_seed_824() {
    converges_from(test_name!(rewinds_converge_from_seed_824), 824, 4096);
}

#[test]
#[ignore = "a device goes quiet out of sync after a rewind (GUN-551)"]
fn rewinds_converge_from_seed_1316() {
    converges_from(test_name!(rewinds_converge_from_seed_1316), 1316, 4096);
}

#[test]
#[ignore = "a device goes quiet out of sync after a rewind (GUN-551)"]
fn rewinds_converge_from_seed_3413() {
    converges_from(test_name!(rewinds_converge_from_seed_3413), 3413, 4096);
}

#[test]
#[ignore = "a device goes quiet out of sync after a rewind (GUN-551)"]
fn rewinds_converge_from_seed_3417() {
    converges_from(test_name!(rewinds_converge_from_seed_3417), 3417, 4096);
}

#[test]
#[ignore = "a device goes quiet out of sync after a rewind, or keeps a write off the server while it waits (GUN-551)"]
fn rewinds_converge_from_the_saved_gun_551_cases() {
    converges_from_saved_cases(
        test_name!(rewinds_converge_from_the_saved_gun_551_cases),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/gun-551.proptest-regressions"
        ),
    );
}

fn two_rewinds_around_a_rolled_back_store() -> Vec<Vec<(usize, Step)>> {
    let act = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    vec![
        vec![
            (0, Step::Backup),
            (2, Step::CopyStore),
            (2, act(0, 0, 0, 0)),
            (1, act(0, 0, 0, 2)),
            (0, Step::Rewind),
        ],
        vec![
            (1, act(0, 0, 0, 0)),
            (0, act(3, 0, 3423204570637552068, 0)),
            (0, Step::Work(0)),
            (0, Step::Backup),
            (2, Step::RollBack),
            (0, Step::Work(0)),
            (1, act(2, 0, 0, 0)),
            (0, Step::Commit(Reply::Served)),
        ],
        vec![
            (0, Step::Rewind),
            (2, act(1, 0, 18832373481938867, 0)),
            (2, act(3, 5290765141707611353, 8942541845889857281, 0)),
            (0, act(0, 0, 0, 3)),
            (0, act(4, 1966063831828710265, 0, 0)),
        ],
    ]
}

#[test]
fn two_rewinds_around_a_rolled_back_store_lose_no_write() {
    keep(3, &two_rewinds_around_a_rolled_back_store()).unwrap();
}

#[test]
#[ignore = "the devices are not quiet after two rewinds around a rolled-back store (GUN-551)"]
fn two_rewinds_around_a_rolled_back_store_converge() {
    converge(3, &two_rewinds_around_a_rolled_back_store()).unwrap();
}

/// A fixed sample of the schedules reaches every fault and every way back from one that the
/// harness counts, so a change to the strategies cannot drop one without this test failing.
#[test]
fn the_schedules_reach_every_fault() {
    reaches(phases(Losses::None), &EVENTS);
}

#[test]
fn the_owed_rename_schedules_reach_both_answers() {
    reaches(
        owing_a_rename(Losses::None),
        &["move with from applied", "move with from superseded"],
    );
}

#[test]
fn the_revival_schedules_bring_back_a_folder_at_its_pending_name() {
    reaches(
        reviving_an_owed_rename(),
        &["a node at a pending name comes back from the trash"],
    );
}

#[test]
fn the_rewind_schedules_reach_every_recovery() {
    reaches(
        phases(Losses::Rewinds),
        &[
            "server rewound",
            "rewind takes a write a disk holds",
            "rewind takes a write only one disk holds",
            "store rolled back",
            "recovery starts after a rewind",
            "recovery starts after a store rollback",
            "crash during a recovery",
            "store rolled back during a recovery",
            "recovery classifies the trees",
            "recovery answer publishes",
            "recovery answer keeps",
            "recovery answer discards",
            "recovery answer deletes",
            "recovery answer restores",
        ],
    );
}

fn reaches(phases: impl Strategy<Value = Vec<Vec<(usize, Step)>>>, events: &[&str]) {
    let mut runner = TestRunner::deterministic();
    let strategy = (2..=3usize, phases);
    let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
    for _ in 0..256 {
        let (devices, phases) = strategy.new_tree(&mut runner).unwrap().current();
        let world = keep(devices, &phases).unwrap();
        for (what, count) in world.seen {
            *seen.entry(what).or_default() += count;
        }
    }
    for (what, count) in &seen {
        println!("{count:>8} {what}");
    }
    for what in events {
        assert!(
            seen.contains_key(what),
            "no schedule of the sample reached: {what}"
        );
    }
}

/// The events of `World::seen` that the sample must reach.
const EVENTS: [&str; 29] = [
    "effect failed before the disk",
    "a batch of effects ends in one transaction",
    "effect refused by the disk",
    "effect parked a node",
    "crash before an effect reached the disk",
    "crash with a download staged",
    "crash with a displaced entry in quarantine",
    "crash after the disk refused an effect",
    "crash after an effect reached the disk",
    "restart finds the effect in flight done",
    "restart finds the effect in flight not done",
    "restart ends an effect that left an entry in quarantine",
    "restart with a commit in flight",
    "reconnect resends a commit the server applied",
    "reconnect resends a commit the server never got",
    "own commit replayed before its result",
    "commit answered from the stored result",
    "result arrives after its records",
    "result arrives before earlier commits",
    "records arrive past a gap",
    "records arrive twice",
    "answer arrives twice",
    "commit refused, sent again",
    "commit of one op refused for its size, its node is a problem",
    "no commit built while a node is a problem",
    "rescan lets a problem node be planned again",
    "commit abandoned, its bytes are gone",
    "op renamed by the server",
    "op merged a folder",
];

fn user(kind: u8, target: usize, parent: usize, name: &str) -> Step {
    act(kind, target, parent, name, 0)
}

fn act(kind: u8, target: usize, parent: usize, name: &str, text: u8) -> Step {
    let name = match name {
        PENDING_NAME => NAMES.len(),
        name => NAMES.iter().position(|other| *other == name).unwrap(),
    };
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text,
    })
}

/// Creates a folder in the root.
fn folder(name: &str) -> Step {
    user(0, 0, 0, name)
}

/// Creates a file in the root.
fn file(name: &str) -> Step {
    user(1, 0, 0, name)
}

/// Overwrites the `target`th entry of the disk, in the order the entries were made.
fn overwrite(target: usize) -> Step {
    user(2, target, 0, "a")
}

/// Moves the `target`th entry into the root, or into the `parent`th folder counted from 1.
fn rename(target: usize, parent: usize, name: &str) -> Step {
    user(3, target, parent, name)
}

fn delete(target: usize) -> Step {
    user(4, target, 0, "a")
}

const SCAN: Step = Step::Scan;
const EFFECT: Step = Step::Effect(Outcome::Run);
/// The effect in flight reaches the disk and the device crashes before the store hears of it.
const CRASH: Step = Step::Effect(Outcome::Crash(Cut::Done));
const COMMIT: Step = Step::Commit(Reply::Served);
const DELIVER: Step = Step::Deliver(0);

/// One device's steps from a rescan to the answer of its commit.
fn commit(device: usize) -> Vec<(usize, Step)> {
    vec![
        (device, SCAN),
        (device, COMMIT),
        (device, COMMIT),
        (device, DELIVER),
    ]
}

#[test]
fn two_devices_take_each_others_files() {
    let world = converge(2, &[vec![(0, file("a")), (1, file("b"))]]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

#[test]
fn a_folder_named_in_nfd_keeps_its_spelling_on_disk() {
    let world = converge(2, &[vec![(0, folder("E\u{301}"))]]).unwrap();
    let names = |d: usize| -> Vec<String> {
        let entries = world.devices[d].disk.entries();
        entries.values().map(|entry| entry.name.clone()).collect()
    };
    assert_eq!(names(0), ["E\u{301}"]);
    assert_eq!(names(1), ["\u{c9}"]);
}

#[test]
fn folders_named_in_nfc_and_nfd_on_two_devices_converge() {
    converge(2, &[vec![(0, folder("\u{c9}")), (1, folder("E\u{301}"))]]).unwrap();
}

#[test]
fn a_file_named_in_nfd_and_changed_elsewhere_keeps_its_spelling() {
    let mut overwritten = vec![(1, overwrite(0))];
    overwritten.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    overwritten.extend(std::iter::repeat_n((0, Step::Work(0)), 3));
    let world = converge(2, &[vec![(0, file("E\u{301}"))], overwritten]).unwrap();
    let names: Vec<&str> = (world.devices[0].disk.entries().values())
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["E\u{301}"]);
}

#[test]
fn a_file_changed_elsewhere_beside_a_folder_with_its_name_in_nfc_converges() {
    let mut owing = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    owing.extend(std::iter::repeat_n((0, Step::Work(0)), 5));
    owing.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    owing.extend([
        (1, rename(1, 0, "b")),
        (0, Step::Work(0)),
        (
            2,
            user(3, 1322304017492565313, 10669873100036100452, "\u{c9}"),
        ),
        (1, user(2, 127064483901184320, 0, "a")),
        (
            0,
            user(3, 15763447294144001574, 2286593307203906816, "E\u{301}"),
        ),
    ]);
    converge(2, &[vec![(0, file("a"))], owing]).unwrap();
}

/// The bytes of the two files are the same, and both files stay (the planted digest
/// deduplication of scripts/engine-mutations.sh removes one).
#[test]
fn two_new_files_with_the_same_bytes_are_both_kept() {
    let world = converge(2, &[vec![(0, file("a")), (0, file("b"))]]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

#[test]
fn two_overwrites_of_one_file_end_as_two_files() {
    let synced = vec![(0, file("a"))];
    let racing = vec![(0, overwrite(0)), (1, overwrite(0))];
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// Device 1 overwrites a file, and device 0 then writes the first bytes back. Device 2 deletes
/// the file while its disk still has the first write. The engine tells contents apart by their
/// bytes, so the delete applies to the last write.
#[test]
fn a_delete_of_a_file_that_got_its_bytes_back_is_applied() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(1, overwrite(0))];
    racing.extend(commit(1));
    racing.extend([(0, DELIVER), (0, EFFECT), (0, EFFECT), (0, overwrite(0))]);
    racing.extend(commit(0));
    racing.push((2, delete(0)));
    let world = converge(3, &[synced, racing]).unwrap();
    assert!(world.devices[0].disk.entries().is_empty());
}

/// The answer to a commit is lost, and the device reads its own commit in the backlog before the
/// stored result arrives.
#[test]
fn a_commit_replayed_before_its_result_is_applied_once() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Drop),
        (0, Step::Connect),
        (0, DELIVER),
        (0, EFFECT),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.seen["commit answered from the stored result"], 1);
    assert_eq!(world.devices[0].disk.entries().len(), 1);
}

/// Both devices create the folder "a" with a file in it. Device 1's folder is merged into device
/// 0's, the answer is lost, and device 1 reads its own commit in the backlog first.
#[test]
fn a_merged_folder_replayed_before_its_result_is_kept_once() {
    let mut schedule = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, folder("a")),
        (1, user(1, 0, 1, "c.txt")),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, Step::Drop),
        (1, Step::Connect),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["op merged a folder"], 1);
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 3);
}

/// GUN-378. Device 0 renames a to b, and the answer to its commit is lost. Device 1 then renames
/// the file to c.txt, and so does the user of device 0, who reads both commits in the backlog
/// before the stored result arrives and then renames the file back to b. Device 1 commits a
/// rename to ss after device 0 acknowledged its commit. Device 0's rename back to b is then
/// committed after ss, so it wins on every device instead of being undone.
#[test]
fn a_rename_made_while_a_replayed_commit_waits_for_its_result_is_kept() {
    let mut synced = vec![(0, file("a"))];
    synced.extend(commit(0));
    let mut racing = vec![
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Drop),
        (1, rename(0, 0, "c.txt")),
    ];
    racing.extend(commit(1));
    racing.extend([
        (1, DELIVER),
        (0, rename(0, 0, "c.txt")),
        (0, Step::Connect),
        (0, DELIVER),
        (0, SCAN),
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, DELIVER),
        (1, rename(0, 0, "ss")),
    ]);
    racing.extend(commit(1));
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.seen["result arrives after its records"], 1);
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"]);
    }
}

/// A download reaches the disk and the device crashes before the store hears of it.
#[test]
fn a_download_cut_by_a_crash_is_not_done_twice() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, CRASH)]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["restart finds the effect in flight done"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// As above, and the user renames the downloaded file while the device is down. The restart finds
/// no file with the download's bytes at its name and reports the download as not done (section
/// 5.3). The engine takes the renamed file for a new one and downloads the file again.
#[test]
fn a_file_renamed_while_the_device_was_down_after_its_download_is_downloaded_again() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, CRASH),
        (1, rename(0, 0, "b")),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["restart finds the effect in flight not done"], 1);
    let names = |d: usize| -> Vec<&str> {
        let entries = world.devices[d].disk.entries().values();
        entries.map(|entry| entry.name.as_str()).collect()
    };
    assert_eq!(names(0), ["a", "b"]);
    assert_eq!(names(1), ["b", "a"]);
}

/// As above, with an overwrite instead of a rename. The engine takes the overwritten file for a new
/// one, so the user's bytes are uploaded beside the file that the download brings again.
#[test]
fn a_file_overwritten_while_the_device_was_down_after_its_download_is_kept_beside_it() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, CRASH), (1, overwrite(0))]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["restart finds a write its user overtook"], 1);
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

fn a_download_a_crash_hid_and_its_user_changed(cut: Cut, change: Step) -> World {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(2, overwrite(0))];
    racing.extend(commit(2));
    racing.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, Step::Effect(Outcome::Crash(cut))),
        (1, change),
        (0, DELIVER),
        (0, EFFECT),
        (0, EFFECT),
        (0, overwrite(0)),
    ]);
    racing.extend(commit(0));
    converge(3, &[synced, racing]).unwrap()
}

#[test]
fn a_download_a_crash_hid_and_its_user_deleted_keeps_an_edit_back_to_the_bytes_it_replaced() {
    for cut in [Cut::Placed, Cut::Done] {
        let world = a_download_a_crash_hid_and_its_user_changed(cut, delete(0));
        assert_eq!(world.seen["restart finds a write its user overtook"], 1);
        for device in &world.devices {
            assert_eq!(device.disk.entries().len(), 1);
        }
    }
}

#[test]
fn a_file_made_at_the_name_a_crashed_download_freed_keeps_an_edit_back_to_the_bytes_it_replaced() {
    let world = a_download_a_crash_hid_and_its_user_changed(Cut::Quarantined, file("a"));
    assert_eq!(world.seen["restart finds a write its user overtook"], 1);
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 2);
    }
}

#[test]
fn a_download_a_crash_hid_and_its_user_overwrote_keeps_an_edit_back_to_the_bytes_it_replaced() {
    for cut in [Cut::Placed, Cut::Done] {
        let world = a_download_a_crash_hid_and_its_user_changed(cut, overwrite(0));
        assert_eq!(world.seen["restart finds a write its user overtook"], 1);
        for device in &world.devices {
            assert_eq!(device.disk.entries().len(), 2);
        }
    }
}

/// A download that replaces a file stops when the old file is in quarantine and the new one has
/// no name yet. The restart gives the download its name and sends the old file to the local
/// trash.
#[test]
fn a_replacement_cut_between_quarantine_and_placement_is_ended_by_the_restart() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    let cut = Step::Effect(Outcome::Crash(Cut::Quarantined));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, cut)]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["crash with a displaced entry in quarantine"], 1);
    assert_eq!(
        world.seen["restart ends an effect that left an entry in quarantine"],
        1
    );
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

#[test]
fn a_crash_after_a_batch_of_downloads_reached_the_disk_is_ended_by_the_restart() {
    let mut racing = vec![(0, file("a")), (0, file("b")), (0, file("c.txt"))];
    racing.extend(commit(0));
    racing.extend([(1, DELIVER), (1, CRASH)]);
    let world = converge(2, &[racing]).unwrap();
    assert_eq!(
        world.seen["crash after part of a batch reached the disk"],
        1
    );
    assert_eq!(world.seen["restart ends a batch of effects"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 3);
}

#[test]
fn a_crash_in_the_middle_of_a_batch_of_replacements_is_ended_by_the_restart() {
    let synced = vec![(0, file("a")), (0, file("b")), (0, file("c.txt"))];
    let mut racing = vec![(0, overwrite(0)), (0, overwrite(1)), (0, overwrite(2))];
    racing.extend(commit(0));
    let cut = Step::Effect(Outcome::Crash(Cut::Quarantined));
    racing.extend([(1, DELIVER), (1, cut)]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(
        world.seen["crash after part of a batch reached the disk"],
        1
    );
    assert_eq!(
        world.seen["restart ends an effect that left an entry in quarantine"],
        1
    );
    assert_eq!(world.devices[1].disk.entries().len(), 3);
}

/// As above, and the user makes a file at the free name while the device is down. The download
/// finds its name taken, and both the user's file and the download are kept.
#[test]
fn a_file_made_at_the_name_of_a_quarantined_file_is_kept() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    let cut = Step::Effect(Outcome::Crash(Cut::Quarantined));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, cut), (1, file("a"))]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["restart finds a write its user overtook"], 1);
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// The server refuses a commit of one op with `LIMIT_EXCEEDED`. The engine marks the node as a
/// problem and builds no commit for it until the next rescan.
#[test]
fn a_commit_of_one_op_refused_for_its_size_waits_for_a_rescan() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
        (0, DELIVER),
        (0, COMMIT),
        (0, COMMIT),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(
        world.seen["commit of one op refused for its size, its node is a problem"],
        1
    );
    assert_eq!(world.seen["no commit built while a node is a problem"], 2);
    assert_eq!(world.seen["rescan lets a problem node be planned again"], 1);
    assert_eq!(world.seen["commit applied"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// A download fails before it touches the disk, and a commit is refused before the server applies
/// it. Both are done again.
#[test]
fn a_failed_download_and_a_refused_commit_are_done_again() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, Step::Commit(Reply::Refused(error::Code::RetryLater))),
        (0, DELIVER),
        (0, COMMIT),
        (0, DELIVER),
        (1, DELIVER),
        (1, Step::Effect(Outcome::Fail)),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["commit refused, sent again"], 1);
    assert_eq!(world.seen["effect failed before the disk"], 1);
}

/// Both devices overwrite one file. Device 1's commit makes a conflict copy and its answer is lost.
/// After reconnecting, device 1 replays its own commit from the log and downloads the copy before
/// the stored result arrives.
///
/// Found as GUN-326.
#[test]
fn a_conflict_copy_replayed_before_its_result_is_kept_once() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0)), (1, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, Step::Drop),
        (1, Step::Connect),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 1 starts to download a new file. Before the download ends, device 0 renames the file
/// and device 1 hears of it.
///
/// Found as GUN-327.
#[test]
fn a_rename_that_arrives_during_the_download_is_not_undone() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (0, rename(0, 0, "b"))]);
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT)]);
    converge(2, &[schedule]).unwrap();
}

/// On a disk that tells "A" from "a", device 0 edits the file "A" and creates the folder "a" while
/// device 1 edits and renames the file. Device 0's edit becomes a conflict copy.
///
/// Found as GUN-328.
#[test]
fn a_conflict_copy_whose_old_name_a_new_folder_took_leaves_waiting() {
    let synced = vec![(0, file("A"))];
    let racing = vec![
        (0, overwrite(0)),
        (0, folder("a")),
        (1, overwrite(0)),
        (1, rename(0, 0, "b")),
        (0, SCAN),
        (1, SCAN),
        (1, COMMIT),
        (0, COMMIT),
        (1, COMMIT),
    ];
    converge(2, &[synced, racing]).unwrap();
}

/// Device 0 creates the file "a" where device 1 has created a folder, so the server renames the
/// file. The user of device 0 deletes the file before the disk has the server's name for it.
///
/// Found as GUN-332.
#[test]
fn a_file_deleted_before_the_disk_took_the_servers_name_stays_deleted() {
    let mut schedule = vec![(1, folder("a")), (0, file("a"))];
    schedule.extend(commit(1));
    schedule.extend(commit(0));
    schedule.extend([(0, DELIVER), (0, delete(0)), (0, SCAN), (0, COMMIT)]);
    converge(2, &[schedule]).unwrap();
}

/// Device 1 edits a file and deletes an empty folder. Meanwhile device 0 has moved the file into
/// that folder and edited it, so device 1's edit becomes a conflict copy inside the folder that the
/// same commit deletes.
#[test]
fn an_edit_that_becomes_a_conflict_copy_in_a_folder_its_commit_deletes_is_kept() {
    let synced = vec![(0, file("b")), (0, folder("a"))];
    let mut racing = vec![
        (1, overwrite(0)),
        (1, delete(1)),
        (0, rename(0, 1, "c.txt")),
        (0, overwrite(0)),
    ];
    racing.extend(commit(0));
    racing.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 1 deletes the empty folder "b". Meanwhile device 0 has moved the folder "a", which holds
/// a file, into "b".
#[test]
fn a_file_in_a_folder_moved_into_a_folder_that_another_device_deletes_is_kept() {
    let synced = vec![
        (0, folder("a")),
        (0, user(1, 0, 1, "c.txt")),
        (0, folder("b")),
    ];
    let mut racing = vec![(1, delete(1)), (0, rename(0, 2, "a"))];
    racing.extend(commit(0));
    racing.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 2 deletes a folder that holds a file, and device 1 renames the folder, which brings both
/// back on the server. Device 0 has taken the delete and is removing the file from its disk when
/// the rename arrives.
///
/// Found as GUN-334.
#[test]
fn a_file_that_an_effect_removes_after_the_server_restored_it_is_not_deleted() {
    let synced = vec![(0, folder("b")), (0, user(1, 0, 1, "a"))];
    let mut racing = vec![(2, delete(0)), (1, rename(0, 0, "c.txt"))];
    racing.extend(commit(2));
    racing.extend([(0, DELIVER), (0, EFFECT)]);
    racing.extend(commit(1));
    racing.extend([(0, DELIVER), (0, EFFECT)]);
    converge(3, &[synced, racing]).unwrap();
}

/// Device 1 plans the download of the file "a". Its user then creates the folder "a", and moves it
/// away after the engine has seen it there. The download comes up before the next scan, while L
/// still holds the folder at that name.
///
/// Found as GUN-330.
#[test]
fn a_download_onto_a_name_that_a_moved_folder_held_leaves_a_loadable_store() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, folder("a")),
        (1, SCAN),
        (1, rename(0, 0, "b")),
        (1, EFFECT),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Another device deletes the folder "a". While its removal is in flight, the user moves a file
/// into the folder, a rescan sees it there, and the user moves it out again. The removal comes up
/// before the next scan, while L still holds the file in the folder.
///
/// Found as GUN-330.
#[test]
fn a_removal_of_a_folder_that_a_file_left_unseen_leaves_a_store_that_loads() {
    let mut schedule = vec![(0, folder("a")), (0, file("b"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    schedule.push((0, delete(0)));
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, rename(1, 1, "b")),
        (1, SCAN),
        (1, rename(1, 0, "b")),
        (1, EFFECT),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Another device deletes the folder "a". While its removal is in flight, the user renames the
/// folder and makes a folder in it, a rescan sees both, and the device stops. The restart finds
/// nothing at the name "a", which recovery reports as the removal done (section 5.3), while L
/// holds the folder under its new name.
///
/// Found as GUN-339.
#[test]
fn a_restart_during_the_removal_of_a_renamed_folder_leaves_a_store_that_loads() {
    let mut schedule = vec![(0, folder("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT), (0, delete(0))]);
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, rename(0, 0, "b")),
        (1, user(0, 0, 1, "a")),
        (1, SCAN),
        (1, Step::Crash),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Device 1 plans the download of the file "a" and stops. Its user renames the file "b", which
/// has the same bytes, to "a". The restart finds those bytes at the name "a", which recovery
/// reports as the download done (section 5.3), in the entry that L holds as "b".
///
/// Found as GUN-342.
#[test]
fn a_file_renamed_onto_the_name_of_a_download_in_flight_is_kept() {
    let mut schedule = vec![(0, file("b"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT), (0, file("a"))]);
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, Step::Crash),
        (1, rename(0, 0, "a")),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
}

/// Device 0 swaps two files through a third name. Its commit moves one of them through a pending
/// name, which the user never gave, and both devices end with the two files at the swapped names.
#[test]
fn two_files_swapped_through_a_third_name_reach_the_other_device() {
    let synced = vec![(0, file("a")), (0, file("b"))];
    let swap = vec![
        (0, rename(0, 0, "c.txt")),
        (0, rename(1, 0, "a")),
        (0, rename(0, 0, "b")),
    ];
    let world = converge(2, &[synced, swap]).unwrap();
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

fn a_pending_folder() -> World {
    let mut world = converge(2, &[vec![(0, Step::CopyStore), (0, file("ss"))]]).unwrap();
    world
        .run(
            1,
            &[
                (0, folder("ß")),
                (0, rename(0, 1, "a")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
            ],
        )
        .unwrap();
    assert!(
        world
            .space()
            .nodes()
            .iter()
            .any(|node| node.name == "ß (pending)")
    );
    world
}

#[test]
fn a_pending_folder_is_renamed_after_its_author_recovers_from_a_store_rollback() {
    let mut world = a_pending_folder();
    world.phase(2, &[(0, Step::RollBack)]).unwrap();
    assert!(world.devices[0].recovery.is_some());
    assert!(world.devices[1].recovery.is_none());
    assert!(world.space().nodes().iter().any(|node| node.name == "ß"));
    assert!(
        !world
            .space()
            .nodes()
            .iter()
            .any(|node| node.name == "ß (pending)")
    );
}

#[test]
fn a_pending_folder_is_renamed_after_its_author_recovers_from_a_rewind() {
    let mut world = a_pending_folder();
    world.phase(2, &[(0, BACKUP), (0, Step::Rewind)]).unwrap();
    assert!(world.devices.iter().all(|device| device.recovery.is_some()));
    assert!(world.space().nodes().iter().any(|node| node.name == "ß"));
    assert!(
        !world
            .space()
            .nodes()
            .iter()
            .any(|node| node.name == "ß (pending)")
    );
}

#[test]
fn a_pending_folder_is_renamed_while_another_device_recovers() {
    let mut world = a_pending_folder();
    world
        .phase(
            2,
            &[
                (1, Step::CopyStore),
                (1, file("b")),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, Step::RollBack),
            ],
        )
        .unwrap();
    assert!(world.devices[1].recovery.is_some());
    assert!(world.devices[0].recovery.is_none());
    assert!(world.space().nodes().iter().any(|node| node.name == "ß"));
    assert!(
        !world
            .space()
            .nodes()
            .iter()
            .any(|node| node.name == "ß (pending)")
    );
}

/// Device 2 sends its rename of a file to b, and while the commit is in flight it receives device
/// 0's rename to b and then device 1's rename to c.txt. The server applies device 2's commit after
/// device 1's, so the file ends as b. The driver plans no effect while a commit is in flight, so
/// device 2 never moves its file to c.txt and then commits that name as its own. The minimal input
/// proptest found for `devices_converge_once_the_faults_stop` (GUN-383).
#[test]
fn a_device_with_a_commit_in_flight_does_not_commit_a_remote_name_back() {
    let phases = [
        vec![(0, file("a")), (0, rename(0, 0, "ß"))],
        vec![
            (2, rename(0, 0, "b")),
            (0, rename(0, 0, "b")),
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Work(913562)),
            (1, COMMIT),
            (1, Step::Work(6613876784364572377)),
            (
                1,
                rename(7269053276869567780, 12266595847907186486, "c.txt"),
            ),
            (1, Step::Work(2973521201126422277)),
            (1, Step::Work(10427516227233112141)),
        ],
    ];
    converge(3, &phases).unwrap();
}

/// Device 2 makes folder A next to the synced folder a, commits it, and moves A into a while
/// device 0 deletes a. A's acknowledged S update waits, since a still holds the name key in S on
/// device 2, and a cannot leave the disk while it holds A. Device 2 sends the move against the
/// waiting update, so the server brings a back with A in it. The minimal input proptest found for
/// `devices_converge_once_the_faults_stop` (GUN-399).
#[test]
fn a_folder_moved_into_a_deleted_folder_while_its_create_waits_is_sent() {
    let phases = [
        vec![(0, folder("a"))],
        vec![
            (2, folder("A")),
            (2, Step::Work(0)),
            (2, COMMIT),
            (2, rename(1, 1, "a")),
            (0, delete(0)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    let entries = world.devices[0].disk.entries();
    assert_eq!(entries.len(), 2);
    let moved = (entries.values())
        .find(|entry| matches!(entry.place, Place::In(_)))
        .expect("A is inside a");
    let Place::In(folder) = moved.place else {
        unreachable!()
    };
    assert_eq!(moved.name, "a");
    assert_eq!(entries[&folder].place, Place::Root);
}

#[test]
fn a_folder_revived_at_its_pending_name_after_its_delete_landed_takes_its_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, Step::Work(4007745936436893426)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 13283880982325300966,
                    parent: 7530012053189467691,
                    name: 6,
                    text: 3,
                }),
            ),
            (2, Step::Work(18389991862780469496)),
            (2, Step::Work(9775417661590067865)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 8851446434072450680,
                    parent: 2745539858988442782,
                    name: 0,
                    text: 2,
                }),
            ),
            (0, Step::Work(4450296097999021389)),
            (2, Step::Deliver(15501889018198306176)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 4254539288740600970,
                    parent: 17259486623974888835,
                    name: 0,
                    text: 3,
                }),
            ),
            (0, Step::Deliver(7951671155622776781)),
            (0, Step::Duplicate(16868798682183253529)),
            (0, Step::Commit(Reply::Served)),
        ],
        vec![
            (1, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 4696540523973067749,
                    parent: 17289433333287293583,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Connect),
            (0, Step::Scan),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 17156652917260369542,
                    parent: 15791111473684421551,
                    name: 3,
                    text: 2,
                }),
            ),
            (1, Step::Work(7157279081864785743)),
            (1, Step::Duplicate(18363909320702985799)),
            (2, Step::Deliver(3717140028059626527)),
            (1, Step::Commit(Reply::Served)),
            (2, Step::Deliver(12029961515716932305)),
            (2, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 13882752570192843626,
                    parent: 10028917631472640197,
                    name: 3,
                    text: 1,
                }),
            ),
            (1, Step::Work(18216690719357743194)),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 16789675239341262655,
                    parent: 995864001040360455,
                    name: 0,
                    text: 2,
                }),
            ),
            (0, Step::Work(14807180377262276184)),
            (0, Step::Duplicate(7397412081686271012)),
            (0, Step::Work(8057686803297367230)),
            (1, Step::Work(4512714835434908715)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 17627914801873212641,
                    parent: 12616595891657092197,
                    name: 6,
                    text: 0,
                }),
            ),
            (2, Step::Work(7337790291513551950)),
            (2, Step::Work(1180466901371692876)),
            (0, Step::Deliver(3821039509024783184)),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 13107520285440010231,
                    parent: 10598960963687546088,
                    name: 4,
                    text: 3,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 2246543535808375351,
                    parent: 6319698275849929893,
                    name: 0,
                    text: 1,
                }),
            ),
            (0, Step::Work(7175920170899054987)),
            (1, Step::Effect(Outcome::Run)),
            (1, Step::Work(4159517033770470710)),
            (0, Step::Work(18404622977775708442)),
            (0, Step::Commit(Reply::Served)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 6365491237751273237,
                    parent: 10782634279233329875,
                    name: 1,
                    text: 3,
                }),
            ),
            (0, Step::Commit(Reply::Served)),
            (2, Step::Work(8995257431548856822)),
            (1, Step::Connect),
            (0, Step::Deliver(10788655906123123742)),
            (1, Step::Deliver(16420210780264312280)),
            (0, Step::Work(479295481448839719)),
            (0, Step::Work(2208229052780071360)),
            (0, Step::Duplicate(3038716511811611936)),
            (0, Step::Effect(Outcome::Fail)),
            (2, Step::Work(15188616158263645844)),
            (2, Step::Work(4858157282421835116)),
        ],
        vec![
            (0, Step::Crash),
            (0, Step::Drop),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 17411163511274481715,
                    parent: 1312539373553298619,
                    name: 1,
                    text: 1,
                }),
            ),
            (2, Step::Work(13584574805752267923)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 14844142914246983467,
                    parent: 13391967695461610201,
                    name: 5,
                    text: 3,
                }),
            ),
            (2, Step::Commit(Reply::Served)),
            (0, Step::Effect(Outcome::Run)),
            (1, Step::Deliver(7904161053102581264)),
            (2, Step::Deliver(11456114341529141909)),
            (0, Step::Work(16877029101800043325)),
            (1, Step::Work(3138824959598783305)),
            (0, Step::Effect(Outcome::Run)),
            (1, Step::Work(9649128248737177936)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_swap_whose_store_rolls_back_after_its_pending_name_lands_ends_at_its_name() {
    converge(
        2,
        &[
            vec![(0, file("a")), (0, Step::CopyStore)],
            vec![
                (0, folder("A")),
                (0, rename(0, 1, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::RollBack),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_rewind_to_a_copy_taken_mid_swap_ends_at_the_swapped_name() {
    converge(
        2,
        &[
            vec![(0, file("c.txt"))],
            vec![
                (1, folder("C.TXT")),
                (1, rename(0, 1, "a")),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (0, Step::Backup),
                (0, Step::Rewind),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_pending_folder_the_user_deletes_during_a_recovery_comes_back_at_its_name() {
    converge(
        2,
        &[
            vec![(0, file("a")), (0, Step::CopyStore)],
            vec![
                (0, folder("A")),
                (0, rename(0, 1, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::RollBack),
                (0, delete(1)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_write_its_device_takes_away_before_sending_it_is_not_lost() {
    converge(
        2,
        &[vec![(0, file("a"))], vec![(0, file("b")), (0, Step::Leave)]],
    )
    .unwrap();
}

#[test]
fn a_device_that_left_still_takes_a_backup_of_the_server() {
    let mut steps = vec![(0, Step::Leave), (0, BACKUP), (1, file("b"))];
    steps.extend(commit(1));
    steps.push((1, Step::Restore));
    let world = converge(2, &[vec![(0, file("a"))], steps]).unwrap();
    assert!(world.seen.contains_key("server restored, commits lost"));
}

#[test]
fn a_pending_folder_whose_device_never_returns_is_renamed_by_another_device() {
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![
                (0, folder("A")),
                (0, rename(0, 1, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Leave),
            ],
        ],
    )
    .unwrap();
    let entries = world.devices[1].disk.entries();
    let folder = (entries.values())
        .find(|entry| entry.place == Place::Root)
        .expect("a folder at the root");
    assert_eq!(folder.name, "A");
}

#[test]
fn a_folder_another_device_finished_then_rewound_before_its_disk_followed_is_renamed_by_the_device_that_kept_it()
 {
    let mut lost = vec![
        (0, BACKUP),
        (0, folder("A")),
        (0, rename(0, 1, "a")),
        (0, Step::Work(0)),
        (0, Step::Work(0)),
        (0, Step::Work(0)),
    ];
    lost.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    lost.extend([(0, Step::Leave), (1, Step::Rewind), (1, Step::Connect)]);
    lost.extend(std::iter::repeat_n((1, Step::Work(0)), 4));
    lost.extend([(1, Step::Answer(0)), (1, Step::Work(0)), (1, Step::Work(0))]);
    let world = converge(2, &[vec![(0, file("a"))], lost]).unwrap();
    let entries = world.devices[1].disk.entries();
    let folder = (entries.values())
        .find(|entry| entry.place == Place::Root && entry.content.is_none())
        .expect("a folder at the root");
    assert_eq!(folder.name, "A (conflict from device-1 2026-11-09)");
}

#[test]
fn a_swap_split_over_commits_ends_at_its_names_while_another_device_finishes_on_disk() {
    let refused = Step::Commit(Reply::Refused(error::Code::LimitExceeded));
    let mut finished_elsewhere = 0;
    for between in 0..16 {
        let mut swap = vec![
            (0, rename(0, 0, "ss")),
            (0, rename(1, 0, "a")),
            (0, rename(0, 0, "b")),
            (0, SCAN),
            (0, refused),
            (0, DELIVER),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
            (0, refused),
            (0, DELIVER),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
        ];
        swap.extend(std::iter::repeat_n((1, Step::Work(0)), between));
        swap.extend([(0, COMMIT), (0, COMMIT), (0, DELIVER)]);
        let world = converge(2, &[vec![(0, folder("a")), (0, folder("b"))], swap]).unwrap();
        assert_eq!(
            world.seen["commit refused for its size, planned again in parts"],
            2
        );
        for device in &world.devices {
            let names: BTreeSet<String> = (device.disk.entries().values())
                .filter(|entry| entry.place == Place::Root)
                .map(|entry| entry.name.clone())
                .collect();
            assert_eq!(names, BTreeSet::from(["a".into(), "b".into()]), "{between}");
        }
        finished_elsewhere += usize::from(world.devices[1].engine.commits.accepted() > 0);
    }
    assert!(finished_elsewhere > 0);
}

#[test]
fn a_finish_another_device_sent_before_the_setter_finished_and_its_user_renamed_again_keeps_the_newer_name()
 {
    let refused = Step::Commit(Reply::Refused(error::Code::LimitExceeded));
    for between in 0..16 {
        let mut swap = vec![
            (0, rename(0, 0, "ss")),
            (0, rename(1, 0, "a")),
            (0, rename(0, 0, "b")),
            (0, SCAN),
            (0, refused),
            (0, DELIVER),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
            (0, refused),
            (0, DELIVER),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
        ];
        swap.extend(std::iter::repeat_n((1, Step::Work(0)), between));
        swap.extend([
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
            (0, rename(0, 0, "c.txt")),
            (0, SCAN),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
        ]);
        let world = converge(2, &[vec![(0, folder("a")), (0, folder("b"))], swap]).unwrap();
        for device in &world.devices {
            let names: BTreeSet<String> = (device.disk.entries().values())
                .filter(|entry| entry.place == Place::Root)
                .map(|entry| entry.name.clone())
                .collect();
            assert_eq!(
                names,
                BTreeSet::from(["a".into(), "c.txt".into()]),
                "{between}"
            );
        }
    }
}

#[test]
fn a_store_rolled_back_after_finishing_another_devices_mark_publishes_no_pending_folder() {
    let mut lost = vec![
        (1, Step::CopyStore),
        (0, folder("A")),
        (0, rename(0, 1, "a")),
    ];
    lost.extend(std::iter::repeat_n((0, Step::Work(0)), 3));
    lost.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    lost.extend([(0, Step::Drop), (1, Step::RollBack), (1, Step::Connect)]);
    lost.extend(std::iter::repeat_n((1, Step::Work(0)), 10));
    let world = converge(2, &[vec![(0, file("a"))], lost]).unwrap();
    for device in &world.devices {
        let folders: Vec<&str> = (device.disk.entries().values())
            .filter(|entry| entry.place == Place::Root && entry.content.is_none())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(folders, ["A"]);
    }
}

const BACKUP: Step = Step::Backup;
const RESTORE: Step = Step::Restore;

/// Runs the phases, with a restore of the server to a backup taken at the start of the second one
/// as the third, and checks that the devices handled the loss as the design says.
fn restored(devices: usize, synced: Vec<(usize, Step)>, lost: Vec<(usize, Step)>) -> World {
    let lost = std::iter::once((0, BACKUP)).chain(lost).collect();
    let world = converge(devices, &[synced, lost, vec![(0, RESTORE)]]).unwrap();
    for what in [
        "server restored, commits lost",
        "reconnect finds the server lost commits",
        "resume finds the server behind",
        "listing replaces R",
    ] {
        assert!(
            world.seen.contains_key(what),
            "the run did not reach: {what}"
        );
    }
    world
}

/// The server loses the commit that created a file. Its author sends the create again.
#[test]
fn a_file_whose_create_the_server_lost_is_created_again() {
    let world = restored(2, Vec::new(), vec![(0, file("a"))]);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// The server loses a delete. Its author sends the delete again, and the file comes back on no
/// device.
#[test]
fn a_file_whose_delete_the_server_lost_stays_deleted() {
    let world = restored(2, vec![(0, file("a"))], vec![(0, delete(0))]);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

/// The server loses a rename. Both devices hold the new name, so both send it again.
#[test]
fn a_file_whose_rename_the_server_lost_keeps_its_new_name() {
    let world = restored(2, vec![(0, file("a"))], vec![(0, rename(0, 0, "b"))]);
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"]);
    }
}
/// The server loses the create of a file, and the devices create it again. A copy of its bytes
/// that the engine makes later, with no user asking for one, is still a divergence.
#[test]
fn a_later_copy_of_a_write_the_server_lost_is_still_refused() {
    let mut world = restored(2, Vec::new(), vec![(0, file("a"))]);
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    for name in ["b", "c", "d"] {
        world.devices[0]
            .disk
            .create(Place::Root, name, bytes.clone())
            .unwrap();
    }
    assert!(world.phase(3, &[]).is_err());
}

/// A restore that loses no rename allows no later rename to a name the user never gave.
#[test]
fn a_restore_that_loses_no_rename_allows_no_unasked_rename() {
    let mut world = converge(2, &[vec![(0, file("a")), (1, file("b"))]]).unwrap();
    world.phase(1, &[(0, BACKUP), (0, RESTORE)]).unwrap();
    let disk = &world.devices[1].disk;
    let b = (disk.visible().iter()).position(|ino| disk.entries()[ino].name == "b");
    world
        .phase(2, &[(1, rename(b.unwrap(), 0, "c.txt"))])
        .unwrap();
    let a = world.devices[0].disk.child(&Place::Root, "a").unwrap();
    world.devices[0].disk.rename(a, Place::Root, "b").unwrap();
    assert!(world.phase(3, &[]).is_err());
}

fn an_edit_of_a_trashed_conflict_named_file() -> (World, koidrive_proto::v1::NodeState) {
    let mut world = converge(2, &[vec![(0, file("a")), (1, file("a")), (0, folder("b"))]]).unwrap();
    let conflict = world
        .space()
        .nodes()
        .into_iter()
        .find(|node| node.name.contains("(conflict from "))
        .unwrap();
    let target = |device: usize| {
        let disk = &world.devices[device].disk;
        disk.visible()
            .iter()
            .position(|ino| disk.entries()[ino].name == conflict.name)
            .unwrap()
    };
    let (author, other) = (target(1), target(0));
    let mut deleted = vec![(1, overwrite(author))];
    deleted.extend(commit(1));
    deleted.push((1, delete(author)));
    deleted.extend(commit(1));
    world.run(1, &deleted).unwrap();
    let trashed = world
        .space()
        .nodes()
        .into_iter()
        .find(|node| node.node_id == conflict.node_id)
        .unwrap();
    assert_eq!(
        trashed.status,
        koidrive_proto::v1::NodeStatus::Trashed as i32
    );
    assert_eq!(trashed.name, conflict.name);
    let mut edit = vec![(0, overwrite(other)), (0, SCAN)];
    edit.extend([(0, DELIVER); 8]);
    world.run(2, &edit).unwrap();
    world.devices[0]
        .engine
        .trees
        .set_synced(conflict.node_id.clone(), None);
    world.devices[0].persist().unwrap();
    (world, conflict)
}

#[test]
fn an_edit_revives_a_file_at_the_conflict_name_the_server_already_holds() {
    let (mut world, conflict) = an_edit_of_a_trashed_conflict_named_file();
    world.phase(2, &[]).unwrap();
    let revived = world
        .space()
        .nodes()
        .into_iter()
        .find(|node| node.node_id == conflict.node_id)
        .unwrap();
    assert_eq!(revived.status, koidrive_proto::v1::NodeStatus::Live as i32);
    assert_eq!(
        (revived.parent_id, revived.name),
        (conflict.parent_id, conflict.name)
    );
    assert_eq!(revived.current.unwrap().content.unwrap().mtime_ns, 4);
}

#[test]
fn a_revival_at_an_unasked_name_or_parent_is_rejected() {
    for rename in [true, false] {
        let (mut world, conflict) = an_edit_of_a_trashed_conflict_named_file();
        let folder = world
            .space()
            .nodes()
            .into_iter()
            .find(|node| node.name == "b")
            .unwrap()
            .node_id;
        world.run(2, &[(0, COMMIT)]).unwrap();
        let device = &mut world.devices[0];
        let mut state = device.engine.commits.state();
        let moved = state
            .in_flight
            .as_mut()
            .unwrap()
            .commit
            .ops
            .iter_mut()
            .find_map(|op| match &mut op.op {
                Some(koidrive_proto::v1::op::Op::Move(moved))
                    if moved.node_id == conflict.node_id =>
                {
                    Some(moved)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (&moved.new_parent_id, &moved.new_name),
            (&conflict.parent_id, &conflict.name)
        );
        if rename {
            moved.new_name = "invented".into();
        } else {
            moved.new_parent_id = folder;
        }
        device.engine.commits = koidrive_engine::commit::Commits::from_state(state).unwrap();
        device.store = device.engine.clone();
        let failure = world.phase(2, &[]).unwrap_err();
        assert!(
            matches!(failure, Failure::Diverged(why) if why.contains("never gave to anything"))
        );
    }
}
/// A restore that loses an edit lets a device send the position L holds for the file again, here
/// when it revives the file that another device's delete trashed after the restore.
#[test]
fn a_restore_that_loses_an_edit_lets_its_file_be_revived() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (0, u(1, 0, 0, 6, 0)),
            (0, Step::Work(0)),
            (2, u(0, 0, 0, 0, 0)),
        ],
        vec![
            (0, u(1, 0, 0, 2, 0)),
            (2, u(0, 0, 0, 2, 0)),
            (2, u(1, 0, 0, 3, 0)),
            (0, Step::Work(0)),
            (0, u(2, 925163541728872510, 0, 0, 0)),
            (0, u(0, 0, 11523797806819799120, 0, 0)),
        ],
        vec![
            (0, u(0, 0, 0, 0, 0)),
            (0, BACKUP),
            (1, u(2, 10499357163619158257, 0, 0, 0)),
            (0, u(0, 0, 20731787025803467, 0, 0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (0, u(4, 4907598952368571970, 0, 0, 0)),
            (1, Step::Work(0)),
            (0, RESTORE),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_commit_applied_again_after_a_restore_hides_no_commit_before_it() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(1, u(1, 17503378262933104031, 8676797805769390255, 0, 0))],
        vec![
            (2, u(1, 14773020708250346160, 17688344867352952878, 1, 3)),
            (1, u(1, 8739664364649419579, 10425898527117367920, 1, 1)),
            (0, u(2, 2703697463063541661, 14694850271240265751, 4, 0)),
        ],
        vec![
            (0, u(0, 14175073634118050617, 4546324325139635686, 6, 1)),
            (1, u(3, 11841437895142882941, 7695071214306762243, 6, 3)),
            (0, Step::Scan),
            (0, Step::Work(17835076681416284481)),
            (2, BACKUP),
            (1, Step::Work(6804034245751716185)),
            (1, Step::Work(8817138495063991201)),
            (0, Step::Work(9139201847012193521)),
            (1, Step::Deliver(5725093936307975295)),
            (2, Step::Deliver(3760675399677619784)),
            (2, u(2, 15632049101928360904, 15695858799699297419, 0, 3)),
            (1, Step::Work(17555396458820976808)),
            (2, Step::Scan),
            (2, Step::Deliver(1503442958024575124)),
            (0, u(0, 16916060028658482220, 3213220271578802601, 5, 3)),
            (0, Step::Deliver(9182944446394345860)),
            (2, Step::Commit(Reply::Served)),
            (1, RESTORE),
            (0, u(2, 9997817214443366706, 12996066320436592824, 5, 2)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    assert!(world.seen.contains_key("resume finds the server behind"));
}

/// Once the devices are quiet after a restore, a copy of a write the restore took from the server
/// is refused like any other copy.
#[test]
fn a_copy_of_a_lost_write_after_the_recovery_is_refused() {
    let mut world = restored(2, Vec::new(), vec![(0, file("a"))]);
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    world.devices[0]
        .disk
        .create(Place::Root, "b", bytes)
        .unwrap();
    assert!(world.phase(3, &[]).is_err());
}

/// A copy of a lost write that the engine makes while the devices still recover from the
/// restore, past the files the restore lets the write end in, is a divergence.
#[test]
fn a_copy_of_a_lost_write_during_the_recovery_is_refused() {
    let mut world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    world.run(2, &[(0, RESTORE)]).unwrap();
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    world.devices[0]
        .disk
        .create(Place::Root, "b", bytes)
        .unwrap();
    let failure = world.phase(2, &[]).unwrap_err();
    assert!(
        failure.to_string().contains("is in more than one file"),
        "{failure}"
    );
}

/// Once the devices are quiet after a restore that rolled a file back, the write it took stays in
/// the files the server holds it in, not in one file for each device that held it.
#[test]
fn a_copy_of_a_rolled_back_write_after_the_recovery_is_refused() {
    let edit = Step::User(Action {
        kind: 2,
        target: 0,
        parent: 0,
        name: 0,
        text: 0,
    });
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, edit)]]).unwrap();
    world.phase(2, &[(0, RESTORE)]).unwrap();
    let lost = (world.devices[0].disk.entries().values()).find_map(|entry| {
        entry
            .content
            .clone()
            .filter(|content| content.mtime_ns == 2)
    });
    world.devices[0]
        .disk
        .create(Place::Root, "b", lost)
        .unwrap();
    let failure = world.phase(3, &[]).unwrap_err();
    assert!(
        failure.to_string().contains("is in more than one file"),
        "{failure}"
    );
}

fn copies_of(world: &World, write: i64) -> usize {
    (files_on(world, 0).into_iter())
        .filter(|(_, content)| content.as_ref().is_some_and(|c| c.mtime_ns == write))
        .count()
}

fn a_write_a_restore_kept_and_another_device_then_edited() -> Vec<Vec<(usize, Step)>> {
    vec![
        vec![
            (0, file("a")),
            (0, Step::Work(0)),
            (2, file("a")),
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (0, SCAN),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (1, Step::Work(0)),
            (2, folder("A")),
            (2, DELIVER),
            (2, user(4, 1175486844842293939, 0, "a")),
            (2, rename(0, 0, "ß")),
            (0, file("A")),
            (0, user(2, 294041810860057272, 0, "a")),
        ],
        vec![(0, rename(0, 0, "A")), (0, BACKUP)],
        vec![
            (0, RESTORE),
            (0, user(2, 3337042255730594815, 0, "a")),
            (2, folder("b")),
            (0, Step::Work(0)),
            (2, user(3, 7894732180522727006, 10174581841506759649, "a")),
            (0, Step::Connect),
            (0, Step::Work(0)),
            (0, Step::Work(3083887596059364753)),
            (0, Step::Work(13851926561577354009)),
        ],
    ]
}

#[test]
fn each_device_that_cannot_tell_a_restore_kept_a_write_keeps_its_copy_beside_a_later_edit() {
    let world = converge(3, &a_write_a_restore_kept_and_another_device_then_edited()).unwrap();
    assert_eq!(copies_of(&world, 3), 2);
}

#[test]
fn a_copy_beside_the_copies_of_a_write_a_restore_kept_is_refused() {
    let mut world = converge(3, &a_write_a_restore_kept_and_another_device_then_edited()).unwrap();
    let kept = (world.devices[0].disk.entries().values())
        .find_map(|entry| (entry.content.clone()).filter(|content| content.mtime_ns == 3));
    world.devices[0]
        .disk
        .create(Place::Root, "c.txt", kept)
        .unwrap();
    let failure = world.phase(3, &[]).unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("write 3 is in more than one file"),
        "{failure}"
    );
}

#[test]
fn a_copy_of_a_kept_write_by_a_device_whose_own_commit_shows_the_server_kept_it_is_refused() {
    let phases = [
        vec![(0, file("a"))],
        vec![(2, delete(0)), (0, rename(0, 0, "ß")), (0, BACKUP)],
        vec![
            (0, RESTORE),
            (2, Step::Connect),
            (2, Step::Work(0)),
            (2, EFFECT),
            (2, overwrite(0)),
            (2, Step::Work(2940287964182624177)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
        ],
    ];
    match converge(3, &phases) {
        Ok(world) => assert_eq!(copies_of(&world, 1), 1),
        Err(failure) => assert!(
            failure
                .to_string()
                .contains("write 1 is in more than one file"),
            "{failure}"
        ),
    }
}

#[test]
fn an_edit_back_to_the_bytes_a_restore_left_keeps_its_file_and_the_lost_edit_beside_it() {
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (1, overwrite(0))],
            vec![(0, RESTORE), (0, overwrite(0))],
        ],
    )
    .unwrap();
    for device in &world.devices {
        let disk = &device.disk;
        let write = |ino: u128| disk.entries()[&ino].content.as_ref().map(|c| c.mtime_ns);
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(write(a), Some(3));
        let writes: BTreeSet<Option<i64>> = disk.entries().keys().map(|ino| write(*ino)).collect();
        assert_eq!(writes, BTreeSet::from([Some(2), Some(3)]));
    }
}

#[test]
fn a_write_two_downloads_place_after_a_restore_took_it_is_kept_once_beside_the_edit_over_it() {
    let mut lost = vec![(0, file("a"))];
    lost.extend(commit(0));
    lost.extend([
        (1, DELIVER),
        (1, EFFECT),
        (2, DELIVER),
        (2, EFFECT),
        (0, overwrite(0)),
        (0, RESTORE),
        (1, EFFECT),
        (2, EFFECT),
    ]);
    let world = converge(3, &[vec![(0, BACKUP)], lost]).unwrap();
    for device in &world.devices {
        let disk = &device.disk;
        let write = |ino: &u128| disk.entries()[ino].content.as_ref().map(|c| c.mtime_ns);
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(write(&a), Some(2));
        let mut writes: Vec<Option<i64>> = disk.entries().keys().map(write).collect();
        writes.sort();
        assert_eq!(writes, [Some(1), Some(2)]);
    }
}

#[test]
fn a_folder_renamed_after_a_restore_is_not_moved_back_by_a_device_that_never_moved_it() {
    let world = converge(
        3,
        &[
            vec![(2, file("ss")), (2, folder("A"))],
            vec![(0, BACKUP), (2, rename(0, 0, "b"))],
            vec![
                (0, folder("c.txt")),
                (0, SCAN),
                (1, rename(0, 0, "a")),
                (0, COMMIT),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
    for device in &world.devices {
        let disk = &device.disk;
        let paths: BTreeSet<Vec<String>> = (disk.entries().keys())
            .filter_map(|ino| disk.path(*ino))
            .collect();
        let expected = [["a"], ["b"], ["c.txt"]].map(|path| path.map(String::from).to_vec());
        assert_eq!(paths, BTreeSet::from(expected));
    }
}

#[test]
fn a_rename_after_a_restore_is_not_undone_by_the_lost_rename_it_replaced() {
    let mut after = vec![(0, RESTORE), (1, Step::Connect)];
    for _ in 0..12 {
        after.extend([(1, Step::Work(0)), (1, DELIVER), (1, EFFECT)]);
    }
    after.extend([
        (1, SCAN),
        (1, rename(0, 0, "ss")),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, DELIVER),
        (1, Step::Work(0)),
        (1, DELIVER),
    ]);
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (0, rename(0, 0, "b"))],
            after,
        ],
    )
    .unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["ss"]);
    }
}

#[test]
fn the_last_of_two_lost_renames_is_kept_through_the_restore() {
    let world = converge(
        3,
        &[
            vec![(0, file("a"))],
            vec![(1, rename(0, 0, "A"))],
            vec![(0, BACKUP), (2, rename(0, 0, "b"))],
            vec![(0, rename(0, 0, "ss"))],
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["ss"]);
    }
}

#[test]
fn a_lost_rename_sent_again_before_a_later_lost_rename_keeps_the_later_one() {
    let mut lost = vec![
        (0, rename(0, 0, "ss")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, RESTORE),
        (1, Step::Connect),
    ];
    for _ in 0..12 {
        lost.extend([(1, Step::Work(0)), (1, DELIVER), (1, EFFECT)]);
    }
    lost.push((0, Step::Connect));
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (1, rename(0, 0, "b"))],
            lost,
        ],
    )
    .unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["ss"]);
    }
}

#[test]
fn a_file_renamed_by_the_server_in_a_folder_it_merged_is_not_moved_back() {
    converge(
        2,
        &[
            vec![
                (0, Step::Work(0)),
                (0, folder("a")),
                (0, rename(0, 0, "ss")),
                (0, user(1, 0, 1, "b")),
                (0, file("a")),
                (0, BACKUP),
            ],
            vec![
                (0, overwrite(1)),
                (0, Step::Work(0)),
                (0, COMMIT),
                (0, user(1, 0, 1, "a")),
                (0, folder("A")),
                (0, RESTORE),
                (0, user(0, 0, 2, "a")),
                (0, Step::Work(0)),
                (0, delete(4)),
                (0, folder("ß")),
            ],
        ],
    )
    .unwrap();
}

fn created_again_by_both_and_deleted_by_one(made: Step, edit: bool) -> World {
    let mut lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
    ];
    lost.extend(edit.then_some((1, overwrite(0))));
    lost.extend([
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ]);
    converge(2, &[Vec::new(), vec![(0, BACKUP), (0, made)], lost]).unwrap()
}

#[test]
fn a_lost_folder_another_device_creates_again_and_deletes_first_is_deleted_on_both() {
    let world = created_again_by_both_and_deleted_by_one(folder("a"), false);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_lost_file_another_device_creates_again_and_deletes_first_is_deleted_on_both() {
    let world = created_again_by_both_and_deleted_by_one(file("a"), false);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn an_edit_of_a_lost_file_another_device_creates_again_and_deletes_first_revives_it() {
    let world = created_again_by_both_and_deleted_by_one(file("a"), true);
    for device in &world.devices {
        let disk = &device.disk;
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(disk.entries().len(), 1);
        assert_eq!(
            disk.entries()[&a].content.as_ref().map(|c| c.mtime_ns),
            Some(2)
        );
    }
}

#[test]
fn a_swap_after_a_restore_passes_through_the_pending_name_of_a_name_a_lost_commit_gave() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (1, u(0, 0, 0, 2, 0)),
            (1, u(1, 0, 6892790594048859264, 0, 0)),
        ],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (0, BACKUP),
            (0, u(1, 0, 1923514285381434823, 0, 0)),
            (2, u(1, 0, 0, 1, 0)),
            (0, u(1, 0, 13397848212569861839, 2, 0)),
            (0, u(1, 0, 0, 3, 0)),
        ],
        vec![
            (2, u(0, 0, 3443233485426864259, 1, 0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (1, DELIVER),
            (0, RESTORE),
            (1, Step::Effect(Outcome::Crash(Cut::Placed))),
        ],
    ];
    converge(2, &phases).unwrap();
}

/// Once the devices are quiet after a restore that lost a rename, the old name is refused like
/// any other name the user did not give.
#[test]
fn a_name_a_restore_lost_is_refused_after_the_recovery() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    world.phase(2, &[(1, rename(0, 0, "b"))]).unwrap();
    world.phase(3, &[(0, RESTORE)]).unwrap();
    world.phase(4, &[(1, rename(0, 0, "c.txt"))]).unwrap();
    let ino = world.devices[0].disk.child(&Place::Root, "c.txt").unwrap();
    world.devices[0].disk.rename(ino, Place::Root, "b").unwrap();
    assert!(world.phase(5, &[]).is_err());
}

/// A file whose create the server renamed is reverted until the effects move it to the server's
/// name, and the server loses the create before they do. Device 1 already took the file, so both
/// devices create it again. Device 0 keeps the node's ID instead of giving the file a new one, so
/// the server ends with the file once.
#[test]
fn a_lost_file_the_server_renamed_is_created_again_once() {
    let mut lost = vec![(0, BACKUP), (0, file("C.TXT"))];
    lost.extend(commit(0));
    lost.extend([
        (1, DELIVER),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
        (0, RESTORE),
    ]);
    converge(2, &[vec![(0, file("c.txt"))], lost]).unwrap();
}

/// Both devices create a file the server lost again under its ID, and device 0's create comes
/// first. The server refuses device 1's create with ID_EXISTS, and device 1 takes the file from R
/// instead of leaving it a problem.
#[test]
fn a_lost_file_both_devices_create_again_is_kept_once() {
    let lost = vec![(0, BACKUP), (0, file("a"))];
    let again = vec![
        (0, RESTORE),
        (1, Step::Connect),
        (1, DELIVER),
        (1, COMMIT),
        (0, Step::Connect),
        (0, DELIVER),
        (0, COMMIT),
        (1, COMMIT),
        (0, COMMIT),
    ];
    let world = converge(2, &[lost, again]).unwrap();
    assert_eq!(world.seen["op rejected"], 1);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().len() == 1)
    );
}

#[test]
fn a_lost_file_another_device_creates_again_after_its_author_is_kept_once() {
    let lost = vec![(0, BACKUP), (0, file("a"))];
    let again = vec![
        (0, RESTORE),
        (1, Step::Connect),
        (1, DELIVER),
        (1, COMMIT),
        (0, Step::Connect),
        (0, DELIVER),
        (0, COMMIT),
        (0, COMMIT),
        (1, COMMIT),
    ];
    let world = converge(2, &[lost, again]).unwrap();
    assert!(!world.seen.contains_key("op rejected"));
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().len() == 1)
    );
}

/// Devices 1 and 0 rename a file that device 2 deletes, and device 1 crashes once it recorded the
/// removal and before the removal reached the file. Recovery finds the old name empty and nothing
/// in the local trash, so the removal failed, and device 1 moves the node instead of creating the
/// file again beside the one device 0 revived (GUN-443).
#[test]
fn a_removal_recovered_after_the_user_moved_its_file_keeps_one_file() {
    let u = |kind, name| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0))],
        vec![
            (2, u(4, 0)),
            (1, u(3, 1)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (1, DELIVER),
            (0, u(3, 1)),
            (1, Step::Effect(Outcome::Crash(Cut::Quarantined))),
        ],
    ];
    converge(3, &phases).unwrap();
}

/// GUN-444: device 1's create is applied at seq 3 and its result arrives, but the server is
/// restored to seq 2 before device 1's stream reaches seq 3. The server lost a commit the device
/// saw accepted, so the device lists R again instead of starting recovery.
#[test]
fn a_device_holding_the_result_of_a_commit_the_server_lost_lists_r_again() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 0))],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (1, u(0, 0, 0, 1, 0)),
            (0, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, BACKUP),
            (1, Step::Work(3407374106458668457)),
            (1, Step::Deliver(4808377001051081459)),
            (0, RESTORE),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn an_edit_made_after_its_commit_was_built_follows_the_conflict_copy_to_its_name() {
    let synced = vec![(0, file("c.txt"))];
    let mut racing = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, overwrite(0)),
        (1, file("C.TXT")),
        (1, SCAN),
        (1, COMMIT),
        (1, overwrite(0)),
        (1, COMMIT),
        (1, DELIVER),
    ]);
    let world = converge(2, &[synced, racing]).unwrap();
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 3);
    }
}

#[test]
fn a_conflict_copy_whose_name_a_file_of_the_same_commit_took_reaches_its_name() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = [
        vec![(0, u(1, 0, 0, 3, 0)), (0, Step::Work(0))],
        vec![
            (1, u(2, 0, 0, 0, 0)),
            (1, Step::Work(0)),
            (2, u(0, 0, 0, 0, 0)),
            (1, u(1, 0, 0, 0, 3)),
            (0, u(0, 8058298412, 13820684896388115470, 3, 2)),
            (1, u(2, 5595505104648703704, 14520073170452222456, 0, 3)),
            (2, Step::Work(1546513718156240915)),
            (2, COMMIT),
            (1, Step::Work(4166004558488083538)),
            (0, u(2, 6580354071670050619, 8205634420763423225, 3, 3)),
            (1, u(3, 2072403800777770851, 14264798506198798549, 4, 0)),
            (0, u(3, 3355719809822832468, 4572565801030521563, 0, 1)),
            (1, u(1, 11181736274241258047, 5920353529640080243, 0, 0)),
            (1, Step::Work(1407293088156174997)),
            (1, u(1, 9500849518790139864, 10437075667485134398, 5, 2)),
            (1, Step::Work(4005447178108011140)),
            (1, u(2, 7452752783842175740, 3965247538109782643, 4, 3)),
            (1, COMMIT),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn an_edited_file_parked_while_a_new_folder_takes_its_name_reaches_the_server() {
    converge(
        2,
        &[
            vec![(0, file("a")), (0, rename(0, 0, "ss"))],
            vec![
                (0, rename(0, 0, "ß")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (1, Step::Work(0)),
                (1, EFFECT),
                (1, folder("ß")),
                (1, overwrite(0)),
                (0, delete(0)),
                (0, Step::Work(0)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_made_while_its_file_is_parked_for_a_rename_stays_on_the_server() {
    let synced = vec![(0, file("c.txt"))];
    let mut racing = vec![(0, rename(0, 0, "C.TXT")), (0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, overwrite(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, DELIVER),
        (1, EFFECT),
        (1, COMMIT),
    ]);
    let world = converge(2, &[synced, racing]).unwrap();
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 2);
    }
}

#[test]
fn a_remote_rename_that_lands_while_a_rename_of_this_device_is_in_flight_is_not_sent_after_it() {
    converge(
        3,
        &[
            vec![(0, file("C.TXT"))],
            vec![
                (0, rename(0, 0, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (2, rename(0, 0, "A")),
                (0, Step::Work(0)),
                (2, DELIVER),
                (2, EFFECT),
                (2, SCAN),
                (2, rename(0, 0, "C.TXT")),
                (2, Step::Work(11306161604300316446)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_remote_rename_that_lands_after_a_rename_of_this_device_was_answered_is_not_sent_after_it() {
    converge(
        3,
        &[
            vec![(0, file("C.TXT"))],
            vec![
                (0, rename(0, 0, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (2, rename(0, 0, "A")),
                (0, Step::Work(0)),
                (2, DELIVER),
                (2, EFFECT),
                (2, SCAN),
                (2, COMMIT),
                (2, COMMIT),
                (2, rename(0, 0, "C.TXT")),
                (2, Step::Work(11111563161135729838)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_remote_rename_that_lands_after_a_rename_of_this_device_was_acknowledged_is_not_sent_after_it()
{
    converge(
        3,
        &[
            vec![(0, file("C.TXT"))],
            vec![
                (0, rename(0, 0, "a")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (2, rename(0, 0, "A")),
                (0, Step::Work(0)),
                (2, DELIVER),
                (2, EFFECT),
                (2, SCAN),
                (2, COMMIT),
                (2, rename(0, 0, "C.TXT")),
                (2, COMMIT),
                (2, DELIVER),
                (2, SCAN),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_write_a_restore_exposed_stays_on_the_disks_through_a_second_restore() {
    let edit = Step::User(Action {
        kind: 2,
        target: 0,
        parent: 0,
        name: 0,
        text: 7,
    });
    let mut lost = vec![(0, RESTORE), (0, Step::Connect), (1, Step::Connect)];
    for _ in 0..30 {
        lost.extend([(0, Step::Work(0)), (1, Step::Work(0))]);
    }
    lost.push((0, RESTORE));
    let world = keep(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, edit)], lost],
    )
    .unwrap();
    assert!(world.devices.iter().all(|device| device.disk.holds(2)));
}

#[test]
fn a_lost_file_another_device_creates_again_renames_and_deletes_stays_deleted() {
    let lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ];
    let world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))], lost]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_lost_file_created_again_edited_and_deleted_stays_deleted() {
    let lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, overwrite(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ];
    let world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))], lost]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_folder_sent_at_a_pending_name_that_an_edit_brings_back_takes_its_name() {
    let phases = vec![
        vec![(0, file("a"))],
        vec![
            (1, overwrite(0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, file("b")),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, folder("A")),
            (0, folder("c.txt")),
            (1, folder("a")),
            (0, Step::Work(191953005214663284)),
            (0, overwrite(9057257590032405897)),
            (0, Step::Work(1656413589262271176)),
            (0, rename(5708514389238217460, 9546544883134816759, "a")),
            (0, Step::Work(0)),
            (0, COMMIT),
            (0, delete(4880399959711454722)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_folder_sent_at_a_pending_name_for_a_case_folded_name_that_an_edit_brings_back_takes_its_name()
{
    let phases = vec![
        vec![(1, file("ß")), (0, file("a")), (0, file("c.txt"))],
        vec![
            (0, overwrite(2828337353004866769)),
            (0, folder("a")),
            (0, Step::Work(0)),
            (0, Step::Work(15079979194851)),
            (
                0,
                act(0, 17522603601231760989, 2149907348572024772, "ss", 1),
            ),
            (1, act(2, 4682896762454486991, 9427922460036998013, "ß", 1)),
            (0, rename(14663728549993967226, 16228656401449306453, "A")),
            (0, Step::Connect),
            (0, SCAN),
            (0, Step::Work(13397615803372223702)),
            (0, Step::Work(8052162068318610133)),
            (
                0,
                act(1, 14296328599741790799, 10114328226956053520, "ß", 3),
            ),
            (0, act(4, 11544038771373029048, 9096678758980585778, "A", 2)),
            (0, COMMIT),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_lost_edit_another_device_took_comes_back_at_its_name_without_a_conflict_copy() {
    for (first, second) in [(0, 1), (1, 0)] {
        let mut world = converge(
            2,
            &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
        )
        .unwrap();
        let edit = (world.devices[0].disk.entries().values())
            .find_map(|entry| entry.content.clone())
            .unwrap();
        let mut again = vec![(0, RESTORE)];
        for device in [first, second] {
            again.push((device, Step::Connect));
            again.extend([(device, DELIVER); 8]);
        }
        for device in [first, second] {
            again.extend(commit(device));
            again.extend([(device, DELIVER); 4]);
        }
        world.phase(2, &again).unwrap();
        for device in &world.devices {
            let entries: Vec<_> = device.disk.entries().values().collect();
            assert_eq!(entries.len(), 1, "device {first} first: {entries:?}");
            assert_eq!(entries[0].name, "a");
            assert_eq!(entries[0].content.as_ref(), Some(&edit));
        }
    }
}

#[test]
fn a_lost_edit_is_kept_beside_an_edit_made_after_the_loss() {
    let mut world = converge(
        3,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let edit = (world.devices[0].disk.entries().values())
        .find_map(|entry| entry.content.clone())
        .unwrap();
    let mut again = vec![(0, RESTORE), (2, Step::Connect)];
    again.extend([(2, DELIVER); 8]);
    again.push((2, overwrite(0)));
    again.extend(commit(2));
    again.extend([(2, DELIVER); 4]);
    world.phase(2, &again).unwrap();
    let later = (world.devices[2].disk.entries().values())
        .find(|entry| entry.name == "a")
        .and_then(|entry| entry.content.clone())
        .unwrap();
    assert_ne!(later, edit);
    for device in &world.devices {
        let entries = device.disk.entries();
        let at_a = entries.values().find(|entry| entry.name == "a").unwrap();
        assert_eq!(at_a.content.as_ref(), Some(&later));
        assert!(
            entries
                .values()
                .any(|entry| entry.content.as_ref() == Some(&edit)),
            "{entries:?}"
        );
    }
}

#[test]
fn a_second_restore_to_a_later_backup_keeps_the_edit_it_brings_back() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut edit = vec![(0, BACKUP), (0, overwrite(0))];
    edit.extend(commit(0));
    edit.extend([(1, DELIVER); 4]);
    edit.push((0, BACKUP));
    world.run(1, &edit).unwrap();
    let mut first = vec![(0, Step::RestoreEarlier), (1, Step::Connect)];
    first.extend([(1, DELIVER); 8]);
    first.extend([(2, Step::Connect), (2, file("b"))]);
    first.extend(commit(2));
    first.extend([(1, DELIVER); 4]);
    world.run(2, &first).unwrap();
    let mut second = vec![(0, RESTORE), (1, Step::Connect)];
    second.extend([(1, DELIVER); 8]);
    second.extend(commit(1));
    world.phase(3, &second).unwrap();
}

#[test]
fn a_second_restore_keeps_an_edit_another_device_never_downloaded() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut edit = vec![(0, BACKUP), (0, overwrite(0))];
    edit.extend(commit(0));
    edit.extend([(1, DELIVER); 4]);
    edit.push((1, file("b")));
    edit.extend(commit(1));
    edit.extend([(1, DELIVER); 4]);
    edit.push((0, BACKUP));
    world.run(1, &edit).unwrap();
    let mut first = vec![(0, Step::RestoreEarlier), (1, Step::Connect)];
    first.extend([(1, DELIVER); 8]);
    first.extend(commit(1));
    first.extend([(1, DELIVER); 4]);
    world.run(2, &first).unwrap();
    let mut second = vec![(0, RESTORE), (1, Step::Connect)];
    second.extend([(1, DELIVER); 8]);
    second.extend(commit(1));
    world.phase(3, &second).unwrap();
}

#[test]
fn a_file_a_user_deletes_after_a_restore_stays_deleted() {
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (0, overwrite(0))],
            vec![(0, RESTORE), (0, delete(0))],
        ],
    )
    .unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_file_a_restore_moves_back_stays_deleted_when_a_user_deletes_it_after() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (2, u(1, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 5, 0)),
            (0, u(1, 0, 71331116303773897, 0, 0)),
            (0, u(0, 0, 1770787363935275214, 5, 0)),
            (2, u(1, 0, 4121262060029593686, 4, 0)),
            (2, u(0, 0, 0, 1, 0)),
            (1, u(0, 0, 0, 5, 0)),
        ],
        vec![
            (0, u(0, 0, 0, 0, 0)),
            (1, u(3, 2084651236646964065, 4004382200176247093, 0, 0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (0, u(4, 90225674511278402, 0, 0, 0)),
            (0, BACKUP),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (0, RESTORE),
        ],
    ];
    let world = keep(2, &phases).unwrap();
    assert!(world.devices.iter().all(|device| !device.disk.holds(2)));
}

#[test]
fn an_own_lost_create_under_a_folder_deleted_after_the_loss_is_kept() {
    let mut world = converge(2, &[vec![(0, folder("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (1, user(1, 0, 1, "b"))];
    steps.extend(commit(1));
    steps.extend([(1, DELIVER); 4]);
    steps.extend([(0, RESTORE), (1, Step::Connect)]);
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, delete(0)));
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 4]);
    world.phase(1, &steps).unwrap();
}

fn paths_on(world: &World, device: usize) -> Vec<String> {
    let disk = &world.devices[device].disk;
    let mut paths: Vec<String> = (disk.visible().into_iter())
        .filter_map(|ino| disk.path(ino))
        .map(|path| path.join("/"))
        .collect();
    paths.sort();
    paths
}

fn a_folder_made_in_a_folder_a_restore_lost(found_first: bool, delete_it: bool) -> World {
    let mut world = converge(
        2,
        &[
            vec![(0, folder("a"))],
            vec![(0, BACKUP), (0, user(0, 0, 1, "b"))],
        ],
    )
    .unwrap();
    let mut steps = vec![
        (0, delete(0)),
        (0, SCAN),
        (0, RESTORE),
        (1, user(0, 0, 2, "ss")),
    ];
    if found_first {
        steps.push((1, SCAN));
    }
    steps.extend([(0, Step::Connect), (1, Step::Connect)]);
    steps.extend([(0, Step::Work(0)); 12]);
    steps.extend([(1, DELIVER); 8]);
    if delete_it {
        steps.extend([(1, delete(2)), (1, SCAN)]);
    }
    world.phase(2, &steps).unwrap();
    world
}

#[test]
fn a_folder_made_after_a_restore_in_a_folder_the_restore_lost_survives_a_delete_of_its_parent() {
    let world = a_folder_made_in_a_folder_a_restore_lost(true, false);
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), ["a", "a/b", "a/b/ss"]);
    }
}

#[test]
fn a_folder_made_in_a_folder_the_restore_lost_survives_a_delete_of_its_parent_that_lands_first() {
    let world = a_folder_made_in_a_folder_a_restore_lost(false, false);
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), ["a", "a/b", "a/b/ss"]);
    }
}

#[test]
fn a_folder_a_restore_lost_gives_way_to_a_delete_once_the_change_in_it_is_gone() {
    let world = a_folder_made_in_a_folder_a_restore_lost(true, true);
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), Vec::<String>::new());
    }
}

fn a_file_a_restore_lost_moved_before_a_delete_of_its_folder_lands(
    parent: usize,
    name: &str,
) -> World {
    let mut world = converge(
        2,
        &[
            vec![(0, folder("a"))],
            vec![
                (0, BACKUP),
                (0, user(0, 0, 1, "b")),
                (0, user(1, 0, 2, "c.txt")),
            ],
        ],
    )
    .unwrap();
    let mut steps = vec![
        (0, delete(0)),
        (0, SCAN),
        (0, RESTORE),
        (1, user(3, 2, parent, name)),
    ];
    steps.extend([(0, Step::Connect), (1, Step::Connect)]);
    steps.extend([(0, Step::Work(0)); 12]);
    steps.extend([(1, DELIVER); 8]);
    steps.push((1, SCAN));
    world.phase(2, &steps).unwrap();
    world
}

#[test]
fn a_file_a_restore_lost_renamed_before_a_delete_of_its_folder_lands_survives_it() {
    let world = a_file_a_restore_lost_moved_before_a_delete_of_its_folder_lands(2, "b");
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), ["a", "a/b", "a/b/b"]);
    }
}

#[test]
fn a_file_a_restore_lost_moved_out_before_a_delete_of_its_folder_lands_survives_it() {
    let world = a_file_a_restore_lost_moved_before_a_delete_of_its_folder_lands(0, "c.txt");
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), ["c.txt"]);
    }
}

#[test]
fn a_file_a_restore_lost_with_an_edit_not_yet_downloaded_gives_way_to_a_delete_of_its_folder() {
    let world = keep(
        2,
        &[
            vec![(0, folder("a"))],
            vec![
                (0, BACKUP),
                (0, user(0, 0, 1, "b")),
                (0, user(1, 0, 2, "c.txt")),
            ],
            vec![
                (0, overwrite(2)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
                (0, delete(0)),
                (0, SCAN),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
    for device in 0..2 {
        assert_eq!(paths_on(&world, device), Vec::<String>::new());
    }
}

#[test]
fn a_resend_built_before_a_delete_reaches_the_server_yields_to_it() {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((1, COMMIT));
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, delete(0)));
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_resend_applied_before_a_delete_built_without_it_yields_to_it() {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, delete(0)), (0, SCAN), (0, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT), (0, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(1, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_folder_deleted_after_a_restore_stays_deleted_when_another_device_held_its_lost_file() {
    let u = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![
            (1, u(1, 0, 0, 3)),
            (1, u(1, 0, 0, 4)),
            (0, u(0, 0, 0, 0)),
            (2, u(1, 0, 0, 1)),
            (1, u(0, 0, 0, 2)),
            (0, u(2, 0, 0, 0)),
            (0, Step::Backup),
            (0, u(1, 0, 954053144849064707, 0)),
        ],
        vec![
            (0, u(2, 5042142129610238072, 0, 0)),
            (0, Step::Restore),
            (2, Step::Work(0)),
            (0, Step::Connect),
            (2, Step::Work(0)),
            (0, COMMIT),
            (0, Step::Work(7574408442752017209)),
            (2, DELIVER),
            (0, u(3, 331644337139499906, 3731511274330555145, 0)),
            (2, u(4, 328653590929317078, 0, 0)),
            (0, Step::Work(339420830684385840)),
        ],
    ];
    let world = converge(2, &phases).unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .filter(|entry| entry.place == Place::Root)
            .map(|entry| entry.name.as_str())
            .collect();
        assert!(!names.contains(&"a"), "{names:?}");
    }
}

#[test]
fn a_resend_keeps_an_edit_back_to_the_restored_bytes_that_the_server_took_first() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, u(1, 0, 0, 1, 0)),
            (1, u(0, 0, 0, 0, 0)),
            (0, u(1, 0, 0, 0, 0)),
            (2, u(3, 12768519513024442974, 0, 2, 0)),
        ],
        vec![
            (0, Step::Backup),
            (1, u(2, 6880866165967015066, 0, 0, 0)),
            (2, u(0, 0, 4008580898241398591, 0, 0)),
            (1, u(0, 0, 0, 3, 0)),
        ],
        vec![
            (0, u(2, 2682981555425727673, 0, 0, 0)),
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Restore),
        ],
    ];
    keep(2, &phases).unwrap();
}

#[test]
fn an_edit_back_to_the_restored_bytes_beats_another_devices_resend() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(1, u(1, 0, 0, 0, 1))],
        vec![(0, u(2, 0, 0, 0, 0)), (0, Step::Backup)],
        vec![
            (0, Step::Restore),
            (1, Step::Connect),
            (1, Step::Work(0)),
            (1, u(2, 0, 0, 0, 1)),
        ],
    ];
    keep(3, &phases).unwrap();
}

#[test]
fn an_edit_back_to_the_restored_bytes_after_another_edit_of_a_resend_beats_the_authors_resend() {
    let write = |kind, text| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name: 0,
            text,
        })
    };
    let phases = [
        vec![(0, write(1, 3))],
        vec![(0, BACKUP), (1, write(2, 0))],
        vec![
            (0, write(2, 0)),
            (0, RESTORE),
            (0, Step::Connect),
            (0, DELIVER),
            (0, Step::Scan),
            (0, write(2, 3)),
        ],
    ];
    converge(3, &phases).unwrap();
}

fn send_a_lost_write_again_from_two_devices_after_a_later_edit(author_first: bool) {
    let edit = |device, text| {
        let action = Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text,
        };
        (device, Step::User(action))
    };
    let mut world = converge(3, &[vec![(1, file("a"))], vec![edit(0, 1), (0, BACKUP)]]).unwrap();
    let lost = (world.devices[2].disk.entries().values())
        .find_map(|entry| entry.content.clone())
        .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push(edit(1, 2));
    steps.extend(commit(1));
    steps.extend([(1, DELIVER); 4]);
    let order = if author_first { [0, 2] } else { [2, 0] };
    for device in order {
        steps.push((device, Step::Connect));
        steps.extend([(device, DELIVER); 8]);
        steps.extend(commit(device));
        steps.extend([(device, DELIVER); 4]);
    }
    world.phase(2, &steps).unwrap();
    for device in &world.devices {
        let entries: Vec<_> = device.disk.entries().values().collect();
        let copies = (entries.iter())
            .filter(|entry| entry.content.as_ref() == Some(&lost))
            .count();
        assert_eq!((entries.len(), copies), (2, 1), "{entries:?}");
    }
}

#[test]
fn a_lost_write_two_devices_send_again_after_a_later_edit_leaves_one_copy() {
    send_a_lost_write_again_from_two_devices_after_a_later_edit(true);
}

#[test]
fn a_lost_write_sent_again_by_another_device_first_leaves_one_copy() {
    send_a_lost_write_again_from_two_devices_after_a_later_edit(false);
}

#[test]
fn a_lost_write_kept_beside_a_later_edit_survives_a_delete_of_the_copy_on_another_device() {
    let edit = |device, text| {
        let action = Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text,
        };
        (device, Step::User(action))
    };
    let mut world = converge(3, &[vec![(1, file("a"))], vec![edit(0, 1), (0, BACKUP)]]).unwrap();
    let lost = (world.devices[2].disk.entries().values())
        .find_map(|entry| entry.content.clone())
        .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push(edit(1, 2));
    steps.extend(commit(1));
    steps.extend([(1, DELIVER); 4]);
    for device in [0, 2] {
        steps.push((device, Step::Connect));
        steps.extend([(device, DELIVER); 8]);
    }
    steps.extend(commit(0));
    steps.extend([(0, DELIVER); 4]);
    steps.extend([(2, DELIVER); 8]);
    steps.extend([(2, EFFECT); 4]);
    steps.push((2, SCAN));
    world.run(2, &steps).unwrap();
    let disk = &world.devices[2].disk;
    let copy = (disk.visible().iter())
        .position(|ino| disk.entries()[ino].name != "a")
        .unwrap();
    let mut delete = vec![(2, user(4, copy, 0, "a"))];
    delete.extend(commit(2));
    delete.extend([(2, DELIVER); 4]);
    world.phase(3, &delete).unwrap();
    for device in &world.devices {
        let entries: Vec<_> = device.disk.entries().values().collect();
        let copies = (entries.iter())
            .filter(|entry| entry.content.as_ref() == Some(&lost))
            .count();
        assert_eq!(copies, 1, "{entries:?}");
    }
}

#[test]
fn the_author_of_a_lost_first_edit_sends_it_again_at_its_name() {
    let mut world = converge(
        2,
        &[vec![(1, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["a"]);
    }
}

fn a_lost_edit_three_devices_held(lost: Step) -> World {
    converge(3, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, lost)]]).unwrap()
}

fn connect(devices: &[usize]) -> Vec<(usize, Step)> {
    let mut steps = vec![(0, RESTORE)];
    for &device in devices {
        steps.push((device, Step::Connect));
        steps.extend([(device, DELIVER); 8]);
    }
    steps
}

fn every_device_holds_a_file(world: &World) -> bool {
    (world.devices.iter()).all(|device| !device.disk.entries().is_empty())
}

fn no_device_holds_a_file(world: &World) -> bool {
    (world.devices.iter()).all(|device| device.disk.entries().is_empty())
}

#[test]
fn the_authors_edit_sent_after_another_devices_resend_beats_a_delete_built_before_both() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.push((2, COMMIT));
    world.phase(2, &steps).unwrap();
    assert!(every_device_holds_a_file(&world));
}

#[test]
fn the_authors_edit_sent_after_another_devices_resend_revives_the_file_a_delete_trashed() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.push((2, COMMIT));
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    world.phase(2, &steps).unwrap();
    assert!(every_device_holds_a_file(&world));
}

#[test]
fn the_authors_lost_edit_revives_a_file_deleted_at_the_restored_version() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let original = world
        .space()
        .nodes()
        .into_iter()
        .find(|node| node.kind == koidrive_proto::v1::NodeKind::File as i32)
        .unwrap()
        .node_id;
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT), (2, COMMIT)]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    world.phase(2, &steps).unwrap();
    for device in &world.devices {
        let contents: Vec<_> = device
            .disk
            .entries()
            .values()
            .filter_map(|entry| entry.content.as_ref().map(|content| content.mtime_ns))
            .collect();
        assert_eq!(contents, [2]);
    }
    let live: Vec<_> = world
        .space()
        .nodes()
        .into_iter()
        .filter(|node| {
            node.kind == koidrive_proto::v1::NodeKind::File as i32
                && node.status == koidrive_proto::v1::NodeStatus::Live as i32
        })
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].node_id, original);
}

#[test]
fn a_delete_at_the_restored_version_stays_deleted_without_the_authors_edit() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(0, delete(0)), (0, SCAN)]);
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT), (2, COMMIT)]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world));
}

#[test]
fn an_unasked_delete_of_restored_content_is_rejected_after_an_older_overwrite() {
    let mut world = converge(
        3,
        &[
            vec![(0, file("a"))],
            vec![(1, overwrite(0))],
            vec![(2, overwrite(0))],
            vec![(0, BACKUP), (0, overwrite(0))],
        ],
    )
    .unwrap();
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(1, SCAN), (1, COMMIT)]);
    world.run(4, &steps).unwrap();
    let device = &mut world.devices[1];
    let mut state = device.engine.commits.state();
    let op = state
        .in_flight
        .as_mut()
        .unwrap()
        .commit
        .ops
        .iter_mut()
        .find(|op| matches!(op.op, Some(koidrive_proto::v1::op::Op::UpdateContent(_))))
        .unwrap();
    let Some(koidrive_proto::v1::op::Op::UpdateContent(update)) = &op.op else {
        unreachable!()
    };
    assert!(update.resend);
    op.op = Some(koidrive_proto::v1::op::Op::Delete(
        koidrive_proto::v1::Delete {
            node_id: update.node_id.clone(),
            requires: None,
        },
    ));
    device.engine.commits = koidrive_engine::commit::Commits::from_state(state).unwrap();
    device.store = device.engine.clone();
    world.run(4, &[(1, COMMIT)]).unwrap();
    let failure = world.phase(4, &[]).unwrap_err();
    assert!(
        matches!(failure, Failure::Diverged(why) if why.contains("took write 3 out of the server's tree"))
    );
}

#[test]
fn the_authors_resent_edit_beats_a_delete_built_before_it() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.push((2, COMMIT));
    world.phase(2, &steps).unwrap();
    assert!(every_device_holds_a_file(&world));
}

fn a_delete_pulled_after_the_authors_edit(resend_first: bool) -> World {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN)]);
    if resend_first {
        steps.extend([(1, COMMIT), (1, COMMIT)]);
    }
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.extend([(2, DELIVER); 8]);
    steps.extend(commit(2));
    world.phase(2, &steps).unwrap();
    world
}

fn an_owed_rename_in_flight_while_another_device_changes_it(change: Step) -> World {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([(1, change), (1, SCAN)]);
    steps.extend([(1, Step::Work(0)); 4]);
    steps.extend([(0, Step::Work(0)); 16]);
    world.phase(1, &steps).unwrap();
    world
}

#[test]
fn a_device_that_deleted_the_file_keeps_the_authors_edit_that_follows_a_resend_it_planned_against()
{
    assert!(every_device_holds_a_file(
        &a_delete_pulled_after_the_authors_edit(true)
    ));
}

#[test]
fn a_device_that_deleted_the_file_keeps_the_authors_resent_edit_it_pulled() {
    assert!(every_device_holds_a_file(
        &a_delete_pulled_after_the_authors_edit(false)
    ));
}

#[test]
fn a_device_that_deleted_the_file_keeps_the_authors_edit_sent_after_the_resend_reached_the_author()
{
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(2, delete(0)), (2, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(2, DELIVER); 8]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.extend([(2, DELIVER); 8]);
    steps.extend(commit(2));
    world.phase(2, &steps).unwrap();
    assert!(every_device_holds_a_file(&world));
}

#[test]
fn a_delete_planned_against_a_resend_it_pulled_trashes_the_file() {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, delete(0)), (0, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world));
}

#[test]
#[ignore = "a resent rename beats a delete built without it (GUN-580)"]
fn a_resent_rename_yields_to_a_delete_built_without_it() {
    let mut world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (0, rename(0, 0, "b"))],
        ],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, delete(0)), (0, SCAN), (0, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT), (0, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(1, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world));
}

#[test]
fn the_author_deleting_its_own_edit_after_another_devices_resend_keeps_the_delete() {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut steps = connect(&[1, 0, 2]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.extend([(0, delete(0)), (0, SCAN)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world));
}

fn the_authors_edit_after_two_losses(resend_first: bool) -> World {
    let mut world = a_lost_edit_three_devices_held(overwrite(0));
    let mut first = connect(&[1, 0, 2]);
    if resend_first {
        first.extend([(1, COMMIT), (1, COMMIT)]);
    }
    first.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    first.extend([(0, DELIVER); 8]);
    first.extend([(1, DELIVER); 8]);
    first.extend([(2, DELIVER); 8]);
    world.run(2, &first).unwrap();
    world.run(3, &connect(&[0, 2])).unwrap();
    let mut second = vec![(2, delete(0)), (2, SCAN), (2, COMMIT)];
    second.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    second.push((2, COMMIT));
    world.phase(3, &second).unwrap();
    world
}

#[test]
fn the_authors_edit_recorded_after_another_devices_resend_survives_a_second_loss_and_a_delete() {
    assert!(every_device_holds_a_file(
        &the_authors_edit_after_two_losses(true)
    ));
}

#[test]
fn the_authors_edit_survives_a_second_loss_and_a_delete() {
    assert!(every_device_holds_a_file(
        &the_authors_edit_after_two_losses(false)
    ));
}

fn an_idle_device_over_a_resend_it_pulled(deleted_before: bool) {
    let user = |kind| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        })
    };
    let mut world = World::new(3);
    world.phase(0, &[(0, user(1))]).unwrap();
    if deleted_before {
        world.phase(10, &[(1, user(2))]).unwrap();
        world.phase(11, &[(0, user(2))]).unwrap();
    }
    let steps = [
        (2, user(2)),
        (0, user(0)),
        (0, BACKUP),
        (2, Step::Work(0)),
        (0, Step::Work(0)),
        (2, Step::Work(0)),
        (2, COMMIT),
        (0, Step::Work(0)),
        (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
        (0, RESTORE),
    ];
    world.phase(1, &steps).unwrap();
}

#[test]
fn an_idle_device_sends_nothing_over_a_resend_it_pulled() {
    an_idle_device_over_a_resend_it_pulled(false);
}

#[test]
fn an_idle_device_whose_user_once_deleted_an_older_write_sends_nothing_over_a_resend() {
    an_idle_device_over_a_resend_it_pulled(true);
}

fn files_on(world: &World, device: usize) -> Vec<(String, Option<Content>)> {
    let mut files: Vec<_> = (world.devices[device].disk.entries().values())
        .map(|entry| (entry.name.clone(), entry.content.clone()))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn content_on(world: &World, device: usize, name: &str) -> Content {
    (world.devices[device].disk.entries().values())
        .find(|entry| entry.name == name)
        .and_then(|entry| entry.content.clone())
        .unwrap()
}

fn reconnect(steps: &mut Vec<(usize, Step)>, device: usize, commits: bool) {
    steps.push((device, Step::Connect));
    steps.extend([(device, DELIVER); 8]);
    if commits {
        steps.extend(commit(device));
        steps.extend([(device, DELIVER); 4]);
    }
}

fn every_device_holds(world: &World, contents: &[&Content]) {
    for device in 0..world.devices.len() {
        let files = files_on(world, device);
        for content in contents {
            assert!(
                files
                    .iter()
                    .any(|(_, held)| held.as_ref() == Some(*content)),
                "device {device} lost a write: {files:?}"
            );
        }
    }
}

#[test]
fn the_authors_lost_edit_over_another_devices_version_is_sent_again_at_its_name() {
    let mut world = converge(
        2,
        &[
            vec![(1, file("a"))],
            vec![(0, BACKUP), (1, overwrite(0))],
            vec![(0, overwrite(0))],
        ],
    )
    .unwrap();
    let edit = content_on(&world, 0, "a");
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(3, &steps).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("a".to_owned(), Some(edit.clone()))]
        );
    }
}

#[test]
fn the_authors_lost_edit_is_sent_again_at_its_name_after_each_of_three_losses() {
    let mut world = converge(
        2,
        &[vec![(1, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let edit = content_on(&world, 0, "a");
    for phase in 2..5 {
        let mut steps = vec![(0, RESTORE)];
        reconnect(&mut steps, 0, true);
        reconnect(&mut steps, 1, false);
        world.phase(phase, &steps).unwrap();
        for device in 0..2 {
            assert_eq!(
                files_on(&world, device),
                [("a".to_owned(), Some(edit.clone()))],
                "after loss {}",
                phase - 1
            );
        }
    }
}

#[test]
fn an_edit_made_between_two_losses_is_sent_again_at_its_name_after_the_second() {
    let mut world = converge(
        2,
        &[vec![(1, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, false);
    world.phase(2, &steps).unwrap();
    world.phase(3, &[(1, overwrite(0))]).unwrap();
    let edit = content_on(&world, 1, "a");
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(4, &steps).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("a".to_owned(), Some(edit.clone()))]
        );
    }
}

#[test]
fn the_authors_lost_edit_is_sent_again_at_its_name_after_a_restore_to_an_earlier_backup() {
    let mut world = converge(
        2,
        &[
            vec![(1, file("a"))],
            vec![(0, BACKUP), (1, overwrite(0))],
            vec![(0, BACKUP), (0, overwrite(0))],
        ],
    )
    .unwrap();
    let edit = content_on(&world, 0, "a");
    for (phase, restore) in [(3, RESTORE), (4, Step::RestoreEarlier)] {
        let mut steps = vec![(0, restore)];
        reconnect(&mut steps, 0, true);
        reconnect(&mut steps, 1, true);
        world.phase(phase, &steps).unwrap();
        for device in 0..2 {
            assert_eq!(
                files_on(&world, device),
                [("a".to_owned(), Some(edit.clone()))],
                "phase {phase}"
            );
        }
    }
}

#[test]
fn an_author_that_edits_again_before_it_reconnects_keeps_the_lost_edit_beside_the_new_one() {
    let mut world = converge(
        2,
        &[vec![(1, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let lost = content_on(&world, 0, "a");
    world.run(2, &[(0, RESTORE), (0, overwrite(0))]).unwrap();
    let edit = content_on(&world, 0, "a");
    let mut steps = Vec::new();
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, false);
    world.phase(3, &steps).unwrap();
    every_device_holds(&world, &[&lost, &edit]);
    for device in 0..2 {
        let files = files_on(&world, device);
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(content_on(&world, device, "a"), edit);
    }
}

#[test]
fn an_author_that_crashes_while_it_sends_a_lost_edit_again_sends_it_at_its_name() {
    let mut world = converge(
        2,
        &[vec![(1, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let edit = content_on(&world, 0, "a");
    let mut steps = vec![(0, RESTORE), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, SCAN), (0, COMMIT), (0, Step::Crash), (0, RESTORE)]);
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, false);
    world.phase(2, &steps).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("a".to_owned(), Some(edit.clone()))]
        );
    }
}

#[test]
fn a_file_another_device_edits_and_deletes_across_a_restore_stays_deleted() {
    let user = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let created = vec![
        (1, user(1, 0, 0, 0)),
        (0, user(1, 0, 0, 1)),
        (0, user(0, 0, 0, 2)),
        (1, user(1, 0, 0, 1)),
        (2, user(0, 0, 8070642558861201026, 0)),
    ];
    let lost = vec![
        (0, user(2, 14626354869250174423, 0, 0)),
        (1, user(0, 0, 0, 0)),
        (1, Step::Work(0)),
        (1, Step::Work(0)),
        (0, Step::Work(0)),
        (2, Step::Work(0)),
        (2, Step::Work(0)),
        (2, Step::Deliver(0)),
        (0, BACKUP),
        (1, Step::Work(10555856991292968265)),
        (1, Step::Work(8608730646068491420)),
        (2, user(4, 8124263451402198873, 0, 0)),
        (0, RESTORE),
    ];
    converge(2, &[created, lost]).unwrap();
}

#[test]
fn a_file_another_device_edited_before_a_restore_and_deleted_after_it_stays_deleted() {
    let user = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let created = vec![
        (1, user(1, 0, 0, 0)),
        (0, user(1, 0, 0, 1)),
        (0, user(0, 0, 0, 2)),
        (1, user(1, 0, 0, 1)),
        (2, user(0, 0, 8070642558861201026, 0)),
    ];
    let lost = vec![
        (0, user(2, 14626354869250174423, 0, 0)),
        (1, user(0, 0, 0, 0)),
        (1, Step::Work(63779932184036)),
        (1, Step::Work(12097345643055467446)),
        (0, Step::Work(6711461528218520286)),
        (2, Step::Work(16636816709496478910)),
        (2, Step::Work(563873543365864804)),
        (2, Step::Deliver(4161883621331327623)),
        (0, BACKUP),
        (1, Step::Work(13190423102586310382)),
        (1, Step::Work(14288492056209541958)),
        (1, Step::Work(12049626435681642106)),
        (2, user(4, 8270141932729720698, 11291193124424285686, 2)),
        (0, RESTORE),
    ];
    keep(2, &[created, lost]).unwrap();
}

#[test]
fn a_file_another_device_renamed_and_edited_before_a_restore_and_deleted_after_it_stays_deleted() {
    let user = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let created = vec![
        (1, user(1, 0, 0, 0, 0)),
        (0, user(1, 0, 0, 1, 0)),
        (0, user(0, 0, 0, 2, 0)),
        (1, user(1, 0, 0, 1, 0)),
        (2, user(0, 0, 8070642558861201026, 0, 0)),
    ];
    let lost = vec![
        (0, user(2, 14626354869250174423, 0, 0, 0)),
        (1, user(0, 0, 0, 0, 0)),
        (1, Step::Work(127559864368073)),
        (1, Step::Work(12097345643055467446)),
        (2, user(3, 12440659901360184398, 381004134453766589, 2, 2)),
        (0, Step::Work(6711461528218520286)),
        (2, Step::Work(16636816709496478910)),
        (2, Step::Work(563873543365864804)),
        (2, Step::Deliver(4161883621331327623)),
        (0, BACKUP),
        (1, Step::Work(13190423102586310382)),
        (1, Step::Work(14288492056209541958)),
        (1, Step::Work(12049626435681642106)),
        (2, user(4, 8270141932729720698, 11291193124424285686, 2, 0)),
        (0, RESTORE),
    ];
    keep(2, &[created, lost]).unwrap();
}

#[test]
fn a_file_another_device_edited_below_a_folder_renamed_in_a_lost_commit_stays_deleted() {
    let synced = vec![(0, folder("b")), (0, user(1, 0, 1, "a"))];
    let mut lost = vec![(1, overwrite(1))];
    lost.extend(commit(1));
    lost.extend([
        (0, DELIVER),
        (0, DELIVER),
        (0, BACKUP),
        (0, rename(0, 0, "ss")),
    ]);
    lost.extend(commit(0));
    lost.extend([(1, delete(1)), (0, RESTORE)]);
    let world = keep(2, &[synced, lost]).unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["ss"], "device {d}");
    }
}

#[test]
fn a_file_another_device_edited_and_renamed_in_a_lost_commit_keeps_its_new_name() {
    let synced = vec![(0, file("a"))];
    let mut lost = vec![(1, overwrite(0))];
    lost.extend(commit(1));
    lost.extend([
        (0, DELIVER),
        (0, DELIVER),
        (0, BACKUP),
        (0, rename(0, 0, "b")),
    ]);
    lost.extend(commit(0));
    lost.push((0, RESTORE));
    let world = keep(2, &[synced, lost]).unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"], "device {d}");
    }
}

#[test]
fn a_file_another_device_edited_and_moved_into_a_new_folder_in_a_lost_commit_stays_there() {
    let synced = vec![(0, file("a"))];
    let mut lost = vec![(1, overwrite(0))];
    lost.extend(commit(1));
    lost.extend([
        (0, DELIVER),
        (0, DELIVER),
        (0, BACKUP),
        (0, folder("ss")),
        (0, rename(0, 1, "a")),
    ]);
    lost.extend(commit(0));
    lost.push((0, RESTORE));
    let world = keep(2, &[synced, lost]).unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let entries = device.disk.entries();
        assert_eq!(entries.len(), 2, "device {d}");
        let moved = (entries.values()).find(|entry| entry.name == "a").unwrap();
        let Place::In(folder) = moved.place else {
            panic!("device {d} holds a in the root");
        };
        assert_eq!(entries[&folder].name, "ss", "device {d}");
    }
}

#[test]
fn a_file_another_device_made_and_edited_keeps_the_name_of_this_devices_first_lost_commit() {
    let synced = vec![(1, file("a"))];
    let mut lost = vec![(1, overwrite(0))];
    lost.extend(commit(1));
    lost.extend([
        (0, DELIVER),
        (0, DELIVER),
        (0, BACKUP),
        (0, rename(0, 0, "b")),
    ]);
    lost.extend(commit(0));
    lost.push((0, RESTORE));
    let world = keep(2, &[synced, lost]).unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"], "device {d}");
    }
}

#[test]
fn a_write_on_a_third_device_reaches_the_server_around_an_owed_rename() {
    let user = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (2, Step::Work(0)),
        (2, Step::Work(0)),
        (0, Step::Connect),
        (2, Step::Work(0)),
        (2, user(4, 1, 0, 0)),
        (0, user(0, 0, 2651832725630026939, 1)),
        (0, user(0, 0, 0, 0)),
        (0, user(0, 0, 0, 0)),
        (0, user(0, 0, 0, 0)),
        (0, user(0, 0, 0, 0)),
        (2, Step::Work(0)),
        (0, user(0, 0, 0, 0)),
        (0, user(0, 0, 0, 0)),
        (2, user(1, 0, 0, 3)),
        (0, Step::Work(0)),
        (2, user(3, 7392438467146408886, 0, 2)),
        (2, user(0, 0, 0, 0)),
        (2, Step::Work(3624270594846902841)),
        (2, user(1, 0, 64282158667723573, 0)),
        (2, Step::Work(99159055136200079)),
        (2, Step::Work(2169569474900780673)),
    ]);
    converge(3, &[vec![(0, file("a"))], steps]).unwrap();
}

fn ids_minted_by_device_1(count: usize, entries: usize) -> Vec<Vec<(usize, Step)>> {
    (0..count)
        .flat_map(|_| [vec![(1, file("ss"))], vec![(1, delete(entries))]])
        .collect()
}

fn a_new_folder_merged_into_a_renamed_folder_its_commit_deletes(
    renamed: &str,
    made: &str,
    synced: &[Step],
    inside: &[Step],
) {
    for ids_minted_before in 0..=4 {
        let mut first = vec![(0, folder("b"))];
        first.extend(synced.iter().map(|step| (0, *step)));
        let mut phases = vec![first];
        phases.extend(ids_minted_by_device_1(ids_minted_before, 1 + synced.len()));
        let mut racing = vec![(0, rename(0, 0, renamed))];
        racing.extend(commit(0));
        racing.extend([(1, delete(0)), (1, folder(made))]);
        racing.extend(inside.iter().map(|step| (1, *step)));
        racing.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
        phases.push(racing);
        if let Err(failure) = converge(2, &phases) {
            panic!("with {ids_minted_before} IDs minted before: {failure:?}");
        }
    }
}

#[test]
fn a_file_made_in_a_new_folder_merged_into_a_folder_its_commit_deletes_reaches_the_server() {
    a_new_folder_merged_into_a_renamed_folder_its_commit_deletes(
        "a",
        "A",
        &[],
        &[user(1, 0, 1, "c.txt")],
    );
}

#[test]
fn a_file_made_in_a_new_folder_whose_name_folds_to_a_folder_its_commit_deletes_reaches_the_server()
{
    a_new_folder_merged_into_a_renamed_folder_its_commit_deletes(
        "ß",
        "ss",
        &[],
        &[user(1, 0, 1, "c.txt")],
    );
}

#[test]
fn a_file_made_below_a_new_folder_merged_into_a_folder_its_commit_deletes_reaches_the_server() {
    a_new_folder_merged_into_a_renamed_folder_its_commit_deletes(
        "a",
        "a",
        &[],
        &[user(0, 0, 1, "b"), user(1, 0, 2, "c.txt")],
    );
}

#[test]
fn a_file_moved_into_a_new_folder_merged_into_a_folder_its_commit_deletes_stays_live() {
    a_new_folder_merged_into_a_renamed_folder_its_commit_deletes(
        "a",
        "a",
        &[file("c.txt")],
        &[rename(0, 1, "c.txt")],
    );
}

#[test]
fn a_file_made_in_a_new_folder_nested_in_a_merged_folder_reaches_the_server() {
    for ids_minted_before in 0..=9 {
        let mut phases = vec![vec![(0, folder("b")), (0, user(0, 0, 1, "b"))]];
        phases.extend(ids_minted_by_device_1(ids_minted_before, 2));
        let mut racing = vec![(0, rename(0, 0, "a")), (0, rename(1, 1, "ss"))];
        racing.extend(commit(0));
        racing.extend([
            (1, delete(1)),
            (1, folder("A")),
            (1, user(0, 0, 2, "ß")),
            (1, user(1, 0, 3, "c.txt")),
            (1, SCAN),
            (1, COMMIT),
            (1, COMMIT),
        ]);
        phases.push(racing);
        if let Err(failure) = converge(2, &phases) {
            panic!("with {ids_minted_before} IDs minted before: {failure:?}");
        }
    }
}

#[test]
fn a_file_made_in_a_new_folder_inside_a_new_folder_merged_into_a_renamed_folder_reaches_the_server()
{
    for ids_minted_before in 0..=4 {
        let mut phases = vec![vec![(0, folder("b")), (0, user(0, 0, 1, "A"))]];
        phases.extend(ids_minted_by_device_1(ids_minted_before, 2));
        let mut racing = vec![(0, rename(0, 0, "a"))];
        racing.extend(commit(0));
        racing.extend([
            (1, delete(1)),
            (1, folder("a")),
            (1, user(0, 0, 2, "A")),
            (1, user(1, 0, 3, "c.txt")),
            (1, SCAN),
            (1, COMMIT),
            (1, COMMIT),
        ]);
        phases.push(racing);
        if let Err(failure) = converge(2, &phases) {
            panic!("with {ids_minted_before} IDs minted before: {failure:?}");
        }
    }
}

#[test]
fn a_folder_renamed_to_the_name_of_a_folder_deleted_beside_a_new_folder_keeps_its_name() {
    let world = converge(
        2,
        &[
            vec![(0, folder("b")), (0, folder("ss"))],
            vec![
                (0, delete(0)),
                (0, rename(0, 0, "b")),
                (0, folder("a")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
            ],
        ],
    )
    .unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let mut names: Vec<String> = (device.disk.entries().values())
            .map(|entry| entry.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b"], "device {d}");
    }
}

#[test]
fn a_file_moved_out_of_a_folder_into_a_new_folder_merged_into_it_stays_live() {
    for ids_minted_before in 0..=4 {
        let mut phases = vec![vec![(0, folder("b")), (0, user(1, 0, 1, "c.txt"))]];
        phases.extend(ids_minted_by_device_1(ids_minted_before, 2));
        let mut racing = vec![(0, rename(0, 0, "a"))];
        racing.extend(commit(0));
        racing.extend([
            (1, folder("A")),
            (1, rename(1, 2, "c.txt")),
            (1, delete(0)),
            (1, SCAN),
            (1, COMMIT),
            (1, COMMIT),
        ]);
        phases.push(racing);
        if let Err(failure) = converge(2, &phases) {
            panic!("with {ids_minted_before} IDs minted before: {failure:?}");
        }
    }
}

#[test]
fn a_folder_renamed_onto_a_deleted_folder_whose_file_moved_into_a_new_folder_keeps_its_name() {
    let world = converge(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "c.txt")),
                (0, folder("ss")),
            ],
            vec![
                (0, folder("a")),
                (0, rename(1, 3, "c.txt")),
                (0, delete(0)),
                (0, rename(1, 0, "b")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
            ],
        ],
    )
    .unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let mut names: Vec<String> = (device.disk.entries().values())
            .map(|entry| entry.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b", "c.txt"], "device {d}");
    }
}

#[test]
fn a_file_made_at_the_name_of_a_deleted_folder_whose_file_moved_into_a_new_folder_keeps_its_name() {
    let world = converge(
        2,
        &[
            vec![(0, folder("b")), (0, user(1, 0, 1, "c.txt"))],
            vec![
                (0, folder("a")),
                (0, rename(1, 2, "c.txt")),
                (0, delete(0)),
                (0, file("b")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
            ],
        ],
    )
    .unwrap();
    for (d, device) in world.devices.iter().enumerate() {
        let mut names: Vec<String> = (device.disk.entries().values())
            .map(|entry| entry.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b", "c.txt"], "device {d}");
    }
}

fn a_lost_create_its_author_sends_again_under_a_new_name(download: Step) -> World {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(0, BACKUP)],
        vec![
            (2, u(1, 0, 0, 2, 0)),
            (0, Step::Work(0)),
            (0, u(3, 0, 0, 0, 0)),
            (0, Step::Work(1130995114980569936)),
            (2, Step::Work(9668427609435895111)),
            (0, Step::Work(0)),
            (2, DELIVER),
            (1, DELIVER),
            (1, download),
            (2, Step::Work(0)),
            (0, COMMIT),
            (0, RESTORE),
        ],
    ];
    let world = converge(2, &phases).unwrap();
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["a"]);
    }
    world
}

#[test]
fn a_lost_create_sent_again_under_a_new_name_is_taken_by_a_device_that_downloaded_it() {
    a_lost_create_its_author_sends_again_under_a_new_name(EFFECT);
}

#[test]
fn a_lost_rename_can_resend_the_conflict_name_its_own_commit_received() {
    converge(
        3,
        &[
            vec![(0, file("b")), (0, BACKUP)],
            vec![
                (2, BACKUP),
                (2, rename(0, 0, "a")),
                (2, SCAN),
                (2, COMMIT),
                (2, COMMIT),
                (2, rename(0, 0, "a")),
                (0, folder("a")),
                (2, RESTORE),
            ],
            vec![
                (0, rename(0, 0, "a")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, rename(0, 0, "b")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_lost_create_sent_again_under_a_new_name_is_taken_by_a_device_whose_download_a_crash_cut() {
    a_lost_create_its_author_sends_again_under_a_new_name(Step::Effect(Outcome::Crash(
        Cut::Placed,
    )));
}

#[test]
#[ignore = "a device resends the position its disk holds, not the one it knew the server held (GUN-642)"]
fn a_second_loss_before_the_download_moves_the_file_keeps_the_authors_name() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (0, RESTORE)]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    world.phase(1, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn a_pending_file_whose_name_its_conflict_copy_takes_is_renamed_by_the_device_that_marked_it() {
    converge(
        3,
        &[
            vec![
                (1, Step::Work(16274642324747499124)),
                (1, Step::Connect),
                (0, Step::Effect(Outcome::Run)),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 4540723837931170798,
                        parent: 13161747283576780999,
                        name: 5,
                        text: 3,
                    }),
                ),
                (0, Step::Scan),
                (1, Step::Work(14001882461254173322)),
                (1, Step::Deliver(10280263283177231077)),
                (0, Step::Effect(Outcome::Crash(Cut::Placed))),
                (0, Step::Work(3150538074527436651)),
                (1, Step::Drop),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Commit(Reply::Served)),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 3074524351106235926,
                        parent: 15622149868922993437,
                        name: 5,
                        text: 0,
                    }),
                ),
                (2, Step::Work(12709215057847084410)),
                (2, Step::Work(13195197093511105023)),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 2007127410610402069,
                        parent: 14009461238078843770,
                        name: 2,
                        text: 3,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 4636927101850804754,
                        parent: 7194277078085297665,
                        name: 2,
                        text: 1,
                    }),
                ),
                (0, Step::Work(8321255590853366247)),
                (2, Step::Scan),
                (1, Step::Effect(Outcome::Crash(Cut::Done))),
                (2, Step::Work(13496689005560533593)),
                (0, Step::Drop),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 966963481405597494,
                        parent: 8245514377295365874,
                        name: 5,
                        text: 0,
                    }),
                ),
                (
                    1,
                    Step::User(Action {
                        kind: 1,
                        target: 225941396932957162,
                        parent: 14792149289048557706,
                        name: 3,
                        text: 0,
                    }),
                ),
                (0, Step::Commit(Reply::Served)),
                (1, Step::Commit(Reply::NoChunks)),
                (0, Step::Drop),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 9006010326599423669,
                        parent: 12030360673653429816,
                        name: 6,
                        text: 1,
                    }),
                ),
                (0, Step::Crash),
                (0, Step::Work(10532977848492514630)),
                (0, Step::Deliver(9482183767260581062)),
                (2, Step::Scan),
                (2, Step::Work(7987139506101263081)),
                (0, Step::Work(10342087221633046513)),
                (1, Step::Commit(Reply::Served)),
                (1, Step::Work(6790053865611655878)),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 7525507307544980790,
                        parent: 11794627914537811387,
                        name: 3,
                        text: 3,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 4222423626720987175,
                        parent: 7107160187266153721,
                        name: 0,
                        text: 2,
                    }),
                ),
            ],
            vec![
                (1, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                (1, Step::Effect(Outcome::Fail)),
                (0, Step::Commit(Reply::Served)),
                (1, Step::Work(14776020963067605780)),
                (1, Step::Duplicate(5337902825096339581)),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 13901156469230108672,
                        parent: 11150338100498388762,
                        name: 2,
                        text: 3,
                    }),
                ),
                (2, Step::Work(7038216941225261646)),
                (0, Step::Work(2320670095518912407)),
                (2, Step::Effect(Outcome::Run)),
                (2, Step::Work(6702283563481621023)),
                (0, Step::Effect(Outcome::Crash(Cut::Placed))),
                (2, Step::Work(9488113402077087457)),
                (0, Step::Work(918825349661777991)),
                (2, Step::Deliver(7230566688530807512)),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 11435112669801920197,
                        parent: 1797153937022296474,
                        name: 4,
                        text: 2,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 7120082117506032661,
                        parent: 6750420819364943549,
                        name: 0,
                        text: 1,
                    }),
                ),
                (1, Step::Commit(Reply::Served)),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Work(8407811867281122132)),
                (2, Step::Commit(Reply::Served)),
                (2, Step::Work(1884201276037343489)),
                (1, Step::Work(7135757195114882032)),
                (2, Step::Work(14687530289417874012)),
                (2, Step::Work(2956825658628593075)),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 6865901930919790778,
                        parent: 8288472523108843720,
                        name: 2,
                        text: 0,
                    }),
                ),
                (0, Step::Work(5124846217184249120)),
                (0, Step::Deliver(10658715262855600137)),
                (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
                (2, Step::Work(15438777473916274418)),
                (2, Step::Commit(Reply::Served)),
                (1, Step::Crash),
                (0, Step::Effect(Outcome::Crash(Cut::Done))),
                (
                    1,
                    Step::User(Action {
                        kind: 2,
                        target: 17703314385067399982,
                        parent: 17656894473945846841,
                        name: 5,
                        text: 0,
                    }),
                ),
                (0, Step::Connect),
                (2, Step::Duplicate(15079964782730117166)),
                (0, Step::Work(17321021939675765847)),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 3475983964813192413,
                        parent: 3142939760521600814,
                        name: 1,
                        text: 1,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 1603506240037526712,
                        parent: 12162886444757042084,
                        name: 1,
                        text: 0,
                    }),
                ),
                (0, Step::Work(11201822032332675027)),
            ],
            vec![
                (2, Step::Scan),
                (2, Step::Crash),
                (2, Step::Drop),
                (0, Step::Work(6015854605376741563)),
                (2, Step::Work(14327447256282067683)),
                (2, Step::Work(13842812011217849245)),
                (1, Step::Work(14146307605526031779)),
                (2, Step::Duplicate(16712681527797065823)),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 337520903820340153,
                        parent: 3629187529615689769,
                        name: 3,
                        text: 0,
                    }),
                ),
                (0, Step::Scan),
                (0, Step::Effect(Outcome::Run)),
                (2, Step::Crash),
                (2, Step::Duplicate(18096242046457636208)),
                (1, Step::Deliver(736796987841017770)),
                (0, Step::Commit(Reply::Served)),
                (0, Step::Work(13078245784386167280)),
                (0, Step::Effect(Outcome::Run)),
                (2, Step::Connect),
                (1, Step::Effect(Outcome::Run)),
                (0, Step::Scan),
                (0, Step::Deliver(3633961901045387142)),
                (0, Step::Connect),
                (1, Step::Work(5790155795775849372)),
                (1, Step::Work(17994945317265741320)),
                (0, Step::Commit(Reply::NoChunks)),
                (1, Step::Commit(Reply::Served)),
                (2, Step::Work(11664352604933389001)),
                (1, Step::Deliver(3379023900342304746)),
                (0, Step::Scan),
                (1, Step::Drop),
                (1, Step::Work(4608420094548971993)),
                (1, Step::Scan),
                (0, Step::Deliver(8678034364532486451)),
                (0, Step::Connect),
                (2, Step::Work(5903877029157569881)),
                (1, Step::Work(2457473270522847108)),
                (2, Step::Duplicate(10767262520509640516)),
                (2, Step::Duplicate(4851272950074024160)),
                (1, Step::Deliver(11340430273384828348)),
                (1, Step::Work(15569514617193981134)),
                (2, Step::Commit(Reply::Served)),
            ],
        ],
    )
    .unwrap();
}

fn names(world: &World) -> Vec<Vec<&str>> {
    (world.devices.iter())
        .map(|device| {
            let mut names: Vec<&str> = (device.disk.entries().values())
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort();
            names
        })
        .collect()
}

fn owe_a_folder_its_name_while_another_device_changes_it(lose_answer: bool, change: Step) -> World {
    owe_a_folder_its_name_after_downloads(lose_answer, change, 8).unwrap()
}

fn owe_a_folder_its_name_after_downloads(
    lose_answer: bool,
    change: Step,
    downloads: usize,
) -> Result<World, Failure> {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.push((0, if lose_answer { Step::Drop } else { DELIVER }));
    steps.extend(vec![(1, Step::Work(0)); downloads]);
    steps.extend([(1, change), (1, SCAN)]);
    steps.extend([(1, Step::Work(0)); 8]);
    if lose_answer {
        steps.push((0, Step::Connect));
    }
    steps.extend([(0, Step::Work(0)); 16]);
    world.phase(1, &steps)?;
    Ok(world)
}

#[test]
fn a_folder_deleted_at_any_point_before_its_owed_rename_stays_deleted() {
    for downloads in 0..=8 {
        for lose_answer in [false, true] {
            let world = owe_a_folder_its_name_after_downloads(lose_answer, delete(1), downloads)
                .unwrap_or_else(|failure| {
                    panic!("{downloads} downloads, lost {lose_answer}: {failure}")
                });
            let kept: &[&str] = if downloads < 5 { &["A", "a"] } else { &[] };
            assert_eq!(
                disk_names(&world),
                [kept, kept],
                "{downloads} downloads, lost {lose_answer}"
            );
        }
    }
}

fn disk_names(world: &World) -> Vec<Vec<&str>> {
    (world.devices.iter())
        .map(|device| {
            let mut names: Vec<&str> = (device.disk.entries().values())
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort();
            names
        })
        .collect()
}

#[test]
fn a_folder_deleted_before_its_creator_hears_it_landed_stays_deleted() {
    let world = owe_a_folder_its_name_while_another_device_changes_it(true, delete(1));
    assert_eq!(disk_names(&world), [Vec::<&str>::new(), vec![]]);
}

#[test]
fn a_folder_deleted_before_it_takes_its_own_name_stays_deleted() {
    let world = owe_a_folder_its_name_while_another_device_changes_it(false, delete(1));
    assert_eq!(disk_names(&world), [Vec::<&str>::new(), vec![]]);
}

#[test]
fn a_folder_renamed_before_it_takes_its_own_name_keeps_that_name() {
    let world = owe_a_folder_its_name_while_another_device_changes_it(false, rename(1, 0, "b"));
    assert_eq!(disk_names(&world), [["a", "b"], ["a", "b"]]);
}

#[test]
fn a_folder_renamed_before_its_creator_hears_it_landed_keeps_that_name() {
    let world = owe_a_folder_its_name_while_another_device_changes_it(true, rename(1, 0, "b"));
    assert_eq!(disk_names(&world), [["a", "b"], ["a", "b"]]);
}

fn rename_a_folder_to_its_owed_name_and_back(downloads: usize, works: usize) -> World {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT), (0, DELIVER)]);
    steps.extend(vec![(1, Step::Work(0)); downloads]);
    steps.extend([(1, rename(1, 0, "A")), (1, SCAN)]);
    steps.extend(vec![(1, Step::Work(0)); works]);
    steps.extend([(1, rename(1, 0, PENDING_NAME)), (1, SCAN)]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([(0, Step::Work(0)); 16]);
    steps.extend([(1, Step::Work(0)); 8]);
    world.phase(1, &steps).unwrap();
    world
}

#[test]
fn a_folder_renamed_to_its_owed_name_and_back_while_the_disk_takes_it_keeps_the_pending_name() {
    let world = rename_a_folder_to_its_owed_name_and_back(6, 1);
    assert_eq!(
        disk_names(&world),
        [[PENDING_NAME, "a"], [PENDING_NAME, "a"]]
    );
}

#[test]
fn a_folder_renamed_back_once_its_owed_rename_is_built_keeps_the_pending_name() {
    let world = rename_a_folder_to_its_owed_name_and_back(7, 1);
    assert_eq!(
        disk_names(&world),
        [[PENDING_NAME, "a"], [PENDING_NAME, "a"]]
    );
}

#[test]
fn a_folder_renamed_back_once_its_owed_rename_is_sent_keeps_the_pending_name() {
    let world = rename_a_folder_to_its_owed_name_and_back(7, 2);
    assert_eq!(
        disk_names(&world),
        [[PENDING_NAME, "a"], [PENDING_NAME, "a"]]
    );
}

#[test]
fn a_folder_renamed_twice_while_its_owed_rename_is_in_flight_keeps_the_pending_name() {
    let world = rename_a_folder_to_its_owed_name_and_back(8, 1);
    assert_eq!(
        disk_names(&world),
        [[PENDING_NAME, "a"], [PENDING_NAME, "a"]]
    );
}

#[test]
fn a_folder_deleted_while_its_creator_writes_into_it_takes_its_own_name() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 3]);
    steps.extend([(0, user(1, 0, 1, "c.txt")), (0, SCAN)]);
    steps.extend([(1, Step::Work(0)); 10]);
    steps.extend([(1, delete(1)), (1, SCAN)]);
    world.phase(1, &steps).unwrap();
    assert_eq!(disk_names(&world), [["A", "c.txt"], ["A", "c.txt"]]);
}

#[test]
fn a_file_deleted_in_the_commit_that_renames_its_deleted_folder_stays_deleted() {
    let synced = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([(1, rename(0, 0, "A")), (1, delete(1))]);
    racing.extend(commit(1));
    let world = converge(2, &[synced, racing]).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("A".into(), None)],
            "device {device}"
        );
    }
}

#[test]
fn a_file_deleted_before_its_folder_is_renamed_out_of_the_trash_stays_deleted() {
    let synced = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, delete(1)),
        (1, SCAN),
        (1, DELIVER),
        (1, rename(0, 0, "A")),
    ]);
    racing.extend(commit(1));
    let world = converge(2, &[synced, racing]).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("A".into(), None)],
            "device {device}"
        );
    }
}

#[test]
fn an_edit_made_before_a_rename_out_of_the_trash_brings_its_file_back_survives_the_delete_sent_with_it()
 {
    let mut world = World::new(3);
    world
        .phase(0, &[(0, folder("a")), (0, user(1, 0, 1, "b"))])
        .unwrap();
    world
        .run(
            1,
            &[
                (1, rename(0, 0, "A")),
                (1, delete(1)),
                (1, SCAN),
                (1, COMMIT),
            ],
        )
        .unwrap();
    world
        .run(
            2,
            &[
                (2, user(2, 1, 0, "a")),
                (2, SCAN),
                (2, COMMIT),
                (2, COMMIT),
                (2, DELIVER),
            ],
        )
        .unwrap();
    for _ in 0..20 {
        world.run(3, &[(0, DELIVER), (0, Step::Work(1))]).unwrap();
    }
    world
        .run(
            4,
            &[
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
            ],
        )
        .unwrap();
    world.run(5, &[(1, COMMIT)]).unwrap();
    world.phase(6, &[]).unwrap();
    let b = (world.space().nodes().into_iter())
        .find(|node| node.name == "b")
        .unwrap();
    assert_eq!(b.status, koidrive_proto::v1::NodeStatus::Live as i32);
}

#[test]
fn an_edit_that_reaches_a_device_after_its_user_removed_the_file_survives_a_rename_out_of_the_trash()
 {
    let mut world = World::new(3);
    world
        .phase(0, &[(0, folder("a")), (0, user(1, 0, 1, "b"))])
        .unwrap();
    world
        .run(1, &[(1, rename(0, 0, "A")), (1, delete(1)), (1, SCAN)])
        .unwrap();
    world
        .run(
            2,
            &[
                (2, user(2, 1, 0, "a")),
                (2, SCAN),
                (2, COMMIT),
                (2, COMMIT),
                (2, DELIVER),
            ],
        )
        .unwrap();
    for _ in 0..20 {
        world.run(3, &[(0, DELIVER), (0, Step::Work(1))]).unwrap();
    }
    world
        .run(
            4,
            &[
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
            ],
        )
        .unwrap();
    for _ in 0..20 {
        world.run(5, &[(1, DELIVER)]).unwrap();
    }
    world.phase(6, &[]).unwrap();
    let b = (world.space().nodes().into_iter())
        .find(|node| node.name == "b")
        .unwrap();
    assert_eq!(b.status, koidrive_proto::v1::NodeStatus::Live as i32);
}

#[test]
fn a_file_replaced_in_a_folder_moved_out_of_the_trash_keeps_its_name() {
    let synced = vec![
        (0, folder("c.txt")),
        (0, user(0, 0, 1, "a")),
        (0, user(1, 0, 2, "b")),
    ];
    let old = content_on(&converge(2, std::slice::from_ref(&synced)).unwrap(), 1, "b");
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, delete(2)),
        (1, SCAN),
        (1, DELIVER),
        (1, user(1, 0, 2, "b")),
        (1, rename(1, 0, "a")),
    ]);
    racing.extend(commit(1));
    let world = converge(2, &[synced, racing]).unwrap();
    let new = content_on(&world, 1, "b");
    assert_ne!(new, old);
    for device in 0..2 {
        let files = [("a".into(), None), ("b".into(), Some(new.clone()))];
        assert_eq!(files_on(&world, device), files, "device {device}");
    }
}

#[test]
fn a_file_its_user_removed_and_then_restores_from_the_trash_stays_restored() {
    let synced = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([(1, delete(1)), (1, SCAN), (1, DELIVER)]);
    let mut world = converge(2, &[synced, racing]).unwrap();
    let engine = &mut world.devices[1].engine;
    let b = (engine.trees.remote().iter())
        .find(|(_, node)| node.shape.name == "b")
        .map(|(id, _)| id.clone())
        .unwrap();
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::from_trash(b, None, None).unwrap();
    assert!(engine.commits.restore(request, intent));
    world.phase(2, &commit(1)).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a", "b"], "device {device}");
    }
}

#[test]
fn a_file_restored_after_its_user_removed_it_keeps_a_rename_made_before_the_restore_is_acknowledged()
 {
    let synced = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([(1, delete(1)), (1, SCAN), (1, DELIVER)]);
    let mut world = converge(2, &[synced, racing]).unwrap();
    let engine = &mut world.devices[1].engine;
    let b = (engine.trees.remote().iter())
        .find(|(_, node)| node.shape.name == "b")
        .map(|(id, _)| id.clone())
        .unwrap();
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::from_trash(b, None, None).unwrap();
    assert!(engine.commits.restore(request, intent));
    world
        .run(2, &[(1, SCAN), (1, COMMIT), (1, COMMIT)])
        .unwrap();
    let mut steps = vec![(0, DELIVER); 6];
    steps.extend([(0, Step::Work(1)); 6]);
    world.run(3, &steps).unwrap();
    let b = (world.devices[0].disk.entries().values())
        .position(|entry| entry.name == "b")
        .unwrap();
    let renamed = [(0, rename(b, 1, "ss")), (0, SCAN), (0, COMMIT), (0, COMMIT)];
    world.run(4, &renamed).unwrap();
    world.phase(5, &[]).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a", "ss"], "device {device}");
    }
}

#[test]
fn a_version_restore_the_server_rejects_leaves_a_file_its_user_removed_deleted() {
    let mut world = world(2);
    world
        .phase(0, &[(0, folder("a")), (0, user(1, 0, 1, "b"))])
        .unwrap();
    let mut racing = vec![(0, delete(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, delete(1)),
        (1, SCAN),
        (1, DELIVER),
        (1, rename(0, 0, "A")),
        (1, SCAN),
    ]);
    world.run(1, &racing).unwrap();
    let engine = &mut world.devices[1].engine;
    let b = (engine.trees.remote().iter())
        .find(|(_, node)| node.shape.name == "b")
        .map(|(id, _)| id.clone())
        .unwrap();
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let missing = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::version(b, missing).unwrap();
    assert!(engine.commits.restore(request, intent));
    world.phase(2, &commit(1)).unwrap();
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device),
            [("A".into(), None)],
            "device {device}"
        );
    }
}

#[test]
fn a_file_restored_while_another_device_deletes_its_folder_stays_restored() {
    let mut world = world(2);
    world
        .phase(0, &[(0, folder("a")), (0, user(1, 0, 1, "b"))])
        .unwrap();
    world
        .run(1, &[(1, delete(1)), (1, rename(0, 0, "A"))])
        .unwrap();
    let engine = &mut world.devices[1].engine;
    let b = (engine.trees.remote().iter())
        .find(|(_, node)| node.shape.name == "b")
        .map(|(id, _)| id.clone())
        .unwrap();
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::from_trash(b, None, None).unwrap();
    assert!(engine.commits.restore(request.clone(), intent));
    world.run(2, &[(1, SCAN), (1, COMMIT)]).unwrap();
    let mut remote = vec![(0, delete(0))];
    remote.extend(commit(0));
    world.run(3, &remote).unwrap();
    world.run(4, &[(1, COMMIT)]).unwrap();
    world.phase(5, &[]).unwrap();
    let resolved = world.devices[1].engine.commits.resolved();
    let restored = resolved.iter().find(|ended| ended.id == request).unwrap();
    assert_eq!(
        restored.outcome,
        koidrive_engine::restore::Outcome::Restored
    );
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["A", "b"], "device {device}");
    }
}

#[test]
fn a_folder_delete_superseded_behind_a_name_that_differs_only_in_case_brings_the_folder_back() {
    let made = vec![(0, folder("a")), (2, file("C.TXT"))];
    let filled = vec![(0, user(1, 0, 1, "ß"))];
    let racing = vec![
        (2, rename(2, 0, "c.txt")),
        (2, delete(1)),
        (0, rename(1, 1, "a")),
        (2, Step::Work(0)),
        (2, Step::Work(0)),
        (0, folder("ß")),
        (0, Step::Work(0)),
    ];
    let world = converge(3, &[made, filled, racing]).unwrap();
    for device in &world.devices {
        let mut paths: Vec<String> = (device.disk.visible().into_iter())
            .filter_map(|ino| Some(device.disk.path(ino)?.join("/")))
            .collect();
        paths.sort();
        assert_eq!(paths, ["a", "a/a", "c.txt", "ß"]);
    }
}

#[test]
fn a_folder_another_device_made_stays_deleted_when_a_restore_takes_its_delete() {
    let synced = vec![(1, folder("b")), (1, user(1, 0, 1, "a"))];
    let mut lost = vec![(0, BACKUP), (0, delete(0))];
    lost.extend(commit(0));
    lost.push((0, RESTORE));
    let world = keep(2, &[synced, lost]).unwrap();
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 0);
    }
}

#[test]
fn a_file_moved_and_edited_in_a_lost_commit_stays_one_file_when_another_file_takes_its_old_name() {
    let mut world = world(2);
    world.phase(0, &[(0, file("a")), (0, file("b"))]).unwrap();
    let lost = [(0, BACKUP), (0, rename(0, 0, "c.txt")), (0, overwrite(0))];
    world.phase(1, &lost).unwrap();
    let disk = &world.devices[1].disk;
    let b = (disk.visible().iter())
        .position(|ino| disk.entries()[ino].name == "b")
        .unwrap();
    world.phase(2, &[(1, rename(b, 0, "a"))]).unwrap();
    let mut steps = vec![(0, RESTORE), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    steps.extend([(0, DELIVER); 4]);
    world.phase(3, &steps).unwrap();
    for device in &world.devices {
        assert_eq!(
            device.disk.entries().len(),
            2,
            "{:?}",
            device.disk.entries()
        );
    }
}

#[test]
fn a_file_another_device_deletes_after_a_restore_stays_deleted_where_this_device_lost_its_move() {
    use koidrive_sim::world::{Outcome::*, Reply::*, Step::*};
    let u = |kind, target, parent, name, text| {
        User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (0, u(1, 0, 17106236007756318171, 5, 2)),
            (0, Effect(Run)),
            (2, u(2, 13154000409921659125, 11317697931782792700, 0, 0)),
            (1, Work(10402927595796392272)),
            (2, u(0, 8102071768465887985, 2491586679729720529, 3, 1)),
            (1, Work(8147076248253367137)),
            (0, Duplicate(17806711057161795297)),
            (2, Work(9827515188722386526)),
            (0, Work(10759842065693380470)),
            (2, Connect),
            (2, Commit(NoChunks)),
            (2, u(2, 12304797714950920699, 8235545250517780698, 2, 2)),
            (0, Effect(Run)),
            (1, u(2, 18222769914265244999, 5113088640866526070, 3, 3)),
            (0, u(3, 10796086952843793315, 17349774340083116548, 1, 0)),
            (2, Deliver(12499940093593466751)),
            (1, Work(4043467003170336494)),
            (2, Work(4728614493924271296)),
            (2, u(4, 9599447120861858895, 18007190340159385861, 5, 2)),
            (0, Backup),
        ],
        vec![
            (2, Backup),
            (2, u(2, 9184836677058907065, 10433568392581601685, 0, 0)),
            (2, Scan),
            (2, Commit(Served)),
            (2, Commit(Served)),
            (2, Deliver(12927180940504033335)),
            (2, Deliver(3825508426826136613)),
            (2, u(2, 9184836677058907065, 3418892106507063918, 3, 3)),
            (2, Scan),
            (2, Commit(Served)),
            (2, Commit(Served)),
            (1, u(3, 6617529400670140390, 9272155802029785947, 4, 3)),
            (2, Commit(Served)),
            (2, u(2, 13905988022610025184, 3052641924150865604, 6, 2)),
            (1, u(1, 7979644240074520884, 13471144562339818831, 4, 3)),
            (1, Commit(Served)),
            (2, Restore),
            (2, Effect(Run)),
            (2, u(1, 16549484964196156729, 15324450509200737979, 0, 2)),
            (2, Effect(Run)),
            (2, Duplicate(3438284062848940079)),
            (2, Deliver(2861012374057950334)),
        ],
        vec![
            (0, u(3, 11081397640828020932, 11362425464966793444, 6, 3)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (0, Deliver(11797932787380850498)),
            (0, u(3, 11081397640828020932, 6919568488104646115, 4, 0)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (2, u(1, 1772315988544606582, 7017746951609423526, 0, 0)),
            (2, Work(15679955762379804781)),
            (1, u(1, 1277192366001788814, 1522225953410853276, 1, 1)),
            (0, Work(16205367382564583885)),
            (2, u(4, 13857229851831582670, 13975215875373442002, 2, 0)),
            (0, Restore),
            (2, u(3, 3702807200985915994, 14081670185505849278, 4, 3)),
            (1, Connect),
        ],
        vec![
            (0, Backup),
            (0, u(3, 7527335667581534596, 10929708698728945290, 6, 0)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (0, Deliver(10466699725936890930)),
            (0, Deliver(6868596529788382619)),
            (0, u(2, 7527335667581534596, 1287398878429798948, 5, 0)),
            (1, Work(1465397633813081855)),
            (0, Restore),
            (2, u(1, 14282863983713927413, 10917475380281013382, 3, 2)),
            (1, Work(6282625744961036041)),
            (0, Deliver(5109877815414700445)),
            (2, u(1, 11808259191354608109, 6804749932233161696, 5, 1)),
            (2, u(1, 11096482189577154791, 2047070662338996294, 4, 3)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_file_deleted_after_its_lost_edit_was_sent_again_stays_deleted_after_a_second_restore() {
    use koidrive_sim::world::{Outcome::*, Reply::*, Step::*};
    let u = |kind, target, parent, name, text| {
        User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (2, u(1, 0, 9903764825449132303, 3, 0)),
            (2, u(1, 0, 11552117565777905621, 1, 0)),
            (0, u(1, 0, 6547235099835790166, 2, 1)),
            (2, Commit(Served)),
            (1, Work(10851495513385420087)),
            (0, Backup),
        ],
        vec![
            (1, Backup),
            (1, u(2, 13451676212994458387, 9565607454629926028, 5, 1)),
            (1, Scan),
            (1, Commit(Served)),
            (1, Commit(Served)),
            (1, Deliver(15439413583016071576)),
            (1, u(2, 13451676212994458387, 1428915013577112214, 2, 0)),
            (0, u(0, 14921052704295272185, 1631154572745136587, 3, 0)),
            (0, Work(5760192452305054293)),
            (1, Work(11147973249046154743)),
            (1, Restore),
            (2, Work(7569785021038804799)),
            (1, Scan),
            (1, u(2, 16917427084108916823, 3208504347092805974, 6, 2)),
            (2, u(0, 13110796547459440586, 12873427126193245819, 1, 2)),
            (2, Deliver(12181449998824957549)),
            (2, Work(15345591448188332441)),
        ],
        vec![
            (0, u(2, 5455820235707350558, 15481885657966313345, 2, 1)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (0, Deliver(8823653801653775740)),
            (0, Deliver(8476531716452877371)),
            (0, u(3, 5455820235707350558, 17745467830829831845, 5, 2)),
            (1, Deliver(2207532471932803233)),
            (2, Work(5196016082092443997)),
            (0, u(1, 10602450107474480572, 15713227814823447551, 6, 3)),
            (2, Work(11845418407008084177)),
            (0, Duplicate(17309292382555486343)),
            (1, Deliver(14085540052365221270)),
            (2, Deliver(11465396930300754326)),
            (0, Restore),
            (1, Work(4953386889912483399)),
            (1, Effect(Run)),
            (0, Work(10626274012016294836)),
            (2, Scan),
            (0, Connect),
        ],
        vec![
            (1, u(2, 14029889503971787886, 17520627783615361546, 4, 1)),
            (1, Scan),
            (1, Commit(Served)),
            (1, Commit(Served)),
            (1, Deliver(10231594919977477748)),
            (1, Deliver(8609908036244146517)),
            (1, u(2, 14029889503971787886, 3245076338140175791, 3, 3)),
            (1, Scan),
            (1, Commit(Served)),
            (1, Commit(Served)),
            (2, u(1, 2450262582666582614, 2694992668587191784, 3, 0)),
            (2, u(4, 16826249660264566776, 15981268369320786190, 4, 2)),
            (1, Effect(Run)),
            (1, Restore),
            (1, u(2, 15018102010109100468, 13671348707082575255, 1, 1)),
            (0, Work(8590167749790942484)),
            (2, Work(777097490670644355)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_lost_delete_is_not_sent_again_over_an_edit_another_device_made_after_the_restore() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, delete(0))];
    steps.extend(commit(0));
    steps.extend([(0, RESTORE), (1, Step::Connect)]);
    steps.extend([(1, DELIVER); 8]);
    steps.push((1, overwrite(0)));
    steps.extend(commit(1));
    world.phase(1, &steps).unwrap();
    for device in 0..2 {
        assert_eq!(files_on(&world, device).len(), 1, "device {device}");
    }
}

#[test]
fn an_observers_rename_survives_when_a_restore_takes_it_with_the_edit_it_downloaded() {
    let mut lost = vec![(0, BACKUP), (0, overwrite(0))];
    lost.extend(commit(0));
    lost.push((1, Step::Connect));
    lost.extend([(1, Step::Work(0)); 16]);
    lost.push((1, rename(0, 0, "b")));
    lost.extend(commit(1));
    lost.extend([(1, DELIVER); 4]);
    lost.push((0, RESTORE));
    reconnect(&mut lost, 1, true);
    reconnect(&mut lost, 0, true);
    let world = keep(3, &[vec![(0, file("a"))], lost]).unwrap();
    for device in 0..3 {
        let files = files_on(&world, device);
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["b"], "device {device}: {files:?}");
    }
}

#[test]
fn a_file_another_device_edited_and_deleted_stays_deleted_after_a_restore() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, u(1, 0, 0, 2, 0)),
            (0, u(1, 0, 0, 3, 0)),
            (1, u(1, 0, 0, 0, 0)),
            (0, u(1, 0, 0, 4, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, BACKUP),
        ],
        vec![
            (0, BACKUP),
            (0, u(2, 0, 0, 0, 0)),
            (0, SCAN),
            (0, COMMIT),
            (0, COMMIT),
            (0, u(2, 0, 0, 0, 0)),
            (0, u(0, 0, 2844692439453172141, 0, 0)),
            (0, RESTORE),
        ],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (0, SCAN),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
            (0, u(2, 0, 0, 0, 0)),
            (1, Step::Work(0)),
            (0, u(4, 5916588535269142056, 0, 0, 0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Work(0)),
            (1, Step::Work(8005512575619858474)),
            (0, RESTORE),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_folder_another_device_deleted_stays_deleted_after_a_restore_that_lost_it() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, u(1, 0, 0, 2, 0)),
            (0, u(1, 0, 0, 3, 0)),
            (1, u(1, 0, 0, 0, 0)),
            (0, u(1, 0, 0, 4, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, BACKUP),
        ],
        vec![
            (0, BACKUP),
            (0, u(2, 0, 0, 0, 0)),
            (0, SCAN),
            (0, COMMIT),
            (0, COMMIT),
            (0, u(2, 0, 0, 0, 0)),
            (0, u(0, 0, 2844692439453172141, 0, 0)),
            (0, RESTORE),
        ],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (0, SCAN),
            (0, COMMIT),
            (0, COMMIT),
            (0, DELIVER),
            (0, u(2, 0, 0, 0, 0)),
            (1, Step::Work(0)),
            (0, u(4, 5919481429929157707, 0, 0, 0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Work(0)),
            (1, Step::Work(8005512575619858474)),
            (0, RESTORE),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_rename_inside_a_lost_folder_its_creator_deleted_converges() {
    let made = vec![(0, user(1, 0, 0, "a"))];
    let moved = vec![
        (0, Step::Backup),
        (0, user(0, 0, 0, "b")),
        (0, Step::Scan),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Deliver(0)),
        (0, Step::Deliver(0)),
        (0, user(3, 0, 1, "a")),
    ];
    let racing = vec![
        (1, user(3, 0, 1, "A")),
        (0, user(4, 1, 0, "a")),
        (0, Step::Restore),
    ];
    converge(2, &[made, moved, racing]).unwrap();
}

#[test]
fn a_lost_folder_is_not_created_again_inside_a_folder_deleted_while_it_waited() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, u(1, 0, 0, 4, 0)),
            (1, u(1, 0, 0, 0, 0)),
            (1, u(0, 0, 0, 3, 0)),
        ],
        vec![
            (0, Step::Backup),
            (2, u(0, 0, 0, 6, 0)),
            (2, u(0, 0, 1, 0, 0)),
            (2, u(3, 1809843479218423697, 3, 2, 0)),
        ],
        vec![
            (0, u(4, 23960249537131406, 7340898669396870823, 6, 2)),
            (0, Step::Restore),
            (1, Step::Connect),
            (1, Step::Work(11631325315051589583)),
            (1, COMMIT),
            (2, u(4, 6786950160668269581, 8740926674541383984, 4, 2)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_kept_child_of_a_lost_folder_stays_deleted_after_a_crash_mid_effect() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (1, u(1, 0, 0, 1, 0)),
            (0, u(1, 0, 0, 0, 0)),
            (1, u(1, 0, 0, 3, 0)),
            (1, u(1, 0, 0, 6, 0)),
        ],
        vec![
            (0, Step::Backup),
            (2, u(0, 0, 1, 4, 0)),
            (2, u(3, 10565563719318406587, 1, 3, 0)),
            (2, u(3, 8511544876117996999, 2, 1, 0)),
        ],
        vec![
            (0, u(4, 17295685281375005034, 6989454378278442494, 6, 2)),
            (0, Step::Restore),
            (1, Step::Connect),
            (1, Step::Work(6048236135531082776)),
            (1, CRASH),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_kept_child_of_a_lost_folder_stays_deleted_across_a_second_restore() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(1, u(0, 0, 0, 3, 0)), (1, u(1, 0, 0, 4, 0))],
        vec![
            (0, Step::Backup),
            (1, u(0, 0, 0, 2, 0)),
            (1, u(3, 11736929676757859842, 2, 5, 0)),
        ],
        vec![
            (1, u(4, 1080863262105148645, 1490760445408345681, 2, 2)),
            (0, Step::Restore),
            (2, Step::Connect),
            (2, Step::Work(17783959944508942500)),
            (0, Step::Restore),
            (2, Step::Work(16535215316277980988)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_rename_inside_a_lost_folder_another_device_deleted_converges() {
    let made = vec![(0, user(1, 0, 0, "a"))];
    let moved = vec![
        (0, Step::Backup),
        (0, user(0, 0, 0, "b")),
        (0, Step::Scan),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Deliver(0)),
        (0, Step::Deliver(0)),
        (0, user(3, 0, 1, "a")),
    ];
    let racing = vec![
        (0, user(4, 1, 0, "a")),
        (0, Step::Scan),
        (0, COMMIT),
        (0, COMMIT),
        (1, Step::Deliver(0)),
        (1, Step::Deliver(0)),
        (1, Step::Deliver(0)),
        (1, user(3, 0, 1, "A")),
        (0, Step::Restore),
    ];
    converge(2, &[made, moved, racing]).unwrap();
}

fn a_deleted_lost_folder_that_another_device_emptied(tail: &[(usize, Step)]) {
    let made = vec![(0, user(1, 0, 0, "a"))];
    let moved = vec![
        (0, Step::Backup),
        (0, user(0, 0, 0, "b")),
        (0, Step::Scan),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Deliver(0)),
        (0, Step::Deliver(0)),
        (0, user(3, 0, 1, "a")),
    ];
    let mut deleted = vec![
        (0, user(4, 1, 0, "a")),
        (0, Step::Scan),
        (0, COMMIT),
        (0, COMMIT),
        (1, Step::Deliver(0)),
        (1, Step::Deliver(0)),
        (1, Step::Deliver(0)),
    ];
    deleted.extend_from_slice(tail);
    converge(2, &[made, moved, deleted]).unwrap();
}

#[test]
fn a_rename_inside_a_deleted_lost_folder_made_after_the_bootstrap_converges() {
    a_deleted_lost_folder_that_another_device_emptied(&[
        (0, Step::Restore),
        (1, Step::Connect),
        (1, Step::Deliver(0)),
        (1, user(3, 0, 1, "A")),
    ]);
}

#[test]
fn a_rename_inside_a_deleted_lost_folder_the_bootstrap_has_not_scanned_converges() {
    a_deleted_lost_folder_that_another_device_emptied(&[
        (1, user(3, 0, 1, "A")),
        (0, Step::Restore),
        (1, Step::Connect),
        (1, Step::Deliver(0)),
        (1, Step::Deliver(0)),
    ]);
}

fn recorded_in_sync(world: &World, device: usize) -> Vec<String> {
    let trees = &world.devices[device].engine.trees;
    (trees.state().local_changes.into_iter())
        .filter(|id| {
            let local = trees.local().get(id).map(|node| node.shape.clone());
            koidrive_engine::tree::same(local.as_ref(), trees.synced().get(id))
        })
        .collect()
}

#[test]
fn devices_that_converge_without_a_loss_record_no_change_l_and_s_agree_on() {
    let mut runner = TestRunner::deterministic();
    let strategy = (2..=3usize, phases(Losses::None));
    let mut recorded = Vec::new();
    for _ in 0..64 {
        let (devices, phases) = strategy.new_tree(&mut runner).unwrap().current();
        let world = keep(devices, &phases).unwrap_or_else(|failure| panic!("{failure}"));
        recorded.extend((0..devices).flat_map(|device| recorded_in_sync(&world, device)));
    }
    assert_eq!(recorded, Vec::<String>::new());
}

#[test]
fn a_conflict_copy_the_engine_names_is_no_change_once_the_server_holds_it() {
    let mut world = converge(2, &[vec![(0, folder("a"))]]).unwrap();
    let mut steps = vec![(0, user(0, 0, 1, "ß")), (0, user(1, 0, 1, "ss"))];
    steps.extend(commit(0));
    world.phase(1, &steps).unwrap();
    assert_eq!(
        paths_on(&world, 0),
        ["a", "a/ss (conflict from device-0 2026-11-09)", "a/ß"]
    );
    assert_eq!(recorded_in_sync(&world, 0), Vec::<String>::new());
}

#[test]
fn a_folder_deleted_while_its_owed_rename_is_in_flight_stays_deleted() {
    let world = an_owed_rename_in_flight_while_another_device_changes_it(delete(1));
    for device in &world.devices {
        assert!(
            device.disk.entries().is_empty(),
            "{:?}",
            device.disk.entries()
        );
    }
}

#[test]
fn a_folder_renamed_while_its_owed_rename_is_in_flight_keeps_that_name() {
    let world = an_owed_rename_in_flight_while_another_device_changes_it(rename(1, 0, "b"));
    for device in &world.devices {
        let mut names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
    }
}

fn an_owed_rename_landed_before_another_devices_change(change: Step, heard: usize) -> World {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([(1, change), (1, SCAN), (1, COMMIT)]);
    steps.extend([(0, Step::Work(0)); 16]);
    steps.extend(std::iter::repeat_n((1, DELIVER), heard));
    steps.push((1, COMMIT));
    world.phase(1, &steps).unwrap();
    world
}

#[test]
fn a_folder_deleted_before_its_owed_rename_landed_stays_deleted() {
    for heard in [0, 8] {
        let world = an_owed_rename_landed_before_another_devices_change(delete(1), heard);
        assert_eq!(names(&world), [[""; 0]; 2], "heard {heard}");
    }
}

#[test]
fn a_folder_renamed_before_its_owed_rename_landed_keeps_that_name() {
    for heard in [0, 8] {
        let world = an_owed_rename_landed_before_another_devices_change(rename(1, 0, "b"), heard);
        assert_eq!(names(&world), [["a", "b"]; 2], "heard {heard}");
    }
}

#[test]
fn a_folder_deleted_on_a_device_that_hears_its_owed_rename_first_stays_deleted() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.push((1, delete(1)));
    steps.extend([(0, Step::Work(0)); 16]);
    steps.extend([(1, Step::Work(0)); 12]);
    world.phase(1, &steps).unwrap();
    assert_eq!(names(&world), [[""; 0]; 2]);
}

#[test]
fn a_folder_deleted_while_its_owed_rename_waits_for_a_reconnect_stays_deleted() {
    let wide = |kind, parent| {
        Step::User(Action {
            kind,
            target: 0,
            parent,
            name: 1,
            text: 0,
        })
    };
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([(0, Step::Drop), (2, wide(0, 3525480467146600595))]);
    steps.extend([(0, folder("a")); 4]);
    steps.extend([
        (1, delete(1)),
        (0, Step::Work(8189)),
        (2, SCAN),
        (1, Step::Work(6345002853588532233)),
        (2, Step::Effect(Outcome::Fail)),
        (2, Step::Drop),
        (1, Step::Work(10884997234294703724)),
        (1, Step::Work(8368452453479477715)),
    ]);
    converge(2, &[vec![(0, file("a"))], steps]).unwrap();
}

#[test]
fn a_folder_deleted_after_a_restore_stays_deleted_on_a_device_that_heard_its_owed_rename() {
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (1, delete(1)),
        (2, Step::Work(0)),
        (2, EFFECT),
        (0, Step::Backup),
        (2, Step::Work(0)),
        (0, COMMIT),
        (2, Step::Work(4133075421559048010)),
        (0, Step::Restore),
    ]);
    keep(3, &[vec![(0, file("a"))], steps]).unwrap();
}

#[test]
fn a_folder_deleted_before_a_restore_stays_deleted_when_its_owed_rename_is_sent_again() {
    let create = |parent| {
        Step::User(Action {
            kind: 0,
            target: 0,
            parent,
            name: 0,
            text: 0,
        })
    };
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (1, delete(1)),
        (0, Step::Backup),
        (0, create(2837005791047431358)),
        (0, create(0)),
        (0, create(0)),
        (0, Step::Work(3925350407796910387)),
        (0, COMMIT),
        (0, Step::Work(0)),
        (0, Step::Restore),
    ]);
    keep(3, &[vec![(0, file("a"))], steps]).unwrap();
}

#[test]
fn a_folder_deleted_after_its_rename_was_sent_stays_deleted_past_an_owed_rename() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (1, rename(1, 0, "A")),
        (1, SCAN),
        (1, COMMIT),
        (1, delete(1)),
        (1, SCAN),
    ]);
    steps.push((0, COMMIT));
    steps.push((1, COMMIT));
    world.phase(1, &steps).unwrap();
    assert_eq!(names(&world), [[""; 0]; 2]);
}

#[test]
fn a_folder_revived_before_its_owed_rename_is_answered_keeps_converging() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (1, delete(1)),
        (1, delete(3979821551319602821)),
        (1, Step::Work(0)),
        (2, overwrite(0)),
        (2, Step::Work(67468823311028661)),
        (2, DELIVER),
        (2, Step::Work(3613414708570467567)),
        (1, Step::Work(0)),
        (2, Step::Work(0)),
        (2, Step::Work(0)),
        (1, COMMIT),
        (0, Step::Work(1304240622328750977)),
        (2, Step::Work(2365667432405746353)),
    ]);
    world.phase(1, &steps).unwrap();
}

#[test]
fn a_folder_deleted_on_a_device_that_hears_its_owed_rename_before_a_scan_stays_deleted() {
    let user = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (0, user(2, 0, 0, 0)),
        (0, Step::Connect),
        (0, Step::Work(199110312177959156)),
        (1, user(3, 4702980891840035612, 3578894644795361832, 2)),
        (1, user(4, 1, 0, 0)),
        (2, user(3, 0, 0, 0)),
        (0, Step::Work(0)),
        (1, Step::Work(3854247624805571084)),
        (1, Step::Work(1799778900217984966)),
        (1, Step::Work(4891621672650151491)),
        (2, user(0, 0, 0, 0)),
        (1, COMMIT),
        (0, user(0, 0, 9397246827782143146, 0)),
        (0, user(2, 0, 0, 0)),
        (0, Step::Work(0)),
        (0, user(0, 0, 6206047745663626968, 0)),
        (0, user(0, 0, 5057020370265126714, 0)),
        (0, user(2, 0, 0, 0)),
    ]);
    converge(3, &[vec![(0, file("a"))], steps]).unwrap();
}

#[test]
fn an_edit_whose_commit_a_restore_took_before_its_result_is_sent_again_where_another_device_lost_its_move()
 {
    use koidrive_sim::world::{Reply::*, Step::*};
    let u = |kind, target, parent, name, text| {
        User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 2)), (0, Backup)],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (0, u(2, 0, 0, 0, 0)),
            (0, Restore),
            (0, u(0, 0, 0, 3, 0)),
            (0, u(3, 6932547575979965906, 0, 1, 0)),
        ],
        vec![
            (1, Backup),
            (1, u(3, 10703750447421770167, 0, 4, 0)),
            (1, Scan),
            (1, Commit(Served)),
            (1, Commit(Served)),
            (1, u(2, 10703750447421770167, 0, 0, 0)),
            (1, Restore),
        ],
        vec![
            (0, u(2, 6402246228121299803, 0, 0, 0)),
            (0, Scan),
            (0, Commit(Served)),
            (0, Commit(Served)),
            (0, u(2, 6402246228121299803, 0, 0, 0)),
            (2, Work(0)),
            (1, Deliver(0)),
            (0, Restore),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_folder_owing_its_name_that_another_device_deletes_and_a_third_brings_back_leaves_its_pending_name()
 {
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend([(0, Step::Work(0)); 5]);
    steps.extend([(1, Step::Work(0)); 8]);
    steps.extend([
        (1, delete(1)),
        (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
        (2, Step::Work(0)),
        (0, Step::Work(0)),
        (2, Step::Effect(Outcome::Crash(Cut::Placed))),
        (0, user(0, 0, 7021841829139713342, "a")),
        (1, Step::Work(0)),
        (1, Step::Work(0)),
        (1, Step::Work(0)),
        (2, user(0, 0, 264496735129991995, "a")),
    ]);
    let world = converge(3, &[vec![(0, file("a"))], steps]).unwrap();
    for device in 0..3 {
        assert_eq!(paths_on(&world, device), ["A", "A/a"], "device {device}");
    }
}

#[test]
fn a_file_edited_after_another_device_deleted_it_converges() {
    let w = |d: usize| (d, Step::Work(0));
    let mut steps = vec![(2, Step::Work(0)); 8];
    steps.push((1, delete(1)));
    steps.extend(commit(1));
    steps.push((2, overwrite(0)));
    steps.extend(commit(2));
    steps.extend(owing());
    steps.extend([
        (1, delete(1)),
        w(0),
        w(0),
        (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
        w(0),
        (0, user(2, 6800060412439942171, 0, "a")),
        (0, folder("a")),
    ]);
    converge(3, &[vec![(0, file("a"))], steps]).unwrap();
}

#[test]
fn a_write_the_server_kept_is_taken_as_synced_when_its_record_arrives_before_its_result() {
    let a = |kind: u8, target: usize, parent: usize, name: usize, text: u8| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let w = |d: usize| (d, Step::Work(0));
    let mut owed = vec![(2, Step::Work(0)); 8];
    owed.push((1, delete(1)));
    owed.extend(commit(1));
    owed.push((2, overwrite(0)));
    owed.extend(commit(2));
    owed.extend(owing());
    owed.extend([
        (1, delete(1)),
        (0, COMMIT),
        w(0),
        (0, Step::Work(4240733720137905515)),
        (0, Step::Commit(Reply::NoChunks)),
        w(0),
        w(0),
        (0, user(2, 3211346187131332213, 0, "a")),
    ]);
    let cases = vec![
        (3, vec![vec![(0, file("a"))], owed]),
        (
            3,
            vec![
                vec![(1, a(1, 0, 0, 0, 0))],
                vec![
                    (2, a(2, 0, 0, 0, 0)),
                    (2, Step::Work(0)),
                    (2, Step::Work(0)),
                    (2, Step::Commit(Reply::Served)),
                    (1, Step::Work(0)),
                    (1, Step::Work(0)),
                    (2, Step::Duplicate(0)),
                    (1, Step::Work(0)),
                    (1, a(2, 0, 0, 0, 0)),
                    (1, Step::Work(0)),
                    (1, Step::Work(43)),
                    (1, Step::Commit(Reply::Served)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, a(1, 0, 0, 0, 3)), (0, a(3, 0, 0, 1, 0))],
                vec![
                    (1, a(0, 0, 0, 0, 0)),
                    (1, a(2, 2008427414769409736, 0, 0, 0)),
                    (1, Step::Work(0)),
                    (1, Step::Work(0)),
                    (1, Step::Commit(Reply::Served)),
                    (2, Step::Work(0)),
                    (0, Step::Effect(Outcome::Run)),
                    (0, Step::Work(0)),
                    (1, a(3, 994829453346764372, 0, 2, 0)),
                    (0, a(2, 132281987842843300, 12501385776686876657, 5, 3)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, a(1, 0, 0, 0, 0))],
                vec![
                    (1, a(2, 0, 0, 0, 0)),
                    (1, Step::Work(0)),
                    (1, Step::Commit(Reply::Served)),
                    (1, Step::Work(0)),
                    (1, a(3, 0, 0, 1, 0)),
                    (2, Step::Work(0)),
                    (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                    (0, a(0, 0, 0, 1, 0)),
                    (0, Step::Work(0)),
                    (2, a(2, 1121116636878847081, 0, 0, 0)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, a(0, 0, 0, 2, 0))],
                vec![
                    (0, a(3, 0, 0, 3, 0)),
                    (0, Step::Scan),
                    (0, Step::Commit(Reply::Served)),
                    (1, a(4, 0, 0, 0, 0)),
                    (1, a(0, 0, 0, 0, 0)),
                    (1, a(1, 0, 1, 0, 0)),
                    (1, Step::Scan),
                    (1, Step::Commit(Reply::Served)),
                    (0, Step::Commit(Reply::Served)),
                    (1, Step::Commit(Reply::Served)),
                    (2, a(1, 0, 0, 2, 2)),
                    (1, a(0, 0, 0, 2, 0)),
                ],
                vec![
                    (1, a(2, 3930299441111155299, 0, 0, 0)),
                    (1, Step::Work(0)),
                    (0, a(4, 910564747632835397, 0, 0, 0)),
                    (1, Step::Work(0)),
                    (1, Step::Work(0)),
                    (2, Step::Deliver(0)),
                    (0, Step::Work(1748583625567181184)),
                    (2, Step::Work(2635125152357438952)),
                    (0, a(2, 1772821328963428142, 0, 0, 2)),
                    (2, Step::Work(2733997028527349943)),
                    (2, Step::Commit(Reply::Served)),
                    (0, Step::Work(0)),
                ],
            ],
        ),
    ];
    for (devices, phases) in cases {
        converge(devices, &phases).unwrap();
    }
}

fn drawn(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

#[test]
fn a_removal_done_before_a_crash_stays_the_engines_when_a_later_removal_of_the_batch_quarantines_its_folder()
 {
    use Step::{Commit, Effect, Work};
    let phases = [
        vec![(0, drawn(1, 0, 0, 0))],
        vec![
            (0, drawn(0, 0, 0, 1)),
            (0, drawn(3, 0, 1, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, drawn(3, 1, 0, 2)),
            (0, drawn(0, 0, 7517258053377794243, 0)),
            (2, Work(0)),
            (0, drawn(2, 10523306665153133971, 0, 0)),
            (0, drawn(1, 0, 0, 5)),
            (0, drawn(0, 0, 3671704290190332164, 5)),
            (0, drawn(0, 0, 2655090210972650204, 3)),
            (1, drawn(2, 3181243908948293186, 0, 0)),
            (2, Work(0)),
            (1, drawn(1, 0, 690241634734802743, 1)),
        ],
        vec![
            (0, drawn(4, 7989535778489345035, 0, 0)),
            (0, drawn(2, 0, 0, 0)),
            (0, Work(0)),
            (0, Commit(Reply::Served)),
            (0, Work(0)),
            (2, Work(0)),
            (2, Effect(Outcome::Crash(Cut::Quarantined))),
            (2, drawn(3, 2582439459331434606, 3460261510432762190, 0)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_removal_done_before_a_crash_stays_the_engines_when_the_user_renames_its_folder_before_the_restart()
 {
    use Step::{Deliver, Effect, Work};
    let phases = [
        vec![(0, drawn(1, 0, 0, 0))],
        vec![
            (0, drawn(0, 0, 0, 1)),
            (0, drawn(3, 0, 1, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (0, Work(0)),
            (0, drawn(0, 0, 70021216061850914, 1)),
            (0, Work(0)),
            (2, drawn(4, 6524570367564971971, 0, 0)),
            (0, Work(0)),
            (1, Work(0)),
            (2, drawn(0, 0, 0, 0)),
            (0, Work(7978978420301962992)),
            (2, Work(5960645924473209624)),
            (0, drawn(0, 0, 0, 0)),
            (1, Deliver(0)),
            (1, Effect(Outcome::Crash(Cut::Done))),
            (1, drawn(3, 1, 0, 2)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (2, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (1, drawn(3, 0, 2436352964411405372, 0)),
            (0, drawn(0, 0, 0, 0)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_removal_done_before_a_crash_stays_the_engines_when_the_user_moves_things_before_the_restart() {
    use Step::*;
    let cases = [
        (
            2,
            vec![
                vec![(0, drawn(1, 0, 0, 0))],
                vec![
                    (0, drawn(0, 0, 0, 1)),
                    (0, drawn(3, 0, 1, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (0, Work(0)),
                    (0, drawn(4, 13115122292465283349, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (2, Work(0)),
                    (2, Work(0)),
                    (1, Work(0)),
                    (0, Work(0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, Work(0)),
                    (1, Effect(Outcome::Run)),
                    (2, drawn(0, 0, 0, 0)),
                    (1, Work(1317200100323545829)),
                    (0, drawn(0, 0, 0, 0)),
                    (1, Deliver(0)),
                    (0, drawn(0, 0, 0, 0)),
                    (1, Effect(Outcome::Crash(Cut::Done))),
                    (1, drawn(3, 1, 0, 2)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, drawn(1, 0, 0, 0))],
                vec![
                    (0, drawn(0, 0, 0, 1)),
                    (0, drawn(3, 0, 1, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, drawn(4, 1, 0, 0)),
                    (0, Work(0)),
                    (1, drawn(4, 21766022428867249, 0, 0)),
                    (0, Work(0)),
                    (1, Work(2350861549685903569)),
                    (1, Commit(Reply::NoChunks)),
                    (0, Work(0)),
                    (0, Effect(Outcome::Crash(Cut::Done))),
                    (0, drawn(3, 0, 2428384108005644156, 0)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, drawn(1, 0, 0, 0))],
                vec![
                    (0, drawn(0, 0, 0, 1)),
                    (0, drawn(3, 0, 1, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, drawn(3, 1, 0, 2)),
                    (1, drawn(1, 0, 14641257299881429946, 0)),
                ],
                vec![
                    (2, drawn(4, 13222924581270380077, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, Work(0)),
                    (2, Work(0)),
                    (2, Work(0)),
                    (1, Work(0)),
                    (1, Effect(Outcome::Crash(Cut::Done))),
                    (1, drawn(3, 493611944506893306, 15420572020311059222, 1)),
                ],
            ],
        ),
        (
            2,
            vec![
                vec![(0, drawn(1, 0, 0, 0))],
                vec![
                    (0, drawn(0, 0, 0, 1)),
                    (0, drawn(3, 0, 1, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, Work(0)),
                    (1, drawn(4, 1, 0, 0)),
                    (0, drawn(0, 0, 1537889247217748016, 1)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(0, 0, 1661168991043558250, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(0, 0, 1678253454579225120, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(0, 0, 1371832607312978784, 0)),
                    (0, drawn(4, 261677229681876137, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                ],
                vec![
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(0, 0, 5107279709152449721, 0)),
                    (0, Work(0)),
                    (0, Work(0)),
                    (0, drawn(0, 0, 20920097817466219, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (0, drawn(2, 0, 0, 0)),
                    (2, drawn(4, 9152466852032898481, 0, 0)),
                    (1, Work(0)),
                    (1, drawn(0, 0, 1650797344649676390, 1)),
                    (1, Work(0)),
                    (0, drawn(0, 0, 0, 2)),
                    (0, Work(0)),
                    (2, Work(0)),
                    (2, Work(0)),
                    (0, drawn(0, 0, 0, 0)),
                    (2, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (0, drawn(0, 0, 0, 0)),
                    (1, Work(0)),
                    (1, Effect(Outcome::Crash(Cut::Quarantined))),
                    (1, drawn(3, 0, 4370877354255850596, 2)),
                ],
            ],
        ),
    ];
    for (devices, phases) in cases {
        converge(devices, &phases).unwrap();
    }
}

#[test]
fn a_file_removed_for_a_trashed_folder_after_its_user_renamed_it_is_downloaded_when_another_device_brings_it_back()
 {
    use Step::*;
    let phases = vec![
        vec![(0, drawn(1, 0, 0, 0))],
        vec![
            (0, drawn(0, 0, 0, 1)),
            (0, drawn(3, 0, 1, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (2, drawn(0, 0, 0, 2)),
            (1, drawn(0, 0, 1048609101496807627, 0)),
            (0, Work(0)),
            (1, drawn(2, 0, 0, 0)),
            (0, Scan),
            (0, Scan),
            (1, drawn(0, 0, 571782100628878603, 0)),
            (2, Duplicate(0)),
            (0, drawn(3, 0, 8731566814113853748, 0)),
            (0, Effect(Outcome::Run)),
            (0, drawn(3, 4351509377776803521, 3824676115574542730, 2)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (2, Work(0)),
            (0, Commit(Reply::Served)),
            (1, drawn(4, 1, 0, 0)),
            (1, Scan),
            (1, Commit(Reply::Served)),
            (1, Commit(Reply::Served)),
            (1, Deliver(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (2, drawn(1, 0, 1, 2)),
            (2, Scan),
            (2, Commit(Reply::Served)),
            (2, Commit(Reply::Served)),
            (2, Deliver(0)),
            (1, drawn(4, 1, 0, 0)),
            (0, drawn(2, 2799465753081984595, 0, 0)),
            (2, Work(3448151242191943654)),
            (2, Work(0)),
            (0, Work(0)),
            (2, drawn(3, 136075407350998388, 2078940460315561838, 1)),
            (2, Scan),
            (0, drawn(0, 0, 0, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, drawn(0, 0, 0, 0)),
            (2, Effect(Outcome::Run)),
            (2, Work(6906514558975687719)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(3, 0, 5077622020572340662, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, Work(0)),
            (0, drawn(3, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, Work(0)),
            (0, drawn(0, 0, 0, 0)),
            (0, drawn(0, 0, 0, 0)),
            (0, Work(0)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_removal_done_before_a_crash_stays_the_engines_when_the_user_renamed_its_file_away_and_back_since_the_plan()
 {
    let phases = vec![
        vec![(0, folder("A")), (0, user(1, 0, 1, "a"))],
        vec![
            (2, delete(0)),
            (2, SCAN),
            (2, COMMIT),
            (2, COMMIT),
            (1, DELIVER),
            (1, DELIVER),
            (1, DELIVER),
            (1, EFFECT),
            (1, rename(1, 1, "c.txt")),
            (1, SCAN),
            (1, rename(1, 1, "a")),
            (1, CRASH),
            (1, rename(0, 0, "b")),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_removal_done_before_a_crash_stays_the_engines_when_the_user_renamed_its_folder_away_and_back_since_the_plan()
 {
    let phases = vec![
        vec![(0, folder("A")), (0, user(1, 0, 1, "a"))],
        vec![
            (2, delete(0)),
            (2, SCAN),
            (2, COMMIT),
            (2, COMMIT),
            (1, DELIVER),
            (1, DELIVER),
            (1, DELIVER),
            (1, EFFECT),
            (1, rename(0, 0, "c.txt")),
            (1, SCAN),
            (1, rename(0, 0, "A")),
            (1, CRASH),
            (1, rename(0, 0, "b")),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_folder_a_restore_loses_and_another_device_resends_at_its_pending_name_takes_its_owed_name() {
    converge(
        2,
        &[
            vec![
                (
                    1,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 4,
                        text: 0,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    0,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 15062546961989128223,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (0, Step::Backup),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 5,
                        text: 0,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 3,
                        text: 0,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 7732960054950755226,
                        parent: 5715037473979142591,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Step::Work(0)),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Step::Work(913565646028813000)),
                (0, Step::Work(2675562080131898980)),
                (1, Step::Work(0)),
                (0, Step::Restore),
                (1, Step::Work(0)),
                (
                    0,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 15650650006150013979,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_back_to_the_pending_name_after_a_rename_to_the_owed_name_is_kept() {
    let mut steps = vec![
        (0, folder("A")),
        (0, rename(0, 1, "a")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
    ];
    steps.extend(std::iter::repeat_n((1, Step::Work(0)), 5));
    steps.extend([(1, rename(1, 0, "A")), (1, SCAN), (1, Step::Work(0))]);
    steps.extend([(1, rename(1, 0, PENDING_NAME)), (1, SCAN)]);
    steps.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    steps.push((0, Step::Connect));
    steps.extend(std::iter::repeat_n((0, Step::Work(0)), 16));
    steps.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    let world = converge(2, &[vec![(0, file("a"))], steps]).unwrap();
    for device in &world.devices {
        let dirs: Vec<&str> = (device.disk.entries().values())
            .filter(|entry| entry.content.is_none())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(dirs, [PENDING_NAME]);
    }
}

#[test]
fn a_rename_back_to_the_pending_name_racing_a_delete_does_not_bring_back_the_folders_files() {
    let mut steps = vec![(0, folder("A")), (0, rename(0, 1, "a"))];
    steps.extend(std::iter::repeat_n((0, Step::Work(0)), 4));
    steps.extend([(0, rename(1, 0, PENDING_NAME)), (0, SCAN)]);
    steps.extend(std::iter::repeat_n((1, Step::Work(0)), 5));
    steps.extend([(1, delete(1)), (1, SCAN)]);
    steps.extend(std::iter::repeat_n((0, Step::Work(0)), 3));
    steps.extend(std::iter::repeat_n((1, Step::Work(0)), 3));
    steps.extend(std::iter::repeat_n((0, Step::Work(0)), 12));
    let world = converge(2, &[vec![(0, file("a"))], steps]).unwrap();
    for device in &world.devices {
        assert!(
            device.disk.entries().is_empty(),
            "{:?}",
            device.disk.entries()
        );
    }
}

#[test]
fn a_folder_keeps_no_pending_name_through_rewinds_and_store_rollbacks() {
    use Reply::*;
    use Step::*;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, act(0, 0, 0, "a", 0)),
            (0, act(1, 0, 6980218576785084698, "ß", 3)),
            (1, act(2, 0, 0, "a", 0)),
            (2, act(1, 0, 4695922804375436334, "A", 0)),
            (0, Work(0)),
            (0, act(1, 0, 2890056923779536055, "b", 3)),
            (1, act(1, 5727942886410326177, 13018960219685472353, "b", 3)),
            (2, act(1, 5196906716127809974, 521330231311856237, "b", 2)),
            (0, act(3, 10333777345206546146, 4786810669791805283, "a", 2)),
            (0, Work(1209161008911898132)),
            (2, Work(6267707061764293434)),
            (2, Work(2981037690173829694)),
        ],
        vec![
            (0, Commit(Served)),
            (
                1,
                act(3, 13226239514003869164, 8790259169687068798, "ss", 2),
            ),
            (2, act(2, 18071194833586715547, 6218949041632901671, "b", 2)),
            (2, Deliver(15695387385703223024)),
            (1, act(0, 9345220605139681005, 16953462668439231719, "A", 0)),
            (2, Work(9244999448452558495)),
            (0, Work(3915609581973158506)),
            (
                0,
                act(1, 14792911615287690638, 12100971707851482584, "ss", 2),
            ),
            (
                0,
                act(3, 8310676636251247845, 15856905699621842553, "c.txt", 3),
            ),
            (1, Work(17144717332709473615)),
            (0, Backup),
            (
                0,
                act(3, 5787055606195510634, 18053727275733928969, "c.txt", 3),
            ),
            (1, Commit(Served)),
            (1, Scan),
            (0, Effect(Outcome::Run)),
            (0, Drop),
            (0, act(3, 4293234221220403339, 4735398761017018611, "b", 2)),
            (0, Work(9409735233393576633)),
            (0, Work(13389529914642427933)),
            (0, Work(2014361990739751959)),
            (0, Deliver(6317153904806597135)),
            (1, Work(15069274704306882784)),
            (1, Work(5504084953793399719)),
            (0, Effect(Outcome::Run)),
            (2, Backup),
            (0, Effect(Outcome::Fail)),
            (
                2,
                act(3, 16187060239217063454, 11560462164387002550, "ss", 2),
            ),
            (2, Backup),
            (2, Backup),
            (1, Effect(Outcome::Run)),
            (1, Commit(Served)),
            (2, RollBack),
            (0, Work(6424897993824961991)),
            (
                0,
                act(1, 11997499721189524715, 15963535816863556402, "ss", 0),
            ),
        ],
        vec![
            (1, Commit(Served)),
            (2, Work(11170281843187104097)),
            (1, Connect),
            (1, Connect),
            (1, Effect(Outcome::Run)),
            (1, Work(8870421899473166975)),
            (
                0,
                act(3, 12536107549687834871, 11346689659078643522, "a", 2),
            ),
            (1, Work(3100904301983880638)),
            (1, Duplicate(309888326988107569)),
            (1, Rewind),
            (1, Deliver(343719886465334017)),
            (0, Work(5905904977128725049)),
            (1, Deliver(9552529758631265802)),
            (0, Work(2264303773289144885)),
            (2, Work(716016871678579446)),
            (1, Commit(Served)),
            (
                1,
                act(2, 6419776216784042297, 11556474060382594156, "C.TXT", 0),
            ),
            (2, Work(7537211416594830433)),
            (0, Commit(Served)),
            (1, Effect(Outcome::Crash(Cut::Done))),
            (0, Work(1765335398415290723)),
            (0, RollBack),
            (2, Deliver(15714596177551913368)),
            (1, Connect),
            (0, CopyStore),
            (0, Rewind),
            (2, Work(6901520755494505989)),
            (2, Work(7780906830979336587)),
            (1, Work(17052687445418157714)),
            (1, Work(15153470601748795340)),
            (2, Scan),
            (1, Commit(Served)),
            (2, Commit(Served)),
            (0, Work(5329214217807541593)),
            (2, Work(1679697755313522790)),
            (0, CopyStore),
            (1, Commit(Served)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_node_keeps_no_pending_name_after_a_store_copy_rolls_back() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (1, act(1, 0, 0, "a", 0)),
            (0, CopyStore),
            (1, act(1, 0, 0, "A", 0)),
            (0, act(1, 0, 0, "ß", 0)),
        ],
        vec![
            (0, act(0, 0, 0, "ss", 0)),
            (0, act(0, 0, 6465797623138868743, "a", 0)),
            (0, act(3, 1283989912345104580, 2925195757298698418, "a", 0)),
            (0, act(0, 0, 10031711809211702159, "A", 0)),
            (0, Work(0)),
            (0, Commit(Served)),
            (0, Work(0)),
            (0, RollBack),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_folder_keeps_no_pending_name_after_a_rewind_on_two_devices() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (2, act(1, 0, 0, "b", 0)),
            (2, act(1, 0, 0, "A", 0)),
            (0, Backup),
            (0, Rewind),
            (0, act(0, 0, 0, "a", 0)),
        ],
        vec![(0, act(3, 8263766555677971211, 11199937710435355667, "ß", 0))],
        vec![
            (2, act(0, 0, 2823199969817622545, "ss", 0)),
            (2, act(3, 3811247810905055373, 3162816818029534310, "a", 0)),
            (0, Work(0)),
            (0, Commit(Served)),
            (2, Commit(Served)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (0, Work(0)),
            (2, act(2, 15847390447526599281, 0, "a", 0)),
            (0, act(0, 0, 0, "a", 0)),
            (0, act(0, 0, 0, "a", 0)),
            (0, Rewind),
            (1, act(0, 0, 0, "a", 0)),
            (0, act(0, 0, 0, "a", 0)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn an_owed_rename_converges_when_another_device_deletes_the_folder_and_a_third_crashes_mid_effect()
{
    use Step::*;
    let phases = vec![
        vec![(0, act(1, 0, 0, "a", 0))],
        vec![
            (0, act(0, 0, 0, "A", 0)),
            (0, act(3, 0, 1, "a", 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, act(4, 1, 0, "a", 0)),
            (2, Work(0)),
            (2, Effect(Outcome::Crash(Cut::Placed))),
            (2, act(3, 6424879429746079920, 0, "b", 0)),
            (0, Work(0)),
            (2, Work(0)),
            (2, Effect(Outcome::Run)),
            (0, Work(0)),
            (0, act(3, 3798803182519135973, 324470043846035000, "a", 0)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_node_keeps_no_pending_name_after_the_server_refuses_a_commit_as_too_large() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (1, Work(16274642324747499124)),
            (1, Connect),
            (0, Effect(Outcome::Run)),
            (2, act(4, 4540723837931170798, 13161747283576780999, "ß", 3)),
            (0, Scan),
            (1, Work(14001882461254173322)),
            (1, Deliver(10280263283177231077)),
            (0, Effect(Outcome::Crash(Cut::Placed))),
            (0, Work(3150538074527436651)),
            (1, Drop),
            (2, Commit(Served)),
            (0, Commit(Served)),
            (0, act(1, 3074524351106235926, 15622149868922993437, "ß", 0)),
            (2, Work(12709215057847084410)),
            (2, Work(13195197093511105023)),
            (0, act(1, 2007127410610402069, 14009461238078843770, "b", 3)),
            (2, act(1, 4636927101850804754, 7194277078085297665, "b", 1)),
            (0, Work(8321255590853366247)),
            (2, Scan),
            (1, Effect(Outcome::Crash(Cut::Done))),
            (2, Work(13496689005560533593)),
            (0, Drop),
            (2, act(2, 966963481405597494, 8245514377295365874, "ß", 0)),
            (
                1,
                act(1, 225941396932957162, 14792149289048557706, "c.txt", 0),
            ),
            (0, Commit(Served)),
            (1, Commit(NoChunks)),
            (0, Drop),
            (
                2,
                act(2, 9006010326599423669, 12030360673653429816, "ss", 1),
            ),
            (0, Crash),
            (0, Work(10532977848492514630)),
            (0, Deliver(9482183767260581062)),
            (2, Scan),
            (2, Work(7987139506101263081)),
            (0, Work(10342087221633046513)),
            (1, Commit(Served)),
            (1, Work(6790053865611655878)),
            (
                2,
                act(3, 7525507307544980790, 11794627914537811387, "c.txt", 3),
            ),
            (2, act(3, 4222423626720987175, 7107160187266153721, "a", 2)),
        ],
        vec![
            (1, Effect(Outcome::Crash(Cut::Quarantined))),
            (1, Effect(Outcome::Fail)),
            (0, Commit(Served)),
            (1, Work(14776020963067605780)),
            (1, Duplicate(5337902825096339581)),
            (
                2,
                act(2, 13901156469230108672, 11150338100498388762, "b", 3),
            ),
            (2, Work(7038216941225261646)),
            (0, Work(2320670095518912407)),
            (2, Effect(Outcome::Run)),
            (2, Work(6702283563481621023)),
            (0, Effect(Outcome::Crash(Cut::Placed))),
            (2, Work(9488113402077087457)),
            (0, Work(918825349661777991)),
            (2, Deliver(7230566688530807512)),
            (
                0,
                act(3, 11435112669801920197, 1797153937022296474, "C.TXT", 2),
            ),
            (0, act(2, 7120082117506032661, 6750420819364943549, "a", 1)),
            (1, Commit(Served)),
            (2, Commit(Served)),
            (0, Work(8407811867281122132)),
            (2, Commit(Served)),
            (2, Work(1884201276037343489)),
            (1, Work(7135757195114882032)),
            (2, Work(14687530289417874012)),
            (2, Work(2956825658628593075)),
            (0, act(3, 6865901930919790778, 8288472523108843720, "b", 0)),
            (0, Work(5124846217184249120)),
            (0, Deliver(10658715262855600137)),
            (0, Commit(Refused(error::Code::LimitExceeded))),
            (2, Work(15438777473916274418)),
            (2, Commit(Served)),
            (1, Crash),
            (0, Effect(Outcome::Crash(Cut::Done))),
            (
                1,
                act(2, 17703314385067399982, 17656894473945846841, "ß", 0),
            ),
            (0, Connect),
            (2, Duplicate(15079964782730117166)),
            (0, Work(17321021939675765847)),
            (0, act(1, 3475983964813192413, 3142939760521600814, "A", 1)),
            (0, act(1, 1603506240037526712, 12162886444757042084, "A", 0)),
            (0, Work(11201822032332675027)),
        ],
        vec![
            (2, Scan),
            (2, Crash),
            (2, Drop),
            (0, Work(6015854605376741563)),
            (2, Work(14327447256282067683)),
            (2, Work(13842812011217849245)),
            (1, Work(14146307605526031779)),
            (2, Duplicate(16712681527797065823)),
            (
                2,
                act(4, 337520903820340153, 3629187529615689769, "c.txt", 0),
            ),
            (0, Scan),
            (0, Effect(Outcome::Run)),
            (2, Crash),
            (2, Duplicate(18096242046457636208)),
            (1, Deliver(736796987841017770)),
            (0, Commit(Served)),
            (0, Work(13078245784386167280)),
            (0, Effect(Outcome::Run)),
            (2, Connect),
            (1, Effect(Outcome::Run)),
            (0, Scan),
            (0, Deliver(3633961901045387142)),
            (0, Connect),
            (1, Work(5790155795775849372)),
            (1, Work(17994945317265741320)),
            (0, Commit(NoChunks)),
            (1, Commit(Served)),
            (2, Work(11664352604933389001)),
            (1, Deliver(3379023900342304746)),
            (0, Scan),
            (1, Drop),
            (1, Work(4608420094548971993)),
            (1, Scan),
            (0, Deliver(8678034364532486451)),
            (0, Connect),
            (2, Work(5903877029157569881)),
            (1, Work(2457473270522847108)),
            (2, Duplicate(10767262520509640516)),
            (2, Duplicate(4851272950074024160)),
            (1, Deliver(11340430273384828348)),
            (1, Work(15569514617193981134)),
            (2, Commit(Served)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_node_named_with_a_sharp_s_keeps_no_pending_name() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (2, Work(1473437278776159742)),
            (0, Scan),
            (0, Deliver(5064889799949271536)),
            (2, Effect(Outcome::Crash(Cut::Done))),
            (0, act(3, 2600449979829275164, 12315716652721855868, "b", 1)),
            (1, Drop),
            (1, Deliver(7622003434601907495)),
            (
                0,
                act(4, 610184442125577405, 17180182887072736626, "C.TXT", 0),
            ),
            (2, Work(16154721216578655822)),
            (1, Work(6955574176339537202)),
            (1, Work(16580796679091681569)),
            (1, act(2, 18234325593230726174, 1192252787774071987, "A", 0)),
            (
                1,
                act(1, 3968332894258334906, 18140202293555168809, "ss", 0),
            ),
            (1, Crash),
            (2, Work(5517839197043865337)),
            (1, Connect),
            (2, Commit(Served)),
            (1, act(1, 5621156745924127467, 81296433174544148, "A", 1)),
            (2, Drop),
            (1, Scan),
            (1, Connect),
            (
                2,
                act(4, 12257017893927675056, 10883324420488777458, "A", 2),
            ),
        ],
        vec![
            (2, Work(12808829587052721832)),
            (1, Work(777768548259338378)),
            (1, Work(3804164300867469701)),
            (0, Work(14491811008877662428)),
            (0, Work(14281551358534990101)),
            (
                0,
                act(1, 14874945564043600677, 10401856113715591309, "ß", 3),
            ),
            (2, act(2, 11394225905497013919, 5040751402013322049, "A", 1)),
            (1, act(0, 10553893793162445828, 4604247632691194192, "ß", 1)),
            (1, Effect(Outcome::Run)),
            (0, Effect(Outcome::Run)),
            (2, Effect(Outcome::Run)),
            (0, Deliver(10641815047390936352)),
            (1, act(3, 1879675841506811055, 18165067502593608789, "ß", 1)),
            (
                0,
                act(1, 11709759775262021255, 11855892359315835031, "ss", 0),
            ),
            (1, Deliver(9261853906572453175)),
            (
                0,
                act(2, 9335828141592495702, 2028375343372489393, "C.TXT", 1),
            ),
            (2, act(3, 5062492614778579626, 9374327271361740376, "a", 3)),
            (1, Deliver(5223581525985808299)),
            (1, Work(13138580071435398981)),
            (1, Work(359186028269978254)),
            (2, Work(10085283306038807177)),
            (1, Effect(Outcome::Run)),
            (2, Work(11598692035777226021)),
            (2, Drop),
            (0, Connect),
            (2, Deliver(6510062360456695788)),
            (0, Work(15243356548299814365)),
            (2, Work(8943011731916699863)),
            (2, Work(8555699403372609436)),
            (0, Work(4338210854930325824)),
            (0, Connect),
            (
                2,
                act(3, 14487921485606286637, 10002135837829220646, "ss", 0),
            ),
            (1, act(4, 10808422865532203273, 7358175393496224460, "A", 0)),
            (0, Work(575203591234896055)),
            (2, Work(14441298908365096425)),
            (1, Work(3934485262074807149)),
            (0, Work(1652176079374840457)),
            (
                0,
                act(2, 111329675772431266, 17637213339518120693, "C.TXT", 3),
            ),
            (2, Work(2130967908856300758)),
            (2, Effect(Outcome::Fail)),
            (2, Work(15816732155870203862)),
            (1, Work(12619941101547758715)),
            (2, Commit(Served)),
            (0, Drop),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn rewinds_and_rollbacks_leave_no_pending_name_from_seed_4101() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (1, Duplicate(12201702494159087721)),
            (
                1,
                act(1, 16208656888390882433, 16651269268201903015, "b", 0),
            ),
            (2, Effect(Outcome::Crash(Cut::Done))),
            (0, Commit(Served)),
            (2, Answer(3)),
            (1, Commit(Served)),
        ],
        vec![
            (0, Work(4990326382468840636)),
            (2, Answer(3)),
            (1, Backup),
            (1, Drop),
            (
                0,
                act(2, 12953377292721208193, 6083887852907627030, "C.TXT", 3),
            ),
            (1, Scan),
            (0, Deliver(12999514086249209230)),
            (0, Work(3095441835775971339)),
            (
                1,
                act(0, 16133926611725548822, 14163026437500845381, "ss", 0),
            ),
            (2, RollBack),
            (2, Deliver(6166741201608057741)),
            (1, Connect),
            (1, Scan),
            (2, act(1, 5405045464409950487, 3658666308455548170, "ss", 3)),
            (0, Duplicate(1254543765751448050)),
            (0, Work(9998331135794321721)),
            (2, Work(8614624186327492318)),
            (
                2,
                act(2, 13324077622336117867, 2270324095945454398, "c.txt", 2),
            ),
            (1, Effect(Outcome::Crash(Cut::Quarantined))),
            (
                1,
                act(2, 13494257421393879220, 1932126861546895880, "c.txt", 1),
            ),
            (
                0,
                act(2, 14151105744198356111, 14754840180731170313, "ß", 3),
            ),
            (2, Effect(Outcome::Run)),
            (0, Work(2126284905842120105)),
            (2, CopyStore),
            (0, Rewind),
            (1, Scan),
            (2, Effect(Outcome::Run)),
            (0, Work(3652112711229384834)),
            (
                2,
                act(2, 859526067160286431, 12098173981811372854, "C.TXT", 2),
            ),
            (2, Commit(Served)),
            (1, Effect(Outcome::Run)),
            (0, Work(17006286246632568303)),
            (0, Work(7931201457199238787)),
            (2, Answer(1)),
            (0, Duplicate(15743290801180678016)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_folder_a_rewind_keeps_does_not_stay_parked_once_its_finish_ends() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (0, Effect(Outcome::Crash(Cut::Done))),
            (1, Work(11498562431919686616)),
            (2, CopyStore),
            (0, CopyStore),
            (1, Connect),
            (1, Scan),
            (0, Answer(3)),
            (2, act(1, 650067070186639843, 16728398050340516224, "A", 3)),
            (
                0,
                act(2, 1846555453511110917, 4042893111013256509, "C.TXT", 3),
            ),
            (
                2,
                act(3, 9459665342546834481, 7938303913219162098, "C.TXT", 1),
            ),
            (
                0,
                act(3, 9228555199316982540, 12130702332831684202, "c.txt", 0),
            ),
            (1, Connect),
            (0, Work(2043695299286265290)),
            (2, CopyStore),
            (0, Rewind),
            (
                2,
                act(1, 15440502346381658944, 13545739534008050857, "ß", 1),
            ),
            (1, Commit(Served)),
            (1, Effect(Outcome::Run)),
            (1, Deliver(8070148154267014932)),
            (0, Work(18380821530554570581)),
            (2, act(3, 9262711800925617647, 3838674182152374832, "b", 3)),
            (0, Commit(NoChunks)),
            (2, RollBack),
            (1, Commit(Served)),
            (0, Work(8479721805482730890)),
            (1, Work(16338716818764410016)),
        ],
        vec![
            (0, Backup),
            (0, Work(1417874290306454951)),
            (2, Work(18124674199809656934)),
            (
                0,
                act(0, 17237120146120376232, 18195066696380191202, "C.TXT", 2),
            ),
            (1, Commit(Served)),
            (0, act(3, 2266382684634142023, 3074742521628779503, "ß", 3)),
            (0, Commit(Served)),
            (1, act(2, 7510987292787035349, 13777006115484315862, "b", 2)),
            (2, Work(4390417409880896892)),
            (2, Work(4359960769589210347)),
            (0, Scan),
            (
                0,
                act(3, 15716338502206233161, 3124920059653504933, "ss", 2),
            ),
            (2, Work(3073944216712617256)),
            (0, Work(5378523195117555757)),
            (1, act(1, 7203455110169052305, 7355033864535803944, "a", 1)),
            (2, Work(538239389620674184)),
            (2, Drop),
            (0, act(2, 14543532078270974118, 4444558039971041101, "A", 3)),
            (2, Scan),
            (0, Effect(Outcome::Crash(Cut::Quarantined))),
            (2, Work(601263882827506204)),
            (0, Answer(2)),
            (0, Rewind),
            (1, Work(4459133671683880391)),
            (2, Work(5917365101121790888)),
            (1, Effect(Outcome::Run)),
            (0, Work(12017090419718457781)),
            (1, Work(10542756604648655888)),
            (0, Rewind),
            (0, Work(6460793537237402427)),
            (1, Connect),
            (0, act(1, 4893805322481635328, 9883675710172109247, "b", 3)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_recovery_answer_does_not_put_a_disk_first_finish_back_at_its_pending_name() {
    use Reply::*;
    use Step::*;
    let phases = vec![
        vec![
            (1, CopyStore),
            (2, Work(15524995717573726893)),
            (0, act(3, 5330074408213598593, 4629833235172449602, "ß", 2)),
            (
                0,
                act(2, 12221271103899464028, 4478685092722496479, "ss", 2),
            ),
            (1, Work(12499432487785595239)),
            (2, act(1, 8621587523375636320, 812792007868294767, "a", 0)),
            (2, Work(14272777250311830062)),
            (1, Scan),
            (2, Deliver(645643439379162787)),
            (0, Work(10106057172731844153)),
            (2, Work(12575155775222267162)),
            (
                1,
                act(0, 16811806145786392238, 6332559109285723292, "ss", 1),
            ),
            (0, Work(17235474259589030873)),
            (2, Duplicate(6064481305356187095)),
            (0, Work(15134368918629876866)),
            (1, Work(5395259734129689130)),
            (2, Work(185346845293437356)),
            (2, Work(16872400819756787932)),
            (0, Effect(Outcome::Run)),
            (
                2,
                act(1, 15488856375784821386, 15380667954563416130, "A", 3),
            ),
            (2, Work(14192248382811230156)),
            (1, Deliver(8753804467369860593)),
            (0, Duplicate(12930602246396409119)),
            (
                1,
                act(0, 5992940940953252200, 833343985184761634, "c.txt", 2),
            ),
            (0, act(3, 13318175572436212200, 6067706390614713363, "b", 1)),
            (1, Rewind),
            (0, Drop),
            (
                0,
                act(1, 18155696389742796712, 9802351646835555614, "ss", 3),
            ),
            (2, Effect(Outcome::Fail)),
            (2, CopyStore),
            (0, Drop),
            (0, Backup),
            (0, Scan),
            (2, act(1, 9673952879272549330, 4771081151139157093, "a", 1)),
            (2, Effect(Outcome::Crash(Cut::Quarantined))),
            (0, Work(11794219750606228031)),
        ],
        vec![
            (0, Deliver(12801337703215765660)),
            (2, Work(5749732312395039133)),
            (
                2,
                act(3, 11534569491105076621, 11994327746722269687, "ß", 2),
            ),
            (
                1,
                act(3, 9961964945745086683, 16164261331459748022, "ss", 2),
            ),
            (0, Deliver(14329710640992089234)),
            (0, Work(8166556500286514750)),
            (1, Scan),
            (
                1,
                act(3, 116357971881685100, 11286622196720506033, "C.TXT", 2),
            ),
            (2, Drop),
            (1, CopyStore),
            (2, Answer(2)),
            (
                2,
                act(1, 17040365570366669079, 16730090114021289938, "c.txt", 2),
            ),
            (0, Work(3819232277731380720)),
            (1, Answer(2)),
            (2, CopyStore),
            (0, Work(3295063082959724920)),
            (1, Work(4175141740836256450)),
            (2, Work(7933023299039968830)),
            (2, Effect(Outcome::Run)),
            (0, Work(18241310793696651180)),
            (1, Drop),
            (0, Rewind),
            (1, Work(18037430195124713214)),
            (1, Work(15885039972047628923)),
            (1, Answer(3)),
            (2, Effect(Outcome::Run)),
            (
                2,
                act(0, 7919009089406704247, 15376120775459737589, "ss", 1),
            ),
            (2, Commit(Served)),
            (0, Effect(Outcome::Crash(Cut::Quarantined))),
            (0, Connect),
            (1, Work(12820724825741600532)),
            (1, Effect(Outcome::Crash(Cut::Done))),
            (1, Work(2638443124996853254)),
            (2, Work(3675057622066958286)),
            (1, act(3, 10083539701709694594, 9492779228744250978, "A", 0)),
            (2, Work(2442558545608522614)),
            (0, Work(760011312421144006)),
            (1, act(1, 5758191652303931555, 10298687174070016111, "A", 2)),
            (1, Work(2036204420601162984)),
            (
                1,
                act(3, 10351665611691567325, 16226793509251689751, "c.txt", 2),
            ),
            (1, Effect(Outcome::Fail)),
            (0, Crash),
            (1, RollBack),
            (2, Commit(Served)),
            (1, Rewind),
            (2, Work(13124782941814221408)),
            (0, act(1, 1356251684355635118, 14199346698827690534, "A", 1)),
        ],
        vec![
            (2, Commit(Served)),
            (0, Work(16056836935150151654)),
            (1, act(1, 8302703626807829992, 8731752035660800897, "b", 3)),
            (1, Effect(Outcome::Run)),
            (1, Deliver(5086885261438442783)),
            (1, Work(11029677862946282605)),
            (2, Effect(Outcome::Fail)),
            (0, Commit(Served)),
            (2, RollBack),
            (0, Work(9884379898888661469)),
            (2, act(0, 8691432408064339738, 3289483588611559551, "ss", 1)),
            (0, Commit(Served)),
            (2, Commit(Refused(error::Code::RetryLater))),
            (2, Work(5339754506212797414)),
            (1, Work(6858298114812976162)),
            (1, Deliver(12351839751664690987)),
            (1, Effect(Outcome::Crash(Cut::Quarantined))),
            (1, Answer(2)),
            (2, Deliver(6713080964176822786)),
        ],
    ];
    converge(2, &phases).unwrap();
}
