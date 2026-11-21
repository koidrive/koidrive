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
    free: impl Fn(&World) -> Vec<(usize, Step)>,
) -> World {
    let mut world = refused_beside_a(None);
    let source = reported_merge(&world);
    let schedule = free(&world);
    world.phase(2, &schedule).unwrap();
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
    world
}

fn on_the_live_folder(
    device: usize,
    step: fn(usize) -> Step,
) -> impl Fn(&World) -> Vec<(usize, Step)> {
    move |world| vec![(device, step(target(world, device, &["a"])))]
}

#[test]
fn a_folder_deleted_beside_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder() {
    freeing_the_name_key_uploads_a_reported_merge_folder_as_new(on_the_live_folder(0, delete));
}

#[test]
fn a_folder_another_device_deletes_beside_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder()
 {
    freeing_the_name_key_uploads_a_reported_merge_folder_as_new(on_the_live_folder(1, delete));
}

fn moved_away_keeping_its_id(free: impl Fn(&World) -> Vec<(usize, Step)>, to: &[&str]) {
    let original = std::cell::RefCell::new(None);
    let world = freeing_the_name_key_uploads_a_reported_merge_folder_as_new(|world| {
        *original.borrow_mut() = live_node_at(world, &["a"]);
        free(world)
    });
    let original = original.into_inner();
    assert!(original.is_some());
    assert_eq!(live_node_at(&world, to), original);
}

#[test]
fn a_folder_renamed_here_away_from_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder() {
    moved_away_keeping_its_id(
        |world| vec![(0, rename(target(world, 0, &["a"]), 0, "b"))],
        &["b"],
    );
}

#[test]
fn a_folder_another_device_renames_away_from_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder()
 {
    moved_away_keeping_its_id(on_the_live_folder(1, |live| rename(live, 0, "b")), &["b"]);
}

#[test]
fn a_folder_another_device_moves_away_from_a_reported_merge_folder_frees_it_to_upload_as_a_new_folder()
 {
    moved_away_keeping_its_id(
        |world| {
            let live = target(world, 1, &["a"]);
            vec![(1, folder("b")), (1, rename(live, 2, "a"))]
        },
        &["b", "a"],
    );
}

#[test]
fn a_file_made_offline_in_a_reported_merge_folder_uploads_in_it_when_the_live_folder_was_renamed_meanwhile()
 {
    let mut world = refused_beside_a(None);
    reported_merge(&world);
    let live = target(&world, 1, &["a"]);
    let merged = folder_number(&world, 0, &["A"]);
    let mut steps = vec![(0, Step::Drop), (1, rename(live, 0, "b"))];
    steps.extend(commit(1));
    steps.extend([(0, user(1, 0, merged, "ss")), (0, Step::Connect)]);
    world.phase(2, &steps).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert!(live_node_at(&world, &["A", "ss"]).is_some());
    assert_eq!(live_node_at(&world, &["b", "ss"]), None);
    assert_eq!(live_node_at(&world, &["A", "c.txt"]), None);
}

fn no_live_file_named(world: &World, name: &str) {
    let live: Vec<_> = (world.space().nodes().into_iter())
        .filter(|node| node.status == NodeStatus::Live as i32 && node.name == name)
        .collect();
    assert!(live.is_empty(), "{live:?}");
}

