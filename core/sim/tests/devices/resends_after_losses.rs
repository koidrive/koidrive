// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_bystanders_resend_after_a_restore_to_an_earlier_backup_keeps_the_write() {
    let u = drawn_with;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, u(1, 9030220727657072367, 11378016110811731764, 0, 1)),
            (1, Step::Backup),
        ],
        vec![
            (2, Step::Backup),
            (1, u(2, 1195153182784767404, 16487922661405148831, 2, 0)),
        ],
        vec![(0, Step::RestoreEarlier), (2, Step::Restore)],
    ];
    converge(3, &phases).unwrap();
}

#[test]
#[ignore = "a rename made after a restore is missing from the server after the recovery copy (GUN-717)"]
fn a_recovery_copy_keeps_a_write_the_server_kept() {
    use koidrive_sim::world::{Reply, Step::*};
    let u = drawn_with;
    let mut first = vec![
        (0, Backup),
        (0, u(0, 0, 0, 2, 0)),
        (0, u(3, 0, 1, 0, 0)),
        (0, Scan),
        (0, Commit(Reply::Served)),
        (0, Commit(Reply::Served)),
        (0, Deliver(0)),
    ];
    first.extend([(1, Work(0)); 14]);
    first.extend([
        (0, Restore),
        (2, u(4, 0, 0, 0, 0)),
        (1, Connect),
        (0, u(3, 700175955643442024, 0, 3, 0)),
    ]);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 0))],
        first,
        vec![
            (0, Restore),
            (1, u(2, 1996617580910179362, 0, 0, 0)),
            (1, Connect),
            (0, u(2, 8035601735646909626, 0, 0, 0)),
            (1, Work(129976861001331385)),
            (1, Work(0)),
            (1, Work(15422585896085069985)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
#[ignore = "a rename and edit the holder had not taken leave a conflict copy beside the file after a second loss (GUN-716)"]
fn a_rename_and_edit_the_holder_had_not_taken_leave_one_file_after_a_second_loss() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.extend([(0, rename(0, 0, "b")), (0, overwrite(0))]);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (0, RESTORE)]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn a_second_rename_and_edit_before_the_holder_resends_leave_one_file() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (0, RESTORE)]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend([(0, rename(0, 0, "c.txt")), (0, overwrite(0))]);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend(commit(1));
    steps.extend([(1, DELIVER); 4]);
    steps.extend([(0, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["c.txt"], "device {device}");
    }
}

#[test]
#[ignore = "the holder's edit while the author's rename is untaken leaves a conflict copy beside the file (GUN-716)"]
fn an_edit_by_the_holder_while_the_authors_rename_is_untaken_leaves_one_file() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([
        (1, Step::Effect(Outcome::Fail)),
        (1, overwrite(0)),
        (0, RESTORE),
    ]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn an_observers_older_bytes_do_not_become_a_copy_beside_a_resent_edit() {
    let mut world = converge(
        2,
        &[
            vec![(0, folder("a"))],
            vec![
                (0, BACKUP),
                (0, user(0, 0, 1, "b")),
                (0, user(1, 0, 2, "c.txt")),
            ],
        ],
    )
    .unwrap();
    world
        .run(
            2,
            &[
                (0, overwrite(2)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
                (1, DELIVER),
            ],
        )
        .unwrap();
    world.phase(3, &[(0, SCAN), (0, RESTORE)]).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert!(
            !names.iter().any(|name| name.contains("conflict")),
            "device {device}: {names:?}"
        );
    }
}

#[test]
fn a_lost_move_and_edit_of_one_file_end_as_one_file() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 4]);
    steps.push((0, RESTORE));
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn a_lost_move_and_edit_the_other_device_never_saw_end_as_one_file() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.push((0, RESTORE));
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
#[ignore = "a stale resend from a device that never saw the author's rename drops the rename (GUN-673)"]
fn a_stale_resend_from_a_device_that_never_saw_the_authors_rename_keeps_the_authors_name() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((1, Step::Drop));
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.push((0, RESTORE));
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(1, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn a_write_a_rewind_took_stays_unsent_when_the_device_crashes_while_placing_it() {
    let act = |kind, target, parent, name| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    let phases = [
        vec![(2, act(0, 0, 0, 3)), (0, act(1, 0, 0, 5))],
        vec![
            (0, Step::Backup),
            (2, act(4, 9614619854247417842, 0, 0)),
            (0, act(3, 2564659630315289460, 11878576693270418400, 0)),
            (0, act(2, 1306322373503834592, 0, 0)),
        ],
        vec![
            (0, Step::Rewind),
            (0, Step::Connect),
            (0, Step::Work(0)),
            (0, act(1, 0, 0, 5)),
            (0, Step::Deliver(0)),
            (0, Step::Work(5595721357874797788)),
            (0, Step::Crash),
        ],
    ];
    converge(3, &phases).unwrap();
}
