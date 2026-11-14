// SPDX-License-Identifier: Apache-2.0

//! Excluded nodes (docs/design/sync-engine.md section 5.10), on hand-built trees: the rules of
//! `.koiignore` files, the built-in exclusions and the device-local list decide which nodes this
//! device leaves alone, and becoming excluded never plans a delete or a removal.

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{self, Base, BatchLimit, Effect, Effects, Location, Stuck};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{LocalRules, RuleFile};
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
        dev: 1,
        ino,
        birth_ns: Some(0),
    }
}

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        shape,
        identity: identity(ino),
        parked: false,
    }
}

/// A node's L, S and R.
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
    trees.settle();
    trees
}

/// The same shape in L, S and R.
fn synced(id: &'static str, shape: Shape) -> Row {
    (id, Some(shape.clone()), Some(shape.clone()), Some(shape))
}

/// A node only R holds.
fn incoming(id: &'static str, shape: Shape) -> Row {
    (id, None, None, Some(shape))
}

fn rules(text: &str) -> Option<RuleFile> {
    Some(RuleFile::parse(text.as_bytes()))
}

fn ops(trees: &mut Trees) -> Vec<op::Op> {
    trees.settle();
    let commit = Commits::default()
        .build(trees, &mut IdGen::new(1), NOW, HISTORY, "me", 7)
        .cloned();
    let ops = commit.map(|commit| commit.ops).unwrap_or_default();
    ops.into_iter().map(|op| op.op.unwrap()).collect()
}