#[test]
fn a_live_file_at_the_name_key_keeps_a_reported_merge_folder_until_the_file_goes() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 0 is quiet, but nodes [\"01a1c2c0-39a0-796a-8df8-fb6da7904e2f\"] wait for names that entries its rules leave out hold",
    );
    let mut world = refused_beside_a(None);
    let source = reported_merge(&world);
    let live = target(&world, 1, &["a"]);
    let mut steps = vec![(0, Step::Drop), (1, rename(live, 0, "b")), (1, file("a"))];
    steps.extend(commit(1));
    steps.push((0, Step::Connect));
    world.phase(2, &steps).unwrap();
    let file = NodeId::from(&live_node_at(&world, &["a"]).unwrap());
    assert_eq!(
        world.reported_problems(0).get(&source),
        Some(&Stuck::MergedInto(file))
    );
    assert_eq!(live_node_at(&world, &["A"]), None);
    ino_at(&world, 0, &["A", "c.txt"]);
    let file = target(&world, 1, &["a"]);
    world.phase(3, &[(1, delete(file))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert!(live_node_at(&world, &["A"]).is_some());
    ino_at(&world, 1, &["A"]);
    no_live_file_named(&world, "c.txt");
}

#[test]
fn a_live_file_at_the_name_key_keeps_out_what_an_anchored_rule_leaves_out_in_a_reported_merge_folder()
 {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 0 is quiet, but nodes [\"01a1c2c0-39a0-796a-8df8-fb6da7904e2f\"] wait for names that entries its rules leave out hold",
    );
    let mut world = refused_beside_a(None);
    world.rule(0, &Ruling::Exclude(vec!["/A/c.txt"])).unwrap();
    world.phase(2, &[]).unwrap();
    let source = reported_merge(&world);
    let live = target(&world, 1, &["a"]);
    let mut steps = vec![(0, Step::Drop), (1, rename(live, 0, "b")), (1, file("a"))];
    steps.extend(commit(1));
    steps.push((0, Step::Connect));
    world.phase(3, &steps).unwrap();
    let file = NodeId::from(&live_node_at(&world, &["a"]).unwrap());
    assert_eq!(
        world.reported_problems(0).get(&source),
        Some(&Stuck::MergedInto(file))
    );
    no_live_file_named(&world, "c.txt");
    assert!((world.space().nodes().iter()).all(|node| !node.name.starts_with("A (conflict from")));
}

fn merge_folders_beside_a_live_folder_made_again(
    live: &str,
    merged: [&str; 2],
    made: &str,
) -> (World, [NodeId; 2], NodeId) {
    let phases = [
        vec![(0, folder(live))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(1, 0, merged[0])),
            (0, folder("b")),
            (0, user(1, 0, 3, "c.txt")),
            (0, rename(3, 0, merged[1])),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let ids = merged.map(|name| held_at(&world, 0, &[name]).unwrap());
    let removed = target(&world, 1, &[live]);
    world
        .phase(2, &[(1, delete(removed)), (1, folder(made))])
        .unwrap();
    let live = NodeId::from(&live_node_at(&world, &[made]).unwrap());
    (world, ids, live)
}

fn sends_nothing_more(world: &mut World, phase: usize) {
    let server = |world: &World| {
        (world.space().nodes().into_iter())
            .map(|node| {
                (
                    node.node_id,
                    node.parent_id,
                    node.name,
                    node.status,
                    node.meta_rev,
                )
            })
            .collect::<Vec<_>>()
    };
    let (before, seq) = (server(world), world.space().head_seq());
    world
        .phase(phase, &[(0, SCAN), (0, COMMIT), (0, COMMIT)])
        .unwrap();
    assert_eq!(server(world), before);
    assert_eq!(world.space().head_seq(), seq);
}

fn left_out_files(world: &World) -> Vec<Vec<String>> {
    let disk = &world.devices[0].disk;
    let mut paths: Vec<Vec<String>> = (disk.entries().iter())
        .filter(|(_, entry)| entry.name == "c.txt")
        .map(|(ino, _)| disk.path(*ino).expect("c.txt is not parked"))
        .collect();
    paths.sort();
    paths
}

fn one_takes_over(world: &mut World, merged: &[NodeId; 2], live: &NodeId, made: &str) -> usize {
    let problems = world.reported_problems(0);
    let reported: Vec<usize> = (0..2)
        .filter(|&i| problems.get(&merged[i]) == Some(&Stuck::MergedInto(live.clone())))
        .collect();
    let [loser] = reported[..] else {
        panic!("{problems:?}");
    };
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(held_at(world, 0, &[made]).as_ref(), Some(live));
    assert_eq!(left_out_files(world).len(), 2);
    sends_nothing_more(world, 3);
    1 - loser
}

fn made_again_around_the_commit_build(
    live: &str,
    merged: &[&str],
    settle_first: bool,
) -> (World, Vec<NodeId>, NodeId) {
    let mut made = vec![
        (0, folder("c.txt")),
        (0, user(1, 0, 2, "c.txt")),
        (0, rename(1, 0, merged[0])),
    ];
    if let [_, second] = merged {
        made.extend([
            (0, folder("b")),
            (0, user(1, 0, 3, "c.txt")),
            (0, rename(3, 0, second)),
        ]);
    }
    let phases = [vec![(0, folder(live))], made];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let ids = (merged.iter())
        .map(|name| held_at(&world, 0, &[name]).unwrap())
        .collect();
    let removed = target(&world, 1, &[live]);
    let mut steps = vec![(1, delete(removed)), (1, folder(live))];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    if settle_first {
        steps.push((0, COMMIT));
    }
    steps.extend([(0, EFFECT); 4]);
    world.phase(2, &steps).unwrap();
    let live = NodeId::from(&live_node_at(&world, &[live]).unwrap());
    (world, ids, live)
}

#[test]
fn a_live_folder_made_again_takes_a_merged_folder_over_whether_effects_run_before_or_after_the_commit_build()
 {
    for settle_first in [false, true] {
        let (mut world, _, live) = made_again_around_the_commit_build("a", &["A"], settle_first);
        assert!(world.reported_problems(0).is_empty());
        assert_eq!(held_at(&world, 0, &["a"]), Some(live));
        assert!(world.devices[0].disk.child(&Place::Root, "A").is_none());
        ino_at(&world, 0, &["a", "c.txt"]);
        sends_nothing_more(&mut world, 3);
        let (mut world, ids, live) =
            made_again_around_the_commit_build("\u{e9}", &["\u{c9}", "E\u{301}"], settle_first);
        one_takes_over(
            &mut world,
            &[ids[0].clone(), ids[1].clone()],
            &live,
            "\u{e9}",
        );
    }
}

#[test]
fn of_two_merge_folders_of_one_key_the_one_named_like_the_live_folder_made_again_takes_it_over() {
    for merged in [["\u{c9}", "E\u{301}"], ["E\u{301}", "\u{c9}"]] {
        let (mut world, ids, live) =
            merge_folders_beside_a_live_folder_made_again("\u{e9}", merged, "\u{c9}");
        let winner = one_takes_over(&mut world, &ids, &live, "\u{c9}");
        assert_eq!(merged[winner], "\u{c9}");
    }
}

#[test]
fn of_two_merge_folders_of_one_key_named_unlike_the_live_folder_made_again_the_lowest_id_takes_it_over()
 {
    for merged in [["\u{c9}", "E\u{301}"], ["E\u{301}", "\u{c9}"]] {
        let (mut world, ids, live) =
            merge_folders_beside_a_live_folder_made_again("\u{e9}", merged, "\u{e9}");
        let winner = one_takes_over(&mut world, &ids, &live, "\u{e9}");
        assert_eq!(ids[winner], ids[0].clone().min(ids[1].clone()));
        assert_eq!(
            left_out_files(&world),
            [[merged[1 - winner], "c.txt"], ["\u{e9}", "c.txt"]]
        );
    }
}

#[test]
fn a_merge_folder_left_beside_one_that_took_the_live_folder_over_uploads_once_moved_away() {
    let (mut world, ids, live) =
        merge_folders_beside_a_live_folder_made_again("\u{e9}", ["\u{c9}", "E\u{301}"], "\u{e9}");
    let loser = 1 - one_takes_over(&mut world, &ids, &live, "\u{e9}");
    let name = ["\u{c9}", "E\u{301}"][loser];
    let moved = target(&world, 0, &[name]);
    world.phase(4, &[(0, rename(moved, 0, "b"))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert_eq!(
        left_out_files(&world),
        [["b", "c.txt"], ["\u{e9}", "c.txt"]]
    );
    assert!(live_node_at(&world, &["b"]).is_some());
    no_live_file_named(&world, "c.txt");
}

#[test]
fn a_merge_folder_left_beside_one_that_took_the_live_folder_over_goes_once_deleted() {
    let (mut world, ids, live) =
        merge_folders_beside_a_live_folder_made_again("\u{e9}", ["\u{c9}", "E\u{301}"], "\u{e9}");
    let loser = 1 - one_takes_over(&mut world, &ids, &live, "\u{e9}");
    let name = ["\u{c9}", "E\u{301}"][loser];
    let removed = target(&world, 0, &[name]);
    world.phase(4, &[(0, delete(removed))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert_eq!(left_out_files(&world), [["\u{e9}", "c.txt"]]);
    no_live_file_named(&world, "c.txt");
}

#[test]
fn a_merge_folder_spelled_like_the_live_folder_made_again_in_another_form_takes_it_over_without_a_rename()
 {
    for merged in [["\u{e9}", "E\u{301}"], ["E\u{301}", "\u{e9}"]] {
        let (mut world, ids, live) =
            merge_folders_beside_a_live_folder_made_again("\u{c9}", merged, "\u{c9}");
        let loser = merged.iter().position(|name| *name == "\u{e9}").unwrap();
        assert_eq!(
            world.reported_problems(0),
            [(ids[loser].clone(), Stuck::MergedInto(live.clone()))].into()
        );
        assert_eq!(held_at(&world, 0, &["E\u{301}"]), Some(live));
        assert_eq!(
            left_out_files(&world),
            [["E\u{301}", "c.txt"], ["\u{e9}", "c.txt"]]
        );
        sends_nothing_more(&mut world, 3);
    }
}

#[test]
#[ignore = "a file that claims the name after a freed merge folder's create is built still gets the folder conflict-renamed, and its excluded child is published (GUN-1431)"]
fn a_file_made_at_the_name_after_a_freed_merge_folders_create_is_built_leaves_its_left_out_file_unsent()
 {
    let mut world = refused_beside_a(None);
    world.rule(0, &Ruling::Exclude(vec!["/A/c.txt"])).unwrap();
    world.phase(2, &[]).unwrap();
    let source = reported_merge(&world);
    let survivor = target(&world, 1, &["a"]);
    world.run(3, &[(1, rename(survivor, 0, "b"))]).unwrap();
    world.run(3, &commit(1)).unwrap();
    assert!(live_node_at(&world, &["a"]).is_none());
    world
        .run(
            3,
            &[
                (0, DELIVER),
                (0, DELIVER),
                (0, EFFECT),
                (0, EFFECT),
                (0, COMMIT),
            ],
        )
        .unwrap();
    let flight = world.devices[0]
        .engine
        .commits
        .in_flight()
        .expect("built the residue create");
    let ops = &flight.commit.ops;
    let [op] = ops.as_slice() else {
        panic!("unexpected ops: {ops:?}");
    };
    let Some(koidrive_proto::v1::op::Op::CreateDir(created)) = &op.op else {
        panic!("{op:?}");
    };
    assert_eq!(created.name, "A");
    assert_ne!(created.node_id, source.to_string());
    assert!(live_node_at(&world, &["A"]).is_none());
    world.run(3, &[(1, file("a"))]).unwrap();
    world.run(3, &commit(1)).unwrap();
    assert!(live_node_at(&world, &["a"]).is_some());
    let quiet = world.phase(3, &[(0, COMMIT)]);
    let nodes = world.space().nodes();
    let copied = nodes
        .iter()
        .find(|node| node.status == NodeStatus::Live as i32 && node.name == "c.txt");
    let conflict = nodes.iter().find(|node| {
        node.status == NodeStatus::Live as i32 && node.name.starts_with("A (conflict from")
    });
    assert!(
        copied.is_none(),
        "excluded copy: {:?}; conflict folder: {:?}; {quiet:?}",
        copied.map(|n| (&n.name, &n.parent_id)),
        conflict.map(|n| (&n.name, &n.node_id))
    );
    quiet.unwrap();
}

#[test]
fn a_kept_folder_takes_over_a_new_live_folder_of_its_name_while_a_merge_folder_beside_it_stays_reported()
 {
    let phases = [
        vec![(0, folder("a"))],
        vec![
            (0, user(1, 0, 1, "c.txt")),
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(2, 0, "A")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let source = reported_merge(&world);
    let live = target(&world, 1, &["a"]);
    world
        .phase(2, &[(1, delete(live)), (1, folder("a"))])
        .unwrap();
    let replacement = NodeId::from(live_node_at(&world, &["a"]).unwrap());
    assert_eq!(held_at(&world, 0, &["a"]), Some(replacement.clone()));
    assert_eq!(
        world.reported_problems(0).get(&source),
        Some(&Stuck::MergedInto(replacement))
    );
    ino_at(&world, 0, &["a", "c.txt"]);
    ino_at(&world, 0, &["A", "c.txt"]);
}

#[test]
fn moving_the_live_folder_into_a_reported_merge_folder_uploads_the_merge_folder_holding_it() {
    let mut world = refused_beside_a(None);
    let source = reported_merge(&world);
    let live = target(&world, 0, &["a"]);
    let merged = folder_number(&world, 0, &["A"]);
    world.phase(2, &[(0, rename(live, merged, "a"))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    let uploaded = live_node_at(&world, &["A"]).unwrap();
    assert_ne!(uploaded, source.to_string());
    assert!(live_node_at(&world, &["A", "a"]).is_some());
    ino_at(&world, 0, &["A", "c.txt"]);
    ino_at(&world, 1, &["A", "a"]);
}

#[test]
fn a_live_folder_made_at_the_name_after_the_live_folder_goes_takes_a_reported_merge_folder_over() {
    let mut world = refused_beside_a(None);
    reported_merge(&world);
    let live = target(&world, 1, &["a"]);
    world
        .phase(2, &[(1, delete(live)), (1, folder("A"))])
        .unwrap();
    assert!(world.reported_problems(0).is_empty());
    let made = live_node_at(&world, &["A"]).unwrap();
    assert_eq!(held_at(&world, 0, &["A"]), Some(NodeId::from(made)));
    assert_eq!(live_node_at(&world, &["a"]), None);
    ino_at(&world, 0, &["A", "c.txt"]);
}

#[test]
fn two_devices_with_the_same_merge_folder_end_with_one_folder_once_the_live_folder_goes() {
    let phases = [
        vec![(0, folder("a"))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(1, 0, "A")),
            (1, folder("c.txt")),
            (1, user(1, 0, 2, "c.txt")),
            (1, rename(1, 0, "A")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec!["*.txt"], vec![]], &phases).unwrap();
    let live = target(&world, 2, &["a"]);
    world.phase(2, &[(2, delete(live))]).unwrap();
    let folder = live_node_at(&world, &["A"]).unwrap();
    for device in 0..2 {
        assert!(world.reported_problems(device).is_empty());
        assert_eq!(
            held_at(&world, device, &["A"]),
            Some(NodeId::from(folder.clone()))
        );
        ino_at(&world, device, &["A", "c.txt"]);
    }
    assert_eq!(
        world
            .space()
            .nodes()
            .iter()
            .filter(|node| node.name.starts_with('A'))
            .count(),
        1
    );
}

#[test]
fn a_nested_merge_folder_uploads_under_its_freed_parent_and_the_deleted_live_folder_stays_deleted()
{
    let phases = [
        vec![(0, folder("a")), (0, user(0, 0, 1, "b"))],
        vec![
            (0, folder("c.txt")),
            (0, user(0, 0, 3, "b")),
            (0, user(1, 0, 4, "c.txt")),
            (0, rename(2, 0, "A")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    assert_eq!(world.reported_problems(0).len(), 2);
    let live = target(&world, 1, &["a"]);
    world.phase(2, &[(1, delete(live))]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert_eq!(live_node_at(&world, &["a"]), None);
    assert!(live_node_at(&world, &["A", "b"]).is_some());
    ino_at(&world, 0, &["A", "b", "c.txt"]);
    ino_at(&world, 1, &["A", "b"]);
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

#[test]
fn a_park_queued_before_its_folder_falls_behind_a_merged_folder_keeps_it_in_place_until_the_chain_goes()
 {
    let mut world = refused_beside_a(None);
    let merged = reported_merge(&world);
    world
        .phase(2, &[(1, folder("b")), (1, folder("ß")), (1, folder("É"))])
        .unwrap();
    let [sharp, accent] = ["ß", "É"].map(|name| held_at(&world, 0, &[name]).unwrap());
    for (from, to) in [("ß", "c.txt"), ("É", "ß"), ("c.txt", "É")] {
        let entry = target(&world, 1, &[from]);
        world.run(3, &[(1, rename(entry, 0, to))]).unwrap();
        world.run(3, &commit(1)).unwrap();
    }
    assert_eq!(live_node_at(&world, &["É"]), Some(sharp.to_string()));
    assert_eq!(live_node_at(&world, &["ß"]), Some(accent.to_string()));
    world.run(3, &[(0, DELIVER); 8]).unwrap();
    world.run(3, &[(0, EFFECT)]).unwrap();
    let [park] = world.devices[0].engine.effects.in_flight() else {
        panic!(
            "expected a park for the swap, got {:?}",
            world.devices[0].engine.effects.in_flight()
        );
    };
    assert!(
        matches!(
            &park.effect,
            Effect::Move {
                to: koidrive_engine::effects::Location::Parked(_),
                ..
            }
        ),
        "{park:?}"
    );
    let parked = park.node.clone();
    let visible = if parked == sharp {
        "ß"
    } else {
        assert_eq!(parked, accent);
        "É"
    };
    let ino = ino_at(&world, 0, &[visible]);
    let survivor = target(&world, 1, &["a"]);
    world.run(4, &[(1, delete(survivor))]).unwrap();
    world.run(4, &commit(1)).unwrap();
    for (from, to) in [("b", "A"), (visible, "b")] {
        let entry = target(&world, 1, &[from]);
        world.run(4, &[(1, rename(entry, 0, to))]).unwrap();
        world.run(4, &commit(1)).unwrap();
    }
    world.run(4, &[(0, DELIVER); 8]).unwrap();
    assert_eq!(
        world.reported_problems(0).get(&parked),
        Some(&Stuck::Behind(merged.clone()))
    );
    world.run(4, &[(0, EFFECT)]).unwrap();
    assert_eq!(
        world.devices[0].disk.child(&Place::Root, visible),
        Some(ino),
        "an in-flight park hid a folder already reported as waiting behind {merged}"
    );
    assert!(world.devices[0].engine.effects.blocked().is_empty());
    let holder = target(&world, 1, &["A"]);
    world.run(5, &[(1, delete(holder))]).unwrap();
    world.run(5, &commit(1)).unwrap();
    world.phase(6, &[]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    let back = live_node_at(&world, &["b"]).unwrap();
    assert_eq!(held_at(&world, 0, &["b"]), Some(NodeId::from(back)));
}

fn parked_ids_on(world: &World, d: usize) -> Vec<NodeId> {
    (world.devices[d].engine.trees.local().iter())
        .filter(|(_, node)| node.parked)
        .map(|(id, _)| id.clone())
        .collect()
}

#[test]
fn a_park_planned_in_a_swap_does_not_apply_once_the_chain_reaches_a_merged_folder() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 0 is quiet, but nodes [\"01a1c2c0-39a0-7735-80db-d7e668859a70\", \"01a1c2c0-39a0-7cdc-8389-c4663807ce68\"] wait for names that entries its rules leave out hold",
    );
    let mut world = converge_excluding(
        &[vec!["*.txt"], vec![]],
        &[vec![
            (0, folder("a")),
            (0, folder("b")),
            (0, folder("\u{df}")),
        ]],
    )
    .unwrap();
    let mut steps = vec![
        (0, folder("c.txt")),
        (0, user(1, 0, 4, "c.txt")),
        (0, rename(3, 0, "A")),
    ];
    world.phase(1, &steps).unwrap();
    let (tb, tss) = (target(&world, 1, &["b"]), target(&world, 1, &["\u{df}"]));
    steps = vec![
        (1, rename(tb, 0, "\u{e9}")),
        (1, rename(tss, 0, "b")),
        (1, rename(tb, 0, "\u{df}")),
    ];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, EFFECT));
    world.run(2, &steps).unwrap();
    let flight = world.devices[0].engine.effects.in_flight().to_vec();
    let parked = flight
        .iter()
        .find(|r| {
            matches!(
                r.effect,
                koidrive_engine::effects::Effect::Move {
                    to: koidrive_engine::effects::Location::Parked(_),
                    ..
                }
            )
        })
        .expect("the swap plans a park")
        .node
        .clone();
    let other = ["b", "\u{df}"]
        .into_iter()
        .find(|n| held_at(&world, 0, &[n]) != Some(parked.clone()))
        .unwrap();
    let moving = live_node_at(&world, &[other]).unwrap();
    world
        .run(3, &[(1, delete(target(&world, 1, &["a"])))])
        .unwrap();
    world
        .run(3, &[(1, rename(target(&world, 1, &[other]), 0, "A"))])
        .unwrap();
    world.run(3, &commit(1)).unwrap();
    assert_eq!(live_node_at(&world, &["A"]), Some(moving.clone()));
    steps = vec![(0, DELIVER); 8];
    steps.push((0, EFFECT));
    world.run(3, &steps).unwrap();
    world.phase(4, &[]).unwrap();
    let report = world.reported_problems(0);
    let held = Stuck::MergedInto(moving.into());
    let merged: Vec<&NodeId> = (report.iter())
        .filter(|(_, reason)| **reason == held)
        .map(|(id, _)| id)
        .collect();
    let [merged] = merged[..] else {
        panic!("{report:?}");
    };
    assert_eq!(report.len(), 3, "{report:?}");
    for (id, reason) in &report {
        assert!(
            id == merged || *reason == Stuck::Behind(merged.clone()),
            "{report:?}"
        );
    }
    assert!(!parked_ids_on(&world, 0).contains(&parked));
}

fn swap_beside_an_excluded_file() -> (World, NodeId, NodeId) {
    let mut world = converge_excluding(
        &[vec!["*.txt"], vec![]],
        &[vec![
            (0, folder("b")),
            (0, folder("\u{df}")),
            (0, file("c.txt")),
        ]],
    )
    .unwrap();
    let (tb, tss) = (target(&world, 1, &["b"]), target(&world, 1, &["\u{df}"]));
    let mut steps = vec![
        (1, rename(tb, 0, "\u{e9}")),
        (1, rename(tss, 0, "b")),
        (1, rename(tb, 0, "\u{df}")),
    ];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, EFFECT));
    world.run(2, &steps).unwrap();
    let [park] = world.devices[0].engine.effects.in_flight() else {
        panic!("expected one park for the swap");
    };
    assert!(
        matches!(
            park.effect,
            Effect::Move {
                to: koidrive_engine::effects::Location::Parked(_),
                ..
            }
        ),
        "{park:?}"
    );
    let parked = park.node.clone();
    let other = (["b", "\u{df}"].into_iter())
        .find_map(|name| held_at(&world, 0, &[name]).filter(|id| *id != parked))
        .unwrap();
    (world, parked, other)
}

fn rename_onto_the_excluded_file(world: &mut World, phase: usize, id: &NodeId) {
    let name = (world.devices[1].engine.trees.local().get(id))
        .unwrap()
        .shape
        .name
        .clone();
    let mut steps = vec![(1, rename(target(world, 1, &[&name]), 0, "c.txt"))];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.run(phase, &steps).unwrap();
}

#[test]
fn a_park_planned_in_a_swap_does_not_apply_once_the_rules_leave_out_its_folder_or_the_one_it_waits_on()
 {
    for renamed_parked in [true, false] {
        let (mut world, parked, other) = swap_beside_an_excluded_file();
        let (renamed, waiting) = match renamed_parked {
            true => (parked, other),
            false => (other, parked),
        };
        rename_onto_the_excluded_file(&mut world, 3, &renamed);
        world.run(3, &[(0, EFFECT)]).unwrap();
        assert!(parked_ids_on(&world, 0).is_empty(), "{renamed_parked}");
        let _red = expect_red(
            Check::NameLeftOut,
            &format!(
                "device 0 is quiet, but nodes [{:?}] wait for names that entries its rules leave out hold",
                waiting.to_string()
            ),
        );
        world.phase(4, &[]).unwrap();
        assert!(parked_ids_on(&world, 0).is_empty(), "{renamed_parked}");
        assert_eq!(
            world.reported_problems(0),
            [(waiting, Stuck::HeldBy(renamed))].into(),
            "{renamed_parked}"
        );
    }
}

#[test]
fn a_swap_at_a_merged_folders_name_key_keeps_both_folders_in_place() {
    let _red = expect_red(
        Check::NameLeftOut,
        "device 0 is quiet, but nodes [\"01a1c2c0-39a0-7510-83bc-38d6c6b89fe4\", \"01a1c2c0-39a0-796a-8df8-fb6da7904e2f\"] wait for names that entries its rules leave out hold",
    );
    let mut world = refused_beside_a(None);
    let merged = reported_merge(&world);
    world.phase(2, &[(1, folder("b"))]).unwrap();
    let a = held_at(&world, 0, &["a"]).unwrap();
    let b = held_at(&world, 0, &["b"]).unwrap();
    let a_ino = ino_at(&world, 0, &["a"]);
    let b_ino = ino_at(&world, 0, &["b"]);
    let ta = target(&world, 1, &["a"]);
    let tb = target(&world, 1, &["b"]);
    let mut steps = vec![
        (1, rename(ta, 0, "É")),
        (1, rename(tb, 0, "a")),
        (1, rename(ta, 0, "b")),
    ];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.run(3, &steps).unwrap();
    assert_eq!(live_node_at(&world, &["a"]), Some(b.to_string()));
    assert_eq!(live_node_at(&world, &["b"]), Some(a.to_string()));
    assert_eq!(
        world.reported_problems(0).get(&merged),
        Some(&Stuck::MergedInto(b.clone()))
    );
    world.run(3, &[(0, EFFECT), (0, EFFECT)]).unwrap();
    world.phase(4, &[]).unwrap();
    let disk = &world.devices[0].disk;
    assert_eq!(
        [disk.child(&Place::Root, "a"), disk.child(&Place::Root, "b")],
        [Some(a_ino), Some(b_ino)],
        "a swap cannot finish while the merged folder still holds the name key: {:?}",
        world.reported_problems(0),
    );
    let report = world.reported_problems(0);
    for id in [a, b] {
        assert_eq!(report.get(&id), Some(&Stuck::Behind(merged.clone())));
    }
}

#[test]
fn a_parked_file_goes_home_past_an_excluded_folder_when_its_exact_server_name_is_a_merged_folder() {
    let mut world = refused_beside_a(None);
    let merged = reported_merge(&world);
    world.phase(2, &[(1, file("b")), (1, file("ß"))]).unwrap();
    let excluded = held_at(&world, 0, &["a"]).unwrap();
    let a = target(&world, 1, &["a"]);
    let b = target(&world, 1, &["b"]);
    let sharp = target(&world, 1, &["ß"]);
    let mut steps = vec![
        (1, rename(b, 0, "É")),
        (1, rename(sharp, 0, "b")),
        (1, rename(b, 0, "ß")),
    ];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.run(3, &steps).unwrap();
    world.run(3, &[(0, EFFECT)]).unwrap();
    let [park] = world.devices[0].engine.effects.in_flight() else {
        panic!("expected one park for the swap");
    };
    assert!(
        matches!(
            park.effect,
            Effect::Move {
                to: koidrive_engine::effects::Location::Parked(_),
                ..
            }
        ),
        "{park:?}"
    );
    let parked = park.node.clone();
    let home = park.after.as_ref().unwrap().name.clone();
    let old_ino = ino_at(&world, 0, &[&home]);
    let other = if home == "b" {
        "ß"
    } else {
        assert_eq!(home, "ß");
        "b"
    };
    let p = target(&world, 1, &[other]);
    let d = target(&world, 1, &[&home]);
    world.run(3, &[(0, EFFECT)]).unwrap();
    assert!(parked_ids_on(&world, 0).contains(&parked));
    let mut steps = vec![
        (1, rename(a, 0, "c.txt")),
        (1, rename(p, 0, "A")),
        (1, rename(d, 0, other)),
    ];
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.run(4, &steps).unwrap();
    assert_eq!(live_node_at(&world, &["A"]), Some(parked.to_string()));
    assert_eq!(held_at(&world, 0, &["a"]), Some(excluded.clone()));
    assert!(world.devices[0].engine.trees.excluded(&excluded));
    assert_eq!(
        world.reported_problems(0).get(&merged),
        Some(&Stuck::MergedInto(parked.clone()))
    );
    world.run(4, &[(0, EFFECT), (0, EFFECT)]).unwrap();
    assert_eq!(
        world.devices[0].disk.child(&Place::Root, &home),
        Some(old_ino),
        "the excluded folder at the server key should send the parked file back to its free home: {:?}",
        world.reported_problems(0)
    );
}

fn moved_out_below_a_merged_folder(offline: Vec<(usize, Step)>) -> (World, NodeId, u128) {
    let phases = [vec![
        (0, folder("b")),
        (0, user(0, 0, 1, "ß")),
        (0, user(1, 0, 2, "ß")),
    ]];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let live = held_at(&world, 0, &["b", "ß"]).unwrap();
    let file = ino_at(&world, 0, &["b", "ß", "ß"]);
    let b = target(&world, 0, &["b"]);
    let moved = target(&world, 1, &["b", "ß"]);
    let mut steps = vec![
        (0, Step::Drop),
        (0, folder("ss")),
        (0, user(1, 0, 3, "c.txt")),
        (0, rename(b, 3, "b")),
    ];
    steps.extend(offline);
    steps.push((1, rename(moved, 0, "ss")));
    steps.extend(commit(1));
    world.run(2, &steps).unwrap();
    (world, live, file)
}

fn live_children(world: &World, path: &[&str]) -> Vec<String> {
    let Some(parent) = live_node_at(world, path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = (world.space().nodes().into_iter())
        .filter(|node| node.status == NodeStatus::Live as i32 && node.parent_id == parent)
        .map(|node| node.name)
        .collect();
    names.sort();
    names
}

fn taken_over(world: &mut World, live: &NodeId, file: u128) {
    assert_eq!(held_at(world, 0, &["ss"]).as_ref(), Some(live));
    assert_eq!(ino_at(world, 0, &["ss", "ß"]), file);
    ino_at(world, 0, &["ss", "c.txt"]);
    let b = ino_at(world, 0, &["ss", "b"]);
    assert_eq!(world.devices[0].disk.child(&Place::In(b), "ß"), None);
    assert_eq!(live_node_at(world, &["ss"]), Some(live.to_string()));
    assert_eq!(live_children(world, &["ss"]), ["b", "ß"]);
    assert!(live_children(world, &["ss", "b"]).is_empty());
    sends_nothing_more(world, 9);
}

#[test]
fn a_merged_folder_takes_over_a_live_folder_another_device_moved_out_from_below_it() {
    let (mut world, live, file) = moved_out_below_a_merged_folder(vec![]);
    world.phase(3, &[(0, Step::Connect)]).unwrap();
    assert!(world.reported_problems(0).is_empty());
    assert!(world.folder_effects[0].iter().any(|record| {
        matches!(record.effect, Effect::RemoveDir { .. }) && record.node != live
    }));
    taken_over(&mut world, &live, file);
}

#[test]
fn a_file_made_in_the_nested_copy_before_the_takeover_keeps_it_there_and_uploads_it_under_a_new_id()
{
    let (mut world, live, file) = moved_out_below_a_merged_folder(vec![]);
    world.run(3, &[(0, Step::Connect)]).unwrap();
    for _ in 0..200 {
        if !world.devices[0].engine.commits.aliases().is_empty() {
            break;
        }
        world.run(3, &[(0, Step::Work(0))]).unwrap();
    }
    assert!(!world.devices[0].engine.commits.aliases().is_empty());
    let nested = folder_number(&world, 0, &["ss", "b", "ß"]);
    world.phase(4, &[(0, user(1, 0, nested, "a"))]).unwrap();
    assert_eq!(held_at(&world, 0, &["ss"]), Some(live.clone()));
    assert_eq!(ino_at(&world, 0, &["ss", "ß"]), file);
    let copy = live_node_at(&world, &["ss", "b", "ß"]).unwrap();
    assert_ne!(copy, live.to_string());
    assert_eq!(live_children(&world, &["ss", "b", "ß"]), ["a"]);
    assert_eq!(live_children(&world, &["ss", "b"]), ["ß"]);
    assert_eq!(live_children(&world, &["ss"]), ["b", "ß"]);
    sends_nothing_more(&mut world, 9);
}

#[test]
fn a_nested_copy_that_holds_a_left_out_entry_stays_with_it_after_a_takeover() {
    let (mut world, live, file) =
        moved_out_below_a_merged_folder(vec![(0, user(1, 0, 2, "c.txt"))]);
    world.phase(3, &[(0, Step::Connect)]).unwrap();
    assert_eq!(held_at(&world, 0, &["ss"]), Some(live.clone()));
    assert_eq!(ino_at(&world, 0, &["ss", "ß"]), file);
    ino_at(&world, 0, &["ss", "b", "ß", "c.txt"]);
    ino_at(&world, 0, &["ss", "c.txt"]);
    assert_eq!(live_children(&world, &["ss"]), ["b", "ß"]);
    assert!(live_children(&world, &["ss", "b"]).is_empty());
}

#[test]
fn a_file_made_in_the_merged_folder_at_the_name_of_a_moved_up_entry_is_kept_as_a_conflict_copy() {
    let (mut world, live, file) = moved_out_below_a_merged_folder(vec![(0, user(1, 0, 3, "ß"))]);
    world.phase(3, &[(0, Step::Connect)]).unwrap();
    assert_eq!(held_at(&world, 0, &["ss"]), Some(live.clone()));
    assert_eq!(ino_at(&world, 0, &["ss", "ß"]), file);
    assert_eq!(
        live_children(&world, &["ss"]),
        ["b", "ß", "ß (conflict from device-0 2026-11-09)"]
    );
}

#[test]
fn a_file_written_into_the_nested_copy_before_its_removal_applies_keeps_it_and_uploads() {
    let (mut world, live, file) = moved_out_below_a_merged_folder(vec![]);
    world.run(3, &[(0, Step::Connect)]).unwrap();
    let removes = |world: &World| {
        (world.devices[0].engine.effects.in_flight().iter())
            .any(|record| matches!(record.effect, Effect::RemoveDir { .. }))
    };
    for _ in 0..200 {
        if removes(&world) {
            break;
        }
        world.run(3, &[(0, Step::Work(0))]).unwrap();
    }
    assert!(removes(&world));
    let nested = folder_number(&world, 0, &["ss", "b", "ß"]);
    world.phase(4, &[(0, user(1, 0, nested, "a"))]).unwrap();
    assert_eq!(held_at(&world, 0, &["ss"]), Some(live.clone()));
    assert_eq!(ino_at(&world, 0, &["ss", "ß"]), file);
    ino_at(&world, 0, &["ss", "b", "ß", "a"]);
    let copy = live_node_at(&world, &["ss", "b", "ß"]).unwrap();
    assert_ne!(copy, live.to_string());
    assert_eq!(live_children(&world, &["ss", "b", "ß"]), ["a"]);
    assert_eq!(live_children(&world, &["ss", "b"]), ["ß"]);
    assert_eq!(live_children(&world, &["ss"]), ["b", "ß"]);
    sends_nothing_more(&mut world, 9);
}

#[test]
fn a_takeover_of_a_live_folder_below_a_merged_folder_converges_after_a_crash_at_any_step() {
    for k in 0..40 {
        let (mut world, live, file) = moved_out_below_a_merged_folder(vec![]);
        world.run(3, &[(0, Step::Connect)]).unwrap();
        world.run(3, &vec![(0, Step::Work(0)); k]).unwrap();
        let crash = match world.devices[0].engine.effects.in_flight().is_empty() {
            true => Step::Crash,
            false => CRASH,
        };
        world
            .phase(4, &[(0, crash), (0, Step::Connect)])
            .unwrap_or_else(|failure| panic!("crash after {k} steps: {failure:?}"));
        taken_over(&mut world, &live, file);
    }
}

#[test]
fn the_merged_folder_named_like_the_live_folder_takes_it_over_from_below_another_at_its_key_which_stays_reported()
 {
    let phases = [vec![
        (0, folder("b")),
        (0, user(0, 0, 1, "ß")),
        (0, user(1, 0, 2, "ß")),
    ]];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let live = held_at(&world, 0, &["b", "ß"]).unwrap();
    let file = ino_at(&world, 0, &["b", "ß", "ß"]);
    let b = target(&world, 0, &["b"]);
    let moved = target(&world, 1, &["b", "ß"]);
    let offline = [
        (0, Step::Drop),
        (0, folder("ss")),
        (0, rename(b, 3, "b")),
        (0, folder("ß")),
    ];
    world.run(2, &offline).unwrap();
    let containing = folder_number(&world, 0, &["ss"]);
    let preferred = folder_number(&world, 0, &["ß"]);
    let mut steps = vec![
        (0, user(1, 0, containing, "c.txt")),
        (0, user(1, 0, preferred, "c.txt")),
        (1, rename(moved, 0, "ß")),
    ];
    steps.extend(commit(1));
    world.run(2, &steps).unwrap();
    world.phase(3, &[(0, Step::Connect)]).unwrap();
    assert_eq!(live_node_at(&world, &["ß"]), Some(live.to_string()));
    assert_eq!(held_at(&world, 0, &["ß"]), Some(live.clone()));
    assert_eq!(ino_at(&world, 0, &["ß", "ß"]), file);
    ino_at(&world, 0, &["ss", "c.txt"]);
    ino_at(&world, 0, &["ß", "c.txt"]);
    assert!(live_children(&world, &["ss", "b"]).is_empty());
    let held = held_at(&world, 0, &["ss"]).unwrap();
    assert_eq!(
        world.reported_problems(0),
        [(held, Stuck::MergedInto(live))].into()
    );
    sends_nothing_more(&mut world, 4);
}

fn nested_with_two_files(swap: bool) -> (World, NodeId, [NodeId; 3], [u128; 2]) {
    let phases = [vec![
        (0, folder("b")),
        (0, user(0, 0, 1, "ß")),
        (0, user(0, 0, 2, "É")),
        (0, user(1, 0, 3, "ß")),
        (0, user(1, 0, 3, "a")),
    ]];
    let mut world = converge_excluding(&[vec!["/ss/c.txt"], vec![]], &phases).unwrap();
    let live = held_at(&world, 0, &["b", "ß"]).unwrap();
    let nodes = [["É"].as_slice(), &["É", "ß"], &["É", "a"]]
        .map(|path| held_at(&world, 0, &[&["b", "ß"], path].concat()).unwrap());
    let files =
        [["É", "ß"], ["É", "a"]].map(|path| ino_at(&world, 0, &[&["b", "ß"], &path[..]].concat()));
    let b = target(&world, 0, &["b"]);
    let moved = target(&world, 1, &["b", "ß"]);
    let mut steps = vec![
        (0, Step::Drop),
        (0, folder("ss")),
        (0, user(1, 0, 4, "c.txt")),
        (0, rename(b, 4, "b")),
    ];
    if swap {
        let [a, s] = [["É", "a"], ["É", "ß"]]
            .map(|path| target(&world, 1, &[&["b", "ß"], &path[..]].concat()));
        let folder = folder_number(&world, 1, &["b", "ß", "É"]);
        steps.extend([
            (1, rename(a, folder, "b")),
            (1, rename(s, folder, "a")),
            (1, rename(a, folder, "ß")),
        ]);
    }
    steps.push((1, rename(moved, 0, "ss")));
    steps.extend(commit(1));
    world.run(2, &steps).unwrap();
    (world, live, nodes, files)
}

fn merged_and_unsettled(world: &mut World) {
    world.run(3, &[(0, Step::Connect)]).unwrap();
    for _ in 0..200 {
        if !world.devices[0].engine.commits.aliases().is_empty() {
            break;
        }
        world.run(3, &[(0, Step::Work(0))]).unwrap();
    }
    assert!(!world.devices[0].engine.commits.aliases().is_empty());
}

fn lost(
    world: &World,
    live: &NodeId,
    nodes: &[NodeId; 3],
    files: [u128; 2],
    deleted: &[usize],
) -> Vec<String> {
    let mut lost = Vec::new();
    if held_at(world, 0, &["ss"]).as_ref() != Some(live) {
        lost.push("the live ID is not at ss".to_string());
    }
    let server = world.space().nodes();
    for (i, id) in nodes.iter().enumerate() {
        let on_server = (server.iter())
            .any(|node| node.node_id == *id && node.status == NodeStatus::Live as i32);
        if on_server == deleted.contains(&i) {
            lost.push(format!("node {i} live on the server: {on_server}"));
        }
    }
    for (i, ino) in files.into_iter().enumerate() {
        let on_disk = (world.devices[0].disk.entries().keys()).any(|at| *at == ino);
        if !on_disk && !deleted.contains(&(i + 1)) {
            lost.push(format!("file {i} is gone from the disk"));
        }
    }
    lost
}

fn after_a_pending_change(change: &str, after: bool) -> Vec<String> {
    let (mut world, live, nodes, files) = nested_with_two_files(false);
    if after {
        merged_and_unsettled(&mut world);
    }
    let a = target(&world, 0, &["ss", "b", "ß", "É", "a"]);
    let folder = target(&world, 0, &["ss", "b", "ß", "É"]);
    let [copy, inside, merged] = [&["ss", "b", "ß"][..], &["ss", "b", "ß", "É"], &["ss"]]
        .map(|path| folder_number(&world, 0, path));
    let original = world.devices[0].disk.entries()[&files[1]].content.clone();
    let moved = match after {
        true => vec!["ss", "b", "ß", "a"],
        false => vec!["ss", "a"],
    };
    let (step, deleted, at) = match change {
        "file deleted" => (delete(a), &[2][..], None),
        "file edited" => (overwrite(a), &[][..], None),
        "file moved within the copy" => (rename(a, copy, "a"), &[][..], Some((moved, 2))),
        "file moved out of the copy" => (rename(a, 0, "a"), &[][..], Some((vec!["a"], 2))),
        "file moved into the merged folder" => {
            (rename(a, merged, "a"), &[][..], Some((vec!["ss", "a"], 2)))
        }
        "file created" => (user(1, 0, inside, "b"), &[][..], None),
        "folder deleted" => (delete(folder), &[0, 1, 2][..], None),
        "folder renamed" => (rename(folder, copy, "a"), &[][..], Some((moved, 0))),
        "folder moved out of the copy" => (rename(folder, 0, "É"), &[][..], Some((vec!["É"], 0))),
        "folder moved into the merged folder" => (
            rename(folder, merged, "a"),
            &[][..],
            Some((vec!["ss", "a"], 0)),
        ),
        _ => unreachable!(),
    };
    world.run(if after { 4 } else { 2 }, &[(0, step)]).unwrap();
    let written = (change == "file edited")
        .then(|| world.devices[0].disk.entries()[&files[1]].content.clone());
    let quiet = match after {
        true => world.phase(4, &[]),
        false => world.phase(3, &[(0, Step::Connect)]),
    };
    let mut failed = lost(&world, &live, &nodes, files, deleted);
    if let Err(failure) = quiet {
        failed.push(format!("{failure:?}"));
    }
    if let Some((path, node)) = at
        && live_node_at(&world, &path) != Some(nodes[node].to_string())
    {
        failed.push(format!(
            "{path:?} on the server is {:?}",
            live_node_at(&world, &path)
        ));
    }
    let content = |device: usize, path: &[&str]| {
        let disk = &world.devices[device].disk;
        (disk.entries().get(&ino_at(&world, device, path))).and_then(|entry| entry.content.clone())
    };
    if let Some(written) = written {
        let kept = (world.devices[0].disk.entries().get(&files[1]))
            .and_then(|entry| entry.content.clone());
        if written == original || kept != written || content(1, &["ss", "É", "a"]) != written {
            failed.push("the edit did not reach both devices".to_string());
        }
    }
    if change == "file created"
        && (live_node_at(&world, &["ss", "É", "b"]).is_none()
            || content(0, &["ss", "É", "b"]).is_none())
    {
        failed.push(format!(
            "the new file is not at [ss, É, b]: {:?}",
            live_node_at(&world, &["ss", "É", "b"])
        ));
    }
    failed
}

#[test]
fn a_change_pending_in_the_nested_copy_before_or_after_the_merge_result_survives_the_takeover() {
    let mut failed = Vec::new();
    for change in [
        "file deleted",
        "file edited",
        "file moved within the copy",
        "file moved out of the copy",
        "file moved into the merged folder",
        "file created",
        "folder deleted",
        "folder renamed",
        "folder moved out of the copy",
        "folder moved into the merged folder",
    ] {
        for after in [false, true] {
            let cell = after_a_pending_change(change, after);
            if !cell.is_empty() {
                failed.push(format!(
                    "{change}, after the merge result: {after}: {cell:?}"
                ));
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn a_takeover_loses_nothing_when_the_nested_copy_is_parked_rejected_or_left_out_by_a_rule_change() {
    for state in ["parked", "rejected", "excluded", "edited, then excluded"] {
        let (mut world, live, nodes, files) = nested_with_two_files(state == "parked");
        if state == "parked" {
            world.run(3, &[(0, Step::Connect)]).unwrap();
        } else {
            merged_and_unsettled(&mut world);
            let a = target(&world, 0, &["ss", "b", "ß", "É", "a"]);
            let refused = Step::Commit(Reply::Refused(error::Code::LimitExceeded));
            match state {
                "rejected" => world.run(
                    4,
                    &[(0, overwrite(a)), (0, SCAN), (0, refused), (0, refused)],
                ),
                "edited, then excluded" => world.run(4, &[(0, overwrite(a))]),
                _ => Ok(()),
            }
            .unwrap();
            if state.ends_with("excluded") {
                world
                    .rule(0, &Ruling::Exclude(vec!["/ss/c.txt", "a"]))
                    .unwrap();
            }
        }
        for _ in 0..400 {
            if held_at(&world, 0, &["ss"]).as_ref() == Some(&live) {
                break;
            }
            world.run(5, &[(0, Step::Work(0))]).unwrap();
        }
        world
            .phase(6, &[(0, Step::Work(0))])
            .unwrap_or_else(|failure| panic!("{state}: {failure:?}"));
        assert_eq!(
            lost(&world, &live, &nodes, files, &[]),
            Vec::<String>::new(),
            "{state}"
        );
    }
}

#[test]
fn a_takeover_goes_ahead_while_an_edit_below_the_nested_copy_is_refused_and_loses_nothing_once_the_refusals_stop()
 {
    let (mut world, live, nodes, files) = nested_with_two_files(false);
    merged_and_unsettled(&mut world);
    let a = target(&world, 0, &["ss", "b", "ß", "É", "a"]);
    world.run(4, &[(0, overwrite(a))]).unwrap();
    let edited = world.devices[0].disk.entries()[&files[1]].content.clone();
    assert!(edited.is_some());
    let refused = Step::Commit(Reply::Refused(error::Code::LimitExceeded));
    world
        .run(
            4,
            &[
                (0, SCAN),
                (0, refused),
                (0, refused),
                (0, refused),
                (0, refused),
            ],
        )
        .unwrap();
    assert_eq!(held_at(&world, 0, &["ss"]), Some(live.clone()));
    assert!(world.devices[0].engine.commits.in_flight().is_some());
    world.phase(5, &[(0, Step::Work(0))]).unwrap();
    assert_eq!(
        lost(&world, &live, &nodes, files, &[]),
        Vec::<String>::new()
    );
    let content = |device: usize| {
        let disk = &world.devices[device].disk;
        disk.entries()[&ino_at(&world, device, &["ss", "É", "a"])]
            .content
            .clone()
    };
    assert_eq!(content(0), edited);
    assert_eq!(content(1), edited);
}

fn renamed_elsewhere_then(world: &mut World, made: Step) {
    let survivor = target(world, 1, &["a"]);
    world.run(3, &[(1, rename(survivor, 0, "b"))]).unwrap();
    world.run(3, &commit(1)).unwrap();
    let steps = [(0, DELIVER), (0, DELIVER), (0, EFFECT), (0, EFFECT)];
    world.run(3, &steps).unwrap();
    world.run(3, &[(0, made), (0, SCAN)]).unwrap();
}

fn renamed_here_then(world: &mut World, made: Step) {
    let live = target(world, 0, &["a"]);
    world
        .run(3, &[(0, rename(live, 0, "b")), (0, made), (0, SCAN)])
        .unwrap();
}

fn waits_while_a_local_entry_holds_the_freed_name_key(free: fn(&mut World, Step), made: Step) {
    let mut world = refused_beside_a(None);
    world.rule(0, &Ruling::Exclude(vec!["/A/c.txt"])).unwrap();
    world.phase(2, &[]).unwrap();
    let source = reported_merge(&world);
    free(&mut world, made);
    world.phase(4, &[]).unwrap();
    let leaked = |world: &World| (world.space().nodes().iter()).any(|node| node.name == "c.txt");
    assert!(!leaked(&world));
    let holder = NodeId::from(live_node_at(&world, &["a"]).unwrap());
    assert_eq!(
        world.reported_problems(0),
        [(source.clone(), Stuck::MergedInto(holder))].into()
    );
    assert!(live_node_at(&world, &["A"]).is_none());
    world
        .phase(5, &[(0, delete(target(&world, 0, &["a"])))])
        .unwrap();
    assert!(!leaked(&world));
    let created = live_node_at(&world, &["A"]).unwrap();
    assert_ne!(created, source.to_string());
    assert_eq!(held_at(&world, 0, &["A"]), Some(NodeId::from(created)));
    ino_at(&world, 0, &["A", "c.txt"]);
    assert!(world.reported_problems(0).is_empty());
    sends_nothing_more(&mut world, 6);
}

#[test]
fn a_reported_merge_folder_waits_while_a_local_file_holds_its_freed_name_key() {
    waits_while_a_local_entry_holds_the_freed_name_key(renamed_elsewhere_then, file("a"));
}

#[test]
fn a_reported_merge_folder_waits_while_a_local_folder_holds_its_freed_name_key() {
    waits_while_a_local_entry_holds_the_freed_name_key(renamed_elsewhere_then, folder("a"));
}

#[test]
fn a_reported_merge_folder_waits_while_a_local_file_takes_the_name_of_the_live_folder_renamed_here()
{
    waits_while_a_local_entry_holds_the_freed_name_key(renamed_here_then, file("a"));
}

#[test]
fn a_reported_merge_folder_waits_while_a_local_folder_takes_the_name_of_the_live_folder_renamed_here()
 {
    waits_while_a_local_entry_holds_the_freed_name_key(renamed_here_then, folder("a"));
}

#[test]
fn a_nested_copy_renamed_to_another_spelling_of_its_name_is_still_taken_over() {
    let phases = [vec![
        (0, folder("b")),
        (0, user(0, 0, 1, "\u{c9}")),
        (0, user(1, 0, 2, "\u{df}")),
    ]];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let live = held_at(&world, 0, &["b", "\u{c9}"]).unwrap();
    let file = ino_at(&world, 0, &["b", "\u{c9}", "\u{df}"]);
    let moved = target(&world, 1, &["b", "\u{c9}"]);
    world.run(2, &[(0, Step::Drop)]).unwrap();
    let nested = target(&world, 0, &["b", "\u{c9}"]);
    let b = folder_number(&world, 0, &["b"]);
    world.run(2, &[(0, rename(nested, b, "E\u{301}"))]).unwrap();
    world.run(2, &[(0, folder("\u{c9}"))]).unwrap();
    let made = folder_number(&world, 0, &["\u{c9}"]);
    let b = target(&world, 0, &["b"]);
    let offline = [(0, user(1, 0, made, "c.txt")), (0, rename(b, made, "b"))];
    world.run(2, &offline).unwrap();
    let mut steps = vec![(1, rename(moved, 0, "\u{c9}"))];
    steps.extend(commit(1));
    world.run(2, &steps).unwrap();
    world.phase(3, &[(0, Step::Connect)]).unwrap();
    assert_eq!(held_at(&world, 0, &["\u{c9}"]), Some(live.clone()));
    assert_eq!(live_node_at(&world, &["\u{c9}"]), Some(live.to_string()));
    assert_eq!(ino_at(&world, 0, &["\u{c9}", "\u{df}"]), file);
    ino_at(&world, 0, &["\u{c9}", "c.txt"]);
    assert!(world.reported_problems(0).is_empty());
    sends_nothing_more(&mut world, 9);
}

#[test]
fn a_merged_folder_uploads_beside_a_kept_folder_once_another_device_deletes_the_live_folder() {
    let phases = vec![
        vec![(0, folder("a")), (0, user(1, 0, 1, "c.txt"))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(2, 0, "A")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let source = held_at(&world, 0, &["A"]).unwrap();
    let kept = held_at(&world, 0, &["a"]).unwrap();
    let live = target(&world, 1, &["a"]);
    world.phase(2, &[(1, delete(live))]).unwrap();
    assert!(live_node_at(&world, &["a"]).is_none());
    let created = live_node_at(&world, &["A"]).unwrap();
    assert_ne!(created, source.to_string());
    let created = NodeId::from(created);
    assert_eq!(held_at(&world, 0, &["A"]), Some(created.clone()));
    ino_at(&world, 0, &["A", "c.txt"]);
    ino_at(&world, 0, &["a", "c.txt"]);
    assert!((world.space().nodes().iter()).all(|node| node.name != "c.txt"));
    let rekeyed = held_at(&world, 0, &["a"]).unwrap();
    assert_ne!(rekeyed, kept);
    assert_eq!(
        world.reported_problems(0),
        [(rekeyed, Stuck::MergedInto(created))].into()
    );
    sends_nothing_more(&mut world, 3);
}

#[test]
fn a_deep_merged_folder_settles_once_another_device_deletes_the_live_folder() {
    let phases = vec![
        vec![
            (0, folder("a")),
            (0, user(0, 0, 1, "b")),
            (0, user(0, 0, 2, "c.txt")),
        ],
        vec![
            (0, folder("C.TXT")),
            (0, user(0, 0, 4, "b")),
            (0, user(0, 0, 5, "c.txt")),
            (0, user(1, 0, 6, "c.txt")),
            (0, rename(3, 0, "A")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let live = target(&world, 1, &["a"]);
    world.phase(2, &[(1, delete(live))]).unwrap();
    assert!(live_node_at(&world, &["a"]).is_none());
    assert!(live_node_at(&world, &["A", "b"]).is_some());
    assert!((world.space().nodes().iter()).all(|node| node.name != "c.txt"));
    sends_nothing_more(&mut world, 3);
}

#[test]
fn merged_folders_settle_once_another_device_deletes_the_folder_that_took_one_over() {
    let phases = vec![
        vec![(0, folder("\u{e9}"))],
        vec![
            (0, folder("c.txt")),
            (0, user(1, 0, 2, "c.txt")),
            (0, rename(1, 0, "\u{c9}")),
            (0, folder("b")),
            (0, user(1, 0, 3, "c.txt")),
            (0, rename(3, 0, "E\u{301}")),
        ],
    ];
    let mut world = converge_excluding(&[vec!["*.txt"], vec![]], &phases).unwrap();
    let live = target(&world, 1, &["\u{e9}"]);
    world
        .phase(2, &[(1, delete(live)), (1, folder("\u{e9}"))])
        .unwrap();
    let live = target(&world, 1, &["\u{e9}"]);
    world.phase(3, &[(1, delete(live))]).unwrap();
    let uploaded = NodeId::from(live_node_at(&world, &["\u{c9}"]).unwrap());
    assert_eq!(held_at(&world, 0, &["E\u{301}"]), Some(uploaded.clone()));
    let other = held_at(&world, 0, &["\u{e9}"]).unwrap();
    assert_eq!(
        world.reported_problems(0),
        [(other, Stuck::MergedInto(uploaded))].into()
    );
    let kept: Vec<Vec<String>> = [["E\u{301}", "c.txt"], ["\u{e9}", "c.txt"]]
        .iter()
        .map(|path| path.iter().map(|name| name.to_string()).collect())
        .collect();
    assert_eq!(left_out_files(&world), kept);
    assert!((world.space().nodes().iter()).all(|node| node.name != "c.txt"));
    sends_nothing_more(&mut world, 4);
}
