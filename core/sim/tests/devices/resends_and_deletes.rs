// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_bystander_resends_another_devices_lost_write_again_as_a_resend_after_a_second_restore_takes_its_first_resend()
 {
    use Step::*;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(
            0,
            User(Action {
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
                User(Action {
                    kind: 2,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Backup),
            (
                0,
                User(Action {
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
                2,
                User(Action {
                    kind: 4,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Restore),
            (1, Connect),
            (1, Work(0)),
            (1, Work(6067052043035634869)),
            (1, Commit(Reply::Served)),
            (1, Work(12716880773219500)),
            (0, Restore),
        ],
    ];
    let world = converge(2, &phases).unwrap();
    for device in 0..2 {
        let paths = paths_on(&world, device);
        assert!(paths.is_empty(), "device {device}: {paths:?}");
    }
}
