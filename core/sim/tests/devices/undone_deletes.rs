// SPDX-License-Identifier: Apache-2.0

use super::*;

fn checked(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    let mut world = world(devices);
    world.revived_deletes = true;
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule)?;
    }
    Ok(world)
}

#[test]
#[ignore = "a delete gives way to the older value a restore brings back (GUN-864)"]
fn a_delete_outlasts_a_restore_that_brings_back_an_older_value() {
    let u = |kind, target, name| {
        Step::User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: 0,
        })
    };
    checked(
        2,
        &[
            vec![(0, u(1, 0, 0)), (0, BACKUP)],
            vec![
                (0, u(0, 0, 1)),
                (0, u(0, 0, 2)),
                (0, BACKUP),
                (0, u(2, 6307646474509524231, 0)),
                (0, Step::RestoreEarlier),
            ],
            vec![
                (0, u(4, 3700173026859558585, 0)),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a delete gives way to the older value a restore brings back (GUN-864)"]
fn a_delete_outlasts_a_restore_after_another_device_renamed_the_file() {
    checked(
        2,
        &[
            vec![(0, file("a")), (0, BACKUP)],
            vec![(0, BACKUP), (0, overwrite(0)), (0, Step::RestoreEarlier)],
            vec![(1, rename(0, 0, "c.txt"))],
            vec![
                (1, Step::Leave),
                (0, delete(0)),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_file_comes_back_when_a_restore_takes_a_delete_that_reached_the_server() {
    let world = checked(
        2,
        &[
            vec![(0, file("a"))],
            vec![
                (0, BACKUP),
                (0, delete(0)),
                (0, SCAN),
                (0, Step::Commit(Reply::Served)),
                (0, Step::Commit(Reply::Served)),
                (0, Step::Leave),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
    let held: Vec<_> = (files_on(&world, 1).into_iter())
        .map(|(name, content)| (name, content.as_ref().map(tag)))
        .collect();
    assert_eq!(held, [("a".to_owned(), Some(1))]);
}

fn deletes_reach_the_server_in(
    test: &'static str,
    file: &'static str,
    cases: impl Strategy<Value = (usize, Vec<Vec<(usize, Step)>>)>,
) {
    let mut runner = TestRunner::new(ProptestConfig {
        test_name: Some(test),
        ..koidrive_test_support::regression_config(file, 0)
    });
    let result = runner.run(&cases, |(devices, phases)| {
        checked(devices, &phases)
            .map(drop)
            .map_err(|failure| TestCaseError::fail(failure.to_string()))
    });
    if let Err(error) = result {
        panic!("{error}");
    }
}

#[test]
#[ignore = "a delete gives way to the older value a restore brings back (GUN-864)"]
fn deletes_reach_the_server_in_the_saved_gun_864_cases_across_restores_and_rewinds() {
    deletes_reach_the_server_in(
        concat!(
            module_path!(),
            "::deletes_reach_the_server_in_the_saved_gun_864_cases_across_restores_and_rewinds"
        ),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/gun-864-restores-and-rewinds.proptest-regressions"
        ),
        (2..=3usize, phases(Losses::RestoresAndRewinds)),
    );
}

#[test]
#[ignore = "a delete gives way to the older value a restore brings back (GUN-864)"]
fn deletes_reach_the_server_in_the_saved_gun_864_cases_across_server_restores() {
    deletes_reach_the_server_in(
        concat!(
            module_path!(),
            "::deletes_reach_the_server_in_the_saved_gun_864_cases_across_server_restores"
        ),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/gun-864-server-restores.proptest-regressions"
        ),
        (2..=3usize, phases(Losses::Restores)),
    );
}

#[test]
fn a_bystanders_store_rollback_leaves_another_devices_delete_checked() {
    let failure = checked(
        3,
        &[
            vec![(0, file("a")), (2, Step::CopyStore), (0, BACKUP)],
            vec![(0, BACKUP), (0, overwrite(0)), (0, Step::RestoreEarlier)],
            vec![(1, rename(0, 0, "c.txt"))],
            vec![
                (1, Step::Leave),
                (0, delete(0)),
                (2, Step::RollBack),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    )
    .err();
    assert!(failure.is_some_and(|f| {
        f.to_string()
            .contains("the delete never reached the server")
    }));
}

#[test]
fn another_devices_copy_does_not_excuse_the_deleted_file() {
    let u = |kind, target, name| {
        Step::User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: 0,
        })
    };
    let copy = Step::Copy(Action {
        kind: 0,
        target: 0,
        parent: 0,
        name: 3,
        text: 0,
    });
    let failure = checked(
        2,
        &[
            vec![(0, u(1, 0, 0)), (0, BACKUP)],
            vec![
                (0, u(0, 0, 1)),
                (0, u(0, 0, 2)),
                (0, BACKUP),
                (0, u(2, 6307646474509524231, 0)),
                (0, Step::RestoreEarlier),
            ],
            vec![
                (1, copy),
                (0, u(4, 3700173026859558585, 0)),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    )
    .err();
    assert!(failure.is_some_and(|f| {
        f.to_string()
            .contains("the delete never reached the server")
    }));
}

#[test]
fn a_restore_the_deleting_user_chooses_after_a_rewind_brings_the_file_back() {
    checked(
        2,
        &[
            vec![
                (0, act(1, 0, 0, "A", 3)),
                (1, act(1, 0, 0, "ss", 1)),
                (1, BACKUP),
            ],
            vec![(1, Step::Rewind)],
            vec![(1, act(4, 0, 0, "ß", 3)), (1, act(2, 0, 0, "\u{e9}", 1))],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_another_device_owes_on_a_file_moved_into_the_deleted_folder_brings_it_back() {
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-76b6-8f17-7744bb83aba9 on device 1 came undone, and no change its user did not see brought the node back",
    );
    checked(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, act(3, 0, 0, "b", 2)), (0, act(0, 0, 0, "a", 1))],
            vec![(1, act(4, 0, 0, "ß", 1)), (0, act(1, 0, 0, "ss", 3))],
            vec![
                (0, BACKUP),
                (0, rename(1, 0, "A")),
                (1, rename(1, 1, "\u{e9}")),
                (1, act(4, 0, 0, "ss", 0)),
            ],
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_folder_another_device_renames_after_the_delete_keeps_its_file_once_moved_out() {
    checked(
        2,
        &[
            vec![(0, folder("b"))],
            vec![
                (0, BACKUP),
                (0, act(1, 0, 0, "\u{e9}", 1)),
                (0, act(4, 1, 0, "b", 0)),
                (1, act(4, 0, 0, "a", 0)),
                (1, folder("C.TXT")),
                (1, act(1, 0, 1, "\u{c9}", 0)),
                (0, act(0, 0, 0, "E\u{301}", 2)),
                (0, act(3, 0, 2, "C.TXT", 0)),
                (1, act(1, 0, 1, "ß", 2)),
                (0, act(1, 0, 1, "C.TXT", 0)),
                (0, RESTORE),
            ],
            vec![
                (1, act(4, 4, 0, "a", 0)),
                (0, act(4, 4, 0, "A", 2)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (0, act(3, 0, 0, "b", 2)),
            ],
            vec![(1, act(3, 4, 0, "ß", 3))],
        ],
    )
    .unwrap();
}

#[test]
fn a_deleted_conflict_copy_leaves_the_original_with_the_same_write() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 2 is in 2 live nodes on the server: [[\"c (conflict from device-1 2026-11-09).txt\"], [\"c.txt\"]]",
    );
    checked(
        3,
        &[
            vec![
                (1, act(1, 0, 0, "a", 3)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (0, BACKUP),
                (1, act(3, 0, 0, "c.txt", 3)),
                (1, act(2, 0, 0, "\u{e9}", 3)),
            ],
            vec![(1, RESTORE)],
            vec![(1, delete(0))],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_of_the_folder_above_that_its_user_never_saw_leaves_the_delete_checked() {
    let mut renamed = vec![(0, BACKUP), (1, rename(0, 0, "ss"))];
    renamed.extend(commit(1));
    renamed.push((0, delete(1)));
    renamed.extend(commit(0));
    checked(
        2,
        &[
            vec![(0, folder("a")), (0, user(1, 0, 1, "b"))],
            renamed,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_move_another_device_made_to_a_folder_moved_into_the_deleted_folder_keeps_its_file() {
    checked(
        2,
        &[
            vec![(0, folder("b"))],
            vec![(1, file("ss"))],
            vec![(1, delete(1))],
            vec![
                (0, BACKUP),
                (0, rename(0, 0, "ss")),
                (0, SCAN),
                (0, COMMIT),
                (1, delete(0)),
                (1, folder("c.txt")),
                (1, user(1, 0, 1, "a")),
                (0, act(0, 0, 0, "\u{e9}", 3)),
                (1, act(1, 0, 0, "\u{c9}", 3)),
                (1, file("\u{e9}")),
                (0, RESTORE),
            ],
            vec![
                (1, act(3, 0, 3, "ss", 3)),
                (0, act(3, 2, 1, "C.TXT", 3)),
                (1, act(4, 5, 0, "ß", 3)),
                (1, Step::Work(3)),
                (1, Step::Work(1)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_change_that_never_reaches_the_server_leaves_the_delete_checked() {
    let failure = checked(
        2,
        &[
            vec![(0, file("a")), (0, BACKUP)],
            vec![(0, BACKUP), (0, overwrite(0)), (0, Step::RestoreEarlier)],
            vec![(1, rename(0, 0, "c.txt"))],
            vec![
                (0, delete(0)),
                (1, overwrite(0)),
                (1, Step::Leave),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    )
    .err();
    assert!(failure.is_some_and(|f| {
        f.to_string()
            .contains("the delete never reached the server")
    }));
}

#[test]
fn a_file_another_device_adds_to_the_deleted_folder_leaves_its_siblings_checked() {
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let mut deleted = vec![(0, BACKUP), (1, user(1, 0, 1, "c.txt")), (0, delete(0))];
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    checked(
        2,
        &[
            vec![(0, folder("b")), (0, user(1, 0, 1, "a"))],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_another_device_makes_to_a_folder_between_leaves_the_deleted_file_checked() {
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7735-80db-d7e668859a70 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let mut deleted = vec![(0, BACKUP), (1, rename(1, 1, "A")), (0, delete(0))];
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(0, 0, 1, "c.txt")),
                (0, user(1, 0, 2, "a")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_another_device_sends_after_the_delete_brings_the_folder_back_with_its_file() {
    let deleted = vec![
        (1, rename(2, 0, "c.txt")),
        (0, delete(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (0, BACKUP),
        (1, rename(0, 0, "A")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (0, RESTORE),
    ];
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            deleted,
        ],
    )
    .unwrap();
}

#[test]
fn a_folder_another_device_puts_back_under_its_old_parent_keeps_the_file_the_deleting_device_added()
{
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7504-af4d-4d6a531020af on device 1 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 1 came undone, and no change its user did not see brought the node back",
    );
    let mut moved = vec![
        (1, folder("C.TXT")),
        (1, rename(0, 2, "ss")),
        (1, user(1, 0, 1, "a")),
    ];
    moved.extend(commit(1));
    moved.extend([
        (0, BACKUP),
        (0, rename(0, 0, "ss")),
        (1, delete(0)),
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
    ]);
    let world = checked(2, &[vec![(0, folder("b"))], moved, vec![(0, RESTORE)]]).unwrap();
    for device in 0..2 {
        assert_eq!(
            held_files(&world, device),
            [("a".to_owned(), 1)],
            "device {device}"
        );
    }
}

#[test]
fn a_folder_another_device_takes_back_out_of_the_deleted_folder_keeps_its_file() {
    let _folder = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-796a-8df8-fb6da7904e2f on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7c5c-8466-a7d0954b76a3 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let mut deleted = vec![
        (1, rename(0, 0, "A")),
        (0, rename(1, 1, "A")),
        (1, Step::Work(0)),
        (0, Step::Work(0)),
        (0, delete(0)),
        (1, Step::Work(0)),
    ];
    deleted.extend([(0, Step::Work(0)); 2]);
    deleted.push((0, BACKUP));
    deleted.extend([(0, Step::Work(0)); 3]);
    deleted.push((1, COMMIT));
    let world = checked(
        2,
        &[
            vec![
                (0, folder("a")),
                (1, folder("\u{e9}")),
                (1, user(1, 0, 1, "\u{e9}")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
    for device in 0..2 {
        assert_eq!(
            held_files(&world, device),
            [("\u{e9}".to_owned(), 1)],
            "device {device}"
        );
    }
}

fn held_files(world: &World, device: usize) -> Vec<(String, i64)> {
    (files_on(world, device).into_iter())
        .filter_map(|(name, content)| Some((name, tag(&content?))))
        .collect()
}

#[test]
fn a_file_another_device_adds_after_the_delete_leaves_the_deleted_file_checked() {
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7cdc-8389-c4663807ce68 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let deleted = vec![
        (0, BACKUP),
        (1, rename(2, 0, "c.txt")),
        (0, delete(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, user(1, 0, 1, "A")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
    ];
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_copy_another_device_sends_after_the_delete_leaves_the_deleted_file_checked() {
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7cdc-8389-c4663807ce68 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let copy = Step::Copy(Action {
        kind: 1,
        target: 1,
        parent: 0,
        name: 6,
        text: 0,
    });
    let world = checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            vec![
                (0, BACKUP),
                (1, rename(2, 0, "c.txt")),
                (1, copy),
                (0, delete(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, user(1, 0, 1, "A")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
            ],
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
    assert!(held_files(&world, 1).contains(&("ss".to_owned(), 1)));
}

#[test]
fn a_file_another_device_adds_to_the_deleted_folder_brings_back_only_the_folder() {
    let world = checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            vec![
                (1, rename(2, 0, "c.txt")),
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (1, user(1, 0, 1, "A")),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
            ],
        ],
    )
    .unwrap();
    for device in 0..2 {
        assert!(!held_files(&world, device).contains(&("a".to_owned(), 1)));
    }
}

#[test]
fn a_rename_of_a_folder_the_server_kept_leaves_the_deleted_file_checked() {
    let _also = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7cdc-8389-c4663807ce68 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 0 came undone, and no change its user did not see brought the node back",
    );
    let deleted = vec![
        (0, BACKUP),
        (1, overwrite(2)),
        (0, delete(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, rename(0, 0, "A")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
    ];
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

fn reports_the_lost_delete(result: Result<World, Failure>) {
    assert!(result.err().is_some_and(|failure| {
        failure
            .to_string()
            .contains("the delete never reached the server")
    }));
}

#[test]
fn a_bystanders_store_rollback_before_the_delete_leaves_it_checked() {
    reports_the_lost_delete(checked(
        3,
        &[
            vec![(0, file("a")), (2, Step::CopyStore), (0, BACKUP)],
            vec![(0, BACKUP), (0, overwrite(0)), (0, Step::RestoreEarlier)],
            vec![(1, rename(0, 0, "c.txt"))],
            vec![
                (1, Step::Leave),
                (2, Step::RollBack),
                (0, delete(0)),
                (0, Step::RestoreEarlier),
                (0, RESTORE),
            ],
        ],
    ));
}

#[test]
fn a_delete_made_before_its_device_hears_of_a_restore_stays_checked() {
    reports_the_lost_delete(checked(
        2,
        &[
            vec![(0, file("a")), (1, BACKUP)],
            vec![
                (1, RESTORE),
                (1, Step::Connect),
                (1, DELIVER),
                (1, COMMIT),
                (0, Step::Connect),
                (1, Step::Work(0)),
                (1, Step::Work(0)),
                (0, delete(0)),
                (0, Step::Work(0)),
            ],
        ],
    ));
}

#[test]
fn a_rename_another_device_makes_after_its_store_rollback_brings_the_file_back() {
    let world = checked(
        2,
        &[
            vec![(1, Step::CopyStore), (0, file("c.txt"))],
            vec![
                (0, BACKUP),
                (1, Step::RollBack),
                (0, rename(0, 0, "a")),
                (1, Step::Work(0)),
                (1, rename(0, 0, "A")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, delete(0)),
                (0, Step::Work(2093181476778376)),
                (1, Step::Connect),
                (1, Step::Work(2036517389419676280)),
                (0, RESTORE),
            ],
        ],
    )
    .unwrap();
    for device in 0..2 {
        let held: Vec<_> = (files_on(&world, device).into_iter())
            .map(|(name, content)| (name, content.as_ref().map(tag)))
            .collect();
        assert_eq!(held, [("A".to_owned(), Some(1))], "device {device}");
    }
}

#[test]
fn a_rename_and_back_the_deleting_device_never_saw_keeps_the_file() {
    let mut deleted = vec![(0, BACKUP), (0, rename(0, 0, "b"))];
    deleted.extend([(0, Step::Work(0)); 2]);
    deleted.push((0, RESTORE));
    deleted.extend([(0, Step::Work(0)); 2]);
    deleted.push((0, rename(0, 0, "E\u{301}")));
    deleted.extend([(0, Step::Work(0)); 3]);
    deleted.extend([(1, delete(0)), (1, Step::Work(1)), (1, Step::Work(1))]);
    let world = checked(2, &[vec![(0, act(1, 0, 0, "E\u{301}", 3))], deleted]).unwrap();
    for (device, name) in [(0, "E\u{301}"), (1, "\u{c9}")] {
        assert_eq!(held_files(&world, device), [(name.to_owned(), 1)]);
    }
}

#[test]
fn a_rename_the_deleting_device_received_but_never_applied_keeps_the_file() {
    let _red = expect_red(
        Check::DeleteUndone,
        "the delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 1 came undone, and no change its user did not see brought the node back",
    );
    let mut deleted = vec![(0, BACKUP), (0, rename(0, 0, "C.TXT"))];
    deleted.extend([(0, Step::Work(0)); 3]);
    deleted.extend([(1, DELIVER), (0, RESTORE), (1, delete(0))]);
    let world = checked(
        2,
        &[
            vec![
                (0, act(1, 0, 0, "C.TXT", 0)),
                (0, act(1, 0, 0, "a", 3)),
                (0, delete(0)),
            ],
            deleted,
        ],
    )
    .unwrap();
    for device in 0..2 {
        assert_eq!(
            held_files(&world, device),
            [("C.TXT".to_owned(), 2)],
            "device {device}"
        );
    }
}

#[test]
fn a_delete_whose_device_leaves_before_sending_it_owes_nothing() {
    checked(
        2,
        &[vec![(0, file("a"))], vec![(1, delete(0)), (1, Step::Leave)]],
    )
    .unwrap();
}

#[test]
fn a_restore_after_the_deleting_device_leaves_brings_back_the_file_it_deleted_beside_a_copy() {
    let copy = Step::Copy(Action {
        kind: 0,
        target: 0,
        parent: 0,
        name: 2,
        text: 0,
    });
    let mut deleted = vec![(0, delete(0)), (1, copy)];
    deleted.extend(commit(1).into_iter().take(3));
    deleted.push((0, BACKUP));
    deleted.extend(commit(0).into_iter().take(3));
    deleted.extend([(0, Step::Leave), (0, RESTORE)]);
    let world = checked(2, &[vec![(0, file("a"))], deleted]).unwrap();
    assert_eq!(
        held_files(&world, 1),
        [("a".to_owned(), 1), ("b".to_owned(), 1)]
    );
}

#[test]
fn a_rename_another_device_saves_over_before_it_leaves_keeps_the_file() {
    let mut renamed = vec![(1, rename(0, 0, "c.txt")), (0, delete(0))];
    renamed.extend(commit(1).into_iter().take(3));
    renamed.extend([
        (0, BACKUP),
        (
            1,
            Step::Save {
                target: 0,
                keeps_mark: false,
            },
        ),
        (1, SCAN),
        (1, Step::Leave),
        (0, RESTORE),
    ]);
    let world = checked(2, &[vec![(0, file("a"))], renamed]).unwrap();
    assert_eq!(held_files(&world, 0), [("c.txt".to_owned(), 1)]);
}

#[test]
fn a_copy_the_engine_makes_after_a_store_rollback_keeps_the_deleted_write() {
    checked(
        2,
        &[
            vec![
                (0, Step::CopyStore),
                (0, BACKUP),
                (1, act(1, 0, 0, "\u{e9}", 1)),
                (0, act(1, 0, 0, "c.txt", 3)),
            ],
            vec![(0, Step::RollBack), (0, BACKUP), (1, Step::RestoreEarlier)],
            vec![(1, RESTORE), (1, act(4, 1, 0, "\u{c9}", 1))],
        ],
    )
    .unwrap();
}

#[test]
fn a_save_another_device_makes_after_the_delete_brings_the_file_back() {
    let mut saved = vec![
        (0, delete(0)),
        (
            1,
            Step::Save {
                target: 0,
                keeps_mark: false,
            },
        ),
    ];
    saved.extend(commit(1));
    saved.push((0, BACKUP));
    saved.extend(commit(0));
    checked(2, &[vec![(0, file("a"))], saved, vec![(0, RESTORE)]]).unwrap();
}

#[test]
fn a_sibling_edit_leaves_the_deleted_file_checked_after_the_deleting_device_leaves() {
    let mut deleted = vec![(0, BACKUP), (1, overwrite(2)), (0, delete(0))];
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    deleted.push((0, Step::Leave));
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn an_unsent_edit_of_the_file_leaves_the_delete_checked_after_the_deleting_device_leaves() {
    let mut renamed = vec![(0, BACKUP), (1, rename(0, 0, "ss"))];
    renamed.extend(commit(1));
    renamed.push((0, delete(1)));
    renamed.extend(commit(0));
    renamed.extend([(1, overwrite(1)), (1, Step::Leave), (0, Step::Leave)]);
    checked(
        2,
        &[
            vec![(0, folder("a")), (0, user(1, 0, 1, "b"))],
            renamed,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_file_delete_sent_without_its_folder_stays_checked_after_the_deleting_device_leaves() {
    let mut deleted = vec![(0, BACKUP), (1, user(1, 0, 1, "c.txt"))];
    deleted.extend(commit(1));
    deleted.extend([
        (0, Step::Deliver(0)),
        (0, delete(0)),
        (1, rename(0, 0, "ss")),
    ]);
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    deleted.push((0, Step::Leave));
    checked(
        2,
        &[
            vec![(0, folder("b")), (0, user(1, 0, 1, "a"))],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn file_deletes_sent_one_by_one_stay_checked_after_the_deleting_device_leaves() {
    let mut deleted = vec![(0, BACKUP), (1, user(1, 0, 1, "ss"))];
    deleted.extend(commit(1));
    deleted.extend([
        (0, Step::Deliver(0)),
        (0, delete(0)),
        (1, rename(0, 0, "A")),
    ]);
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    deleted.push((0, Step::Leave));
    checked(
        2,
        &[
            vec![
                (0, folder("b")),
                (0, user(1, 0, 1, "a")),
                (0, user(1, 0, 1, "c.txt")),
            ],
            deleted,
            vec![(0, RESTORE)],
        ],
    )
    .unwrap();
}

#[test]
fn a_restore_answer_brings_back_the_file_its_user_deleted() {
    let mut world = on(&[Precision::Fine; 2]);
    world.phase(0, &[(0, u(1, 0, 0, 0, 0))]).unwrap();
    world.phase(1, &[(0, Step::Backup)]).unwrap();
    let mut schedule = vec![
        (0, u(4, 0, 0, 0, 0)),
        (0, Step::Scan),
        (0, Step::Rewind),
        (0, Step::Connect),
    ];
    schedule.extend([(0, Step::Work(0)); 12]);
    schedule.push((0, Step::Answer(1)));
    world.run(2, &schedule).unwrap();
    world.phase(3, &[]).unwrap();
}

#[test]
fn a_restore_answer_is_not_settled_by_a_delete_the_server_lost_from_a_device_that_left() {
    let mut world = on(&[Precision::Fine; 2]);
    world.phase(0, &[(0, file("a"))]).unwrap();
    world.phase(1, &[(0, Step::Backup)]).unwrap();
    let mut schedule = vec![
        (0, delete(0)),
        (0, Step::Scan),
        (0, Step::Rewind),
        (1, Step::Connect),
    ];
    schedule.extend([(1, Step::Work(0)); 12]);
    schedule.extend([(1, Step::Backup), (1, delete(0))]);
    schedule.extend([(1, Step::Work(0)); 12]);
    schedule.extend([(1, Step::Restore), (1, Step::Leave), (0, Step::Connect)]);
    schedule.extend([(0, Step::Work(0)); 12]);
    schedule.push((0, Step::Answer(1)));
    world.run(2, &schedule).unwrap();
    world.phase(3, &[]).unwrap();
}
