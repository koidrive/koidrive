// SPDX-License-Identifier: Apache-2.0

use super::*;

fn u(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

#[test]
#[ignore = "a rename of a file its own edit revived never reaches the server when another device took the name (PlaceKept)"]
fn a_rename_of_a_file_its_users_edit_revived_reaches_the_server_when_another_device_took_the_name()
{
    use Step::*;
    let phases = vec![
        vec![(0, u(1, 0, 0, 7))],
        vec![
            (0, u(4, 0, 0, 0)),
            (0, Work(0)),
            (2, Work(0)),
            (1, u(2, 0, 0, 0)),
            (2, Commit(Reply::Served)),
            (2, u(0, 0, 0, 7)),
            (1, Work(2507844928219525457)),
            (1, Deliver(0)),
            (2, Work(0)),
            (0, u(0, 0, 0, 0)),
            (1, u(3, 0, 0, 0)),
            (1, Work(3159997680851536184)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
#[ignore = "a rename made after a store rollback is dropped by the recovery without an answer (PlaceKept)"]
fn a_rename_made_after_a_store_rollback_survives_the_recovery() {
    use Step::*;
    let phases = vec![
        vec![(2, CopyStore), (0, u(1, 0, 0, 7))],
        vec![
            (0, u(3, 0, 0, 0)),
            (2, Work(0)),
            (0, Commit(Reply::NoChunks)),
            (0, RollBack),
            (2, u(3, 0, 0, 7)),
            (0, u(0, 0, 0, 0)),
            (0, u(0, 0, 0, 0)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_move_a_recovery_answer_gave_up_that_the_server_still_holds_settles_the_create_it_moved() {
    use Step::*;
    let phases = vec![
        vec![
            (2, u(1, 0, 0, 0)),
            (1, u(0, 0, 0, 0)),
            (0, u(1, 0, 0, 0)),
            (0, u(3, 0, 0, 6)),
        ],
        vec![
            (1, u(3, 15267162127865811314, 11911686193232281106, 5)),
            (1, CopyStore),
            (0, u(0, 0, 0, 0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Commit(Reply::Served)),
            (1, u(0, 0, 0, 1)),
            (1, RollBack),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_move_a_recovery_answer_gave_up_after_a_store_rollback_that_the_server_still_holds_settles_the_create_it_moved()
 {
    use Step::*;
    let phases = vec![
        vec![
            (0, Rewind),
            (0, Work(0)),
            (0, u(1, 0, 0, 0)),
            (0, u(4, 0, 0, 0)),
            (0, u(1, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(0, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(0, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(0, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(0, 0, 0, 0)),
            (0, u(4, 0, 0, 0)),
            (1, u(1, 0, 0, 0)),
            (2, u(0, 0, 1070167780213394287, 9)),
            (0, u(1, 0, 0, 2)),
            (0, u(1, 0, 0, 9)),
            (1, CopyStore),
            (2, u(0, 0, 1070167780213394287, 9)),
        ],
        vec![
            (1, u(3, 14935240610524001261, 6898617175372474716, 7)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, RollBack),
            (1, u(0, 0, 3983559647531957158, 0)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
#[ignore = "a folder made after a rewind inside a folder its user deleted is gone after the recovery restores the parent (IntentKept)"]
fn a_folder_made_after_a_rewind_inside_a_deleted_folder_comes_back_with_the_restore_answer() {
    use Step::*;
    let phases = vec![
        vec![(0, u(0, 0, 0, 0))],
        vec![
            (2, u(0, 0, 3567707688385218979, 0)),
            (0, u(0, 0, 6566776624965678292, 1)),
            (0, Backup),
            (0, u(0, 0, 0, 1)),
            (0, Rewind),
            (2, u(0, 0, 2117512966000889671, 2)),
            (0, Work(0)),
            (0, u(4, 7229048669508164125, 0, 0)),
            (0, u(0, 0, 0, 1)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_same_bytes_rewrite_of_a_restored_value_counts_as_held_when_another_devices_edit_replaces_it() {
    use Step::*;
    let w = |kind: u8, target: usize, name: usize, text: u8| {
        User(Action {
            kind,
            target,
            parent: 0,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(0, w(1, 0, 0, 2)), (0, w(2, 0, 0, 2))],
        vec![
            (1, w(2, 0, 0, 0)),
            (0, Backup),
            (0, w(2, 0, 0, 0)),
            (1, Work(0)),
            (1, Work(0)),
            (0, Work(0)),
            (1, Commit(Reply::Served)),
            (2, Work(3508390921734383153)),
            (0, w(3, 0, 2, 0)),
            (2, Work(11250415762383832883)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Restore),
            (0, Work(0)),
            (0, Work(0)),
            (0, w(2, 2399411542774756895, 0, 3)),
        ],
    ];
    keep(2, &phases).unwrap();
    converge(2, &phases).unwrap();
}

#[test]
fn a_restore_answer_to_a_file_its_user_wrote_and_deleted_settles_the_write() {
    use Step::*;
    let phases = vec![
        vec![(0, u(0, 0, 0, 0)), (1, u(1, 0, 0, 0))],
        vec![
            (0, u(1, 0, 1056978756788217528, 1)),
            (0, u(0, 0, 791140932447120422, 2)),
            (0, u(3, 3252944353893610552, 1191256286020652927, 0)),
            (1, u(1, 0, 0, 1)),
        ],
        vec![
            (0, Backup),
            (0, u(4, 538865608399718080, 0, 0)),
            (0, u(2, 3101528685789730419, 0, 0)),
            (0, Rewind),
            (0, u(2, 5364182254041305409, 0, 0)),
            (0, u(2, 0, 0, 0)),
            (0, u(4, 1840357175477560216, 0, 0)),
        ],
    ];
    keep(2, &phases).unwrap();
}
