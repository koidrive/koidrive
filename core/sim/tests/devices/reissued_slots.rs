// SPDX-License-Identifier: Apache-2.0

use super::*;

fn device_0_reissues_slot_1(lost: &[(usize, Step)]) -> Result<World, Failure> {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]])?;
    let mut b = vec![(0, BACKUP), (0, Step::RestoreEarlier), (1, Step::Connect)];
    b.extend([(1, Step::Work(0)); 16]);
    b.push((1, file("b")));
    b.extend(commit(1));
    b.extend([(1, Step::Work(0)); 8]);
    world.phase(1, &b)?;
    let mut edit = vec![(1, overwrite(0))];
    edit.extend(commit(1));
    world.phase(2, &edit)?;
    let mut edit = vec![(0, overwrite(1))];
    edit.extend(commit(0));
    world.phase(3, &edit)?;
    world.phase(4, &[(1, Step::Leave)])?;
    world.phase(5, lost)?;
    Ok(world)
}

#[test]
fn another_devices_edit_is_sent_again_when_a_restore_brings_back_another_commit_at_this_devices_slot()
 {
    device_0_reissues_slot_1(&[(0, RESTORE)]).unwrap();
}

#[test]
fn another_devices_edit_is_sent_again_when_a_second_restore_brings_back_another_commit_at_a_kept_slot()
 {
    device_0_reissues_slot_1(&[(0, Step::RestoreEarlier), (0, Step::Connect), (0, RESTORE)])
        .unwrap();
}

#[test]
fn a_store_copied_before_the_listing_takes_the_kept_slot_below_one_the_server_gave_out_again_as_held()
 {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, file("b"))]],
    )
    .unwrap();
    world
        .phase(2, &[(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)])
        .unwrap();
    let copy = &world.devices[0].store_copy.as_ref().unwrap().commits;
    assert_eq!(copy.accepted(), 1);
    assert!(copy.made_here(2));
    let shared = copy.state().slots[&1].seq;
    let live = &world.devices[0].engine.commits;
    assert_eq!(live.accepted(), 2);
    world
        .run(
            3,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    let commits = &world.devices[0].engine.commits;
    assert_eq!(commits.accepted(), 2);
    assert_eq!(commits.held_through(), shared);
    world.phase(4, &[]).unwrap();
}

#[test]
fn a_store_copied_before_the_listing_takes_another_devices_delete_of_a_file_it_edited_before_the_loss()
 {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, overwrite(0))]]).unwrap();
    world.phase(2, &[(0, BACKUP), (0, file("b"))]).unwrap();
    let mut lost = vec![(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)];
    lost.extend([(0, Step::Work(0)); 40]);
    lost.push((1, Step::Connect));
    lost.extend([(1, Step::Work(0)); 16]);
    lost.push((1, delete(0)));
    lost.extend(commit(1));
    lost.extend([(1, Step::Work(0)); 8]);
    world.run(3, &lost).unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 3);
    world
        .run(
            4,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 3);
    world.phase(5, &[]).unwrap();
}

#[test]
fn the_restore_cases_hold_when_the_server_sends_no_commit_token() {
    TOKENLESS_WELCOME.set(true);
    a_file_another_device_edits_and_deletes_across_a_restore_stays_deleted();
    a_kept_child_of_a_lost_folder_stays_deleted_after_a_crash_mid_effect();
    a_lost_folder_is_not_created_again_inside_a_folder_deleted_while_it_waited();
    a_later_edit_made_over_a_kept_write_does_not_need_a_recovery_copy();
    a_lost_move_and_edit_sent_again_after_a_restore_keep_the_write_in_one_file();
}
