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
