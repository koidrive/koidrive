// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Origin, Pending, Trees, Waiting};
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{self, Base, BatchLimit, Effect, Effects, Location, Stuck};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{LocalRules, RuleFile, Source};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{self as proto, op};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";
const AT_ROOT: [&str; 0] = [];

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        made_over: Vec::new(),
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

fn identity(ino: u128) -> FileIdentity {
    FileIdentity {
        ino,
        birth_ns: Some(0),
        handle: None,
    }
}

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        displaced: false,
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: identity(ino),
        parked: false,
    }
}

type Row = (&'static str, Option<Shape>, Option<Shape>, Option<Shape>);

fn trees(nodes: &[Row]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (i, (id, l, s, r)) in nodes.iter().enumerate() {
        if let Some(shape) = l {
            trees.set_local((*id).into(), local(shape.clone(), i as u128 + 1));
        }
        trees.set_synced((*id).into(), s.clone());
        if let Some(shape) = r {
            trees.set_remote((*id).into(), remote(shape.clone()));
        }
    }
    trees.settle(&Default::default());
    trees
}

fn synced(id: &'static str, shape: Shape) -> Row {
    (id, Some(shape.clone()), Some(shape.clone()), Some(shape))
}

fn incoming(id: &'static str, shape: Shape) -> Row {
    (id, None, None, Some(shape))
}

fn rules(text: &str) -> Option<RuleFile> {
    Some(RuleFile::parse(text.as_bytes()))
}

fn ops(trees: &mut Trees) -> Vec<op::Op> {
    trees.settle(&Default::default());
    let commit = Commits::default()
        .build(trees, &mut IdGen::new(1), NOW, HISTORY, "me", 7)
        .cloned();
    let ops = commit.map(|commit| commit.ops).unwrap_or_default();
    ops.into_iter().map(|op| op.op.unwrap()).collect()
}

fn effects(trees: &mut Trees) -> Vec<(String, Effect)> {
    let (mut effects, mut ids, mut done) = (Effects::default(), IdGen::new(2), Vec::new());
    loop {
        trees.settle(&Default::default());
        let Some(record) = effects
            .plan(trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            return done;
        };
        let made = identity(100 + done.len() as u128);
        effects
            .done(trees, None, &record.effect_id, Some(made))
            .unwrap();
        done.push((record.node, record.effect));
    }
}

fn nodes(effects: &[(String, Effect)]) -> Vec<&str> {
    effects.iter().map(|(node, _)| node.as_str()).collect()
}

fn delete(node: &str) -> op::Op {
    op::Op::Delete(proto::Delete {
        node_id: node.into(),
        requires: None,
        ..Default::default()
    })
}

fn set_content(trees: &mut Trees, id: &str, digest: u8) {
    let mut node = trees.remote().get(id).unwrap().clone();
    node.shape.content = Some(content(digest));
    trees.set_remote(id.into(), node);
}

fn trash(trees: &mut Trees, id: &str) {
    let mut node = trees.remote().get(id).unwrap().clone();
    node.trashed = true;
    trees.set_remote(id.into(), node);
}

fn place(trees: &mut Trees, id: &str, parent: &str, name: &str) {
    let mut node = trees.local().get(id).unwrap().clone();
    node.shape.parent = parent.into();
    node.shape.name = name.into();
    trees.set_local(id.into(), node);
}

#[test]
fn a_rule_excludes_what_it_matches_in_any_tree_and_everything_below_a_folder() {
    let mut trees = trees(&[
        synced("logs", dir(ROOT, "logs")),
        synced("old", file("logs", "old.txt", 1)),
        incoming("new", file("logs", "new.txt", 2)),
        synced("kept", file(ROOT, "kept.txt", 3)),
        synced("rules", file(ROOT, ".koiignore", 4)),
    ]);
    assert!(
        ["logs", "old", "new", "kept", "rules"]
            .iter()
            .all(|id| !trees.excluded(id))
    );
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("logs/\n.koiignore\n"));
    assert!(["logs", "old", "new"].iter().all(|id| trees.excluded(id)));
    assert!(!trees.excluded("kept"));
    assert!(!trees.excluded("rules"), "a rules file cannot hide itself");
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    assert!(["logs", "old", "new"].iter().all(|id| !trees.excluded(id)));
}

