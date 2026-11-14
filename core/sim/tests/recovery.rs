// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::commit::{Commits, InFlight};
use koidrive_engine::question::{Answer, QuestionKind, RecoveryCause, RecoveryTrigger};
use koidrive_proto::v1::{Commit, Move, NodeStatus, Op, error, op};
use koidrive_sim::world::disk::{Disk, Place};
use koidrive_sim::world::{Action, Cut, Failure, NAMES, Outcome, Reply, Step, World};

fn user(kind: u8, target: usize, parent: usize, name: &str) -> Step {
    let name = NAMES.iter().position(|other| *other == name).unwrap();
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

fn file(name: &str) -> Step {
    user(1, 0, 0, name)
}

fn overwrite(target: usize) -> Step {
    user(2, target, 0, "a")
}

fn rename(target: usize, name: &str) -> Step {
    user(3, target, 0, name)
}

fn delete(target: usize) -> Step {
    user(4, target, 0, "a")
}

const SCAN: Step = Step::Scan;
const EFFECT: Step = Step::Effect(Outcome::Run);
const COMMIT: Step = Step::Commit(Reply::Served);
const DELIVER: Step = Step::Deliver(0);

fn commit(device: usize) -> Vec<(usize, Step)> {
    vec![
        (device, SCAN),
        (device, COMMIT),
        (device, COMMIT),
        (device, DELIVER),
    ]
}

fn synced(devices: usize, phases: &[Vec<(usize, Step)>]) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
    world
}

fn live(world: &World) -> Vec<String> {
    let root = world.space().space_id().to_owned();
    let mut names: Vec<String> = (world.space().nodes().into_iter())
        .filter(|node| node.node_id != root && node.status == NodeStatus::Live as i32)
        .map(|node| node.name)
        .collect();
    names.sort();
    names
}

fn refusal(world: &World, d: usize, commit: &Commit) -> Option<error::Code> {
    let mut space = world.space().clone();
    let refused = space.commit(&world.devices[d].session, 0, commit).err()?;
    error::Code::try_from(refused.code).ok()
}

fn recovering(world: &World, d: usize) -> &str {
    (world.devices[d].recovery.as_deref())
        .unwrap_or_else(|| panic!("device {d} did not start recovery"))
}

#[test]
fn pitr_replay() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(
            1,
            &[
                (0, Step::Backup),
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, Step::Drop),
                (0, Step::Rewind),
                (0, Step::Connect),
            ],
        )
        .unwrap();
    assert_eq!(recovering(&world, 0), "the server's history changed");
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is kept as evidence")
        .commit
        .clone();
    assert_eq!(refusal(&world, 0, &k), Some(error::Code::HistoryMismatch));
    world.run(1, &[(0, DELIVER), (0, Step::Answer(1))]).unwrap();
    assert_eq!(world.devices[0].engine.commits.in_flight(), None);
    assert!(world.seen.contains_key("recovery answer restores"));
    world.phase(2, &[]).unwrap();
    recovering(&world, 1);
    assert_eq!(live(&world), ["a"]);
    assert_eq!(world.devices[0].disk.entries().len(), 1);
    assert_ne!(world.space().device_seq("device-0"), k.device_seq);
}

#[test]
fn history_change_on_connect() {
    let mut world = synced(
        2,
        &[
            vec![(0, Step::Backup), (0, file("a"))],
            vec![(1, file("b"))],
        ],
    );
    world.run(2, &[(0, Step::Rewind)]).unwrap();
    let before = world.devices[1].engine.clone();
    world.run(2, &[(1, Step::Connect)]).unwrap();
    recovering(&world, 1);
    assert_eq!(world.devices[1].engine.trees, before.trees);
    world.run(2, &[(1, Step::Work(0))]).unwrap();
    let trees = &world.devices[1].engine.trees;
    assert!(trees.synced().iter().eq(before.trees.synced().iter()));
    assert!(trees.local().iter().eq(before.trees.local().iter()));
    world
        .run(
            2,
            &[
                (0, Step::Connect),
                (0, DELIVER),
                (0, Step::Answer(0)),
                (1, Step::Connect),
                (1, DELIVER),
                (1, Step::Answer(1)),
            ],
        )
        .unwrap();
    assert!(world.seen.contains_key("recovery answer keeps"));
    assert!(world.seen.contains_key("recovery answer discards"));
    world.phase(3, &[]).unwrap();
    assert_eq!(live(&world), ["a", "b"]);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

#[test]
fn stale_store_replay() {
    let mut world = synced(1, &[]);
    world
        .run(
            0,
            &[
                (0, file("a")),
                (0, SCAN),
                (0, COMMIT),
                (0, Step::CopyStore),
                (0, COMMIT),
                (0, DELIVER),
                (0, file("b")),
            ],
        )
        .unwrap();
    world.run(0, &commit(0)).unwrap();
    world.truncate_log();
    world
        .run(
            0,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert!(recovering(&world, 0).contains("cannot account for"));
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is in flight in the copied store")
        .commit
        .clone();
    assert_eq!(refusal(&world, 0, &k), Some(error::Code::StaleDeviceState));
    world.phase(1, &[]).unwrap();
    assert_eq!(live(&world), ["a", "b"]);
}

#[test]
fn store_rollback_below_the_log_floor() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(
            0,
            &[
                (0, Step::CopyStore),
                (0, Step::Drop),
                (1, file("b")),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
            ],
        )
        .unwrap();
    world.truncate_log();
    world
        .run(
            1,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert!(world.seen.contains_key("resume finds the log truncated"));
    assert!(world.devices[0].recovery.is_none());
    world.phase(1, &[]).unwrap();
    assert_eq!(live(&world), ["a", "b"]);
}

#[test]
fn remote_rename_then_store_rollback() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (0, rename(0, "b"))])
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[
                (1, DELIVER),
                (1, EFFECT),
                (1, EFFECT),
                (1, Step::RollBack),
                (1, Step::Connect),
            ],
        )
        .unwrap();
    assert!(recovering(&world, 1).contains("root marker"));
    world.phase(2, &[]).unwrap();
    assert_eq!(live(&world), ["b"]);
    let disk = world.devices[1].disk.entries();
    assert_eq!(disk.len(), 1);
    assert!(disk.values().all(|entry| entry.name == "b"));
}