/// Runs the effects until none is left and returns each with its node, in order.
fn effects(trees: &mut Trees) -> Vec<(String, Effect)> {
    let (mut effects, mut ids, mut done) = (Effects::default(), IdGen::new(2), Vec::new());
    loop {
        trees.settle();
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
    trees.set_rules(&AT_ROOT, rules("logs/\n.koiignore\n"));
    assert!(["logs", "old", "new"].iter().all(|id| trees.excluded(id)));
    assert!(!trees.excluded("kept"));
    assert!(!trees.excluded("rules"), "a rules file cannot hide itself");
    trees.set_rules(&AT_ROOT, None);
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("/build/\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    set_content(&mut trees, "a", 7);
    assert_eq!(effects(&mut trees), []);

    trees.set_rules(&AT_ROOT, rules("# nothing\n"));
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
    trees.set_rules(&AT_ROOT, rules("!.DS_Store\n!Thumbs.db\n!.koidrive/\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    place(&mut trees, "a", ROOT, "a.log");
    assert_eq!(ops(&mut trees), [], "neither a move nor a delete is sent");
    set_content(&mut trees, "a", 7);
    trash(&mut trees, "a");
    assert_eq!(effects(&mut trees), [], "its local copy is left as it is");
}

/// Another entry takes the name of a folder L keeps for its copy in an ignored folder. The folder and the file in it
/// leave L and stay excluded, also after a restart. Each takes part in sync again once an entry with its identity is
/// placed, or once R holds it trashed.
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
        assert!(trees.local().get(id).is_none() && trees.excluded(id));
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

    assert_eq!(trees.displaced_with_identity(&held), Some(&"a".to_owned()));
    let undated = trees.displaced_local()["b"];
    assert_eq!(undated.birth_ns, None);
    assert_eq!(
        trees.displaced_with_identity(&undated),
        None,
        "a record without a birth time matches no file"
    );
    trees.set_local("a".into(), local(file(ROOT, "back.txt", 1), 2));
    assert!(!trees.excluded("a") && trees.displaced_with_identity(&held).is_none());
    trash(&mut trees, "b");
    assert!(!trees.excluded("b"));
    assert_eq!(
        trees.displaced_local().keys().collect::<Vec<_>>(),
        ["d"],
        "the folder is still out"
    );
}

/// A node that R does not hold live has nothing to keep on the server, so it leaves L without a record.
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
    trees.set_rules(&AT_ROOT, rules("/build/\n"));
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

#[test]
fn a_node_the_user_moves_out_of_an_ignored_path_is_moved_on_the_server() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.log", 1))]);
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("/build/\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    assert!(trees.kept_for_excluded()("d"));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(nodes(&effects(&mut trees)), ["d"]);
    assert_eq!(trees.local().live("d"), Some(&dir("m", "d")));
    assert_eq!(trees.synced().live("d"), Some(&dir("m", "d")));
    assert_eq!(trees.synced().live("new"), Some(&file(ROOT, "d", 2)));
    assert_eq!(trees.check(), Ok(()));
    trees.set_rules(&AT_ROOT, None);
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
        trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("a.txt\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("/archive/\n"));
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

/// A parked folder whose old name "docs" another node took, and whose R position the rules now exclude, with an
/// excluded file in it.
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
    trees.set_rules(&AT_ROOT, rules("/archive/\n*.log\n"));
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
        effects::stuck(&trees, &Effects::default(), None),
        BTreeMap::from([("old".into(), Stuck::Parked)])
    );
}

/// The child sorts before its folder, so settling classifies it before it drops the folder from S with the child: in
/// sync, which takes it out of the dirty set, or edited. The child's S entry is gone, so it is classified again, once,
/// and the state loads.
#[test]
fn a_child_settled_before_its_folder_leaves_s_is_classified_again() {
    for digest in [1, 2] {
        let (mut trees, _) = parked_excluded_folder();
        trees.set_local("child".into(), local(file("old", "build.log", digest), 2));
        trees.settle();
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
    trees.set_rules(&AT_ROOT, rules("/x/\n/y/\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    trees.rules_scanned();
    assert!(trees.excluded("old") && trees.waiting().contains_key("old"));
    assert_eq!(effects(&mut trees), []);
    assert!(!trees.local().get("old").unwrap().parked);
    assert!(!trees.local().get("draft").unwrap().parked);
}

#[test]
fn s_of_an_excluded_node_takes_what_l_and_r_both_hold() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.log", 1))]);
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    set_content(&mut trees, "a", 7);
    trees.set_local("a".into(), local(file(ROOT, "a.log", 7), 1));
    trees.set_local("b".into(), local(file(ROOT, "b.log", 2), 2));
    trees.set_remote("b".into(), remote(file(ROOT, "b.log", 2)));
    assert!(trees.excluded("a") && trees.excluded("b"));
    trees.settle();
    assert_eq!(trees.work(), [], "settling alone takes both");
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "a.log", 7)));
    assert_eq!(trees.synced().live("b"), Some(&file(ROOT, "b.log", 2)));
    assert_eq!(ops(&mut trees), []);
    assert_eq!(effects(&mut trees), []);
}

#[test]
fn a_remote_rules_file_is_written_before_anything_else_below_its_folder() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        incoming("a-outside", file(ROOT, "z.txt", 1)),
        incoming("b-secret", file("d", "secret.txt", 2)),
        incoming("c-sub", dir("d", "sub")),
        incoming("d-deep", file("c-sub", "deep.txt", 3)),
        incoming("z-rules", file("d", ".koiignore", 4)),
    ]);
    let (mut effects, mut ids, mut order) = (Effects::default(), IdGen::new(2), Vec::new());
    loop {
        trees.settle();
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
            trees.set_rules(&["d"], rules("secret.txt\n"));
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
    before.set_rules(&AT_ROOT, rules("*.log\n"));
    before.remove_local("log");
    before.remove_local("txt");
    trash(&mut before, "gone");
    before.settle();

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

    trees.set_rules(&AT_ROOT, rules("*.log\n"));
    trees.rules_scanned();
    assert_eq!(ops(&mut trees), [delete("txt")]);
    assert_eq!(effects(&mut trees), []);
}

/// A case-sensitive volume holds folders whose names differ only in case, and L holds them as two
/// nodes. The rules of one being unknown holds nothing below the other.
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

