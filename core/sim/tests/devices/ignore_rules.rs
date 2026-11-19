// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::effects::Effect;

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
fn a_folder_kept_for_excluded_entries_waits_for_a_scan_of_itself_once_they_go() {
    let (_, mut phases) = kept_by_its_rules();
    phases.push(vec![(1, folder("ss"))]);
    let mut world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    let kept = |world: &World| {
        let trees = &world.devices[1].engine.trees;
        (trees.local().iter()).any(|(id, node)| node.shape.name == "a" && trees.kept_folder(id))
    };
    assert!(kept(&world));
    let txt = target(&world, 1, &["a", "c.txt"]);
    let other = folder_number(&world, 1, &["ss"]);
    let own = folder_number(&world, 1, &["a"]);

    world
        .run(
            phases.len(),
            &[(1, delete(txt)), (1, Step::ScanFolder(other))],
        )
        .unwrap();
    assert!(kept(&world));

    world
        .run(phases.len(), &[(1, Step::ScanFolder(own))])
        .unwrap();
    assert!(!kept(&world));
}

#[test]
fn a_file_made_at_the_name_of_a_folder_its_rules_exclude_downloads_under_a_conflict_name() {
    let phases = [vec![(1, folder("ss"))], vec![(0, file("ss"))]];
    let world = converge_excluding(&[vec![], vec!["ss/"]], &phases).unwrap();
    let names: Vec<String> = (files_on(&world, 0).into_iter())
        .map(|(name, _)| name)
        .collect();
    let [copy] = names.as_slice() else {
        panic!("{names:?}");
    };
    assert!(copy.starts_with("ss (conflict from "), "{copy}");
    assert_eq!(content_on(&world, 1, copy), content_on(&world, 0, copy));
    assert_eq!(world.devices[1].disk.entries().len(), 2);
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
    removes_only_the_refused_folder(
        &converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap(),
        1,
    );
}

fn removes_only_the_refused_folder(world: &World, attempts: usize) {
    let refused = NodeId::from("01a1c2c0-39a0-7cdc-8389-c4663807ce68");
    let ran = &world.folder_effects[0];
    assert!(
        ran.len() == attempts
            && ran.iter().all(|record| {
                record.node == refused && matches!(record.effect, Effect::RemoveDir { .. })
            }),
        "{ran:?}"
    );
}

#[test]
fn a_folder_another_device_deletes_stays_parked_around_a_file_its_rules_came_to_exclude() {
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

#[test]
fn a_folder_kept_for_excluded_entries_goes_at_the_restart_after_they_go() {
    let (mut world, phases) = kept_by_its_rules();
    let txt = target(&world, 1, &["a", "c.txt"]);
    let steps = [(1, Step::Crash), (1, delete(txt)), (1, Step::Connect)];
    world.run(phases.len(), &steps).unwrap();
    let trees = &world.devices[1].engine.trees;
    let kept =
        (trees.local().iter()).any(|(id, node)| node.shape.name == "a" && trees.kept_folder(id));
    assert!(!kept);
}

fn refused_beside_a(last: (usize, Step)) -> Result<World, Failure> {
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
        vec![last],
    ];
    converge_excluding(&[vec!["*.txt"], vec![]], &phases)
}

#[test]
fn a_folder_deleted_beside_a_refused_folder_of_its_name_in_another_case_is_deleted_everywhere() {
    removes_only_the_refused_folder(&refused_beside_a((0, delete(0))).unwrap(), 2);
}

#[test]
fn a_folder_renamed_elsewhere_to_the_case_of_a_refused_folder_beside_it_is_renamed_here() {
    let _parked = expect_red(
        Check::RemoveLeftOut,
        "device 0 is quiet, but it keeps the folders of [\"01a1c2c0-39a0-7cdc-8389-c4663807ce68\"] parked, which hold entries its rules leave out",
    );
    removes_only_the_refused_folder(&refused_beside_a((1, rename(0, 0, "A"))).unwrap(), 2);
}

fn deleted_while_its_rules_leave_a_file_out(during: Vec<(usize, Step)>) -> Result<World, Failure> {
    let mut deleting = vec![(1, Step::CopyStore), (0, delete(0))];
    deleting.extend(during);
    let phases = [
        vec![
            (0, folder("a")),
            (0, user(1, 0, 1, "c.txt")),
            (0, user(1, 0, 1, "b")),
        ],
        deleting,
        vec![],
    ];
    let rulings = [
        (1, 1, Ruling::Exclude(vec!["*.txt"])),
        (2, 1, Ruling::Exclude(vec![])),
    ];
    converge_ruled(&[vec![], vec![]], &rulings, &phases)
}

#[test]
fn a_folder_another_device_deletes_goes_once_its_rules_let_the_file_they_left_out_back_in() {
    let world = deleted_while_its_rules_leave_a_file_out(Vec::new()).unwrap();
    assert_eq!(live_node_at(&world, &["a"]), None);
    assert!(files_on(&world, 1).is_empty());
}

