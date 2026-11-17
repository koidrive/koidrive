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

fn deletes_reach_the_server_in(test: &'static str, file: &'static str, losses: Losses) {
    let mut runner = TestRunner::new(ProptestConfig {
        test_name: Some(test),
        ..koidrive_test_support::regression_config(file, 0)
    });
    let result = runner.run(&(2..=3usize, phases(losses)), |(devices, phases)| {
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
        Losses::RestoresAndRewinds,
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
        Losses::Restores,
    );
}
