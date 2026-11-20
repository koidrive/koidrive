// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::effects::{Effect, Stuck};

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
fn a_folder_made_in_a_folder_another_device_renamed_to_a_name_its_rules_exclude_stays_on_its_device()
 {
    let phases = [
        vec![(0, folder("a"))],
        vec![(0, rename(0, 0, "E\u{301}"))],
        vec![(1, user(0, 0, 1, "b"))],
    ];
    let world = converge_excluding(&[vec![], vec!["e\u{301}/"]], &phases).unwrap();
    assert_eq!(live_node_at(&world, &["\u{c9}", "b"]), None);
    ino_at(&world, 1, &["a", "b"]);
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
fn a_folder_the_server_refuses_stays_in_place_and_is_reported() {
    let mut world = refused_beside_a(None);
    let source = reported_merge(&world);
    let kept = content_on(&world, 0, "c.txt");
    let before = world.folder_effects[0].len();
    world
        .phase(2, &[(0, Step::Crash), (0, Step::Connect)])
        .unwrap();
    assert_eq!(reported_merge(&world), source);
    assert_eq!(content_on(&world, 0, "c.txt"), kept);
    assert_eq!(world.folder_effects[0].len(), before);
}

fn reported_merge(world: &World) -> NodeId {
    let source = held_at(world, 0, &["A"]).unwrap();
    let survivor = NodeId::from("01a1c2c0-39a0-7510-83bc-38d6c6b89fe4");
    assert_eq!(
        world.reported_problems(0).get(&source),
        Some(&Stuck::MergedInto(survivor))
    );
    ino_at(world, 0, &["A", "c.txt"]);
    assert!(!world.devices[0].engine.trees.kept_folder(&source));
    assert!(!world.folder_effects[0].iter().any(|record| {
        record.node == source
            && matches!(
                record.effect,
                Effect::CreateDir { .. } | Effect::RemoveDir { .. }
            )
    }));
    source
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
fn a_folder_made_at_the_name_of_a_file_renamed_to_an_excluded_name_syncs_with_what_it_holds() {
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
fn a_folder_its_include_list_takes_syncs_through_a_pending_name_outside_that_list() {
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

fn refused_beside_a(last: Option<(usize, Step)>) -> World {
    let mut phases = vec![
        vec![(0, folder("a"))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(1, 0, "A")),
        ],
    ];
    if let Some(last) = last {
        phases.push(vec![last]);
    }
    converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap()
}

fn freeing_the_name_key_uploads_a_reported_merge_folder_as_new(
    device: usize,
    free: fn(usize) -> Step,
) {
    let mut world = refused_beside_a(None);
    let source = reported_merge(&world);
    let live = target(&world, device, &["a"]);
    world.phase(2, &[(device, free(live))]).unwrap();
    assert_eq!(live_node_at(&world, &["a"]), None);
    assert!(world.reported_problems(0).is_empty());
    let created: Vec<String> = (world.space().nodes().into_iter())
        .filter(|node| node.name == "A")
        .map(|node| node.node_id)
        .collect();
    assert_eq!(created.len(), 1);
    assert_ne!(created[0], source.to_string());
    assert_eq!(
        held_at(&world, 0, &["A"]),
        Some(NodeId::from(created[0].clone()))
    );
    assert!(
        world
            .space()
            .nodes()
            .iter()
            .all(|node| node.name != "c.txt")
    );
    ino_at(&world, 0, &["A", "c.txt"]);
    let empty = ino_at(&world, 1, &["A"]);
    assert_eq!(
        world.devices[1].disk.child(&Place::In(empty), "c.txt"),
        None
    );
    world.phase(3, &[]).unwrap();
    assert_eq!(live_node_at(&world, &["A"]), Some(created[0].clone()));
    assert_eq!(
        (world.space().nodes().iter())
            .filter(|node| node.name == "A")
            .count(),
        1
    );
    let uploaded = NodeId::from(created[0].clone());
    assert!(!world.folder_effects[0].iter().any(|record| {
        record.node == uploaded
            && matches!(
                record.effect,
                Effect::CreateDir { .. } | Effect::RemoveDir { .. }
            )
    }));
}

#[test]
fn a_folder_deleted_beside_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder() {
    freeing_the_name_key_uploads_a_reported_merge_folder_as_new(0, delete);
}

#[test]
fn a_folder_another_device_deletes_beside_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder()
 {
    freeing_the_name_key_uploads_a_reported_merge_folder_as_new(1, delete);
}

#[test]
fn a_folder_another_device_renames_away_from_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder()
 {
    freeing_the_name_key_uploads_a_reported_merge_folder_as_new(1, |live| rename(live, 0, "b"));
}

#[test]
fn a_folder_renamed_elsewhere_to_the_case_of_a_refused_folder_beside_it_is_renamed_here() {
    let mut world = refused_beside_a(Some((1, rename(0, 0, "A"))));
    let source = reported_merge(&world);
    let survivor = held_at(&world, 0, &["a"]).unwrap();
    assert_eq!(
        world.reported_problems(0).get(&survivor),
        Some(&Stuck::Behind(source))
    );
    assert_eq!(live_node_at(&world, &["A"]), Some(survivor.to_string()));
    let remove = target(&world, 0, &["A"]);
    world.phase(3, &[(0, delete(remove))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert_eq!(held_at(&world, 0, &["A"]), Some(survivor));
}

#[test]
fn moving_a_reported_merge_folder_away_clears_the_problem() {
    let mut world = refused_beside_a(None);
    reported_merge(&world);
    let content = content_on(&world, 0, "c.txt");
    let source = target(&world, 0, &["A"]);
    world.phase(2, &[(0, rename(source, 0, "b"))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    ino_at(&world, 0, &["b", "c.txt"]);
    assert_eq!(content_on(&world, 0, "c.txt"), content);
}

#[test]
fn a_rule_lift_syncs_the_left_out_file_of_a_reported_merge_folder() {
    let mut world = refused_beside_a(None);
    reported_merge(&world);
    let content = content_on(&world, 0, "c.txt");
    world.rule(0, &Ruling::Exclude(vec![])).unwrap();
    world.phase(2, &[]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert_eq!(world.devices[0].disk.child(&Place::Root, "A"), None);
    ino_at(&world, 0, &["a", "c.txt"]);
    assert_eq!(content_on(&world, 0, "c.txt"), content);
    assert!(live_node_at(&world, &["a", "c.txt"]).is_some());
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

fn refused_at_a_left_out_folders_name(
    rule: &'static str,
    first: &[Vec<(usize, Step)>],
    arrives: Step,
) -> (World, usize) {
    let mut world = converge_excluding(&[vec![], vec![rule]], first).unwrap();
    let mut steps = vec![(0, arrives)];
    steps.extend(commit(0));
    steps.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT)]);
    world.run(first.len(), &steps).unwrap();
    assert!(
        !world.devices[1]
            .engine
            .trees
            .state()
            .names_left_out
            .is_empty()
    );
    (world, first.len())
}

fn left_out_name_shapes() -> [(World, usize); 2] {
    let downloaded = [vec![(1, folder("ss")), (1, user(1, 0, 1, "b"))]];
    let moved = [vec![(1, folder("ss"))], vec![(0, file("a"))]];
    [
        refused_at_a_left_out_folders_name("ss/", &downloaded, file("ss")),
        refused_at_a_left_out_folders_name("ss/", &moved, rename(0, 0, "ss")),
    ]
}

#[test]
fn a_file_refused_at_a_left_out_folders_name_takes_one_conflict_name_however_often_the_device_commits()
 {
    for (shape, (mut world, phase)) in left_out_name_shapes().into_iter().enumerate() {
        let steps: Vec<(usize, Step)> = (0..3).flat_map(|_| commit(1)).collect();
        world.run(phase, &steps).unwrap();
        world.phase(phase + 1, &[]).unwrap();
        for device in 0..2 {
            let copies = conflict_copies(&world, device);
            let [copy] = copies.as_slice() else {
                panic!("shape {shape}, device {device}: {copies:?}");
            };
            assert_eq!(
                copy.matches("(conflict from").count(),
                1,
                "shape {shape}: {copy}"
            );
        }
    }
}

#[test]
fn a_file_deleted_here_before_it_follows_its_conflict_name_comes_back_for_the_move_its_user_never_saw()
 {
    let [_, (mut world, phase)] = left_out_name_shapes();
    let mut steps = commit(1);
    steps.push((1, delete(1)));
    world.run(phase, &steps).unwrap();
    world.phase(phase + 1, &[]).unwrap();
    for device in 0..2 {
        assert_eq!(conflict_copies(&world, device).len(), 1, "{device}");
    }
}

#[test]
fn a_file_refused_beside_a_folder_an_anchored_rule_leaves_out_takes_one_conflict_name_once_another_device_renames_their_parent()
 {
    let first = [vec![(1, folder("b")), (1, user(0, 0, 1, "ss"))]];
    let (mut world, phase) =
        refused_at_a_left_out_folders_name("/b/ss/", &first, user(1, 0, 1, "ss"));
    let mut steps = vec![(0, rename(0, 0, "A"))];
    steps.extend(commit(0));
    world.run(phase, &steps).unwrap();
    world.phase(phase + 1, &[]).unwrap();
    assert_eq!(paths_on(&world, 0), paths_on(&world, 1));
    assert_eq!(
        conflict_copies(&world, 1).len(),
        1,
        "{:?}",
        paths_on(&world, 1)
    );
}

#[test]
fn a_refused_file_gets_one_conflict_name_while_its_parents_rename_is_blocked() {
    for parent_moves_first in [false, true] {
        refuse_under_a_parent_whose_rename_is_blocked(parent_moves_first, None);
    }
}

#[test]
fn a_refused_file_skips_conflict_names_left_out_at_either_parent_path() {
    for rule in ["/a/* (conflict *-??-??)", "/b/* (conflict *-??-??)"] {
        for parent_moves_first in [false, true] {
            let copy =
                refuse_under_a_parent_whose_rename_is_blocked(parent_moves_first, Some(rule));
            assert!(copy.ends_with(" 2)"), "{rule}: {copy}");
        }
    }
}

fn refuse_under_a_parent_whose_rename_is_blocked(
    parent_moves_first: bool,
    candidate_rule: Option<&'static str>,
) -> String {
    let mut rules = vec!["*.txt", "/b/ss/"];
    rules.extend(candidate_rule);
    let mut world = converge_excluding(
        &[vec![], rules],
        &[
            vec![(0, folder("a"))],
            vec![(1, user(1, 0, 1, "c.txt"))],
            vec![(0, delete(0))],
            vec![(0, folder("b"))],
        ],
    )
    .unwrap();
    ino_at(&world, 1, &["a", "c.txt"]);
    let local_parent = folder_number(&world, 1, &["b"]);
    world
        .run(4, &[(1, user(0, 0, local_parent, "ss"))])
        .unwrap();
    let rename_parent = |world: &mut World| {
        let parent = target(world, 0, &["b"]);
        let mut steps = vec![(0, rename(parent, 0, "a"))];
        steps.extend(commit(0));
        steps.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT)]);
        world.run(4, &steps).unwrap();
    };
    if parent_moves_first {
        rename_parent(&mut world);
    }
    let parent = if parent_moves_first { "a" } else { "b" };
    let server_parent = folder_number(&world, 0, &[parent]);
    let mut steps = vec![(0, user(1, 0, server_parent, "ss"))];
    steps.extend(commit(0));
    steps.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT)]);
    world.run(4, &steps).unwrap();
    if !parent_moves_first {
        assert!(
            !world.devices[1]
                .engine
                .trees
                .state()
                .names_left_out
                .is_empty()
        );
        rename_parent(&mut world);
    }
    let holder = ino_at(&world, 1, &["b", "ss"]);
    let file = live_node_at(&world, &["a", "ss"]).unwrap();
    let attempts: Vec<_> = (0..3).flat_map(|_| commit(1)).collect();
    world.run(4, &attempts).unwrap();
    let copy = world.space().node(&file).unwrap().name;
    assert_eq!(copy.matches("(conflict from").count(), 1, "{copy}");
    assert_eq!(world.seen.get("move with from applied"), Some(&1));
    assert!(
        world.devices[1]
            .engine
            .trees
            .state()
            .names_left_out
            .is_empty()
    );
    world.run(4, &[(1, EFFECT); 2]).unwrap();
    ino_at(&world, 1, &["b", &copy]);
    assert_eq!(ino_at(&world, 1, &["b", "ss"]), holder);
    copy
}