#[test]
fn a_folder_deleted_while_its_rules_leave_a_file_out_goes_through_crashes_and_rollbacks() {
    for n in 0..16 {
        for tail in [
            None,
            Some(Step::Crash),
            Some(Step::Effect(Outcome::Crash(Cut::Quarantined))),
            Some(Step::RollBack),
        ] {
            let mut during: Vec<(usize, Step)> =
                std::iter::repeat_n((1, Step::Work(0)), n).collect();
            during.extend(tail.map(|step| (1, step)));
            deleted_while_its_rules_leave_a_file_out(during).unwrap();
        }
    }
}

fn edited_then_moved_into_an_ignored_folder(moves_a_folder: bool) {
    let (made, moved) = if moves_a_folder {
        (
            vec![
                (0, folder("a")),
                (0, user(0, 0, 1, "ss")),
                (0, user(1, 0, 2, "c.txt")),
            ],
            vec![
                (0, folder("b")),
                (0, overwrite(2)),
                (0, SCAN),
                (0, rename(1, 3, "ss")),
                (0, SCAN),
                (1, delete(0)),
            ],
        )
    } else {
        (
            vec![(0, folder("a")), (0, user(1, 0, 1, "c.txt"))],
            vec![
                (0, folder("b")),
                (0, overwrite(1)),
                (0, SCAN),
                (0, rename(1, 2, "c.txt")),
                (0, SCAN),
                (1, delete(0)),
            ],
        )
    };
    let world = converge_excluding(&[vec!["b/"], vec![]], &[made, moved]).unwrap();
    assert_eq!(live_node_at(&world, &["a"]), None);
    let nodes = world.space().nodes();
    let held = (nodes.iter()).find(|node| node.name == "c.txt").unwrap();
    assert_ne!(held.status, NodeStatus::Live as i32);
    let old = (held.current.as_ref()).and_then(|version| version.content.clone());
    let edited = c_txt(&world, 0);
    assert!(
        old.is_some_and(|old| old != edited),
        "the server keeps the version it has"
    );
}

#[test]
fn a_file_edited_then_moved_into_an_ignored_folder_keeps_its_edit_there_when_its_old_folder_goes() {
    edited_then_moved_into_an_ignored_folder(false);
}

#[test]
fn a_folder_moved_into_an_ignored_folder_with_an_edit_keeps_it_there_when_its_old_folder_goes() {
    edited_then_moved_into_an_ignored_folder(true);
}

#[test]
fn a_file_at_the_name_of_a_left_out_folder_downloads_at_that_name_once_the_folder_moves_away() {
    let mut racing = vec![(0, user(1, 0, 1, "ss"))];
    racing.extend(commit(0));
    racing.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
        (1, rename(1, 0, "ss")),
    ]);
    let phases = [
        vec![(0, folder("b"))],
        vec![(1, user(0, 0, 1, "ss"))],
        racing,
    ];
    let world = converge_excluding(&[vec![], vec!["ss/"]], &phases).unwrap();
    let files = |device| -> Vec<(String, Option<Content>)> {
        (files_on(&world, device).into_iter())
            .filter(|(_, content)| content.is_some())
            .collect()
    };
    assert_eq!(files(0).len(), 1);
    assert_eq!(files(1), files(0));
    assert_eq!(files(1)[0].0, "ss");
    assert_eq!(world.devices[1].disk.entries().len(), 3);
}

#[test]
fn a_file_another_device_renames_onto_the_name_of_a_folder_its_rules_exclude_gets_a_conflict_name()
{
    let phases = [
        vec![(1, folder("ss"))],
        vec![(0, file("a"))],
        vec![(0, rename(0, 0, "ss"))],
    ];
    let world = converge_excluding(&[vec![], vec!["ss/"]], &phases).unwrap();
    let names: Vec<String> = (files_on(&world, 0).into_iter())
        .map(|(name, _)| name)
        .collect();
    let [copy] = names.as_slice() else {
        panic!("{names:?}");
    };
    assert!(copy.starts_with("ss (conflict from "), "{copy}");
    assert_eq!(content_on(&world, 1, copy), content_on(&world, 0, copy));
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

#[test]
fn a_file_refused_at_the_name_of_a_left_out_folder_keeps_that_name_once_the_rules_let_the_folder_in()
 {
    let mut world = converge_excluding(
        &[vec![], vec!["ss/"]],
        &[vec![(1, folder("ss")), (1, user(1, 0, 1, "b"))]],
    )
    .unwrap();
    let mut steps = vec![(0, file("ss"))];
    steps.extend(commit(0));
    steps.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT)]);
    world.run(1, &steps).unwrap();
    assert!(
        !world.devices[1]
            .engine
            .trees
            .state()
            .names_left_out
            .is_empty()
    );
    world.rule(1, &Ruling::Exclude(vec![])).unwrap();
    world.phase(2, &[]).unwrap();
    assert!(
        live_node_at(&world, &["ss"]).is_some(),
        "{:?}",
        files_on(&world, 1)
    );
}
