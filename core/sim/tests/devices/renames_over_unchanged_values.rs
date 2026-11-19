// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_rename_by_another_device_after_a_restore_and_an_edit_leaves_one_file_after_a_rewind() {
    use Step::*;
    converge(
        3,
        &[
            vec![(0, u(1, 0, 0, 0, 0)), (0, Backup)],
            vec![
                (0, Restore),
                (0, u(2, 0, 0, 0, 0)),
                (0, u(0, 0, 0, 1, 0)),
                (2, u(3, 0, 0, 1, 0)),
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
                (0, u(0, 0, 0, 0, 0)),
                (0, u(1, 0, 0, 1, 0)),
                (0, u(1, 0, 0, 2, 0)),
                (0, u(1, 0, 2226843182039624671, 6, 0)),
                (0, Backup),
            ],
            vec![
                (0, Backup),
                (0, u(2, 1039137327131671793, 0, 0, 0)),
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
                (0, u(1, 0, 0, 0, 0)),
                (0, Backup),
                (0, u(0, 0, 0, 1, 0)),
                (0, u(0, 0, 3646487834648185621, 0, 0)),
            ],
            vec![
                (0, Backup),
                (0, RestoreEarlier),
                (1, u(2, 4305928427121440511, 0, 0, 0)),
            ],
            vec![(0, Rewind)],
        ],
    )
    .unwrap();
}

#[test]
fn a_rename_after_an_edit_and_a_restore_to_an_earlier_backup_leaves_one_file_after_a_restore() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 2 is in 3 live nodes on the server: [[\"b\"], [\"b (conflict from device-1 2026-11-09)\"], [\"b (conflict from device-2 2026-11-09)\"]]",
    );
    use Step::*;
    converge(
        3,
        &[
            vec![(0, Backup), (0, u(1, 0, 0, 1, 0))],
            vec![
                (0, u(2, 0, 0, 0, 0)),
                (0, Backup),
                (0, u(3, 0, 0, 2, 0)),
                (0, RestoreEarlier),
            ],
            vec![(0, u(0, 0, 0, 0, 0)), (0, Restore)],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_rename_before_two_restores_to_earlier_backups_leaves_one_file() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 2 is in 2 live nodes on the server: [[\"a\"], [\"a (conflict from device-0 2026-11-09)\"]]",
    );
    use Step::*;
    converge(
        3,
        &[
            vec![(0, Backup), (0, u(1, 0, 0, 0, 0))],
            vec![
                (2, u(3, 0, 0, 1, 0)),
                (0, Backup),
                (0, RestoreEarlier),
                (0, u(2, 0, 0, 0, 0)),
                (0, Backup),
            ],
            vec![(0, RestoreEarlier)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_after_new_folders_and_a_restore_to_an_earlier_backup_leaves_one_file_after_a_restore() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 2 is in 2 live nodes on the server: [[\"a\"], [\"a (conflict from device-0 2026-11-09 2)\"]]",
    );
    use Step::*;
    converge(
        2,
        &[
            vec![
                (0, Backup),
                (0, u(1, 0, 0, 0, 0)),
                (0, Work(0)),
                (2, Work(0)),
                (2, Work(0)),
                (0, Backup),
            ],
            vec![
                (0, u(0, 0, 0, 1, 0)),
                (2, u(0, 0, 8937569764786422473, 0, 0)),
                (0, u(0, 0, 0, 2, 0)),
                (0, RestoreEarlier),
                (1, u(0, 0, 0, 4, 0)),
                (0, u(2, 16805682748251341024, 0, 0, 0)),
            ],
            vec![(0, u(0, 0, 0, 0, 0)), (0, Restore), (0, u(0, 0, 0, 0, 0))],
        ],
    )
    .unwrap();
}

#[test]
fn another_devices_rename_and_edit_leave_one_file_after_two_restores() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 3 is in 2 live nodes on the server: [[\"ß\", \"a\"], [\"ß\", \"a (conflict from device-1 2026-11-09)\"]]",
    );
    use Step::*;
    converge(
        2,
        &[
            vec![
                (2, u(1, 0, 0, 3, 0)),
                (2, Work(0)),
                (1, u(0, 0, 0, 0, 0)),
                (2, Work(0)),
                (2, u(0, 0, 0, 5, 0)),
                (0, Work(4665899113959692)),
                (0, Backup),
                (2, u(3, 1478117154531925494, 4887729075126604849, 0, 0)),
                (1, u(1, 0, 8785384080190603307, 0, 0)),
            ],
            vec![
                (2, u(2, 2498200291474586324, 0, 0, 0)),
                (0, Restore),
                (2, u(0, 0, 6378741448404807564, 3, 0)),
                (1, u(0, 0, 0, 0, 0)),
                (2, Work(0)),
                (2, Connect),
                (2, Deliver(0)),
                (2, Commit(Reply::Served)),
                (0, u(0, 0, 0, 0, 0)),
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
                (2, u(1, 0, 0, 2, 0)),
                (0, u(2, 0, 0, 0, 0)),
                (0, Work(0)),
                (0, Commit(Reply::Served)),
                (1, u(1, 0, 0, 0, 0)),
                (1, u(3, 0, 0, 7, 0)),
                (0, u(0, 0, 0, 0, 0)),
                (2, Work(49713346884689564)),
                (0, Backup),
            ],
            vec![
                (2, u(0, 0, 0, 1, 0)),
                (0, u(1, 0, 0, 3, 3)),
                (2, u(2, 292261509554156280, 0, 0, 3)),
                (0, Work(0)),
                (0, Restore),
                (0, Connect),
                (0, u(4, 86702644334452595, 0, 0, 0)),
                (2, Work(5782721906212901568)),
                (2, Work(1510384945067285601)),
                (0, Work(369469179041198605)),
            ],
            vec![(0, Restore)],
        ],
    )
    .unwrap();
}