fn conflict_copies(world: &World, device: usize) -> Vec<String> {
    (files_on(world, device).into_iter())
        .map(|(name, _)| name)
        .filter(|name| name.contains("(conflict from"))
        .collect()
}

#[test]
fn a_file_another_device_deletes_after_the_disk_refused_its_name_here_stays_deleted() {
    for heard in [true, false] {
        let [_, (mut world, phase)] = left_out_name_shapes();
        let mut steps = vec![(0, delete(0))];
        steps.extend(commit(0));
        steps.extend(heard.then_some((1, DELIVER)));
        steps.extend(commit(1));
        world.run(phase, &steps).unwrap();
        world.phase(phase + 1, &[]).unwrap();
        assert_eq!(world.seen.get("move with from applied"), None, "{heard}");
        let superseded = world.seen.get("move with from superseded").copied();
        assert_eq!(superseded, (!heard).then_some(1), "{heard}");
        assert_eq!(live_node_at(&world, &["ss"]), None);
        for device in 0..2 {
            assert!(
                files_on(&world, device)
                    .iter()
                    .all(|(_, content)| content.is_none())
            );
        }
    }
}

#[test]
fn a_file_another_device_renames_after_the_disk_refused_its_name_here_keeps_that_name() {
    for heard in [true, false] {
        let [_, (mut world, phase)] = left_out_name_shapes();
        let mut steps = vec![(0, rename(0, 0, "b"))];
        steps.extend(commit(0));
        steps.extend(heard.then_some((1, DELIVER)));
        steps.extend(commit(1));
        world.run(phase, &steps).unwrap();
        world.phase(phase + 1, &[]).unwrap();
        assert_eq!(world.seen.get("move with from applied"), None, "{heard}");
        let superseded = world.seen.get("move with from superseded").copied();
        assert_eq!(superseded, (!heard).then_some(1), "{heard}");
        assert!(live_node_at(&world, &["b"]).is_some());
        for device in 0..2 {
            assert_eq!(conflict_copies(&world, device), Vec::<String>::new());
            assert!(files_on(&world, device).iter().any(|(name, _)| name == "b"));
        }
        assert!(
            world.devices[1]
                .engine
                .trees
                .state()
                .names_left_out
                .is_empty()
        );
    }
}

