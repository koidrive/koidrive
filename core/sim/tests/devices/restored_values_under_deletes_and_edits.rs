// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
#[ignore = "a delete of a resent edit comes undone when a restore brings back the value it was made over (GUN-1013)"]
fn a_delete_of_a_resent_edit_stays_after_a_restore_brings_back_the_value_it_was_made_over() {
    use koidrive_sim::world::Step::*;
    let u = |kind, target, name| {
        User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0, 0)), (0, Backup)],
        vec![
            (0, u(0, 0, 1)),
            (0, u(0, 0, 2)),
            (0, Backup),
            (0, u(2, 6307646474509524231, 0)),
            (0, RestoreEarlier),
        ],
        vec![
            (0, u(4, 3700173026859558585, 0)),
            (0, RestoreEarlier),
            (0, Restore),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_delete_of_a_file_whose_edit_a_restore_lost_stays_on_a_device_that_never_took_the_edit() {
    use koidrive_sim::world::Step::*;
    let u = |kind, target, name| {
        User(Action {
            kind,
            target,
            parent: 0,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![
            (0, u(0, 0, 0)),
            (0, u(1, 0, 1)),
            (0, u(1, 0, 2)),
            (0, u(4, 0, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Backup),
            (1, u(0, 0, 0)),
            (0, u(2, 43635376045345639, 0)),
            (0, u(0, 0, 3)),
        ],
        vec![
            (0, u(2, 2372553838036446217, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (0, Restore),
            (0, u(4, 16828409463697550597, 0)),
        ],
    ];
    converge(2, &phases).unwrap();
}

fn renamed_by_another_device_before_a_restore(change: Step) -> World {
    let phases = vec![
        vec![(0, file("a")), (0, BACKUP)],
        vec![(0, BACKUP), (0, overwrite(0)), (0, Step::RestoreEarlier)],
        vec![(1, rename(0, 0, "c.txt"))],
        vec![(0, change), (0, Step::RestoreEarlier), (0, RESTORE)],
    ];
    converge(2, &phases).unwrap()
}

#[test]
#[ignore = "a delete of a resent file comes undone after a restore when another device renamed it (GUN-1013)"]
fn a_delete_of_a_resent_file_stays_after_a_restore_when_another_device_renamed_it() {
    let world = renamed_by_another_device_before_a_restore(delete(0));
    assert!(no_device_holds_a_file(&world), "{:?}", paths_on(&world, 0));
}

#[test]
#[ignore = "an edit of a resent file leaves a conflict copy after a restore when another device renamed it (GUN-1013)"]
fn an_edit_of_a_resent_file_leaves_one_file_after_a_restore_when_another_device_renamed_it() {
    let world = renamed_by_another_device_before_a_restore(overwrite(0));
    for device in 0..2 {
        assert_eq!(
            files_on(&world, device).len(),
            1,
            "device {device}: {:?}",
            paths_on(&world, device)
        );
    }
}

#[test]
fn a_write_stays_on_its_disk_when_another_device_edits_and_deletes_it_before_a_restore() {
    use Step::*;
    converge(
        2,
        &[
            vec![(0, u(1, 0, 0, 0, 2))],
            vec![
                (0, Backup),
                (0, u(0, 0, 0, 6, 0)),
                (2, u(0, 0, 0, 2, 0)),
                (1, u(2, 0, 0, 0, 0)),
                (0, Work(0)),
                (2, u(2, 180933619215633337, 0, 0, 0)),
                (0, Work(0)),
                (0, Work(0)),
                (2, u(4, 27504344672840725, 0, 0, 0)),
                (0, Work(0)),
            ],
            vec![
                (0, Restore),
                (0, u(2, 6533790443927934083, 0, 0, 0)),
                (2, u(0, 0, 0, 1, 0)),
                (0, Connect),
                (0, Work(1933992437937104005)),
                (2, u(0, 0, 0, 3, 0)),
                (0, u(2, 8580201904733844845, 0, 0, 2)),
                (2, Work(2255869094274784908)),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a delete of a file whose edit a restore lost comes undone across crashes and a dropped answer (GUN-1013)"]
fn a_delete_of_a_file_whose_edit_a_restore_lost_stays_across_crashes_and_a_dropped_answer() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (0, u(0, 0, 0, 0, 0)),
                (2, u(1, 0, 0, 1, 0)),
                (2, u(1, 0, 0, 2, 0)),
                (2, u(4, 0, 0, 0, 0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, Backup),
                (1, u(0, 0, 0, 0, 0)),
                (0, u(2, 43635376045345639, 801052, 2, 2)),
                (1, u(1, 14315092324237007413, 11893526088766759936, 6, 2)),
            ],
            vec![
                (2, u(2, 2372560661617564589, 16794891138620891841, 0, 3)),
                (0, Connect),
                (2, Work(17588204844164713536)),
                (2, Work(14106446112149645025)),
                (2, Work(16848271966855046244)),
                (0, Commit(Reply::NoChunks)),
                (1, Effect(Outcome::Crash(Cut::Done))),
                (0, Commit(Reply::Served)),
                (0, Commit(Reply::Served)),
                (0, Work(12630302266806770175)),
                (1, Commit(Reply::Served)),
                (0, Drop),
                (1, Deliver(9393120553199037839)),
                (0, u(4, 4101864591004803189, 17267819489377474840, 1, 2)),
                (0, u(4, 2896573261742558571, 7636275295828537346, 0, 0)),
                (1, Work(10387773818721929339)),
                (0, Restore),
                (1, Crash),
                (0, u(4, 16899321840197717245, 9965728718656330087, 4, 3)),
                (2, Connect),
                (0, u(1, 15623518591987863817, 16053291650037472945, 6, 3)),
                (0, Duplicate(5655773862559933977)),
                (0, Effect(Outcome::Crash(Cut::Done))),
                (0, u(3, 7805667264558650924, 5898078227447655741, 5, 0)),
            ],
        ],
    )
    .unwrap();
}