#[test]
fn remote_rename_then_store_rollback_and_a_delete() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (0, rename(0, "b"))])
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[(1, DELIVER), (1, EFFECT), (1, EFFECT), (1, Step::RollBack)],
        )
        .unwrap();
    world.run(1, &[(0, delete(0))]).unwrap();
    world.run(1, &commit(0)).unwrap();
    world.phase(2, &[]).unwrap();
    recovering(&world, 1);
    assert!(live(&world).is_empty());
    assert!(world.devices[1].disk.entries().is_empty());
}

#[test]
fn a_folder_parked_for_a_case_rename_then_store_rollback() {
    let mut world = synced(2, &[vec![(0, user(0, 0, 0, "c.txt"))]]);
    let written = Step::User(Action {
        kind: 1,
        target: 0,
        parent: 1,
        name: 2,
        text: 1,
    });
    world
        .phase(
            1,
            &[
                (1, Step::CopyStore),
                (1, written),
                (0, rename(0, "C.TXT")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, COMMIT),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (1, Step::RollBack),
            ],
        )
        .unwrap();
    world.phase(2, &[]).unwrap();
    world.phase(3, &[]).unwrap();
    let disk = &world.devices[1].disk;
    let visible = (disk.visible().into_iter())
        .filter_map(|ino| disk.entries()[&ino].content.as_ref())
        .any(|content| content.mtime_ns == 1);
    assert!(visible || live(&world).contains(&"b".to_owned()));
}

fn four_commits() -> World {
    let world = synced(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, file("b"))],
            vec![(0, overwrite(0))],
            vec![(0, overwrite(1))],
        ],
    );
    assert_eq!(world.space().device_seq("device-0"), 4);
    world
}

#[test]
fn accepted_response_lost() {
    let mut world = four_commits();
    world
        .run(
            4,
            &[
                (0, overwrite(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, Step::Drop),
            ],
        )
        .unwrap();
    assert_eq!(world.space().device_seq("device-0"), 5);
    world.phase(4, &[]).unwrap();
    assert!(world.devices.iter().all(|device| device.recovery.is_none()));
    assert!(
        world
            .seen
            .contains_key("reconnect resends a commit the server applied")
    );
    assert!(
        world
            .seen
            .contains_key("commit answered from the stored result")
    );
    assert_eq!(world.space().device_seq("device-0"), 5);
}

#[test]
fn refused_then_replan() {
    let mut world = four_commits();
    world
        .run(4, &[(0, overwrite(0)), (0, SCAN), (0, COMMIT)])
        .unwrap();
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is in flight")
        .commit
        .clone();
    assert_eq!(k.device_seq, 5);
    world
        .run(
            4,
            &[
                (0, overwrite(0)),
                (0, Step::Commit(Reply::NoChunks)),
                (0, DELIVER),
                (0, SCAN),
                (0, COMMIT),
            ],
        )
        .unwrap();
    let replanned = (world.devices[0].engine.commits.in_flight())
        .expect("the changes are planned again")
        .commit
        .clone();
    assert_eq!(replanned.device_seq, 5);
    assert_ne!(replanned.commit_id, k.commit_id);
    world.phase(4, &[]).unwrap();
    assert!(world.devices.iter().all(|device| device.recovery.is_none()));
    assert_eq!(world.space().device_seq("device-0"), 5);
}

#[test]
fn rewound_write_only_one_disk_holds() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    assert!(world.devices[0].disk.holds(1) && world.devices[1].disk.holds(1));
    world
        .run(
            1,
            &[
                (0, Step::Drop),
                (1, delete(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (0, Step::Rewind),
            ],
        )
        .unwrap();
    assert!(
        world
            .seen
            .contains_key("rewind takes a write only one disk holds")
    );
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_whose_delete_this_device_has_not_applied() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[
                (1, delete(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
            ],
        )
        .unwrap();
    assert!(world.devices[0].engine.effects.in_flight().is_empty());
    assert!(world.devices[0].disk.holds(1));
    world.run(1, &[(0, Step::Rewind)]).unwrap();
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_another_device_overwrote() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[
                (0, Step::Drop),
                (1, overwrite(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (1, delete(0)),
                (0, Step::Rewind),
            ],
        )
        .unwrap();
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_is_held_and_never_uploaded_unanswered() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world.run(1, &[(0, Step::Rewind)]).unwrap();
    let unanswered = [
        Step::Connect,
        DELIVER,
        SCAN,
        COMMIT,
        DELIVER,
        COMMIT,
        DELIVER,
    ];
    let schedule: Vec<(usize, Step)> = (unanswered.iter())
        .flat_map(|step| [(0, *step), (1, *step)])
        .collect();
    world.run(2, &schedule).unwrap();
    assert_eq!(live(&world), Vec::<String>::new());
    assert!(world.devices[0].disk.holds(1) && world.devices[1].disk.holds(1));
    world.phase(3, &[]).unwrap();
    assert_eq!(live(&world), ["a"]);
}

#[test]
fn crash_and_rollback_during_recovery() {
    for (fault, during) in [
        (Step::Crash, "crash during a recovery"),
        (Step::RollBack, "store rolled back during a recovery"),
    ] {
        let mut world = synced(1, &[vec![(0, Step::Backup), (0, file("a"))]]);
        world
            .run(
                1,
                &[(0, Step::CopyStore), (0, Step::Rewind), (0, Step::Connect)],
            )
            .unwrap();
        recovering(&world, 0);
        world
            .run(1, &[(0, fault), (0, Step::Connect), (0, Step::Connect)])
            .unwrap();
        assert!(world.seen.contains_key(during), "{fault:?}");
        recovering(&world, 0);
        world.phase(2, &[]).unwrap();
        assert!(world.devices[0].disk.holds(1), "{fault:?}");
    }
}

#[test]
fn commit_refused_after_store_rollback() {
    let act = |kind, name, text| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name,
            text,
        })
    };
    let mut world = World::new(2);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    world
        .run(
            0,
            &[
                (0, act(1, 0, 0)),
                (0, act(1, 1, 1)),
                (0, act(0, 0, 0)),
                (0, Step::Work(0)),
                (0, act(2, 0, 0)),
                (0, EFFECT),
                (0, act(0, 0, 0)),
                (0, COMMIT),
                (0, act(0, 0, 0)),
                (0, Step::CopyStore),
                (0, Step::Work(2445374581055045480)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, act(0, 0, 0)),
                (0, Step::RollBack),
            ],
        )
        .unwrap();
    world.phase(1, &[]).unwrap();
    assert!(
        world
            .seen
            .contains_key("recovery starts after a refused commit"),
        "{:?}",
        world.seen
    );
}

fn erase_disk(world: &mut World, d: usize) {
    let inos: Vec<u128> = world.devices[d].disk.entries().keys().copied().collect();
    for ino in inos {
        world.devices[d].disk.delete(ino);
    }
}

#[test]
fn repeated_rewind_keeps_the_write_obligation() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[(0, Step::Rewind), (0, Step::Backup), (0, Step::Rewind)],
        )
        .unwrap();
    erase_disk(&mut world, 0);
    erase_disk(&mut world, 1);
    assert!(matches!(world.phase(2, &[]), Err(Failure::Broken(_))));
}

