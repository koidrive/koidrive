// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn observers_keep_one_copy_of_a_lost_write_across_another_restore() {
    let phases = [
        vec![
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
            (0, Step::Backup),
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
            (0, Step::Work(0)),
            (0, Step::Connect),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, Step::Restore),
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
            (0, Step::Backup),
        ],
        vec![
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 12718368800917382766,
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
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 0,
                    parent: 711456114797108233,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Step::Work(0)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Work(0)),
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
                2,
                Step::User(Action {
                    kind: 3,
                    target: 8462061729394261601,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Restore),
        ],
    ];
    let mut world = world(3);
    world.phase(0, &phases[0]).unwrap();
    let source = version_of_write(&world, 1).version_id;
    world.phase(1, &phases[1]).unwrap();
    assert!(version_of_write(&world, 2).made_over.contains(&source));
    let mut writes: Vec<_> = world
        .space()
        .nodes()
        .into_iter()
        .filter(|node| node.status == koidrive_proto::v1::NodeStatus::Live as i32)
        .filter_map(|node| node.current?.content.map(|content| content.mtime_ns))
        .collect();
    writes.sort_unstable();
    assert_eq!(writes, [1, 2]);
}
