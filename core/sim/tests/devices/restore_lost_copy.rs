// SPDX-License-Identifier: Apache-2.0

use super::*;

fn u(kind: u8, target: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent: 0,
        name,
        text,
    })
}

fn two_restores_of_one_file() -> [Vec<(usize, Step)>; 2] {
    [
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (2, u(2, 0, 0, 0)),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (0, u(2, 0, 0, 0)),
            (0, Step::RestoreEarlier),
        ],
    ]
}

#[test]
#[ignore = "a device keeps its lost copy of a write in a file of its own when a rewind brings the write back in the original (GUN-1012)"]
fn a_discarded_overwrite_across_a_rewind_keeps_the_rewound_write_in_one_file() {
    let [created, restored] = two_restores_of_one_file();
    let mut rewound = vec![
        (1, u(2, 4100883647648707004, 7, 0)),
        (0, Step::Rewind),
        (1, Step::Connect),
    ];
    rewound.extend(std::iter::repeat_n((1, Step::Work(0)), 6));
    rewound.extend([(1, Step::Answer(0)), (1, Step::Answer(1))]);
    converge(3, &[created, restored, rewound]).unwrap();
}

#[test]
#[ignore = "a device keeps its lost copy of a write in a file of its own when a rewind brings the write back in the original (GUN-1012)"]
fn a_same_bytes_rewrite_across_a_rewind_keeps_the_rewound_write_in_one_file() {
    let [created, restored] = two_restores_of_one_file();
    converge(
        3,
        &[
            created,
            restored,
            vec![(0, Step::Rewind), (2, u(2, 17200373593075710099, 1, 0))],
            vec![(2, u(2, 10175757954459202047, 1, 2))],
        ],
    )
    .unwrap();
}
