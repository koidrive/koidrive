// SPDX-License-Identifier: Apache-2.0

use super::*;

fn holders(world: &World) -> Vec<Vec<String>> {
    (0..world.devices.len())
        .map(|d| paths_on(world, d))
        .collect()
}

fn every_device_holds_a_file(world: &World) -> bool {
    (world.devices.iter()).all(|device| !device.disk.entries().is_empty())
}

#[test]
#[ignore = "a resent rename the deleting device never saw undoes its delete (GUN-580)"]
fn a_delete_beats_a_resent_rename_its_device_never_saw() {
    let mut world = converge(
        3,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (0, rename(0, 0, "b"))],
        ],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE)];
    for device in [1, 2] {
        steps.push((device, Step::Connect));
        steps.extend([(device, DELIVER); 8]);
    }
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.push((2, COMMIT));
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
#[ignore = "a delete planned against a resend comes undone when the server loses the resend again (GUN-589)"]
fn a_delete_planned_against_a_resend_the_server_then_loses_stays() {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, BACKUP));
    steps.extend([(0, delete(0)), (0, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, RESTORE));
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    steps.push((1, Step::Connect));
    for _ in 0..2 {
        for d in [0, 1] {
            steps.extend([(d, DELIVER); 8]);
            steps.extend(commit(d));
        }
    }
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
#[ignore = "a delete made after a listing showed another device's resend comes undone (GUN-581)"]
fn a_delete_stays_against_a_resend_its_device_learned_from_a_listing() {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]],
    )
    .unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, Step::Drop), (0, delete(0)), (0, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(1, DELIVER); 8]);
    world.run(2, &steps).unwrap();
    world.truncate_log();
    let mut steps = vec![(0, Step::Connect)];
    steps.extend([(0, DELIVER); 12]);
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    world.phase(3, &steps).unwrap();
    assert!(world.seen.contains_key("listing replaces R"));
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
#[ignore = "a folder deleted after a second server loss comes back from an observer's resend (GUN-692)"]
fn a_delete_stays_across_a_second_loss_of_an_observers_resend() {
    let world = keep(
        2,
        &[
            vec![(0, folder("a"))],
            vec![
                (0, BACKUP),
                (0, user(0, 0, 1, "b")),
                (0, user(1, 0, 2, "c.txt")),
            ],
            {
                let mut steps = vec![(0, RESTORE), (0, BACKUP), (1, Step::Connect)];
                steps.extend([(1, Step::Work(0)); 12]);
                steps
            },
            {
                let mut steps = vec![(0, RESTORE), (0, delete(0)), (0, SCAN), (0, Step::Connect)];
                steps.extend([(0, Step::Work(0)); 12]);
                steps.push((1, Step::Connect));
                steps.extend([(1, DELIVER); 8]);
                steps
            },
        ],
    )
    .unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
fn a_new_folder_does_not_bring_back_a_deleted_folder_without_a_restore() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    converge(
        2,
        &[
            vec![
                (1, u(0, 0, 0, 5, 0)),
                (0, Step::Work(0)),
                (0, u(1, 0, 0, 0, 0)),
            ],
            vec![
                (1, u(1, 0, 312516678923509493, 0, 0)),
                (0, u(0, 0, 0, 0, 0)),
                (0, u(0, 0, 0, 0, 0)),
                (0, u(0, 0, 0, 0, 0)),
                (2, u(2, 0, 0, 0, 0)),
                (0, Step::Work(0)),
                (0, u(2, 0, 0, 0, 0)),
                (0, u(2, 0, 0, 0, 0)),
                (0, u(2, 0, 0, 0, 0)),
            ],
            vec![
                (1, u(0, 0, 10871575225237674698, 6, 0)),
                (2, u(4, 3068338248260445892, 0, 0, 0)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (1, Step::Commit(Reply::Served)),
                (1, u(0, 0, 8899275754376522441, 0, 0)),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "an unsent delete comes undone after a restore to an earlier backup and back (GUN-869)"]
fn an_unsent_delete_stays_across_a_restore_to_an_earlier_backup_and_back() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(1, file("a"))]]).unwrap();
    let mut steps = vec![
        (0, BACKUP),
        (0, delete(0)),
        (0, SCAN),
        (0, Step::RestoreEarlier),
        (0, Step::Connect),
    ];
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, Step::Work(0)); 4]);
    steps.push((0, RESTORE));
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
fn an_unscanned_delete_stays_after_a_restore() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(1, file("a"))]]).unwrap();
    let mut steps = vec![(0, delete(0)), (0, RESTORE), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
#[ignore = "a resent create at the place a lost rename moved the file to undoes a later delete (GUN-862)"]
fn a_delete_is_not_undone_by_a_resent_create_at_the_moved_place() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "A")), (0, SCAN), (0, COMMIT), (0, COMMIT)];
    steps.extend([(1, DELIVER); 8]);
    steps.extend([(1, Step::Work(0)); 6]);
    steps.extend([(0, delete(0)), (0, SCAN), (0, RESTORE)]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
#[ignore = "an edit reverted to the bytes another device deleted brings the deleted file back (GUN-541)"]
fn a_reverted_edit_of_a_file_another_device_deleted_leaves_it_deleted() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, act(2, 0, 0, "a", 1)), (0, SCAN)];
    steps.extend([(1, delete(0))]);
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, act(2, 0, 0, "a", 0)), (0, SCAN)]);
    steps.extend(commit(0));
    world.phase(1, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "{:?}", holders(&world));
}

fn a_delete_made_before_an_edit_and_its_revert(scan_first: bool) {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(1, delete(0))];
    if scan_first {
        steps.push((1, SCAN));
    }
    steps.push((0, act(2, 0, 0, "a", 1)));
    steps.extend(commit(0));
    steps.push((0, act(2, 0, 0, "a", 0)));
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    world.phase(1, &steps).unwrap();
    assert!(every_device_holds_a_file(&world), "{:?}", holders(&world));
}

#[test]
fn a_scanned_delete_keeps_an_edit_and_its_revert_it_never_saw() {
    a_delete_made_before_an_edit_and_its_revert(true);
}

#[test]
fn an_unscanned_delete_keeps_an_edit_and_its_revert_it_never_saw() {
    a_delete_made_before_an_edit_and_its_revert(false);
}