#[test]
fn accepted_write_not_yet_in_the_remote_tree_is_required() {
    let mut world = synced(1, &[]);
    world
        .run(
            0,
            &[
                (0, Step::Backup),
                (0, file("a")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
            ],
        )
        .unwrap();
    assert_eq!(world.space().head_seq(), 1);
    assert_eq!(world.devices[0].engine.cursor.seq, 0);
    world
        .run(1, &[(0, Step::Rewind), (0, Step::Connect)])
        .unwrap();
    recovering(&world, 0);
    erase_disk(&mut world, 0);
    assert!(matches!(world.phase(1, &[]), Err(Failure::Broken(_))));
}

#[test]
fn rewound_write_only_a_quarantine_holds() {
    for swept in [false, true] {
        let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
        world
            .run(
                1,
                &[
                    (0, Step::Drop),
                    (1, user(2, 0, 0, "a")),
                    (1, SCAN),
                    (1, COMMIT),
                    (1, COMMIT),
                    (1, DELIVER),
                    (0, Step::Connect),
                    (0, DELIVER),
                    (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                    (1, delete(0)),
                    (0, Step::Rewind),
                ],
            )
            .unwrap();
        assert!(world.devices[0].disk.holds(1) && !world.devices[0].disk.shows(1));
        assert!(!world.devices[1].disk.holds(1));
        if swept {
            let record = (world.devices[0].engine.effects.in_flight().first())
                .unwrap()
                .clone();
            world.devices[0].disk = Disk::default();
            world.devices[0].disk.apply(&record, None);
            assert!(matches!(world.phase(2, &[]), Err(Failure::Broken(_))));
        } else {
            world.phase(2, &[]).unwrap();
            assert!(world.devices[0].disk.binned(1));
        }
    }
}

#[test]
fn a_store_rollback_brings_a_file_left_in_quarantine_back_as_the_same_file() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    let disk = &world.devices[1].disk;
    let ino = disk.child(&Place::Root, "a").unwrap();
    world
        .run(
            1,
            &[
                (1, Step::CopyStore),
                (0, overwrite(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
                (1, DELIVER),
                (1, Step::Effect(Outcome::Crash(Cut::Placed))),
                (1, Step::RollBack),
                (1, SCAN),
            ],
        )
        .unwrap();
    assert!(
        world
            .seen
            .contains_key("recovery starts after a store rollback")
    );
    let entry = &world.devices[1].disk.entries()[&ino];
    assert_eq!(
        (entry.place.clone(), entry.name.as_str()),
        (Place::Root, "a (recovered)")
    );
    let local = world.devices[1].engine.trees.local();
    assert!(local.iter().any(|(_, node)| node.identity.ino == ino));
    world.phase(2, &[]).unwrap();
}

#[test]
fn a_recovery_answer_survives_a_crash() {
    for crash_first in [false, true] {
        let mut world = synced(1, &[vec![(0, Step::Backup), (0, file("a"))]]);
        let discard = (0, Step::Answer(1));
        let crash = (0, Step::Crash);
        let mut schedule = vec![(0, Step::Rewind), (0, Step::Connect), (0, DELIVER)];
        schedule.extend(match crash_first {
            false => [discard, crash],
            true => [crash, discard],
        });
        world.run(1, &schedule).unwrap();
        world.phase(2, &[]).unwrap();
        assert!(
            world.devices[0].disk.binned(1),
            "crash first: {crash_first}"
        );
        assert_eq!(
            live(&world),
            Vec::<String>::new(),
            "crash first: {crash_first}"
        );
    }
}

#[test]
fn a_keep_given_while_the_device_is_down_counts_after_the_restart() {
    let mut world = synced(1, &[vec![(0, Step::Backup), (0, file("a"))]]);
    let schedule = [
        (0, Step::Rewind),
        (0, Step::Connect),
        (0, DELIVER),
        (0, Step::Crash),
        (0, Step::Answer(0)),
    ];
    world.run(1, &schedule).unwrap();
    world.phase(2, &[]).unwrap();
    assert_eq!(live(&world), ["a"]);
}

#[test]
fn a_keep_the_engine_asks_again_after_a_refused_commit_no_longer_counts() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[
                (0, Step::Rewind),
                (0, Step::Connect),
                (0, DELIVER),
                (0, Step::Answer(0)),
                (0, COMMIT),
                (1, Step::Connect),
                (1, DELIVER),
                (1, file("b")),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (0, Step::Commit(Reply::Refused(error::Code::HeadMoved))),
                (0, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
                (0, COMMIT),
            ],
        )
        .unwrap();
    assert!(world.seen.contains_key("commit refused, planned again"));
    let commits = &mut world.devices[0].engine.commits;
    let asked = (commits.questions().iter())
        .find(|question| {
            matches!(question.kind, QuestionKind::Recovery { .. }) && question.answer.is_none()
        })
        .map(|question| question.id.clone())
        .expect("the refused answer is asked again");
    assert!(commits.decide(&asked, Answer::Keep));
    let failure = world.phase(2, &[]).unwrap_err();
    assert!(
        failure.to_string().contains("sent write 1 again"),
        "{failure}"
    );
}

#[test]
fn a_commit_refused_as_stale_recovers_on_the_same_connection() {
    let mut world = synced(
        2,
        &[vec![
            (0, file("a")),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, Step::Connect),
            (0, Step::CopyStore),
            (0, overwrite(0)),
        ]],
    );
    world.phase(1, &[(0, Step::RollBack)]).unwrap();
    assert!(recovering(&world, 0).contains("StaleDeviceState"));
    assert_eq!(live(&world), ["a"]);
}

#[test]
fn a_write_a_crash_left_in_quarantine_comes_back_after_a_store_rollback() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(
            0,
            Step::User(Action {
                kind: 1,
                target: 0,
                parent: 0,
                name: 5,
                text: 0,
            }),
        )],
        vec![
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 14426465923166535661,
                    name: 0,
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
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 3988947350516217696,
                    name: 5,
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
                    name: 3,
                    text: 0,
                }),
            ),
        ],
        vec![
            (0, Step::Rewind),
            (1, Step::Connect),
            (1, Step::Deliver(0)),
            (1, Step::CopyStore),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 4488192439968402499,
                    name: 0,
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
            (1, Step::Effect(Outcome::Crash(Cut::Placed))),
            (1, Step::RollBack),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_folder_a_store_rollback_left_under_its_pending_name_gets_its_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 6,
                    text: 0,
                }),
            ),
            (2, Step::CopyStore),
        ],
        vec![
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 5,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 14826533567582276334,
                    parent: 4492995069366977623,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Step::Work(0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (2, Step::RollBack),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 11006393723562091909,
                    name: 1,
                    text: 0,
                }),
            ),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_folder_a_rewind_took_while_its_pending_create_was_in_flight_gets_its_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
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
            (0, Step::Backup),
            (0, Step::Rewind),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 0,
                    parent: 0,
                    name: 6,
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
                    name: 5,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 6789773372046668762,
                    parent: 10405259531666089,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Step::Work(0)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Backup),
            (0, Step::Rewind),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn an_answer_whose_folder_lands_on_a_taken_name_goes_with_what_trashes_its_children_again() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
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
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Backup),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 8549237452471102982,
                    name: 4,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
        ],
        vec![
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 1933779041458299499,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 3639856141973430611,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 3394355903379861067,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
            (2, Step::Connect),
            (2, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 2590053778025793202,
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
                    kind: 3,
                    target: 15160934574865875955,
                    parent: 17331866330941033804,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Backup),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 141313649325630119,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 1968012419361195557,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 989546708587543989,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 13728960611318022,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
    ];
    let mut world = World::new(3);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_file_the_user_moves_after_its_discard_is_asked_about_again() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, Step::CopyStore),
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
        ],
        vec![(
            0,
            Step::User(Action {
                kind: 1,
                target: 0,
                parent: 0,
                name: 1,
                text: 0,
            }),
        )],
        vec![
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 7924102408557525344,
                    parent: 0,
                    name: 3,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 4,
                    text: 0,
                }),
            ),
            (0, Step::RollBack),
            (0, Step::Work(0)),
            (2, Step::Connect),
            (2, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 566362301715400456,
                    parent: 3328292687002582,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Answer(1)),
            (0, Step::Work(8819360418494945305)),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_write_of_the_bytes_a_rewind_brought_back_to_the_server_is_in_sync() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 2,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
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
                    name: 2,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 3,
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
                0,
                Step::User(Action {
                    kind: 2,
                    target: 15392405797277166738,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Backup),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 4,
                    text: 0,
                }),
            ),
        ],
        vec![
            (0, Step::Rewind),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Connect),
            (1, Step::Work(0)),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 14994159496654458832,
                    parent: 0,
                    name: 0,
                    text: 2,
                }),
            ),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_downloaded_rename_a_store_rollback_forgot_follows_the_servers_next_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
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
            (0, Step::Work(0)),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 6,
                    text: 0,
                }),
            ),
            (0, Step::Work(13944531431250489867)),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 6,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 4,
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
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 6,
                    text: 0,
                }),
            ),
            (2, Step::CopyStore),
        ],
        vec![
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 3,
                    text: 0,
                }),
            ),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 560796138302386498,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Step::RollBack),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 12730017126478213878,
                    parent: 0,
                    name: 5,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (1, Step::CopyStore),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 860131464002953881,
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
                    kind: 3,
                    target: 7038708760116472726,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::RollBack),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_node_a_first_link_question_holds_leaves_its_recovery_question() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Backup),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 2,
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
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
        vec![
            (0, Step::Rewind),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 8100569281466065364,
                    parent: 1018938365598749887,
                    name: 3,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 13623586823969923053,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Connect),
            (1, Step::Work(0)),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 8718101247369672750,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
        vec![
            (0, Step::Rewind),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 3064395616091082862,
                    parent: 4509771055137587162,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Connect),
            (
                2,
                Step::User(Action {
                    kind: 2,
                    target: 1467271627689515377,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 2,
                    target: 3002018913897446738,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Work(0)),
        ],
    ];
    let mut world = World::new(3);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_keep_counts_while_another_copy_of_the_same_write_is_still_asked() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 3,
                    text: 0,
                }),
            ),
            (0, Step::CopyStore),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (2, Step::Work(0)),
            (2, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 0,
                    parent: 0,
                    name: 4,
                    text: 0,
                }),
            ),
            (2, Step::RollBack),
        ],
        vec![
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn devices_converge_after_the_pending_name_schedule_9cfe694b() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, Step::Commit(Reply::Served)),
            (0, Step::Work(1678135804352773165)),
            (1, Step::RollBack),
            (0, Step::Commit(Reply::NoChunks)),
            (2, Step::Deliver(3793625697838736801)),
            (2, Step::Effect(Outcome::Crash(Cut::Quarantined))),
            (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
            (1, Step::RollBack),
            (2, Step::Connect),
            (
                2,
                Step::User(Action {
                    kind: 2,
                    target: 1483757812610876444,
                    parent: 15400726047956998106,
                    name: 5,
                    text: 1,
                }),
            ),
            (0, Step::Crash),
            (
                1,
                Step::User(Action {
                    kind: 4,
                    target: 8213115807201070263,
                    parent: 4870175941323612043,
                    name: 4,
                    text: 0,
                }),
            ),
            (2, Step::Work(9180925909042372718)),
            (2, Step::Work(11078133099900180635)),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 3522485130523014030,
                    parent: 2089532220651389126,
                    name: 4,
                    text: 3,
                }),
            ),
            (1, Step::Effect(Outcome::Run)),
            (1, Step::Effect(Outcome::Run)),
            (0, Step::Work(3074173634258013773)),
            (0, Step::Scan),
        ],
        vec![
            (
                1,
                Step::User(Action {
                    kind: 4,
                    target: 16191003474821590294,
                    parent: 6075498867146312717,
                    name: 2,
                    text: 0,
                }),
            ),
            (1, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 3030783299993503740,
                    parent: 3095699000700355969,
                    name: 3,
                    text: 1,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 594124139088372594,
                    parent: 1399831688281708225,
                    name: 2,
                    text: 0,
                }),
            ),
            (1, Step::Work(5787558899418688356)),
            (0, Step::Work(2996396767938045795)),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 5924906350268423086,
                    parent: 14234893979738200804,
                    name: 4,
                    text: 0,
                }),
            ),
            (0, Step::Work(6697031048010952150)),
            (
                1,
                Step::User(Action {
                    kind: 4,
                    target: 5088754516240887609,
                    parent: 18358624339635737435,
                    name: 2,
                    text: 3,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 15980817049921036341,
                    parent: 12177747712281809510,
                    name: 2,
                    text: 3,
                }),
            ),
            (0, Step::Work(10273565487794980654)),
            (2, Step::Work(1369581937657243175)),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 13244575930038582848,
                    parent: 13405575057506845602,
                    name: 6,
                    text: 2,
                }),
            ),
            (2, Step::Work(4541074513451189574)),
            (1, Step::Work(15383151192004442872)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 3258289154744778167,
                    parent: 593544252596569516,
                    name: 0,
                    text: 3,
                }),
            ),
            (2, Step::CopyStore),
            (1, Step::Work(11259356120189098193)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 17616392070710735396,
                    parent: 11459562640979543472,
                    name: 4,
                    text: 3,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 6977140299694815242,
                    parent: 2057165008244939154,
                    name: 4,
                    text: 1,
                }),
            ),
            (1, Step::RollBack),
            (0, Step::Work(3155143635992321242)),
            (0, Step::Duplicate(6942584924682014156)),
            (1, Step::Commit(Reply::Refused(error::Code::RetryLater))),
            (1, Step::Work(7284385958750319554)),
            (0, Step::CopyStore),
            (2, Step::Work(2334500041967961765)),
            (1, Step::RollBack),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 17034038593812752629,
                    parent: 14308984375954542640,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Effect(Outcome::Crash(Cut::Placed))),
            (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (2, Step::Work(3735515594356359895)),
            (0, Step::Work(10933292786603404408)),
            (
                2,
                Step::User(Action {
                    kind: 2,
                    target: 12345297952476340962,
                    parent: 3520345178713736089,
                    name: 4,
                    text: 3,
                }),
            ),
            (2, Step::RollBack),
            (1, Step::RollBack),
            (1, Step::Work(4846927134428026024)),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 8837907222356422978,
                    parent: 15033041381845604180,
                    name: 0,
                    text: 2,
                }),
            ),
            (2, Step::Commit(Reply::Refused(error::Code::RetryLater))),
            (1, Step::Backup),
            (0, Step::Effect(Outcome::Run)),
        ],
        vec![
            (2, Step::Work(7281896897794176145)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 12568376680645498212,
                    parent: 15856729731793171292,
                    name: 4,
                    text: 3,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 4,
                    target: 6773035301965967572,
                    parent: 6910145351506530089,
                    name: 6,
                    text: 3,
                }),
            ),
            (1, Step::Commit(Reply::Served)),
            (0, Step::Work(1779388013282903149)),
            (0, Step::Work(17187154587679135126)),
            (0, Step::Work(3489123898931942307)),
            (1, Step::Crash),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 17357361888280278269,
                    parent: 17417519783480700442,
                    name: 1,
                    text: 1,
                }),
            ),
            (2, Step::Effect(Outcome::Run)),
            (1, Step::Rewind),
            (1, Step::Connect),
            (2, Step::CopyStore),
            (2, Step::Work(14806035630764795105)),
            (0, Step::Work(14040364021261400092)),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn devices_converge_after_the_pending_name_schedule_76bd3ad9() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, Step::CopyStore),
            (2, Step::Duplicate(3821575279212099044)),
            (2, Step::Work(5003156587985942617)),
            (2, Step::Drop),
            (0, Step::Deliver(3512248117917166544)),
            (2, Step::Scan),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 7317596403454770210,
                    parent: 1869719586044867545,
                    name: 0,
                    text: 3,
                }),
            ),
            (0, Step::Work(13006441216638154340)),
            (0, Step::Rewind),
            (2, Step::Rewind),
            (2, Step::Work(9170462148534717227)),
            (0, Step::Drop),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 8703851146980186638,
                    parent: 16132837282921786211,
                    name: 4,
                    text: 0,
                }),
            ),
            (1, Step::Work(7837650526050027696)),
            (2, Step::Work(437896349006447999)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 4241186696259286992,
                    parent: 32947367584785211,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 14496672410804294428,
                    parent: 6266229051460241512,
                    name: 2,
                    text: 1,
                }),
            ),
            (0, Step::Work(3992193359144950037)),
            (1, Step::Work(13418786756973946143)),
            (1, Step::Work(3000323602202841085)),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 15299381087254985608,
                    parent: 11680154182007586496,
                    name: 3,
                    text: 3,
                }),
            ),
            (0, Step::CopyStore),
            (1, Step::Work(5873849719681658143)),
            (2, Step::Scan),
            (2, Step::Deliver(2666868948759130692)),
            (1, Step::Work(15144999619698914607)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 1988515490865228988,
                    parent: 17222429470031499611,
                    name: 0,
                    text: 3,
                }),
            ),
            (2, Step::CopyStore),
            (0, Step::Deliver(6335781912732004650)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::RollBack),
            (1, Step::Backup),
            (0, Step::Deliver(7704499965567121105)),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 18114088351951365733,
                    parent: 16658249797152239823,
                    name: 3,
                    text: 1,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 5806453833927334872,
                    parent: 527602182562088030,
                    name: 5,
                    text: 1,
                }),
            ),
            (1, Step::Work(3658718270207204068)),
            (2, Step::Work(15776571550548661329)),
            (0, Step::Work(13817088142985505250)),
            (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
            (0, Step::Commit(Reply::NoChunks)),
        ],
        vec![
            (
                1,
                Step::User(Action {
                    kind: 3,
                    target: 13929757687425420658,
                    parent: 15058245278125818083,
                    name: 2,
                    text: 3,
                }),
            ),
            (1, Step::Work(5435673942609222944)),
            (1, Step::CopyStore),
            (1, Step::Work(5506028832134813162)),
            (0, Step::Effect(Outcome::Crash(Cut::Done))),
        ],
        vec![
            (1, Step::Work(3661622352125242828)),
            (0, Step::Deliver(11735594479001764815)),
            (
                1,
                Step::User(Action {
                    kind: 3,
                    target: 1299033807191007704,
                    parent: 14398343491199992407,
                    name: 1,
                    text: 0,
                }),
            ),
            (1, Step::Work(18196214669711094634)),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 9345776105146544400,
                    parent: 6149247698557914064,
                    name: 0,
                    text: 1,
                }),
            ),
            (1, Step::Work(8271955957730270680)),
            (0, Step::Effect(Outcome::Run)),
            (
                1,
                Step::User(Action {
                    kind: 3,
                    target: 4905050406842262002,
                    parent: 3170806385144620269,
                    name: 6,
                    text: 2,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 14880937513454553414,
                    parent: 5660521589684608302,
                    name: 6,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 6800231823375650411,
                    parent: 18159374723944061070,
                    name: 4,
                    text: 1,
                }),
            ),
            (2, Step::Effect(Outcome::Crash(Cut::Quarantined))),
            (1, Step::Duplicate(3021621771758099771)),
            (1, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 15967108874105397800,
                    parent: 7156066116794278917,
                    name: 3,
                    text: 3,
                }),
            ),
            (0, Step::Deliver(1188015815948138954)),
            (2, Step::Work(3357482797778124719)),
            (1, Step::Effect(Outcome::Run)),
            (2, Step::Work(10677139619515158362)),
            (0, Step::Backup),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 14971262294002000521,
                    parent: 14952945298421008629,
                    name: 6,
                    text: 2,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 5691423889175525399,
                    parent: 12934007348368069199,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Effect(Outcome::Run)),
            (0, Step::Connect),
            (2, Step::Work(8767120444197238126)),
            (0, Step::Drop),
            (2, Step::Commit(Reply::Served)),
            (1, Step::Work(13365885611161726514)),
            (2, Step::Work(14299612028849699329)),
            (1, Step::Commit(Reply::Served)),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 7766868771556160805,
                    parent: 16941876658195807845,
                    name: 6,
                    text: 3,
                }),
            ),
            (0, Step::CopyStore),
            (1, Step::Work(4086877570563636264)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 16320565745506182656,
                    parent: 12920660807550467742,
                    name: 1,
                    text: 2,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 16925203979765563066,
                    parent: 10094849599837666645,
                    name: 2,
                    text: 3,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 11248540581585733917,
                    parent: 7047684936048317542,
                    name: 0,
                    text: 1,
                }),
            ),
            (0, Step::Work(12112787718372913864)),
            (1, Step::Connect),
            (1, Step::Backup),
            (2, Step::Deliver(13343292961633367574)),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 10387780970731656787,
                    parent: 4100502543459521635,
                    name: 5,
                    text: 0,
                }),
            ),
            (1, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 14936675569161083977,
                    parent: 5550373563864812672,
                    name: 2,
                    text: 2,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 9467090453946807277,
                    parent: 4819511808381020132,
                    name: 2,
                    text: 0,
                }),
            ),
            (1, Step::RollBack),
            (0, Step::Scan),
            (1, Step::Rewind),
            (0, Step::Work(16351518424873810172)),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn devices_converge_after_the_minimized_seed_1904_schedule() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::Work(0)),
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (1, Step::CopyStore),
            (1, Step::Commit(Reply::Served)),
            (1, Step::Commit(Reply::Served)),
            (0, Step::Backup),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 3160298876391600409,
                    name: 2,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
        ],
        vec![
            (0, Step::Rewind),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 13125847551652424949,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 879727453547500741,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
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
                    parent: 237068713482240919,
                    name: 3,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 1957113664089590360,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
        vec![
            (2, Step::Rewind),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 2334358369158807981,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 10697909251576999854,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 331555691748582456,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 4,
                    target: 15036791391022041878,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
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
            (1, Step::RollBack),
        ],
    ];
    let mut world = World::new(3);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn devices_converge_after_the_minimized_seed_319_schedule() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(0, Step::Backup)],
        vec![
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 5,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 2311405476325602923,
                    name: 2,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 1503351204126987877,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
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
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 5384964184581216212,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 19857839010291241,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 4827291036041025440,
                    name: 6,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
        ],
    ];
    let mut world = World::new(3);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_discarded_folder_hands_its_children_over_when_another_device_publishes_its_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, Step::Backup),
            (1, Step::CopyStore),
            (0, Step::Rewind),
            (
                0,
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
                    name: 1,
                    text: 0,
                }),
            ),
            (1, Step::Connect),
            (1, Step::Work(0)),
            (
                1,
                Step::User(Action {
                    kind: 0,
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
                    kind: 4,
                    target: 8659313011094240714,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (1, Step::RollBack),
            (0, Step::Work(0)),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Backup),
            (0, Step::Work(7457509508091863224)),
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 11962654929561584969,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Step::Commit(Reply::Served)),
        ],
        vec![
            (
                0,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 11392994737266077749,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Rewind),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_store_rollback_asks_its_questions_as_a_store_rollback() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (1, overwrite(0))])
        .unwrap();
    world.run(1, &commit(1)).unwrap();
    world
        .run(
            1,
            &[(0, DELIVER), (0, EFFECT), (0, EFFECT), (0, overwrite(0))],
        )
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[
                (1, Step::RollBack),
                (1, Step::Connect),
                (1, Step::Connect),
                (1, DELIVER),
                (1, DELIVER),
            ],
        )
        .unwrap();
    recovering(&world, 1);
    let causes: Vec<RecoveryTrigger> = (world.devices[1].engine.commits.questions().iter())
        .filter_map(|question| question.cause())
        .map(|cause| cause.trigger)
        .collect();
    assert_eq!(causes, [RecoveryTrigger::StoreRolledBack]);
}

