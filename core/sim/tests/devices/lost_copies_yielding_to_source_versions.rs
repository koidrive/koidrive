// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
#[ignore = "a lost copy is not created again while the server holds its source version with the same write (one file per write)"]
fn a_lost_copy_yields_to_its_source_version_after_store_rollbacks_and_a_rewind() {
    converge(
        2,
        &[
            vec![
                (0, Step::Backup),
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
                (2, Step::Work(0)),
                (2, Step::CopyStore),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (0, Step::Backup),
                (0, Step::RollBack),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 1945515385904847284,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Step::RestoreEarlier),
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
                (2, Step::RollBack),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 4,
                        text: 0,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 9100156339362910142,
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
#[ignore = "a lost copy is not created again while the server holds its source version with the same write (one file per write)"]
fn a_lost_copy_is_not_sent_again_once_the_server_holds_its_source_version_with_the_same_write() {
    converge(
        3,
        &[
            vec![
                (0, Step::Backup),
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
            ],
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
                (0, Step::RestoreEarlier),
                (
                    1,
                    Step::User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
            ],
            vec![(0, Step::Rewind)],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a lost copy is not created again while the server holds its source version with the same write, and the rolled back edit stays on the server (WriteKept, one file per write)"]
fn a_lost_copy_yields_to_its_source_version_after_a_rolled_back_edit_and_an_earlier_restore() {
    converge(
        3,
        &[
            vec![
                (2, Step::CopyStore),
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
                (0, Step::CopyStore),
                (0, Step::Backup),
            ],
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
                (0, Step::RollBack),
                (0, Step::RestoreEarlier),
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
                (2, Step::RollBack),
            ],
            vec![(0, Step::Backup), (0, Step::RestoreEarlier)],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a lost copy is not created again while the server holds its source version with the same write (one file per write)"]
fn a_lost_copy_yields_to_its_source_version_after_a_rolled_back_edit_and_a_second_restore() {
    converge(
        2,
        &[
            vec![
                (0, Step::Work(0)),
                (2, Step::CopyStore),
                (0, Step::Backup),
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
            ],
            vec![
                (0, Step::Backup),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 83057763675715484,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Step::RestoreEarlier),
                (2, Step::RollBack),
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
            ],
            vec![(0, Step::Restore)],
        ],
    )
    .unwrap();
}
