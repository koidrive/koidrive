// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_file_its_rules_exclude_stays_on_its_device_while_the_other_files_sync() {
    let exclude = [vec![], vec!["c.txt"]];
    let world = converge_excluding(
        &exclude,
        &[
            vec![(0, file("a")), (1, file("c.txt"))],
            vec![(0, overwrite(0)), (0, file("b")), (1, overwrite(0))],
        ],
    )
    .unwrap();
    assert_eq!(live_node_at(&world, &["c.txt"]), None);
    let names = |device| -> Vec<String> {
        (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect()
    };
    assert_eq!(names(0), ["a", "b"]);
    assert_eq!(names(1), ["a", "b", "c.txt"]);
    assert_eq!(content_on(&world, 0, "a"), content_on(&world, 1, "a"));
}

fn kept_by_its_rules() -> (World, Vec<Vec<(usize, Step)>>) {
    let phases = vec![
        vec![(0, folder("a")), (0, user(1, 0, 1, "b"))],
        vec![(1, user(1, 0, 1, "c.txt"))],
        vec![(0, delete(0))],
    ];
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    (world, phases)
}

#[test]
fn a_folder_another_device_deletes_stays_on_the_device_whose_rules_exclude_a_file_in_it() {
    let (world, _) = kept_by_its_rules();
    assert_eq!(live_node_at(&world, &["a"]), None);
    assert!(files_on(&world, 0).is_empty());
    ino_at(&world, 1, &["a", "c.txt"]);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
    assert!(
        world
            .seen
            .contains_key("a folder to remove that holds only excluded entries stays")
    );
}

#[test]
fn a_file_made_at_the_name_of_a_folder_kept_for_excluded_entries_downloads_under_a_conflict_name() {
    let (_, mut phases) = kept_by_its_rules();
    phases.push(vec![(0, file("a"))]);
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    let names: Vec<String> = (files_on(&world, 0).into_iter())
        .map(|(name, _)| name)
        .collect();
    let [copy] = names.as_slice() else {
        panic!("{names:?}");
    };
    assert!(copy.starts_with("a (conflict from "), "{copy}");
    assert!(live_node_at(&world, &[copy]).is_some());
    assert_eq!(content_on(&world, 1, copy), content_on(&world, 0, copy));
    ino_at(&world, 1, &["a", "c.txt"]);
}

#[test]
fn a_folder_kept_for_excluded_entries_goes_once_they_go_while_its_device_is_stopped() {
    let (_, mut phases) = kept_by_its_rules();
    phases.push(vec![(1, Step::Crash), (1, delete(1))]);
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    assert!(world.devices[1].disk.entries().is_empty());
    assert!(files_on(&world, 0).is_empty());
}

#[test]
fn a_file_made_at_the_name_of_a_folder_its_rules_exclude_waits_for_that_name() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 1 is quiet, but the effects for [\"01a1c2c0-39a0-7510-83bc-38d6c6b89fe4\"] wait for names that entries its rules leave out hold",
    );
    let phases = [vec![(1, folder("ss"))], vec![(0, file("ss"))]];
    converge_excluding(&[vec![], vec!["ss/"]], &phases).unwrap();
}

#[test]
fn a_folder_made_in_a_folder_another_device_renamed_to_a_name_its_rules_exclude_is_sent() {
    let _red = expect_red(
        Check::SentLeftOut,
        "a commit of device 1 places node 01a1c2c0-39a0-796a-8df8-fb6da7904e2f at [\"c.txt\", \"b\"], which the rules of device 1 leave out",
    );
    let phases = [
        vec![(0, folder("a"))],
        vec![(0, rename(0, 0, "c.txt"))],
        vec![(1, user(0, 0, 1, "b"))],
    ];
    converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
}

#[test]
fn a_file_at_a_name_another_spelling_of_which_its_rules_came_to_exclude_waits_for_that_name() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 1 is quiet, but nodes [\"01a1c2c0-39a0-7cb2-989b-810659c5a8bc\"] wait for names that entries its rules leave out hold",
    );
    let phases = [
        vec![
            (2, file("A")),
            (0, folder("a")),
            (1, folder("a")),
            (0, user(0, 0, 3983142787068221396, "b")),
            (0, rename(6113237590744703406, 5114041128735224957, "c.txt")),
        ],
        vec![
            (0, rename(6162043229936789262, 3027433216408009345, "b")),
            (1, act(3, 13474642980939706019, 2727123195511875546, "A", 3)),
        ],
    ];
    converge_excluding(&[vec![], vec!["*.txt", "\u{e9}"], vec![]], &phases).unwrap();
}