fn waiting_after_a_rewind(devices: usize) -> World {
    let mut world = World::new(devices);
    world
        .phase(0, &[(0, Step::Backup), (0, file("a"))])
        .unwrap();
    world.run(1, &[(0, Step::Rewind)]).unwrap();
    for d in 0..devices {
        world
            .run(1, &[(d, Step::Connect), (d, Step::Deliver(0))])
            .unwrap();
    }
    world
}

fn rekey_the_create_in_flight(world: &mut World, d: usize, another_file: bool) -> String {
    let device = &mut world.devices[d];
    let mut state = device.engine.commits.state();
    let Some(op::Op::CreateFile(create)) = &mut state.in_flight.as_mut().unwrap().commit.ops[0].op
    else {
        panic!("{:?}", state.in_flight);
    };
    let old = create.node_id.clone();
    let mut local = device.engine.trees.local().get(&old).unwrap().clone();
    if another_file {
        let ino = (device.disk)
            .create(Place::Root, "b", local.shape.content.clone())
            .unwrap();
        local.identity.ino = ino;
        local.shape.name = "b".into();
        create.name = "b".into();
    }
    let new = device.engine.ids.uuid_v7(1_794_268_740_001);
    create.node_id = new.clone();
    device.engine.trees.remove_local(&old);
    device.engine.trees.set_local(new.clone(), local);
    device.engine.commits = Commits::from_state(state).unwrap();
    device.engine.trees.take_changes();
    device.store = device.engine.reload().unwrap();
    new
}

