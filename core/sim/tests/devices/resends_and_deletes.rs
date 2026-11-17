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

fn restored_earlier_after_a_resend() -> Vec<Vec<(usize, Step)>> {
    use Step::*;
    let user = |kind: u8, text: u8| {
        User(Action {
            kind,
            target: 0,
            parent: 0,
            name: 0,
            text,
        })
    };
    vec![
        vec![(0, user(1, 0))],
        vec![(1, user(2, 0)), (0, Backup)],
        vec![(0, user(2, 1)), (0, Backup), (0, user(0, 0))],
        vec![
            (0, Leave),
            (0, Restore),
            (1, Connect),
            (1, Work(0)),
            (1, Work(6067052043035634869)),
            (1, Commit(Reply::Served)),
            (1, Work(12716880773219500)),
            (0, RestoreEarlier),
        ],
    ]
}

#[test]
fn a_bystander_resends_another_devices_lost_write_again_after_a_restore_to_an_earlier_copy_takes_its_resend_and_its_own_edit_below()
 {
    let world = converge(2, &restored_earlier_after_a_resend()).unwrap();
    assert_eq!(paths_on(&world, 1), ["a"]);
    assert!(world.devices[1].disk.holds(3));
    assert!(world.devices[1].engine.commits.questions().is_empty());
}

#[test]
fn a_bystander_with_no_record_of_the_version_it_holds_keeps_another_devices_lost_write_as_a_conflict_copy_after_a_restore_to_an_earlier_copy()
 {
    let phases = restored_earlier_after_a_resend();
    let (last, earlier) = phases.split_last().unwrap();
    let mut world = world(2);
    for (phase, schedule) in earlier.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
    let (restore, resent) = last.split_last().unwrap();
    world.run(earlier.len(), resent).unwrap();
    let device = &mut world.devices[1];
    let mut state = device.engine.trees.state();
    state.content_provenance.clear();
    device.engine.trees.restore(state).unwrap();
    device.engine.trees.rules_scanned();
    device.store.trees = device.engine.trees.clone();
    world
        .phase(earlier.len(), std::slice::from_ref(restore))
        .unwrap();
    let paths = paths_on(&world, 1);
    assert_eq!(paths[0], "a");
    assert!(
        paths[1].starts_with("a (conflict from device-1"),
        "{paths:?}"
    );
    assert_eq!(paths.len(), 2);
    assert!(world.devices[1].disk.holds(3));
}

#[test]
fn a_copy_a_bystander_keeps_of_another_devices_lost_write_yields_to_a_delete_of_its_folder() {
    use Step::*;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                0,
                User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (
                0,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                }),
            ),
        ],
        vec![
            (
                0,
                User(Action {
                    kind: 2,
                    target: 4331116183656926,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Backup),
            (
                0,
                User(Action {
                    kind: 3,
                    target: 2975849262981074948,
                    parent: 6544567074253531843,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
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
            (0, Restore),
            (
                2,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 4941178086974508048,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Connect),
            (2, Work(2743188700098888673)),
            (
                0,
                User(Action {
                    kind: 4,
                    target: 753389883202084897,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Work(0)),
            (2, Work(3317640963076976485)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        assert_eq!(paths_on(&world, device), ["a"], "device {device}");
    }
}
