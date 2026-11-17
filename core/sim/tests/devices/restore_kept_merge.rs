// SPDX-License-Identifier: Apache-2.0

use super::*;

fn merged_then(change: Vec<(usize, Step)>) -> World {
    let mut p = vec![
        (0, folder("\u{c9}")),
        (0, folder("E\u{301}")),
        (0, user(1, 0, 2, "a")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
    ];
    p.extend([(0, DELIVER); 3]);
    p.push((0, Step::Backup));
    p.push((1, Step::Connect));
    p.extend([(1, DELIVER); 4]);
    p.extend([(1, EFFECT); 6]);
    p.extend(change);
    p.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
    p.extend([(1, DELIVER); 2]);
    p.extend([(0, DELIVER); 4]);
    p.push((1, Step::Backup));
    p.extend([(1, file("c.txt")), (1, SCAN), (1, COMMIT), (1, COMMIT)]);
    p.extend([(0, DELIVER); 4]);
    p.push((0, Step::Restore));
    p.push((0, Step::Connect));
    p.extend([(0, DELIVER); 6]);
    converge(2, &[p]).unwrap()
}

fn names(world: &World, device: usize) -> Vec<String> {
    files_on(world, device)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn a_folder_merged_into_one_another_device_deleted_stays_deleted_after_a_restore_keeps_the_merge() {
    let world = merged_then(vec![(1, delete(0))]);
    assert_eq!(names(&world, 0), ["c.txt"]);
    assert_eq!(names(&world, 1), ["c.txt"]);
}

#[test]
fn a_folder_merged_into_one_another_device_renamed_is_not_created_again_after_a_restore_keeps_the_merge()
 {
    let world = merged_then(vec![(1, rename(0, 0, "b"))]);
    assert_eq!(names(&world, 0), ["a", "b", "c.txt"]);
    assert_eq!(names(&world, 1), ["a", "b", "c.txt"]);
}
