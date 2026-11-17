// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_resent_create_under_a_folder_a_user_deleted_after_a_restore_keeps_the_delete() {
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
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 6023520618854034607,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Backup),
            ],
            vec![
                (
                    1,
                    Step::User(Action {
                        kind: 0,
                        target: 0,
                        parent: 250095372574605890,
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
                        name: 5,
                        text: 0,
                    }),
                ),
                (0, Step::Work(0)),
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
                (0, Step::Work(13455858)),
                (0, Step::Commit(Reply::Served)),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 2817478716356051602,
                        parent: 5622325779178779261,
                        name: 4,
                        text: 0,
                    }),
                ),
                (1, Step::Scan),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 13058700028631697383,
                        parent: 13527487848144952449,
                        name: 3,
                        text: 3,
                    }),
                ),
                (1, Step::Work(4676884674109876101)),
                (
                    2,
                    Step::User(Action {
                        kind: 4,
                        target: 10920075462221998926,
                        parent: 343589842454217441,
                        name: 6,
                        text: 3,
                    }),
                ),
                (0, Step::Restore),
            ],
        ],
    )
    .unwrap();
}