#[test]
fn a_keep_follows_its_file_when_the_node_is_rekeyed() {
    let mut world = waiting_after_a_rewind(1);
    world.run(1, &[(0, Step::Answer(0)), (0, COMMIT)]).unwrap();
    let new = rekey_the_create_in_flight(&mut world, 0, false);
    world.run(2, &[(0, COMMIT)]).unwrap();
    let node = world.space().node(&new).unwrap();
    assert_eq!(node.current.unwrap().content.unwrap().mtime_ns, 1);
}

#[test]
fn a_keep_does_not_count_for_another_file_with_the_same_write() {
    let mut world = waiting_after_a_rewind(1);
    world.run(1, &[(0, Step::Answer(0)), (0, COMMIT)]).unwrap();
    rekey_the_create_in_flight(&mut world, 0, true);
    let failure = world.run(2, &[(0, COMMIT)]).unwrap_err();
    assert!(
        failure.to_string().contains("sent write 1 again"),
        "{failure}"
    );
}

#[test]
fn a_keep_stops_counting_once_its_rekeyed_file_is_asked_again() {
    let mut world = waiting_after_a_rewind(1);
    world.run(1, &[(0, Step::Answer(0)), (0, COMMIT)]).unwrap();
    let new = rekey_the_create_in_flight(&mut world, 0, false);
    world
        .run(
            2,
            &[
                (0, Step::Rewind),
                (0, Step::Connect),
                (0, Step::Deliver(0)),
                (0, COMMIT),
            ],
        )
        .unwrap();
    let commits = &mut world.devices[0].engine.commits;
    let question = (commits.questions().iter())
        .find(|question| question.answer.is_none() && question.shown.contains_key(&new))
        .unwrap()
        .clone();
    assert!(commits.decide(&question.id, question.kind.answers()[0]));
    let failure = world.run(3, &[(0, COMMIT), (0, COMMIT)]).unwrap_err();
    assert!(
        failure.to_string().contains("sent write 1 again"),
        "{failure}"
    );
}

