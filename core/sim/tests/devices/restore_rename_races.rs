// SPDX-License-Identifier: Apache-2.0

use super::*;

fn take(device: usize) -> Vec<(usize, Step)> {
    let mut steps = vec![(device, DELIVER); 4];
    steps.extend([(device, EFFECT); 4]);
    steps
}

fn live_names(world: &World) -> Vec<String> {
    let root = world.space().space_id();
    let mut names: Vec<String> = (world.space().nodes().into_iter())
        .filter(|node| node.node_id != root)
        .filter(|node| node.status == koidrive_proto::v1::NodeStatus::Live as i32)
        .map(|node| node.name)
        .collect();
    names.sort();
    names
}

fn every_disk_shows(world: &World, devices: &[usize], name: &str) {
    for &device in devices {
        let names: Vec<String> = (files_on(world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, [name], "device {device}");
    }
}

fn restore_and_reconnect(world: &mut World, phase: usize, order: &[usize]) {
    let mut steps = vec![(0, RESTORE)];
    for &device in order {
        reconnect(&mut steps, device, true);
    }
    world.phase(phase, &steps).unwrap();
}

#[test]
fn a_rename_applied_after_a_concurrent_rename_survives_a_restore_to_between_them() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "c.txt")), (0, SCAN), (0, COMMIT)];
    steps.extend([(1, rename(0, 0, "b")), (1, SCAN), (1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, BACKUP), (0, COMMIT)]);
    steps.extend([(0, DELIVER); 4]);
    steps.extend(take(1));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["c.txt"]);
    world.phase(1, &[(0, RESTORE)]).unwrap();
    every_disk_shows(&world, &[0, 1], "c.txt");
}

#[test]
fn a_rename_made_while_a_rename_to_the_name_the_server_already_held_waits_for_its_result_survives_a_restore()
 {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(1, rename(0, 0, "b")), (1, SCAN), (1, COMMIT), (1, COMMIT)];
    steps.extend([
        (0, BACKUP),
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
    ]);
    steps.push((0, rename(0, 0, "c.txt")));
    steps.extend([(0, DELIVER); 4]);
    steps.extend([
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
    ]);
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["c.txt"]);
    world.phase(1, &[(0, RESTORE)]).unwrap();
    every_disk_shows(&world, &[0, 1], "c.txt");
}

#[test]
fn a_rename_made_over_another_devices_rename_survives_when_both_are_sent_again_older_first() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (1, rename(0, 0, "b"))];
    steps.extend(commit(1));
    steps.extend(take(0));
    steps.push((0, rename(0, 0, "c.txt")));
    steps.extend(commit(0));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["c.txt"]);
    restore_and_reconnect(&mut world, 2, &[1, 0]);
    every_disk_shows(&world, &[0, 1], "c.txt");
}

#[test]
fn a_rename_made_over_the_rename_of_a_device_that_left_survives_a_restore_to_before_both() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (1, rename(0, 0, "b"))];
    steps.extend(commit(1));
    steps.push((1, Step::Leave));
    steps.extend(take(0));
    steps.push((0, rename(0, 0, "c.txt")));
    steps.extend(commit(0));
    steps.push((0, RESTORE));
    world.phase(1, &steps).unwrap();
    every_disk_shows(&world, &[0], "c.txt");
    assert_eq!(live_names(&world), ["c.txt"]);
}

#[test]
fn a_rename_made_after_a_restore_by_a_device_that_never_saw_the_loss_beats_the_lost_rename() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, rename(0, 0, "c.txt"))];
    steps.extend(commit(0));
    steps.push((0, RESTORE));
    steps.push((1, Step::Connect));
    steps.extend(take(1));
    steps.push((1, rename(0, 0, "b")));
    steps.extend(commit(1));
    reconnect(&mut steps, 0, true);
    world.phase(1, &steps).unwrap();
    every_disk_shows(&world, &[0, 1], "b");
}

#[test]
fn the_later_of_two_concurrent_lost_renames_survives_when_the_earlier_is_sent_again_first() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![
        (0, BACKUP),
        (0, rename(0, 0, "c.txt")),
        (0, SCAN),
        (0, COMMIT),
    ];
    steps.extend([(1, rename(0, 0, "b")), (1, SCAN), (1, COMMIT)]);
    steps.extend([(0, COMMIT), (1, COMMIT)]);
    steps.extend([(1, DELIVER); 4]);
    steps.push((0, DELIVER));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["b"]);
    restore_and_reconnect(&mut world, 2, &[0, 1]);
    every_disk_shows(&world, &[0, 1], "b");
}

#[test]
fn a_later_rename_survives_a_restore_when_an_earlier_rename_whose_result_was_lost_is_sent_again_first()
 {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, rename(0, 0, "c.txt"))];
    steps.extend([(0, SCAN), (0, COMMIT), (0, COMMIT), (0, Step::Drop)]);
    steps.extend([(1, rename(0, 0, "b")), (1, SCAN), (1, COMMIT), (1, COMMIT)]);
    steps.extend([(1, DELIVER); 4]);
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["b"]);
    restore_and_reconnect(&mut world, 2, &[0, 1]);
    every_disk_shows(&world, &[0, 1], "b");
}