#[test]
fn a_synced_node_a_new_rule_matches_is_left_alone_on_both_sides() {
    let mut trees = trees(&[
        synced("logs", dir(ROOT, "logs")),
        synced("a", file("logs", "a.log", 1)),
        synced("b", file(ROOT, "b.log", 2)),
        synced("c", file(ROOT, "c.txt", 3)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);

    set_content(&mut trees, "a", 7);
    trash(&mut trees, "b");
    set_content(&mut trees, "c", 8);
    let done = effects(&mut trees);
    assert_eq!(
        nodes(&done),
        ["c"],
        "only the included file follows the server"
    );
    assert_eq!(ops(&mut trees), []);
    assert_eq!(
        trees.local().get("a").unwrap().shape,
        file("logs", "a.log", 1)
    );
    assert_eq!(
        trees.local().get("b").unwrap().shape,
        file(ROOT, "b.log", 2)
    );
    assert_eq!(trees.synced().live("b"), Some(&file(ROOT, "b.log", 2)));
}

#[test]
fn a_remote_node_that_matches_a_rule_is_not_downloaded() {
    let mut trees = trees(&[
        incoming("build", dir(ROOT, "build")),
        incoming("out", file("build", "out.bin", 1)),
        incoming("src", dir(ROOT, "src")),
        incoming("main", file("src", "main.rs", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/build/\n"));
    let done = effects(&mut trees);
    assert_eq!(nodes(&done), ["src", "main"]);
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_node_is_synced_again_once_its_rule_is_removed() {
    let mut trees = trees(&[
        synced("a", file(ROOT, "a.log", 1)),
        synced("b", file(ROOT, "b.log", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    set_content(&mut trees, "a", 7);
    assert_eq!(effects(&mut trees), []);

    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("# nothing\n"));
    let mut edited = trees.local().get("b").unwrap().clone();
    edited.shape.content = Some(content(9));
    trees.set_local("b".into(), edited);
    let done = effects(&mut trees);
    assert_eq!(
        nodes(&done),
        ["a"],
        "the remote edit made while ignored is downloaded"
    );
    let update = op::Op::UpdateContent(proto::UpdateContent {
        made_over: Vec::new(),
        source_version_id: String::new(),
        node_id: "b".into(),
        content: Some(content(9)),
        resend: false,
    });
    assert_eq!(
        ops(&mut trees),
        [update],
        "the edit made while ignored is uploaded"
    );
}

#[test]
fn the_platform_files_and_the_state_folder_are_excluded_without_any_rule() {
    let mut trees = trees(&[
        synced("docs", dir(ROOT, "docs")),
        incoming("p1", file(ROOT, ".DS_Store", 1)),
        incoming("p2", file("docs", "Thumbs.db", 2)),
        incoming("p3", file("docs", "desktop.ini", 3)),
        incoming("p4", file(ROOT, "Icon\r", 4)),
        incoming("p5", file("docs", ".ds_store", 5)),
        incoming("state", dir(ROOT, ".koidrive")),
        incoming("inner", dir("docs", ".koidrive")),
        ("mine", Some(file("docs", ".DS_Store", 6)), None, None),
    ]);
    let done = effects(&mut trees);
    assert_eq!(
        nodes(&done),
        ["inner"],
        "only the state folder at the root is built in"
    );
    assert_eq!(
        ops(&mut trees),
        [],
        "a platform file on disk is not uploaded"
    );
    trees.set_rules(
        &AT_ROOT,
        Source::Koiignore,
        rules("!.DS_Store\n!Thumbs.db\n!.koidrive/\n"),
    );
    assert_eq!(
        effects(&mut trees),
        [],
        "no pattern brings a built-in exclusion back"
    );
}

#[test]
fn a_device_local_include_list_narrows_what_syncs() {
    let mut trees = trees(&[
        incoming("work", dir(ROOT, "Work")),
        incoming("plan", file("work", "plan.txt", 1)),
        incoming("photos", dir(ROOT, "Photos")),
        incoming("cat", file("photos", "cat.jpg", 2)),
        incoming("top", file(ROOT, "top.txt", 3)),
        ("note", Some(file(ROOT, "note.txt", 4)), None, None),
    ]);
    let none: [&str; 0] = [];
    trees.set_local_rules(LocalRules::new(&["Work/"], &none).unwrap());
    let done = effects(&mut trees);
    assert_eq!(nodes(&done), ["work", "plan"]);
    assert_eq!(
        ops(&mut trees),
        [],
        "a file outside the list is not uploaded"
    );

    trees.set_local_rules(LocalRules::new(&none, &["Photos/"]).unwrap());
    let done = effects(&mut trees);
    assert_eq!(
        nodes(&done),
        ["top"],
        "without the list only the exclude pattern narrows"
    );
    assert_eq!(ops(&mut trees).len(), 1);
}

#[test]
fn a_file_renamed_to_an_ignored_name_stays_on_the_server_as_it_was() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.txt", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    place(&mut trees, "a", ROOT, "a.log");
    assert_eq!(ops(&mut trees), [], "neither a move nor a delete is sent");
    set_content(&mut trees, "a", 7);
    trash(&mut trees, "a");
    assert_eq!(effects(&mut trees), [], "its local copy is left as it is");
}

#[test]
fn a_node_whose_name_another_entry_took_stays_excluded_until_it_returns_or_is_trashed() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("a", file("d", "a.txt", 1)),
        synced("b", file("d", "b.txt", 2)),
    ]);
    let mut undated = trees.local().get("b").unwrap().clone();
    undated.identity.birth_ns = None;
    trees.set_local("b".into(), undated);
    trees.ignore_local("d");
    let held = trees.local().get("a").unwrap().identity;
    assert!(["d", "a", "b"].iter().all(|id| trees.recorded_local(id)));
    for id in ["d", "a", "b"] {
        trees.displace_local(id);
        assert!(trees.displaced_local().contains_key(id) && trees.excluded(id));
    }
    trees.set_local("new".into(), local(file(ROOT, "d", 3), 9));
    let sent = ops(&mut trees);
    assert!(
        matches!(sent.as_slice(), [op::Op::CreateFile(_)]),
        "only the new file is sent, and nothing is deleted: {sent:?}"
    );
    set_content(&mut trees, "a", 7);
    assert_eq!(effects(&mut trees), [], "none is downloaded again");

    let mut restored = trees.clone();
    restored.restore(trees.state()).unwrap();
    assert_eq!(restored.state(), trees.state());
    assert!(["d", "a", "b"].iter().all(|id| restored.ignored(id)));

    assert_eq!(trees.local_with_identity(&held), Some(&"a".to_owned()));
    let reused = FileIdentity {
        birth_ns: held.birth_ns.map(|birth| birth + 1),
        ..held
    };
    assert_eq!(trees.local_with_identity(&reused), None);
    let undated = trees.displaced_local()["b"];
    assert_eq!(undated.birth_ns, None);
    assert_eq!(
        trees.local_with_identity(&undated),
        None,
        "a record without a birth time matches no file"
    );
    trees.set_local("a".into(), local(file(ROOT, "back.txt", 1), 2));
    assert!(!trees.excluded("a") && !trees.displaced_local().contains_key("a"));
    trash(&mut trees, "b");
    assert!(!trees.excluded("b"));
    assert_eq!(
        trees.displaced_local().keys().collect::<Vec<_>>(),
        ["d"],
        "the folder is still out"
    );
}

#[test]
fn a_displaced_node_r_does_not_hold_live_is_not_recorded() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.txt", 1))]);
    trees.ignore_local("a");
    trash(&mut trees, "a");
    trees.displace_local("a");
    assert!(trees.local().get("a").is_none() && trees.displaced_local().is_empty());
}

#[test]
fn a_file_moved_into_an_ignored_folder_that_is_no_node_stays_on_the_server_as_it_was() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("a", file("d", "a.txt", 1)),
        synced("inner", dir("d", "inner")),
        synced("b", file("inner", "b.txt", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/build/\n"));
    trees.ignore_local("a");
    trees.ignore_local("inner");
    assert!(trees.excluded("a") && trees.excluded("inner"));
    assert!(
        trees.excluded("b"),
        "what a moved folder holds moved with it"
    );
    assert_eq!(ops(&mut trees), []);
    set_content(&mut trees, "a", 7);
    trash(&mut trees, "b");
    assert_eq!(effects(&mut trees), []);

    place(&mut trees, "a", ROOT, "back.txt");
    assert!(!trees.excluded("a"));
    let moved = op::Op::Move(proto::Move {
        node_id: "a".into(),
        new_parent_id: ROOT.into(),
        new_name: "back.txt".into(),
        from: None,
        pending_for: String::new(),
    });
    assert_eq!(
        ops(&mut trees),
        [moved],
        "a node that returns is moved, not created"
    );
}

fn holding_an_excluded_folders_name(incoming: RemoteNode) -> Trees {
    let mut trees = trees(&[("kept", Some(dir(ROOT, "ig")), None, None)]);
    trees.set_remote("f".into(), incoming);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/ig/\n"));
    assert!(trees.excluded("kept"));
    trees
}

#[test]
fn a_server_file_at_an_excluded_folders_name_is_renamed_from_its_place_to_a_conflict_name() {
    let mut trees = holding_an_excluded_folders_name(remote(file(ROOT, "ig", 1)));
    let ops = ops(&mut trees);
    let [op::Op::Move(moved)] = &ops[..] else {
        panic!("one rename: {ops:?}");
    };
    let from = proto::Place {
        parent_id: ROOT.into(),
        name: "ig".into(),
    };
    assert_eq!((moved.node_id.as_str(), &moved.from), ("f", &Some(from)));
    assert!(
        moved.new_name.starts_with("ig (conflict from "),
        "{}",
        moved.new_name
    );
}

#[test]
fn a_server_file_at_an_excluded_folders_name_the_rules_leave_no_conflict_name_for_is_reported() {
    let mut trees = holding_an_excluded_folders_name(remote(file(ROOT, "ig", 1)));
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/ig/\n*conflict*\n"));
    assert_eq!(ops(&mut trees), []);
    let renaming = Commits::default().renaming(&trees, NOW);
    assert_eq!(
        effects::stuck(&trees, &Effects::default(), None, &renaming),
        BTreeMap::from([("f".into(), Stuck::HeldBy("kept".into()))])
    );
}

#[test]
fn a_server_file_under_a_pending_name_at_an_excluded_folders_name_is_not_renamed() {
    let incoming = RemoteNode {
        pending_for: Some("device".into()),
        ..remote(file(ROOT, "ig", 1))
    };
    let mut trees = holding_an_excluded_folders_name(incoming);
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_node_the_user_moves_out_of_an_ignored_path_is_moved_on_the_server() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.log", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert!(trees.excluded("a"));
    place(&mut trees, "a", ROOT, "a.txt");
    assert!(!trees.excluded("a"));
    let moved = op::Op::Move(proto::Move {
        node_id: "a".into(),
        new_parent_id: ROOT.into(),
        new_name: "a.txt".into(),
        from: None,
        pending_for: String::new(),
    });
    assert_eq!(ops(&mut trees), [moved]);
}

#[test]
fn a_remote_move_into_an_ignored_path_leaves_the_local_copy_where_it_is() {
    let mut trees = trees(&[
        synced("a", file(ROOT, "a.txt", 1)),
        synced("b", file(ROOT, "b.txt", 2)),
        incoming("build", dir(ROOT, "build")),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/build/\n"));
    for id in ["a", "b"] {
        let mut node = trees.remote().get(id).unwrap().clone();
        node.shape.parent = "build".into();
        trees.set_remote(id.into(), node);
    }
    assert_eq!(effects(&mut trees), []);
    assert_eq!(
        trees.local().get("a").unwrap().shape,
        file(ROOT, "a.txt", 1)
    );

    let mut edited = trees.local().get("a").unwrap().clone();
    edited.shape.content = Some(content(9));
    trees.set_local("a".into(), edited);
    trees.remove_local("b");
    assert_eq!(
        ops(&mut trees),
        [],
        "editing or deleting it later sends nothing"
    );
}

#[test]
fn deleting_a_folder_that_holds_an_excluded_node_deletes_only_the_rest() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("log", file("d", "a.log", 1)),
        synced("txt", file("d", "b.txt", 2)),
        synced("sub", dir("d", "sub")),
        synced("deep", file("sub", "c.txt", 3)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    for id in ["deep", "sub", "txt", "log", "d"] {
        trees.remove_local(id);
    }
    let sent: BTreeSet<String> = ops(&mut trees)
        .into_iter()
        .map(|op| format!("{op:?}"))
        .collect();
    let wanted: BTreeSet<String> = [delete("txt"), delete("sub")]
        .iter()
        .map(|op| format!("{op:?}"))
        .collect();
    assert_eq!(
        sent, wanted,
        "the folder and its excluded file stay on the server"
    );
    assert_eq!(
        effects(&mut trees),
        [],
        "and the folder is not created again here"
    );
}

#[test]
fn a_remote_change_to_an_excluded_node_does_not_bring_back_a_folder_the_user_deleted() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.remove_local("log");
    trees.remove_local("d");
    assert_eq!(ops(&mut trees), [], "the folder holds an excluded file");
    set_content(&mut trees, "log", 7);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "d")));
}

#[test]
fn a_folder_deleted_here_that_another_device_moved_comes_back_there_and_frees_its_old_name() {
    let mut trees = trees(&[
        synced("m", dir(ROOT, "m")),
        ("d", None, Some(dir(ROOT, "d")), Some(dir("m", "d"))),
        (
            "log",
            None,
            Some(file("d", "a.log", 1)),
            Some(file("d", "a.log", 1)),
        ),
        (
            "new",
            Some(file(ROOT, "d", 2)),
            None,
            Some(file(ROOT, "d", 2)),
        ),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert!(trees.kept_for_excluded()("d"));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d"]);
    assert_eq!(trees.local().live("d"), Some(&dir("m", "d")));
    assert_eq!(trees.synced().live("d"), Some(&dir("m", "d")));
    assert_eq!(trees.synced().live("new"), Some(&file(ROOT, "d", 2)));
    assert_eq!(trees.check(), Ok(()));
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    assert_eq!(ops(&mut trees), [delete("log")]);
}

#[test]
fn a_kept_folder_and_the_node_that_takes_its_name_swap_names_in_s() {
    for holder in [dir as fn(&str, &str) -> Shape, |parent, name| {
        file(parent, name, 3)
    }] {
        let mut trees = trees(&[
            ("d", None, Some(dir(ROOT, "d")), Some(dir(ROOT, "m"))),
            (
                "log",
                None,
                Some(file("d", "a.log", 1)),
                Some(file("d", "a.log", 1)),
            ),
            (
                "m",
                Some(holder(ROOT, "d")),
                Some(holder(ROOT, "m")),
                Some(holder(ROOT, "d")),
            ),
        ]);
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
        assert!(trees.kept_for_excluded()("d"));
        assert_eq!(ops(&mut trees), []);
        assert_eq!(nodes(&effects(&mut trees)), ["d"]);
        assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "m")));
        assert_eq!(trees.synced().live("m"), Some(&holder(ROOT, "d")));
        assert_eq!(trees.check(), Ok(()));
    }
}

#[test]
fn a_parked_node_the_rules_now_exclude_is_moved_back_to_where_l_keeps_it() {
    let mut trees = trees(&[(
        "a",
        None,
        Some(file(ROOT, "a.txt", 1)),
        Some(file(ROOT, "A.TXT", 1)),
    )]);
    let parked = LocalNode {
        parked: true,
        ..local(file(ROOT, "a.txt", 1), 1)
    };
    trees.set_local("a".into(), parked);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("a.txt\n"));
    assert!(trees.ignored("a") && !trees.excluded("a"));
    let done = effects(&mut trees);
    let [(_, Effect::Move { from, to, .. })] = &done[..] else {
        panic!("one move out of the parked directory: {done:?}");
    };
    assert_eq!(from, &Location::Parked("a".into()));
    let at_root = Location::At {
        base: Base::Root,
        dirs: vec![],
        name: "a.txt".into(),
    };
    assert_eq!(to, &at_root, "its last synced name, not the server's");
    assert_eq!(
        trees.local().get("a").unwrap().shape,
        file(ROOT, "a.txt", 1)
    );
    assert!(trees.excluded("a"));
}

#[test]
fn a_folder_the_user_deleted_comes_back_for_a_parked_node_the_rules_now_exclude() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        (
            "a",
            None,
            Some(file("d", "a.txt", 1)),
            Some(file("d", "A.TXT", 1)),
        ),
        synced("e", dir("d", "e")),
    ]);
    let parked = LocalNode {
        parked: true,
        ..local(file("d", "a.txt", 1), 1)
    };
    trees.set_local("a".into(), parked);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("a.txt\n"));
    trees.remove_local("e");
    trees.remove_local("d");
    assert_eq!(ops(&mut trees), [delete("e")]);
    assert_eq!(nodes(&effects(&mut trees)), ["d", "a"]);
    let placed = trees.local().get("a").unwrap();
    assert!(!placed.parked);
    assert_eq!(placed.shape, file("d", "a.txt", 1));
    assert!(trees.excluded("a"));
    assert_eq!(trees.local().get("d").unwrap().shape, dir(ROOT, "d"));
}

