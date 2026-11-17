// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_bystander_sends_a_write_again_over_the_older_content_two_restores_in_a_row_leave() {
    let twice = [
        vec![(0, Step::RestoreEarlier), (0, RESTORE)],
        vec![
            (0, Step::RestoreEarlier),
            (0, BACKUP),
            (0, Step::RestoreEarlier),
        ],
    ];
    for restores in twice {
        converge(
            2,
            &[
                vec![(0, BACKUP), (0, file("c.txt"))],
                vec![(0, BACKUP), (1, overwrite(0))],
                restores,
            ],
        )
        .unwrap();
    }
}