#[test]
fn a_folder_the_server_refuses_waits_to_be_removed_around_the_files_its_rules_exclude() {
    let _red = expect_red(
        Check::RemoveLeftOut,
        "device 0 is quiet, but it waits to remove the folders of [\"01a1c2c0-39a0-7cdc-8389-c4663807ce68\"], which hold entries its rules leave out",
    );
    let phases = [
        vec![(0, folder("a"))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(1, 0, "A")),
        ],
    ];
    converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
}

#[test]
fn a_folder_another_device_deletes_stays_parked_around_a_file_its_rules_came_to_exclude() {
    let _red = expect_red(
        Check::RemoveLeftOut,
        "device 1 is quiet, but it keeps the folders of [\"01a1c2c0-39a0-7510-83bc-38d6c6b89fe4\"] parked, which hold entries its rules leave out",
    );
    let _waits = expect_red(
        Check::NameLeftOut,
        "device 1 is quiet, but nodes [\"01a1c2c0-39a0-7c94-ad9a-de661683ef13\"] wait for names that entries its rules leave out hold",
    );
    let phases = [
        vec![
            (0, folder("a")),
            (0, user(1, 0, 5117382378431579780, "\u{df}")),
            (2, folder("a")),
            (0, overwrite(0)),
        ],
        vec![
            (2, delete(5752264358636203358)),
            (
                2,
                act(0, 397382682828019, 13755243886006184495, "\u{e9}", 3),
            ),
            (
                2,
                act(3, 10926111558922868753, 14156498429036113526, "A", 2),
            ),
            (
                1,
                act(3, 2311634670425845215, 5908466150070952589, "c.txt", 2),
            ),
        ],
    ];
    converge_excluding(&[vec![], vec!["*.txt"], vec![]], &phases).unwrap();
}

#[test]
fn a_folder_another_device_deletes_waits_around_a_new_file_moved_to_where_its_rules_exclude() {
    let _red = expect_red(
        Check::RemoveLeftOut,
        "device 1 is quiet, but it waits to settle the folders of [\"01a1c2c0-39a0-7510-83bc-38d6c6b89fe4\"], which hold only nodes its rules leave out",
    );
    let phases = [
        vec![(0, folder("\u{c9}"))],
        vec![
            (0, delete(0)),
            (1, user(1, 0, 1, "a")),
            (1, folder("b")),
            (1, Step::Work(0)),
            (1, rename(1, 2, "ss")),
        ],
    ];
    converge_excluding(&[vec![], vec!["/b"]], &phases).unwrap();
}

#[test]
fn a_folder_made_at_the_name_l_keeps_for_a_file_moved_into_an_excluded_folder_waits_for_it() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 1 is quiet, but nodes [\"01a1c2c0-39a0-7cdc-8389-c4663807ce68\"] wait for names that entries its rules leave out hold",
    );
    let phases = [
        vec![(0, file("a")), (1, folder("c.txt"))],
        vec![
            (1, act(3, 451136702324572615, 2740734820468211981, "a", 0)),
            (0, rename(0, 0, "b")),
            (2, folder("a")),
            (0, act(0, 13001166, 13484083470633789766, "E\u{301}", 2)),
        ],
    ];
    converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
}

#[test]
fn a_folder_made_at_the_name_of_a_file_renamed_to_an_excluded_name_waits_with_what_it_holds() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 0 is quiet, but nodes [\"01a1c2c0-39a0-7504-af4d-4d6a531020af\", \"01a1c2c0-39a0-796a-8df8-fb6da7904e2f\"] wait for names that entries its rules leave out hold",
    );
    let phases = [
        vec![(0, file("\u{c9}"))],
        vec![
            (1, rename(0, 0, "c.txt")),
            (0, folder("a")),
            (1, folder("\u{c9}")),
            (1, user(0, 0, 5931057209777015375, "a")),
        ],
    ];
    converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
}