#[test]
fn a_node_whose_place_an_excluded_node_holds_is_not_parked_to_make_room() {
    let mut trees = trees(&[
        (
            "old",
            Some(file(ROOT, "notes.txt", 1)),
            Some(file(ROOT, "notes.txt", 1)),
            Some(file(ROOT, "notes.log", 1)),
        ),
        (
            "draft",
            Some(file(ROOT, "draft.txt", 2)),
            Some(file(ROOT, "draft.txt", 2)),
            Some(file(ROOT, "notes.txt", 2)),
        ),
        incoming("new", file(ROOT, "draft.txt", 3)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert!(trees.excluded("old"), "its local copy stays at notes.txt");
    assert_eq!(effects(&mut trees), []);
    assert!(!trees.local().get("draft").unwrap().parked);
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_parked_node_whose_old_name_another_node_took_follows_r_into_the_folder_the_rules_exclude() {
    let mut trees = trees(&[
        (
            "old",
            None,
            Some(dir(ROOT, "docs")),
            Some(dir(ROOT, "archive")),
        ),
        (
            "new",
            Some(dir(ROOT, "docs")),
            None,
            Some(dir(ROOT, "docs")),
        ),
    ]);
    let parked = LocalNode {
        parked: true,
        ..local(dir(ROOT, "docs"), 9)
    };
    trees.set_local("old".into(), parked.clone());
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/archive/\n"));
    let to = Location::At {
        base: Base::Root,
        dirs: vec![],
        name: "archive".into(),
    };
    let moved = Effect::Move {
        from: Location::Parked("old".into()),
        identity: parked.identity,
        to,
    };
    assert_eq!(effects(&mut trees), [("old".into(), moved)]);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.synced().live("new"), Some(&dir(ROOT, "docs")));
    assert_eq!(trees.synced().live("old"), Some(&dir(ROOT, "archive")));
    assert_eq!(
        trees.local().get("old").unwrap().shape,
        dir(ROOT, "archive")
    );
    assert!(trees.excluded("old"));
}

fn parked_excluded_folder() -> (Trees, LocalNode) {
    let mut trees = trees(&[
        (
            "old",
            None,
            Some(dir(ROOT, "docs")),
            Some(dir("far", "docs")),
        ),
        synced("child", file("old", "build.log", 1)),
        incoming("far", dir(ROOT, "archive")),
        (
            "new",
            Some(dir(ROOT, "docs")),
            None,
            Some(dir(ROOT, "docs")),
        ),
    ]);
    let parked = LocalNode {
        parked: true,
        ..local(dir(ROOT, "docs"), 9)
    };
    trees.set_local("old".into(), parked.clone());
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/archive/\n*.log\n"));
    (trees, parked)
}

#[test]
fn a_parked_folder_the_rules_now_exclude_frees_its_name_in_s_with_what_it_holds() {
    let (mut trees, parked) = parked_excluded_folder();
    assert_eq!(effects(&mut trees), [], "its old name is taken");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.synced().live("new"), Some(&dir(ROOT, "docs")));
    assert_eq!(trees.synced().live("old"), None);
    assert_eq!(trees.synced().live("child"), None);
    assert_eq!(trees.local().get("old"), Some(&parked));
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(
        effects::stuck(&trees, &Effects::default(), None, &Default::default()),
        BTreeMap::from([("old".into(), Stuck::Parked)])
    );
}

#[test]
fn a_child_settled_before_its_folder_leaves_s_is_classified_again() {
    for digest in [1, 2] {
        let (mut trees, _) = parked_excluded_folder();
        trees.set_local("child".into(), local(file("old", "build.log", digest), 2));
        trees.settle(&Default::default());
        let left = trees.work();
        assert_eq!(trees.synced().live("old"), None, "digest {digest}");
        assert_eq!(trees.synced().live("child"), None, "digest {digest}");
        let child: Vec<_> = left.iter().filter(|(id, _)| id == "child").collect();
        assert_eq!(
            child,
            [&("child".into(), trees.class("child"))],
            "digest {digest}"
        );
        let mut restored = trees.clone();
        restored.restore(trees.state()).unwrap();
    }
}

#[test]
fn an_excluded_folder_s_cannot_follow_to_r_frees_its_name_in_s_all_the_same() {
    let mut trees = trees(&[
        (
            "moved",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "docs")),
            Some(dir("far", "docs")),
        ),
        synced("child", file("moved", "a.txt", 1)),
        incoming("far", dir(ROOT, "y")),
        (
            "new",
            Some(dir(ROOT, "docs")),
            None,
            Some(dir(ROOT, "docs")),
        ),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/x/\n/y/\n"));
    assert!(trees.excluded("moved") && trees.excluded("far"));
    assert_eq!(effects(&mut trees), []);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.synced().live("new"), Some(&dir(ROOT, "docs")));
    assert_eq!(
        trees.synced().live("moved"),
        Some(&dir(ROOT, "x")),
        "S has no folder y to follow R into, and a.txt keeps it from dropping the folder"
    );
}

#[test]
fn a_folder_is_not_parked_to_move_below_an_excluded_folder_it_holds() {
    let mut trees = trees(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a")),
            Some(dir("b", "a")),
        ),
        (
            "x",
            Some(dir("a", "x.log")),
            Some(dir("a", "x.log")),
            Some(dir(ROOT, "x")),
        ),
        synced("b", dir("x", "b")),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert!(trees.excluded("x") && trees.excluded("b") && trees.excluded("a"));
    assert_eq!(
        effects(&mut trees),
        [],
        "b stays inside a on disk, so parking a would not let a move into b"
    );
    assert!(!trees.local().get("a").unwrap().parked);
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn an_excluded_node_whose_s_update_waits_is_not_parked_to_make_room() {
    let mut trees = trees(&[
        (
            "old",
            Some(file(ROOT, "notes.txt", 5)),
            Some(file(ROOT, "notes.txt", 1)),
            Some(file(ROOT, "notes.log", 5)),
        ),
        (
            "draft",
            Some(file(ROOT, "draft.txt", 2)),
            Some(file(ROOT, "draft.txt", 2)),
            Some(file(ROOT, "notes.txt", 2)),
        ),
    ]);
    let mut state = trees.state();
    let acknowledged = Waiting {
        shape: Some(file(ROOT, "notes.txt", 5)),
        origin: Origin::Other,
    };
    state.waiting.insert("old".into(), acknowledged);
    assert_eq!(trees.restore(state), Ok(()));
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.rules_scanned();
    assert!(trees.excluded("old") && trees.waiting().contains_key("old"));
    assert_eq!(effects(&mut trees), []);
    assert!(!trees.local().get("old").unwrap().parked);
    assert!(!trees.local().get("draft").unwrap().parked);
}

#[test]
fn s_of_an_excluded_node_takes_what_l_and_r_both_hold() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.log", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    set_content(&mut trees, "a", 7);
    trees.set_local("a".into(), local(file(ROOT, "a.log", 7), 1));
    trees.set_local("b".into(), local(file(ROOT, "b.log", 2), 2));
    trees.set_remote("b".into(), remote(file(ROOT, "b.log", 2)));
    assert!(trees.excluded("a") && trees.excluded("b"));
    trees.settle(&Default::default());
    assert_eq!(trees.work(), [], "settling alone takes both");
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "a.log", 7)));
    assert_eq!(trees.synced().live("b"), Some(&file(ROOT, "b.log", 2)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
}

#[test]
fn a_remote_rules_file_is_written_before_anything_else_below_its_folder() {
    a_remote_rules_file_of_kind_is_written_first(Source::Koiignore);
}

#[test]
fn a_remote_gitignore_file_is_written_before_anything_else_below_its_folder() {
    a_remote_rules_file_of_kind_is_written_first(Source::Gitignore);
}

fn a_remote_rules_file_of_kind_is_written_first(source: Source) {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        incoming("a-outside", file(ROOT, "z.txt", 1)),
        incoming("b-secret", file("d", "secret.txt", 2)),
        incoming("c-sub", dir("d", "sub")),
        incoming("d-deep", file("c-sub", "deep.txt", 3)),
        incoming("z-rules", file("d", source.file_name(), 4)),
    ]);
    let (mut effects, mut ids, mut order) = (Effects::default(), IdGen::new(2), Vec::new());
    loop {
        trees.settle(&Default::default());
        let Some(record) = effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let made = identity(100 + order.len() as u128);
        effects
            .done(&mut trees, None, &record.effect_id, Some(made))
            .unwrap();
        if record.node == "z-rules" {
            trees.set_rules(&["d"], source, rules("secret.txt\n"));
        }
        order.push(record.node);
    }
    assert_eq!(order, ["a-outside", "z-rules", "c-sub", "d-deep"]);
    assert!(trees.local().get("b-secret").is_none());
}