#[test]
fn a_store_older_than_the_disk_asks_its_questions_with_that_cause() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (0, rename(0, "b"))])
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[
                (1, DELIVER),
                (1, EFFECT),
                (1, EFFECT),
                (1, overwrite(0)),
                (1, Step::RollBack),
                (1, Step::Connect),
                (1, Step::Connect),
                (1, DELIVER),
                (1, DELIVER),
            ],
        )
        .unwrap();
    assert!(recovering(&world, 1).contains("root marker"));
    let causes: Vec<RecoveryTrigger> = (world.devices[1].engine.commits.questions().iter())
        .filter_map(|question| question.cause())
        .map(|cause| cause.trigger)
        .collect();
    assert_eq!(causes, [RecoveryTrigger::StoreOlderThanDisk]);
}

fn act(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

#[test]
fn a_store_rollback_follows_another_devices_delete_of_a_folder_it_had_received() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (1, act(0, 0, 0, 0)),
            (1, Step::Work(0)),
            (1, Step::CopyStore),
            (0, act(0, 0, 0, 2)),
        ],
        vec![
            (2, act(0, 0, 741887062922577134, 0)),
            (1, act(4, 452847626577586837, 0, 0)),
        ],
        vec![
            (0, act(4, 2885810932734483432, 0, 0)),
            (2, Step::Work(0)),
            (0, Step::Work(0)),
            (1, Step::RollBack),
            (0, Step::Work(0)),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_publish_answer_brings_back_a_file_another_device_deleted() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (0, act(0, 0, 0, 1)),
            (1, act(1, 0, 0, 0)),
            (1, Step::Work(0)),
            (0, act(0, 0, 0, 2)),
            (1, Step::CopyStore),
            (0, act(1, 0, 0, 0)),
        ],
        vec![
            (0, act(2, 8771547057052906495, 0, 0)),
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (2, act(0, 0, 159158691643262560, 0)),
            (1, Step::RollBack),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Work(11940768220528935720)),
            (2, act(4, 6520436976943434048, 0, 0)),
        ],
    ];
    let mut world = World::new(2);
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
}

