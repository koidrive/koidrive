// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_device_sends_its_own_nodes_again_after_a_restore_takes_them() {
    converge(
        2,
        &[vec![
            (0, Step::Backup),
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
                    parent: 5505792871386048596,
                    name: 8,
                    text: 0,
                }),
            ),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 1903993966365310358,
                    parent: 136821266194568094,
                    name: 7,
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
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 3331734342487532775,
                    parent: 1383182514749597970,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Step::Work(3300016626867719300)),
            (2, Step::Work(9019637829209103660)),
            (0, Step::Restore),
        ]],
    )
    .unwrap();
}