#[test]
fn nothing_is_planned_after_a_restore_until_the_rules_are_scanned() {
    let mut before = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("log", file("d", "a.log", 1)),
        synced("txt", file("d", "b.txt", 2)),
        synced("gone", file(ROOT, "gone.log", 3)),
    ]);
    before.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    before.remove_local("log");
    before.remove_local("txt");
    trash(&mut before, "gone");
    before.settle(&Default::default());

    let mut trees = Trees::new(ROOT);
    for (id, node) in before.remote().iter() {
        trees.set_remote(id.clone(), node.clone());
    }
    for (id, node) in before.local().iter() {
        trees.set_local(id.clone(), node.clone());
    }
    for (id, shape) in before.synced().iter() {
        trees.set_synced(id.clone(), Some(shape.clone()));
    }
    assert_eq!(trees.restore(before.state()), Ok(()));
    assert!(!trees.rules_known());
    assert_eq!(
        ops(&mut trees),
        [],
        "without the rules this would delete the ignored file"
    );
    assert_eq!(
        effects(&mut trees),
        [],
        "and remove the local copy of the other"
    );

    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.rules_scanned();
    assert_eq!(ops(&mut trees), [delete("txt")]);
    assert_eq!(effects(&mut trees), []);
}

#[test]
fn a_folder_whose_rules_are_unknown_holds_nothing_below_its_namesake_in_another_case() {
    let mut trees = trees(&[
        ("lower", Some(dir(ROOT, "proj")), None, None),
        ("upper", Some(dir(ROOT, "PROJ")), None, None),
        ("readable", Some(file("lower", "file.txt", 1)), None, None),
        ("unknown", Some(file("upper", "file.txt", 1)), None, None),
    ]);
    assert_eq!(trees.check(), Ok(()));
    trees.set_rules_unknown("upper");
    assert!(trees.held("unknown"));
    assert!(!trees.held("readable"));
}

#[test]
fn reading_a_folder_keeps_the_hold_of_its_namesake_in_another_case() {
    let mut trees = trees(&[
        ("lower", Some(dir(ROOT, "proj")), None, None),
        ("upper", Some(dir(ROOT, "PROJ")), None, None),
        ("unknown", Some(file("upper", "file.txt", 1)), None, None),
    ]);
    assert_eq!(trees.check(), Ok(()));
    trees.set_rules_unknown("upper");
    trees.set_rules(&["proj"], Source::Koiignore, None);
    trees.set_rules_known("lower");
    assert!(trees.held("unknown"));
}

#[test]
fn the_hold_of_a_folder_ends_when_it_leaves_l() {
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "d")),
            Some(dir(ROOT, "d")),
            Some(dir(ROOT, "d")),
        ),
        ("f", None, Some(file("d", "f", 1)), Some(file("d", "f", 1))),
    ]);
    trees.set_rules_unknown("d");
    assert!(trees.held("f"));
    trees.remove_local("d");
    assert!(!trees.held("f"));
}