#[test]
fn a_later_rename_survives_a_restore_when_a_bystander_sends_the_earlier_lost_rename_again_first() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (1, rename(0, 0, "b"))];
    steps.extend(commit(1));
    steps.push((1, Step::Leave));
    steps.extend(take(2));
    steps.extend(take(0));
    steps.push((0, rename(0, 0, "c.txt")));
    steps.extend(commit(0));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["c.txt"]);
    restore_and_reconnect(&mut world, 2, &[2, 0]);
    every_disk_shows(&world, &[0, 2], "c.txt");
}

#[test]
fn a_move_the_server_renamed_is_sent_again_when_a_restore_loses_the_rename() {
    let mut lost = vec![
        (0, Step::Backup),
        (1, rename(1, 0, "ss")),
        (1, overwrite(1)),
    ];
    lost.extend([(1, Step::Work(0)); 4]);
    lost.push((0, Step::Restore));
    let phases = [
        vec![
            (1, folder("a")),
            (1, user(1, 0, 1, "b")),
            (1, folder("\u{df}")),
        ],
        lost,
        vec![],
    ];
    let world = converge(2, &phases).unwrap();
    assert_eq!(files_on(&world, 0), files_on(&world, 1));
    assert!(world.devices.iter().all(|device| device.disk.shows(2)));
}

fn moved_beside_a_folder_whose_name_folds_to_ss() -> World {
    converge(
        2,
        &[vec![
            (1, folder("a")),
            (1, user(1, 0, 1, "b")),
            (1, folder("\u{df}")),
        ]],
    )
    .unwrap()
}

fn visible_b(world: &World, device: usize) -> usize {
    let ino = ino_at(world, device, &["a", "b"]);
    (world.devices[device].disk.visible().into_iter())
        .position(|visible| visible == ino)
        .unwrap()
}

#[test]
fn a_rename_another_device_makes_after_a_restore_beats_a_server_renamed_move_the_restore_lost() {
    let mut world = moved_beside_a_folder_whose_name_folds_to_ss();
    let mut steps = vec![(0, BACKUP), (1, rename(1, 0, "ss"))];
    steps.extend(commit(1));
    steps.extend([(0, RESTORE), (0, Step::Connect)]);
    steps.extend(take(0));
    steps.push((0, rename(visible_b(&world, 0), 0, "c.txt")));
    steps.extend(commit(0));
    reconnect(&mut steps, 1, true);
    world.phase(1, &steps).unwrap();
    assert!(
        live_names(&world).contains(&"c.txt".to_owned()),
        "{:?}",
        live_names(&world)
    );
}

#[test]
fn a_server_renamed_move_the_restore_kept_is_not_sent_over_a_later_rename_the_restore_lost() {
    let mut world = moved_beside_a_folder_whose_name_folds_to_ss();
    let mut steps = vec![(1, rename(1, 0, "ss"))];
    steps.extend(commit(1));
    steps.extend([(0, BACKUP), (0, Step::Connect)]);
    steps.extend(take(0));
    steps.push((0, rename(visible_b(&world, 0), 0, "c.txt")));
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 4]);
    world.run(1, &steps).unwrap();
    restore_and_reconnect(&mut world, 2, &[1, 0]);
    assert!(
        live_names(&world).contains(&"c.txt".to_owned()),
        "{:?}",
        live_names(&world)
    );
}

#[test]
fn a_later_delete_the_restore_lost_stays_done_when_the_renamed_mover_reconnects_first() {
    let mut steps = vec![
        (1, rename(1, 0, "ss")),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (0, BACKUP),
    ];
    steps.extend(take(0));
    steps.extend([(0, delete(2)), (0, SCAN), (0, COMMIT), (0, COMMIT)]);
    steps.extend([(1, DELIVER); 6]);
    steps.push((0, RESTORE));
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    let phases = [
        vec![
            (1, folder("a")),
            (1, user(1, 0, 1, "b")),
            (1, folder("\u{df}")),
        ],
        steps,
        vec![],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_move_the_server_renamed_is_sent_again_after_each_of_two_restores_loses_the_rename() {
    let mut world = moved_beside_a_folder_whose_name_folds_to_ss();
    let mut steps = vec![(0, BACKUP), (1, rename(1, 0, "ss")), (1, overwrite(1))];
    steps.extend(commit(1));
    for _ in 0..2 {
        steps.push((0, RESTORE));
        reconnect(&mut steps, 1, true);
    }
    world.phase(1, &steps).unwrap();
    assert_eq!(files_on(&world, 0), files_on(&world, 1));
    assert!(world.devices.iter().all(|device| device.disk.shows(2)));
    assert!(
        live_names(&world).iter().any(|name| name.starts_with("ss")),
        "{:?}",
        live_names(&world)
    );
}
