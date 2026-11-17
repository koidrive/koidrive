// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn an_edit_resent_as_a_conflict_copy_stays_in_one_file_after_a_rewind() {
    converge(
        3,
        &[
            vec![(
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            )],
            vec![
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
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
                (0, Step::RestoreEarlier),
            ],
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
                (0, Step::Rewind),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_resent_as_a_conflict_copy_stays_in_one_file_after_an_earlier_restore() {
    converge(
        3,
        &[
            vec![(
                0,
                Step::User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            )],
            vec![
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
                (0, Step::Backup),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
            ],
            vec![(0, Step::Backup), (0, Step::RestoreEarlier)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_resent_as_a_conflict_copy_stays_in_one_file_when_a_restore_brings_back_its_version() {
    converge(
        3,
        &[
            vec![
                (1, Step::Commit(Reply::Served)),
                (0, Step::Work(10857026126750760327)),
                (0, Step::Answer(1)),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 3311029978998549997,
                        parent: 4889060932184237793,
                        name: 3,
                        text: 3,
                    }),
                ),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 3350099667364564034,
                        parent: 13939761295719185397,
                        name: 2,
                        text: 0,
                    }),
                ),
                (1, Step::Effect(Outcome::Run)),
                (2, Step::Work(16808147437909232298)),
            ],
            vec![
                (
                    1,
                    Step::User(Action {
                        kind: 2,
                        target: 9834652260756273480,
                        parent: 14510926504866488881,
                        name: 9,
                        text: 3,
                    }),
                ),
                (0, Step::Work(1700660306527558795)),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 15064871403399930363,
                        parent: 7690534438783096802,
                        name: 6,
                        text: 3,
                    }),
                ),
                (0, Step::Deliver(300745894145524020)),
                (2, Step::Scan),
                (1, Step::Work(13330979368830247814)),
                (2, Step::Work(15490457543093987966)),
                (0, Step::Scan),
                (1, Step::Commit(Reply::Served)),
                (1, Step::Scan),
                (0, Step::Backup),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 16592406176418282867,
                        parent: 7911502493192348679,
                        name: 4,
                        text: 2,
                    }),
                ),
                (1, Step::Scan),
                (1, Step::Commit(Reply::Served)),
                (1, Step::RollBack),
                (1, Step::Scan),
                (2, Step::Work(14555881598617945590)),
                (2, Step::CopyStore),
                (2, Step::Work(7260556471425574467)),
                (2, Step::Work(9605223603004543914)),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 17632189588001984505,
                        parent: 6043058376376125525,
                        name: 9,
                        text: 1,
                    }),
                ),
                (1, Step::Deliver(7578445822582569923)),
                (2, Step::Backup),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 14772259326133816940,
                        parent: 2566042525944579580,
                        name: 8,
                        text: 2,
                    }),
                ),
                (1, Step::Work(5004686681485946375)),
                (2, Step::Connect),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 15819884606009034702,
                        parent: 2154675339803965894,
                        name: 1,
                        text: 0,
                    }),
                ),
                (0, Step::RestoreEarlier),
                (0, Step::Work(2874722188071010862)),
                (1, Step::Deliver(3068071552127943424)),
                (2, Step::Connect),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 1042267458408622716,
                        parent: 9024425542503421187,
                        name: 4,
                        text: 1,
                    }),
                ),
            ],
            vec![
                (
                    1,
                    Step::User(Action {
                        kind: 0,
                        target: 13996149545372296891,
                        parent: 3754859505670715402,
                        name: 8,
                        text: 2,
                    }),
                ),
                (0, Step::Work(8355272694676286632)),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 16374962981216556516,
                        parent: 15948395495296919323,
                        name: 4,
                        text: 1,
                    }),
                ),
                (2, Step::Crash),
                (1, Step::Work(11851559492971910032)),
                (2, Step::Work(14340256999361305936)),
                (2, Step::Work(2370092440841663323)),
                (2, Step::Work(10136564159584160511)),
                (1, Step::Backup),
                (1, Step::RestoreEarlier),
                (2, Step::Effect(Outcome::Run)),
                (
                    1,
                    Step::User(Action {
                        kind: 4,
                        target: 2251243652576861595,
                        parent: 17097658996354482096,
                        name: 0,
                        text: 1,
                    }),
                ),
                (0, Step::Work(10474191025459082442)),
                (1, Step::Crash),
                (1, Step::Duplicate(17982422311326493038)),
                (0, Step::Work(8445642667722691600)),
            ],
        ],
    )
    .unwrap();
}

fn u(kind: u8, target: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent: 0,
        name,
        text,
    })
}

#[test]
fn a_file_deleted_before_a_second_restore_loses_no_write() {
    keep(
        3,
        &[
            vec![(0, u(1, 0, 0, 0))],
            vec![
                (0, u(2, 0, 0, 0)),
                (0, Step::Backup),
                (2, u(2, 0, 0, 0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
            ],
            vec![(0, Step::Backup), (0, Step::RestoreEarlier)],
            vec![(0, u(4, 7189654988924490997, 9, 0)), (0, Step::Restore)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_made_offline_across_a_rewind_loses_no_write() {
    keep(
        3,
        &[
            vec![(0, u(1, 0, 0, 0))],
            vec![
                (2, u(2, 0, 0, 0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (0, u(2, 0, 0, 0)),
                (0, Step::RestoreEarlier),
            ],
            vec![(0, Step::Rewind), (2, u(2, 17200373593075710099, 1, 0))],
            vec![(2, u(2, 10175757954459202047, 1, 2))],
        ],
    )
    .unwrap();
}
