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

#[test]
#[ignore = "a bystander's create that resent another device's file revives the deleted folder after a second restore (GUN-1006)"]
fn a_bystanders_create_that_resent_another_devices_file_stays_a_resend_after_a_second_restore() {
    use Step::*;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (
                2,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 5,
                    text: 0,
                }),
            ),
            (2, Work(0)),
            (
                1,
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
                    name: 2,
                    text: 0,
                }),
            ),
            (
                0,
                User(Action {
                    kind: 1,
                    target: 0,
                    parent: 4995830252304420462,
                    name: 1,
                    text: 0,
                }),
            ),
            (2, Commit(Reply::Served)),
            (2, Commit(Reply::Served)),
            (0, Backup),
            (
                0,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 2280524893592302938,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                1,
                User(Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 3,
                    text: 0,
                }),
            ),
        ],
        vec![
            (
                2,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 2324108908231934117,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Work(0)),
            (0, Restore),
            (2, Connect),
            (
                0,
                User(Action {
                    kind: 4,
                    target: 5630648104909550835,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Work(0)),
            (2, Work(0)),
            (0, Connect),
            (
                0,
                User(Action {
                    kind: 2,
                    target: 1088424535393301162,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Work(9509308981100096768)),
            (
                1,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 293270352897540747,
                    name: 7,
                    text: 0,
                }),
            ),
            (2, Work(0)),
            (
                0,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 11802185550846683935,
                    name: 1,
                    text: 0,
                }),
            ),
            (
                1,
                User(Action {
                    kind: 3,
                    target: 1940465844613316820,
                    parent: 13649535398099238466,
                    name: 0,
                    text: 0,
                }),
            ),
            (2, Commit(Reply::Served)),
        ],
        vec![
            (
                0,
                User(Action {
                    kind: 0,
                    target: 0,
                    parent: 1355549907959955166,
                    name: 0,
                    text: 0,
                }),
            ),
            (0, Restore),
            (
                0,
                User(Action {
                    kind: 4,
                    target: 16436166219119933091,
                    parent: 0,
                    name: 0,
                    text: 0,
                }),
            ),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(
            !paths.iter().any(|path| path.starts_with("ß")),
            "device {device}: {paths:?}"
        );
    }
}

#[test]
fn a_bystander_sends_another_devices_lost_write_again_after_a_second_restore_to_an_earlier_copy_takes_its_resend_and_its_own_edit_below()
 {
    use Step::*;
    let act = |kind: u8| {
        User(Action {
            kind,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        })
    };
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(0, act(1))],
        vec![(1, act(2)), (0, Backup)],
        vec![(0, act(2)), (0, Backup), (0, act(0))],
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
    ];
    converge(2, &phases).unwrap();
}

fn work(device: usize, n: usize) -> Vec<(usize, Step)> {
    (0..n).map(|i| (device, Step::Work(i))).collect()
}

fn a_lost_write_a_bystander_copies_into_a_folder_another_device_deletes(
    copy_before_delete: bool,
) -> World {
    let mut steps = vec![(0, Step::Backup), (0, overwrite(1))];
    steps.extend(commit(0));
    steps.extend(work(1, 12));
    steps.push((0, Step::Restore));
    steps.push((2, Step::Connect));
    steps.extend(work(2, 12));
    steps.push((2, overwrite(1)));
    steps.extend(commit(2));
    let mut bystander = vec![(1, Step::Connect)];
    bystander.extend(work(1, 30));
    let deleter = [(2, delete(0)), (2, SCAN), (2, COMMIT), (2, COMMIT)];
    match copy_before_delete {
        true => steps.extend(bystander.into_iter().chain(deleter)),
        false => steps.extend(deleter.into_iter().chain(bystander)),
    }
    steps.push((0, delete(1)));
    converge(3, &[vec![(0, folder("A")), (0, user(1, 0, 1, "b"))], steps]).unwrap()
}

fn the_bystanders_copy_lands_and_then(k: usize, then: &[(usize, Step)]) -> World {
    let mut steps = vec![(0, Step::Backup), (0, overwrite(2))];
    steps.extend(commit(0));
    steps.extend(work(1, 12));
    steps.push((0, Step::Restore));
    steps.push((2, Step::Connect));
    steps.extend(work(2, 12));
    steps.push((2, overwrite(2)));
    steps.extend(commit(2));
    steps.push((1, Step::Connect));
    steps.extend(work(1, k));
    steps.extend_from_slice(then);
    steps.extend(work(1, 30));
    steps.push((0, delete(2)));
    let made = vec![(0, folder("b")), (0, folder("A")), (0, user(1, 0, 2, "b"))];
    converge(3, &[made, steps]).unwrap()
}

fn no_device_holds_a_path_under_a(world: &World) {
    for device in 0..3 {
        let paths = paths_on(world, device);
        assert!(
            !paths.iter().any(|path| path.starts_with("A")),
            "device {device}: {paths:?}"
        );
    }
}

#[test]
#[ignore = "a bystander's copy of a lost write that lands before the delete of its folder keeps the deleted folder (KOI-1028)"]
fn review_h1_copy_lands_before_the_delete() {
    no_device_holds_a_path_under_a(
        &a_lost_write_a_bystander_copies_into_a_folder_another_device_deletes(true),
    );
}

#[test]
fn review_h1_delete_lands_before_the_copy() {
    no_device_holds_a_path_under_a(
        &a_lost_write_a_bystander_copies_into_a_folder_another_device_deletes(false),
    );
}

#[test]
#[ignore = "a deleted folder stays live and empty when the bystander's user moved its copy of a lost write out first (KOI-1028)"]
fn review_h1_copy_moved_out_lands_before_the_delete() {
    let then = [
        (1, rename(2, 0, "c.txt")),
        (1, SCAN),
        (2, delete(1)),
        (2, SCAN),
        (2, COMMIT),
        (2, COMMIT),
    ];
    no_device_holds_a_path_under_a(&the_bystanders_copy_lands_and_then(3, &then));
}

#[test]
#[ignore = "a bystander sends its copy of a lost write again as its own create after a second restore and revives the deleted folder (KOI-1028)"]
fn a_bystanders_copy_of_a_lost_write_stays_a_copy_after_a_second_restore_and_leaves_its_deleted_folder_deleted()
 {
    let mut then = vec![(0, Step::Restore), (2, Step::Connect)];
    then.extend(work(2, 12));
    then.extend([(2, delete(1)), (2, SCAN), (2, COMMIT), (2, COMMIT)]);
    then.push((1, Step::Connect));
    no_device_holds_a_path_under_a(&the_bystanders_copy_lands_and_then(5, &then));
}