#[test]
fn a_file_whose_conflict_rename_is_in_flight_when_the_rules_let_its_name_in_follows_that_rename() {
    for lift in 0..5 {
        let [_, (mut world, phase)] = left_out_name_shapes();
        let steps = commit(1);
        world.run(phase, &steps[..lift]).unwrap();
        world.rule(1, &Ruling::Exclude(vec![])).unwrap();
        world.run(phase, &steps[lift..]).unwrap();
        world.phase(phase + 1, &[]).unwrap();
        assert_eq!(paths_on(&world, 0), paths_on(&world, 1), "{lift}");
        assert_eq!(conflict_copies(&world, 1).len(), 1, "{lift}");
    }
}

fn held_where_the_server_renamed_its_folder() -> (World, String) {
    let phases = [
        vec![(0, folder("a"))],
        vec![(0, rename(0, 0, "c.txt"))],
        vec![(1, user(1, 0, 1, "b"))],
    ];
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    assert_eq!(live_node_at(&world, &["a"]), None);
    assert_eq!(live_node_at(&world, &["c.txt", "b"]), None);
    let folder = live_node_at(&world, &["c.txt"]).unwrap();
    (world, folder)
}

#[test]
fn an_entry_held_in_a_folder_its_rules_exclude_on_the_server_syncs_in_a_new_folder_once_another_device_deletes_it()
 {
    for later in [vec![], vec![(1, user(1, 0, 1, "ss"))]] {
        let (mut world, folder) = held_where_the_server_renamed_its_folder();
        world.phase(3, &[(0, delete(0))]).unwrap();
        world.phase(4, &later).unwrap();
        assert_eq!(live_node_at(&world, &["c.txt"]), None);
        assert_eq!(live_node_at(&world, &["c.txt", "b"]), None);
        assert_ne!(live_node_at(&world, &["a"]), Some(folder));
        assert!(live_node_at(&world, &["a", "b"]).is_some());
        assert_eq!(
            live_node_at(&world, &["a", "ss"]).is_some(),
            !later.is_empty()
        );
    }
}

