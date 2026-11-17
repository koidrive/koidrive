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
fn a_store_copied_before_the_listing_takes_nothing_as_held_at_a_slot_the_server_gave_out_again() {
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
    assert_eq!(commits.held_through(), 0);
    world.phase(4, &[]).unwrap();
}
