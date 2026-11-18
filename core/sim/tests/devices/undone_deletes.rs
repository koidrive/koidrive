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
        .map(|(name, content)| (name, content.map(|c| c.mtime_ns)))
        .collect();
    assert_eq!(held, [("a".to_owned(), Some(1))]);
}

#[test]
fn an_edit_another_device_made_over_the_deleted_value_brings_the_file_back() {
    let world = checked(
        2,
        &[
            vec![(0, file("a"))],
            vec![(1, overwrite(0)), (0, delete(0))],
        ],
    )
    .unwrap();
    for device in 0..2 {
        let held: Vec<_> = (files_on(&world, device).into_iter())
            .map(|(name, content)| (name, content.map(|c| c.mtime_ns)))
            .collect();
        assert_eq!(held, [("a".to_owned(), Some(2))], "device {device}");
    }
}

#[test]
fn a_rename_another_device_made_after_the_delete_brings_the_file_back() {
    let world = checked(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, delete(0)), (1, rename(0, 0, "b"))],
        ],
    )
    .unwrap();
    for device in 0..2 {
        let held: Vec<_> = (files_on(&world, device).into_iter())
            .map(|(name, content)| (name, content.map(|c| c.mtime_ns)))
            .collect();
        assert_eq!(held, [("b".to_owned(), Some(1))], "device {device}");
    }
}

#[test]
fn a_write_its_user_replaces_before_it_reaches_the_server_needs_no_file() {
    let world = converge(
        2,
        &[
            vec![(0, BACKUP), (1, file("a"))],
            vec![(0, overwrite(0)), (0, BACKUP), (0, Step::RestoreEarlier)],
            vec![(1, overwrite(0)), (1, overwrite(0)), (0, RESTORE)],
        ],
    )
    .unwrap();
    for device in 0..2 {
        assert!(
            (files_on(&world, device).into_iter())
                .all(|(_, content)| content.is_none_or(|content| content.mtime_ns != 3)),
            "device {device}"
        );
    }
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
#[ignore = "a file deleted before its folder comes back when another device moves the folder first (GUN-1054)"]
fn deletes_reach_the_server_in_the_saved_gun_1054_cases_around_a_folder_another_device_replaces() {
    deletes_reach_the_server_in(
        concat!(
            module_path!(),
            "::deletes_reach_the_server_in_the_saved_gun_1054_cases_around_a_folder_another_device_replaces"
        ),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/gun-1054-folder-another-device-replaces.proptest-regressions"
        ),
        replaced_folder(),
    );
}

#[test]
#[ignore = "a file deleted before its folder comes back when another device moves the folder first (GUN-1054)"]
fn a_file_deleted_before_its_folder_stays_deleted_when_another_device_moves_the_folder_first() {
    let mut moved = vec![
        (0, delete(1)),
        (0, delete(0)),
        (1, folder("C.TXT")),
        (1, rename(0, 2, "A")),
    ];
    moved.extend(commit(1));
    moved.extend(commit(0));
    checked(2, &[vec![(0, folder("b")), (0, user(1, 0, 1, "a"))], moved]).unwrap();
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
    checked(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, act(3, 0, 0, "b", 2)), (0, act(0, 0, 0, "a", 1))],
            vec![(1, act(4, 0, 0, "ß", 1)), (0, act(1, 0, 0, "ss", 3))],
            vec![
                (0, rename(1, 0, "A")),
                (1, rename(1, 1, "\u{e9}")),
                (1, act(4, 0, 0, "ss", 0)),
            ],
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
                (0, act(1, 0, 0, "\u{e9}", 1)),
                (0, act(4, 1, 0, "b", 0)),
                (1, act(4, 0, 0, "a", 0)),
                (1, folder("C.TXT")),
                (1, act(1, 0, 1, "\u{c9}", 0)),
                (0, act(0, 0, 0, "E\u{301}", 2)),
                (0, act(3, 0, 2, "C.TXT", 0)),
                (1, act(1, 0, 1, "ß", 2)),
                (0, act(1, 0, 1, "C.TXT", 0)),
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
fn a_deleted_original_leaves_its_conflict_copy_with_the_same_write() {
    checked(
        3,
        &[
            vec![
                (1, Step::CopyStore),
                (0, BACKUP),
                (2, Step::CopyStore),
                (0, file("a")),
                (1, file("a")),
            ],
            vec![(2, Step::RollBack), (0, Step::Rewind)],
            vec![(2, delete(2))],
        ],
    )
    .unwrap();
}

#[test]
fn a_deleted_conflict_copy_leaves_the_original_with_the_same_write() {
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
    let mut renamed = vec![(1, rename(0, 0, "ss"))];
    renamed.extend(commit(1));
    renamed.push((0, delete(1)));
    renamed.extend(commit(0));
    checked(
        2,
        &[vec![(0, folder("a")), (0, user(1, 0, 1, "b"))], renamed],
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
                (0, rename(0, 0, "ss")),
                (0, SCAN),
                (0, COMMIT),
                (1, delete(0)),
                (1, folder("c.txt")),
                (1, user(1, 0, 1, "a")),
                (0, act(0, 0, 0, "\u{e9}", 3)),
                (1, act(1, 0, 0, "\u{c9}", 3)),
                (1, file("\u{e9}")),
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
fn an_edit_of_the_file_that_another_device_never_sends_leaves_the_delete_checked() {
    let mut renamed = vec![(1, rename(0, 0, "ss"))];
    renamed.extend(commit(1));
    renamed.push((0, delete(1)));
    renamed.extend(commit(0));
    renamed.extend([(1, overwrite(1)), (1, Step::Leave)]);
    checked(
        2,
        &[vec![(0, folder("a")), (0, user(1, 0, 1, "b"))], renamed],
    )
    .unwrap();
}

#[test]
fn a_file_another_device_adds_to_the_deleted_folder_leaves_its_siblings_checked() {
    let mut deleted = vec![(1, user(1, 0, 1, "c.txt")), (0, delete(0))];
    deleted.extend(commit(1));
    deleted.extend(commit(0));
    checked(
        2,
        &[vec![(0, folder("b")), (0, user(1, 0, 1, "a"))], deleted],
    )
    .unwrap();
}

#[test]
fn an_edit_another_device_makes_to_a_sibling_leaves_the_deleted_file_checked() {
    let mut deleted = vec![(1, overwrite(2)), (0, delete(0))];
    deleted.extend(commit(1));
    deleted.extend(commit(0));
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
fn a_rename_another_device_makes_to_a_folder_between_leaves_the_deleted_file_checked() {
    let mut deleted = vec![(1, rename(1, 1, "A")), (0, delete(0))];
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
        ],
    )
    .unwrap();
}

#[test]
fn a_file_another_device_adds_after_the_delete_leaves_the_deleted_file_checked() {
    let deleted = vec![
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
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_of_a_folder_the_server_kept_leaves_the_deleted_file_checked() {
    let deleted = vec![
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
        ],
    )
    .unwrap();
}