#[test]
fn a_folder_held_by_its_server_path_syncs_in_new_folders_once_another_device_deletes_a_folder_above_it()
 {
    let phases = [
        vec![(0, folder("a")), (0, user(0, 0, 1, "b"))],
        vec![(0, rename(1, 1, "c.txt"))],
        vec![(1, user(1, 0, 2, "ss"))],
    ];
    let mut world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    assert_eq!(live_node_at(&world, &["a", "c.txt", "ss"]), None);
    let folder = live_node_at(&world, &["a"]);
    world.phase(3, &[(0, delete(0))]).unwrap();
    assert_ne!(live_node_at(&world, &["a"]), folder);
    assert!(live_node_at(&world, &["a", "b", "ss"]).is_some());
    assert_eq!(live_node_at(&world, &["a", "c.txt"]), None);
}

#[test]
fn a_restored_or_purged_server_folder_leaves_the_new_folder_its_held_entry_syncs_in() {
    let (mut world, folder) = held_where_the_server_renamed_its_folder();
    world.phase(3, &[(0, delete(0))]).unwrap();
    world.restored_from_trash(0, &folder);
    let engine = &mut world.devices[0].engine;
    let request = engine.ids.uuid_v7(1_700_000_000_000);
    let intent = RestoreIntent::from_trash((&folder).into(), None, None).unwrap();
    assert!(engine.commits.restore(request, intent));
    world.phase(4, &commit(0)).unwrap();
    assert_eq!(live_node_at(&world, &["c.txt"]), Some(folder.clone()));
    assert_eq!(live_node_at(&world, &["c.txt", "b"]), None);
    assert!(live_node_at(&world, &["a", "b"]).is_some());
    ino_at(&world, 0, &["c.txt"]);
    for unseen in [false, true] {
        let (mut world, folder) = held_where_the_server_renamed_its_folder();
        let mut deleting = vec![(0, delete(0))];
        deleting.extend(commit(0));
        match unseen {
            true => world.run(3, &deleting).unwrap(),
            false => world.phase(3, &deleting).unwrap(),
        }
        assert!(world.space_mut().purge(&folder, 0));
        world.phase(4, &[]).unwrap();
        assert!(live_node_at(&world, &["a", "b"]).is_some());
        assert_eq!(live_node_at(&world, &["c.txt"]), None);
    }
}