#[test]
fn a_keep_does_not_count_for_bringing_back_a_later_deletion() {
    let mut world = World::new(2);
    world
        .phase(0, &[(0, Step::Backup), (0, act(0, 0, 0, 0))])
        .unwrap();
    world
        .run(
            1,
            &[
                (0, Step::Rewind),
                (1, Step::Connect),
                (1, DELIVER),
                (1, Step::Answer(0)),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
            ],
        )
        .unwrap();
    let (id, local) = (world.devices[1].engine.trees.local().iter())
        .next()
        .map(|(id, local)| (id.clone(), local.clone()))
        .unwrap();
    assert_eq!(
        world.space().node(&id).unwrap().status,
        NodeStatus::Live as i32
    );
    world
        .run(
            2,
            &[
                (0, Step::Connect),
                (0, DELIVER),
                (0, DELIVER),
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
            ],
        )
        .unwrap();
    assert_eq!(
        world.space().node(&id).unwrap().status,
        NodeStatus::Trashed as i32
    );
    let engine = &mut world.devices[1].engine;
    let mut state = engine.commits.state();
    assert!(state.in_flight.is_none());
    let moved = Move {
        node_id: id.clone(),
        new_parent_id: local.shape.parent,
        new_name: local.shape.name,
        from: None,
    };
    state.in_flight = Some(InFlight {
        commit: Commit {
            commit_id: engine.ids.uuid_v7(1_794_268_740_010),
            history_id: engine.history.clone(),
            device_seq: state.accepted + 1,
            base_seq: engine.cursor.seq,
            ops: vec![Op {
                op: Some(op::Op::Move(moved)),
            }],
            ..Default::default()
        },
        deletes: Default::default(),
        restores: Default::default(),
        resent: Default::default(),
        result: None,
    });
    engine.commits = Commits::from_state(state).unwrap();
    let failure = (world.run(3, &[(1, COMMIT)])).expect_err("nobody asked for the folder again");
    assert!(
        failure.to_string().contains("brought back node"),
        "{failure}"
    );
}

