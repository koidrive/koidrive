// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn the_author_sends_its_edit_again_when_a_second_restore_comes_before_it_lists_the_tree() {
    for devices in [1, 2] {
        converge(
            devices,
            &[
                vec![(0, file("a")), (0, BACKUP)],
                vec![(0, overwrite(0)), (0, BACKUP)],
                vec![(0, Step::RestoreEarlier), (0, Step::Connect), (0, RESTORE)],
            ],
        )
        .unwrap();
    }
}
