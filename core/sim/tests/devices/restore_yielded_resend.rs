// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn an_observer_that_gave_way_to_a_delete_does_not_create_the_write_again_when_it_lists_the_server_again()
 {
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
                (0, Step::Connect),
                (0, Step::Work(17078780445911177944)),
                (
                    0,
                    Step::User(Action {
                        kind: 4,
                        target: 17785016745073826483,
                        parent: 6860128358995468275,
                        name: 5,
                        text: 0,
                    }),
                ),
                (0, Step::Commit(Reply::Served)),
            ],
        ],
    )
    .unwrap();
}
