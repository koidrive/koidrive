// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_new_file_with_a_lost_files_bytes_is_sent_after_a_store_rollback() {
    keep(
        2,
        &[vec![
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 7,
                    text: 3,
                }),
            ),
            (0, Step::CopyStore),
            (2, Step::Work(0)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Deliver(0)),
            (2, Step::Work(0)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 7,
                    text: 3,
                }),
            ),
            (2, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 5995543246996412090,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Work(0)),
            (0, Step::Backup),
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
            (0, Step::Work(9438907486793066888)),
            (0, Step::Work(19443614543567048)),
            (0, Step::Restore),
            (2, Step::RollBack),
        ]],
    )
    .unwrap();
}

#[test]
fn a_restart_after_a_crash_keeps_two_placed_downloads_apart_when_the_user_swaps_their_names() {
    let u = |kind, target, name| {
        Step::User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: if kind == 1 { 1 } else { 0 },
        })
    };
    let world = converge(
        2,
        &[vec![
            (0, u(1, 0, 0)),
            (0, u(1, 0, 4)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Work(0)),
            (1, Step::Deliver(0)),
            (1, Step::Effect(Outcome::Crash(Cut::Placed))),
            (1, u(3, 29687920466624875, 1)),
            (1, u(3, 953053783945106222, 4)),
        ]],
    )
    .unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["A", "C.TXT"], "device {device}");
    }
}
