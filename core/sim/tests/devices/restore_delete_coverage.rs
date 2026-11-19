// SPDX-License-Identifier: Apache-2.0

use super::*;
use koidrive_engine::tree::NodeId;

fn take(device: usize) -> Vec<(usize, Step)> {
    let mut steps = vec![(device, DELIVER); 4];
    steps.extend([(device, EFFECT); 4]);
    steps
}

fn live_files(world: &World) -> usize {
    let root = world.space().space_id();
    (world.space().nodes().into_iter())
        .filter(|node| node.node_id != root)
        .filter(|node| node.status == koidrive_proto::v1::NodeStatus::Live as i32)
        .count()
}

fn disks_are_empty(world: &World, devices: &[usize]) {
    for &device in devices {
        assert_eq!(files_on(world, device), [], "device {device}");
    }
}

#[test]
#[ignore = "a lost edit its author sends again before the delete of a user who held it brings the deleted file back (KOI-1020 #2)"]
fn a_lost_edit_sent_again_before_the_delete_of_a_user_who_held_it_stays_deleted() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.extend(take(1));
    steps.push((1, delete(0)));
    steps.extend(commit(1));
    steps.push((0, RESTORE));
    steps.push((2, Step::Connect));
    steps.extend(take(2));
    steps.push((2, delete(0)));
    steps.extend(commit(2));
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(1, &steps).unwrap();
    disks_are_empty(&world, &[0, 1, 2]);
}

#[test]
#[ignore = "an edit sent again after a restore revives a file that a later delete made after seeing the edit trashed (KOI-1020 #4)"]
fn a_delete_made_after_an_edit_that_revived_a_deleted_file_stays_when_both_are_sent_again() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, delete(0))];
    steps.extend(commit(0));
    steps.extend([
        (0, BACKUP),
        (1, overwrite(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
    ]);
    steps.extend([(1, DELIVER); 4]);
    steps.extend(take(0));
    world.run(1, &steps).unwrap();
    assert_eq!(live_files(&world), 1);
    let mut steps = vec![(0, delete(0))];
    steps.extend(commit(0));
    steps.push((0, RESTORE));
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    world.phase(1, &steps).unwrap();
    disks_are_empty(&world, &[0, 1]);
}

#[test]
#[ignore = "a copy the server makes of a lost edit keeps alive a folder another device deleted (KOI-1020 #5)"]
fn a_lost_edit_kept_as_a_copy_in_a_folder_another_device_deletes_is_kept_outside_it() {
    let mut world = converge(3, &[vec![(0, folder("b")), (0, user(1, 0, 1, "a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, overwrite(1))];
    steps.extend(commit(0));
    steps.extend(take(1));
    world.run(1, &steps).unwrap();
    let lost = content_on(&world, 1, "a");
    let mut steps = vec![(0, RESTORE), (2, Step::Connect)];
    steps.extend(take(2));
    steps.push((2, overwrite(1)));
    steps.extend(commit(2));
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    reconnect(&mut steps, 1, true);
    steps.push((2, COMMIT));
    steps.extend([(2, DELIVER); 4]);
    world.phase(1, &steps).unwrap();
    for device in 0..3 {
        let files = files_on(&world, device);
        assert!(
            files.iter().all(|(_, content)| content.is_some()),
            "device {device}: {files:?}"
        );
        assert!(
            files
                .iter()
                .any(|(_, content)| content.as_ref() == Some(&lost)),
            "device {device}: {files:?}"
        );
    }
}

#[test]
#[ignore = "a bystander's resend of a lost edit revives a file whose deleter held the edit (KOI-1020 #6)"]
fn a_delete_whose_user_held_a_lost_edit_beats_a_bystanders_resend_of_it() {
    let mut world = converge(3, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.extend(take(1));
    steps.extend(take(2));
    steps.extend([(2, delete(0)), (2, SCAN), (0, RESTORE)]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 2, true);
    world.phase(1, &steps).unwrap();
    disks_are_empty(&world, &[0, 1, 2]);
}

#[test]
fn a_folder_deleted_before_a_restore_to_before_its_create_stays_deleted_when_a_bystander_creates_it_again()
 {
    let mut steps = vec![(0, BACKUP), (0, folder("b"))];
    steps.extend(commit(0));
    steps.push((0, Step::Leave));
    steps.extend(take(1));
    steps.extend(take(2));
    steps.push((2, delete(0)));
    steps.extend(commit(2));
    steps.push((0, RESTORE));
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 2, true);
    let world = converge(3, &[steps]).unwrap();
    disks_are_empty(&world, &[1, 2]);
}

#[test]
fn a_delete_made_after_a_restore_from_the_trash_stays_when_a_backup_from_before_the_first_delete_returns()
 {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let root = world.space().space_id().to_owned();
    let file = (world.space().nodes().into_iter())
        .find(|node| node.node_id != root)
        .unwrap()
        .node_id;
    let mut steps = vec![(0, BACKUP), (0, delete(0))];
    steps.extend(commit(0));
    world.phase(1, &steps).unwrap();
    world.restored_from_trash(0, &file);
    let engine = &mut world.devices[0].engine;
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::from_trash(NodeId::from(file), None, None).unwrap();
    assert!(engine.commits.restore(request, intent));
    world.phase(2, &commit(0)).unwrap();
    assert_eq!(live_files(&world), 1);
    let mut steps = vec![(0, Step::Leave), (1, delete(0))];
    steps.extend(commit(1));
    steps.push((1, RESTORE));
    world.phase(3, &steps).unwrap();
    disks_are_empty(&world, &[1]);
}
