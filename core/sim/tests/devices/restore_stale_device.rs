// SPDX-License-Identifier: Apache-2.0

use super::*;

fn act(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text,
    })
}

#[test]
fn a_device_recovers_when_a_restore_brings_back_its_commit_at_a_slot_it_filled_again() {
    let refilled_at_once = vec![
        vec![
            (0, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, Step::Backup),
        ],
        vec![
            (0, Step::Backup),
            (0, Step::RestoreEarlier),
            (0, Step::Backup),
            (0, act(0, 0, 0, 1, 0)),
            (2, Step::Connect),
            (0, Step::Work(0)),
            (0, act(0, 0, 0, 0, 0)),
            (2, Step::Work(2641328760825688377)),
            (2, Step::Work(2686750302578877049)),
            (0, Step::RestoreEarlier),
        ],
    ];
    let refilled_after_a_backup = vec![
        vec![
            (2, act(1, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, Step::Backup),
            (0, act(0, 0, 0, 0, 0)),
        ],
        vec![
            (0, Step::Backup),
            (0, Step::RestoreEarlier),
            (2, Step::Connect),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (2, Step::Work(6708768365833688321)),
            (0, Step::Work(3658981457186381541)),
            (0, Step::RestoreEarlier),
        ],
    ];
    let refilled_on_both_devices = vec![
        vec![
            (1, act(1, 0, 0, 0, 0)),
            (0, Step::Backup),
            (1, act(0, 0, 0, 1, 0)),
            (1, act(4, 2530034480979724333, 0, 0, 0)),
            (1, Step::Work(0)),
            (1, act(3, 0, 0, 1, 0)),
            (2, act(0, 0, 0, 0, 0)),
            (1, Step::Work(739179444374148440)),
        ],
        vec![
            (2, act(0, 0, 0, 1, 0)),
            (2, Step::Work(0)),
            (2, Step::Commit(Reply::Served)),
            (2, Step::Commit(Reply::Served)),
            (0, Step::Backup),
            (2, act(2, 0, 0, 0, 0)),
            (2, act(0, 0, 0, 0, 0)),
            (0, act(1, 0, 0, 0, 0)),
            (1, act(0, 0, 4489521831829375340, 1, 0)),
            (1, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, Step::RestoreEarlier),
            (0, act(1, 0, 0, 2, 2)),
        ],
        vec![
            (1, act(4, 0, 0, 0, 0)),
            (1, Step::Work(0)),
            (1, Step::Commit(Reply::NoChunks)),
            (0, act(0, 0, 0, 0, 0)),
            (2, act(0, 0, 0, 0, 0)),
            (0, act(0, 0, 0, 0, 0)),
            (0, Step::Restore),
        ],
    ];
    for phases in [
        refilled_at_once,
        refilled_after_a_backup,
        refilled_on_both_devices,
    ] {
        keep(2, &phases).unwrap();
        converge(2, &phases).unwrap();
    }
}