#[test]
fn a_server_entry_in_a_folder_whose_path_here_the_rules_exclude_is_not_written_there() {
    let mut trees = trees(&[
        (
            "f",
            Some(dir(ROOT, "ß")),
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
        ),
        incoming("c", file("f", "a", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("ss/\n"));
    assert!(trees.excluded("f"));
    assert!(
        trees.excluded("c"),
        "x/a is included, but here it would be ß/a"
    );
    assert_eq!(effects(&mut trees), []);
    assert_eq!(ops(&mut trees), []);
    assert!(trees.local().get("c").is_none());
}

#[test]
fn what_a_folder_moved_into_a_folder_excluded_here_holds_stays_out_too() {
    let mut trees = trees(&[
        (
            "z",
            Some(dir(ROOT, "ß")),
            Some(dir(ROOT, "ß")),
            Some(dir(ROOT, "z")),
        ),
        synced("y", dir(ROOT, "y")),
        (
            "f",
            Some(dir("y", "f")),
            Some(dir("y", "f")),
            Some(dir("z", "f")),
        ),
        synced("c", file("f", "c", 1)),
        incoming("d", file("f", "d", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("ss/\n"));
    assert!(trees.excluded("f") && trees.excluded("c") && trees.excluded("d"));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    trees.remove_local("c");
    trees.remove_local("f");
    assert_eq!(ops(&mut trees), [], "deleting it later sends nothing");
}

#[test]
fn what_a_folder_whose_place_here_the_rules_exclude_holds_stays_out_too() {
    let mut trees = trees(&[
        synced("q", dir(ROOT, "ß")),
        (
            "p",
            Some(dir("q", "p")),
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
        ),
        (
            "f",
            Some(dir(ROOT, "f")),
            Some(dir(ROOT, "f")),
            Some(dir("p", "f")),
        ),
        synced("c", file("f", "c", 1)),
        incoming("d", file("f", "d", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("ss/\n"));
    assert!(trees.excluded("p") && trees.excluded("f"));
    assert!(trees.excluded("c") && trees.excluded("d"));
    assert_eq!(effects(&mut trees), []);
    trees.remove_local("c");
    assert_eq!(ops(&mut trees), [], "deleting it later sends nothing");
}

#[test]
fn a_server_entry_is_not_written_where_the_rules_exclude_it_in_its_folder_here() {
    let mut trees = trees(&[
        (
            "f",
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "z")),
        ),
        ("z", Some(file(ROOT, "z", 2)), None, None),
        incoming("c", file("f", "c", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/y/c\n"));
    assert!(!trees.excluded("f") && trees.excluded("c"));
    assert_eq!(
        effects(&mut trees),
        [],
        "f waits for z, so c would land at y/c"
    );
    assert!(trees.local().get("c").is_none());
}

#[test]
fn what_a_new_server_folder_whose_place_here_the_rules_exclude_holds_stays_out_too() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "z")),
        ),
        ("z", Some(file(ROOT, "z", 2)), None, None),
        synced("a", dir("p", "a")),
        incoming("c", dir("a", "c")),
        incoming("x", file("c", "x", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/y/a/c\n"));
    assert!(!trees.excluded("p") && !trees.excluded("a"));
    assert!(trees.excluded("c") && trees.excluded("x"));
    assert_eq!(effects(&mut trees), []);
}

#[test]
fn a_new_server_entry_in_a_folder_the_rules_exclude_at_its_synced_place_stays_out() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "w")),
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "z")),
        ),
        synced("a", dir("p", "a")),
        incoming("x", file("a", "x", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/y/a\n"));
    assert!(!trees.excluded("p") && trees.excluded("a"));
    assert!(trees.excluded("x"));
    assert!(!nodes(&effects(&mut trees)).contains(&"x"));
}

#[test]
fn a_file_another_device_owed_a_rename_to_an_ignored_name_stays_where_it_is() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.txt", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.set_remote(
        "a".into(),
        RemoteNode {
            meta_rev: 2,
            ..remote(file(ROOT, "a.log", 1))
        },
    );
    assert_eq!(effects(&mut trees), []);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(
        trees.local().get("a").map(|node| node.shape.name.as_str()),
        Some("a.txt")
    );
}

#[test]
fn a_kept_folder_another_device_moves_below_the_folder_taking_its_stale_name_comes_back_there() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "ss")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.remove_local("log");
    trees.remove_local("d");
    trees.set_remote("d".into(), remote(dir(ROOT, "b")));
    trees.set_local("new".into(), local(dir(ROOT, "ß"), 9));
    trees.set_remote("new".into(), remote(dir(ROOT, "ß")));
    trees.set_remote("d".into(), remote(dir("new", "b")));
    assert!(!trees.ignored("d"));
    assert!(trees.kept_for_excluded()("d"));
    assert_eq!(ops(&mut trees), []);
    let done: Vec<String> = effects(&mut trees).into_iter().map(|(id, _)| id).collect();
    assert_eq!(done, ["d"]);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.synced().live("new"), Some(&dir(ROOT, "ß")));
    assert_eq!(trees.synced().live("d"), Some(&dir("new", "b")));
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_kept_folder_another_device_renames_comes_back_with_the_child_the_rules_let_in() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "ss")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/ss/a.log\n"));
    trees.remove_local("log");
    trees.remove_local("d");
    trees.set_remote("d".into(), remote(dir(ROOT, "b")));
    assert!(!trees.ignored("d"));
    assert!(trees.excluded("log"));
    assert!(trees.kept_for_excluded()("d"));
    assert_eq!(ops(&mut trees), []);
    let done: Vec<String> = effects(&mut trees).into_iter().map(|(id, _)| id).collect();
    assert_eq!(done, ["d", "log"]);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.local().live("log"), Some(&file("d", "a.log", 1)));
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_node_deleted_while_excluded_is_downloaded_again_when_the_rules_let_it_in() {
    let mut trees = trees(&[
        synced("dist", dir(ROOT, "dist")),
        synced("app", file("dist", "app.js", 1)),
        synced("sub", dir("dist", "sub")),
        synced("deep", file("sub", "report.docx", 2)),
        synced("log", file(ROOT, "a.log", 3)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n*.log\n"));
    for id in ["deep", "sub", "app", "dist", "log"] {
        trees.remove_local(id);
    }
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);

    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    assert_eq!(ops(&mut trees), []);
    let mut done = nodes(&effects(&mut trees))
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    done.sort();
    assert_eq!(done, ["app", "deep", "dist", "log", "sub"]);
    for id in ["dist", "app", "sub", "deep", "log"] {
        assert_eq!(trees.local().live(id), trees.remote().live(id), "{id}");
    }
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_that_vanished_in_a_deleted_folder_the_server_keeps_for_an_excluded_file_downloads_into_it()
 {
    let mut trees = trees(&[
        synced("k", dir(ROOT, "k")),
        synced("v", file("k", "v.docx", 1)),
        synced("x", file("k", "x.docx", 2)),
        synced("e", dir(ROOT, "e")),
        synced("p", dir(ROOT, "p")),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("e/\n*.log\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "v", "p", "v.log");
    rename_here(&mut trees, "x", "e", "x.docx");
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("v") && trees.excluded("x"));
    for id in ["v", "p", "k"] {
        trees.remove_local(id);
    }
    assert_eq!(ops(&mut trees), [delete("p")]);
    trash(&mut trees, "p");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["k", "v"]);
    assert_eq!(trees.local().live("v"), Some(&file("k", "v.docx", 1)));
    assert_eq!(trees.local().live("x"), Some(&file("e", "x.docx", 2)));
    assert_eq!(trees.synced().get("x"), Some(&file("k", "x.docx", 2)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_folder_deleted_with_the_excluded_file_it_holds_comes_back_with_it_when_the_rules_let_the_file_in()
 {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.remove_local("log");
    trees.remove_local("d");
    assert_eq!(ops(&mut trees), []);

    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d", "log"]);
    assert_eq!(trees.local().live("log"), Some(&file("d", "a.log", 1)));
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn a_node_deleted_after_the_rules_let_it_in_again_is_deleted() {
    let mut trees = trees(&[
        synced("dist", dir(ROOT, "dist")),
        synced("app", file("dist", "app.js", 1)),
        synced("log", file(ROOT, "a.log", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n*.log\n"));
    assert_eq!(ops(&mut trees), []);
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into(), "dist".into()], Pending::default());
    for id in ["app", "dist", "log"] {
        trees.remove_local(id);
    }
    let mut sent = ops(&mut trees);
    sent.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(sent, [delete("dist"), delete("log")]);
    assert_eq!(effects(&mut trees), []);
}

fn the_mark_of_an_excluded_dist_after_the_server_drops_it(drop_remote: impl Fn(&mut Trees, &str)) {
    for vanished in [true, false] {
        for lift in [true, false] {
            let case = format!("vanished={vanished}, lift={lift}");
            let mut trees = trees(&[
                synced("d", dir(ROOT, "dist")),
                synced("a", file("d", "a.js", 1)),
            ]);
            trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
            assert_eq!(ops(&mut trees), []);
            if vanished {
                trees.remove_local("a");
                trees.remove_local("d");
                assert_eq!(ops(&mut trees), []);
            }
            assert_eq!(
                trees.state().unobserved,
                BTreeSet::from(["a".into(), "d".into()])
            );
            for id in ["a", "d"] {
                drop_remote(&mut trees, id);
            }
            assert_eq!(ops(&mut trees), [], "{case}");
            assert_eq!(effects(&mut trees), [], "{case}");
            if lift {
                trees.set_rules(&AT_ROOT, Source::Koiignore, None);
                trees.listed(&[ROOT.into(), "d".into()], Pending::default());
            }
            assert_eq!(ops(&mut trees), [], "{case}");
            let done = effects(&mut trees);
            assert!(
                (done.iter()).all(|(_, effect)| !matches!(
                    effect,
                    Effect::WriteFile { .. } | Effect::CreateDir { .. }
                )),
                "{case}: {done:?}"
            );
            assert_eq!(ops(&mut trees), [], "{case}");
            for id in ["a", "d"] {
                assert!(!trees.held(id), "{case}: {id}");
            }
            let left_here = !vanished && !lift;
            let held_here: Vec<&str> = (trees.local().iter()).map(|(id, _)| id.as_str()).collect();
            let state = trees.state();
            let marked: Vec<&str> = state.unobserved.iter().map(String::as_str).collect();
            match left_here {
                true => {
                    assert_eq!(
                        held_here,
                        ["a", "d"],
                        "{case}: the local copy stays while the rules leave it out"
                    );
                    assert_eq!(marked, ["a", "d"], "{case}");
                }
                false => {
                    assert!(held_here.is_empty(), "{case}: {held_here:?}");
                    assert!(trees.synced().iter().next().is_none(), "{case}");
                    assert!(marked.is_empty(), "{case}: {marked:?}");
                }
            }
            assert_eq!(trees.check(), Ok(()), "{case}");
        }
    }
}

#[test]
fn the_mark_of_an_excluded_node_goes_with_it_when_the_server_purges_it() {
    the_mark_of_an_excluded_dist_after_the_server_drops_it(|trees, id| trees.remove_remote(id));
}

#[test]
fn the_mark_of_an_excluded_node_goes_with_it_when_another_device_trashes_it() {
    the_mark_of_an_excluded_dist_after_the_server_drops_it(|trees, id| {
        let shape = trees.remote().get(id).unwrap().shape.clone();
        trees.set_remote(
            id.into(),
            RemoteNode {
                trashed: true,
                ..remote(shape)
            },
        );
    });
}

fn restarted(before: &Trees) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (id, node) in before.remote().iter() {
        trees.set_remote(id.clone(), node.clone());
    }
    for (id, node) in before.local().iter() {
        trees.set_local(id.clone(), node.clone());
    }
    for (id, shape) in before.synced().iter() {
        trees.set_synced(id.clone(), Some(shape.clone()));
    }
    assert_eq!(trees.restore(before.state()), Ok(()));
    trees
}

fn rename_here(trees: &mut Trees, id: &str, parent: &str, name: &str) {
    let mut node = trees.local().get(id).unwrap().clone();
    node.shape.parent = parent.into();
    node.shape.name = name.into();
    trees.set_local_changed(id.into(), node);
}

#[test]
fn a_file_the_server_moved_out_of_an_excluded_folder_it_trashed_downloads_again_after_both_copies_went_while_stopped()
 {
    let mut before = trees(&[
        synced("d", dir(ROOT, "dist")),
        synced("f", file("d", "f.docx", 1)),
    ]);
    before.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
    assert_eq!(ops(&mut before), []);
    before.remove_local("f");
    before.remove_local("d");
    let mut moved = before.remote().get("f").unwrap().clone();
    moved.shape.parent = ROOT.into();
    before.set_remote("f".into(), moved);
    trash(&mut before, "d");

    let mut trees = restarted(&before);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
    trees.rules_scanned();
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["f"]);
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "f.docx", 1)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_node_whose_copy_vanished_while_excluded_keeps_its_place_until_the_rules_are_scanned_after_a_restart()
 {
    let mut before = trees(&[synced("a", file(ROOT, "a.log", 1))]);
    before.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut before), []);
    before.remove_local("a");
    assert_eq!(ops(&mut before), []);

    let mut trees = restarted(&before);
    assert!(!trees.rules_known());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    assert_eq!(
        trees
            .vanished_at(ROOT, "a.log", Kind::File)
            .map(String::as_str),
        Some("a")
    );

    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.rules_scanned();
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    assert_eq!(
        trees
            .vanished_at(ROOT, "a.log", Kind::File)
            .map(String::as_str),
        Some("a")
    );

    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["a"]);
    assert_eq!(trees.local().live("a"), Some(&file(ROOT, "a.log", 1)));
}

#[test]
fn an_excluded_file_lost_with_its_folder_downloads_as_the_same_node_after_the_folder_comes_back() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "dist")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n*.log\n"));
    assert_eq!(ops(&mut trees), []);
    trees.remove_local("log");
    trees.remove_local("d");
    assert_eq!(ops(&mut trees), []);

    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.listed(&[ROOT.into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d"]);
    assert!(trees.excluded("log"));
    assert_eq!(trees.synced().live("log"), None);
    assert_eq!(trees.local().live("log"), None);

    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into(), "d".into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["log"]);
    assert_eq!(trees.local().live("log"), Some(&file("d", "a.log", 1)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_node_whose_copy_vanished_below_a_folder_with_unread_rules_waits_for_them() {
    let mut before = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("f", file("d", "f.txt", 1)),
    ]);
    before.set_rules(&["d"], Source::Koiignore, rules("f.txt\n"));
    assert_eq!(ops(&mut before), []);
    assert!(before.unobserved("f"));
    before.remove_local("f");
    assert_eq!(ops(&mut before), []);

    let mut trees = restarted(&before);
    trees.set_rules_unknown("d");
    trees.rules_scanned();
    trees.listed(&[ROOT.into(), "d".into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    assert_eq!(
        trees
            .vanished_at("d", "f.txt", Kind::File)
            .map(String::as_str),
        Some("f")
    );

    trees.set_rules(&["d"], Source::Koiignore, rules("f.txt\n"));
    trees.set_rules_known("d");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    assert_eq!(
        trees
            .vanished_at("d", "f.txt", Kind::File)
            .map(String::as_str),
        Some("f")
    );

    trees.set_rules(&["d"], Source::Koiignore, None);
    trees.listed(&[ROOT.into(), "d".into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["f"]);
}

#[test]
fn a_file_moved_out_of_an_excluded_folder_keeps_its_move_when_the_folder_vanishes_and_comes_back() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "dist")),
        synced("f", file("d", "f.docx", 1)),
        synced("g", file("d", "g.docx", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
    assert_eq!(ops(&mut trees), []);
    place(&mut trees, "f", ROOT, "f.docx");
    trees.remove_local("g");
    trees.remove_local("d");
    let sent = ops(&mut trees);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        matches!(&sent[0], op::Op::Move(moved) if moved.node_id == "f"),
        "{sent:?}"
    );
    assert_eq!(trees.synced().get("f"), Some(&file("d", "f.docx", 1)));
    let mut moved = trees.remote().get("f").unwrap().clone();
    moved.shape.parent = ROOT.into();
    trees.set_remote("f".into(), moved);

    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into()], Pending::default());
    let mut done: Vec<String> = nodes(&effects(&mut trees))
        .into_iter()
        .map(String::from)
        .collect();
    done.sort();
    assert_eq!(done, ["d", "g"]);
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "f.docx", 1)));
    assert_eq!(trees.synced().get("f"), Some(&file(ROOT, "f.docx", 1)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_below_a_folder_renamed_out_of_the_rules_stays_observed_after_the_relisting() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "dist")),
        synced("sub", dir("d", "sub")),
        synced("f", file("sub", "f.docx", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "d", ROOT, "shipped");
    assert_eq!(trees.take_relist(), ["d"]);
    trees.listed(&["d".into(), "sub".into()], Pending::default());
    assert!(!trees.unobserved("f"));
    set_content(&mut trees, "f", 2);
    trees.settle(&Default::default());
    assert!(!trees.unobserved("f"), "a remote edit marks the file again");
    trees.remove_local("f");
    trees.settle(&Default::default());
    assert_eq!(trees.vanished_at("sub", "f.docx", Kind::File), None);
}

#[test]
fn a_folder_removed_with_only_an_excluded_file_left_in_it_stays_removed_after_its_included_file_is_deleted()
 {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("log", file("d", "a.log", 1)),
        synced("txt", file("d", "b.txt", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    for id in ["log", "txt", "d"] {
        trees.remove_local(id);
    }
    trees.listed(&[ROOT.into()], Pending::default());
    assert_eq!(ops(&mut trees), [delete("txt")]);
    let mut gone = trees.remote().get("txt").unwrap().clone();
    gone.trashed = true;
    trees.set_remote("txt".into(), gone);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.synced().get("txt"), None);
    assert_eq!(effects(&mut trees), []);
    assert!(trees.synced().get("d").is_some());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_removed_below_a_folder_renamed_into_the_rules_downloads_once_s_follows_the_rename() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "dist")),
        synced("sub", dir("d", "sub")),
        synced("f", file("sub", "f.docx", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("dist/\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "d", ROOT, "shipped");
    assert_eq!(trees.take_relist(), ["d"]);
    trees.remove_local("f");
    trees.listed(&[ROOT.into(), "d".into(), "sub".into()], Pending::default());
    let sent = ops(&mut trees);
    assert!(
        matches!(sent.as_slice(), [op::Op::Move(moved)] if moved.node_id == "d"),
        "{sent:?}"
    );
    assert_eq!(trees.synced().get("d"), Some(&dir(ROOT, "dist")));
    assert!(trees.synced().get("f").is_some());
    let mut moved = trees.remote().get("d").unwrap().clone();
    moved.shape.name = "shipped".into();
    trees.set_remote("d".into(), moved);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(trees.synced().get("d"), Some(&dir(ROOT, "shipped")));
    assert_eq!(nodes(&effects(&mut trees)), ["f"]);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_renamed_out_of_the_rules_downloads_again_at_its_synced_place_when_its_new_folder_is_deleted()
 {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("f", file("d", "f.docx", 1)),
        synced("p", dir(ROOT, "p")),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "f", "p", "f.log");
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("f"));
    trees.remove_local("p");
    trees.remove_local("f");
    assert_eq!(ops(&mut trees), [delete("p")]);
    assert_eq!(nodes(&effects(&mut trees)), ["f"]);
    assert_eq!(trees.local().live("f"), Some(&file("d", "f.docx", 1)));
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_removed_below_a_folder_renamed_back_into_the_rules_before_a_scan_lists_it_downloads_again()
 {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("f", file("d", "f.docx", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "d", ROOT, "d.log");
    trees.settle(&Default::default());
    assert!(trees.unobserved("f"));
    rename_here(&mut trees, "d", ROOT, "d");
    assert_eq!(trees.take_relist(), ["d"]);
    assert!(!trees.excluded("f") && trees.unobserved("f"));
    trees.remove_local("f");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["f"]);
    assert_eq!(trees.local().live("f"), Some(&file("d", "f.docx", 1)));
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_folder_deleted_with_a_file_that_vanished_while_renamed_out_of_the_rules_stays_for_that_file() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("f", file("d", "f.docx", 1)),
        synced("g", file("d", "g.docx", 2)),
        synced("p", dir(ROOT, "p")),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    rename_here(&mut trees, "f", "p", "f.log");
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("f"));
    for id in ["f", "p", "g", "d"] {
        trees.remove_local(id);
    }
    assert_eq!(ops(&mut trees), [delete("p"), delete("g")]);
    trash(&mut trees, "p");
    trash(&mut trees, "g");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d", "f"]);
    assert_eq!(trees.local().live("d"), Some(&dir(ROOT, "d")));
    assert_eq!(trees.local().live("f"), Some(&file("d", "f.docx", 1)));
    assert_eq!(ops(&mut trees), []);
    assert!(trees.work().is_empty());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_renamed_out_of_the_rules_downloads_again_however_its_folder_is_moved_renamed_or_deleted()
{
    let applied = |trees: &mut Trees, sent: Vec<op::Op>| {
        for op in sent {
            match op {
                op::Op::Delete(deleted) => {
                    let mut below = vec![deleted.node_id];
                    while let Some(id) = below.pop() {
                        below.extend(
                            (trees.remote().iter())
                                .filter(|(_, node)| !node.trashed && node.shape.parent == id)
                                .map(|(child, _)| child.clone()),
                        );
                        assert_ne!(id, "f");
                        trash(trees, &id);
                    }
                }
                op::Op::Move(moved) => {
                    let mut node = trees.remote().get(&moved.node_id).unwrap().clone();
                    (node.shape.parent, node.shape.name) = (moved.new_parent_id, moved.new_name);
                    trees.set_remote(moved.node_id, node);
                }
                other => panic!("{other:?}"),
            }
        }
    };
    let renamed_there = |trees: &mut Trees| {
        let mut node = trees.remote().get("d").unwrap().clone();
        node.shape.name = "e".into();
        trees.set_remote("d".into(), node);
    };
    for case in 0..7 {
        let mut trees = trees(&[
            synced("a", dir(ROOT, "a")),
            synced("d", dir("a", "d")),
            synced("f", file("d", "f.docx", 1)),
            synced("g", file("d", "g.docx", 2)),
            synced("h", file("a", "h.docx", 3)),
            synced("p", dir(ROOT, "p")),
            synced("q", dir(ROOT, "q")),
        ]);
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
        assert_eq!(ops(&mut trees), []);
        if case == 5 {
            renamed_there(&mut trees);
        }
        rename_here(&mut trees, "f", "p", "f.log");
        assert_eq!(ops(&mut trees), []);
        trees.remove_local("f");
        trees.remove_local("p");
        let gone: &[&str] = match case {
            0 => {
                rename_here(&mut trees, "d", "q", "d2");
                &["g"]
            }
            1 | 2 => {
                rename_here(&mut trees, "d", "q", "d");
                &["g", "d", "q"]
            }
            3 | 4 => {
                rename_here(&mut trees, "d", "a", "d2");
                &["g", "d"]
            }
            _ => &["g", "d", "h", "a"],
        };
        if matches!(case, 2 | 4) {
            let sent = ops(&mut trees);
            applied(&mut trees, sent);
        }
        for id in gone {
            trees.remove_local(id);
        }
        if case == 6 {
            let sent = ops(&mut trees);
            renamed_there(&mut trees);
            applied(&mut trees, sent);
        }
        for _ in 0..4 {
            let sent = ops(&mut trees);
            applied(&mut trees, sent);
            effects(&mut trees);
        }
        assert!(trees.work().is_empty(), "case {case}: {:?}", trees.work());
        assert!(trees.remote().live("f").is_some(), "case {case}");
        assert_eq!(
            trees.local().live("f"),
            trees.remote().live("f"),
            "case {case}"
        );
        assert!(!trees.unobserved("f"), "case {case}");
        assert_eq!(trees.check(), Ok(()), "case {case}");
    }
}

#[test]
fn a_file_whose_folder_is_deleted_around_it_downloads_again_after_a_restart_without_another_scan() {
    let mut before = trees(&[
        synced("d", dir(ROOT, "d")),
        synced("f", file("d", "f.docx", 1)),
        synced("g", file("d", "g.docx", 2)),
        synced("p", dir(ROOT, "p")),
    ]);
    before.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut before), []);
    rename_here(&mut before, "f", "p", "f.log");
    assert_eq!(ops(&mut before), []);
    for id in ["f", "p", "g", "d"] {
        before.remove_local(id);
    }
    assert_eq!(ops(&mut before), [delete("p"), delete("g")]);

    let mut trees = restarted(&before);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.rules_scanned();
    trash(&mut trees, "p");
    trash(&mut trees, "g");
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d", "f"]);
    assert_eq!(trees.local().live("f"), Some(&file("d", "f.docx", 1)));
    assert_eq!(ops(&mut trees), []);
    assert!(trees.work().is_empty());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_a_scan_reports_as_l_holds_it_after_the_rules_let_it_in_again_is_deleted_once_removed() {
    let mut trees = trees(&[synced("f", file(ROOT, "f.docx", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.docx\n"));
    assert_eq!(ops(&mut trees), []);
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("f"));
    let held = trees.local().get("f").unwrap().clone();
    trees.set_local_changed("f".into(), held);
    trees.settle(&Default::default());
    assert!(!trees.unobserved("f"));
    trees.remove_local("f");
    let sent = ops(&mut trees);
    assert!(
        matches!(sent.as_slice(), [op::Op::Delete(deleted)] if deleted.node_id == "f"),
        "{sent:?}"
    );
    assert!(nodes(&effects(&mut trees)).is_empty());
}

#[test]
fn a_file_the_rules_of_the_folder_it_moves_into_leave_out_downloads_again_after_it_vanishes() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p")),
        synced("log", file("p", "a.log", 1)),
        synced("q", dir(ROOT, "q")),
    ]);
    trees.set_rules(&["q"], Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    place(&mut trees, "p", "q", "p");
    let sent = ops(&mut trees);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let mut moved = trees.remote().get("p").unwrap().clone();
    moved.shape.parent = "q".into();
    trees.set_remote("p".into(), moved);
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("log"));
    trees.remove_local("log");
    assert_eq!(ops(&mut trees), []);
    trees.set_rules(&["q"], Source::Koiignore, None);
    trees.listed(&["q".into(), "p".into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["log"]);
}

fn excluded_and_removed_in_one_batch(folder: bool, rules_first: bool, stopped: bool) -> Trees {
    let (before, rule, gone): (Trees, &str, &[&str]) = match folder {
        true => (
            trees(&[
                synced("d", dir(ROOT, "d")),
                synced("f", file("d", "f.docx", 1)),
            ]),
            "d/\n",
            &["f", "d"],
        ),
        false => (
            trees(&[synced("f", file(ROOT, "f.docx", 1))]),
            "*.docx\n",
            &["f"],
        ),
    };
    let mut trees = match stopped {
        true => restarted(&before),
        false => before,
    };
    let exclude = |trees: &mut Trees| {
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules(rule));
        trees.rules_scanned();
    };
    if rules_first {
        exclude(&mut trees);
    }
    for id in gone {
        trees.remove_local(id);
    }
    if !rules_first {
        exclude(&mut trees);
    }
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[ROOT.into()], Pending::default());
    trees
}

#[test]
fn a_copy_removed_in_the_batch_whose_rules_exclude_it_downloads_again() {
    for (folder, downloaded) in [(false, &["f"][..]), (true, &["d", "f"][..])] {
        let mut trees = excluded_and_removed_in_one_batch(folder, true, false);
        assert_eq!(ops(&mut trees), [], "folder: {folder}");
        assert_eq!(nodes(&effects(&mut trees)), downloaded, "folder: {folder}");
        assert_eq!(trees.check(), Ok(()));
    }
}

#[test]
fn a_copy_removed_before_the_rules_excluded_it_is_deleted_once_they_let_it_in_again() {
    for (folder, top) in [(false, "f"), (true, "d")] {
        let mut trees = excluded_and_removed_in_one_batch(folder, false, false);
        assert_eq!(ops(&mut trees), [delete(top)], "folder: {folder}");
        assert_eq!(effects(&mut trees), [], "folder: {folder}");
    }
}

#[test]
fn a_copy_removed_while_the_device_was_stopped_through_a_rules_change_downloads_again_whichever_came_first()
 {
    for (folder, downloaded) in [(false, &["f"][..]), (true, &["d", "f"][..])] {
        let mut trees = excluded_and_removed_in_one_batch(folder, true, true);
        assert_eq!(ops(&mut trees), [], "folder: {folder}");
        assert_eq!(nodes(&effects(&mut trees)), downloaded, "folder: {folder}");
        assert_eq!(trees.check(), Ok(()));
    }
}

#[test]
fn a_folder_removed_while_left_out_comes_back_whole_with_a_removed_folder_the_server_keeps_for_an_excluded_file()
 {
    let mut trees = trees(&[
        synced("a", dir(ROOT, "a")),
        synced("A", file("a", "A.docx", 1)),
        synced("ss", dir("a", "ss")),
        incoming("c", file("ss", "c.txt", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("c.txt\n"));
    assert_eq!(ops(&mut trees), []);
    trees.remove_local("ss");
    assert_eq!(ops(&mut trees), []);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("c.txt\na/\n"));
    assert_eq!(ops(&mut trees), []);
    assert!(trees.unobserved("a") && trees.unobserved("A"));
    trees.remove_local("A");
    trees.remove_local("a");
    assert_eq!(ops(&mut trees), []);

    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("c.txt\n"));
    trees.listed(&[ROOT.into()], Pending::default());
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["a", "A", "ss"]);
    let left: Vec<String> = (trees.work().into_iter())
        .map(|(id, _)| id)
        .filter(|id| !trees.excluded(id))
        .collect();
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_released_file_moved_into_a_folder_whose_release_waits_on_unknown_rules_keeps_the_trees_consistent()
 {
    let mut trees = trees(&[
        synced("u", dir(ROOT, "u")),
        synced("v", dir("u", "v")),
        synced("f", file(ROOT, "f.docx", 1)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("v/\nf.docx\n"));
    assert_eq!(ops(&mut trees), []);
    trees.remove_local("f");
    trees.remove_local("v");
    assert_eq!(ops(&mut trees), []);
    trees.set_rules_unknown("u");
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    ops(&mut trees);
    assert!(trees.synced().get("f").is_none() && trees.synced().get("v").is_some());
    trees.set_remote("f".into(), remote(file("v", "f.docx", 1)));
    trees.settle(&Default::default());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_vanished_node_below_a_folder_the_user_moved_waits_while_its_place_on_disk_is_pending() {
    for (pending, held) in [("out/a.log", true), ("dist/a.log", false)] {
        let mut trees = trees(&[
            synced("d", dir(ROOT, "dist")),
            synced("a", file("d", "a.log", 1)),
        ]);
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
        assert_eq!(ops(&mut trees), []);
        trees.remove_local("a");
        place(&mut trees, "d", ROOT, "out");
        trees.set_rules(&AT_ROOT, Source::Koiignore, None);
        trees.listed(&[ROOT.into()], Pending::new([pending.split('/')]));
        trees.settle(&Default::default());
        assert_eq!(
            trees.synced().live("a").is_some(),
            held,
            "pending {pending}"
        );
        assert_eq!(trees.check(), Ok(()));
        trees.listed(&[ROOT.into()], Pending::default());
        trees.settle(&Default::default());
        assert_eq!(trees.synced().live("a"), None, "pending {pending}");
        assert_eq!(trees.check(), Ok(()));
    }
}

#[test]
fn a_folder_whose_copy_goes_while_pending_keeps_what_a_release_took_from_it_waiting() {
    let mut trees = trees(&[
        synced("p", dir(ROOT, "p.log")),
        synced("x", file("p", "x.txt", 1)),
        synced("y", file("p", "y.txt", 2)),
    ]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    trees.remove_local("x");
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    trees.listed(&[], Pending::default());
    trees.settle(&Default::default());
    assert_eq!(trees.synced().live("x"), None);
    trees.remove_local("y");
    trees.remove_local("p");
    trees.listed(&[], Pending::new([["p.log"]]));
    trees.settle(&Default::default());
    assert!(trees.synced().live("p").is_some());
    assert_eq!(trees.check(), Ok(()));
    trees.listed(&[], Pending::default());
    trees.settle(&Default::default());
    assert_eq!(trees.synced().live("p"), None);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_create_removed_while_excluded_leaves_no_mark_once_no_commit_sends_it() {
    let mut trees = trees(&[]);
    trees.set_local("n".into(), local(file(ROOT, "n.log", 1), 1));
    assert!(matches!(ops(&mut trees)[..], [op::Op::CreateFile(_)]));
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    trees.remove_local("n");
    trees.settle(&Default::default());
    assert_eq!(trees.check(), Ok(()));
    trees.watch_positions([]);
    trees.settle(&Default::default());
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn a_file_waiting_under_a_name_that_differs_only_in_case_keeps_its_place_pending() {
    let mut trees = trees(&[synced("x", file(ROOT, "x.log", 1))]);
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
    assert_eq!(ops(&mut trees), []);
    trees.remove_local("x");
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    let renamed =
        Pending::new(Vec::<Vec<&str>>::new()).waiting([(["X.log"], true), (["x.log"], false)]);
    trees.listed(&[], renamed);
    trees.settle(&Default::default());
    assert!(trees.synced().live("x").is_some());
    trees.listed(&[], Pending::default().waiting([(["X.log"], false)]));
    trees.settle(&Default::default());
    assert_eq!(trees.synced().live("x"), None);
    assert_eq!(trees.check(), Ok(()));
}

#[test]
fn an_excluded_node_the_server_renames_keeps_its_synced_entries_while_their_old_place_is_pending() {
    for (renamed, to, pending) in [
        ("x", "y.log", vec!["x.log"]),
        ("x", "y.txt", vec!["x.log"]),
        ("x", "y.log", vec![]),
        ("p", "r.log", vec!["p.log", "x.log"]),
    ] {
        let parent = if renamed == "p" { "p" } else { ROOT };
        let mut trees = trees(&[
            synced("p", dir(ROOT, "p.log")),
            synced("x", file(parent, "x.log", 1)),
        ]);
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
        assert_eq!(ops(&mut trees), []);
        trees.remove_local("x");
        trees.remove_local(renamed);
        trees.settle(&Default::default());
        let mut node = trees.remote().get(renamed).unwrap().clone();
        node.shape.name = to.into();
        trees.set_remote(renamed.into(), node);
        trees.listed(&[], Pending::new([pending.clone()]));
        for _ in 0..3 {
            trees.settle(&Default::default());
            assert_eq!(trees.check(), Ok(()));
        }
        assert_ne!(trees.synced().live("x"), None, "{renamed} {to} {pending:?}");
        trees.listed(&[], Pending::default());
        trees.settle(&Default::default());
        assert_eq!(trees.synced().live("x"), None, "{renamed} {to} {pending:?}");
        assert_eq!(trees.check(), Ok(()));
    }
}

#[test]
fn an_excluded_file_the_server_moves_keeps_its_synced_entry_while_its_place_in_r_is_pending() {
    for folder_in_s in [false, true] {
        let folder = match folder_in_s {
            true => synced("p", dir(ROOT, "a")),
            false => incoming("p", dir(ROOT, "dest")),
        };
        let mut trees = trees(&[synced("x", file(ROOT, "x.log", 1)), folder]);
        trees.set_rules(&AT_ROOT, Source::Koiignore, rules("*.log\n"));
        ops(&mut trees);
        trees.remove_local("x");
        if folder_in_s {
            trees.remove_local("p");
        }
        trees.settle(&Default::default());
        for (id, parent, name) in [("p", ROOT, "dest"), ("x", "p", "y.log")] {
            let mut node = trees.remote().get(id).unwrap().clone();
            node.shape.parent = parent.into();
            node.shape.name = name.into();
            trees.set_remote(id.into(), node);
        }
        trees.listed(&[], Pending::new([["dest"]]));
        for _ in 0..3 {
            trees.settle(&Default::default());
            assert_eq!(trees.check(), Ok(()));
        }
        assert_ne!(trees.synced().live("x"), None, "{folder_in_s}");
        trees.listed(&[], Pending::default());
        trees.settle(&Default::default());
        assert_eq!(trees.synced().live("x"), None, "{folder_in_s}");
        assert_eq!(trees.check(), Ok(()));
    }
}

fn excluded_folder_released_with_its_file() -> Trees {
    let mut trees = trees(&[
        incoming("far", dir(ROOT, "far")),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(dir("far", "x")),
        ),
        synced("z", file("x", "g", 1)),
    ]);
    let parked = LocalNode {
        parked: true,
        ..local(dir(ROOT, "x"), 2)
    };
    trees.set_local("x".into(), parked);
    trees.set_local("y".into(), local(file(ROOT, "x", 2), 4));
    trees.set_synced("y".into(), Some(file("x", "f", 2)));
    trees.set_remote("y".into(), remote(file(ROOT, "x", 2)));
    trees.set_rules(&AT_ROOT, Source::Koiignore, rules("/far/\n"));
    trees.settle(&Default::default());
    trees
}

#[test]
fn another_devices_trash_of_an_excluded_folder_is_not_undone() {
    let mut trees = excluded_folder_released_with_its_file();
    trash(&mut trees, "z");
    trash(&mut trees, "x");
    assert_eq!(ops(&mut trees), []);
}

#[test]
fn an_unchanged_excluded_file_takes_the_remote_edit_when_included_again() {
    let mut trees = excluded_folder_released_with_its_file();
    set_content(&mut trees, "z", 9);
    trees.set_rules(&AT_ROOT, Source::Koiignore, None);
    effects(&mut trees);
    assert_eq!(ops(&mut trees), []);
    assert_eq!(
        trees.local().get("z").map(|node| &node.shape),
        Some(&file("x", "g", 9))
    );
}