#[test]
fn an_entry_held_by_its_server_path_joins_a_folder_another_device_makes_at_its_place() {
    let (mut world, deleted) = held_where_the_server_renamed_its_folder();
    world.phase(3, &[(0, delete(0)), (0, folder("a"))]).unwrap();
    let made = live_node_at(&world, &["a"]).unwrap();
    assert_ne!(made, deleted);
    assert!(live_node_at(&world, &["a", "b"]).is_some());
    assert_eq!(
        world
            .space()
            .nodes()
            .iter()
            .filter(|node| node.name == "a")
            .count(),
        1
    );
}

fn synced_where_the_server_renamed_its_folder(made: Step) -> (World, String) {
    let phases = [
        vec![(0, folder("a")), (0, made)],
        vec![(0, rename(0, 0, "c.txt"))],
    ];
    let world = converge_excluding(&[vec![], vec!["*.txt"]], &phases).unwrap();
    assert!(live_node_at(&world, &["c.txt", "b"]).is_some());
    let folder = live_node_at(&world, &["c.txt"]).unwrap();
    (world, folder)
}

#[test]
fn synced_entries_of_a_folder_held_by_its_server_path_follow_another_devices_delete() {
    for made in [user(1, 0, 1, "b"), user(0, 0, 1, "b")] {
        for new in [vec![], vec![(1, user(1, 0, 1, "ss"))]] {
            let (mut world, folder) = synced_where_the_server_renamed_its_folder(made);
            world.phase(2, &new).unwrap();
            world.phase(3, &[(0, delete(0))]).unwrap();
            assert_eq!(live_node_at(&world, &["c.txt"]), None);
            assert_eq!(live_node_at(&world, &["a", "b"]), None);
            assert_eq!(
                live_node_at(&world, &["a", "ss"]).is_some(),
                !new.is_empty()
            );
            assert_ne!(live_node_at(&world, &["a"]), Some(folder));
        }
    }
}

