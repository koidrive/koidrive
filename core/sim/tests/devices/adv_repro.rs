// SPDX-License-Identifier: Apache-2.0

use super::*;

fn act_raw(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

#[test]
fn adv_repro_647933() {
    converge(
        2,
        &[
            vec![(0, act_raw(1, 0, 0, 0))],
            vec![
                (0, Step::Backup),
                (1, act_raw(0, 0, 0, 1)),
                (1, act_raw(3, 0, 1, 0)),
                (1, Step::Scan),
                (1, Step::Commit(Reply::Served)),
                (1, Step::Commit(Reply::Served)),
                (1, Step::Deliver(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, act_raw(4, 1485386961051957829, 0, 0)),
                (0, Step::Restore),
                (2, act_raw(0, 0, 0, 2)),
                (0, act_raw(0, 0, 129279550227661037, 0)),
                (0, Step::Work(0)),
                (0, Step::Connect),
                (2, Step::Work(0)),
                (2, Step::Work(6879339188089465413)),
                (1, act_raw(3, 13522733509158688172, 12725651392473307273, 1)),
                (0, Step::Work(4926725346472754988)),
                (0, Step::Deliver(2218495212486824941)),
                (0, Step::Commit(Reply::Served)),
                (0, act_raw(0, 0, 0, 2)),
                (2, Step::Work(512022398505713393)),
            ],
        ],
    )
    .unwrap();
}
