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

fn every_disk_shows(world: &World, devices: &[usize], shown: &[&str]) {
    for &device in devices {
        let names: Vec<String> = (files_on(world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, shown, "device {device}");
    }
}

fn restore_and_reconnect(world: &mut World, phase: usize, order: &[usize]) {
    let mut steps = vec![(0, RESTORE)];
    for &device in order {
        reconnect(&mut steps, device, true);
    }
    world.phase(phase, &steps).unwrap();
}

fn rename_then_delete_before_restore(created: Vec<(usize, Step)>, shown: &[&str]) -> World {
    let mut world = converge(2, &[created]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, rename(0, 0, "b"))];
    steps.extend(commit(0));
    steps.extend(take(1));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), shown);
    every_disk_shows(&world, &[0, 1], shown);

    let mut steps = vec![(1, delete(0))];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 4]);
    world.run(2, &steps).unwrap();
    assert!(live_names(&world).is_empty());
    every_disk_shows(&world, &[0], shown);
    world
}

#[test]
fn a_lost_delete_of_a_renamed_file_wins_when_the_renamer_reconnects_first() {
    let mut world = rename_then_delete_before_restore(vec![(0, file("a"))], &["b"]);
    restore_and_reconnect(&mut world, 3, &[0, 1]);
    assert!(live_names(&world).is_empty());
    assert!(no_device_holds_a_file(&world));
}

fn folder_with_a_file() -> Vec<(usize, Step)> {
    vec![(0, folder("a")), (0, user(1, 0, 1, "c.txt"))]
}

#[test]
fn a_lost_delete_of_a_renamed_folder_wins_when_the_renamer_reconnects_first() {
    let mut world = rename_then_delete_before_restore(folder_with_a_file(), &["b", "c.txt"]);
    restore_and_reconnect(&mut world, 3, &[0, 1]);
    assert!(live_names(&world).is_empty());
    assert!(no_device_holds_a_file(&world));
}

#[test]
fn a_lost_delete_spares_a_fresh_rename_at_a_revision_a_lost_rename_had() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, rename(0, 0, "b"))];
    steps.extend(commit(0));
    steps.extend(take(1));
    world.run(1, &steps).unwrap();
    every_disk_shows(&world, &[0, 1], &["b"]);
    every_disk_shows(&world, &[2], &["a"]);
    let lost_rev = meta_rev_of(&world, "b");

    let mut steps = vec![(0, rename(0, 0, "c.txt"))];
    steps.extend(commit(0));
    steps.extend(take(1));
    world.run(2, &steps).unwrap();
    every_disk_shows(&world, &[0, 1], &["c.txt"]);

    let mut steps = vec![(1, delete(0))];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 4]);
    world.run(3, &steps).unwrap();
    assert!(live_names(&world).is_empty());

    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 2, false);
    steps.push((2, rename(0, 0, "b")));
    steps.extend(commit(2));
    world.run(4, &steps).unwrap();
    assert_eq!(live_names(&world), ["b"]);
    assert_eq!(meta_rev_of(&world, "b"), lost_rev);

    let mut steps = Vec::new();
    reconnect(&mut steps, 1, true);
    world.run(5, &steps).unwrap();
    assert_eq!(live_names(&world), ["b"]);
}

fn meta_rev_of(world: &World, name: &str) -> u64 {
    (world.space().nodes().into_iter())
        .find(|node| node.name == name)
        .unwrap()
        .meta_rev
}

#[test]
fn a_lost_delete_wins_when_its_device_made_a_new_file_at_the_name() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (1, delete(0))];
    steps.extend(commit(1));
    steps.extend(take(0));
    steps.push((1, act(1, 0, 0, "a", 3)));
    steps.extend(commit(1));
    steps.extend(take(0));
    world.run(1, &steps).unwrap();
    assert_eq!(live_names(&world), ["a"]);
    restore_and_reconnect(&mut world, 2, &[1, 0]);
    assert_eq!(live_names(&world), ["a"]);
    every_disk_shows(&world, &[0, 1], &["a"]);
}

#[test]
fn a_lost_delete_spares_a_fresh_rename_at_a_reused_revision_where_a_lost_create_held_the_name() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let original_id = (world.space().nodes().into_iter())
        .find(|n| n.name == "a")
        .unwrap()
        .node_id;
    let mut steps = vec![(0, BACKUP), (0, rename(0, 0, "b"))];
    steps.extend(commit(0));
    steps.extend(take(1));
    world.run(1, &steps).unwrap();
    let lost_rev = meta_rev_of(&world, "b");
    let mut steps = vec![(1, delete(0))];
    steps.extend(commit(1));
    steps.push((1, act(1, 0, 0, "b", 3)));
    steps.extend(commit(1));
    world.run(2, &steps).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 2, false);
    steps.push((2, rename(0, 0, "b")));
    steps.extend(commit(2));
    world.run(3, &steps).unwrap();
    assert_eq!(meta_rev_of(&world, "b"), lost_rev);
    let mut steps = Vec::new();
    reconnect(&mut steps, 1, true);
    world.run(4, &steps).unwrap();
    let original = (world.space().nodes().into_iter())
        .find(|n| n.node_id == original_id)
        .unwrap();
    assert_eq!(original.status, koidrive_proto::v1::NodeStatus::Live as i32);
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
    every_disk_shows(&world, &[0, 1], &["c.txt"]);
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
    every_disk_shows(&world, &[0, 1], &["c.txt"]);
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
    every_disk_shows(&world, &[0, 1], &["c.txt"]);
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
    every_disk_shows(&world, &[0], &["c.txt"]);
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
    every_disk_shows(&world, &[0, 1], &["b"]);
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
    every_disk_shows(&world, &[0, 1], &["b"]);
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
    every_disk_shows(&world, &[0, 1], &["b"]);
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
    every_disk_shows(&world, &[0, 2], &["c.txt"]);
}