const NEW_FOLDER_SENT: &str = "a commit of device 1 places node 01a1c2c0-39a0-7504-af4d-4d6a531020af at [\"c.txt\", \"ss\"], which the rules of device 1 leave out";
const MOVED_FILE_SENT: &str = "a commit of device 1 places node 01a1c2c0-39a0-7735-80db-d7e668859a70 at [\"c.txt\", \"ss\", \"b\"], which the rules of device 1 leave out";

fn synced_file_moved_into_a_new_folder_where_the_server_renamed_its_folder()
-> Result<World, Failure> {
    let phases = [
        vec![(0, folder("a")), (0, user(1, 0, 1, "b"))],
        vec![(0, rename(0, 0, "c.txt"))],
        vec![(1, user(0, 0, 1, "ss")), (1, rename(1, 2, "b"))],
    ];
    converge_excluding(&[vec![], vec!["*.txt"]], &phases)
}

#[test]
fn a_new_folder_holding_a_synced_file_moved_into_a_folder_another_device_renamed_to_a_name_its_rules_exclude_is_sent()
 {
    let _folder = expect_red(Check::SentLeftOut, NEW_FOLDER_SENT);
    let _file = expect_red(Check::SentLeftOut, MOVED_FILE_SENT);
    synced_file_moved_into_a_new_folder_where_the_server_renamed_its_folder().unwrap();
}

#[test]
fn a_commit_that_places_two_entries_where_its_rules_exclude_fails_on_the_one_no_guard_expects() {
    let _folder = expect_red(Check::SentLeftOut, NEW_FOLDER_SENT);
    let failure = synced_file_moved_into_a_new_folder_where_the_server_renamed_its_folder();
    let why = format!("after phase 2: after the faults stopped, on device 1: {MOVED_FILE_SENT}");
    assert_eq!(failure.err(), Some(Failure::Broken(why)));
}

#[test]
fn a_file_made_in_a_folder_its_include_list_takes_syncs_through_the_folders_pending_name() {
    let work = (1, Step::Work(0));
    let mut phase = vec![(1, folder("\u{df}")), work, (1, folder("ss"))];
    phase.extend([(1, user(1, 0, 2, "b")), work, (1, rename(0, 0, "a"))]);
    let rulings = [(0, 1, Ruling::Include(vec!["ss/"]))];
    converge_ruled(&[vec![], vec![]], &rulings, &[phase]).unwrap();
}
