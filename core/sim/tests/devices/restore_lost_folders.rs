// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn an_own_lost_create_below_a_resent_folder_is_sent_again_after_a_delete_above_it() {
    converge(
        2,
        &[
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
                (0, Step::Work(0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (
                    0,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 7859059150060165253,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![(
                1,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 2850711948788266403,
                    name: 0,
                    text: 0,
                }),
            )],
            vec![
                (0, Step::Restore),
                (
                    0,
                    Step::User(Action {
                        kind: 4,
                        target: 3124382587728251214,
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
fn a_rename_beside_a_kept_file_in_a_lost_folder_makes_no_recovery_copy() {
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
                (1, Step::Scan),
                (1, Step::Work(0)),
                (1, Step::Commit(Reply::Served)),
                (0, Step::Deliver(0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 8,
                        text: 0,
                    }),
                ),
                (2, Step::Backup),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 0,
                        parent: 13451055082625294843,
                        name: 0,
                        text: 0,
                    }),
                ),
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
                    1,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (1, Step::Scan),
                (1, Step::Work(0)),
                (
                    1,
                    Step::User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 5,
                        text: 0,
                    }),
                ),
                (1, Step::Restore),
            ],
        ],
    )
    .unwrap();
}