#[test]
fn a_publish_for_a_folder_does_not_count_for_its_unchanged_child() {
    let mut world = World::new(2);
    world
        .phase(0, &[(0, act(0, 0, 0, 0)), (0, act(1, 0, 1, 2))])
        .unwrap();
    let local = world.devices[1].engine.trees.local();
    let named = |name: &str| {
        (local.iter())
            .find(|(_, node)| node.shape.name == name)
            .map(|(id, _)| id.clone())
            .unwrap()
    };
    let (folder, child) = (named("a"), named("b"));
    world
        .run(
            1,
            &[
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
            ],
        )
        .unwrap();
    let engine = &mut world.devices[1].engine;
    let mut renamed = engine.trees.local().get(&folder).unwrap().clone();
    renamed.shape.name = "c.txt".into();
    engine.trees.set_local(folder.clone(), renamed);
    let seq = engine.commits.accepted();
    let cause = RecoveryCause::of(RecoveryTrigger::StoreRolledBack);
    (engine.commits).recover(
        &mut engine.trees,
        seq,
        &cause,
        &mut engine.ids,
        1_794_268_740_020,
    );
    let held: Vec<&String> = (engine.commits.questions().iter())
        .flat_map(|question| question.holds())
        .collect();
    assert_eq!(held, [&folder]);
    world.run(2, &[(1, Step::Answer(0)), (1, COMMIT)]).unwrap();
    let engine = &mut world.devices[1].engine;
    let mut state = engine.commits.state();
    let flight = state.in_flight.as_mut().unwrap();
    flight.deletes = Default::default();
    (flight.commit.ops)
        .retain(|op| !matches!(&op.op, Some(op::Op::Delete(delete)) if delete.node_id == child));
    engine.commits = Commits::from_state(state).unwrap();
    let failure = world
        .run(3, &[(1, COMMIT)])
        .expect_err("nobody asked for the child");
    assert!(
        failure
            .to_string()
            .contains(&format!("brought back node {child}")),
        "{failure}"
    );
}

#[test]
fn a_write_a_rewind_took_and_a_restore_brings_back_is_no_resend() {
    let mut world = World::new(2);
    world
        .phase(0, &[(0, Step::Backup), (0, file("a"))])
        .unwrap();
    world
        .phase(
            1,
            &[
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
                (0, Step::Rewind),
                (0, Step::RestoreEarlier),
            ],
        )
        .unwrap();
}

#[test]
fn a_file_a_device_deletes_after_another_device_keeps_it_through_a_rewind_stays_deleted() {
    let user = |kind, target, name| {
        Step::User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![(1, user(1, 0, 0)), (0, Step::Backup)],
        vec![
            (2, user(0, 0, 1)),
            (0, Step::Rewind),
            (2, Step::Connect),
            (2, Step::Work(1540128748179039060)),
        ],
        vec![
            (0, user(0, 0, 2)),
            (2, user(4, 6878815396834242546, 0)),
            (2, Step::Work(0)),
            (2, Step::Work(3265949467000100339)),
            (2, Step::Work(2407435917824306126)),
        ],
    ];
    synced(3, &phases);
}