#[test]
fn a_folder_kept_for_a_file_its_rules_exclude_stays_out_of_sync_once_the_file_leaves() {
    let phases = [
        vec![(1, folder("\u{df}")), (1, file("C.TXT"))],
        vec![(1, rename(1, 1, "c.txt")), (0, delete(0))],
        vec![(1, rename(1, 0, "\u{c9}"))],
    ];
    converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
}

#[test]
fn a_rules_file_another_device_writes_leaves_out_what_it_names_until_it_goes() {
    let rulings = [
        (0, 0, Ruling::File(vec!["*.txt"])),
        (2, 0, Ruling::File(vec![])),
    ];
    let phases = [vec![(0, file("a"))], vec![(1, file("c.txt"))], vec![]];
    let world = converge_ruled(&[vec![], vec![]], &rulings, &phases[..2]).unwrap();
    assert_eq!(live_node_at(&world, &["c.txt"]), None);
    ino_at(&world, 1, &[".koiignore"]);
    let world = converge_ruled(&[vec![], vec![]], &rulings, &phases).unwrap();
    assert_eq!(c_txt(&world, 0), c_txt(&world, 1));
}

#[test]
fn files_its_rules_left_out_sync_once_its_rules_let_them_back() {
    let lifted = [(1, 1, Ruling::Exclude(vec![]))];
    let included = [
        (0, 1, Ruling::Include(vec!["a"])),
        (1, 1, Ruling::Include(vec!["a", "c.txt"])),
    ];
    let phases = [vec![(1, file("c.txt")), (1, file("a"))], vec![]];
    for (exclude, rulings) in [(vec!["*.txt"], &lifted[..]), (vec![], &included[..])] {
        let mut world = converge_ruled(&[vec![], exclude.clone()], rulings, &phases[..1]).unwrap();
        assert_eq!(live_node_at(&world, &["c.txt"]), None);
        assert!(live_node_at(&world, &["a"]).is_some());
        world.run(1, &[(1, Step::Crash)]).unwrap();
        world.rule(1, &rulings[rulings.len() - 1].2).unwrap();
        world.phase(1, &[]).unwrap();
        assert_eq!(c_txt(&world, 0), c_txt(&world, 1));
        let world = converge_ruled(&[vec![], exclude], rulings, &phases).unwrap();
        assert_eq!(c_txt(&world, 0), c_txt(&world, 1));
    }
}

#[test]
fn a_synced_file_its_rules_come_to_leave_out_keeps_its_bytes_when_another_device_changes_it() {
    let mut renamed = vec![vec![(0, folder("a")), (0, user(1, 0, 1, "b"))]];
    renamed.extend([vec![(1, rename(1, 1, "c.txt"))], vec![(0, delete(1))]]);
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &renamed).unwrap();
    assert_eq!(live_node_at(&world, &["a", "b"]), None);
    ino_at(&world, 1, &["a", "c.txt"]);
    let ruled = |last| {
        let rulings = [(1, 1, Ruling::Exclude(vec!["*.txt"]))];
        let phases = [vec![(0, file("c.txt"))], last];
        converge_ruled(&[vec![], vec![]], &rulings, &phases).unwrap()
    };
    let (before, world) = (ruled(vec![]), ruled(vec![(0, overwrite(0))]));
    assert_eq!(c_txt(&world, 1), c_txt(&before, 1));
    assert_ne!(c_txt(&world, 0), c_txt(&world, 1));
}

#[test]
fn a_folder_its_include_list_takes_under_a_pending_name_is_sent() {
    let _red = expect_red(
        Check::SentLeftOut,
        "a commit of device 1 places node 01a1c2c0-39a0-7504-af4d-4d6a531020af at [\"ss (pending)\"], which the rules of device 1 leave out",
    );
    let work = (1, Step::Work(0));
    let mut phase = vec![(1, folder("\u{df}")), work, (1, folder("ss")), work];
    phase.push((1, rename(0, 0, "a")));
    let rulings = [(0, 1, Ruling::Include(vec!["ss/"]))];
    converge_ruled(&[vec![], vec![]], &rulings, &[phase]).unwrap();
}

fn c_txt(world: &World, device: usize) -> Content {
    content_on(world, device, "c.txt")
}
