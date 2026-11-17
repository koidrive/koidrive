// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_rename_by_another_device_after_a_restore_and_an_edit_leaves_one_file_after_a_rewind() {
    use Step::*;
    converge(
        3,
        &[
            vec![
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Backup),
            ],
            vec![
                (0, Restore),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
            ],
            vec![(0, Rewind)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_before_a_restore_to_an_earlier_backup_leaves_one_file_after_a_rewind() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 2226843182039624671,
                        name: 6,
                        text: 0,
                    }),
                ),
                (0, Backup),
            ],
            vec![
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 1039137327131671793,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, RestoreEarlier),
            ],
            vec![(0, Rewind)],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_edit_after_a_restore_to_an_earlier_backup_leaves_one_file_after_a_rewind() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 3646487834648185621,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (0, Backup),
                (0, RestoreEarlier),
                (
                    2,
                    User(Action {
                        kind: 2,
                        target: 4305928427121440511,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![(0, Rewind)],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_after_an_edit_and_a_restore_to_an_earlier_backup_leaves_one_file_after_a_restore() {
    use Step::*;
    converge(
        3,
        &[
            vec![
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (0, RestoreEarlier),
            ],
            vec![
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Restore),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_rename_before_two_restores_to_earlier_backups_leaves_one_file() {
    use Step::*;
    converge(
        3,
        &[
            vec![
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    2,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (0, Backup),
                (0, RestoreEarlier),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Backup),
            ],
            vec![(0, RestoreEarlier)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_after_new_folders_and_a_restore_to_an_earlier_backup_leaves_one_file_after_a_restore() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (0, Backup),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Work(0)),
                (2, Work(0)),
                (2, Work(0)),
                (0, Backup),
            ],
            vec![
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 8937569764786422473,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (0, RestoreEarlier),
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 4,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 16805682748251341024,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Restore),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_rename_and_edit_leave_one_file_after_two_restores() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (
                    2,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 3,
                        text: 0,
                    }),
                ),
                (2, Work(0)),
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(0)),
                (
                    2,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 5,
                        text: 0,
                    }),
                ),
                (0, Work(4665899113959692)),
                (0, Backup),
                (
                    2,
                    User(Action {
                        kind: 3,
                        target: 1478117154531925494,
                        parent: 4887729075126604849,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 8785384080190603307,
                        name: 0,
                        text: 0,
                    }),
                ),
            ],
            vec![
                (
                    2,
                    User(Action {
                        kind: 2,
                        target: 2498200291474586324,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Restore),
                (
                    2,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 6378741448404807564,
                        name: 3,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(0)),
                (2, Connect),
                (2, Deliver(0)),
                (2, Commit(Reply::Served)),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(1461741780366827873)),
            ],
            vec![(0, Restore)],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a delete made between two restores comes undone after another device's rename and edit (GUN-831)"]
fn a_rename_and_another_devices_edit_leave_one_file_after_two_restores() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (
                    2,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 2,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 2,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, Work(0)),
                (0, Commit(Reply::Served)),
                (
                    1,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (
                    1,
                    User(Action {
                        kind: 3,
                        target: 0,
                        parent: 0,
                        name: 7,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(49713346884689564)),
                (0, Backup),
            ],
            vec![
                (
                    2,
                    User(Action {
                        kind: 0,
                        target: 0,
                        parent: 0,
                        name: 1,
                        text: 0,
                    }),
                ),
                (
                    0,
                    User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 3,
                        text: 3,
                    }),
                ),
                (
                    2,
                    User(Action {
                        kind: 2,
                        target: 292261509554156280,
                        parent: 0,
                        name: 0,
                        text: 3,
                    }),
                ),
                (0, Work(0)),
                (0, Restore),
                (0, Connect),
                (
                    0,
                    User(Action {
                        kind: 4,
                        target: 86702644334452595,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                ),
                (2, Work(5782721906212901568)),
                (2, Work(1510384945067285601)),
                (0, Work(369469179041198605)),
            ],
            vec![(0, Restore)],
        ],
    )
    .unwrap();
}