/// Reading the rules of one of two folders whose names differ only in case says nothing about the
/// rules of the other, which stay unknown.
#[test]
fn reading_a_folder_keeps_the_hold_of_its_namesake_in_another_case() {
    let mut trees = trees(&[
        ("lower", Some(dir(ROOT, "proj")), None, None),
        ("upper", Some(dir(ROOT, "PROJ")), None, None),
        ("unknown", Some(file("upper", "file.txt", 1)), None, None),
    ]);
    assert_eq!(trees.check(), Ok(()));
    trees.set_rules_unknown("upper");
    trees.set_rules(&["proj"], None);
    trees.set_rules_known("lower");
    assert!(trees.held("unknown"));
}

/// A folder L no longer holds has no rules on this disk, so the nodes S and R still place below it
/// are planned again, and the deletion of the folder can be sent.
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
    trees.set_rules(&AT_ROOT, rules("ss/\n"));
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
    trees.set_rules(&AT_ROOT, rules("ss/\n"));
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
    trees.set_rules(&AT_ROOT, rules("ss/\n"));
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
    trees.set_rules(&AT_ROOT, rules("/y/c\n"));
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
    trees.set_rules(&AT_ROOT, rules("/y/a/c\n"));
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
    trees.set_rules(&AT_ROOT, rules("/y/a\n"));
    assert!(!trees.excluded("p") && trees.excluded("a"));
    assert!(trees.excluded("x"));
    assert!(!nodes(&effects(&mut trees)).contains(&"x"));
}

#[test]
fn a_folder_renamed_here_keeps_planning_below_it_fast() {
    let (depth, files) = (60, 500);
    let mut trees = Trees::new(ROOT);
    for i in 0..depth {
        let parent = if i == 0 {
            ROOT.to_string()
        } else {
            format!("d{}", i - 1)
        };
        let shape = dir(&parent, &format!("n{i}"));
        let here = if i == 0 {
            dir(ROOT, "renamed")
        } else {
            shape.clone()
        };
        trees.set_local(format!("d{i}"), local(here, i as u128 + 1));
        trees.set_synced(format!("d{i}"), Some(shape.clone()));
        trees.set_remote(format!("d{i}"), remote(shape));
    }
    let bottom = format!("d{}", depth - 1);
    for j in 0..files {
        let leaf = file(&bottom, &format!("f{j}.txt"), 1);
        trees.set_local(format!("f{j}"), local(leaf.clone(), 1000 + j as u128));
        trees.set_synced(format!("f{j}"), Some(leaf.clone()));
        trees.set_remote(format!("f{j}"), remote(leaf));
        trees.set_remote(
            format!("g{j}"),
            remote(file(&bottom, &format!("g{j}.txt"), 2)),
        );
    }
    trees.set_rules(&AT_ROOT, rules("ss/\n"));
    let started = std::time::Instant::now();
    assert_eq!(ops(&mut trees).len(), 1);
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(20),
        "planning took {took:?}"
    );
    assert!(!trees.excluded("g0"));
}

#[test]
fn a_file_another_device_owed_a_rename_to_an_ignored_name_stays_where_it_is() {
    let mut trees = trees(&[synced("a", file(ROOT, "a.txt", 1))]);
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
    trees.set_rules(&AT_ROOT, rules("*.log\n"));
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
fn a_kept_folder_another_device_renames_comes_back_and_the_child_the_rules_let_in_is_deleted() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "ss")),
        synced("log", file("d", "a.log", 1)),
    ]);
    trees.set_rules(&AT_ROOT, rules("/ss/a.log\n"));
    trees.remove_local("log");
    trees.remove_local("d");
    trees.set_remote("d".into(), remote(dir(ROOT, "b")));
    assert!(!trees.ignored("d"));
    assert!(trees.excluded("log"));
    assert!(trees.kept_for_excluded()("d"));
    assert_eq!(ops(&mut trees), []);
    let done: Vec<String> = effects(&mut trees).into_iter().map(|(id, _)| id).collect();
    assert_eq!(done, ["d"]);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(ops(&mut trees), [delete("log")]);
}
