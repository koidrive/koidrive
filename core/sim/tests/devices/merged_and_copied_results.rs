// SPDX-License-Identifier: Apache-2.0

use super::*;

fn u(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text,
    })
}

#[test]
fn a_folder_create_merged_into_a_folder_its_user_never_saw_and_then_deleted_lets_another_users_delete_of_that_folder_stand()
 {
    use Step::*;
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 0))],
        vec![
            (0, u(0, 0, 0, 1, 0)),
            (0, u(3, 0, 1, 0, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, Work(0)),
            (1, u(4, 1, 0, 0, 0)),
            (2, Work(0)),
            (2, u(0, 0, 0, 1, 0)),
            (0, Work(0)),
            (2, Work(4251637265606471842)),
            (2, Work(4571734596570069797)),
            (2, Work(10357135322553900038)),
            (2, u(0, 0, 1458184424143578841, 0, 0)),
            (2, u(4, 2074602684986994091, 0, 0, 0)),
            (2, Scan),
            (2, Work(4019436027751202215)),
            (2, Commit(Reply::Served)),
            (2, Deliver(0)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(paths.is_empty(), "device {device}: {paths:?}");
    }
}

#[test]
fn a_conflict_copy_of_a_file_a_restore_made_its_device_forget_keeps_the_name_the_server_gave_it() {
    use Step::*;
    let phases = vec![
        vec![(1, u(1, 0, 0, 1, 3))],
        vec![
            (2, u(3, 0, 0, 0, 0)),
            (0, Backup),
            (1, u(2, 0, 0, 0, 0)),
            (0, u(2, 0, 0, 0, 0)),
        ],
        vec![
            (0, Restore),
            (0, u(0, 0, 0, 0, 0)),
            (1, u(2, 3460126477107675283, 0, 0, 3)),
            (1, Work(0)),
            (1, Connect),
            (1, Deliver(0)),
            (1, Work(3091807929981066713)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(
            paths.contains(&"a".to_owned()),
            "device {device}: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.starts_with("a (conflict from device-1")),
            "device {device}: {paths:?}"
        );
    }
}

#[test]
fn a_rename_made_while_a_conflicting_edit_is_in_flight_keeps_the_users_name_on_the_conflict_copy() {
    use Step::*;
    let phases = vec![
        vec![(1, u(1, 0, 0, 1, 3))],
        vec![
            (2, u(3, 0, 0, 0, 0)),
            (0, Backup),
            (1, u(2, 0, 0, 0, 0)),
            (0, u(2, 0, 0, 0, 0)),
        ],
        vec![
            (0, Restore),
            (0, u(0, 0, 0, 0, 0)),
            (1, u(2, 3460126477107675283, 0, 0, 3)),
            (1, Work(0)),
            (1, Connect),
            (1, Deliver(0)),
            (1, Work(3091807929981066713)),
            (1, u(3, 1, 0, 2, 0)),
            (1, Scan),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(
            paths.contains(&"b".to_owned()),
            "device {device}: {paths:?}"
        );
        let (original, edited) = (3, 4);
        assert_eq!(
            (
                tag(&content_on(&world, device, "a")),
                tag(&content_on(&world, device, "b"))
            ),
            (original, edited),
            "device {device}: {paths:?}"
        );
    }
}

#[test]
fn a_conflict_copy_keeps_its_place_when_another_device_renamed_the_original_before_a_restore() {
    use Step::*;
    let phases = vec![
        vec![(1, u(1, 0, 0, 1, 3))],
        vec![
            (2, u(3, 0, 0, 0, 0)),
            (0, Backup),
            (1, u(2, 0, 0, 0, 0)),
            (0, u(2, 0, 0, 0, 0)),
        ],
        vec![
            (0, u(3, 0, 0, 2, 1)),
            (0, Restore),
            (0, u(0, 0, 0, 0, 0)),
            (1, u(2, 3460126477107675283, 0, 0, 3)),
            (1, Work(0)),
            (1, Connect),
            (1, Deliver(0)),
            (1, Work(3091807929981066713)),
        ],
    ];
    converge(3, &phases).unwrap();
}
