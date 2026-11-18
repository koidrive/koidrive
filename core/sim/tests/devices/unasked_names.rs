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
fn a_device_that_missed_the_delete_of_a_folder_owing_its_name_brings_it_back_by_its_users_edit_and_finishes_the_name()
 {
    let _red = expect_red(
        Check::OwnerFinish,
        "a commit of device 2 finishes the pending name \"A\" of node 01a1c2c0-39a0-7cdc-8389-c4663807ce68, which the user of device 2 never gave it",
    );
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
            (0, Commit(Reply::Refused(error::Code::RetryLater))),
            (1, u(4, 1, 0, 0, 0)),
            (1, Scan),
            (1, Commit(Reply::Served)),
            (1, Commit(Reply::Served)),
            (1, Deliver(0)),
            (0, u(1, 0, 1, 2, 0)),
            (0, Scan),
            (0, Commit(Reply::Served)),
            (0, Commit(Reply::Served)),
            (0, Deliver(0)),
            (1, u(4, 1, 0, 0, 0)),
            (0, Connect),
            (2, u(4, 0, 0, 0, 0)),
            (2, Work(865446196818534493)),
            (2, Work(7651933159810410712)),
            (2, Effect(Outcome::Run)),
            (2, Effect(Outcome::Run)),
            (2, u(0, 0, 384094451388906499, 2, 0)),
            (2, Work(1805074336413393753)),
            (2, Work(9854330730590648617)),
            (2, Work(7193782762414192021)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(
            paths.contains(&"A".to_owned()),
            "device {device}: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("(pending)")),
            "device {device}: {paths:?}"
        );
    }
}

#[test]
fn a_name_the_server_gave_a_create_stays_resendable_while_a_restore_leaves_the_server_refusing() {
    use Step::*;
    let phases = vec![
        vec![
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (2, u(0, 0, 0, 2, 0)),
            (0, Work(0)),
            (1, u(2, 0, 0, 0, 0)),
            (0, u(0, 0, 11927410706735195804, 0, 0)),
            (0, Work(0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (2, Work(0)),
            (2, Work(0)),
            (0, Backup),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, CopyStore),
            (0, Work(0)),
            (2, Work(1087398682890972493)),
            (0, u(0, 0, 2327697017353793335, 0, 0)),
            (0, Backup),
            (2, u(3, 0, 6665902632929577350, 1, 0)),
            (0, Work(0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, Work(0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, Rewind),
            (0, u(0, 0, 0, 0, 0)),
            (0, Rewind),
            (0, u(0, 0, 0, 0, 0)),
            (0, Rewind),
            (0, u(0, 0, 0, 0, 0)),
            (2, u(0, 0, 0, 0, 0)),
            (1, u(1, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (0, u(0, 0, 8098649286303538872, 3, 0)),
            (0, Backup),
        ],
        vec![(2, u(0, 0, 0, 1, 0)), (0, RestoreEarlier)],
        vec![(0, Restore)],
    ];
    let world = converge(3, &phases).unwrap();
    for device in 0..3 {
        let paths = paths_on(&world, device);
        assert!(
            paths
                .iter()
                .any(|path| path.starts_with("A (conflict from device-2")),
            "device {device}: {paths:?}"
        );
    }
}
