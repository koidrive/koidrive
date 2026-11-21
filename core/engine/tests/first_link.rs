// SPDX-License-Identifier: Apache-2.0

//! Matching a first link's local entries against the server's tree (docs/design/sync-engine.md
//! section 6.4).

use std::collections::BTreeSet;

use koidrive_engine::classify::{Origin, Pending, Trees, Waiting};
use koidrive_engine::commit::{Commits, Failure, Halt, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, Effects, Location, Stuck, stuck};
use koidrive_engine::first_link::{History, PriorSync, adopt};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{RuleFile, Source};
use koidrive_engine::names::conflict_name;
use koidrive_engine::question::{Answer, Question, QuestionKind, RecoveryGroup};
use koidrive_engine::restore::RestoreIntent;
use koidrive_engine::state::{CommitsState, TreesState};
use koidrive_engine::tree::{
    Content, ContentKey, FileIdentity, Kind, LocalNode, Names, NodeId, RemoteNode, Shape,
};
use koidrive_proto::v1::{CommitResult, Error, OpResult, Welcome, error, op, op_result};
use koidrive_sim::world::key;

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";

fn content(digest: u8) -> ContentKey {
    key(&Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    })
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

/// Trees with `local` in L and `remote` in R, and nothing in S, as on a first link.
fn trees(local: &[(&str, Shape)], remote: &[(&str, Shape)]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in local.iter().enumerate() {
        let identity = FileIdentity {
            ino: ino as u128 + 1,
            birth_ns: Some(0),
            handle: None,
        };
        let node = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
            chunks: Box::default(),
        };
        trees.set_local((*id).into(), node);
    }
    for (id, shape) in remote {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
            chunks: Box::default(),
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle(&Default::default());
    trees
}

fn mark(trees: &mut Trees, id: &str, stem: &str) {
    let mut node = trees.remote().get(&NodeId::from(id)).expect("in R").clone();
    node.pending_for = Some(stem.into());
    trees.set_remote(id.into(), node);
    trees.settle(&Default::default());
}

/// The local nodes by ID with their parent and name, in order.
fn local(trees: &Trees) -> Vec<(NodeId, NodeId, String)> {
    let nodes = trees.local().iter();
    nodes
        .map(|(id, node)| {
            (
                id.clone(),
                node.shape.parent.clone(),
                node.shape.name.clone(),
            )
        })
        .collect()
}

#[test]
fn an_entry_a_store_lost_track_of_is_adopted_by_a_place_the_log_shows_its_node_left() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1))],
        &[("r-a", file(ROOT, "b", 1))],
    );
    let history = History::from([("r-a".into(), vec![file(ROOT, "a", 1)])]);
    assert_eq!(adopt(&mut trees, Some(&history)), Vec::<String>::new());
    assert_eq!(local(&trees), [("r-a".into(), ROOT.into(), "a".into())]);
    assert_eq!(
        trees.synced().live(&"r-a".into()),
        Some(&file(ROOT, "a", 1))
    );
}

fn written_at(name: &str, mtime_ns: i64) -> Shape {
    let mut content = content(1);
    content.mtime_ns = mtime_ns;
    Shape {
        content: Some(content),
        ..file(ROOT, name, 1)
    }
}

#[test]
fn a_new_file_on_a_coarse_disk_with_a_lost_nodes_bytes_is_not_adopted_by_the_nodes_old_place() {
    for local_ns in [5_000_000_000, 4_000_000_000, 1_240_000_000] {
        let mut trees = trees(
            &[("l-a", written_at("a", local_ns))],
            &[("r-a", written_at("b", 1_234_567_891))],
        );
        let history = History::from([("r-a".into(), vec![written_at("a", 1_234_567_891)])]);
        assert_eq!(adopt(&mut trees, Some(&history)), ["l-a"], "{local_ns}");
    }
}

#[test]
fn a_download_whose_time_a_coarse_disk_cut_is_adopted_by_its_nodes_old_place() {
    for local_ns in [
        1_234_567_891,
        1_234_567_800,
        1_234_567_000,
        1_230_000_000,
        1_000_000_000,
        0,
    ] {
        let mut trees = trees(
            &[("l-a", written_at("a", local_ns))],
            &[("r-a", written_at("b", 1_234_567_891))],
        );
        let history = History::from([("r-a".into(), vec![written_at("a", 1_234_567_891)])]);
        assert_eq!(
            adopt(&mut trees, Some(&history)),
            Vec::<String>::new(),
            "{local_ns}"
        );
        assert_eq!(local(&trees), [("r-a".into(), ROOT.into(), "a".into())]);
    }
}

#[test]
fn a_place_the_log_shows_two_nodes_left_with_the_same_bytes_adopts_neither() {
    for remote in [
        &[("r-b", file(ROOT, "c", 2))][..],
        &[("r-a", file(ROOT, "b", 1)), ("r-b", file(ROOT, "c", 2))],
    ] {
        let mut trees = trees(&[("l-a", file(ROOT, "a", 1))], remote);
        let history = History::from([
            ("r-a".into(), vec![file(ROOT, "a", 1)]),
            ("r-b".into(), vec![file(ROOT, "a", 1)]),
        ]);
        assert_eq!(adopt(&mut trees, Some(&history)), ["l-a"], "{remote:?}");
    }
}

#[test]
fn an_entry_with_bytes_the_server_has_since_replaced_at_its_place_is_held() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1))],
        &[("r-a", file(ROOT, "a", 2))],
    );
    let history = History::from([("r-a".into(), vec![file(ROOT, "a", 1), file(ROOT, "a", 2)])]);
    assert_eq!(adopt(&mut trees, Some(&history)), ["l-a"]);
}

#[test]
fn a_place_the_log_shows_is_not_adopted_when_s_would_hold_a_cycle() {
    let mut trees = trees(
        &[("p", dir(ROOT, "p")), ("l-d", dir("p", "n"))],
        &[("x", dir(ROOT, "x")), ("p", dir("x", "p"))],
    );
    trees.set_synced("x".into(), Some(dir(ROOT, "x")));
    trees.set_synced("p".into(), Some(dir("x", "p")));
    trees.settle(&Default::default());
    let history = History::from([("x".into(), vec![dir("p", "n")])]);
    assert_eq!(adopt(&mut trees, Some(&history)), ["l-d"]);
    assert_eq!(trees.synced().check(Names::ByKey), Ok(()));
}

#[test]
fn an_entry_the_server_holds_with_the_same_name_and_bytes_is_adopted() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
        ],
    );
    assert_eq!(adopt(&mut trees, None), Vec::<String>::new());
    assert_eq!(
        local(&trees),
        [
            ("r-a".into(), "r-docs".into(), "a".into()),
            ("r-docs".into(), ROOT.into(), "docs".into())
        ]
    );
    trees.settle(&Default::default());
    assert!(trees.dirty().is_empty(), "{:?}", trees.dirty());
}

#[test]
fn a_first_link_holds_an_entry_like_one_in_the_servers_trash() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1))],
        &[("r-b", file(ROOT, "b", 2))],
    );
    let trashed = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "a", 1),
        trashed: true,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("r-a".into(), trashed);
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    commits.first_link(&mut trees, &mut IdGen::new(7), NOW, PriorSync::Shown);
    let held: Vec<&NodeId> = (commits.questions().iter())
        .flat_map(|question| question.holds())
        .collect();
    assert_eq!(held, ["l-a"]);
    assert!(trees.local().get(&"l-a".into()).is_some());
}

#[test]
fn a_folder_is_adopted_under_its_pending_name_when_a_file_holds_its_name() {
    let mut trees = trees(
        &[("l-docs", dir(ROOT, "docs"))],
        &[
            ("r-file", file(ROOT, "docs", 1)),
            ("r-docs", dir(ROOT, "docs (pending)")),
        ],
    );
    mark(&mut trees, "r-docs", "docs");
    assert_eq!(adopt(&mut trees, None), Vec::<String>::new());
    assert_eq!(
        local(&trees),
        [("r-docs".into(), ROOT.into(), "docs".into())]
    );
}

#[test]
fn a_folder_is_not_adopted_under_a_name_the_engine_never_gives_as_pending() {
    for name in ["docs (pending2)", "docs (pending 1)", "docs (pending  2)"] {
        let mut trees = trees(
            &[("l-docs", dir(ROOT, "docs"))],
            &[
                ("r-file", file(ROOT, "docs", 1)),
                ("r-docs", dir(ROOT, name)),
            ],
        );
        assert_eq!(adopt(&mut trees, None), ["l-docs"], "{name}");
    }
}

#[test]
fn a_folder_is_not_adopted_under_a_name_a_user_gave_that_looks_pending() {
    let mut trees = trees(
        &[("l-docs", dir(ROOT, "docs"))],
        &[
            ("r-file", file(ROOT, "docs", 1)),
            ("r-docs", dir(ROOT, "docs (pending)")),
        ],
    );
    assert_eq!(adopt(&mut trees, None), ["l-docs"]);
    assert_eq!(
        local(&trees),
        [("l-docs".into(), ROOT.into(), "docs".into())]
    );
}

#[test]
fn a_folder_two_pending_names_could_belong_to_is_held() {
    let mut trees = trees(
        &[("l-docs", dir(ROOT, "docs"))],
        &[
            ("r-one", dir(ROOT, "docs (pending)")),
            ("r-two", dir(ROOT, "docs (pending 2)")),
        ],
    );
    mark(&mut trees, "r-one", "docs");
    mark(&mut trees, "r-two", "docs");
    assert_eq!(adopt(&mut trees, None), ["l-docs"]);
    assert_eq!(
        local(&trees),
        [("l-docs".into(), ROOT.into(), "docs".into())]
    );
}

/// A folder both hold is adopted although its contents differ, and its children are matched one
/// by one: the file with other bytes and the file the server lacks are held, and the server's
/// other file is left for the effects to download.
#[test]
fn a_shared_folder_is_adopted_and_its_children_are_matched_one_by_one() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-same", file("l-docs", "same", 1)),
            ("l-edited", file("l-docs", "edited", 2)),
            ("l-mine", file("l-docs", "mine", 3)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-same", file("r-docs", "same", 1)),
            ("r-edited", file("r-docs", "edited", 9)),
            ("r-theirs", file("r-docs", "theirs", 4)),
        ],
    );
    let mut held = adopt(&mut trees, None);
    held.sort();
    assert_eq!(held, ["l-edited", "l-mine"]);
    assert!(trees.local().get(&"r-docs".into()).is_some());
    assert!(trees.local().get(&"r-same".into()).is_some());
    assert_eq!(
        trees.local().get(&"l-edited".into()).unwrap().shape.parent,
        "r-docs"
    );
    assert!(trees.synced().get(&"r-edited".into()).is_none());
}

/// A local folder the server lacks is held whole, as one item; a name that differs only by case,
/// and an entry of another kind, are held too.
#[test]
fn a_new_folder_a_name_that_only_folds_alike_and_another_kind_are_held() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 1)),
            ("l-report", file(ROOT, "Report.txt", 2)),
            ("l-notes", file(ROOT, "notes", 3)),
        ],
        &[
            ("r-report", file(ROOT, "report.txt", 2)),
            ("r-notes", dir(ROOT, "notes")),
        ],
    );
    let mut held = adopt(&mut trees, None);
    held.sort();
    assert_eq!(held, ["l-new", "l-notes", "l-report"]);
    assert!(trees.synced().iter().next().is_none());
}

/// An excluded entry is left out of the match: it is neither adopted nor held, even where the
/// server holds the same name.
#[test]
fn an_excluded_entry_is_neither_adopted_nor_held() {
    let mut trees = trees(
        &[
            ("l-logs", dir(ROOT, "logs")),
            ("l-old", file("l-logs", "old.log", 1)),
            ("l-cache", file(ROOT, "cache.log", 2)),
        ],
        &[("r-logs", dir(ROOT, "logs"))],
    );
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"*.log\n")),
    );
    assert_eq!(adopt(&mut trees, None), Vec::<String>::new());
    assert!(trees.local().get(&"r-logs".into()).is_some());
    assert!(trees.local().get(&"l-cache".into()).is_some());
    assert!(trees.synced().get(&"l-cache".into()).is_none());
    assert!(trees.synced().get(&"l-old".into()).is_none());
}

/// The ops of the next commit, each as its kind and node.
fn ops(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(&'static str, String)> {
    let Some(commit) = commits.build(trees, ids, NOW, HISTORY, "me", 1) else {
        return Vec::new();
    };
    (commit.ops.iter())
        .map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => ("create file", create.node_id.clone()),
            Some(op::Op::CreateDir(create)) => ("create dir", create.node_id.clone()),
            Some(op::Op::Move(moved)) => ("move", moved.node_id.clone()),
            Some(op::Op::UpdateContent(update)) => ("update", update.node_id.clone()),
            Some(op::Op::Delete(delete)) => ("delete", delete.node_id.clone()),
            Some(op::Op::Restore(restore)) => ("restore", restore.node_id.clone()),
            other => panic!("unexpected op {other:?}"),
        })
        .collect()
}

/// The one open question, which must be about entries created on this device.
fn question(commits: &Commits) -> Question {
    let [question] = commits.questions() else {
        panic!("one question: {:?}", commits.questions());
    };
    assert_eq!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        }
    );
    question.clone()
}

/// The space holds `docs/a`, and the folder linked to it holds the same `docs/a`, its own
/// `docs/b`, a `notes` file whose name the space gives other bytes, and a folder `new` with
/// one file.
fn linked() -> (Commits, Trees, IdGen) {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
            ("l-b", file("l-docs", "b", 2)),
            ("l-notes", file(ROOT, "notes", 3)),
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 4)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
            ("r-notes", file(ROOT, "notes", 9)),
        ],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    (commits, trees, ids)
}

#[test]
fn a_first_link_holds_what_matches_nothing_under_one_question_and_sends_nothing_for_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    let tops: Vec<&NodeId> = asked.held.keys().collect();
    assert_eq!(tops, ["l-b", "l-new", "l-notes"]);
    let mut held: Vec<&NodeId> = asked.holds().collect();
    held.sort();
    assert_eq!(held, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert_eq!(asked.held[&"l-new".into()].files, 1);
    assert!(held.iter().all(|id| trees.held(id)));
    assert!(!trees.held(&"r-docs".into()) && !trees.held(&"r-a".into()));

    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(
        question(&commits).id,
        asked.id,
        "nothing changed, so it is the same question"
    );
}

/// The space's `notes` has other bytes than the folder's, which is held: the effects neither
/// download the space's `notes` over it nor park it to make room.
#[test]
fn a_remote_node_whose_name_a_held_entry_takes_is_not_downloaded() {
    let (_, trees, mut ids) = linked();
    let mut effects = Effects::default();
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
}

#[test]
fn a_first_link_to_a_space_with_nothing_live_holds_nothing() {
    let mut trees = trees(&[("l-mine", file(ROOT, "mine", 1))], &[]);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    assert!(commits.questions().is_empty());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-mine".to_owned())]
    );
}

#[test]
fn a_first_link_to_a_space_listed_empty_holds_only_what_the_trash_names() {
    let mut trees = trees(
        &[
            ("l-budget", file(ROOT, "Budget.xlsx", 1)),
            ("l-slides", file(ROOT, "slides.pptx", 3)),
        ],
        &[],
    );
    let trashed = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "Slides.pptx", 2),
        trashed: true,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("r-slides".into(), trashed);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    let held: Vec<&NodeId> = (commits.questions().iter())
        .flat_map(|question| question.holds())
        .collect();
    assert_eq!(held, ["l-slides"]);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-budget".to_owned())]
    );
}

#[test]
fn a_first_link_to_a_space_listed_empty_holds_nothing_another_device_added_since() {
    let mut trees = trees(
        &[
            ("l-budget", file(ROOT, "Budget.xlsx", 1)),
            ("l-slides", file(ROOT, "Slides.pptx", 2)),
        ],
        &[("r-slides", file(ROOT, "Slides.pptx", 2))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    assert!(commits.questions().is_empty());
    assert!(
        trees.local().get(&"r-slides".into()).is_some(),
        "the same Slides.pptx is adopted as the space's node"
    );
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-budget".to_owned())]
    );
}

#[test]
fn a_fresh_first_link_to_a_live_space_holds_nothing_and_sends_what_the_space_lacks() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
            ("l-b", file("l-docs", "b", 2)),
            ("l-notes", file(ROOT, "notes", 3)),
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 4)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
            ("r-notes", file(ROOT, "notes", 9)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    assert_eq!(commits.questions(), []);
    assert!(
        trees.local().get(&"r-a".into()).is_some(),
        "the same docs/a is adopted"
    );
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-b".to_owned()),
            ("create file", "l-inside".to_owned()),
            ("create file", "l-notes".to_owned()),
        ]
    );
}

#[test]
fn a_fresh_first_links_file_whose_name_the_space_holds_takes_a_conflict_name_without_a_question() {
    let mut trees = trees(
        &[("l-notes", file(ROOT, "notes", 3))],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    assert_eq!(commits.questions(), []);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-notes".to_owned())]
    );
    let mut commits = Commits::from_state(commits.state()).unwrap();
    renamed_create(&mut commits, &mut trees, "l-notes", "notes (conflict)", 1);
    assert_eq!(commits.questions(), []);
    assert_eq!(
        trees.remote().live(&"r-notes".into()),
        Some(&file(ROOT, "notes", 9))
    );
}

#[test]
fn a_fresh_first_link_to_a_live_space_holds_only_what_the_trash_names() {
    let mut trees = trees(
        &[
            ("l-budget", file(ROOT, "Budget.xlsx", 1)),
            ("l-slides", file(ROOT, "slides.pptx", 3)),
        ],
        &[("r-docs", dir(ROOT, "docs"))],
    );
    let trashed = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "Slides.pptx", 2),
        trashed: true,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("r-slides".into(), trashed);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    assert_eq!(question(&commits).holds().collect::<Vec<_>>(), ["l-slides"]);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-budget".to_owned())]
    );
}

#[test]
fn a_fresh_first_link_holds_a_folder_that_holds_an_entry_at_a_path_the_trash_holds() {
    let mine = "r\u{e9}sum\u{e9}s";
    for theirs in [mine, "R\u{e9}sum\u{e9}s", "re\u{301}sume\u{301}s"] {
        let mut trees = trees(
            &[
                ("l-docs", dir(ROOT, mine)),
                ("l-deleted", file("l-docs", "deleted.txt", 1)),
                ("l-budget", file(ROOT, "Budget.xlsx", 2)),
            ],
            &[
                ("r-docs", dir(ROOT, theirs)),
                ("r-keep", file("r-docs", "keep.txt", 3)),
            ],
        );
        let trashed = RemoteNode {
            made_over: Vec::new(),
            shape: file("r-docs", "Deleted.txt", 1),
            trashed: true,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
            chunks: Box::default(),
        };
        trees.set_remote("r-deleted".into(), trashed);
        trees.settle(&Default::default());
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let mut held: Vec<&NodeId> = asked.holds().collect();
        held.sort();
        if theirs.starts_with('R') {
            assert_eq!(held, ["l-deleted", "l-docs"], "{theirs}");
        } else {
            assert_eq!(held, ["l-deleted"], "{theirs}");
        }
        assert_eq!(
            ops(&mut commits, &mut trees, &mut ids),
            [("create file", "l-budget".to_owned())],
            "{theirs}"
        );
    }
}

#[test]
fn a_first_link_adopts_an_entry_whose_name_the_space_holds_in_another_unicode_form() {
    let mut trees = trees(
        &[
            ("l-dir", dir(ROOT, "\u{1112}\u{1161}\u{11ab}")),
            ("l-keep", file("l-dir", "re\u{301}sume\u{301}.docx", 1)),
        ],
        &[
            ("r-dir", dir(ROOT, "\u{d55c}")),
            ("r-keep", file("r-dir", "r\u{e9}sum\u{e9}.docx", 1)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert_eq!(commits.questions(), []);
    let mut adopted: Vec<&NodeId> = trees.local().iter().map(|(id, _)| id).collect();
    adopted.sort();
    assert_eq!(adopted, ["r-dir", "r-keep"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_held_entry_that_is_edited_or_moved_stays_held_under_a_new_question() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);

    let mut edited = trees.local().get(&"l-b".into()).unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_edit = question(&commits);
    assert_ne!(after_edit.id, asked.id);
    assert_eq!(
        after_edit.held.keys().collect::<Vec<_>>(),
        asked.held.keys().collect::<Vec<_>>()
    );

    let mut moved = trees.local().get(&"l-notes".into()).unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("l-notes".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_move = question(&commits);
    assert_ne!(after_move.id, after_edit.id);
    let tops: Vec<&NodeId> = after_move.held.keys().collect();
    assert_eq!(tops, ["l-b", "l-new"], "notes is held inside new now");
    assert!(trees.held(&"l-notes".into()));
}

#[test]
fn an_entry_moved_out_of_a_held_folder_stays_held_as_an_item_of_its_own() {
    let (mut commits, mut trees, mut ids) = linked();
    let mut moved = trees.local().get(&"l-inside".into()).unwrap().clone();
    moved.shape.parent = "r-docs".into();
    trees.set_local("l-inside".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let tops: Vec<NodeId> = question(&commits).held.into_keys().collect();
    assert_eq!(tops, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert!(trees.held(&"l-inside".into()));
}

#[test]
fn a_held_entry_the_user_deletes_leaves_the_question_and_the_last_one_ends_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local(&"l-inside".into());
    trees.remove_local(&"l-new".into());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after = question(&commits);
    assert_ne!(after.id, asked.id);
    let tops: Vec<&NodeId> = after.held.keys().collect();
    assert_eq!(tops, ["l-b", "l-notes"]);

    trees.remove_local(&"l-b".into());
    trees.remove_local(&"l-notes".into());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.questions().is_empty());
}

#[test]
fn a_held_entry_the_rules_leave_out_for_a_while_stays_held_until_the_person_answers() {
    let (mut commits, mut trees, mut ids) = linked();
    let root: [&str; 0] = [];
    let held = ["l-b", "l-inside", "l-new", "l-notes"];
    trees.set_rules(
        &root,
        Source::Koiignore,
        Some(RuleFile::parse(b"notes\nnew/\n")),
    );
    assert!(trees.excluded(&"l-notes".into()) && trees.excluded(&"l-new".into()));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, Source::Koiignore, None);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "nothing held is sent once the rules let it in again"
    );
    assert!(held.iter().all(|id| trees.held(&NodeId::from(*id))));

    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Publish));
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-b".to_owned()),
            ("create file", "l-inside".to_owned()),
            ("create file", "l-notes".to_owned()),
        ]
    );
}

const WRITTEN_NS: i64 = 1_794_268_740_123_456_789;

fn unlinked(local: &str, remote: &[(&str, Shape)]) -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[("l-file", written(local, WRITTEN_NS))], remote);
    trees.rules_scanned();
    trees.settle(&Default::default());
    (Commits::default(), trees, IdGen::new(7))
}

type Run = (
    Vec<(&'static str, String)>,
    Vec<(NodeId, NodeId, String)>,
    Vec<Effect>,
);

fn run_beside(local_name: &str, remote: &[(&str, Shape)], read_only: bool) -> Run {
    let (mut commits, mut trees, mut ids) = unlinked(local_name, remote);
    let mut effects = Effects::default();
    if read_only {
        commits.connected(&Welcome {
            history_id: HISTORY.into(),
            read_only: true,
            ..Default::default()
        });
    }
    let (mut sent, mut done) = (Vec::new(), Vec::new());
    for ino in 100..200u128 {
        trees.settle(commits.aliases());
        if sent.is_empty() {
            sent = ops(&mut commits, &mut trees, &mut ids);
        }
        let Some(record) =
            (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
                .first()
                .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        (effects.done(
            &mut trees,
            commits.in_flight(),
            &record.effect_id,
            Some(identity),
        ))
        .unwrap();
        done.push(record.effect);
    }
    (sent, local(&trees), done)
}

#[test]
fn an_equal_server_file_at_a_new_local_files_name_is_linked_before_any_effect_is_planned() {
    let (_, mut trees, mut ids) = unlinked("a.docx", &[("r-file", written("a.docx", WRITTEN_NS))]);
    trees.settle(&Default::default());
    let mut effects = Effects::default();
    assert_eq!(
        effects.plan(&trees, None, &mut ids, NOW, BatchLimit::ONE),
        []
    );
    assert_eq!(
        local(&trees),
        [(
            NodeId::from("r-file"),
            NodeId::from(ROOT),
            "a.docx".to_owned()
        )]
    );
}

#[test]
fn a_local_file_and_an_equal_server_file_that_arrive_in_separate_settles_are_linked() {
    let shape = written("a.docx", WRITTEN_NS);
    let mut local_first = trees(&[("l-file", shape.clone())], &[]);
    let remote = trees(&[], &[("r-file", shape.clone())])
        .remote()
        .get(&"r-file".into())
        .cloned();
    local_first.set_remote("r-file".into(), remote.unwrap());
    let mut remote_first = trees(&[], &[("r-file", shape.clone())]);
    remote_first.set_local("l-file".into(), local_node(shape, 1));
    for mut trees in [local_first, remote_first] {
        trees.settle(&Default::default());
        assert_eq!(
            local(&trees),
            [(
                NodeId::from("r-file"),
                NodeId::from(ROOT),
                "a.docx".to_owned()
            )]
        );
    }
}

fn settled_below(folders: &[(&str, Shape)]) -> (Trees, IdGen) {
    let mut child = written("notes", WRITTEN_NS);
    child.parent = folders.last().expect("a folder").0.into();
    let mut remote = folders.to_vec();
    remote.push(("r-file", child.clone()));
    let remote = trees(&[], &remote);
    let mut settled = Trees::new(ROOT);
    settled.rules_scanned();
    let mut local = folders.to_vec();
    local.push(("l-file", child));
    for (ino, (id, shape)) in local.into_iter().enumerate() {
        settled.set_local(id.into(), local_node(shape, ino as u128 + 1));
    }
    for (id, node) in remote.remote().iter() {
        settled.set_remote(id.clone(), node.clone());
    }
    settled.settle(&Default::default());
    (settled, IdGen::new(7))
}

fn linked_before_planning(folders: &[(&str, Shape)]) {
    let (mut trees, mut ids) = settled_below(folders);
    let limit = BatchLimit {
        effects: 100,
        bytes: u64::MAX,
    };
    let planned = Effects::default()
        .plan(&trees, None, &mut ids, NOW, limit)
        .to_vec();
    assert!(
        !(planned.iter()).any(|record| record.node == "l-file" || record.node == "r-file"),
        "{planned:?}"
    );
    assert!(trees.local().get(&"l-file".into()).is_none());
    assert!(trees.local().get(&"r-file".into()).is_some());
    assert_eq!(ops(&mut Commits::default(), &mut trees, &mut ids), []);
}

#[test]
fn a_file_is_linked_before_planning_when_its_folder_enters_s_in_the_same_settle() {
    linked_before_planning(&[("p", dir(ROOT, "folder"))]);
}

#[test]
fn a_file_is_linked_before_planning_when_its_two_folders_enter_s_in_the_same_settle() {
    linked_before_planning(&[("g", dir(ROOT, "outer")), ("p", dir("g", "inner"))]);
}

fn restarted(trees: &Trees) -> Trees {
    restarted_without(trees, &[])
}

fn restarted_without(trees: &Trees, gone: &[&str]) -> Trees {
    restarted_from(trees, gone, trees.state())
}

fn restarted_from(trees: &Trees, gone: &[&str], state: TreesState) -> Trees {
    let mut fresh = Trees::new(ROOT);
    for (id, node) in
        (trees.local().iter()).filter(|(id, _)| !gone.iter().any(|gone| **id == *gone))
    {
        fresh.set_local(id.clone(), node.clone());
    }
    for (id, node) in trees.remote().iter() {
        fresh.set_remote(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        fresh.set_synced(id.clone(), Some(shape.clone()));
    }
    fresh.restore(state).unwrap();
    fresh.rules_scanned();
    fresh
}

fn linked_once_the_folder_is_placed(place: impl FnOnce(&mut Trees)) {
    let remote_parent = dir(ROOT, "remote-folder");
    let mut child = written("notes", WRITTEN_NS);
    child.parent = "p".into();
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    trees.set_local("p".into(), local_node(dir(ROOT, "local-folder"), 1));
    trees.set_local("l-file".into(), local_node(child.clone(), 2));
    trees.set_remote("p".into(), remote_node(remote_parent));
    trees.set_remote("r-file".into(), remote_node(child));
    trees.settle(&Default::default());
    assert!(trees.synced().get(&"p".into()).is_none());
    assert!(trees.local().get(&"l-file".into()).is_some());
    place(&mut trees);
    trees.settle(&Default::default());
    assert!(trees.local().get(&"l-file".into()).is_none());
    assert!(trees.local().get(&"r-file".into()).is_some());
    let sent = ops(&mut Commits::default(), &mut trees, &mut IdGen::new(7));
    assert!(
        !sent.contains(&("create file", "l-file".into())),
        "{sent:?}"
    );
}

#[test]
fn a_file_is_linked_when_its_folder_enters_s_in_a_later_settle() {
    linked_once_the_folder_is_placed(|trees| {
        trees.set_synced("p".into(), Some(dir(ROOT, "remote-folder")));
    });
}

#[test]
fn a_file_is_linked_when_the_user_moves_its_folder_to_the_servers_place() {
    linked_once_the_folder_is_placed(|trees| {
        trees.set_local_changed("p".into(), local_node(dir(ROOT, "remote-folder"), 1));
    });
}

#[test]
fn a_file_is_linked_when_its_folder_enters_s_after_a_restart() {
    linked_once_the_folder_is_placed(|trees| {
        *trees = restarted(trees);
        trees.settle(&Default::default());
        assert!(trees.local().get(&"l-file".into()).is_some());
        trees.set_synced("p".into(), Some(dir(ROOT, "remote-folder")));
    });
}

#[test]
fn a_file_added_in_a_folder_whose_create_result_lands_later_is_linked_to_the_servers_copy() {
    let mut trees = trees(
        &[("d", dir(ROOT, "docs"))],
        &[("r-docs", dir(ROOT, "docs"))],
    );
    trees.rules_scanned();
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "d".to_owned())]
    );
    let copy = String::from("docs (conflict from me 2026-11-09)");
    let mut file = written("a.docx", WRITTEN_NS);
    file.parent = "d".into();
    trees.set_remote("d".into(), remote_node(dir(ROOT, &copy)));
    trees.set_remote("r-file".into(), remote_node(file.clone()));
    trees.set_local("l-file".into(), local_node(file, 50));
    trees.settle(commits.aliases());
    renamed_create(&mut commits, &mut trees, "d", &copy, 2);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.local().get(&"l-file".into()).is_none());
    assert!(trees.local().get(&"r-file".into()).is_some());
}

#[test]
fn a_file_the_rules_stop_excluding_is_linked_to_an_equal_server_file_on_the_next_settle() {
    let root: [&str; 0] = [];
    let mut trees = trees(&[], &[]);
    trees.rules_scanned();
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"a.docx\n")));
    trees.set_local(
        "l-file".into(),
        local_node(written("a.docx", WRITTEN_NS), 1),
    );
    trees.set_remote("r-file".into(), remote_node(written("a.docx", WRITTEN_NS)));
    trees.settle(&Default::default());
    assert!(trees.excluded(&"l-file".into()));
    assert!(trees.local().get(&"l-file".into()).is_some());

    trees.set_rules(&root, Source::Koiignore, None);
    trees.settle(&Default::default());
    assert!(!trees.excluded(&"r-file".into()));
    assert_eq!(
        local(&trees),
        [(
            NodeId::from("r-file"),
            NodeId::from(ROOT),
            "a.docx".to_owned()
        )]
    );
    assert_eq!(
        ops(&mut Commits::default(), &mut trees, &mut IdGen::new(7)),
        []
    );
}

#[test]
fn a_file_whose_folder_moves_out_of_the_rules_is_linked_to_an_equal_server_file_on_the_next_settle()
{
    let root: [&str; 0] = [];
    let mut trees = trees(&[], &[]);
    trees.rules_scanned();
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"x/\n")));
    trees.set_local("p".into(), local_node(dir(ROOT, "x"), 1));
    trees.set_remote("p".into(), remote_node(dir(ROOT, "x")));
    trees.set_synced("p".into(), Some(dir(ROOT, "x")));
    let mut file = written("a.docx", WRITTEN_NS);
    file.parent = "p".into();
    trees.set_local("l-file".into(), local_node(file.clone(), 2));
    trees.set_remote("r-file".into(), remote_node(file));
    trees.settle(&Default::default());
    assert!(trees.excluded(&"l-file".into()));

    trees.set_local("p".into(), local_node(dir(ROOT, "y"), 1));
    trees.settle(&Default::default());
    assert!(!trees.excluded(&"l-file".into()));
    assert_eq!(
        local(&trees),
        [
            (NodeId::from("p"), NodeId::from(ROOT), "y".to_owned()),
            (
                NodeId::from("r-file"),
                NodeId::from("p"),
                "a.docx".to_owned()
            ),
        ]
    );
}

#[test]
fn a_file_whose_create_ended_unanswered_is_linked_to_an_equal_server_file_instead_of_sent_again() {
    let (mut commits, mut trees, mut ids) = unlinked("a.docx", &[]);
    let created = [("create file", "l-file".to_owned())];
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let head_moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(
        commits.failed(&mut trees, &head_moved),
        Ok(Failure::Abandoned)
    );
    trees.set_remote("r-file".into(), remote_node(written("a.docx", WRITTEN_NS)));
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(
        local(&trees),
        [(
            NodeId::from("r-file"),
            NodeId::from(ROOT),
            "a.docx".to_owned()
        )]
    );
}

#[test]
fn a_read_only_device_downloads_a_conflict_copy_beside_a_local_file_of_the_same_name() {
    let copy = conflict_name("a.docx", "Other Mac", NOW as i64, 1);
    let (_, held, _) = run_beside("a.docx", &[("r-file", written(&copy, WRITTEN_NS))], true);
    assert!(held.iter().any(|(_, _, name)| *name == copy), "{held:?}");
    assert!(held.iter().any(|(_, _, name)| name == "a.docx"), "{held:?}");
}

#[test]
fn a_new_local_file_is_sent_beside_another_devices_equal_conflict_copy() {
    let copy = conflict_name("a.docx", "Other Mac", NOW as i64, 1);
    let (sent, held, _) = run_beside("a.docx", &[("r-file", written(&copy, WRITTEN_NS))], false);
    assert_eq!(sent, [("create file", "l-file".to_owned())], "{held:?}");
}

#[test]
fn a_local_file_beside_an_equal_server_file_and_its_conflict_copy_moves_neither() {
    let copy = conflict_name("a.docx", "Other Mac", NOW as i64, 1);
    let remote = [
        ("r-a", written(&copy, WRITTEN_NS)),
        ("r-b", written("a.docx", WRITTEN_NS)),
    ];
    let (_, _, done) = run_beside("a.docx", &remote, false);
    assert!(
        !done
            .iter()
            .any(|effect| matches!(effect, Effect::Move { .. })),
        "{done:?}"
    );
}

#[test]
fn a_new_local_file_is_not_merged_into_a_server_file_named_as_its_conflict_copy() {
    let literal = "notes (conflict from pc 2026-11-09)";
    let (mut commits, mut trees, mut ids) =
        unlinked("notes", &[("r-file", written(literal, WRITTEN_NS))]);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-file".to_owned())]
    );
}

#[test]
fn a_device_that_cannot_build_still_downloads_a_conflict_copy_beside_its_file() {
    let literal = "notes (conflict from pc 2026-11-09)";
    let (commits, mut trees, mut ids) =
        unlinked("notes", &[("r-file", written(literal, WRITTEN_NS))]);
    let mut commits = Commits::from_state(CommitsState {
        halted: Some(Halt::ReadOnly),
        ..commits.state()
    })
    .unwrap();
    for _ in 0..3 {
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        trees.settle(commits.aliases());
    }
    let mut effects = Effects::default();
    let planned = effects.plan(&trees, None, &mut ids, NOW, BatchLimit::ONE);
    assert!(
        (planned.iter())
            .any(|record| record.node == "r-file"
                && matches!(record.effect, Effect::WriteFile { .. })),
        "{planned:?}"
    );
}

/// An entry created after the first link, beside the held ones, is an ordinary create.
#[test]
fn an_entry_created_after_the_first_link_is_sent_at_once() {
    let (mut commits, mut trees, mut ids) = linked();
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("r-docs", "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-later".into(), node);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-later".to_owned())]
    );
}

#[test]
fn publishing_sends_every_held_entry_as_a_create() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(!commits.decide(&mut trees, &asked.id, Answer::Delete));
    assert!(commits.decide(&mut trees, &asked.id, Answer::Publish));
    assert!(
        !commits.decide(&mut trees, &asked.id, Answer::Publish),
        "it took an answer"
    );
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-b".to_owned()),
            ("create file", "l-inside".to_owned()),
            ("create file", "l-notes".to_owned()),
        ]
    );
    assert!(commits.questions().is_empty());
    assert!(!trees.held(&"l-b".into()));
}

/// The server gives this device's create of `id` the conflict name `name`: R holds the file under
/// it at `seq`, and the commit's result names it.
fn renamed_create(commits: &mut Commits, trees: &mut Trees, id: &str, name: &str, seq: u64) {
    let shape = trees.local().get(&NodeId::from(id)).unwrap().shape.clone();
    let node = RemoteNode {
        made_over: Vec::new(),
        shape: Shape {
            name: name.into(),
            ..shape
        },
        trashed: false,
        content_rev: seq,
        meta_rev: seq,
        meta_edit_rev: seq,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: seq,
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote(id.into(), node);
    let renamed = OpResult {
        code: op_result::Code::AppliedRenamed as i32,
        name: name.into(),
        ..Default::default()
    };
    let result = CommitResult {
        seq,
        results: vec![renamed],
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle(commits.aliases());
}

/// Publishing a first link's `notes`, whose name the space holds with other bytes, keeps both
/// files: its create takes a conflict name and asks nothing, also when the first commit that
/// carried it was forgotten and the next one was stored and loaded again before its result. A
/// file created later that takes a conflict name is asked about.
#[test]
fn a_published_create_that_takes_a_conflict_name_asks_nothing_and_a_later_create_asks() {
    let mut trees = trees(
        &[("l-notes", file(ROOT, "notes", 3))],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Publish));
    let created = [("create file", "l-notes".to_owned())];
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let head_moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(
        commits.failed(&mut trees, &head_moved),
        Ok(Failure::Abandoned)
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let mut commits = Commits::from_state(commits.state()).unwrap();
    renamed_create(&mut commits, &mut trees, "l-notes", "notes (conflict)", 2);
    assert_eq!(commits.questions(), []);

    let later = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file(ROOT, "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-later".into(), later);
    trees.settle(commits.aliases());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-later".to_owned())]
    );
    let theirs = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "later", 7),
        trashed: false,
        content_rev: 3,
        meta_rev: 3,
        meta_edit_rev: 3,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 3,
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("r-later".into(), theirs);
    renamed_create(&mut commits, &mut trees, "l-later", "later (conflict)", 3);
    let [asked] = commits.questions() else {
        panic!("one question: {:?}", commits.questions());
    };
    let QuestionKind::Conflict { original, copy, .. } = &asked.kind else {
        panic!("a conflict: {asked:?}");
    };
    assert_eq!(
        (&original.node_id, &copy.node_id),
        (&"r-later".into(), &"l-later".into())
    );
}

/// The effects of discarding `linked`'s entries and of the downloads that follow, applied as if
/// they succeeded, until none is left. Returns them, by node.
fn discarded(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(NodeId, Effect)> {
    let asked = question(commits);
    assert!(commits.decide(trees, &asked.id, Answer::Discard));
    assert_eq!(
        ops(commits, trees, ids),
        [],
        "nothing is sent for a discarded entry"
    );
    assert!(commits.questions().is_empty());
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 100.. {
        trees.settle(commits.aliases());
        let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        done.push((record.node, record.effect));
    }
    trees.settle(commits.aliases());
    done
}

/// Discarding sets S to the held entries, which R does not hold, so the effects move them to the
/// local trash, children before their folder, and then download what R holds at their paths.
#[test]
fn discarding_moves_the_held_entries_to_the_local_trash_and_downloads_the_space() {
    let (mut commits, mut trees, mut ids) = linked();
    let done = discarded(&mut commits, &mut trees, &mut ids);
    let removed: Vec<&NodeId> = (done.iter())
        .filter(|(_, effect)| {
            matches!(effect, Effect::RemoveFile { .. } | Effect::RemoveDir { .. })
        })
        .map(|(node, _)| node)
        .collect();
    assert_eq!(removed.len(), 4, "{done:?}");
    let position = |id: &str| removed.iter().position(|node| **node == id);
    assert!(position("l-inside") < position("l-new"));
    let notes = done.iter().position(|(node, _)| node == "l-notes");
    let download = done.iter().position(|(node, _)| node == "r-notes");
    assert!(notes < download && download.is_some(), "{done:?}");
    assert_eq!(
        local(&trees),
        [
            ("r-a".into(), "r-docs".into(), "a".into()),
            ("r-docs".into(), ROOT.into(), "docs".into()),
            ("r-notes".into(), ROOT.into(), "notes".into()),
        ]
    );
    assert!(trees.work().is_empty(), "{:?}", trees.work());
}

/// The answer acts on what the person saw: an edit made after the answer and before the next
/// commit is planned drops it, and the edited entry stays held under a new question.
#[test]
fn an_answer_to_entries_that_changed_since_is_not_taken() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    let mut edited = trees.local().get(&"l-b".into()).unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert_ne!(again.id, asked.id);
    assert_eq!(again.answer, None);
    assert!(trees.synced().get(&"l-b".into()).is_none() && trees.held(&"l-b".into()));
}

#[test]
fn an_answer_to_a_replaced_question_changes_nothing() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local(&"l-b".into());
    ops(&mut commits, &mut trees, &mut ids);
    assert!(!commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(trees.held(&"l-notes".into()));
}

#[test]
fn a_discard_keeps_its_folder_off_the_server_when_the_user_empties_a_server_folder_in_it_after_the_answer()
 {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
            ("o", dir(ROOT, "other")),
            ("s", file(ROOT, "kept", 3)),
        ],
        &[
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
            ("o", dir(ROOT, "other")),
            ("s", file(ROOT, "kept", 3)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    let mut renamed = trees.local().get(&"s".into()).unwrap().clone();
    renamed.shape.name = "renamed".into();
    trees.set_local_changed("s".into(), renamed);
    trees.settle(commits.aliases());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "s".to_owned())]
    );
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    let mut out = trees.local().get(&"d".into()).unwrap().clone();
    out.shape.parent = "o".into();
    trees.set_local_changed("d".into(), out);
    trees.remove_local(&"d".into());
    trees.remove_local(&"o".into());
    trees.settle(commits.aliases());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    commits
        .received(CommitResult {
            seq: 1,
            results: vec![OpResult {
                code: op_result::Code::Applied as i32,
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap();
    let mut s = trees.remote().get(&"s".into()).unwrap().clone();
    s.shape.name = "renamed".into();
    trees.set_remote("s".into(), s);
    assert!(commits.acknowledge(&mut trees, 1));
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert_eq!(
        sent,
        [("delete", "o".to_owned()), ("delete", "d".to_owned())]
    );
    assert_eq!(
        local(&trees),
        [
            ("s".into(), ROOT.into(), "renamed".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn a_discard_answered_after_another_device_changes_a_server_folder_the_rules_leave_out_keeps_its_folder_off_the_server()
 {
    let changes: [(&str, Shape); 2] = [("c", file("x", "child", 8)), ("n", file("x", "added", 6))];
    for (id, shape) in changes {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("o", dir(ROOT, "other")),
                ("x", dir(ROOT, "server")),
                ("d", dir("x", "sub")),
                ("c", file("x", "child", 3)),
            ],
            &[
                ("o", dir(ROOT, "other")),
                ("x", dir(ROOT, "server")),
                ("d", dir("x", "sub")),
                ("c", file("x", "child", 3)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let ignored = Some(RuleFile::parse(b"ignored\n"));
        trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        (moved.shape.parent, moved.shape.name) = ("f".into(), "ignored".into());
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        let mut remote = trees.remote().get(&"c".into()).unwrap().clone();
        (remote.shape, remote.content_rev, remote.edit_rev) = (shape, 2, 2);
        trees.set_remote(id.into(), remote);
        trees.settle(commits.aliases());
        assert_eq!(drive_until_quiet(&mut commits, &mut trees, &mut ids), []);
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        let mut out = trees.local().get(&"d".into()).unwrap().clone();
        out.shape.parent = "o".into();
        trees.set_local_changed("d".into(), out);
        trees.remove_local(&"d".into());
        trees.remove_local(&"o".into());
        trees.settle(commits.aliases());
        let mut sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        sent.extend(drive_until_quiet(&mut commits, &mut trees, &mut ids));
        assert_eq!(
            sent,
            [("delete", "o".to_owned()), ("delete", "d".to_owned())],
            "{id}"
        );
    }
}

#[test]
fn a_discard_answered_while_a_change_below_a_server_folder_is_in_flight_keeps_its_folder_off_the_server()
 {
    let changes = [
        ("c", Some(file("x", "child", 4)), "update"),
        ("n", Some(file("x", "added", 6)), "create file"),
        ("m", Some(dir("x", "made")), "create dir"),
        ("e", None, "delete"),
    ];
    for (id, shape, op) in changes {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("o", dir(ROOT, "other")),
                ("x", dir(ROOT, "server")),
                ("d", dir("x", "sub")),
                ("c", file("x", "child", 3)),
                ("e", file("x", "gone", 5)),
            ],
            &[
                ("o", dir(ROOT, "other")),
                ("x", dir(ROOT, "server")),
                ("d", dir("x", "sub")),
                ("c", file("x", "child", 3)),
                ("e", file("x", "gone", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let ignored = Some(RuleFile::parse(b"ignored\n"));
        trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
        match &shape {
            Some(shape) => {
                let mut sent = local_node(shape.clone(), 77);
                if let Some(edited) = trees.local().get(&NodeId::from(id)) {
                    sent = LocalNode {
                        shape: shape.clone(),
                        ..edited.clone()
                    };
                }
                trees.set_local_changed(id.into(), sent);
            }
            None => trees.remove_local(&NodeId::from(id)),
        }
        trees.settle(commits.aliases());
        assert_eq!(
            ops(&mut commits, &mut trees, &mut ids),
            [(op, id.to_owned())]
        );
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        (moved.shape.parent, moved.shape.name) = ("f".into(), "ignored".into());
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        commits
            .received(CommitResult {
                seq: 1,
                results: vec![OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        match shape {
            Some(shape) => {
                let mut remote = trees.remote().get(&"c".into()).unwrap().clone();
                (remote.shape, remote.content_rev) = (shape, 2);
                trees.set_remote(id.into(), remote);
            }
            None => trees.remove_remote(&NodeId::from(id)),
        }
        assert!(commits.acknowledge(&mut trees, 1));
        let mut out = trees.local().get(&"d".into()).unwrap().clone();
        out.shape.parent = "o".into();
        trees.set_local_changed("d".into(), out);
        trees.remove_local(&"d".into());
        trees.settle(commits.aliases());
        let mut sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        sent.extend(drive_until_quiet(&mut commits, &mut trees, &mut ids));
        assert_eq!(sent, [("delete", "d".to_owned())], "{id}");
    }
}

#[test]
fn a_discard_answered_while_a_change_to_a_server_folder_it_takes_along_is_in_flight_takes_it_along()
{
    let changes = [
        ("c", file("x", "child", 4), "update"),
        ("n", file("x", "n", 5), "create file"),
        ("x", dir(ROOT, "renamed"), "move"),
    ];
    for (id, shape, op) in changes {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("c", file("x", "child", 3)),
            ],
            &[("x", dir(ROOT, "server")), ("c", file("x", "child", 3))],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let ignored = Some(RuleFile::parse(b"ignored\n"));
        trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
        let node = trees.local().get(&NodeId::from(id)).cloned();
        let node = node.unwrap_or_else(|| local_node(shape.clone(), 90));
        trees.set_local_changed(
            id.into(),
            LocalNode {
                shape: shape.clone(),
                ..node
            },
        );
        trees.settle(commits.aliases());
        assert_eq!(
            ops(&mut commits, &mut trees, &mut ids),
            [(op, id.to_owned())]
        );
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        let result = OpResult {
            code: op_result::Code::Applied as i32,
            node_id: id.into(),
            ..Default::default()
        };
        let results = vec![result];
        (commits.received(CommitResult {
            seq: 1,
            results,
            ..Default::default()
        }))
        .unwrap();
        let mut remote = trees.remote().get(&"c".into()).unwrap().clone();
        (remote.shape, remote.content_rev, remote.meta_rev) = (shape.clone(), 2, 2);
        trees.set_remote(id.into(), remote);
        assert!(commits.acknowledge(&mut trees, 1));
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        assert_eq!(
            drive_until_quiet(&mut commits, &mut trees, &mut ids),
            [],
            "{op}"
        );
        let x = (trees.remote().get(&"x".into()).unwrap().shape.name).clone();
        let mut expected = vec![("c".into(), "x".into(), "child".into())];
        if id == "n" {
            expected.push(("n".into(), "x".into(), "n".into()));
        }
        expected.push(("x".into(), ROOT.into(), x));
        assert_eq!(local(&trees), expected, "{op}");
    }
}

#[test]
fn a_discarded_folder_the_user_puts_an_entry_in_is_asked_about_again() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-later".into(), node);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"l-new".into()));
    let mut effects = Effects::default();
    let mut removed = Vec::new();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        if matches!(
            record.effect,
            Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
        ) {
            removed.push(record.node.clone());
        }
        let identity = FileIdentity {
            ino: 100 + removed.len() as u128,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        trees.settle(commits.aliases());
    }
    removed.sort();
    assert_eq!(removed, ["l-b", "l-inside", "l-notes"], "the folder stays");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert!(again.id != asked.id && again.answer.is_none());
    assert!(commits.decide(&mut trees, &again.id, Answer::Publish));
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.retain(|(_, node)| node == "l-new" || node == "l-later");
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-later".to_owned())
        ]
    );
}

#[test]
fn a_discarded_folder_that_already_holds_a_synced_entry_is_kept_and_sent() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-notes", file(ROOT, "notes", 9)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let mut moved = trees.local().get(&"r-notes".into()).unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "l-new".to_owned()),
            ("move", "r-notes".to_owned())
        ]
    );
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
}

#[test]
fn a_discarded_folder_the_user_moves_a_synced_entry_into_through_a_new_folder_is_kept_and_sent() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-notes", file(ROOT, "notes", 9)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let sub = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: dir("l-new", "sub"),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-sub".into(), sub);
    let mut moved = trees.local().get(&"r-notes".into()).unwrap().clone();
    moved.shape.parent = "l-sub".into();
    trees.set_local("r-notes".into(), moved);
    trees.settle(commits.aliases());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "l-new".to_owned()),
            ("create dir", "l-sub".to_owned()),
            ("move", "r-notes".to_owned())
        ]
    );
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
}

#[test]
fn a_discarded_folder_the_user_moves_a_synced_entry_into_is_kept_after_another_device_deletes_the_entry()
 {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-notes", file(ROOT, "notes", 9)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get(&"r-notes".into()).unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    let mut deleted = trees.remote().get(&"r-notes".into()).unwrap().clone();
    deleted.trashed = true;
    trees.set_remote("r-notes".into(), deleted);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert_eq!(
        sent.first(),
        Some(&("create dir", "l-new".to_owned())),
        "{sent:?}"
    );
    assert!(sent.iter().any(|(_, id)| id == "r-notes"), "{sent:?}");
}

#[test]
fn a_discarded_folder_holding_an_entry_the_server_trashed_and_s_forgot_is_sent_instead_of_asked_again()
 {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-notes", file(ROOT, "notes", 9)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get(&"r-notes".into()).unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    let mut deleted = trees.remote().get(&"r-notes".into()).unwrap().clone();
    deleted.trashed = true;
    trees.set_remote("r-notes".into(), deleted);
    trees.set_synced("r-notes".into(), None);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert_eq!(
        sent.first(),
        Some(&("create dir", "l-new".to_owned())),
        "{sent:?}"
    );
}

#[test]
fn a_discarded_folder_whose_subfolder_the_user_puts_an_entry_in_is_asked_about_again() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-sub", dir("l-new", "sub")),
            ("l-inside", file("l-sub", "inside", 4)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-sub", "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-later".into(), node);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"l-sub".into()) && trees.reclaimed(&"l-new".into()));
    let mut effects = Effects::default();
    let mut touched = Vec::new();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        touched.push(record.node.clone());
        let identity = FileIdentity {
            ino: 100 + touched.len() as u128,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        trees.settle(commits.aliases());
    }
    touched.retain(|node| node != "r-notes");
    assert_eq!(touched, ["l-inside"], "both folders stay where they are");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert!(again.id != asked.id && again.answer.is_none());
    assert!(commits.decide(&mut trees, &again.id, Answer::Publish));
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.retain(|(_, node)| node != "l-inside");
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create dir", "l-sub".to_owned()),
            ("create file", "l-later".to_owned())
        ]
    );
}

#[test]
fn a_discarded_file_the_user_edits_is_asked_about_again_with_its_folder() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 4)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "inside", 5),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-inside".into(), node);
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    let mut touched = Vec::new();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        touched.push(record.node.clone());
        let identity = FileIdentity {
            ino: 100 + touched.len() as u128,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        trees.settle(commits.aliases());
    }
    assert_eq!(touched, ["r-notes"], "the file and its folder stay");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert!(again.id != asked.id && again.answer.is_none());
    assert!(commits.decide(&mut trees, &again.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-inside".to_owned())
        ]
    );
}

/// A matched folder whose rules a scan could not read keeps them unknown under the server's ID, so
/// nothing below it is planned until a scan reads it.
#[test]
fn an_adopted_folder_keeps_its_rules_unknown() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
            ("r-new", file("r-docs", "new", 2)),
        ],
    );
    trees.set_rules_unknown(&"l-docs".into());
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(
        trees.local().get(&"r-docs".into()).is_some(),
        "the folder is adopted"
    );
    assert!(trees.held(&"r-new".into()));
    assert_eq!(
        Effects::default()
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
}

/// On a first link, the person discards A, which takes the name key of the adopted a, so its S
/// update waits. A commit of this device that creates `pad` was applied at seq 2 before the
/// server lost it. The waiting update came from the person's answer, not from a commit, so the
/// rollback keeps it and A is not uploaded.
#[test]
fn a_discard_whose_s_update_waits_stays_after_a_server_loss() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1)), ("l-A", file(ROOT, "A", 2))],
        &[("r-a", file(ROOT, "a", 1))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert_eq!(asked.shown.keys().collect::<Vec<_>>(), ["l-A"]);
    let pad = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file(ROOT, "pad", 3),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("pad".into(), pad);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, [("create file", "pad".to_owned())]);
    let pad = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "pad", 3),
        trashed: false,
        content_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 2,
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("pad".into(), pad);
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.waiting().contains_key(&"l-A".into()));
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote(&"pad".into());
    trees.forget_lost(
        trashed,
        edited,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle(commits.aliases());
    assert!(trees.waiting().contains_key(&"l-A".into()));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.iter().any(|(_, id)| id == "l-A"), "{sent:?}");
}

#[test]
fn a_reclaimed_folder_that_cannot_leave_s_yet_keeps_its_childs_waiting_update() {
    let mut trees = Trees::new(ROOT);
    for (id, shape, ino) in [("f", dir(ROOT, "a"), 1), ("c", dir("f", "child"), 2)] {
        trees.set_synced(id.into(), Some(shape.clone()));
        trees.set_local(
            id.into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape,
                identity: FileIdentity {
                    ino,
                    birth_ns: Some(0),
                    handle: None,
                },
                parked: false,
                chunks: Box::default(),
            },
        );
    }
    let remote = |shape: Shape| RemoteNode {
        made_over: Vec::new(),
        shape,
        trashed: false,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("p".into(), remote(dir(ROOT, "a")));
    trees.set_remote("c".into(), remote(dir("p", "child")));
    trees.settle(&Default::default());
    let mut state = trees.state();
    state.discarded.insert("f".into(), dir(ROOT, "a"));
    let waiting = Waiting {
        shape: Some(dir("p", "child")),
        origin: Origin::Commit(7),
    };
    state.waiting.insert("c".into(), waiting.clone());
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert!(trees.reclaimed(&"f".into()));
    let mut commits = Commits::from_state(CommitsState {
        accepted: 7,
        ..Default::default()
    })
    .unwrap();
    commits.build(
        &mut trees,
        &mut IdGen::new(7),
        1_794_268_740_000,
        "history",
        "me",
        1,
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.waiting().get(&"c".into()), Some(&waiting));
}

fn remote_node(shape: Shape) -> RemoteNode {
    RemoteNode {
        made_over: Vec::new(),
        shape,
        trashed: false,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    }
}

fn a_reclaimed_folder_that_took_along_a_server_file(server_digest: u8) -> Trees {
    let mut trees = Trees::new(ROOT);
    let local = [
        ("f", dir(ROOT, "a"), 1),
        ("c", dir("f", "child"), 2),
        ("k", file("f", "kept", 3), 3),
    ];
    for (id, shape, ino) in local {
        if id != "k" {
            trees.set_synced(id.into(), Some(shape.clone()));
        }
        trees.set_local(id.into(), local_node(shape, ino));
    }
    trees.set_remote("p".into(), remote_node(dir(ROOT, "a")));
    trees.set_remote("c".into(), remote_node(dir("p", "child")));
    trees.set_remote("k".into(), remote_node(file(ROOT, "kept", server_digest)));
    trees.settle(&Default::default());
    let mut state = trees.state();
    state.discarded.insert("f".into(), dir(ROOT, "a"));
    state.discarded.insert("k".into(), file("f", "kept", 3));
    let waiting = Waiting {
        shape: Some(dir("p", "child")),
        origin: Origin::Commit(7),
    };
    state.waiting.insert("c".into(), waiting);
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert!(trees.reclaimed(&"f".into()));
    trees
}

fn effects_on_k(trees: &Trees, commits: &Commits) -> Vec<String> {
    let mut effects = Effects::default();
    let mut ids = IdGen::new(9);
    (effects
        .plan(trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .iter())
    .filter(|record| record.node == "k")
    .map(|record| format!("{:?}", record.effect))
    .collect()
}

#[test]
fn a_reclaimed_folder_that_cannot_leave_s_yet_lets_go_of_the_server_entries_it_took_along() {
    let mut trees = a_reclaimed_folder_that_took_along_a_server_file(3);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 7,
        ..Default::default()
    })
    .unwrap();
    let mut ids = IdGen::new(7);
    commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 1);
    trees.settle(commits.aliases());
    assert_eq!(effects_on_k(&trees, &commits), Vec::<String>::new());
    assert_eq!(
        trees.local().get(&"k".into()).unwrap().shape,
        file("f", "kept", 3)
    );
}

#[test]
fn a_reclaimed_folder_keeps_the_server_entries_it_took_along_in_place_before_the_next_commit() {
    let trees = a_reclaimed_folder_that_took_along_a_server_file(3);
    assert_eq!(
        effects_on_k(&trees, &Commits::default()),
        Vec::<String>::new()
    );
}

fn reclaim_with_waiting(
    synced: &[(&str, Shape)],
    remote: &[(&str, Shape)],
    waiting: &[(&str, Shape)],
) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in synced.iter().enumerate() {
        trees.set_synced((*id).into(), Some(shape.clone()));
        let identity = FileIdentity {
            ino: ino as u128 + 1,
            birth_ns: Some(0),
            handle: None,
        };
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
            chunks: Box::default(),
        };
        trees.set_local((*id).into(), local);
    }
    for (id, shape) in remote {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
            chunks: Box::default(),
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle(&Default::default());
    let mut state = trees.state();
    state.discarded.insert("f".into(), dir(ROOT, "a"));
    for (id, shape) in waiting {
        let waiting = Waiting {
            shape: Some(shape.clone()),
            origin: Origin::Commit(7),
        };
        state.waiting.insert((*id).into(), waiting);
    }
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert!(trees.reclaimed(&"f".into()));
    let mut commits = Commits::from_state(CommitsState {
        accepted: 7,
        ..Default::default()
    })
    .unwrap();
    commits.build(
        &mut trees,
        &mut IdGen::new(7),
        1_794_268_740_000,
        "history",
        "me",
        1,
    );
    assert_eq!(trees.check(), Ok(()));
    trees
}

#[test]
fn a_reclaimed_folder_leaves_s_with_its_childs_waiting_move_while_another_update_waits() {
    let trees = reclaim_with_waiting(
        &[
            ("f", dir(ROOT, "a")),
            ("c", dir("f", "child")),
            ("u", file(ROOT, "u", 1)),
        ],
        &[
            ("c", dir(ROOT, "a")),
            ("q", dir(ROOT, "q")),
            ("u", file("q", "u", 1)),
        ],
        &[("c", dir(ROOT, "a")), ("u", file("q", "u", 1))],
    );
    assert_eq!(trees.synced().live(&"f".into()), None);
    assert_eq!(trees.synced().live(&"c".into()), Some(&dir(ROOT, "a")));
    assert!(trees.waiting().contains_key(&"u".into()));
}

#[test]
fn a_reclaimed_folder_leaves_s_although_its_own_update_waits() {
    let trees = reclaim_with_waiting(
        &[("f", dir(ROOT, "a")), ("c", dir("f", "child"))],
        &[("c", dir(ROOT, "a"))],
        &[("c", dir(ROOT, "a")), ("f", dir("q", "a"))],
    );
    assert_eq!(trees.synced().live(&"f".into()), None);
    assert!(!trees.waiting().contains_key(&"f".into()));
}

#[test]
fn a_discarded_entry_the_rules_leave_out_for_a_while_goes_to_the_local_trash_once_they_let_it_in() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let parked = LocalNode {
        parked: true,
        ..trees.local().get(&"l-b".into()).unwrap().clone()
    };
    trees.set_local("l-b".into(), parked);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"b\n")));
    trees.settle(commits.aliases());
    assert!(trees.synced().get(&"l-b".into()).is_none());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, Source::Koiignore, None);
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 200.. {
        trees.settle(commits.aliases());
        assert_eq!(
            ops(&mut commits, &mut trees, &mut ids),
            [],
            "a discarded entry is not sent: {done:?}"
        );
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        done.push((record.node, record.effect));
    }
    assert!(trees.local().get(&"l-b".into()).is_none(), "{done:?}");
    assert!(
        (done.iter())
            .any(|(node, effect)| node == "l-b" && matches!(effect, Effect::RemoveFile { .. })),
        "{done:?}"
    );
}

fn run_effects(
    effects: &mut Effects,
    commits: &mut Commits,
    trees: &mut Trees,
    ids: &mut IdGen,
    first_ino: u128,
) -> Vec<(NodeId, Effect)> {
    let mut done = Vec::new();
    for ino in first_ino.. {
        trees.settle(commits.aliases());
        assert_eq!(ops(commits, trees, ids), [], "{done:?}");
        let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        done.push((record.node, record.effect));
    }
    done
}

#[test]
fn a_discarded_folder_that_holds_an_entry_the_rules_leave_out_keeps_its_name_and_reports_the_server_folder_it_blocks()
 {
    let mut trees = trees(
        &[("l-ss", dir(ROOT, "ss")), ("l-a", file("l-ss", "a", 1))],
        &[
            ("r-sharp-s", dir(ROOT, "ß")),
            ("r-c", file("r-sharp-s", "c.txt", 2)),
        ],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"a\n")));
    let mut effects = Effects::default();

    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(done.is_empty(), "{done:?}");
    let kept = trees.local().get(&"l-ss".into()).unwrap();
    assert!(!kept.parked && kept.shape.same_position(&dir(ROOT, "ss")));
    assert!(trees.local().get(&"l-a".into()).is_some());
    assert_eq!(
        stuck(&trees, &effects, commits.in_flight(), &Default::default()),
        [
            ("r-c".into(), Stuck::Behind("r-sharp-s".into())),
            ("r-sharp-s".into(), Stuck::HeldBy("l-ss".into())),
        ]
        .into()
    );

    trees.set_rules(&root, Source::Koiignore, None);
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    trees.settle(commits.aliases());
    assert_eq!(
        local(&trees),
        [
            ("r-c".into(), "r-sharp-s".into(), "c.txt".into()),
            ("r-sharp-s".into(), ROOT.into(), "ß".into()),
        ],
        "{done:?}"
    );
    assert!(trees.work().is_empty(), "{:?}", trees.work());
}

fn discarded_around_an_entry_l_does_not_know(
    server: &[(&str, Shape)],
    parked: bool,
    restart: bool,
) -> (Commits, Trees, Effects, IdGen) {
    let mut trees = trees(
        &[
            ("l-dir", dir(ROOT, "C.TXT")),
            ("l-f", file("l-dir", "C.TXT", 1)),
        ],
        server,
    );
    let with_rules = |mut trees: Trees| {
        let root: [&str; 0] = [];
        trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"a\n")));
        trees
    };
    trees = with_rules(trees);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    if parked {
        let parked = LocalNode {
            parked: true,
            ..trees.local().get(&"l-dir".into()).unwrap().clone()
        };
        trees.set_local("l-dir".into(), parked);
    }
    if restart {
        trees = with_rules(restarted(&trees));
        commits = Commits::from_state(commits.state()).unwrap();
    }
    let mut effects = Effects::default();
    let held = [("a", Some(Kind::Dir))];
    let mut done = Vec::new();
    for ino in 100..140 {
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{done:?}");
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            return (commits, trees, effects, ids);
        };
        if record.node == "l-dir" && matches!(record.effect, Effect::RemoveDir { .. }) {
            effects
                .kept(
                    &mut trees,
                    None,
                    commits.aliases(),
                    &record.effect_id,
                    &held,
                )
                .unwrap();
            if restart {
                trees = with_rules(restarted(&trees));
            }
        } else {
            let identity = FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            };
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(identity),
                )
                .unwrap();
        }
        done.push((record.node, record.effect));
    }
    panic!("the effects do not settle: {done:?}");
}

#[test]
fn a_discarded_folder_holding_an_entry_l_does_not_know_stays_in_place_and_holds_the_server_folder_of_its_name()
 {
    for (parked, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut commits, mut trees, mut effects, mut ids) =
            discarded_around_an_entry_l_does_not_know(
                &[
                    ("r-dir", dir(ROOT, "c.txt")),
                    ("r-ss", file("r-dir", "ss", 2)),
                ],
                parked,
                restart,
            );
        assert_eq!(
            local(&trees),
            [("l-dir".into(), ROOT.into(), "C.TXT".into())],
            "{parked} {restart}"
        );
        assert!(!trees.local().get(&"l-dir".into()).unwrap().parked);
        assert_eq!(
            stuck(&trees, &effects, commits.in_flight(), &Default::default()),
            [
                ("r-dir".into(), Stuck::HeldBy("l-dir".into())),
                ("r-ss".into(), Stuck::Behind("r-dir".into())),
            ]
            .into(),
            "{parked} {restart}"
        );
        let root: [&str; 0] = [];
        trees.set_rules(&root, Source::Koiignore, None);
        trees.set_local_changed("l-a".into(), local_node(dir("l-dir", "a"), 50));
        run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 300);
        assert_eq!(
            local(&trees),
            [
                ("l-a".into(), "r-dir".into(), "a".into()),
                ("r-dir".into(), ROOT.into(), "c.txt".into()),
                ("r-ss".into(), "r-dir".into(), "ss".into()),
            ],
            "{parked} {restart}"
        );
        assert!(question(&commits).shown.contains_key(&"l-a".into()));
    }
}

#[test]
fn a_discarded_folder_holding_an_entry_l_does_not_know_stays_beside_a_server_folder_of_another_name()
 {
    for restart in [false, true] {
        let (commits, trees, effects, _) = discarded_around_an_entry_l_does_not_know(
            &[("r-dir", dir(ROOT, "b")), ("r-ss", file("r-dir", "ss", 2))],
            false,
            restart,
        );
        assert_eq!(
            local(&trees),
            [
                ("l-dir".into(), ROOT.into(), "C.TXT".into()),
                ("r-dir".into(), ROOT.into(), "b".into()),
                ("r-ss".into(), "r-dir".into(), "ss".into()),
            ],
            "{restart}"
        );
        assert!(stuck(&trees, &effects, commits.in_flight(), &Default::default()).is_empty());
    }
}

#[test]
fn a_deep_discarded_folder_goes_to_the_local_trash_folder_by_folder() {
    let ids: Vec<String> = (0..24).map(|depth| format!("l-{depth}")).collect();
    let mut entries = Vec::new();
    let mut parent = ROOT;
    for id in &ids {
        entries.push((id.as_str(), dir(parent, "deeper")));
        parent = id;
    }
    entries.push(("l-file", file(parent, "file", 1)));
    let mut trees = trees(&entries, &[("r-notes", file(ROOT, "notes", 9))]);
    let mut commits = Commits::default();
    let mut ids_gen = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids_gen, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids_gen), []);
    let mut effects = Effects::default();
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids_gen, 100);
    assert_eq!(done.len(), ids.len() + 2, "{done:?}");
    assert_eq!(
        local(&trees),
        [("r-notes".into(), ROOT.into(), "notes".into())]
    );
}

#[test]
fn a_discarded_entry_stays_discarded_when_the_server_merges_its_folder_into_another() {
    let (mut commits, mut trees, mut ids) = linked();
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(
        &root,
        Source::Koiignore,
        Some(RuleFile::parse(
            b"inside
",
        )),
    );
    let identity = FileIdentity {
        ino: 99,
        birth_ns: Some(0),
        handle: None,
    };
    let later = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "later", 6),
        identity,
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local("l-later".into(), later);
    let mut effects = Effects::default();
    for ino in 100.. {
        trees.settle(commits.aliases());
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity { ino, ..identity };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert!(again.answer.is_none());
    assert!(commits.decide(&mut trees, &again.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "l-new".into()),
            ("create file", "l-later".into())
        ]
    );

    let remote = |shape| RemoteNode {
        made_over: Vec::new(),
        shape,
        trashed: false,
        content_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 2,
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
    };
    trees.set_remote("r-new".into(), remote(dir(ROOT, "new")));
    trees.set_remote("l-later".into(), remote(file("r-new", "later", 6)));
    let merged = OpResult {
        code: op_result::Code::MergedInto as i32,
        node_id: "r-new".into(),
        ..Default::default()
    };
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![merged, applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    assert_eq!(
        trees.local().get(&"l-inside".into()).unwrap().shape.parent,
        "r-new"
    );

    trees.set_rules(&root, Source::Koiignore, None);
    trees.settle(commits.aliases());
    let sent = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 2);
    assert!(sent.is_none(), "{sent:?}");
}

#[test]
fn a_file_the_user_took_back_from_a_discard_is_sent_again_once_the_server_loses_it() {
    let (mut commits, mut trees, mut ids) = linked();
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let edit = |trees: &mut Trees, commits: &Commits, digest| {
        let mut node = trees.local().get(&"l-b".into()).unwrap().clone();
        node.shape.content = Some(content(digest));
        trees.set_local("l-b".into(), node);
        trees.settle(commits.aliases());
    };
    let applied = |commits: &mut Commits, trees: &mut Trees, seq, digest| {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape: file("r-docs", "b", digest),
            trashed: false,
            content_rev: seq,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: seq,
            pending_for: None,
            pending_by: None,
            chunks: Box::default(),
        };
        trees.set_remote("l-b".into(), node);
        let applied = OpResult {
            code: op_result::Code::Applied as i32,
            ..Default::default()
        };
        let result = CommitResult {
            seq,
            results: vec![applied],
            ..Default::default()
        };
        commits.received(result).unwrap();
        assert!(commits.acknowledge(trees, seq));
    };
    let sent = |commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen, base| {
        let commit = commits.build(trees, ids, NOW, HISTORY, "me", base);
        (commit.into_iter().flat_map(|commit| &commit.ops))
            .map(|op| match &op.op {
                Some(op::Op::CreateFile(create)) => ("create file", create.node_id.clone()),
                Some(op::Op::UpdateContent(update)) => ("update", update.node_id.clone()),
                other => panic!("unexpected op {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    edit(&mut trees, &commits, 7);
    assert_eq!(sent(&mut commits, &mut trees, &mut ids, 1), []);
    let again = question(&commits);
    assert!(again.answer.is_none());
    assert!(commits.decide(&mut trees, &again.id, Answer::Publish));
    assert_eq!(
        sent(&mut commits, &mut trees, &mut ids, 1),
        [("create file", "l-b".into())]
    );
    applied(&mut commits, &mut trees, 2, 7);
    edit(&mut trees, &commits, 2);
    assert_eq!(
        sent(&mut commits, &mut trees, &mut ids, 2),
        [("update", "l-b".into())]
    );
    applied(&mut commits, &mut trees, 3, 2);

    trees.remove_remote(&"l-b".into());
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        0,
        0,
    );
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    if let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
    {
        assert_ne!(record.node, "l-b", "{record:?}");
    }
    assert_eq!(
        sent(&mut commits, &mut trees, &mut ids, 3),
        [("create file", "l-b".into())]
    );
}

fn local_node(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    }
}

struct LetIn {
    trees: Trees,
    sent: Vec<(&'static str, String)>,
    done: Vec<(NodeId, Effect)>,
}

fn discarded_twice_around_a_server_node(
    local: &[(&str, Shape)],
    remote: &[(&str, Shape)],
    moved: &str,
    change: impl FnOnce(&mut Trees),
    after_discard: impl FnOnce(&mut Trees),
) -> LetIn {
    let held = [("l-new", dir(ROOT, "new")), ("l-e", file("l-new", "e", 1))];
    let mut trees = trees(&[&held[..], local].concat(), remote);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(
        &root,
        Source::Koiignore,
        Some(RuleFile::parse(b"e\n*.txt\nss/\nd/\n")),
    );
    let mut remote = trees.remote().get(&NodeId::from(moved)).unwrap().clone();
    remote.shape.parent = "r-ss".into();
    remote.meta_rev = 2;
    trees.set_remote(moved.into(), remote);
    trees.settle(commits.aliases());
    let mut local = trees.local().get(&NodeId::from(moved)).unwrap().clone();
    local.shape.parent = "l-new".into();
    trees.set_local(moved.into(), local);
    trees.settle(commits.aliases());
    change(&mut trees);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_local("l-later".into(), local_node(file("l-new", "later", 6), 99));
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert!(commits.decide(&mut trees, &again.id, Answer::Discard));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(trees.local().get(&"l-later".into()).is_none());
    after_discard(&mut trees);

    trees.set_rules(&root, Source::Koiignore, None);
    let (mut sent, mut done) = (Vec::new(), Vec::new());
    for ino in 200.. {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        done.push((record.node, record.effect));
    }
    LetIn { trees, sent, done }
}

#[test]
fn a_folder_discarded_again_after_a_reclaim_stays_off_the_server_once_the_rules_let_in_a_server_file_moved_into_it()
 {
    let remote = [("r-c", file(ROOT, "c.txt", 9)), ("r-ss", dir(ROOT, "ss"))];
    let LetIn { trees, sent, .. } = discarded_twice_around_a_server_node(
        &[("l-c", file(ROOT, "c.txt", 9))],
        &remote,
        "r-c",
        |_| {},
        |_| {},
    );
    assert_eq!(
        sent,
        [],
        "the folder discarded twice or its server file is sent"
    );
    assert!(trees.local().get(&"l-new".into()).is_none());
    assert_eq!(
        trees.local().get(&"r-c".into()).map(|node| &node.shape),
        Some(&file("r-ss", "c.txt", 9))
    );
}

#[test]
fn a_server_file_that_s_still_places_in_a_discarded_folder_gives_that_place_up() {
    let remote = [("r-c", file(ROOT, "c.txt", 9)), ("r-ss", dir(ROOT, "ss"))];
    let local = [("l-c", file(ROOT, "c.txt", 9))];
    let LetIn { trees, sent, .. } = discarded_twice_around_a_server_node(
        &local,
        &remote,
        "r-c",
        |trees| {
            assert!(trees.set_synced_if_valid("r-c".into(), Some(file("l-new", "c.txt", 9))));
            let state = trees.state();
            trees.restore(state).unwrap();
            trees.rules_scanned();
            trees.settle(&Default::default());
            assert_eq!(trees.synced().live(&"r-c".into()), None);
        },
        |_| {},
    );
    assert_eq!(sent, []);
    assert_eq!(
        trees.local().get(&"r-c".into()).map(|node| &node.shape),
        Some(&file("r-ss", "c.txt", 9))
    );
}

#[test]
fn a_server_file_another_device_edits_after_its_folder_is_discarded_still_goes_with_the_folder() {
    let remote = [("r-c", file(ROOT, "c.txt", 9)), ("r-ss", dir(ROOT, "ss"))];
    let local = [("l-c", file(ROOT, "c.txt", 9))];
    let LetIn { trees, sent, .. } = discarded_twice_around_a_server_node(
        &local,
        &remote,
        "r-c",
        |_| {},
        |trees| {
            let mut edited = trees.remote().get(&"r-c".into()).unwrap().clone();
            edited.shape.content = Some(content(5));
            edited.content_rev = 3;
            edited.edit_rev = 3;
            trees.set_remote("r-c".into(), edited);
        },
    );
    assert_eq!(sent, []);
    assert!(trees.local().get(&"l-new".into()).is_none());
    assert_eq!(
        trees.local().get(&"r-c".into()).map(|node| &node.shape),
        Some(&file("r-ss", "c.txt", 5))
    );
}

#[test]
fn a_server_file_edited_while_excluded_before_its_folder_is_discarded_keeps_the_edit() {
    let remote = [("r-c", file(ROOT, "c.txt", 9)), ("r-ss", dir(ROOT, "ss"))];
    let LetIn { trees, sent, done } = discarded_twice_around_a_server_node(
        &[("l-c", file(ROOT, "c.txt", 9))],
        &remote,
        "r-c",
        |trees| {
            let mut edited = trees.local().get(&"r-c".into()).unwrap().clone();
            edited.shape.content = Some(content(7));
            trees.set_local_changed("r-c".into(), edited);
        },
        |_| {},
    );
    assert!(
        !done
            .iter()
            .any(|(id, effect)| id == "r-c" && matches!(effect, Effect::RemoveFile { .. })),
        "the edit made before the Discard is removed: {done:?}"
    );
    assert!(
        (trees.local().iter()).any(|(_, node)| node.shape.content == Some(content(7))),
        "{sent:?} {done:?}"
    );
}

#[test]
fn a_server_folder_holding_a_new_local_file_stays_when_its_discarded_folder_is_let_in() {
    let remote = [
        ("r-d", dir(ROOT, "d")),
        ("r-f", file("r-d", "f", 3)),
        ("r-ss", dir(ROOT, "ss")),
    ];
    let LetIn { sent, done, .. } = discarded_twice_around_a_server_node(
        &[("l-d", dir(ROOT, "d")), ("l-f", file("l-d", "f", 3))],
        &remote,
        "r-d",
        |trees| {
            trees.set_local("l-x".into(), local_node(file("r-d", "x", 4), 98));
        },
        |_| {},
    );
    assert!(
        !done.iter().any(|(id, _)| id == "l-x" || id == "r-d"),
        "the folder or the file the person never saw is removed: {done:?}"
    );
    assert!(sent.contains(&("create file", "l-x".into())), "{sent:?}");
}

#[test]
fn an_excluded_entry_the_person_never_saw_stays_when_its_discarded_folder_is_let_in() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-in", file("l-new", "inside", 4)),
            ("l-e2", file("l-new", "e2", 5)),
        ],
        &[("r-a", file(ROOT, "a", 9))],
    );
    let root: [&str; 0] = [];
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"e2\n")));
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(!asked.holds().any(|id| id == "l-e2"));
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);

    trees.set_rules(&root, Source::Koiignore, None);
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(
        trees.local().get(&"l-e2".into()).is_some(),
        "new/e2, which no question showed, went to the local trash: {done:?}"
    );
}

#[test]
fn an_edit_to_an_excluded_server_file_after_its_folder_is_discarded_is_not_removed() {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    let mut trashed = trees.remote().get(&"x".into()).unwrap().clone();
    trashed.trashed = true;
    trees.set_remote("x".into(), trashed);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut edited = trees.local().get(&"x".into()).unwrap().clone();
    edited.shape.content = Some(content(8));
    trees.set_local_changed("x".into(), edited);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert!(trees.edited(&"x".into()));
    let mut planner = Effects::default();
    let effects = planner.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
    assert!(effects.iter().all(|record| record.node != "x" || !matches!(record.effect, Effect::RemoveFile { .. })), "a post-Discard edit is removed: {effects:?}");
}

#[test]
fn a_file_another_device_trashed_with_its_folder_while_the_rules_excluded_it_is_not_sent_again() {
    let entries = [
        ("f", dir(ROOT, "docs")),
        ("x", file("f", "x.txt", 1)),
        ("y", file("f", "y", 2)),
    ];
    let mut trees = trees(&entries, &entries);
    for (id, shape) in &entries {
        trees.set_synced((*id).into(), Some(shape.clone()));
    }
    let root: [&str; 0] = [];
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"*.txt\n")));
    trees.settle(&Default::default());
    for id in ["f", "x", "y"] {
        let mut node = trees.remote().get(&NodeId::from(id)).unwrap().clone();
        node.trashed = true;
        node.meta_rev = 2;
        node.trash_group = "g".into();
        trees.set_remote(id.into(), node);
    }
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    trees.set_rules(&root, Source::Koiignore, None);
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(
        trees.local().get(&"x".into()).is_none(),
        "{:?}",
        local(&trees)
    );
}

#[test]
fn an_unchanged_excluded_subtree_another_device_deleted_is_not_brought_back() {
    let entries = [("f", dir(ROOT, "folder")), ("c", file("f", "child", 9))];
    let mut trees = trees(&entries, &entries);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    assert!(commits.questions().is_empty());
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"folder/\n")),
    );
    for id in ["c", "f"] {
        let mut node = trees.remote().get(&NodeId::from(id)).unwrap().clone();
        node.trashed = true;
        node.meta_rev = 2;
        trees.set_remote(id.into(), node);
    }
    trees.settle(commits.aliases());
    assert!(trees.check().is_ok());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.is_empty(),
        "nothing changed locally, but a remote delete is undone: {sent:?}"
    );
    let mut effects = Effects::default();
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(
        trees.local().get(&"c".into()).is_none() && trees.local().get(&"f".into()).is_none(),
        "remote deletion did not reach disk: {done:?}"
    );
}

#[test]
fn a_server_file_moved_into_a_discarded_folder_and_edited_on_another_device_is_not_sent_with_it() {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut edited = trees.remote().get(&"x".into()).unwrap().clone();
    edited.shape.content = Some(content(7));
    edited.content_rev = 2;
    edited.edit_rev = 2;
    trees.set_remote("x".into(), edited);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(FileIdentity {
                        ino,
                        birth_ns: Some(0),
                        handle: None,
                    }),
                )
                .unwrap();
            continue;
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert!(
            sent.is_empty(),
            "no local change followed Discard, but its folder is sent: {sent:?}"
        );
        break;
    }
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 7)
    );
}

#[test]
fn a_server_file_moved_into_a_folder_before_the_folder_is_discarded_goes_back_where_the_server_holds_it()
 {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(FileIdentity {
                        ino,
                        birth_ns: Some(0),
                        handle: None,
                    }),
                )
                .unwrap();
            continue;
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert!(
            sent.is_empty(),
            "no local change followed Discard, but its folder is sent: {sent:?}"
        );
        break;
    }
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 9)
    );
}

#[test]
fn a_server_folder_taken_along_by_a_discard_goes_home_into_the_folder_the_user_deleted_around_it() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("h", dir(ROOT, "home")),
            ("x", dir("h", "server")),
            ("g", file("h", "gone", 4)),
        ],
        &[
            ("h", dir(ROOT, "home")),
            ("x", dir("h", "server")),
            ("g", file("h", "gone", 4)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.remove_local(&"g".into());
    trees.remove_local(&"h".into());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    let mut sent = Vec::new();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert_eq!(sent, [("delete", "g".to_owned())]);
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"h".into()).unwrap().shape,
        dir(ROOT, "home")
    );
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir("h", "server")
    );
    assert_eq!(trees.synced().get(&"x".into()), Some(&dir("h", "server")));
}

#[test]
fn a_server_folder_moved_into_a_discarded_folder_goes_back_with_what_the_server_added_to_it() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("z", dir(ROOT, "server")),
            ("a", file("z", "A", 3)),
        ],
        &[
            ("z", dir(ROOT, "server")),
            ("a", file("z", "A", 3)),
            ("y", dir("z", "k.txt")),
        ],
    );
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"*.txt\n")),
    );
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"*.txt\nignored\n")),
    );
    let mut moved = trees.local().get(&"z".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("z".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(FileIdentity {
                        ino,
                        birth_ns: Some(0),
                        handle: None,
                    }),
                )
                .unwrap();
            continue;
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert!(
            sent.is_empty(),
            "no local change followed Discard, but its folder is sent: {sent:?}"
        );
        break;
    }
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"z".into()).unwrap().shape,
        dir(ROOT, "server")
    );
    assert_eq!(
        trees.local().get(&"y".into()).unwrap().shape,
        dir("z", "k.txt")
    );
}

#[test]
fn a_server_file_back_where_the_server_holds_it_after_a_discard_goes_when_the_server_drops_it() {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    &mut trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(FileIdentity {
                        ino,
                        birth_ns: Some(0),
                        handle: None,
                    }),
                )
                .unwrap();
            continue;
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert!(
            sent.is_empty(),
            "no local change followed Discard, but its folder is sent: {sent:?}"
        );
        break;
    }
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 9)
    );
    trees.remove_remote(&"x".into());
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.is_empty(),
        "the server dropped the file, but it is sent again: {sent:?}"
    );
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 300);
    assert!(trees.local().get(&"x".into()).is_none());
}

fn a_taken_along_server_folder_after_the_server_changes(
    changed: &[&str],
    change: impl Fn(&mut RemoteNode),
) -> Trees {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", dir(ROOT, "server"))],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 5))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_synced("c".into(), Some(file("x", "child", 5)));
    trees.set_local("c".into(), local_node(file("x", "child", 5), 50));
    for id in changed {
        let mut node = trees.remote().get(&NodeId::from(*id)).unwrap().clone();
        change(&mut node);
        node.content_rev += 1;
        node.meta_rev += 1;
        node.edit_rev += 1;
        node.meta_edit_rev += 1;
        trees.set_remote((*id).into(), node);
    }
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    trees
}

#[test]
fn a_server_folder_taken_along_by_a_discard_follows_the_server_when_it_trashes_what_arrived_inside()
{
    let trees = a_taken_along_server_folder_after_the_server_changes(&["x", "c"], |node| {
        node.trashed = true
    });
    assert_eq!(local(&trees), []);
}

#[test]
fn a_server_folder_taken_along_by_a_discard_follows_the_server_when_it_edits_what_arrived_inside() {
    let trees = a_taken_along_server_folder_after_the_server_changes(&["c"], |node| {
        node.shape.content = Some(content(6))
    });
    assert_eq!(
        local(&trees),
        [
            ("c".into(), "x".into(), "child".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file("x", "child", 6)
    );
}

#[test]
fn a_server_folder_taken_along_by_a_discard_follows_the_server_when_it_moves_what_arrived_inside_out()
 {
    let trees = a_taken_along_server_folder_after_the_server_changes(&["c"], |node| {
        node.shape.parent = ROOT.into();
        node.shape.name = "out".into();
    });
    assert_eq!(
        local(&trees),
        [
            ("c".into(), ROOT.into(), "out".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn a_server_folder_holding_only_an_excluded_entry_the_user_deleted_goes_with_its_discarded_folder()
{
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("e", dir("x", "left")),
        ],
        &[
            ("x", dir(ROOT, "server")),
            ("e", dir("x", "left")),
            ("n", file("e", "added", 5)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\nleft\n")),
    );
    trees.settle(commits.aliases());
    trees.remove_local(&"e".into());
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut sent = Vec::new();
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(
        local(&trees),
        [
            ("e".into(), "x".into(), "left".into()),
            ("n".into(), "e".into(), "added".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn a_server_folder_taken_along_by_a_discard_stays_discarded_when_the_user_moves_an_entry_out_and_deletes_it()
 {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("o", dir(ROOT, "other")),
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
        ],
        &[
            ("o", dir(ROOT, "other")),
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let mut out = trees.local().get(&"d".into()).unwrap().clone();
    out.shape.parent = "o".into();
    trees.set_local_changed("d".into(), out);
    trees.remove_local(&"d".into());
    trees.remove_local(&"o".into());
    trees.settle(commits.aliases());
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert_eq!(sent, [("delete", "o".to_owned())]);
    assert_eq!(local(&trees), [("x".into(), ROOT.into(), "server".into())]);
}

fn a_server_entry_deleted_inside_a_taken_along_folder(
    remote: &[(&str, Shape)],
    synced: &[(&str, Shape)],
) -> (Trees, Commits, IdGen) {
    let remote: Vec<(&str, Shape)> = [("x", dir(ROOT, "server"))]
        .into_iter()
        .chain(remote.iter().cloned())
        .collect();
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", dir(ROOT, "server"))],
        &remote,
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    for (ino, (id, shape)) in (60..).zip(synced) {
        trees.set_local((*id).into(), local_node(shape.clone(), ino));
        trees.set_synced((*id).into(), Some(shape.clone()));
    }
    for (id, _) in synced.iter().rev() {
        trees.remove_local(&NodeId::from(*id));
    }
    trees.settle(commits.aliases());
    (trees, commits, ids)
}

fn the_discard_holds_once_the_server_entry_comes_back(
    remote: &[(&str, Shape)],
    synced: &[(&str, Shape)],
) -> Vec<(NodeId, NodeId, String)> {
    let (mut trees, mut commits, mut ids) =
        a_server_entry_deleted_inside_a_taken_along_folder(remote, synced);
    assert!(!trees.reclaimed(&"f".into()));
    run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    local(&trees)
}

#[test]
fn deleting_a_folder_inside_a_taken_along_server_folder_before_its_entries_arrive_keeps_the_discard()
 {
    let local = the_discard_holds_once_the_server_entry_comes_back(
        &[("d", dir("x", "sub")), ("e", file("d", "unseen", 5))],
        &[("d", dir("x", "sub"))],
    );
    assert_eq!(
        local,
        [
            ("d".into(), "x".into(), "sub".into()),
            ("e".into(), "d".into(), "unseen".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn deleting_a_folder_inside_a_taken_along_server_folder_the_server_renamed_keeps_the_discard() {
    let local = the_discard_holds_once_the_server_entry_comes_back(
        &[("d", dir("x", "renamed"))],
        &[("d", dir("x", "sub"))],
    );
    assert_eq!(
        local,
        [
            ("d".into(), "x".into(), "renamed".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn deleting_a_file_inside_a_taken_along_server_folder_the_server_edited_keeps_the_discard() {
    let local = the_discard_holds_once_the_server_entry_comes_back(
        &[("d", file("x", "doc", 5))],
        &[("d", file("x", "doc", 4))],
    );
    assert_eq!(
        local,
        [
            ("d".into(), "x".into(), "doc".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn deleting_a_synced_file_inside_a_folder_the_server_brings_back_reclaims_the_discarded_folder() {
    for remote in [
        vec![("d", dir("x", "sub")), ("g", file("d", "seen", 3))],
        vec![
            ("d", dir("x", "sub")),
            ("g", file("d", "seen", 3)),
            ("e", file("d", "unseen", 5)),
        ],
    ] {
        let (mut trees, mut commits, mut ids) = a_server_entry_deleted_inside_a_taken_along_folder(
            &remote,
            &[("d", dir("x", "sub")), ("g", file("d", "seen", 3))],
        );
        assert!(trees.reclaimed(&"f".into()), "{remote:?}");
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert!(sent.contains(&("create dir", "f".into())), "{sent:?}");
        assert!(sent.contains(&("move", "x".into())), "{sent:?}");
    }
}

#[test]
fn a_server_folder_moved_within_a_taken_along_one_before_the_discard_keeps_the_discard_once_the_rules_let_it_in()
 {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("y", dir("x", "sub")),
            ("z", dir("x", "inner")),
        ],
        &[
            ("x", dir(ROOT, "server")),
            ("y", dir("x", "sub")),
            ("z", dir("x", "inner")),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\nhidden\n")),
    );
    for (id, parent, name) in [("x", "f", "ignored"), ("z", "y", "hidden")] {
        let mut moved = trees.local().get(&NodeId::from(id)).unwrap().clone();
        moved.shape.parent = parent.into();
        moved.shape.name = name.into();
        trees.set_local_changed(id.into(), moved);
    }
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"hidden\n")),
    );
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed(&"f".into()));
    run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir(ROOT, "server")
    );
}

#[test]
fn a_file_added_inside_a_server_folder_moved_into_a_discarded_folder_keeps_its_path_and_the_folder_is_sent()
 {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 9)),
        ],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored/\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_local_changed(
        "later".into(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: file("x", "later", 8),
            identity: FileIdentity {
                ino: 99,
                birth_ns: Some(0),
                handle: None,
            },
            parked: false,
            chunks: Box::default(),
        },
    );
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
    assert!(
        planned.iter().all(|record| record.node != "x"
            || record
                .after
                .as_ref()
                .is_none_or(|shape| shape.parent != ROOT)),
        "a post-Discard file is moved away from the path the user gave it: {planned:?}"
    );
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.contains(&("create dir", "f".into())),
        "the changed subtree's folder was not reclaimed: {sent:?}"
    );
}

fn a_server_folder_in_a_discarded_folder_changed_after_the_answer(
    change: impl FnOnce(&mut Trees),
) -> (Vec<(&'static str, String)>, Trees) {
    let server = [
        ("x", dir(ROOT, "server")),
        ("c", file("x", "child", 9)),
        ("d", file("x", "other", 7)),
    ];
    let mut trees = trees(&[&[("f", dir(ROOT, "new"))], &server[..]].concat(), &server);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(
            b"ignored/
",
        )),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    change(&mut trees);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    let mut sent = Vec::new();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    (sent, trees)
}

fn a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(
    change: impl FnOnce(&mut Trees),
) -> (Vec<(&'static str, String)>, Trees) {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(change);
    assert!(sent.contains(&("create dir", "f".into())), "{sent:?}");
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir("f", "ignored")
    );
    (sent, trees)
}

#[test]
fn a_server_file_deleted_at_an_excluded_path_in_a_taken_along_folder_comes_back_with_the_folder() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
        trees.remove_local(&"c".into())
    });
    assert_eq!(sent, []);
    assert_eq!(
        local(&trees),
        [
            ("c".into(), "x".into(), "child".into()),
            ("d".into(), "x".into(), "other".into()),
            ("x".into(), ROOT.into(), "server".into())
        ]
    );
}

#[test]
fn a_server_folder_the_user_deletes_with_its_file_in_a_discarded_folder_stays_gone_here_in_either_order()
 {
    for order in [["c", "x"], ["x", "c"]] {
        let (sent, trees) =
            a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
                for id in order {
                    trees.remove_local(&NodeId::from(id));
                }
            });
        assert_eq!(sent, [], "{order:?}");
        assert!(trees.local().get(&"x".into()).is_none(), "{order:?}");
        assert!(trees.local().get(&"c".into()).is_none(), "{order:?}");
        assert_eq!(
            trees.remote().live(&"c".into()),
            Some(&file("x", "child", 9))
        );
    }
}

fn a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(
    change: impl FnOnce(&mut Trees),
) -> (Vec<(&'static str, String)>, Trees) {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", dir(ROOT, "server"))],
        &[("x", dir(ROOT, "server")), ("p", dir(ROOT, "away"))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let mut away = trees.remote().get(&"x".into()).unwrap().clone();
    away.shape = dir("p", "server");
    trees.set_remote("x".into(), away.clone());
    away.shape = file("x", "child", 5);
    trees.set_remote("c".into(), away);
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().get(&"x".into()), None);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        if trees.local().get(&"c".into()).is_some() {
            break;
        }
        let record = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
            .unwrap();
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir("f", "ignored")
    );
    change(&mut trees);
    let mut sent = Vec::new();
    for ino in 200..250 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    (sent, trees)
}

#[test]
fn a_server_file_the_user_moves_into_a_taken_along_folder_and_deletes_there_is_deleted_alone() {
    for lift_before_move in [true, false] {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("e", file(ROOT, "elsewhere", 5)),
            ],
            &[
                ("x", dir(ROOT, "server")),
                ("e", file(ROOT, "elsewhere", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored\n")),
        );
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        if lift_before_move {
            trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        }
        let mut into = trees.local().get(&"e".into()).unwrap().clone();
        into.shape.parent = "x".into();
        trees.set_local_changed("e".into(), into);
        trees.settle(commits.aliases());
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.listed(&[ROOT.into(), "f".into(), "x".into()], Pending::default());
        trees.settle(commits.aliases());
        trees.remove_local(&"e".into());
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert_eq!(sent, [("delete", "e".to_owned())], "{lift_before_move}");
        assert_eq!(
            local(&trees),
            [("x".into(), ROOT.into(), "server".into())],
            "{lift_before_move}"
        );
    }
}

#[test]
fn a_server_file_deleted_while_unobserved_in_a_taken_along_folder_leaves_the_folder_taken_along() {
    for moved_in in [true, false] {
        let (parent, path) = match moved_in {
            true => (ROOT, &b"/elsewhere\n"[..]),
            false => ("x", &b"/server/elsewhere\n"[..]),
        };
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("e", file(parent, "elsewhere", 5)),
            ],
            &[
                ("x", dir(ROOT, "server")),
                ("e", file(parent, "elsewhere", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored\n")),
        );
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        let mut into = trees.local().get(&"e".into()).unwrap().clone();
        into.shape.parent = "x".into();
        trees.set_local_changed("e".into(), into);
        trees.settle(commits.aliases());
        assert!(trees.unobserved(&"e".into()), "{moved_in}");
        trees.remove_local(&"e".into());
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(path)),
        );
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert!(sent.is_empty(), "{moved_in}: {sent:?}");
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            dir(ROOT, "server"),
            "{moved_in}"
        );
    }
}

#[test]
fn a_file_the_user_deletes_after_it_arrived_in_a_server_folder_in_a_discarded_folder_is_deleted_alone()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            trees.remove_local(&"c".into());
        });
    assert_eq!(sent, [("delete", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir("p", "server")
    );
    assert!(trees.local().get(&"f".into()).is_none());
    assert!(trees.local().get(&"c".into()).is_none());
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_leaves_the_folder_discarded()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get(&"c".into()).unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir("p", "server")
    );
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file(ROOT, "moved", 5)
    );
}

#[test]
fn a_file_moved_out_of_a_server_folder_in_a_discarded_folder_leaves_the_folder_discarded() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
        let mut moved = trees.local().get(&"c".into()).unwrap().clone();
        moved.shape.parent = ROOT.into();
        trees.set_local_changed("c".into(), moved);
    });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        dir(ROOT, "server")
    );
    assert!(trees.local().get(&"f".into()).is_none());
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file(ROOT, "child", 9)
    );
}

#[test]
fn a_file_edited_inside_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(|trees| {
        let mut edited = trees.local().get(&"c".into()).unwrap().clone();
        edited.shape.content = Some(content(4));
        trees.set_local_changed("c".into(), edited);
    });
    assert!(sent.contains(&("update", "c".into())), "{sent:?}");
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file("x", "child", 4)
    );
}

#[test]
fn a_file_the_server_dropped_and_edited_here_inside_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users()
 {
    let (_, trees) = a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(|trees| {
        trees.remove_remote(&"c".into());
        let mut edited = trees.local().get(&"c".into()).unwrap().clone();
        edited.shape.content = Some(content(4));
        trees.set_local_changed("c".into(), edited);
    });
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file("x", "child", 4)
    );
}

#[test]
fn an_edit_after_a_child_of_a_server_folder_in_a_discarded_folder_is_purged_keeps_its_path() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 9)),
        ],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored/\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut trashed = trees.remote().get(&"c".into()).unwrap().clone();
    trashed.trashed = true;
    trees.set_remote("c".into(), trashed);
    trees.remove_remote(&"c".into());
    let mut edited = trees.local().get(&"c".into()).unwrap().clone();
    edited.shape.content = Some(content(8));
    trees.set_local_changed("c".into(), edited);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert!(trees.check().is_ok());
    let mut effects = Effects::default();
    let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
    assert!(
        planned.iter().all(|record| record.node != "x"
            || record
                .after
                .as_ref()
                .is_none_or(|shape| shape.parent != ROOT)),
        "a post-Discard edit is relocated after its child was purged: {planned:?}"
    );
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.contains(&("create dir", "f".into())),
        "the changed subtree's folder was not reclaimed: {sent:?}"
    );
}

#[test]
fn a_user_folder_parked_below_a_folder_the_server_added_keeps_the_discarded_folder_the_users() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 9)),
            ("u", dir("f", "mine")),
        ],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored/\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(!trees.reclaimed(&"f".into()));

    let server = trees.remote().get(&"x".into()).unwrap().clone();
    let added = RemoteNode {
        shape: dir("x", "added"),
        ..server.clone()
    };
    trees.set_remote("d".into(), added);
    let mut moved = trees.local().get(&"u".into()).unwrap().clone();
    moved.shape.parent = "x".into();
    trees.set_local_changed("u".into(), moved);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"f".into()));
    let mut effects = Effects::default();
    let download = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "d");
    let identity = FileIdentity {
        ino: 901,
        birth_ns: Some(0),
        handle: None,
    };
    (effects.done(
        &mut trees,
        commits.in_flight(),
        &download.effect_id,
        Some(identity),
    ))
    .unwrap();
    let collision = RemoteNode {
        shape: dir("d", "mine"),
        ..server
    };
    trees.set_remote("z".into(), collision);
    let mut moved = trees.local().get(&"u".into()).unwrap().clone();
    moved.shape.parent = "d".into();
    trees.set_local_changed("u".into(), moved);
    trees.settle(commits.aliases());
    let park = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert!(matches!(park.effect, Effect::Move { .. }) && park.node == "u");
    (effects.done(&mut trees, commits.in_flight(), &park.effect_id, None)).unwrap();
    trees.settle(commits.aliases());
    assert!(trees.local().get(&"u".into()).unwrap().parked);

    assert!(trees.reclaimed(&"f".into()));
    let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
    assert!(
        (planned.iter()).all(|record| record.node != "x"),
        "the server folder leaves the folder that is the user's again: {planned:?}"
    );
}

#[test]
fn a_reclaimed_folder_parked_for_a_server_file_is_created_parked_and_takes_rs_name() {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("l-x", dir(ROOT, "server"))],
        &[("x", dir(ROOT, "server"))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"f".into()));
    let server = trees.remote().get(&"x".into()).unwrap().clone();
    let taking = RemoteNode {
        shape: file(ROOT, "new", 5),
        ..server.clone()
    };
    trees.set_remote("n".into(), taking);
    trees.settle(commits.aliases());

    let mut effects = Effects::default();
    let park = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(park.node, "f");
    assert!(matches!(
        park.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    (effects.done(&mut trees, commits.in_flight(), &park.effect_id, None)).unwrap();
    trees.settle(commits.aliases());
    assert!(trees.local().get(&"f".into()).unwrap().parked);
    let download = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "n");
    assert_eq!(download.after, Some(file(ROOT, "new", 5)));
    let identity = FileIdentity {
        ino: 901,
        birth_ns: Some(0),
        handle: None,
    };
    (effects.done(
        &mut trees,
        commits.in_flight(),
        &download.effect_id,
        Some(identity),
    ))
    .unwrap();
    trees.settle(commits.aliases());

    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "f".to_owned()), ("move", "x".to_owned())]
    );
    let created = RemoteNode {
        shape: dir(ROOT, "new (conflict)"),
        ..server.clone()
    };
    trees.set_remote("f".into(), created);
    let moved = RemoteNode {
        shape: dir("f", "server"),
        ..server
    };
    trees.set_remote("x".into(), moved);
    let renamed = OpResult {
        code: op_result::Code::AppliedRenamed as i32,
        name: "new (conflict)".into(),
        ..Default::default()
    };
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![renamed, applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    let out = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(out.node, "f");
    assert!(matches!(
        out.effect,
        Effect::Move {
            from: Location::Parked(_),
            ..
        }
    ));
    assert_eq!(out.after, Some(dir(ROOT, "new (conflict)")));
    (effects.done(&mut trees, commits.in_flight(), &out.effect_id, None)).unwrap();
    trees.settle(commits.aliases());
    let placed = trees.local().get(&"f".into()).unwrap();
    assert!(!placed.parked);
    assert_eq!(placed.shape, dir(ROOT, "new (conflict)"));
    assert_eq!(
        trees.local().get(&"n".into()).unwrap().shape,
        file(ROOT, "new", 5)
    );
}

#[test]
fn a_server_folder_added_after_the_discard_and_moved_out_leaves_the_discarded_folder_held() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 9)),
        ],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored/\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());

    let server = trees.remote().get(&"x".into()).unwrap().clone();
    let added = RemoteNode {
        shape: dir("x", "added"),
        ..server
    };
    trees.set_remote("d".into(), added);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed(&"f".into()));
    let mut effects = Effects::default();
    let download = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "d");
    let identity = FileIdentity {
        ino: 902,
        birth_ns: Some(0),
        handle: None,
    };
    (effects.done(
        &mut trees,
        commits.in_flight(),
        &download.effect_id,
        Some(identity),
    ))
    .unwrap();
    trees.settle(commits.aliases());
    let mut moved = trees.local().get(&"d".into()).unwrap().clone();
    moved.shape.parent = ROOT.into();
    trees.set_local_changed("d".into(), moved);
    trees.settle(commits.aliases());

    assert!(!trees.reclaimed(&"f".into()));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        vec![("move", "d".to_string())]
    );
}

fn written(name: &str, mtime_ns: i64) -> Shape {
    let mut shape = file(ROOT, name, 1);
    shape.content.as_mut().expect("a file").mtime_ns = mtime_ns;
    shape
}

fn sent_beside(local_mtime_ns: i64, remote_mtime_ns: i64) -> (Vec<(&'static str, String)>, Trees) {
    let mut trees = trees(
        &[("l-a", written("a", local_mtime_ns))],
        &[("r-a", written("a", remote_mtime_ns))],
    );
    trees.rules_scanned();
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    (ops(&mut commits, &mut trees, &mut ids), trees)
}

#[test]
fn a_new_local_file_with_the_bytes_and_mtime_of_a_server_file_at_its_place_is_linked_to_it() {
    let (sent, trees) = sent_beside(1_794_268_740_123_456_789, 1_794_268_740_123_456_789);
    assert_eq!(sent, []);
    assert_eq!(local(&trees), [("r-a".into(), ROOT.into(), "a".into())]);
}

#[test]
fn a_new_local_file_with_the_bytes_of_another_write_at_its_place_is_sent() {
    let (sent, _) = sent_beside(1_794_268_740_123_456_789, 1_794_268_740_987_654_321);
    assert_eq!(sent, [("create file", "l-a".to_owned())]);
}

#[test]
fn two_writes_with_the_same_bytes_and_a_coarse_mtime_are_not_linked() {
    for mtime_ns in [1_794_268_740_000_000_000, 1_794_755_239_590_000_000] {
        let (sent, trees) = sent_beside(mtime_ns, mtime_ns);
        assert_eq!(sent, [("create file", "l-a".to_owned())], "{mtime_ns}");
        assert_eq!(trees.synced().get(&"r-a".into()), None, "{mtime_ns}");
    }
}

#[test]
fn a_download_whose_time_a_coarse_disk_cut_is_linked_to_the_server_file_at_its_place() {
    for local_mtime_ns in [
        1_794_268_746_000_000_000,
        1_794_268_747_000_000_000,
        1_794_268_747_230_000_000,
        1_794_268_747_234_567_800,
    ] {
        let (sent, trees) = sent_beside(local_mtime_ns, 1_794_268_747_234_567_891);
        assert_eq!(sent, [], "{local_mtime_ns}");
        assert_eq!(
            trees.synced().live(&"r-a".into()),
            Some(&written("a", 1_794_268_747_234_567_891)),
            "{local_mtime_ns}"
        );
        assert_eq!(local(&trees), [("r-a".into(), ROOT.into(), "a".into())]);
    }
}

#[test]
fn a_coarse_time_that_is_not_the_server_files_time_cut_to_its_precision_is_sent() {
    for local_mtime_ns in [
        1_794_268_748_000_000_000,
        1_794_268_744_000_000_000,
        1_794_268_747_240_000_000,
        1_794_268_747_234_567_900,
    ] {
        let (sent, trees) = sent_beside(local_mtime_ns, 1_794_268_747_234_567_891);
        assert_eq!(
            sent,
            [("create file", "l-a".to_owned())],
            "{local_mtime_ns}"
        );
        assert_eq!(trees.synced().get(&"r-a".into()), None, "{local_mtime_ns}");
    }
}

#[test]
fn two_writes_with_the_same_bytes_and_a_copied_sub_second_mtime_are_linked() {
    let (sent, trees) = sent_beside(1_794_268_740_123_456_789, 1_794_268_740_123_456_789);
    assert_eq!(sent, []);
    assert_eq!(
        trees.synced().live(&"r-a".into()),
        Some(&written("a", 1_794_268_740_123_456_789))
    );
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_trashes_it()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get(&"c".into()).unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
            let mut trashed = trees.remote().get(&"c".into()).unwrap().clone();
            trashed.trashed = true;
            trees.set_remote("c".into(), trashed);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file(ROOT, "moved", 5)
    );
}

#[test]
fn a_file_moved_out_of_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_trashes_it()
 {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
        let mut moved = trees.local().get(&"c".into()).unwrap().clone();
        moved.shape.parent = ROOT.into();
        trees.set_local_changed("c".into(), moved);
        let mut trashed = trees.remote().get(&"c".into()).unwrap().clone();
        trashed.trashed = true;
        trees.set_remote("c".into(), trashed);
    });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file(ROOT, "child", 9)
    );
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_moves_and_trashes_it()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get(&"c".into()).unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
            let mut trashed = trees.remote().get(&"c".into()).unwrap().clone();
            trashed.shape.parent = ROOT.into();
            trashed.shape.name = "remote-moved".into();
            trashed.trashed = true;
            trees.set_remote("c".into(), trashed);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get(&"c".into()).unwrap().shape,
        file(ROOT, "moved", 5)
    );
}

#[test]
fn a_file_taken_along_by_a_reclaimed_folder_that_cannot_leave_s_yet_takes_the_servers_edit_instead_of_being_created_again()
 {
    let mut trees = a_reclaimed_folder_that_took_along_a_server_file(4);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 7,
        ..Default::default()
    })
    .unwrap();
    let mut ids = IdGen::new(7);
    commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 1);
    trees.settle(commits.aliases());

    assert!(trees.synced().get(&"f".into()).is_some());
    trees.set_remote("p".into(), remote_node(dir(ROOT, "b")));
    let mut p = trees.local().get(&"f".into()).unwrap().clone();
    p.shape = dir(ROOT, "b");
    p.identity.ino = 10;
    trees.set_local("p".into(), p);
    trees.set_synced("p".into(), Some(dir(ROOT, "b")));
    let mut c = trees.local().get(&"c".into()).unwrap().clone();
    c.shape = dir("p", "child");
    trees.set_local("c".into(), c);
    trees.set_synced("c".into(), Some(dir("p", "child")));
    trees.settle(commits.aliases());
    let first = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 1)
        .cloned();
    for q in commits.questions().to_vec() {
        commits.decide(&mut trees, &q.id, Answer::Publish);
    }
    let next = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 1)
        .cloned();
    for sent in [first, next].into_iter().flatten() {
        assert!(!sent.ops.iter().any(|op| matches!(&op.op,
            Some(op::Op::CreateFile(f)) if f.content.as_ref().is_some_and(|c| c.manifest_digest == vec![3; 32])
        )), "unchanged server bytes are uploaded as a new local file: {sent:?}");
    }
}

#[test]
fn a_file_taken_along_by_a_reclaimed_folder_that_cannot_leave_s_yet_stays_in_it_while_the_server_edits_it()
 {
    let mut trees = a_reclaimed_folder_that_took_along_a_server_file(4);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 7,
        ..Default::default()
    })
    .unwrap();
    let mut ids = IdGen::new(7);
    commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 1);
    let mut effects = Effects::default();
    let (mut sent, mut done) = (Vec::new(), Vec::new());
    for ino in 100..200 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        (effects.done(
            &mut trees,
            commits.in_flight(),
            &record.effect_id,
            Some(identity),
        ))
        .unwrap();
        done.push((record.node, record.effect));
    }
    assert!(
        !done.iter().any(|(_, effect)| matches!(effect,
            Effect::RemoveFile { displaced } if displaced.identity.ino == 3
        )),
        "{done:?}"
    );
    let k = trees.local().get(&"k".into()).unwrap();
    assert_eq!((k.shape.clone(), k.identity.ino), (file("f", "kept", 3), 3));
    assert!(sent.contains(&("move", "k".to_owned())), "{sent:?}");
    assert!(sent.iter().all(|(op, _)| *op != "create file"), "{sent:?}");
}

fn a_folder_whose_reclaim_is_undone_stays_discarded_when_the_rules_let_it_in(
    lift_after_effects: bool,
) {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("g", dir("f", "sub")),
            ("e", file("g", "e", 4)),
            ("x", file(ROOT, "server", 9)),
        ],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_local("n".into(), local_node(file("f", "n", 6), 99));
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"f".into()));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.remove_local(&"n".into());
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed(&"f".into()));
    let mut effects = Effects::default();
    let mut done = Vec::new();
    if lift_after_effects {
        done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    }
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    done.extend(run_effects(
        &mut effects,
        &mut commits,
        &mut trees,
        &mut ids,
        200,
    ));
    assert!(commits.questions().is_empty());
    assert!(trees.local().get(&"f".into()).is_none(), "{done:?}");
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 9)
    );
}

#[test]
fn a_folder_whose_reclaim_is_undone_stays_discarded_when_the_rules_let_it_in_before_the_effects_run()
 {
    a_folder_whose_reclaim_is_undone_stays_discarded_when_the_rules_let_it_in(false);
}

#[test]
fn a_folder_whose_reclaim_is_undone_stays_discarded_when_the_rules_let_it_in_after_the_effects_run()
{
    a_folder_whose_reclaim_is_undone_stays_discarded_when_the_rules_let_it_in(true);
}

fn a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out()
-> (Commits, Trees, IdGen, Question) {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("g", dir("f", "sub")),
            ("e", file("g", "e", 4)),
            ("x", file(ROOT, "server", 9)),
        ],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    (commits, trees, ids, asked)
}

#[test]
fn a_discard_takes_along_a_server_file_the_rules_let_in_while_the_folder_was_asked_about() {
    let (mut commits, mut trees, mut ids, _) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    let done = run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    assert!(trees.local().get(&"f".into()).is_none(), "{done:?}");
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 9)
    );
}

fn discarded_after_another_device_purges_a_server_file_moved_in(
    edited: bool,
    restart: bool,
) -> Vec<(&'static str, String)> {
    let (mut commits, mut trees, mut ids, _) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    trees.remove_remote(&"x".into());
    if edited {
        let mut edited = trees.local().get(&"x".into()).unwrap().clone();
        edited.shape.content = Some(content(8));
        trees.set_local_changed("x".into(), edited);
    }
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    if restart {
        trees = restarted(&trees);
        commits = Commits::from_state(commits.state()).unwrap();
    }
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "{edited} {restart}"
    );
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    let mut sent = Vec::new();
    let mut effects = Effects::default();
    for ino in 100..160 {
        trees.settle(commits.aliases());
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
        let Some(record) =
            (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
                .first()
                .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        (effects.done(
            &mut trees,
            commits.in_flight(),
            &record.effect_id,
            Some(identity),
        ))
        .unwrap();
    }
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "{edited} {restart}"
    );
    if !edited {
        assert!(trees.local().get(&"f".into()).is_none(), "{restart}");
        assert!(trees.local().get(&"x".into()).is_none(), "{restart}");
    }
    sent
}

#[test]
fn a_server_file_another_device_purges_after_the_user_moved_it_in_goes_with_its_discarded_folder() {
    for restart in [false, true] {
        assert_eq!(
            discarded_after_another_device_purges_a_server_file_moved_in(false, restart),
            [],
            "{restart}"
        );
    }
}

#[test]
fn a_server_file_the_user_edits_after_another_device_purges_it_makes_its_discarded_folder_the_users()
 {
    let sent = discarded_after_another_device_purges_a_server_file_moved_in(true, false);
    assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
    assert!(sent.contains(&("create file", "x".to_owned())), "{sent:?}");
}

#[test]
fn a_second_discard_takes_along_a_server_file_the_rules_let_in_while_the_folder_was_asked_about_again()
 {
    let (mut commits, mut trees, mut ids, asked) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_local("n".into(), local_node(file("f", "n", 6), 99));
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"f".into()));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    let again = question(&commits);
    assert!(again.id != asked.id && again.answer.is_none());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(trees.local().get(&"f".into()).is_none(), "{done:?}");
    assert!(trees.local().get(&"n".into()).is_none(), "{done:?}");
    assert_eq!(
        trees.local().get(&"x".into()).unwrap().shape,
        file(ROOT, "server", 9)
    );
}

#[test]
fn publishing_sends_a_server_file_the_user_moved_in_while_the_rules_left_it_out_only_with_its_folder()
 {
    let (mut commits, mut trees, mut ids, _) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
    assert!(sent.contains(&("move", "x".to_owned())), "{sent:?}");
}

#[derive(Clone, Copy, Debug)]
enum BeforeTheRulesLetItIn {
    AnotherDeviceTrashesIt,
    AnotherDeviceTrashesItBeforeTheAnswer,
    ThePersonDiscardsTheFolderAgain,
}

fn rules_let_in_a_server_folder_a_discard_took_along(
    form: BeforeTheRulesLetItIn,
    restart: bool,
) -> Trees {
    use BeforeTheRulesLetItIn::*;
    let mut trees = trees(
        &[
            ("n", dir(ROOT, "new")),
            ("s", dir(ROOT, "b")),
            ("sx", file("s", "y", 4)),
            ("hc", dir("n", "C.TXT")),
        ],
        &[
            ("s", dir(ROOT, "b")),
            ("sx", file("s", "y", 4)),
            ("sa", dir("s", "ignored")),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let root: [&str; 0] = [];
    let mut held = trees.local().get(&"hc".into()).unwrap().clone();
    held.shape = dir("s", "C.TXT");
    trees.set_local_changed("hc".into(), held);
    trees.set_rules(
        &root,
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored/\n")),
    );
    let mut moved = trees.local().get(&"s".into()).unwrap().clone();
    moved.shape = dir("n", "ignored");
    trees.set_local_changed("s".into(), moved);
    trees.set_local_changed("hx".into(), local_node(file("hc", "x", 3), 98));
    trees.settle(commits.aliases());
    let trash = |trees: &mut Trees, commits: &Commits| {
        for id in ["s", "sx", "sa"] {
            let mut trashed = trees.remote().get(&id.into()).unwrap().clone();
            (trashed.trashed, trashed.meta_rev) = (true, trashed.meta_rev + 1);
            trees.set_remote(id.into(), trashed);
        }
        trees.settle(commits.aliases());
    };
    if let AnotherDeviceTrashesItBeforeTheAnswer = form {
        trash(&mut trees, &commits);
    }
    let discard = |trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen| {
        while let Some(open) = (commits.questions().iter()).find(|asked| asked.answer.is_none()) {
            let id = open.id.clone();
            assert!(commits.decide(trees, &id, Answer::Discard));
            assert_eq!(ops(commits, trees, ids), []);
        }
    };
    discard(&mut trees, &mut commits, &mut ids);
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    let trashed = !matches!(form, ThePersonDiscardsTheFolderAgain);
    match form {
        AnotherDeviceTrashesIt => {
            trash(&mut trees, &commits);
            run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 120);
        }
        ThePersonDiscardsTheFolderAgain => {
            trees.set_local("nn".into(), local_node(dir("n", "C.TXT"), 99));
            trees.settle(commits.aliases());
            assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
            discard(&mut trees, &mut commits, &mut ids);
            run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 150);
        }
        AnotherDeviceTrashesItBeforeTheAnswer => {}
    }
    if trashed {
        let s = &trees.local().get(&"s".into()).unwrap().shape;
        assert_eq!(*s, dir("n", "ignored"), "{form:?} {restart}");
    }
    trees.set_rules(&root, Source::Koiignore, None);
    trees.settle(commits.aliases());
    if restart {
        trees = restarted(&trees);
        commits = Commits::from_state(commits.state()).unwrap();
        effects = Effects::default();
    }
    let mut done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    discard(&mut trees, &mut commits, &mut ids);
    done.extend(run_effects(
        &mut effects,
        &mut commits,
        &mut trees,
        &mut ids,
        250,
    ));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "{form:?} {restart}"
    );
    assert!(
        trees.local().get(&"n".into()).is_none(),
        "{form:?} {restart}: {done:?}"
    );
    trees
}

#[test]
fn a_server_folder_another_device_trashes_goes_to_the_local_trash_with_its_discarded_folder_once_the_rules_let_it_in()
 {
    for form in [
        BeforeTheRulesLetItIn::AnotherDeviceTrashesIt,
        BeforeTheRulesLetItIn::AnotherDeviceTrashesItBeforeTheAnswer,
    ] {
        for restart in [false, true] {
            let trees = rules_let_in_a_server_folder_a_discard_took_along(form, restart);
            assert!(
                trees.local().get(&"s".into()).is_none(),
                "{form:?} {restart}"
            );
        }
    }
}

#[test]
fn a_server_folder_goes_back_from_a_folder_discarded_twice_once_the_rules_let_it_in() {
    for restart in [false, true] {
        let form = BeforeTheRulesLetItIn::ThePersonDiscardsTheFolderAgain;
        let trees = rules_let_in_a_server_folder_a_discard_took_along(form, restart);
        assert_eq!(
            trees.local().get(&"s".into()).unwrap().shape,
            dir(ROOT, "b"),
            "{restart}"
        );
        assert!(trees.local().get(&"nn".into()).is_none(), "{restart}");
        assert!(trees.state().discard_members.is_empty(), "{restart}");
    }
}

#[test]
fn a_discard_sends_a_folder_with_a_server_file_moved_in_while_the_rules_let_it_in_and_left_out_later()
 {
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"server\n")),
    );
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&mut trees, &question(&commits).id, Answer::Discard));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(
        sent,
        [("create dir", "f".to_owned()), ("move", "x".to_owned())]
    );
}

mod a_server_file_moved_into_an_asked_folder {
    use super::*;

    #[derive(Clone, Copy)]
    enum Origin {
        Downloaded,
        AdoptedFromANewEntry,
        AdoptedInPlace,
    }

    const ORIGINS: [Origin; 3] = [
        Origin::Downloaded,
        Origin::AdoptedFromANewEntry,
        Origin::AdoptedInPlace,
    ];

    fn asked(rules: Option<&[u8]>, origin: Origin) -> (Commits, Trees, IdGen) {
        let mut local = vec![
            ("f", dir(ROOT, "new")),
            ("g", dir("f", "sub")),
            ("e", file("g", "e", 4)),
        ];
        match origin {
            Origin::Downloaded => {}
            Origin::AdoptedFromANewEntry => local.push(("mine", file(ROOT, "server", 9))),
            Origin::AdoptedInPlace => local.push(("x", file(ROOT, "server", 9))),
        }
        let mut trees = trees(&local, &[("x", file(ROOT, "server", 9))]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        question(&commits);
        assert_eq!(settle(&mut commits, &mut trees, &mut ids), []);
        assert!(trees.local().get(&"mine".into()).is_none());
        assert_eq!(
            trees.local().get(&"x".into()).map(|node| &node.shape),
            Some(&file(ROOT, "server", 9))
        );
        set_rules(&mut trees, rules);
        (commits, trees, ids)
    }

    fn set_rules(trees: &mut Trees, rules: Option<&[u8]>) {
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            rules.map(RuleFile::parse),
        );
        trees.settle(&Default::default());
    }

    fn scan(trees: &mut Trees, id: &str, shape: Shape) {
        let mut node = trees.local().get(&NodeId::from(id)).unwrap().clone();
        node.shape = shape;
        trees.set_local_changed(id.into(), node);
        trees.settle(&Default::default());
    }

    fn quiet(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) {
        assert_eq!(ops(commits, trees, ids), []);
    }

    fn settle(
        commits: &mut Commits,
        trees: &mut Trees,
        ids: &mut IdGen,
    ) -> Vec<(&'static str, String)> {
        let mut sent = ops(commits, trees, ids);
        let mut effects = Effects::default();
        for ino in 300.. {
            trees.settle(commits.aliases());
            sent.extend(ops(commits, trees, ids));
            let Some(record) =
                (effects.plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE))
                    .first()
                    .cloned()
            else {
                break;
            };
            let identity = FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            };
            (effects.done(
                trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            ))
            .unwrap();
        }
        sent
    }

    fn discard(
        mut commits: Commits,
        mut trees: Trees,
        mut ids: IdGen,
    ) -> (Vec<(&'static str, String)>, Trees) {
        let id = question(&commits).id;
        assert!(commits.decide(&mut trees, &id, Answer::Discard));
        let sent = settle(&mut commits, &mut trees, &mut ids);
        (sent, trees)
    }

    fn kept_and_sent(sent: &[(&'static str, String)], trees: &Trees) {
        assert!(trees.local().get(&"f".into()).is_some(), "{sent:?}");
        assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
        assert!(sent.contains(&("move", "x".to_owned())), "{sent:?}");
    }

    fn taken_along(sent: &[(&'static str, String)], trees: &Trees, digest: u8) {
        assert!(sent.is_empty(), "{sent:?}");
        assert!(trees.local().get(&"f".into()).is_none(), "{sent:?}");
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            file(ROOT, "server", digest)
        );
    }

    #[test]
    fn while_included_is_sent_with_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(None, origin);
            scan(&mut t, "x", file("f", "kept", 9));
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_included_is_sent_with_it_after_the_rules_leave_it_out_and_let_it_in_again() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(None, origin);
            scan(&mut t, "x", file("f", "kept", 9));
            quiet(&mut c, &mut t, &mut i);
            set_rules(&mut t, Some(b"kept\n"));
            quiet(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_left_out_and_renamed_there_to_an_included_name_is_sent_with_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            scan(&mut t, "x", file("f", "visible", 9));
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_left_out_then_moved_out_and_back_in_after_the_rules_let_it_in_is_sent_with_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            scan(&mut t, "x", file(ROOT, "server", 9));
            let _ = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_left_out_again_after_moving_in_and_out_is_taken_along_once_the_rules_let_it_in() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            for _ in 0..3 {
                scan(&mut t, "x", file("f", "ignored", 9));
                quiet(&mut c, &mut t, &mut i);
                scan(&mut t, "x", file(ROOT, "server", 9));
                let _ = ops(&mut c, &mut t, &mut i);
            }
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            taken_along(&sent, &t, 9);
        }
    }

    #[test]
    fn a_nested_folder_while_left_out_is_taken_along_once_the_rules_let_it_in() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("g", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            taken_along(&sent, &t, 9);
        }
    }

    #[test]
    fn while_left_out_is_taken_along_after_the_folder_is_renamed() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            let first = question(&c).id;
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            scan(&mut t, "f", dir(ROOT, "renamed"));
            quiet(&mut c, &mut t, &mut i);
            assert_ne!(question(&c).id, first);
            set_rules(&mut t, None);
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            taken_along(&sent, &t, 9);
        }
    }

    #[test]
    fn while_left_out_goes_with_the_discarded_folder_the_user_adds_to_after_the_rules_let_it_in() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            let first = question(&c);
            assert!(c.decide(&mut t, &first.id, Answer::Discard));
            quiet(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            t.set_local("n".into(), local_node(file("f", "n", 6), 99));
            t.settle(&Default::default());
            let sent = settle(&mut c, &mut t, &mut i);
            assert!(c.questions().is_empty());
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_left_out_is_taken_along_after_the_server_trashes_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            let mut node = t.remote().get(&"x".into()).unwrap().clone();
            node.trashed = true;
            node.meta_rev = 2;
            node.meta_edit_rev = 2;
            t.set_remote("x".into(), node);
            t.settle(&Default::default());
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            assert!(
                !sent.iter().any(|(_, id)| id == "x" || id == "f"),
                "{sent:?}"
            );
            assert!(t.local().get(&"f".into()).is_none(), "{sent:?}");
        }
    }

    #[test]
    fn while_left_out_is_taken_along_after_the_server_moves_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            let mut node = t.remote().get(&"x".into()).unwrap().clone();
            node.shape = file(ROOT, "elsewhere", 9);
            node.meta_rev = 2;
            node.meta_edit_rev = 2;
            t.set_remote("x".into(), node);
            t.settle(&Default::default());
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            assert!(
                !sent.iter().any(|(_, id)| id == "x" || id == "f"),
                "{sent:?}"
            );
            assert_eq!(
                t.local().get(&"x".into()).unwrap().shape,
                file(ROOT, "elsewhere", 9)
            );
        }
    }

    fn edited_on_the_server(t: &mut Trees) {
        let mut node = t.remote().get(&"x".into()).unwrap().clone();
        node.shape = file(ROOT, "server", 10);
        node.content_rev = 2;
        node.edit_rev = 2;
        t.set_remote("x".into(), node);
        t.settle(&Default::default());
    }

    #[test]
    fn while_left_out_is_taken_along_with_the_server_s_edit() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            edited_on_the_server(&mut t);
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            taken_along(&sent, &t, 10);
        }
    }

    #[test]
    fn an_adopted_file_moved_in_while_left_out_is_not_sent_after_the_server_edits_it() {
        for origin in [Origin::AdoptedFromANewEntry, Origin::AdoptedInPlace] {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            edited_on_the_server(&mut t);
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, _) = discard(c, t, i);
            sent.extend(more);
            assert!(
                !sent.iter().any(|(_, id)| id == "x" || id == "f"),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn an_adopted_file_moved_in_while_left_out_comes_back_with_the_server_s_edit() {
        for origin in [Origin::AdoptedFromANewEntry, Origin::AdoptedInPlace] {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            edited_on_the_server(&mut t);
            set_rules(&mut t, None);
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            assert!(sent.is_empty(), "{sent:?}");
            assert!(t.local().get(&"f".into()).is_none());
            assert_eq!(
                t.local().get(&"x".into()).unwrap().shape,
                file(ROOT, "server", 10)
            );
            assert_eq!(
                t.synced().live(&"x".into()),
                Some(&file(ROOT, "server", 10))
            );
        }
    }

    #[test]
    fn while_left_out_is_not_sent_after_the_server_edits_it_while_the_rules_still_leave_it_out() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            edited_on_the_server(&mut t);
            let mut sent = ops(&mut c, &mut t, &mut i);
            let (more, _) = discard(c, t, i);
            sent.extend(more);
            assert!(sent.is_empty(), "{sent:?}");
        }
    }

    #[test]
    fn while_left_out_and_rewritten_with_the_same_bytes_is_not_taken_along_with_the_server_s_edit()
    {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            let mut node = t.local().get(&"x".into()).unwrap().clone();
            node.identity.ino += 1000;
            t.set_local_changed("x".into(), node);
            t.settle(&Default::default());
            quiet(&mut c, &mut t, &mut i);
            edited_on_the_server(&mut t);
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            kept_and_sent(&sent, &t);
        }
    }

    #[test]
    fn while_left_out_keeps_the_user_s_edit_when_the_server_moves_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 11));
            quiet(&mut c, &mut t, &mut i);
            let mut node = t.remote().get(&"x".into()).unwrap().clone();
            node.shape = file(ROOT, "renamed", 9);
            node.meta_rev = 2;
            node.meta_edit_rev = 2;
            t.set_remote("x".into(), node);
            t.settle(&Default::default());
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            assert!(t.local().get(&"f".into()).is_some(), "{sent:?}");
            assert_eq!(
                t.local().get(&"x".into()).map(|node| node.shape.content),
                Some(file(ROOT, "x", 11).content),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn while_left_out_keeps_the_user_s_edit_when_the_server_edits_it_too() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            scan(&mut t, "x", file("f", "ignored", 11));
            edited_on_the_server(&mut t);
            let mut sent = ops(&mut c, &mut t, &mut i);
            set_rules(&mut t, None);
            sent.extend(ops(&mut c, &mut t, &mut i));
            let (more, t) = discard(c, t, i);
            sent.extend(more);
            assert!(
                (t.local().iter()).any(|(_, node)| node.shape.content == Some(content(11))),
                "{sent:?} {:?}",
                t.local().iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_scan_judges_a_new_entry_and_a_move_but_not_a_rule_change_or_an_effects_move() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            let left_out = |t: &Trees, id: &str| {
                t.local()
                    .get(&NodeId::from(id))
                    .unwrap()
                    .left_out_when_placed
            };
            t.set_local_changed("y".into(), local_node(file("f", "ignored", 5), 98));
            t.set_local_changed("z".into(), local_node(file("f", "visible", 5), 97));
            assert!(left_out(&t, "y") && !left_out(&t, "z"));
            scan(&mut t, "x", file("f", "ignored", 9));
            assert!(left_out(&t, "x"));
            set_rules(&mut t, None);
            assert!(left_out(&t, "x") && left_out(&t, "y"));
            quiet(&mut c, &mut t, &mut i);
            let (sent, t) = discard(c, t, i);
            assert_eq!(
                t.local().get(&"x".into()).unwrap().shape,
                file(ROOT, "server", 9),
                "{sent:?}"
            );
            assert!(
                left_out(&t, "x"),
                "the effects move back leaves the flag alone"
            );
        }
    }

    #[test]
    fn a_file_linked_to_the_same_write_on_the_server_moved_in_while_left_out_is_taken_along_with_the_server_s_edit()
     {
        let mut t = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("g", dir("f", "sub")),
                ("e", file("g", "e", 4)),
            ],
            &[],
        );
        let (mut c, mut i) = (Commits::default(), IdGen::new(7));
        c.first_link(&mut t, &mut i, NOW, PriorSync::Shown);
        question(&c);
        let mut copied = file(ROOT, "server", 9);
        copied.content.as_mut().expect("a file").mtime_ns = 1_794_268_740_123_456_789;
        let identity = FileIdentity {
            ino: 50,
            birth_ns: Some(0),
            handle: None,
        };
        t.set_local(
            "copy".into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: copied.clone(),
                identity,
                parked: false,
                chunks: Box::default(),
            },
        );
        t.set_remote(
            "x".into(),
            RemoteNode {
                made_over: Vec::new(),
                shape: copied,
                trashed: false,
                content_rev: 1,
                meta_rev: 1,
                meta_edit_rev: 1,
                trash_group: String::new(),
                version_id: String::new(),
                edit_rev: 1,
                pending_for: None,
                pending_by: None,
                chunks: Box::default(),
            },
        );
        t.settle(&Default::default());
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        assert!(t.local().get(&"copy".into()).is_none());
        set_rules(&mut t, Some(b"ignored\n"));
        let mut moved = t.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        t.set_local_changed("x".into(), moved);
        t.settle(&Default::default());
        quiet(&mut c, &mut t, &mut i);
        edited_on_the_server(&mut t);
        set_rules(&mut t, None);
        quiet(&mut c, &mut t, &mut i);
        let (sent, t) = discard(c, t, i);
        taken_along(&sent, &t, 10);
    }

    fn server_folder_moved_in_while_left_out() -> (Commits, Trees, IdGen) {
        let mut t = trees(
            &[("f", dir(ROOT, "new")), ("e", file("f", "e", 4))],
            &[("sf", dir(ROOT, "server")), ("sc", file("sf", "c", 9))],
        );
        let (mut c, mut i) = (Commits::default(), IdGen::new(7));
        c.first_link(&mut t, &mut i, NOW, PriorSync::Shown);
        question(&c);
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        set_rules(
            &mut t,
            Some(
                b"ignored
",
            ),
        );
        scan(&mut t, "sf", dir("f", "ignored"));
        quiet(&mut c, &mut t, &mut i);
        set_rules(&mut t, None);
        quiet(&mut c, &mut t, &mut i);
        (c, t, i)
    }

    #[test]
    fn a_server_file_deleted_before_a_scan_sees_it_is_not_asked_about_again_and_comes_back_with_its_taken_along_folder()
     {
        for unseen in [
            "",
            "while a scan is pending",
            "renamed by the server out of the rules",
            "excluded by a rule the same scan reports",
        ] {
            let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
            let asked = question(&c).id;
            let same_scan = unseen == "excluded by a rule the same scan reports";
            if same_scan {
                scan(&mut t, "sc", file("sf", "c", 9));
            }
            assert!(c.decide(&mut t, &asked, Answer::Discard));
            if same_scan {
                let rules = RuleFile::parse(b"/new/ignored/c\n");
                t.set_rules(&[] as &[&str], Source::Koiignore, Some(rules));
            }
            t.remove_local(&"sc".into());
            match unseen {
                "while a scan is pending" => {
                    t.listed(&[], Pending::new([["new", "ignored"]]));
                }
                "renamed by the server out of the rules" => {
                    t.set_rules(
                        &[] as &[&str],
                        Source::Koiignore,
                        Some(RuleFile::parse(b"hidden\n")),
                    );
                    on_the_server(&mut t, "sc", |node| node.shape.name = "hidden".into());
                }
                _ => {}
            }
            t.settle(&Default::default());
            assert_eq!(settle(&mut c, &mut t, &mut i), [], "{unseen}");
            assert!(
                c.questions().iter().all(|open| open.answer.is_some()),
                "{unseen}: {:?}",
                c.questions()
            );
            if unseen.is_empty() {
                assert_sc_comes_back(&t);
            }
        }
    }

    #[test]
    fn a_left_out_file_removed_after_s_gives_up_its_place_is_not_asked_about_again() {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        let sc = NodeId::from("sc");
        assert!(t.unobserved(&sc));
        assert!(c.decide(&mut t, &question(&c).id, Answer::Discard));
        assert!(question(&c).frozen.is_some());
        set_rules(&mut t, Some(b"c-leftout\nignored-dest/\n"));
        on_the_server(&mut t, "rd", |node| node.shape = dir(ROOT, "ignored-dest"));
        let mut copy = t.local().get(&sc).unwrap().clone();
        copy.shape.parent = "f".into();
        copy.shape.name = "c-leftout".into();
        t.set_local_changed(sc.clone(), copy);
        on_the_server(&mut t, "sc", |node| node.shape.parent = "rd".into());
        assert!(t.local().get(&sc).is_some());
        assert!(t.synced().get(&sc).is_none(), "{:?}", t.synced().get(&sc));
        assert!(t.unobserved(&sc));
        assert_eq!(t.check(), Ok(()));
        t.set_rules(&[] as &[&str], Source::Koiignore, None);
        t.remove_local(&sc);
        t.settle(&Default::default());
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        assert!(
            c.questions().iter().all(|open| open.answer.is_some()),
            "{:?}",
            c.questions()
        );
        assert_eq!(
            t.local().get(&sc).map(|node| &node.shape),
            Some(&file("rd", "c", 9))
        );
    }

    #[test]
    fn a_left_out_file_removed_as_the_rules_change_after_s_gives_up_its_place_is_not_asked_about_again()
     {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        let sc = NodeId::from("sc");
        assert!(t.unobserved(&sc));
        assert!(c.decide(&mut t, &question(&c).id, Answer::Discard));
        assert!(question(&c).frozen.is_some());
        set_rules(&mut t, Some(b"c-leftout\nignored-dest/\n"));
        on_the_server(&mut t, "rd", |node| node.shape = dir(ROOT, "ignored-dest"));
        let mut copy = t.local().get(&sc).unwrap().clone();
        copy.shape.parent = "f".into();
        copy.shape.name = "c-leftout".into();
        t.set_local_changed(sc.clone(), copy);
        on_the_server(&mut t, "sc", |node| node.shape.parent = "rd".into());
        assert!(t.local().get(&sc).is_some());
        assert!(t.synced().get(&sc).is_none(), "{:?}", t.synced().get(&sc));
        assert!(t.unobserved(&sc));
        assert_eq!(t.check(), Ok(()));
        set_rules(&mut t, None);
        scan(&mut t, "sc", file("f", "c-leftout", 9));
        assert!(!t.unobserved(&sc));
        assert!(t.synced().get(&sc).is_none());
        t.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"c-leftout\n")),
        );
        t.remove_local(&sc);
        t.settle(&Default::default());
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        assert!(
            c.questions().iter().all(|open| open.answer.is_some()),
            "{:?}",
            c.questions()
        );
        assert_eq!(
            t.local().get(&sc).map(|node| &node.shape),
            Some(&file("rd", "c", 9))
        );
    }

    fn assert_sc_comes_back(t: &Trees) {
        assert!(
            t.remote()
                .get(&"sc".into())
                .is_some_and(|node| !node.trashed)
        );
        assert_eq!(
            t.local().get(&"sf".into()).map(|node| &node.shape),
            Some(&dir(ROOT, "server"))
        );
        assert_eq!(
            t.local()
                .get(&"sc".into())
                .map(|node| (&node.shape.parent, &node.shape.name)),
            Some((&"sf".into(), &"c".to_owned()))
        );
    }

    #[test]
    fn a_server_file_deleted_after_a_scan_sees_it_is_sent_and_a_second_discard_takes_its_folder_home()
     {
        for answered_before_the_ack in [false, true] {
            let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
            let asked = question(&c).id;
            assert!(c.decide(&mut t, &asked, Answer::Discard));
            t.listed(&["f".into(), "sf".into()], Pending::default());
            assert!(!t.unobserved(&"sc".into()));
            t.remove_local(&"sc".into());
            t.settle(&Default::default());
            assert_eq!(ops(&mut c, &mut t, &mut i), [("delete", "sc".to_owned())]);
            let again = question(&c);
            assert_ne!(again.id, asked);
            assert_eq!(
                again.holds().cloned().collect::<Vec<_>>(),
                [NodeId::from("e"), NodeId::from("f")]
            );
            let applied = |c: &mut Commits, t: &mut Trees| {
                let seq = c.in_flight().unwrap().commit.base_seq + 1;
                let mut node = t.remote().get(&"sc".into()).unwrap().clone();
                node.trashed = true;
                node.trash_group = format!("{seq}:0");
                (node.meta_rev, node.meta_edit_rev) = (seq, seq);
                t.set_remote("sc".into(), node);
                let results = vec![OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                }];
                (c.received(CommitResult {
                    seq,
                    results,
                    ..Default::default()
                }))
                .unwrap();
                assert!(c.acknowledge(t, seq));
            };
            if !answered_before_the_ack {
                applied(&mut c, &mut t);
            }
            assert!(c.decide(&mut t, &again.id, Answer::Discard));
            if answered_before_the_ack {
                applied(&mut c, &mut t);
            }
            let sent = drive_until_quiet(&mut c, &mut t, &mut i);
            assert!(sent.is_empty(), "{answered_before_the_ack}: {sent:?}");
            assert!(
                c.questions().is_empty(),
                "{answered_before_the_ack}: {:?}",
                c.questions()
            );
            assert_eq!(
                t.local().get(&"sf".into()).map(|node| &node.shape),
                Some(&dir(ROOT, "server")),
                "{answered_before_the_ack}"
            );
            assert!(t.local().get(&"sc".into()).is_none());
            assert!(drive_until_quiet(&mut c, &mut t, &mut i).is_empty());
        }
    }

    fn on_the_server(t: &mut Trees, id: &str, change: impl FnOnce(&mut RemoteNode)) {
        let mut node = (t
            .remote()
            .get(&NodeId::from(id))
            .or(t.remote().get(&"sc".into()))
            .unwrap())
        .clone();
        change(&mut node);
        (node.content_rev, node.meta_rev, node.edit_rev) =
            (node.content_rev + 1, node.meta_rev + 1, node.edit_rev + 1);
        t.set_remote(id.into(), node);
        t.settle(&Default::default());
    }

    fn downloaded(t: &mut Trees, shape: Shape) {
        let server = shape.clone();
        on_the_server(t, "sn", |node| node.shape = server);
        let mut effects = Effects::default();
        let record = (effects.plan(t, None, &mut IdGen::new(9), NOW, BatchLimit::ONE))
            .first()
            .cloned()
            .unwrap();
        let identity = FileIdentity {
            ino: 400,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(t, None, &record.effect_id, Some(identity))
            .unwrap();
        assert_eq!(t.local().get(&"sn".into()).unwrap().shape, shape);
    }

    fn by_the_user(t: &mut Trees, id: &str, change: impl FnOnce(&mut Shape)) {
        let mut node = (t.local().get(&NodeId::from(id)).cloned())
            .unwrap_or_else(|| local_node(file("sf", "n", 7), 90));
        change(&mut node.shape);
        t.set_local_changed(id.into(), node);
        t.settle(&Default::default());
    }

    fn nothing_of_f(sent: &[(&'static str, String)]) {
        let of_f = |id: &String| ["f", "e", "sf"].contains(&id.as_str());
        assert!(!sent.iter().any(|(_, id)| of_f(id)), "{sent:?}");
    }

    fn answered() -> (Commits, Trees, IdGen, String) {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        on_the_server(&mut t, "sm", |node| node.shape = dir(ROOT, "moved"));
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        let asked = question(&c).id;
        assert!(c.decide(&mut t, &asked, Answer::Discard));
        (c, t, i, asked)
    }

    #[test]
    fn a_discard_takes_along_a_server_folder_the_server_changes_after_the_answer() {
        let changes: [fn(&mut Trees); 6] = [
            |t| on_the_server(t, "sf", |node| node.shape = dir(ROOT, "renamed")),
            |t| on_the_server(t, "sf", |node| node.shape = dir("sm", "server")),
            |t| on_the_server(t, "sn", |node| node.shape = file("sf", "new", 5)),
            |t| on_the_server(t, "sm", |node| node.shape.parent = "sf".into()),
            |t| on_the_server(t, "sc", |node| node.shape = file("sf", "c", 10)),
            |t| t.remove_remote(&"sc".into()),
        ];
        for (index, change) in changes.into_iter().enumerate() {
            let (mut c, mut t, mut i, _) = answered();
            change(&mut t);
            let sent = settle(&mut c, &mut t, &mut i);
            nothing_of_f(&sent);
            assert!(t.local().get(&"f".into()).is_none(), "{index}: {sent:?}");
            let remote = t
                .remote()
                .live(&"sf".into())
                .map(|shape| (&shape.parent, &shape.name));
            let local =
                (t.local().get(&"sf".into())).map(|node| (&node.shape.parent, &node.shape.name));
            assert_eq!(local, remote, "{index}: {sent:?}");
        }
    }

    #[test]
    fn a_discard_asks_again_when_the_user_changes_a_server_folder_it_takes_along_after_the_answer()
    {
        type Change = fn(&mut Trees);
        let changes: [(&str, Change); 7] = [
            ("sc", |t| {
                by_the_user(t, "sc", |shape| shape.content = Some(content(5)))
            }),
            ("n", |t| by_the_user(t, "n", |_| {})),
            ("sc", |t| {
                by_the_user(t, "sc", |shape| shape.name = "renamed".into())
            }),
            ("sm", |t| {
                by_the_user(t, "sm", |shape| shape.parent = "f".into())
            }),
            ("sc", |t| {
                downloaded(t, dir("sf", "new"));
                by_the_user(t, "sc", |shape| shape.parent = "sn".into());
            }),
            ("sn", |t| {
                downloaded(t, file("sf", "new", 5));
                by_the_user(t, "sn", |shape| shape.content = Some(content(8)));
            }),
            ("sn", |t| {
                downloaded(t, file("sf", "new", 5));
                by_the_user(t, "sn", |shape| shape.name = "renamed".into());
            }),
        ];
        for (index, (changed, change)) in changes.into_iter().enumerate() {
            let (mut c, mut t, mut i, asked) = answered();
            change(&mut t);
            let shape = t.local().get(&changed.into()).unwrap().shape.clone();
            nothing_of_f(&settle(&mut c, &mut t, &mut i));
            let [again] = c.questions() else {
                panic!("{index}: {:?}", local(&t));
            };
            assert!(again.id != asked && again.answer.is_none(), "{index}");
            assert!(t.local().get(&"f".into()).is_some(), "{index}");
            assert_eq!(
                t.local().get(&changed.into()).unwrap().shape,
                shape,
                "{index}"
            );
        }
    }

    #[test]
    fn a_discard_asked_again_forgets_the_folders_its_answer_takes_along() {
        let (mut c, mut t, mut i, _) = answered();
        assert!(!t.state().discard_answered.is_empty());
        by_the_user(&mut t, "sc", |shape| shape.name = "renamed".into());
        nothing_of_f(&settle(&mut c, &mut t, &mut i));
        assert_eq!(c.questions().len(), 1);
        assert!(t.state().discard_answered.is_empty());
    }

    #[test]
    fn a_discard_asks_again_about_a_file_renamed_before_the_answer_and_edited_after_it() {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        by_the_user(&mut t, "sc", |shape| shape.name = "renamed".into());
        let renamed = [("move", "sc".to_owned())];
        assert_eq!(drive_until_quiet(&mut c, &mut t, &mut i), renamed);
        let asked = question(&c).id;
        assert!(c.decide(&mut t, &asked, Answer::Discard));
        by_the_user(&mut t, "sc", |shape| shape.content = Some(content(5)));
        nothing_of_f(&settle(&mut c, &mut t, &mut i));
        assert!(question(&c).answer.is_none());
        assert_eq!(
            t.local().get(&"sc".into()).unwrap().shape,
            file("sf", "renamed", 5)
        );
    }

    #[test]
    fn a_discard_keeps_a_server_folder_the_server_trashes_after_the_answer_here_and_off_the_server()
    {
        let (mut c, mut t, mut i, _) = answered();
        on_the_server(&mut t, "sf", |node| node.trashed = true);
        nothing_of_f(&settle(&mut c, &mut t, &mut i));
        assert_eq!(
            t.local().get(&"sc".into()).unwrap().shape,
            file("sf", "c", 9)
        );
        assert_eq!(t.local().get(&"sf".into()).unwrap().shape.parent, "f");
    }

    #[test]
    fn a_discard_answered_while_the_rules_leave_out_a_new_entry_in_its_server_folder_is_recorded() {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        set_rules(
            &mut t,
            Some(
                b"skipped
",
            ),
        );
        on_the_server(&mut t, "sn", |node| node.shape = file("sf", "skipped", 5));
        quiet(&mut c, &mut t, &mut i);
        assert!(c.decide(&mut t, &question(&c).id, Answer::Discard));
        assert_eq!(
            c.questions().first().map(|q| q.frozen.is_some()),
            Some(true)
        );
    }

    #[test]
    fn a_discard_answered_while_the_rules_below_its_folders_are_unread_keeps_its_folder_off_the_server()
     {
        for (unknown, change) in [("f", 0), ("sf", 1), ("f", 2), ("f", 3)] {
            let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
            t.set_rules_unknown(&unknown.into());
            assert!(c.decide(&mut t, &question(&c).id, Answer::Discard));
            match change {
                0 => by_the_user(&mut t, "sc", |shape| shape.content = Some(content(5))),
                1 => {
                    by_the_user(&mut t, "sc", |shape| shape.parent = ROOT.into());
                    t.remove_local(&"sc".into());
                }
                rules => {
                    let left_out = if rules == 2 { &b"e\n"[..] } else { b"*\n" };
                    let rules = Some(RuleFile::parse(left_out));
                    t.set_rules(&["new"], Source::Koiignore, rules);
                }
            }
            t.settle(&Default::default());
            let mut sent = settle(&mut c, &mut t, &mut i);
            t.set_rules_known(&unknown.into());
            t.settle(&Default::default());
            sent.extend(settle(&mut c, &mut t, &mut i));
            nothing_of_f(&sent);
            if change > 1 {
                assert!(t.local().get(&"e".into()).is_some());
                assert_eq!(question(&c).answer, None);
            }
        }
    }

    #[test]
    fn a_discard_answered_before_a_download_into_its_server_folder_decides_at_the_build() {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        on_the_server(&mut t, "sn", |node| node.shape = file("sf", "new", 5));
        assert!(c.decide(&mut t, &question(&c).id, Answer::Discard));
        assert_eq!(
            c.questions().first().map(|q| q.frozen.is_none()),
            Some(true)
        );
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        assert!(t.local().get(&"f".into()).is_none());
        assert_eq!(t.local().get(&"sn".into()).unwrap().shape.parent, "sf");
    }
}

fn drive_until_quiet(
    commits: &mut Commits,
    trees: &mut Trees,
    ids: &mut IdGen,
) -> Vec<(&'static str, String)> {
    let mut sent = Vec::new();
    let mut effects = Effects::default();
    for step in 0..200 {
        trees.settle(commits.aliases());
        if let Some(commit) = commits
            .build(trees, ids, NOW, HISTORY, "me", 10 + step)
            .cloned()
        {
            let seq = 10 + step;
            let mut results = Vec::new();
            for op in commit.ops.iter().map(|op| op.op.clone().unwrap()) {
                match op {
                    op::Op::Delete(d) => {
                        sent.push(("delete", d.node_id.clone()));
                        let mut doomed = vec![NodeId::from(&d.node_id)];
                        while let Some(id) = doomed.pop() {
                            doomed.extend(
                                (trees.remote().iter())
                                    .filter(|(_, n)| n.shape.parent == id && !n.trashed)
                                    .map(|(k, _)| k.clone()),
                            );
                            let mut n = trees.remote().get(&id).unwrap().clone();
                            n.trashed = true;
                            n.trash_group = format!("{seq}:0");
                            (n.meta_rev, n.meta_edit_rev) = (seq, seq);
                            trees.set_remote(id, n);
                        }
                    }
                    op::Op::CreateDir(c) => {
                        sent.push(("create dir", c.node_id.clone()));
                        let mut n = trees.remote().get(&"x".into()).unwrap().clone();
                        n.shape = dir(&c.parent_id, &c.name);
                        trees.set_remote(NodeId::from(&c.node_id), n);
                    }
                    op::Op::Move(m) => {
                        sent.push(("move", m.node_id.clone()));
                        let mut n = trees
                            .remote()
                            .get(&NodeId::from(&m.node_id))
                            .unwrap()
                            .clone();
                        n.shape.parent = NodeId::from(&m.new_parent_id);
                        n.shape.name = m.new_name.clone();
                        (n.meta_rev, n.meta_edit_rev) = (seq, seq);
                        trees.set_remote(NodeId::from(&m.node_id), n);
                    }
                    other => panic!("unexpected op {other:?}"),
                }
                results.push(OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                });
            }
            commits
                .received(CommitResult {
                    seq,
                    results,
                    ..Default::default()
                })
                .unwrap();
            assert!(commits.acknowledge(trees, seq));
            continue;
        }
        let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino: 1000 + step as u128,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
    }
    trees.settle(commits.aliases());
    sent
}

#[test]
fn a_discarded_folder_stays_discarded_when_the_excluded_entry_the_user_deleted_is_a_file() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("e", file("x", "left", 5)),
        ],
        &[("x", dir(ROOT, "server")), ("e", file("x", "left", 5))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\nleft\n")),
    );
    trees.settle(commits.aliases());
    trees.remove_local(&"e".into());
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert!(sent.iter().all(|(_, id)| id == "e"), "{sent:?}");
    assert!(local(&trees).contains(&("x".into(), ROOT.into(), "server".into())));
    assert!(trees.local().get(&"f".into()).is_none());
}

#[test]
fn a_server_folder_the_user_removed_an_entry_from_before_the_rules_left_it_out_is_not_taken_along()
{
    for restart in [false, true] {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("e", file("x", "left", 5)),
            ],
            &[("x", dir(ROOT, "server")), ("e", file("x", "left", 5))],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.remove_local(&"e".into());
        if restart {
            trees = restarted(&trees);
        }
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored\nleft\n")),
        );
        trees.settle(commits.aliases());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(
            ops(&mut commits, &mut trees, &mut ids).is_empty(),
            "{restart}"
        );
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "f".to_owned()),
                ("move", "x".to_owned()),
                ("delete", "e".to_owned())
            ],
            "{restart}"
        );
        assert_eq!(
            local(&trees),
            [
                ("f".into(), ROOT.into(), "new".into()),
                ("x".into(), "f".into(), "ignored".into())
            ],
            "{restart}"
        );
    }
}

#[test]
fn a_server_folder_the_user_removed_an_entry_from_is_not_taken_along_after_another_device_renames_it_out_of_the_rules()
 {
    for restart in [false, true] {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("e", file("x", "kept", 5)),
            ],
            &[("x", dir(ROOT, "server")), ("e", file("x", "kept", 5))],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.remove_local(&"e".into());
        if restart {
            trees = restarted(&trees);
        }
        let mut renamed = trees.remote().get(&"e".into()).unwrap().clone();
        renamed.shape.name = "left".into();
        renamed.meta_rev += 1;
        renamed.meta_edit_rev += 1;
        trees.set_remote("e".into(), renamed);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored\nleft\n")),
        );
        trees.settle(commits.aliases());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(
            ops(&mut commits, &mut trees, &mut ids).is_empty(),
            "{restart}"
        );
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            sent,
            [("create dir", "f".to_owned()), ("move", "x".to_owned())],
            "{restart}"
        );
        assert_eq!(
            local(&trees),
            [
                ("e".into(), "x".into(), "left".into()),
                ("f".into(), ROOT.into(), "new".into()),
                ("x".into(), "f".into(), "ignored".into())
            ],
            "{restart}"
        );
    }
}

#[test]
fn an_entry_moved_into_another_taken_along_folder_and_deleted_at_an_excluded_path_comes_back() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
            ("y", dir(ROOT, "yonder")),
        ],
        &[
            ("x", dir(ROOT, "server")),
            ("d", dir("x", "sub")),
            ("y", dir(ROOT, "yonder")),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ia\nib\n")),
    );
    for (id, name) in [("x", "ia"), ("y", "ib")] {
        let mut moved = trees.local().get(&NodeId::from(id)).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = name.into();
        trees.set_local_changed(id.into(), moved);
    }
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let mut out = trees.local().get(&"d".into()).unwrap().clone();
    out.shape.parent = "y".into();
    trees.set_local_changed("d".into(), out);
    trees.remove_local(&"d".into());
    trees.settle(commits.aliases());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, []);
    assert_eq!(
        local(&trees),
        [
            ("d".into(), "x".into(), "sub".into()),
            ("x".into(), ROOT.into(), "server".into()),
            ("y".into(), ROOT.into(), "yonder".into())
        ]
    );
}

#[test]
fn a_server_folder_holding_an_entry_an_earlier_discard_dropped_goes_with_its_folder_when_asked_again()
 {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("n", file("x", "note", 3)),
        ],
        &[("x", dir(ROOT, "server"))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let added = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("f", "added", 4),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    };
    trees.set_local_changed("m".into(), added);
    trees.settle(commits.aliases());
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(local(&trees), [("x".into(), ROOT.into(), "server".into())]);
}

#[test]
fn a_discarded_folder_renamed_and_added_to_then_discarded_again_sends_nothing() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("n", file("x", "note", 3)),
        ],
        &[("x", dir(ROOT, "server"))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_local_changed("m".into(), local_node(file("f", "added", 4), 99));
    let mut renamed = trees.local().get(&"f".into()).unwrap().clone();
    renamed.shape.name = "renamed".into();
    trees.set_local_changed("f".into(), renamed);
    trees.settle(commits.aliases());
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(local(&trees), [("x".into(), ROOT.into(), "server".into())]);
}

#[test]
fn a_renamed_server_entry_keeps_its_name_when_its_taken_along_folder_is_discarded_again() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 3)),
        ],
        &[("x", dir(ROOT, "server")), ("c", file("x", "child", 3))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_local_changed("m".into(), local_node(file("f", "added", 4), 99));
    let mut renamed = trees.local().get(&"c".into()).unwrap().clone();
    renamed.shape.name = "renamed".into();
    trees.set_local_changed("c".into(), renamed);
    trees.settle(commits.aliases());
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let asked = question(&commits);
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert!(drive_until_quiet(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(local(&trees).contains(&("c".into(), "x".into(), "renamed".into())));
    assert!(trees.local().get(&"f".into()).is_some());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, [("move", "c".into())]);
    assert_eq!(
        trees.remote().get(&"c".into()).unwrap().shape.name,
        "renamed"
    );
    assert_eq!(
        local(&trees),
        [
            ("c".into(), "x".into(), "renamed".into()),
            ("x".into(), ROOT.into(), "server".into()),
        ]
    );
}

fn discard_until_quiet(
    commits: &mut Commits,
    trees: &mut Trees,
    ids: &mut IdGen,
) -> Vec<(&'static str, String)> {
    let mut sent = Vec::new();
    loop {
        sent.extend(drive_until_quiet(commits, trees, ids));
        let open: Vec<String> = (commits.questions().iter())
            .filter(|question| question.answer.is_none())
            .map(|question| question.id.clone())
            .collect();
        if open.is_empty() {
            return sent;
        }
        for id in open {
            assert!(commits.decide(trees, &id, Answer::Discard));
        }
    }
}

fn a_server_folder_taken_along_into_a_discarded_folder(
    entry: (&str, Shape),
    on_server: bool,
) -> (Commits, Trees, IdGen) {
    let mut remote = vec![("x", dir(ROOT, "server"))];
    if on_server {
        remote.push(entry.clone());
    }
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", dir(ROOT, "server")), entry],
        &remote,
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(discard_until_quiet(&mut commits, &mut trees, &mut ids).is_empty());
    (commits, trees, ids)
}

fn deleting_a_taken_along_folder_keeps_it_on_the_server(
    entry: (&str, Shape),
    on_server: bool,
    deleted: &[&str],
) {
    let (mut commits, mut trees, mut ids) =
        a_server_folder_taken_along_into_a_discarded_folder(entry.clone(), on_server);
    for id in deleted {
        trees.remove_local(&NodeId::from(*id));
    }
    trees.settle(commits.aliases());
    let mut sent = discard_until_quiet(&mut commits, &mut trees, &mut ids);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    sent.extend(discard_until_quiet(&mut commits, &mut trees, &mut ids));
    assert!(sent.is_empty(), "{sent:?}");
    let kept = |id: &str| {
        trees
            .remote()
            .get(&NodeId::from(id))
            .is_some_and(|node| !node.trashed)
    };
    assert!(kept("x"));
    assert_eq!(kept(entry.0), on_server);
    let local = local(&trees);
    assert!(
        local.contains(&("x".into(), ROOT.into(), "server".into())),
        "{local:?}"
    );
    if on_server {
        assert!(
            local.contains(&(entry.0.into(), "x".into(), entry.1.name.clone())),
            "{local:?}"
        );
    }
}

#[test]
fn deleting_a_discarded_folder_with_the_server_folder_it_took_along_keeps_that_folder_on_the_server()
 {
    deleting_a_taken_along_folder_keeps_it_on_the_server(
        ("c", file("x", "child", 3)),
        true,
        &["c", "x", "f"],
    );
}

#[test]
fn deleting_a_taken_along_server_folder_at_its_excluded_path_keeps_it_on_the_server() {
    deleting_a_taken_along_folder_keeps_it_on_the_server(
        ("c", file("x", "child", 3)),
        true,
        &["c", "x"],
    );
}

#[test]
fn deleting_a_taken_along_server_folder_with_only_a_local_note_keeps_it_on_the_server() {
    deleting_a_taken_along_folder_keeps_it_on_the_server(
        ("n", file("x", "note", 3)),
        false,
        &["n", "x"],
    );
}

#[test]
fn deleting_only_the_discarded_note_in_a_taken_along_server_folder_keeps_it_taken_along() {
    deleting_a_taken_along_folder_keeps_it_on_the_server(
        ("n", file("x", "note", 3)),
        false,
        &["n"],
    );
}

#[test]
fn deleting_only_the_discarded_note_while_stopped_keeps_its_server_folder_taken_along() {
    let (mut commits, before, mut ids) =
        a_server_folder_taken_along_into_a_discarded_folder(("n", file("x", "note", 3)), false);
    let mut trees = restarted_without(&before, &["n"]);
    trees.settle(commits.aliases());
    let mut sent = discard_until_quiet(&mut commits, &mut trees, &mut ids);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    sent.extend(discard_until_quiet(&mut commits, &mut trees, &mut ids));
    assert!(sent.is_empty(), "{sent:?}");
    assert!(
        trees
            .remote()
            .get(&"x".into())
            .is_some_and(|node| !node.trashed)
    );
    assert_eq!(local(&trees), [("x".into(), ROOT.into(), "server".into())]);
}

fn discarded_after_a_server_folder_moved_in_while_excluded(
    before: &[(&str, Shape)],
    excluded: &[(&str, Shape)],
    answered: Option<&[(&str, Shape)]>,
    restart: bool,
) -> (Vec<(&'static str, String)>, Trees, Commits) {
    discarded_after_a_server_folder_moved_in(before, excluded, answered, restart, false, false)
}

fn discarded_after_a_server_folder_moved_in(
    before: &[(&str, Shape)],
    excluded: &[(&str, Shape)],
    answered: Option<&[(&str, Shape)]>,
    restart: bool,
    nested: bool,
    in_view: bool,
) -> (Vec<(&'static str, String)>, Trees, Commits) {
    let (mut sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(before, excluded, answered, nested, in_view);
    if restart {
        trees = restarted(&trees);
        commits = Commits::from_state(commits.state()).unwrap();
    }
    sent.extend(quiet(&mut trees, &mut commits, &mut ids));
    (sent, trees, commits)
}

fn answered_discard_of_a_server_folder_moved_in(
    before: &[(&str, Shape)],
    excluded: &[(&str, Shape)],
    answered: Option<&[(&str, Shape)]>,
    nested: bool,
    in_view: bool,
) -> (Vec<(&'static str, String)>, Trees, Commits, IdGen) {
    let mut trees = trees(
        &[
            ("h", dir(ROOT, "a")),
            ("s", dir(ROOT, "b")),
            ("sd", dir("s", "d")),
            ("sx", file("s", "y", 4)),
            ("hc", dir("s", "C.TXT")),
            ("hx", file("hc", "x", 3)),
            ("sf", file(ROOT, "c.txt", 5)),
        ],
        &[
            ("s", dir(ROOT, "b")),
            ("sd", dir("s", "d")),
            ("sx", file("s", "y", 4)),
            ("sf", file(ROOT, "c.txt", 5)),
        ],
    );
    if nested {
        trees.set_local("hd".into(), local_node(dir("hc", "inner"), 98));
        trees.settle(&Default::default());
    }
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let change = |trees: &mut Trees, commits: &Commits, (id, shape): &(&str, Shape)| {
        let node = trees.local().get(&(*id).into()).unwrap().clone();
        let shape = shape.clone();
        trees.set_local_changed((*id).into(), LocalNode { shape, ..node });
        trees.settle(commits.aliases());
    };
    before
        .iter()
        .for_each(|moved| change(&mut trees, &commits, moved));
    let root: [&str; 0] = [];
    let into_h = ("s", dir("h", "c.txt"));
    if in_view {
        change(&mut trees, &commits, &into_h);
    }
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"a\n")));
    if !in_view {
        change(&mut trees, &commits, &into_h);
    }
    excluded
        .iter()
        .for_each(|moved| change(&mut trees, &commits, moved));
    trees.set_rules(&root, Source::Koiignore, None);
    trees.settle(commits.aliases());
    let mut sent = Vec::new();
    while let Some(open) =
        (commits.questions().iter()).find(|asked| answered.is_some() && asked.answer.is_none())
    {
        let id = open.id.clone();
        assert!(commits.decide(&mut trees, &id, Answer::Discard));
        sent.extend(ops(&mut commits, &mut trees, &mut ids));
    }
    for moved in answered.unwrap_or_default() {
        change(&mut trees, &commits, moved);
    }
    (sent, trees, commits, ids)
}

fn quiet(trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen) -> Vec<(&'static str, String)> {
    let mut sent = Vec::new();
    let mut effects = Effects::default();
    for ino in 100..160 {
        trees.settle(commits.aliases());
        if let Some(record) = (effects.plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE))
            .first()
            .cloned()
        {
            let identity = FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            };
            (effects.done(
                trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            ))
            .unwrap();
            continue;
        }
        let built = ops(commits, trees, ids);
        if built.is_empty() {
            return sent;
        }
        sent.extend(built);
    }
    panic!("still planning after 60 steps: {sent:?} {:?}", local(trees));
}

fn planned_move(commits: &Commits, id: &str) -> Option<(String, String)> {
    (commits.in_flight()?.commit.ops.iter()).find_map(|op| match &op.op {
        Some(op::Op::Move(moved)) if moved.node_id == id => {
            Some((moved.new_parent_id.clone(), moved.new_name.clone()))
        }
        _ => None,
    })
}

fn assert_discarded_with_the_server_folder_back(sent: &[(&'static str, String)], trees: &Trees) {
    assert!(
        !sent
            .iter()
            .any(|(_, id)| ["h", "s", "hc", "hx", "sf"].contains(&id.as_str())),
        "the Discard undoes the move into the excluded folder, but sends {sent:?}"
    );
    assert_eq!(
        trees.local().get(&"s".into()).unwrap().shape,
        dir(ROOT, "b")
    );
    for gone in ["h", "hc", "hx"] {
        assert!(
            trees.local().get(&gone.into()).is_none(),
            "{gone}: {:?}",
            local(trees)
        );
    }
    assert_eq!(
        trees.local().get(&"sf".into()).unwrap().shape,
        file(ROOT, "c.txt", 5)
    );
}

#[test]
fn a_discard_sends_back_a_server_folder_moved_into_it_while_excluded_with_its_held_folder() {
    for restart in [false, true] {
        let (sent, trees, _) =
            discarded_after_a_server_folder_moved_in_while_excluded(&[], &[], Some(&[]), restart);
        assert_eq!(sent, [], "restart {restart}");
        assert_discarded_with_the_server_folder_back(&sent, &trees);
        for (id, shape) in [("sd", dir("s", "d")), ("sx", file("s", "y", 4))] {
            assert_eq!(
                trees.local().get(&id.into()).unwrap().shape,
                shape,
                "{restart}"
            );
        }
    }
}

#[test]
fn a_discarded_held_folder_sends_back_a_server_file_the_user_moved_into_it_in_view() {
    for restart in [false, true] {
        let moved = [("sf", file("hc", "c.txt", 5))];
        let (sent, trees, _) = discarded_after_a_server_folder_moved_in_while_excluded(
            &moved,
            &[],
            Some(&[]),
            restart,
        );
        assert_eq!(sent, [], "restart {restart}");
        assert_discarded_with_the_server_folder_back(&sent, &trees);
    }
}

#[test]
fn a_held_folder_moved_deeper_into_the_server_folder_still_goes_back_with_it() {
    let moved = [("hc", dir("sd", "C.TXT")), ("sf", file("hc", "c.txt", 5))];
    let (sent, trees, _) =
        discarded_after_a_server_folder_moved_in_while_excluded(&moved, &[], Some(&[]), false);
    assert_eq!(sent, []);
    assert_discarded_with_the_server_folder_back(&sent, &trees);
}

#[test]
fn a_server_file_moved_within_the_taken_along_folder_in_view_keeps_its_place() {
    let moved = [("sx", file("sd", "z", 4))];
    let (sent, trees, commits) =
        discarded_after_a_server_folder_moved_in_while_excluded(&moved, &[], Some(&[]), false);
    assert_eq!(sent, [("move", "sx".into())]);
    assert_eq!(
        planned_move(&commits, "sx"),
        Some(("sd".into(), "z".into()))
    );
    assert_discarded_with_the_server_folder_back(&sent, &trees);
    assert_eq!(
        trees.local().get(&"sx".into()).unwrap().shape,
        file("sd", "z", 4)
    );
}

#[test]
fn a_held_folder_whose_question_is_open_stays_in_the_server_folder_moved_into_an_excluded_one() {
    let moved = [("sf", file("hc", "c.txt", 5))];
    let (sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(&moved, &[], None, false, false);
    let asked = question(&commits);
    assert_eq!(sent, []);
    assert_eq!(quiet(&mut trees, &mut commits, &mut ids), []);
    let open = question(&commits);
    assert_eq!((open.id, open.answer), (asked.id, None));
    assert_eq!(
        trees.local().get(&"s".into()).unwrap().shape,
        dir("h", "c.txt")
    );
    assert_eq!(
        trees.local().get(&"hc".into()).unwrap().shape,
        dir("s", "C.TXT")
    );
    assert_eq!(
        trees.local().get(&"sf".into()).unwrap().shape,
        file("hc", "c.txt", 5)
    );
}

#[test]
fn a_server_folder_the_user_edited_in_keeps_the_excluded_folder_it_was_moved_into() {
    let edited = [("sx", file("s", "y", 9))];
    let (sent, trees, commits) =
        discarded_after_a_server_folder_moved_in_while_excluded(&[], &edited, Some(&[]), false);
    assert!(sent.contains(&("create dir", "h".into())), "{sent:?}");
    assert_eq!(
        planned_move(&commits, "s"),
        Some(("h".into(), "c.txt".into()))
    );
    assert_eq!(
        trees.local().get(&"s".into()).unwrap().shape,
        dir("h", "c.txt")
    );
}

#[test]
fn a_held_folder_the_user_adds_to_after_the_discard_is_sent_inside_the_server_folder_alone() {
    let after = [("sf", file("hc", "c.txt", 5))];
    let (sent, trees, commits) =
        discarded_after_a_server_folder_moved_in_while_excluded(&[], &[], Some(&after), false);
    assert_eq!(
        planned_move(&commits, "sf"),
        Some(("hc".into(), "c.txt".into()))
    );
    assert_eq!(trees.local().get(&"sf".into()).unwrap().shape, after[0].1);
    assert!(
        !sent.iter().any(|(_, id)| id == "h" || id == "s"),
        "{sent:?}"
    );
    assert!(sent.contains(&("create dir", "hc".into())), "{sent:?}");
    assert_eq!(
        trees.local().get(&"s".into()).unwrap().shape,
        dir(ROOT, "b")
    );
    assert_eq!(
        trees.local().get(&"hc".into()).unwrap().shape,
        dir("s", "C.TXT")
    );
    assert!(
        trees.local().get(&"h".into()).is_none(),
        "{:?}",
        local(&trees)
    );
}

#[test]
fn a_held_folder_nested_in_another_inside_the_server_folder_sends_back_a_server_file_moved_into_it()
{
    let before = [("sf", file("hd", "c.txt", 5))];
    let (sent, trees, _) =
        discarded_after_a_server_folder_moved_in(&before, &[], Some(&[]), false, true, false);
    assert_eq!(sent, [], "{:?}", local(&trees));
    assert_discarded_with_the_server_folder_back(&sent, &trees);
    assert!(
        trees.local().get(&"hd".into()).is_none(),
        "{:?}",
        local(&trees)
    );
}

#[test]
fn a_held_folder_in_a_server_folder_moved_in_view_keeps_the_server_file_the_user_moved_into_it() {
    let before = [("sf", file("hc", "c.txt", 5))];
    let (sent, trees, _) =
        discarded_after_a_server_folder_moved_in(&before, &[], Some(&[]), false, false, true);
    assert_eq!(sent, [("create dir", "h".into()), ("move", "s".into())]);
    assert_eq!(
        trees.local().get(&"s".into()).unwrap().shape,
        dir("h", "c.txt")
    );
    assert_eq!(
        trees.local().get(&"hc".into()).unwrap().shape,
        dir("s", "C.TXT")
    );
    assert_eq!(
        trees.local().get(&"sf".into()).unwrap().shape,
        file("hc", "c.txt", 5)
    );
}

#[test]
fn a_server_file_edited_in_a_held_folder_inside_the_server_folder_keeps_its_edit() {
    let before = [
        ("sf", file("hc", "c.txt", 5)),
        ("sf", file("hc", "c.txt", 9)),
    ];
    let (sent, trees, _) =
        discarded_after_a_server_folder_moved_in_while_excluded(&before, &[], Some(&[]), false);
    assert!(sent.contains(&("update", "sf".into())), "{sent:?}");
    assert_eq!(
        trees.local().get(&"sf".into()).unwrap().shape,
        file("hc", "c.txt", 9)
    );
}

#[test]
fn deleting_a_server_file_another_device_moves_into_the_taken_along_folder_changes_that_file_alone()
{
    for (restart, before_the_upgrade) in [(false, false), (true, false), (true, true)] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let sf: NodeId = "sf".into();
        another_device_moves(&mut trees, &commits, &sf, file("s", "moved.txt", 5));
        if restart {
            let mut state = trees.state();
            if before_the_upgrade {
                state.discard_members.clear();
            }
            trees = restarted_from(&trees, &[], state);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.remove_local(&sf);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        if before_the_upgrade {
            assert_eq!(
                sent,
                [
                    ("create dir", "h".into()),
                    ("move", "s".into()),
                    ("delete", "sf".into())
                ]
            );
            continue;
        }
        assert_eq!(sent, [("delete", "sf".into())], "restart {restart}");
        assert_eq!(
            trees.local().get(&"s".into()).unwrap().shape,
            dir(ROOT, "b")
        );
        for gone in ["h", "hc", "hx"] {
            assert!(
                trees.local().get(&gone.into()).is_none(),
                "{:?}",
                local(&trees)
            );
        }
    }
}

fn another_device_moves(trees: &mut Trees, commits: &Commits, id: &NodeId, shape: Shape) {
    let mut moved = trees.remote().get(id).unwrap().clone();
    (moved.shape, moved.meta_rev, moved.meta_edit_rev) = (shape.clone(), 2, 2);
    trees.set_remote(id.clone(), moved);
    let downloaded = trees.local().get(id).unwrap().clone();
    trees.set_local(
        id.clone(),
        LocalNode {
            shape: shape.clone(),
            ..downloaded
        },
    );
    trees.set_synced(id.clone(), Some(shape));
    trees.settle(commits.aliases());
}

#[test]
fn deleting_an_entry_another_device_restores_into_the_taken_along_folder_changes_that_entry_alone()
{
    let (sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
    assert_eq!(sent, []);
    let sf: NodeId = "sf".into();
    let mut node = trees.remote().get(&sf).unwrap().clone();
    (node.trashed, node.meta_rev) = (true, 2);
    trees.set_remote(sf.clone(), node.clone());
    trees.settle(commits.aliases());
    let restored = file("s", "c.txt", 5);
    (node.trashed, node.meta_rev, node.shape) = (false, 3, restored.clone());
    trees.set_remote(sf.clone(), node);
    trees.set_local(sf.clone(), local_node(restored.clone(), 77));
    trees.set_synced(sf.clone(), Some(restored));
    trees.settle(commits.aliases());
    trees.remove_local(&sf);
    let sent = quiet(&mut trees, &mut commits, &mut ids);
    assert_eq!(sent, [("delete", "sf".into())], "{:?}", local(&trees));
    assert!(trees.local().get(&"h".into()).is_none());
}

#[test]
fn deleting_an_entry_another_device_moves_within_the_taken_along_folder_makes_its_discarded_folder_the_users()
 {
    let (sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
    assert_eq!(sent, []);
    let sx: NodeId = "sx".into();
    another_device_moves(&mut trees, &commits, &sx, file("sd", "moved.txt", 4));
    trees.remove_local(&sx);
    let sent = quiet(&mut trees, &mut commits, &mut ids);
    assert!(sent.contains(&("create dir", "h".into())), "{sent:?}");
    assert!(sent.contains(&("delete", "sx".into())), "{sent:?}");
}

#[test]
fn deleting_an_entry_inside_a_folder_another_device_moves_into_the_taken_along_folder_changes_that_entry_alone()
 {
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let (incoming, inside): (NodeId, NodeId) = ("incoming".into(), "inside".into());
        for (id, shape, like, ino) in [
            (&incoming, dir(ROOT, "incoming"), "sd", 181),
            (&inside, file("incoming", "file", 5), "sf", 182),
        ] {
            let mut remote = trees.remote().get(&like.into()).unwrap().clone();
            remote.shape = shape.clone();
            trees.set_remote(id.clone(), remote);
            trees.set_local(id.clone(), local_node(shape.clone(), ino));
            trees.set_synced(id.clone(), Some(shape));
        }
        trees.settle(commits.aliases());
        another_device_moves(&mut trees, &commits, &incoming, dir("s", "incoming"));
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.remove_local(&inside);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [("delete", "inside".into())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
    }
}

#[test]
fn an_entry_the_user_adds_to_the_taken_along_folder_makes_its_discarded_folder_the_users() {
    for moved_in in [true, false] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let (added, sends) = match moved_in {
            true => ("sf", "move"),
            false => ("new", "create file"),
        };
        let shape = file("s", "added.txt", 5);
        let node = (trees.local().get(&added.into()).cloned())
            .unwrap_or_else(|| local_node(shape.clone(), 183));
        trees.set_local_changed(
            added.into(),
            LocalNode {
                shape: shape.clone(),
                ..node
            },
        );
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                (sends, added.into())
            ]
        );
        assert_eq!(
            trees.local().get(&"s".into()).unwrap().shape,
            dir("h", "c.txt")
        );
        assert_eq!(trees.local().get(&added.into()).unwrap().shape, shape);
    }
}

#[test]
fn a_server_folder_whose_unobserved_delete_reverts_is_taken_along_before_its_entries_download_again()
 {
    for unobserved in [true, false] {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("y", dir("x", "y")),
                ("c", file("y", "c", 5)),
            ],
            &[
                ("x", dir(ROOT, "server")),
                ("y", dir("x", "y")),
                ("c", file("y", "c", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored\n")),
        );
        if unobserved {
            let mut renamed = trees.local().get(&"y".into()).unwrap().clone();
            renamed.shape.name = "ignored".into();
            trees.set_local_changed("y".into(), renamed);
            trees.settle(commits.aliases());
            assert!(trees.unobserved(&"c".into()));
            trees.remove_local(&"c".into());
            trees.remove_local(&"y".into());
        } else {
            trees.remove_local(&"c".into());
        }
        trees.settle(commits.aliases());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(
            ops(&mut commits, &mut trees, &mut ids).is_empty(),
            "{unobserved}"
        );
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        if !unobserved {
            assert_eq!(
                sent,
                [
                    ("create dir", "f".to_owned()),
                    ("move", "x".to_owned()),
                    ("delete", "c".to_owned())
                ]
            );
            assert_eq!(
                trees.local().get(&"x".into()).unwrap().shape,
                dir("f", "ignored")
            );
            continue;
        }
        assert!(sent.is_empty(), "{sent:?}");
        assert!(drive_until_quiet(&mut commits, &mut trees, &mut ids).is_empty());
        assert_eq!(
            local(&trees),
            [
                ("c".into(), "y".into(), "c".into()),
                ("x".into(), ROOT.into(), "server".into()),
                ("y".into(), "x".into(), "y".into())
            ]
        );
    }
}

#[test]
fn an_unobserved_delete_in_the_batch_that_changes_the_rules_does_not_make_the_folder_the_users() {
    let mut trees = trees(
        &[
            ("f", dir(ROOT, "new")),
            ("x", dir(ROOT, "server")),
            ("y", dir("x", "y")),
            ("c", file("y", "c", 5)),
        ],
        &[
            ("x", dir(ROOT, "server")),
            ("y", dir("x", "y")),
            ("c", file("y", "c", 5)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut renamed = trees.local().get(&"y".into()).unwrap().clone();
    renamed.shape.name = "ignored".into();
    trees.set_local_changed("y".into(), renamed);
    trees.remove_local(&"c".into());
    trees.remove_local(&"y".into());
    trees.settle(commits.aliases());
    let mut moved = trees.local().get(&"x".into()).unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    assert_eq!(drive_until_quiet(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_folder_whose_copy_disappears_while_unobserved_downloads_again_around_a_folder_the_user_moved_out_of_it()
 {
    for included in [false, true] {
        let mut trees = trees(
            &[
                ("d", dir(ROOT, "d")),
                ("x", dir("d", "child")),
                ("g", dir(ROOT, "g")),
                ("b", dir(ROOT, "b")),
            ],
            &[
                ("d", dir(ROOT, "d")),
                ("x", dir("d", "child")),
                ("g", dir(ROOT, "g")),
                ("f", file("d", "f", 1)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"/build/\na\n")),
        );
        trees.set_local_changed("a".into(), local_node(dir("b", "a"), 9));
        trees.settle(commits.aliases());
        trees.ignore_local(&"d".into());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = if included { "g" } else { "a" }.into();
        trees.set_local_changed("x".into(), moved);
        trees.remove_local(&"d".into());
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        let moved_to = if included { "g" } else { "a" };
        let expected: Vec<(NodeId, NodeId, String)> = [
            ("a", "b", "a"),
            ("b", ROOT, "b"),
            ("d", ROOT, "d"),
            ("f", "d", "f"),
            ("g", ROOT, "g"),
            ("x", moved_to, "child"),
        ]
        .into_iter()
        .map(|(id, parent, name)| (id.into(), parent.into(), name.into()))
        .collect();
        assert_eq!(local(&trees), expected, "{included}: {sent:?}");
        let moves: &[(&str, String)] = if included {
            &[("move", String::new())]
        } else {
            &[]
        };
        assert_eq!(sent.len(), moves.len(), "{included}: {sent:?}");
        assert!(
            sent.iter().all(|op| *op == ("move", "x".into())),
            "{included}: {sent:?}"
        );
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{included}");
        if included {
            continue;
        }
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"/build/\n")),
        );
        for id in ["a", "x"] {
            let scanned = trees.local().get(&id.into()).unwrap().clone();
            trees.set_local_changed(id.into(), scanned);
        }
        assert_eq!(drive_until_quiet(&mut commits, &mut trees, &mut ids), []);
        let asked = question(&commits);
        assert!(commits.decide(&mut trees, &asked.id, Answer::Publish));
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "b".into()),
                ("create dir", "a".into()),
                ("move", "x".into())
            ]
        );
        assert_eq!(
            trees.remote().get(&"x".into()).unwrap().shape,
            dir("a", "child")
        );
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    }
}

#[test]
fn a_folder_moved_out_of_a_vanished_folder_survives_a_server_purge() {
    for case in [
        "",
        "into an excluded folder",
        "deleted here after the release",
    ] {
        let rules: &[u8] = match case {
            "into an excluded folder" => b"/build/\n/g/\n",
            _ => b"/build/\n",
        };
        let mut trees = trees(
            &[
                ("d", dir(ROOT, "d")),
                ("x", dir("d", "child")),
                ("g", dir(ROOT, "g")),
            ],
            &[
                ("d", dir(ROOT, "d")),
                ("x", dir("d", "child")),
                ("g", dir(ROOT, "g")),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(rules)),
        );
        trees.settle(commits.aliases());
        trees.ignore_local(&"d".into());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "g".into();
        trees.set_local_changed("x".into(), moved);
        trees.remove_local(&"d".into());
        trees.settle(commits.aliases());
        assert_eq!(trees.synced().get(&"x".into()), None, "{case}");
        if case == "deleted here after the release" {
            trees.remove_local(&"x".into());
        }
        trees.remove_remote(&"x".into());
        trees.settle(commits.aliases());
        if case == "into an excluded folder" {
            assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
            trees.set_rules(
                &[] as &[&str],
                Source::Koiignore,
                Some(RuleFile::parse(b"/build/\n")),
            );
            trees.listed(&[ROOT.into(), "g".into()], Pending::default());
            trees.settle(commits.aliases());
        }
        let limit = BatchLimit {
            effects: 100,
            bytes: u64::MAX,
        };
        let planned = Effects::default()
            .plan(&trees, commits.in_flight(), &mut ids, NOW, limit)
            .to_vec();
        assert!(
            !planned.iter().any(|record| record.node == "x"),
            "{case}: {planned:?}"
        );
        let sent = ops(&mut commits, &mut trees, &mut ids);
        if case == "deleted here after the release" {
            assert_eq!(sent, [], "{case}");
            assert_eq!(trees.local().get(&"x".into()), None);
        } else {
            assert_eq!(sent, [("create dir", "x".into())], "{case}");
        }
    }
}

#[test]
fn a_taken_along_server_folder_stays_in_a_discarded_folder_the_user_changes_while_its_old_place_is_excluded()
 {
    for undone in [false, true] {
        let mut trees = trees(
            &[
                ("d", dir(ROOT, "out")),
                ("h", dir("d", "held")),
                ("g", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("c", dir("x", "child")),
            ],
            &[
                ("d", dir(ROOT, "out")),
                ("x", dir(ROOT, "server")),
                ("c", dir("x", "child")),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"/out\n")),
        );
        trees.settle(commits.aliases());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "h".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        let mut moved = trees.local().get(&"c".into()).unwrap().clone();
        moved.shape.parent = "h".into();
        trees.set_local_changed("c".into(), moved);
        let mut moved = trees.local().get(&"d".into()).unwrap().clone();
        moved.shape.parent = "g".into();
        trees.set_local_changed("d".into(), moved);
        trees.settle(commits.aliases());
        let mut effects = Effects::default();
        let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
        assert!(
            planned.iter().all(|record| record.node != "x"),
            "{undone}: {planned:?}"
        );
        if undone {
            let mut moved = trees.local().get(&"c".into()).unwrap().clone();
            moved.shape.parent = "x".into();
            trees.set_local_changed("c".into(), moved);
        }
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        let (held, x_in) = match undone {
            false => (vec![("h".into(), "d".into(), "held".into())], "h"),
            true => (vec![], ROOT),
        };
        let expected: Vec<(NodeId, NodeId, String)> = [
            (
                "c".into(),
                if undone { "x" } else { "h" }.into(),
                "child".into(),
            ),
            ("d".into(), "g".into(), "out".into()),
            ("g".into(), ROOT.into(), "new".into()),
        ]
        .into_iter()
        .chain(held)
        .chain([("x".into(), x_in.into(), "server".into())])
        .collect();
        assert_eq!(local(&trees), expected, "{undone}: {sent:?}");
        assert!(!undone || sent.iter().all(|(_, id)| id != "h"), "{sent:?}");
    }
}

#[test]
fn an_adopted_file_another_device_trashes_with_its_bytes_goes_to_the_local_trash() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1))],
        &[("r-a", file(ROOT, "a", 1))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    let mut node = trees.remote().get(&"r-a".into()).unwrap().clone();
    node.trashed = true;
    node.meta_rev = 2;
    node.meta_edit_rev = 2;
    trees.set_remote("r-a".into(), node);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let planned = Effects::default()
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .to_vec();
    assert!(
        matches!(&planned[..], [record] if record.node == "r-a" && matches!(record.effect, Effect::RemoveFile { .. })),
        "{planned:?}"
    );
}

#[test]
fn a_folder_moved_out_of_a_vanished_folder_sends_the_users_removals_below_it_once_its_place_is_let_in()
 {
    for removed in [&["c", "y"][..], &["c"]] {
        let shapes = [
            ("d", dir(ROOT, "d")),
            ("x", dir("d", "x")),
            ("y", dir("x", "y")),
            ("c", file("y", "c", 5)),
            ("g", dir(ROOT, "g")),
        ];
        let mut trees = trees(&shapes, &shapes);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"/build/\n/g/\n")),
        );
        trees.settle(commits.aliases());
        for id in removed {
            trees.remove_local(&(*id).into());
        }
        trees.settle(commits.aliases());
        trees.ignore_local(&"d".into());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "g".into();
        trees.set_local_changed("x".into(), moved);
        trees.remove_local(&"d".into());
        trees.settle(commits.aliases());
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert_eq!(sent, [], "{removed:?}");
        let held = |trees: &Trees, id: &str| trees.local().get(&id.into()).is_some();
        assert!(held(&trees, "d"), "{removed:?}: {:?}", local(&trees));
        assert!(!held(&trees, "c"), "{removed:?}");
        assert_eq!(held(&trees, "y"), removed.len() == 1, "{removed:?}");
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"/build/\n")),
        );
        trees.listed(&[ROOT.into(), "g".into()], Pending::default());
        trees.settle(commits.aliases());
        let mut sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        sent.sort();
        let deleted = removed.last().unwrap().to_string();
        assert_eq!(
            sent,
            [("delete", deleted.clone()), ("move", "x".into())],
            "{removed:?}"
        );
        assert!(!held(&trees, "c"), "{removed:?}");
        assert_eq!(held(&trees, "y"), removed.len() == 1, "{removed:?}");
        assert!(
            trees
                .remote()
                .get(&deleted.as_str().into())
                .unwrap()
                .trashed
        );
        assert_eq!(drive_until_quiet(&mut commits, &mut trees, &mut ids), []);
        let mut effects = Effects::default();
        let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
        assert!(planned.is_empty(), "{removed:?}: {planned:?}");
    }
}

#[test]
fn a_server_folder_is_the_users_once_the_user_deletes_one_of_its_entries_inside_a_new_folder() {
    for restart in [false, true] {
        for (left_in, kept) in [("x", false), ("z", false), (ROOT, false), (ROOT, true)] {
            let mut trees = trees(
                &[
                    ("f", dir(ROOT, "new")),
                    ("x", dir(ROOT, "server")),
                    ("e", file("x", "kept", 5)),
                    ("z", dir(ROOT, "other")),
                    ("k", file("z", "keep", 6)),
                ],
                &[
                    ("x", dir(ROOT, "server")),
                    ("e", file("x", "kept", 5)),
                    ("z", dir(ROOT, "other")),
                    ("k", file("z", "keep", 6)),
                ],
            );
            let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
            commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
            let asked = question(&commits);
            trees.set_rules(
                &[] as &[&str],
                Source::Koiignore,
                Some(RuleFile::parse(b"ignored*\n")),
            );
            trees.settle(commits.aliases());
            for (id, name) in [("x", "ignored"), ("z", "ignored2")] {
                let mut moved = trees.local().get(&id.into()).unwrap().clone();
                moved.shape.parent = "f".into();
                moved.shape.name = name.into();
                trees.set_local_changed(id.into(), moved);
            }
            trees.settle(commits.aliases());
            assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
            assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
            trees.set_rules(&[] as &[&str], Source::Koiignore, None);
            trees.set_local_changed("y".into(), local_node(dir("x", "made"), 40));
            let mut moved = trees.local().get(&"e".into()).unwrap().clone();
            moved.shape.parent = "y".into();
            trees.set_local_changed("e".into(), moved);
            trees.remove_local(&"e".into());
            let mut moved = trees.local().get(&"y".into()).unwrap().clone();
            moved.shape.parent = left_in.into();
            trees.set_local_changed("y".into(), moved);
            if !kept {
                trees.remove_local(&"y".into());
            }
            if restart {
                trees = restarted(&trees);
            }
            let mut sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
            sent.sort();
            let made = kept.then(|| ("create dir", "y".to_owned()));
            let expected: Vec<(&str, String)> = [
                ("create dir", "f".to_owned()),
                ("delete", "e".to_owned()),
                ("move", "x".to_owned()),
                ("move", "z".to_owned()),
            ]
            .into_iter()
            .chain(made)
            .collect();
            let mut expected = expected;
            expected.sort();
            assert_eq!(sent, expected, "{restart} {left_in} {kept}");
            assert!(
                local(&trees).contains(&("x".into(), "f".into(), "ignored".into())),
                "{restart} {left_in} {kept}: {:?}",
                local(&trees)
            );
        }
    }
}

#[test]
fn a_server_folder_is_the_users_only_when_the_user_deletes_one_of_its_entries_while_it_is_inside() {
    for (case, nested, y_in) in [
        ("nested", true, Some("x")),
        ("outside", false, Some(ROOT)),
        ("held", false, None),
    ] {
        let mut trees = trees(
            &[
                ("f", dir(ROOT, "new")),
                ("x", dir(ROOT, "server")),
                ("h", dir("x", "ignored-held")),
                ("w", dir("x", "inner")),
                ("e", file(if nested { "w" } else { "x" }, "kept", 5)),
            ],
            &[
                ("x", dir(ROOT, "server")),
                ("w", dir("x", "inner")),
                ("e", file(if nested { "w" } else { "x" }, "kept", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"ignored*\n")),
        );
        trees.settle(commits.aliases());
        let mut moved = trees.local().get(&"x".into()).unwrap().clone();
        moved.shape.parent = "f".into();
        moved.shape.name = "ignored".into();
        trees.set_local_changed("x".into(), moved);
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        if let Some(y_in) = y_in {
            trees.set_local_changed("y".into(), local_node(dir(y_in, "made"), 40));
        }
        let mut moved = trees.local().get(&"e".into()).unwrap().clone();
        moved.shape.parent = if y_in.is_some() { "y" } else { "h" }.into();
        trees.set_local_changed("e".into(), moved);
        trees.remove_local(&"e".into());
        trees.remove_local(&"y".into());
        let mut sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        sent.sort();
        let x_in = if nested { "f" } else { ROOT };
        let expected: Vec<(&str, String)> = match nested {
            true => vec![
                ("create dir", "f".into()),
                ("delete", "e".into()),
                ("move", "x".into()),
            ],
            false => vec![("delete", "e".into())],
        };
        assert_eq!(sent, expected, "{case}");
        let x = trees.local().get(&"x".into()).unwrap();
        assert_eq!(
            x.shape.parent,
            NodeId::from(x_in),
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn a_server_file_another_device_moves_into_a_taken_along_folder_goes_to_the_local_trash() {
    let mut trees = trees(
        &[
            ("h", dir(ROOT, "a")),
            ("s", dir(ROOT, "b")),
            ("sx", file("s", "y", 4)),
            ("m", file(ROOT, "m.txt", 12)),
        ],
        &[
            ("s", dir(ROOT, "b")),
            ("sx", file("s", "y", 4)),
            ("m", file(ROOT, "m.txt", 12)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"a\n")),
    );
    let mut taken = trees.local().get(&"s".into()).unwrap().clone();
    taken.shape = dir("h", "c.txt");
    trees.set_local_changed("s".into(), taken);
    trees.settle(commits.aliases());
    while let Some(open) = (commits.questions().iter()).find(|asked| asked.answer.is_none()) {
        let id = open.id.clone();
        assert!(commits.decide(&mut trees, &id, Answer::Discard));
    }
    drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert!(trees.excluded(&"s".into()));
    let mut moved = trees.remote().get(&"m".into()).unwrap().clone();
    moved.shape.parent = "s".into();
    moved.meta_rev += 1;
    moved.meta_edit_rev += 1;
    trees.set_remote("m".into(), moved);
    assert_eq!(drive_until_quiet(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.local().get(&"m".into()).is_none());
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    drive_until_quiet(&mut commits, &mut trees, &mut ids);
    assert_eq!(
        trees
            .local()
            .get(&"m".into())
            .map(|node| node.shape.parent.clone()),
        Some("s".into())
    );
}

#[test]
fn a_server_entry_another_device_moves_in_between_the_discard_answer_and_its_build_is_not_one_of_its_entries()
 {
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
        assert_eq!(sent, []);
        let question = question(&commits);
        assert!(commits.decide(&mut trees, &question.id, Answer::Discard));
        assert!(commits.questions()[0].frozen.is_some());
        let sf: NodeId = "sf".into();
        another_device_moves(&mut trees, &commits, &sf, file("s", "moved.txt", 5));
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert!(commits.questions().is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.remove_local(&sf);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [("delete", "sf".into())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
    }
}

#[test]
fn deleting_an_entry_another_device_creates_in_a_taken_along_folder_after_the_answer_changes_that_entry_alone()
 {
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
        assert_eq!(sent, []);
        let question = question(&commits);
        assert!(commits.decide(&mut trees, &question.id, Answer::Discard));
        let created: NodeId = "created".into();
        another_device_places(
            &mut trees,
            &commits,
            "created",
            file("s", "created.txt", 6),
            2,
        );
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert!(commits.questions().is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.remove_local(&created);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [("delete", "created".into())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
        let s = trees.local().get(&"s".into()).unwrap();
        assert_eq!(
            (s.shape.parent.clone(), s.shape.name.as_str()),
            (ROOT.into(), "b"),
            "{restart}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum UserChange {
    Edit,
    Rename,
    Move,
}

fn change_as_the_user(trees: &mut Trees, id: &NodeId, change: UserChange) -> &'static str {
    let mut node = trees.local().get(id).unwrap().clone();
    let sends = match change {
        UserChange::Edit => {
            node.shape.content = Some(content(9));
            "update"
        }
        UserChange::Rename => {
            node.shape.name = "renamed.txt".into();
            "move"
        }
        UserChange::Move => {
            node.shape.parent = "sd".into();
            "move"
        }
    };
    trees.set_local_changed(id.clone(), node);
    sends
}

fn folders_of(stays: &BTreeSet<(NodeId, NodeId)>, id: &NodeId) -> Vec<String> {
    (stays.iter())
        .filter(|(node, _)| node == id)
        .map(|(_, folder)| folder.to_string())
        .collect()
}

fn another_device_brings_into_the_taken_along_folder(
    inside: bool,
) -> (Trees, Commits, IdGen, NodeId) {
    let (sent, mut trees, commits, ids) =
        answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
    assert_eq!(sent, []);
    if !inside {
        let sf: NodeId = "sf".into();
        another_device_moves(&mut trees, &commits, &sf, file("s", "moved.txt", 5));
        return (trees, commits, ids, sf);
    }
    let (incoming, inside): (NodeId, NodeId) = ("incoming".into(), "inside".into());
    for (id, shape, like, ino) in [
        (&incoming, dir(ROOT, "incoming"), "sd", 181),
        (&inside, file("incoming", "file", 5), "sf", 182),
    ] {
        let mut remote = trees.remote().get(&like.into()).unwrap().clone();
        remote.shape = shape.clone();
        trees.set_remote(id.clone(), remote);
        trees.set_local(id.clone(), local_node(shape.clone(), ino));
        trees.set_synced(id.clone(), Some(shape));
    }
    trees.settle(commits.aliases());
    another_device_moves(&mut trees, &commits, &incoming, dir("s", "incoming"));
    (trees, commits, ids, inside)
}

#[test]
fn a_change_the_user_makes_to_an_entry_another_device_brings_into_the_taken_along_folder_changes_that_entry_alone()
 {
    for change in [UserChange::Edit, UserChange::Rename, UserChange::Move] {
        for (inside, restart) in [(false, false), (false, true), (true, false), (true, true)] {
            let case = format!("{change:?} inside {inside} restart {restart}");
            let (mut trees, mut commits, mut ids, id) =
                another_device_brings_into_the_taken_along_folder(inside);
            if restart {
                trees = restarted(&trees);
                commits = Commits::from_state(commits.state()).unwrap();
            }
            let sends = change_as_the_user(&mut trees, &id, change);
            let sent = quiet(&mut trees, &mut commits, &mut ids);
            assert_eq!(
                sent,
                [(sends, id.to_string())],
                "{case}: {:?}",
                local(&trees)
            );
            assert_eq!(
                trees.local().get(&"s".into()).unwrap().shape,
                dir(ROOT, "b"),
                "{case}"
            );
            assert!(trees.local().get(&"h".into()).is_none(), "{case}");
        }
    }
}

#[test]
fn a_store_from_before_arrivals_were_recorded_makes_the_discarded_folder_the_users_when_the_user_edits_one()
 {
    let (mut trees, mut commits, mut ids, sf) =
        another_device_brings_into_the_taken_along_folder(false);
    let mut state = trees.state();
    assert!(folders_of(&state.discard_stays, &sf).is_empty());
    state.discard_stays.clear();
    state.first_link_open.clear();
    trees = restarted_from(&trees, &[], state);
    commits = Commits::from_state(commits.state()).unwrap();
    change_as_the_user(&mut trees, &sf, UserChange::Edit);
    assert_eq!(
        quiet(&mut trees, &mut commits, &mut ids),
        [
            ("create dir", "h".into()),
            ("move", "s".into()),
            ("update", "sf".into())
        ]
    );
}

#[test]
fn an_entry_the_user_adds_inside_an_entry_another_device_brought_in_makes_the_discarded_folder_the_users()
 {
    for moved_in in [false, true] {
        let (mut trees, mut commits, mut ids, _) =
            another_device_brings_into_the_taken_along_folder(true);
        let (added, sends) = match moved_in {
            true => ("sx", "move"),
            false => ("new", "create file"),
        };
        let shape = file("incoming", "added.txt", 4);
        let node = (trees.local().get(&added.into()).cloned())
            .unwrap_or_else(|| local_node(shape.clone(), 183));
        trees.set_local_changed(added.into(), LocalNode { shape, ..node });
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                (sends, added.into())
            ],
            "{moved_in}"
        );
    }
}

#[test]
fn moving_an_entry_another_device_brought_into_one_taken_along_folder_into_another_makes_that_folder_the_users()
 {
    for (from, to, source, destination) in [("x", "s2", "h1", "h2"), ("s2", "x", "h2", "h1")] {
        let mut trees = trees(
            &[
                ("h1", dir(ROOT, "a")),
                ("h2", dir(ROOT, "b")),
                ("x", dir(ROOT, "first")),
                ("s2", dir(ROOT, "second")),
                ("sf", file(ROOT, "note.txt", 5)),
            ],
            &[
                ("x", dir(ROOT, "first")),
                ("s2", dir(ROOT, "second")),
                ("sf", file(ROOT, "note.txt", 5)),
            ],
        );
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        let asked = question(&commits);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"a\nb\n")),
        );
        for (id, parent) in [("x", "h1"), ("s2", "h2")] {
            let mut moved = trees.local().get(&id.into()).unwrap().clone();
            moved.shape.parent = parent.into();
            trees.set_local_changed(id.into(), moved);
        }
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.settle(commits.aliases());
        let sf: NodeId = "sf".into();
        another_device_moves(&mut trees, &commits, &sf, file(from, "note.txt", 5));
        let mut moved = trees.local().get(&sf).unwrap().clone();
        moved.shape.parent = to.into();
        trees.set_local_changed(sf, moved);
        let sent = drive_until_quiet(&mut commits, &mut trees, &mut ids);
        assert!(
            sent.contains(&("create dir", destination.into())),
            "{from} -> {to}: {sent:?}"
        );
        assert!(!sent.contains(&("create dir", source.into())), "{sent:?}");
        assert!(sent.contains(&("move", "sf".into())), "{sent:?}");
    }
}

fn a_held_folder_in_a_trashed_server_folder_a_discard_takes_along(
    built: bool,
) -> (Trees, Commits, IdGen) {
    let mut trees = trees(
        &[
            ("n", dir(ROOT, "a")),
            ("p", dir(ROOT, "b")),
            ("h", dir("p", "held")),
            ("q", dir("h", "q")),
            ("f", file("q", "f", 5)),
        ],
        &[
            ("p", dir(ROOT, "b")),
            ("q", dir(ROOT, "q")),
            ("f", file("q", "f", 5)),
        ],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Source::Koiignore, Some(RuleFile::parse(b"a\n")));
    let mut moved = trees.local().get(&"p".into()).unwrap().clone();
    moved.shape = dir("n", "b");
    trees.set_local_changed("p".into(), moved);
    trees.settle(commits.aliases());
    let mut trashed = trees.remote().get(&"p".into()).unwrap().clone();
    (trashed.trashed, trashed.meta_rev) = (true, 2);
    trees.set_remote("p".into(), trashed);
    trees.set_rules(&root, Source::Koiignore, None);
    trees.settle(commits.aliases());
    while let Some(open) = (commits.questions().iter()).find(|asked| asked.answer.is_none()) {
        let id = open.id.clone();
        assert!(commits.decide(&mut trees, &id, Answer::Discard));
    }
    if built {
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert!(commits.questions().is_empty());
    }
    (trees, commits, ids)
}

fn edit_f(trees: &mut Trees) {
    let mut edited = trees.local().get(&"f".into()).unwrap().clone();
    edited.shape.content = Some(content(9));
    trees.set_local_changed("f".into(), edited);
}

#[test]
fn an_edit_in_a_held_folder_a_discard_dropped_inside_a_trashed_server_folder_is_sent_there_and_the_outer_folder_stays_discarded()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids) =
            a_held_folder_in_a_trashed_server_folder_a_discard_takes_along(true);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        edit_f(&mut trees);
        trees.settle(commits.aliases());
        let commit = (commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 3))
            .cloned()
            .unwrap();
        let sent: Vec<(&str, String, String)> = (commit.ops.iter())
            .map(|op| match op.op.clone().unwrap() {
                op::Op::CreateDir(c) => ("create dir", c.node_id, c.parent_id),
                op::Op::Move(m) => ("move", m.node_id, m.new_parent_id),
                op::Op::UpdateContent(u) => ("update", u.node_id, String::new()),
                other => panic!("unexpected op {other:?}"),
            })
            .collect();
        assert_eq!(
            sent,
            [
                ("create dir", "h".into(), "p".into()),
                ("move", "q".into(), "h".into()),
                ("update", "f".into(), String::new())
            ],
            "{restart}"
        );
        let mut revived = trees.remote().get(&"p".into()).unwrap().clone();
        (revived.trashed, revived.meta_rev) = (false, 3);
        trees.set_remote_made_here("p".into(), revived.clone());
        let created = RemoteNode {
            shape: dir("p", "held"),
            ..revived
        };
        trees.set_remote_made_here("h".into(), created);
        let mut moved = trees.remote().get(&"q".into()).unwrap().clone();
        (moved.shape, moved.meta_rev, moved.meta_edit_rev) = (dir("h", "q"), 3, 3);
        trees.set_remote_made_here("q".into(), moved);
        let mut edited = trees.remote().get(&"f".into()).unwrap().clone();
        (edited.shape.content, edited.content_rev, edited.edit_rev) = (Some(content(9)), 3, 3);
        trees.set_remote_edited_here("f".into(), edited);
        let results = vec![
            OpResult {
                code: op_result::Code::Applied as i32,
                ..Default::default()
            };
            3
        ];
        commits
            .received(CommitResult {
                seq: 3,
                results,
                ..Default::default()
            })
            .unwrap();
        assert!(commits.acknowledge(&mut trees, 3));
        assert_eq!(quiet(&mut trees, &mut commits, &mut ids), [], "{restart}");
        let remote = trees.remote().get(&"p".into()).unwrap();
        assert!(
            !remote.trashed && remote.shape == dir(ROOT, "b"),
            "{restart}"
        );
        for (id, shape) in [
            ("p", dir(ROOT, "b")),
            ("h", dir("p", "held")),
            ("q", dir("h", "q")),
        ] {
            assert_eq!(
                trees.local().get(&id.into()).map(|node| &node.shape),
                Some(&shape),
                "{restart} {id}: {:?}",
                local(&trees)
            );
        }
        assert!(
            trees.local().get(&"n".into()).is_none(),
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.remote().get(&"n".into()).is_none(), "{restart}");
    }
}

#[test]
fn a_trashed_server_folder_a_discard_takes_along_goes_to_the_local_trash_when_the_user_leaves_it_alone()
 {
    let (mut trees, mut commits, mut ids) =
        a_held_folder_in_a_trashed_server_folder_a_discard_takes_along(true);
    assert_eq!(quiet(&mut trees, &mut commits, &mut ids), []);
    for gone in ["n", "p", "h"] {
        assert!(
            trees.local().get(&gone.into()).is_none(),
            "{gone}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn an_edit_in_a_held_folder_inside_a_trashed_server_folder_before_the_discard_is_built_asks_again()
{
    let (mut trees, mut commits, mut ids) =
        a_held_folder_in_a_trashed_server_folder_a_discard_takes_along(false);
    let answered = question(&commits);
    edit_f(&mut trees);
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        !(sent.iter()).any(|(_, id)| ["n", "p", "h", "q"].contains(&id.as_str())),
        "{sent:?}"
    );
    let asked = question(&commits);
    assert_eq!(asked.answer, None);
    assert_ne!(asked.id, answered.id);
    assert!(trees.local().get(&"n".into()).is_some());
}

#[test]
fn a_change_the_user_makes_to_an_entry_another_device_moves_in_between_the_answer_and_its_build_changes_that_entry_alone()
 {
    for change in [UserChange::Edit, UserChange::Rename, UserChange::Move] {
        for restart in [false, true] {
            let (sent, mut trees, mut commits, mut ids) =
                answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
            assert_eq!(sent, []);
            let asked = question(&commits);
            assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
            let sf: NodeId = "sf".into();
            let frozen = commits.questions()[0].frozen.as_ref().unwrap();
            assert!(frozen.whole && !frozen.below.contains_key(&sf));
            another_device_moves(&mut trees, &commits, &sf, file("s", "moved.txt", 5));
            assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
            assert!(commits.questions().is_empty());
            if restart {
                trees = restarted(&trees);
                commits = Commits::from_state(commits.state()).unwrap();
            }
            trees.listed(&["h".into(), "s".into()], Pending::default());
            let sends = change_as_the_user(&mut trees, &sf, change);
            let sent = quiet(&mut trees, &mut commits, &mut ids);
            assert_eq!(sent, [(sends, "sf".into())], "{change:?} {restart}");
            assert!(
                trees.local().get(&"h".into()).is_none(),
                "{change:?} {restart}"
            );
        }
    }
}

#[test]
fn a_server_only_entry_of_a_discard_answered_before_such_entries_were_frozen_stays_one_of_its_entries()
 {
    for legacy in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
        assert_eq!(sent, []);
        let sn: NodeId = "sn".into();
        let shape = file("s", "missing", 5);
        trees.set_rules(
            &[] as &[&str],
            Source::Koiignore,
            Some(RuleFile::parse(b"missing\n")),
        );
        trees.set_remote(sn.clone(), remote_node(shape.clone()));
        trees.settle(commits.aliases());
        assert!(trees.local().get(&sn).is_none() && trees.synced().get(&sn).is_none());
        let asked = question(&commits);
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        let mut saved = commits.state();
        let frozen = saved.questions[0].frozen.as_mut().unwrap();
        assert!(frozen.whole && frozen.below[&sn].taken);
        if legacy {
            frozen.below.remove(&sn);
            frozen.whole = false;
        }
        trees = restarted(&trees);
        commits = Commits::from_state(saved).unwrap();
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        assert!(commits.questions().is_empty());
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.set_local(sn.clone(), local_node(shape.clone(), 201));
        trees.set_synced(sn.clone(), Some(shape));
        trees.settle(commits.aliases());
        trees.listed(&["h".into(), "s".into()], Pending::default());
        assert!(!trees.unobserved(&sn));
        trees.remove_local(&sn);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                ("delete", "sn".into())
            ],
            "{legacy}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_some(), "{legacy}");
    }
}

#[test]
fn an_entry_the_folder_held_at_the_discard_that_another_device_moves_into_an_arrival_still_makes_the_folder_the_users_when_the_user_changes_it()
 {
    for (delete, restart) in [(true, false), (false, false), (false, true)] {
        let (mut trees, mut commits, mut ids, _) =
            another_device_brings_into_the_taken_along_folder(true);
        let sx: NodeId = "sx".into();
        assert_eq!(trees.state().discard_members.get(&sx), Some(&"s".into()));
        another_device_moves(&mut trees, &commits, &sx, file("incoming", "y", 4));
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = match delete {
            true => {
                trees.remove_local(&sx);
                "delete"
            }
            false => change_as_the_user(&mut trees, &sx, UserChange::Edit),
        };
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                (sends, "sx".into())
            ],
            "delete {delete} restart {restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn an_entry_the_folder_held_at_the_answer_that_leaves_it_before_the_build_and_comes_back_is_still_one_of_its_entries()
 {
    for restart in [false, true] {
        let (_, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
        let asked = question(&commits);
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        let sx: NodeId = "sx".into();
        assert!(commits.questions()[0].frozen.as_ref().unwrap().below[&sx].taken);
        another_device_moves(&mut trees, &commits, &sx, file(ROOT, "y", 4));
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        assert!(commits.questions().is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        another_device_moves(&mut trees, &commits, &sx, file("s", "y", 4));
        change_as_the_user(&mut trees, &sx, UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                ("update", "sx".into())
            ],
            "{restart}"
        );
    }
}

#[test]
fn a_rename_of_an_arrival_or_an_entry_that_came_with_it_changes_that_entry_alone_after_another_device_moves_it_out()
 {
    for (with_it, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut trees, mut commits, mut ids, inside) =
            another_device_brings_into_the_taken_along_folder(with_it);
        let arrival: NodeId = if with_it {
            "incoming".into()
        } else {
            inside.clone()
        };
        assert!(folders_of(&trees.state().discard_stays, &arrival).is_empty());
        change_as_the_user(&mut trees, &inside, UserChange::Rename);
        trees.settle(commits.aliases());
        let mut moved = trees.remote().get(&inside).unwrap().clone();
        (moved.shape, moved.meta_rev, moved.meta_edit_rev) = (file(ROOT, "file", 5), 3, 3);
        trees.set_remote(inside.clone(), moved);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(sent, [("move", inside.to_string())], "{with_it} {restart}");
        assert!(
            trees.local().get(&"h".into()).is_none(),
            "{with_it} {restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn an_entry_the_folder_held_that_comes_back_inside_a_folder_another_device_moves_in_is_still_one_of_its_entries()
 {
    let (sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
    assert_eq!(sent, []);
    let (outer, sx): (NodeId, NodeId) = ("outer".into(), "sx".into());
    let mut remote = trees.remote().get(&"sd".into()).unwrap().clone();
    remote.shape = dir(ROOT, "outer");
    trees.set_remote(outer.clone(), remote);
    trees.set_local(outer.clone(), local_node(dir(ROOT, "outer"), 184));
    trees.set_synced(outer.clone(), Some(dir(ROOT, "outer")));
    trees.settle(commits.aliases());
    another_device_moves(&mut trees, &commits, &sx, file("outer", "y", 4));
    another_device_moves(&mut trees, &commits, &outer, dir("s", "outer"));
    let arrivals = trees.state().discard_stays;
    assert!(folders_of(&arrivals, &outer).is_empty());
    assert_eq!(trees.state().discard_members.get(&sx), Some(&"s".into()));
    change_as_the_user(&mut trees, &sx, UserChange::Edit);
    assert_eq!(
        quiet(&mut trees, &mut commits, &mut ids),
        [
            ("create dir", "h".into()),
            ("move", "s".into()),
            ("update", "sx".into())
        ]
    );
}

fn another_devices_folder_with_a_child(trees: &mut Trees, commits: &Commits) -> (NodeId, NodeId) {
    let (outer, inner): (NodeId, NodeId) = ("outer".into(), "inner".into());
    for (id, shape, ino) in [
        (&outer, dir(ROOT, "outer"), 185),
        (&inner, dir("outer", "inner"), 186),
    ] {
        let mut remote = trees.remote().get(&"sd".into()).unwrap().clone();
        remote.shape = shape.clone();
        trees.set_remote(id.clone(), remote);
        trees.set_local(id.clone(), local_node(shape.clone(), ino));
        trees.set_synced(id.clone(), Some(shape));
    }
    trees.settle(commits.aliases());
    (outer, inner)
}

fn another_device_commits_moves(
    trees: &mut Trees,
    commits: &Commits,
    moves: &[(&NodeId, Shape)],
    rev: u64,
) {
    trees.apply_batch(|trees| {
        for (id, shape) in moves {
            let mut moved = trees.remote().get(id).unwrap().clone();
            (moved.shape, moved.meta_rev, moved.meta_edit_rev) = (shape.clone(), rev, rev);
            trees.set_remote((*id).clone(), moved);
        }
    });
    for (id, shape) in moves {
        let downloaded = trees.local().get(id).unwrap().clone();
        trees.set_local(
            (*id).clone(),
            LocalNode {
                shape: shape.clone(),
                ..downloaded
            },
        );
        trees.set_synced((*id).clone(), Some(shape.clone()));
    }
    trees.settle(commits.aliases());
}

#[test]
fn a_batch_that_moves_a_folder_into_its_former_child_records_the_arrival_once_it_has_applied() {
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let (outer, inner) = another_devices_folder_with_a_child(&mut trees, &commits);
        another_device_commits_moves(
            &mut trees,
            &commits,
            &[
                (&outer, dir("inner", "outer2")),
                (&inner, dir("s", "inner")),
            ],
            2,
        );
        let arrivals = trees.state().discard_stays;
        assert!(folders_of(&arrivals, &outer).is_empty(), "{restart}");
        assert!(folders_of(&arrivals, &inner).is_empty(), "{restart}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        change_as_the_user(&mut trees, &outer, UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [("move", outer.to_string())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
    }
}

#[test]
fn a_child_a_batch_moves_out_before_its_folder_arrives_is_not_an_arrival() {
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let (outer, inner) = another_devices_folder_with_a_child(&mut trees, &commits);
        another_device_commits_moves(
            &mut trees,
            &commits,
            &[(&outer, dir("s", "outer2")), (&inner, dir(ROOT, "inner"))],
            2,
        );
        let arrivals = trees.state().discard_stays;
        assert!(folders_of(&arrivals, &outer).is_empty(), "{restart}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let node = trees.local().get(&inner).unwrap().clone();
        trees.set_local_changed(
            inner.clone(),
            LocalNode {
                shape: dir("s", "inner"),
                ..node
            },
        );
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                ("move", inner.to_string())
            ],
            "{restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn moving_in_an_entry_another_device_brought_in_and_took_out_again_makes_the_discarded_folder_the_users()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sf) =
            another_device_brings_into_the_taken_along_folder(false);
        another_device_commits_moves(&mut trees, &commits, &[(&sf, file(ROOT, "c.txt", 5))], 3);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let node = trees.local().get(&sf).unwrap().clone();
        trees.set_local_changed(
            sf.clone(),
            LocalNode {
                shape: file("s", "moved.txt", 5),
                ..node
            },
        );
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "h".into()),
                ("move", "s".into()),
                ("move", "sf".into())
            ],
            "{restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn a_change_to_an_arrival_in_two_nested_taken_along_folders_changes_that_entry_alone_after_the_inner_folders_discard_ends()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, _) =
            another_device_brings_into_the_taken_along_folder(true);
        let incoming: NodeId = "incoming".into();
        let mut state = trees.state();
        state
            .discard_members
            .insert(incoming.clone(), incoming.clone());
        state
            .discarded
            .insert(incoming.clone(), dir("h", "incoming"));
        trees = restarted_from(&trees, &[], state);
        let added: NodeId = "added".into();
        let shape = file("incoming", "added.txt", 6);
        let mut remote = trees.remote().get(&"sf".into()).unwrap().clone();
        remote.shape = shape.clone();
        trees.set_remote(added.clone(), remote);
        trees.set_local(added.clone(), local_node(shape.clone(), 187));
        trees.set_synced(added.clone(), Some(shape));
        trees.settle(commits.aliases());
        assert!(folders_of(&trees.state().discard_stays, &added).is_empty());
        let node = trees.local().get(&incoming).unwrap().clone();
        trees.set_local(incoming.clone(), node);
        trees.settle(commits.aliases());
        assert!(!trees.state().discarded.contains_key(&incoming));
        assert!(folders_of(&trees.state().discard_stays, &added).is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        change_as_the_user(&mut trees, &added, UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [("update", added.to_string())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
    }
}

#[test]
fn renaming_an_entry_another_device_adds_between_the_answer_and_the_build_changes_that_entry_alone()
{
    for restart in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
        assert_eq!(sent, []);
        let asked = question(&commits);
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        let added: NodeId = "added".into();
        let shape = file("s", "added.txt", 6);
        trees.set_remote(added.clone(), remote_node(shape.clone()));
        trees.set_local(added.clone(), local_node(shape.clone(), 188));
        trees.set_synced(added.clone(), Some(shape));
        trees.settle(commits.aliases());
        assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
        assert!(commits.questions().is_empty());
        assert!(folders_of(&trees.state().discard_stays, &added).is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.listed(&["h".into(), "s".into()], Pending::default());
        let sends = change_as_the_user(&mut trees, &added, UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, added.to_string())],
            "{restart}: {:?}",
            local(&trees)
        );
        assert!(trees.local().get(&"h".into()).is_none(), "{restart}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum EditedInPlace {
    Never,
    AtTheOldPlaceInTheAnswerWindow,
    AfterTheConfirmation,
}

#[test]
fn an_entry_this_device_moves_into_a_taken_along_folder_before_the_answer_and_the_server_confirms_after_it_is_one_of_its_entries()
 {
    use EditedInPlace::*;
    for (edited, from_the_log, change, restart) in [
        (Never, false, UserChange::Rename, false),
        (Never, true, UserChange::Rename, true),
        (Never, false, UserChange::Edit, true),
        (Never, true, UserChange::Edit, false),
        (
            AtTheOldPlaceInTheAnswerWindow,
            true,
            UserChange::Rename,
            false,
        ),
        (
            AtTheOldPlaceInTheAnswerWindow,
            false,
            UserChange::Rename,
            true,
        ),
        (AfterTheConfirmation, true, UserChange::Rename, true),
        (AfterTheConfirmation, false, UserChange::Rename, false),
    ] {
        let case = format!("{edited:?} from the log {from_the_log} {change:?} restart {restart}");
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                let sent = this_device_sends_a_move(trees, commits, ids, "m", moved_in.clone());
                assert_eq!(sent, [("move", "m".to_owned())]);
            },
            |trees, commits| {
                let frozen = commits.questions()[0].frozen.as_ref().unwrap();
                assert!(
                    frozen
                        .below
                        .get(&"m".into())
                        .is_some_and(|was| was.taken && was.member_of.is_none())
                );
                if edited == AtTheOldPlaceInTheAnswerWindow {
                    another_device_edits_in_place(trees, commits, "m", 3);
                }
                the_server_confirms_the_move(
                    trees,
                    commits,
                    "m",
                    moved_in.clone(),
                    5,
                    from_the_log,
                );
            },
        );
        assert_eq!(sent, [], "{case}");
        if edited == AfterTheConfirmation {
            another_device_edits_in_place(&mut trees, &commits, "m", 6);
        }
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{case}");
        assert_eq!(
            trees.state().discard_members.get(&"m".into()),
            Some(&"x".into()),
            "{case}"
        );
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"m".into(), change);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "f".into()),
                ("move", "x".into()),
                (sends, "m".into())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

fn a_server_folder_this_device_discards(
    extra: &[(&str, Shape)],
    before_answer: impl FnOnce(&mut Trees, &mut Commits, &mut IdGen),
    in_window: impl FnOnce(&mut Trees, &mut Commits),
) -> (Trees, Commits, IdGen, Vec<(&'static str, String)>) {
    let mut both = vec![
        ("x", dir(ROOT, "server")),
        ("xd", dir("x", "inner")),
        ("c", file("x", "child", 3)),
        ("m", file(ROOT, "m", 7)),
    ];
    both.extend(extra.iter().cloned());
    let mut local = vec![("f", dir(ROOT, "new"))];
    local.extend(both.iter().cloned());
    let mut trees = trees(&local, &both);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    before_answer(&mut trees, &mut commits, &mut ids);
    let ignored = Some(RuleFile::parse(b"ignored\n"));
    trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
    let mut x = trees.local().get(&"x".into()).unwrap().clone();
    (x.shape.parent, x.shape.name) = ("f".into(), "ignored".into());
    trees.set_local_changed("x".into(), x);
    trees.settle(commits.aliases());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    in_window(&mut trees, &mut commits);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(commits.questions().is_empty());
    assert_eq!(
        trees.state().discard_members.get(&"x".into()),
        Some(&"x".into())
    );
    (trees, commits, ids, sent)
}

fn another_device_places(trees: &mut Trees, commits: &Commits, id: &str, shape: Shape, rev: u64) {
    let id: NodeId = id.into();
    let mut placed =
        (trees.remote().get(&id).cloned()).unwrap_or_else(|| remote_node(shape.clone()));
    (placed.shape, placed.meta_rev, placed.meta_edit_rev) = (shape.clone(), rev, rev);
    trees.set_remote(id.clone(), placed);
    match trees.local().get(&id).cloned() {
        Some(downloaded) => trees.set_local(
            id.clone(),
            LocalNode {
                shape: shape.clone(),
                ..downloaded
            },
        ),
        None => trees.set_local(id.clone(), local_node(shape.clone(), 300 + rev as u128)),
    }
    trees.set_synced(id, Some(shape));
    trees.settle(commits.aliases());
}

fn another_device_edits_in_place(trees: &mut Trees, commits: &Commits, id: &str, rev: u64) {
    let id: NodeId = id.into();
    let mut edited = trees.remote().get(&id).unwrap().clone();
    let shape = Shape {
        content: Some(content(40 + rev as u8)),
        ..edited.shape.clone()
    };
    (edited.shape, edited.content_rev, edited.edit_rev) = (shape.clone(), rev, rev);
    trees.set_remote(id.clone(), edited);
    let downloaded = trees.local().get(&id).unwrap().clone();
    trees.set_local(
        id.clone(),
        LocalNode {
            shape: Shape {
                content: shape.content,
                ..downloaded.shape.clone()
            },
            ..downloaded
        },
    );
    trees.set_synced(id, Some(shape));
    trees.settle(commits.aliases());
}

fn this_device_sends_a_move(
    trees: &mut Trees,
    commits: &mut Commits,
    ids: &mut IdGen,
    id: &str,
    shape: Shape,
) -> Vec<(&'static str, String)> {
    let node = trees.local().get(&id.into()).unwrap().clone();
    trees.set_local_changed(id.into(), LocalNode { shape, ..node });
    trees.settle(commits.aliases());
    ops(commits, trees, ids)
}

fn the_server_confirms_the_move(
    trees: &mut Trees,
    commits: &mut Commits,
    id: &str,
    shape: Shape,
    seq: u64,
    from_the_log: bool,
) {
    let result = OpResult {
        code: op_result::Code::Applied as i32,
        node_id: id.into(),
        ..Default::default()
    };
    let received = CommitResult {
        seq,
        results: vec![result],
        ..Default::default()
    };
    if !from_the_log {
        commits.received(received.clone()).unwrap();
    }
    let mut remote = trees.remote().get(&id.into()).unwrap().clone();
    (remote.shape, remote.meta_rev, remote.meta_edit_rev) = (shape, seq, seq);
    trees.set_remote_made_here(id.into(), remote);
    if from_the_log {
        commits.acknowledge(trees, seq);
        commits.received(received).unwrap();
    }
    assert!(commits.acknowledge(trees, seq));
    trees.settle(commits.aliases());
}

#[test]
fn moving_an_arrival_this_device_moved_out_back_in_makes_the_discarded_folder_the_users() {
    for (came_with, from_the_log, restart) in [
        (false, false, false),
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let case = format!("came with {came_with} from the log {from_the_log} restart {restart}");
        let extra = [("y", dir(ROOT, "y")), ("yc", file("y", "yc", 8))];
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&extra, |_, _, _| {}, |_, _| {});
        assert_eq!(sent, []);
        let (target, content) = match came_with {
            true => {
                another_device_places(&mut trees, &commits, "y", dir("x", "y"), 2);
                ("yc", 8)
            }
            false => {
                another_device_places(&mut trees, &commits, "m", file("x", "m", 7), 2);
                ("m", 7)
            }
        };
        let out = file(ROOT, target, content);
        let sent =
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, target, out.clone());
        assert_eq!(sent, [("move", target.to_owned())], "{case}");
        the_server_confirms_the_move(&mut trees, &mut commits, target, out, 3, from_the_log);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sent = this_device_sends_a_move(
            &mut trees,
            &mut commits,
            &mut ids,
            target,
            file("x", target, content),
        );
        assert_eq!(
            sent,
            [
                ("create dir", "f".to_owned()),
                ("move", "x".to_owned()),
                ("move", target.to_owned())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn renaming_an_arrival_this_device_moved_within_the_folder_changes_that_entry_alone() {
    for (came_with, confirmed, from_the_log, restart) in [
        (false, false, false, false),
        (false, true, false, true),
        (false, true, true, false),
        (true, false, false, true),
        (true, true, true, true),
        (true, true, false, false),
    ] {
        let case = format!(
            "came with {came_with} confirmed {confirmed} from the log {from_the_log} restart {restart}"
        );
        let extra = [("y", dir(ROOT, "y")), ("yc", file("y", "yc", 8))];
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&extra, |_, _, _| {}, |_, _| {});
        assert_eq!(sent, []);
        let (target, content) = match came_with {
            true => {
                another_device_places(&mut trees, &commits, "y", dir("x", "y"), 2);
                ("yc", 8)
            }
            false => {
                another_device_places(&mut trees, &commits, "m", file("x", "m", 7), 2);
                ("m", 7)
            }
        };
        let within = file("xd", target, content);
        if confirmed {
            let sent = this_device_sends_a_move(
                &mut trees,
                &mut commits,
                &mut ids,
                target,
                within.clone(),
            );
            assert_eq!(sent, [("move", target.to_owned())], "{case}");
            the_server_confirms_the_move(&mut trees, &mut commits, target, within, 3, from_the_log);
        } else {
            let node = trees.local().get(&target.into()).unwrap().clone();
            trees.set_local_changed(
                target.into(),
                LocalNode {
                    shape: within,
                    ..node
                },
            );
        }
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &target.into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, target.into())],
            "{case}: {:?}",
            local(&trees)
        );
        assert_eq!(
            (trees.local().get(&"x".into()).map(|x| &x.shape)),
            Some(&dir(ROOT, "server")),
            "{case}"
        );
        assert!(trees.local().get(&"f".into()).is_none(), "{case}");
    }
}

#[test]
fn renaming_an_entry_this_device_put_in_a_folder_before_another_device_brought_the_folder_in_changes_that_entry_alone()
 {
    for (from_the_log, restart) in [(false, true), (true, false)] {
        let case = format!("from the log {from_the_log} restart {restart}");
        let extra = [("y", dir(ROOT, "y")), ("g", file(ROOT, "g", 8))];
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &extra,
            |trees, commits, ids| {
                let put = file("y", "g", 8);
                let sent = this_device_sends_a_move(trees, commits, ids, "g", put.clone());
                assert_eq!(sent, [("move", "g".to_owned())]);
                the_server_confirms_the_move(trees, commits, "g", put, 1, from_the_log);
            },
            |_, _| {},
        );
        assert_eq!(sent, [], "{case}");
        another_device_places(&mut trees, &commits, "y", dir("x", "y"), 2);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"g".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "g".into())],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn renaming_an_entry_this_device_moved_into_an_outside_folder_another_device_then_brings_in_changes_that_entry_alone()
 {
    for (edited, from_the_log, restart) in [
        (false, false, false),
        (false, true, true),
        (true, true, false),
        (true, false, true),
    ] {
        let case = format!("edited {edited} from the log {from_the_log} restart {restart}");
        let extra = [("y", dir(ROOT, "y")), ("g", file(ROOT, "g", 8))];
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&extra, |_, _, _| {}, |_, _| {});
        assert_eq!(sent, [], "{case}");
        let put = file("y", "g", 8);
        let sent = this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "g", put.clone());
        assert_eq!(sent, [("move", "g".to_owned())], "{case}");
        the_server_confirms_the_move(&mut trees, &mut commits, "g", put, 3, from_the_log);
        another_device_places(&mut trees, &commits, "y", dir("x", "y"), 4);
        if edited {
            another_device_edits_in_place(&mut trees, &commits, "g", 5);
        }
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"g".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "g".into())],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn renaming_an_arrival_this_device_moved_into_a_folder_it_then_brought_in_changes_the_folder() {
    for (from_the_log, restart) in [(false, false), (true, true)] {
        let case = format!("from the log {from_the_log} restart {restart}");
        let extra = [("y", dir(ROOT, "y"))];
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&extra, |_, _, _| {}, |_, _| {});
        assert_eq!(sent, [], "{case}");
        another_device_places(&mut trees, &commits, "m", file("x", "m", 7), 2);
        let put = file("y", "m", 7);
        let sent = this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", put.clone());
        assert_eq!(sent, [("move", "m".to_owned())], "{case}");
        the_server_confirms_the_move(&mut trees, &mut commits, "m", put, 3, from_the_log);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sent = this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "y", dir("x", "y"));
        assert_eq!(
            sent,
            [
                ("create dir", "f".to_owned()),
                ("move", "x".to_owned()),
                ("move", "y".to_owned())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SameMove {
    NotSent,
    AnotherDeviceFirst,
    ThisDeviceFirst,
}

#[test]
fn renaming_an_entry_the_user_moved_in_that_another_device_moved_to_the_same_place_changes_the_folder()
 {
    use SameMove::*;
    for (order, from_the_log, restart) in [
        (NotSent, false, false),
        (NotSent, false, true),
        (AnotherDeviceFirst, false, false),
        (AnotherDeviceFirst, true, true),
        (ThisDeviceFirst, false, true),
        (ThisDeviceFirst, true, false),
    ] {
        let case = format!("{order:?} from the log {from_the_log} restart {restart}");
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                let node = trees.local().get(&"m".into()).unwrap().clone();
                trees.set_local_changed(
                    "m".into(),
                    LocalNode {
                        shape: moved_in.clone(),
                        ..node
                    },
                );
                trees.settle(commits.aliases());
                if order != NotSent {
                    assert_eq!(ops(commits, trees, ids), [("move", "m".to_owned())]);
                }
            },
            |trees, commits| match order {
                NotSent => another_device_places(trees, commits, "m", moved_in.clone(), 2),
                AnotherDeviceFirst => {
                    another_device_places(trees, commits, "m", moved_in.clone(), 2);
                    the_server_confirms_the_move(
                        trees,
                        commits,
                        "m",
                        moved_in.clone(),
                        3,
                        from_the_log,
                    );
                }
                ThisDeviceFirst => {
                    the_server_confirms_the_move(
                        trees,
                        commits,
                        "m",
                        moved_in.clone(),
                        2,
                        from_the_log,
                    );
                    another_device_places(trees, commits, "m", moved_in.clone(), 3);
                }
            },
        );
        assert_eq!(sent, [], "{case}");
        assert_eq!(
            trees.state().discard_members.get(&"m".into()),
            Some(&"x".into()),
            "{case}"
        );
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "f".into()),
                ("move", "x".into()),
                (sends, "m".into())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

fn the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
    brought: &str,
    before: &[(&str, &str, u64)],
    seq: u64,
    after: &[(&str, &str, u64)],
) -> Vec<(&'static str, String)> {
    let shape = |id: &str, parent: &str| match id {
        "m" => file(parent, "m", 7),
        _ => dir(parent, id),
    };
    let moved = dir("x", brought);
    let mut results = Vec::new();
    for (from_the_log, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("y", dir(ROOT, "y")), ("z", dir(ROOT, "z"))],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, brought, moved.clone()),
                    [("move", brought.to_owned())]
                );
            },
            |trees, commits| {
                for (id, parent, rev) in before {
                    another_device_places(trees, commits, id, shape(id, parent), *rev);
                }
                the_server_confirms_the_move(
                    trees,
                    commits,
                    brought,
                    moved.clone(),
                    seq,
                    from_the_log,
                );
                for (id, parent, rev) in after {
                    another_device_places(trees, commits, id, shape(id, parent), *rev);
                }
            },
        );
        assert_eq!(sent, []);
        assert_eq!(trees.remote().live(&brought.into()).unwrap().parent, "x");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        if let Some(earlier) = results.last() {
            assert_eq!(
                &sent, earlier,
                "from the log {from_the_log}, restart {restart}"
            );
        }
        results.push(sent);
    }
    results.remove(0)
}

const THE_FOLDER_BECOMES_THE_USERS: [(&str, &str); 3] =
    [("create dir", "f"), ("move", "x"), ("update", "m")];

fn sent_as_str<'a>(sent: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    sent.iter().map(|(op, id)| (*op, id.as_str())).collect()
}

#[test]
fn a_move_another_device_makes_to_a_folder_outside_the_discarded_folder_decides_nothing_once_this_device_brings_that_folder_in()
 {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "y",
        &[("m", "y", 2)],
        3,
        &[],
    );
    assert_eq!(sent_as_str(&sent), THE_FOLDER_BECOMES_THE_USERS);
}

#[test]
fn an_arrival_another_device_moves_out_goes_with_the_folder_it_left_for_once_this_device_brings_that_folder_in()
 {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "y",
        &[("m", "x", 2), ("m", "y", 3)],
        4,
        &[],
    );
    assert_eq!(sent_as_str(&sent), THE_FOLDER_BECOMES_THE_USERS);
}

#[test]
fn an_arrival_another_device_moves_out_and_back_in_stays_an_arrival_in_a_folder_this_device_brought_in()
 {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "y",
        &[("m", "x", 2), ("m", "y", 3), ("m", "x", 4)],
        5,
        &[("m", "y", 6)],
    );
    assert_eq!(sent_as_str(&sent), [("update", "m")]);
}

#[test]
fn an_entry_that_came_with_a_folder_another_device_moves_out_goes_with_the_folder_this_device_then_brings_in()
 {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "z",
        &[("y", "x", 2), ("m", "y", 3), ("y", "z", 4)],
        5,
        &[],
    );
    assert_eq!(sent_as_str(&sent), THE_FOLDER_BECOMES_THE_USERS);
}

#[test]
fn an_entry_another_device_moves_into_a_folder_this_device_brought_in_is_an_arrival() {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "y",
        &[],
        2,
        &[("m", "y", 3)],
    );
    assert_eq!(sent_as_str(&sent), [("update", "m")]);
}

#[test]
fn renaming_an_entry_this_device_brought_in_after_another_device_moves_it_out_unsynced_changes_the_folder()
 {
    for later in [None, Some(file("z", "m", 9)), Some(file(ROOT, "m", 7))] {
        let edited_there = later.as_ref().is_some_and(|shape| shape.parent == "z");
        let changes = [(false, UserChange::Rename), (true, UserChange::Edit)];
        for (from_the_log, change) in changes.map(|(log, change)| match edited_there {
            true => (log, UserChange::Rename),
            false => (log, change),
        }) {
            let case = format!("later {later:?} from the log {from_the_log} {change:?}");
            let moved = file("x", "m", 7);
            let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
                &[("z", dir(ROOT, "z"))],
                |trees, commits, ids| {
                    assert_eq!(
                        this_device_sends_a_move(trees, commits, ids, "m", moved.clone()),
                        [("move", "m".to_owned())]
                    );
                },
                |trees, commits| {
                    the_server_confirms_the_move(
                        trees,
                        commits,
                        "m",
                        moved.clone(),
                        3,
                        from_the_log,
                    );
                },
            );
            assert_eq!(sent, [], "{case}");
            let m: NodeId = "m".into();
            assert_eq!(trees.synced().get(&m).unwrap().parent, "x", "{case}");
            for (rev, shape) in [(4, file("z", "m", 7))]
                .into_iter()
                .chain(later.clone().map(|shape| (5, shape)))
            {
                let mut record = trees.remote().get(&m).unwrap().clone();
                (record.shape, record.meta_rev, record.meta_edit_rev) = (shape, rev, rev);
                trees.set_remote(m.clone(), record);
                trees.settle(commits.aliases());
            }
            let sends = change_as_the_user(&mut trees, &m, change);
            let mut sent = quiet(&mut trees, &mut commits, &mut ids);
            sent.sort();
            let mut expected = vec![
                ("create dir", "f".into()),
                ("move", "x".into()),
                (sends, "m".into()),
            ];
            expected.sort();
            assert_eq!(sent, expected, "{case}: {:?}", local(&trees));
        }
    }
}

#[test]
fn an_entry_the_user_moves_in_before_the_answer_that_another_device_moves_in_before_the_build_is_not_one_of_its_entries()
 {
    let (trees, _, _, sent) = a_server_folder_this_device_discards(
        &[],
        |trees, commits, _| {
            let node = trees.local().get(&"m".into()).unwrap().clone();
            let shape = file("x", "m", 7);
            trees.set_local_changed("m".into(), LocalNode { shape, ..node });
            trees.settle(commits.aliases());
        },
        |trees, commits| {
            let mut record = trees.remote().get(&"m".into()).unwrap().clone();
            (record.shape, record.meta_rev, record.meta_edit_rev) = (file("xd", "m", 7), 2, 2);
            trees.set_remote("m".into(), record);
            trees.settle(commits.aliases());
        },
    );
    assert_eq!(sent, [("move", "m".to_owned())]);
    assert!(!trees.state().discard_members.contains_key(&"m".into()));
}

#[test]
fn an_entry_another_device_moves_between_outside_folders_comes_with_the_folder_this_device_brings_in()
 {
    for from_the_log in [false, true] {
        let moved = dir("x", "q");
        let (trees, _, _, sent) = a_server_folder_this_device_discards(
            &[
                ("q", dir(ROOT, "q")),
                ("m", file("q", "m", 7)),
                ("z", dir(ROOT, "z")),
            ],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "q", moved.clone()),
                    [("move", "q".to_owned())]
                );
            },
            |trees, commits| {
                assert!(
                    commits.questions()[0]
                        .frozen
                        .as_ref()
                        .is_some_and(|f| f.whole)
                );
                another_device_places(trees, commits, "m", file("z", "m", 7), 2);
                another_device_places(trees, commits, "m", file("q", "m", 7), 3);
                the_server_confirms_the_move(trees, commits, "q", moved.clone(), 4, from_the_log);
            },
        );
        assert_eq!(sent, [], "from the log {from_the_log}");
        assert_eq!(
            trees.state().discard_members.get(&"m".into()),
            Some(&"x".into()),
            "from the log {from_the_log}"
        );
    }
}

#[test]
fn an_arrival_another_device_moves_into_a_folder_this_device_brought_in_stays_an_arrival() {
    let sent = the_user_edits_an_entry_after_another_device_places_entries_around_this_device_bringing_a_folder_in(
        "y",
        &[("m", "x", 2)],
        3,
        &[("m", "y", 4)],
    );
    assert_eq!(sent_as_str(&sent), [("update", "m")]);
}

#[test]
fn editing_an_entry_another_device_moved_out_before_the_answer_that_this_device_still_has_in_the_folder_changes_that_entry_alone()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, _| {
                let mut record = trees.remote().get(&"c".into()).unwrap().clone();
                (record.shape, record.meta_rev, record.meta_edit_rev) =
                    (file(ROOT, "child", 3), 2, 2);
                trees.set_remote("c".into(), record);
                trees.settle(commits.aliases());
            },
            |_, _| {},
        );
        assert_eq!(sent, [], "restart {restart}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"c".into(), UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "c".into())],
            "restart {restart}: {:?}",
            local(&trees)
        );
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            dir(ROOT, "server"),
            "restart {restart}"
        );
    }
}

#[test]
fn renaming_an_entry_the_user_moved_in_whose_old_folder_another_device_then_brings_in_changes_the_folder()
 {
    for (downloaded, from_the_log, restart) in [
        (false, false, true),
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let case = format!("downloaded {downloaded} from the log {from_the_log} restart {restart}");
        let moved_in = file("x", "e", 4);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("a", dir(ROOT, "a")), ("e", file("a", "e", 4))],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "e", moved_in.clone()),
                    [("move", "e".to_owned())]
                );
            },
            |trees, commits| {
                if downloaded {
                    another_device_places(trees, commits, "a", dir("x", "a"), 2);
                } else {
                    let mut record = trees.remote().get(&"a".into()).unwrap().clone();
                    (record.shape, record.meta_rev, record.meta_edit_rev) = (dir("x", "a"), 2, 2);
                    trees.set_remote("a".into(), record);
                    trees.settle(commits.aliases());
                }
                the_server_confirms_the_move(
                    trees,
                    commits,
                    "e",
                    moved_in.clone(),
                    3,
                    from_the_log,
                );
            },
        );
        assert_eq!(sent, [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"e".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "f".into()),
                ("move", "x".into()),
                (sends, "e".into())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn changing_an_entry_another_device_moves_out_between_the_answer_and_the_build_changes_the_folder()
{
    for (change, restart) in [(UserChange::Edit, false), (UserChange::Rename, true)] {
        let case = format!("{change:?} restart {restart}");
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |_, _, _| {},
            |trees, commits| {
                let mut record = trees.remote().get(&"c".into()).unwrap().clone();
                (record.shape, record.meta_rev, record.meta_edit_rev) =
                    (file(ROOT, "child", 3), 2, 2);
                trees.set_remote("c".into(), record);
                trees.settle(commits.aliases());
            },
        );
        assert_eq!(sent, [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"c".into(), change);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{case}: {sent:?}"
        );
        assert!(sent.contains(&(sends, "c".into())), "{case}: {sent:?}");
    }
}

#[test]
fn deleting_an_entry_another_device_brings_in_with_its_folder_in_the_answer_window_after_this_device_moves_it_within_changes_that_entry_alone()
 {
    for (moved_in_window, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("moved in window {moved_in_window} restart {restart}");
        let moved = file("xd", "e", 8);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("a", dir(ROOT, "a")), ("e", file("a", "e", 8))],
            |_, _, _| {},
            |trees, commits| {
                another_device_places(trees, commits, "a", dir("x", "a"), 2);
                if moved_in_window {
                    let mut record = trees.remote().get(&"e".into()).unwrap().clone();
                    (record.shape, record.meta_rev, record.meta_edit_rev) = (moved.clone(), 3, 3);
                    trees.set_remote_made_here("e".into(), record);
                    let node = trees.local().get(&"e".into()).unwrap().clone();
                    trees.set_local(
                        "e".into(),
                        LocalNode {
                            shape: moved.clone(),
                            ..node
                        },
                    );
                    trees.set_synced("e".into(), Some(moved.clone()));
                    trees.settle(commits.aliases());
                }
            },
        );
        assert_eq!(sent, [], "{case}");
        if !moved_in_window {
            let sent =
                this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "e", moved.clone());
            assert_eq!(sent, [("move", "e".to_owned())], "{case}");
            the_server_confirms_the_move(&mut trees, &mut commits, "e", moved.clone(), 3, false);
        }
        assert!(
            !trees.state().discard_members.contains_key(&"e".into()),
            "{case}"
        );
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        trees.listed(&["x".into(), "xd".into()], Pending::default());
        trees.remove_local(&"e".into());
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [("delete", "e".to_owned())],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn renaming_an_arrival_this_device_moves_within_the_folder_while_another_device_moves_it_out_and_back_changes_that_entry_alone()
 {
    for (from_the_log, restart) in [(false, true), (true, false)] {
        let case = format!("from the log {from_the_log} restart {restart}");
        let within = file("xd", "m", 7);
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&[], |_, _, _| {}, |_, _| {});
        assert_eq!(sent, [], "{case}");
        another_device_places(&mut trees, &commits, "m", file("x", "m", 7), 2);
        assert_eq!(
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", within.clone()),
            [("move", "m".to_owned())],
            "{case}"
        );
        for (parent, rev) in [(ROOT, 3), ("x", 4)] {
            let mut record = trees.remote().get(&"m".into()).unwrap().clone();
            (record.shape, record.meta_rev, record.meta_edit_rev) =
                (file(parent, "m", 7), rev, rev);
            trees.set_remote("m".into(), record);
            trees.settle(commits.aliases());
        }
        the_server_confirms_the_move(&mut trees, &mut commits, "m", within, 5, from_the_log);
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "m".into())],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn renaming_an_entry_the_user_moved_in_that_another_device_moved_elsewhere_in_the_folder_first_changes_the_folder()
 {
    for (from_the_log, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("from the log {from_the_log} restart {restart}");
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "m", moved_in.clone()),
                    [("move", "m".to_owned())]
                );
            },
            |trees, commits| {
                let mut record = trees.remote().get(&"m".into()).unwrap().clone();
                (record.shape, record.meta_rev, record.meta_edit_rev) = (file("xd", "m", 7), 2, 2);
                trees.set_remote("m".into(), record);
                trees.settle(commits.aliases());
                the_server_confirms_the_move(
                    trees,
                    commits,
                    "m",
                    moved_in.clone(),
                    3,
                    from_the_log,
                );
            },
        );
        assert_eq!(sent, [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Rename);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [
                ("create dir", "f".into()),
                ("move", "x".into()),
                (sends, "m".into())
            ],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn an_entry_a_server_commit_moves_within_a_folder_it_brings_in_is_an_arrival_in_either_record_order()
 {
    for outer_first in [false, true] {
        let (sent, mut trees, mut commits, mut ids) =
            answered_discard_of_a_server_folder_moved_in(&[], &[], Some(&[]), false, false);
        assert_eq!(sent, []);
        let (outer, inner) = another_devices_folder_with_a_child(&mut trees, &commits);
        let side: NodeId = "side".into();
        let entry: NodeId = "batch-file".into();
        for (id, shape, ino) in [
            (&side, dir("outer", "side"), 198),
            (&entry, file("inner", "note", 5), 199),
        ] {
            trees.set_remote(id.clone(), remote_node(shape.clone()));
            trees.set_local(id.clone(), local_node(shape.clone(), ino));
            trees.set_synced(id.clone(), Some(shape));
        }
        trees.settle(commits.aliases());
        assert_eq!(trees.remote().live(&inner).unwrap().parent, outer);
        let mut moves = [
            (&entry, file("side", "note", 5)),
            (&outer, dir("s", "outer")),
        ];
        if outer_first {
            moves.reverse();
        }
        another_device_commits_moves(&mut trees, &commits, &moves, 2);
        change_as_the_user(&mut trees, &entry, UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [("update", "batch-file".into())],
            "outer first {outer_first}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn editing_an_entry_this_device_moved_in_that_another_device_moves_out_before_the_answer_keeps_the_folder()
 {
    for (from_the_log, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("from the log {from_the_log} restart {restart}");
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "m", moved_in.clone()),
                    [("move", "m".into())]
                );
                the_server_confirms_the_move(
                    trees,
                    commits,
                    "m",
                    moved_in.clone(),
                    2,
                    from_the_log,
                );
                let mut record = trees.remote().get(&"m".into()).unwrap().clone();
                (record.shape, record.meta_rev, record.meta_edit_rev) = (file(ROOT, "m", 7), 3, 3);
                trees.set_remote("m".into(), record);
                trees.settle(commits.aliases());
            },
            |trees, _| {
                assert_eq!(trees.synced().get(&"m".into()).unwrap().parent, "x");
                assert_eq!(trees.remote().live(&"m".into()).unwrap().parent, ROOT);
            },
        );
        assert_eq!(sent, [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        sent.sort();
        assert_eq!(
            sent,
            [
                ("create dir", "f".into()),
                ("move", "x".into()),
                ("update", "m".into())
            ],
            "{case}: {:?}",
            local(&trees)
        );
        assert_eq!(
            trees.local().get(&"f".into()).map(|f| &f.shape),
            Some(&dir(ROOT, "new")),
            "{case}"
        );
    }
}

#[test]
fn a_store_from_before_stays_were_recorded_counts_an_entry_another_device_brought_in_as_this_devices()
 {
    for before_the_upgrade in [false, true] {
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&[], |_, _, _| {}, |_, _| {});
        assert_eq!(sent, []);
        another_device_places(&mut trees, &commits, "m", file("x", "m", 7), 2);
        let mut state = trees.state();
        if before_the_upgrade {
            state.discard_stays.clear();
            state.first_link_open.clear();
        }
        trees = restarted_from(&trees, &[], state);
        commits = Commits::from_state(commits.state()).unwrap();
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Rename);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        match before_the_upgrade {
            true => assert_eq!(
                sent,
                [
                    ("create dir", "f".into()),
                    ("move", "x".into()),
                    (sends, "m".into())
                ],
                "{:?}",
                local(&trees)
            ),
            false => assert_eq!(sent, [(sends, "m".into())], "{:?}", local(&trees)),
        }
    }
}

#[test]
fn a_first_link_that_holds_nothing_and_the_sync_after_it_record_no_stays() {
    let mut remote: Vec<(String, Shape)> = Vec::new();
    for a in 0..10 {
        let ida = format!("a{a}");
        remote.push((ida.clone(), dir(ROOT, &ida)));
        for b in 0..10 {
            let idb = format!("{ida}b{b}");
            remote.push((idb.clone(), dir(&ida, &idb)));
            for c in 0..10 {
                let idc = format!("{idb}c{c}");
                remote.push((idc.clone(), dir(&idb, &idc)));
                for f in 0..20 {
                    let idf = format!("{idc}f{f}");
                    remote.push((idf.clone(), file(&idc, &idf, 1)));
                }
            }
        }
    }
    let refs: Vec<(&str, Shape)> = remote
        .iter()
        .map(|(id, s)| (id.as_str(), s.clone()))
        .collect();
    let mut trees = trees(&[], &refs);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(commits.questions().is_empty());
    for n in 0..1000 {
        let id = format!("new{n}");
        let created = remote_node(file("a0b0c0", &id, 2));
        trees.apply_batch(|trees| trees.set_remote(id.as_str().into(), created));
    }
    let mut moved = trees.remote().get(&"a1".into()).unwrap().clone();
    (moved.shape, moved.meta_rev) = (dir("a2b2c2", "a1"), 2);
    trees.set_remote("a1".into(), moved);
    trees.settle(commits.aliases());
    let state = trees.state();
    assert!(state.first_link_open.is_empty());
    assert_eq!(state.discard_stays.len(), 0);
}

#[test]
fn moves_another_device_makes_without_a_first_link_record_no_stays() {
    let mut both = vec![
        ("a".to_owned(), dir(ROOT, "a")),
        ("b".to_owned(), dir(ROOT, "b")),
        ("big".to_owned(), dir("a", "big")),
    ];
    for i in 0..200 {
        both.push((format!("e{i}"), file("big", &format!("e{i}"), 5)));
    }
    let both: Vec<(&str, Shape)> = both
        .iter()
        .map(|(id, shape)| (id.as_str(), shape.clone()))
        .collect();
    let mut trees = trees(&both, &both);
    let commits = Commits::default();
    trees.settle(commits.aliases());
    for (rev, parent) in [(2, "b"), (3, "a"), (4, "b")] {
        another_device_places(&mut trees, &commits, "big", dir(parent, "big"), rev);
    }
    assert_eq!(trees.state().discard_stays.len(), 0);
}

#[test]
fn a_folder_asked_about_again_after_its_discard_records_stays_from_the_new_question_with_none() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(!trees.state().first_link_open.is_empty());
    assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.state().first_link_open.is_empty());
    trees.set_local("l-later".into(), local_node(file("l-new", "later", 6), 99));
    trees.settle(commits.aliases());
    assert!(trees.reclaimed(&"l-new".into()));
    let mut effects = Effects::default();
    let mut removed = Vec::new();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        if matches!(
            record.effect,
            Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
        ) {
            removed.push(record.node.clone());
        }
        let identity = FileIdentity {
            ino: 100 + removed.len() as u128,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(identity),
            )
            .unwrap();
        trees.settle(commits.aliases());
    }
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert_ne!(again.id, asked.id);
    let state = trees.state();
    assert_eq!(state.first_link_open.get(&"l-new".into()), Some(&true));
    assert!(state.discard_stays.is_empty());
}

#[test]
fn stays_end_with_the_last_first_link_question_and_a_later_question_starts_with_none() {
    for restart in [false, true] {
        let both = [
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 3)),
            ("m", file(ROOT, "m", 7)),
        ];
        let mut local = vec![("f", dir(ROOT, "new"))];
        local.extend(both.iter().cloned());
        let mut trees = trees(&local, &both);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let moved_in = file("x", "m", 7);
        assert_eq!(
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", moved_in.clone()),
            [("move", "m".into())]
        );
        the_server_confirms_the_move(&mut trees, &mut commits, "m", moved_in, 2, false);
        let open = trees.state();
        assert_eq!(
            open.discard_stays,
            [("m".into(), "x".into())].into(),
            "restart {restart}"
        );
        assert_eq!(open.first_link_open, [("f".into(), true)].into());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
            commits.hold(&mut trees);
            let reopened = trees.state();
            assert_eq!(reopened.discard_stays, open.discard_stays);
            assert_eq!(reopened.first_link_open, open.first_link_open);
        }
        assert!(commits.decide(&mut trees, &asked.id, Answer::Publish));
        quiet(&mut trees, &mut commits, &mut ids);
        assert!(commits.questions().is_empty(), "restart {restart}");
        let closed = trees.state();
        assert!(closed.first_link_open.is_empty(), "restart {restart}");
        assert!(closed.discard_stays.is_empty(), "restart {restart}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
            commits.hold(&mut trees);
        }
        another_device_places(&mut trees, &commits, "m", file(ROOT, "m", 7), 3);
        assert!(trees.state().discard_stays.is_empty(), "restart {restart}");
        trees.set_local("g".into(), local_node(dir(ROOT, "other"), 400));
        trees.settle(commits.aliases());
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let later = trees.state();
        assert_eq!(
            later.first_link_open.get(&"g".into()),
            Some(&true),
            "restart {restart}"
        );
        assert!(later.discard_stays.is_empty(), "restart {restart}");
    }
}

#[test]
fn a_store_upgraded_while_a_first_link_question_is_open_counts_no_entry_as_another_devices() {
    for upgraded in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, _| {
                if upgraded {
                    let mut state = trees.state();
                    state.discard_stays.clear();
                    state.first_link_open.clear();
                    *trees = restarted_from(trees, &[], state);
                    *commits = Commits::from_state(commits.state()).unwrap();
                    commits.hold(trees);
                }
            },
            |trees, commits| another_device_places(trees, commits, "m", file("x", "m", 7), 2),
        );
        assert_eq!(sent, [], "upgraded {upgraded}");
        let state = trees.state();
        assert_eq!(state.first_link_open.is_empty(), upgraded);
        assert!(state.discard_stays.is_empty(), "upgraded {upgraded}");
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        match upgraded {
            true => assert!(sent.contains(&("create dir", "f".into())), "{sent:?}"),
            false => assert_eq!(sent, [(sends, "m".into())]),
        }
    }
}

#[test]
fn an_entry_this_device_creates_in_the_folder_before_the_answer_counts_as_this_devices_once_acknowledged()
 {
    for (from_the_log, crash) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = format!("from the log {from_the_log} crash {crash}");
        let created = file("xd", "n", 5);
        let confirm = |trees: &mut Trees, commits: &mut Commits| {
            let result = OpResult {
                code: op_result::Code::Applied as i32,
                node_id: "n".into(),
                ..Default::default()
            };
            (commits.received(CommitResult {
                seq: 2,
                results: vec![result],
                ..Default::default()
            }))
            .unwrap();
            let mut remote = remote_node(created.clone());
            (remote.meta_rev, remote.meta_edit_rev) = (2, 2);
            match from_the_log {
                true => trees.set_remote("n".into(), remote),
                false => trees.set_remote_made_here("n".into(), remote),
            }
            assert!(commits.acknowledge(trees, 2));
            trees.settle(commits.aliases());
        };
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                trees.set_local_changed("n".into(), local_node(created.clone(), 500));
                trees.settle(commits.aliases());
                assert_eq!(ops(commits, trees, ids), [("create file", "n".to_owned())]);
            },
            |trees, commits| {
                let before = (trees.clone(), commits.state());
                confirm(trees, commits);
                if crash {
                    *trees = restarted(&before.0);
                    *commits = Commits::from_state(before.1).unwrap();
                    commits.hold(trees);
                    assert!(trees.state().discard_stays.is_empty());
                    assert!(commits.in_flight().is_some());
                    confirm(trees, commits);
                }
            },
        );
        assert_eq!(sent, [], "{case}");
        assert!(
            (trees.state().discard_stays).contains(&("n".into(), "x".into())),
            "{case}"
        );
        change_as_the_user(&mut trees, &"n".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{case}: {sent:?}"
        );
    }
}

fn another_device_creates_files_in(trees: &mut Trees, commits: &Commits, folder: &str, count: u64) {
    for at in 0..count {
        let (id, name) = (format!("created-{at}"), format!("created-{at}.txt"));
        another_device_places(trees, commits, &id, file(folder, &name, 5), 2);
    }
}

#[test]
fn another_devices_creates_in_an_unrelated_folder_record_no_stays_while_first_link_state_is_open() {
    let both = [("x", dir(ROOT, "server")), ("u", dir(ROOT, "unrelated"))];
    let mut local = vec![("f", dir(ROOT, "new"))];
    local.extend(both.iter().cloned());
    let mut trees = trees(&local, &both);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(question(&commits).answer.is_none());
    assert_eq!(trees.state().first_link_open, [("f".into(), true)].into());
    another_device_creates_files_in(&mut trees, &commits, "u", 1000);
    assert!(trees.state().discard_stays.is_empty());

    let (sent, mut trees, mut commits, mut ids) =
        answered_discard_of_a_server_folder_moved_in(&[], &[], None, false, false);
    assert_eq!(sent, []);
    trees.set_local("hk".into(), local_node(file("h", "keep.log", 7), 177));
    let kept = Some(RuleFile::parse(b"keep.log\n"));
    trees.set_rules(&[] as &[&str], Source::Koiignore, kept);
    trees.settle(commits.aliases());
    while let Some(open) = (commits.questions().iter()).find(|asked| asked.answer.is_none()) {
        let id = open.id.clone();
        assert!(commits.decide(&mut trees, &id, Answer::Discard));
    }
    assert_eq!(quiet(&mut trees, &mut commits, &mut ids), []);
    assert!(commits.questions().is_empty());
    assert!(!trees.state().first_link_open.is_empty());
    assert!(!trees.state().discard_members.is_empty());
    another_device_places(&mut trees, &commits, "u", dir(ROOT, "unrelated"), 2);
    another_device_creates_files_in(&mut trees, &commits, "u", 1000);
    assert!(trees.state().discard_stays.is_empty());
}

#[test]
fn an_entry_this_device_moved_in_counts_as_another_devices_once_another_device_brings_it_in_again()
{
    for (moved, edited, restart, moved_within) in [
        ("m", "m", false, false),
        ("m", "m", true, false),
        ("g", "gm", false, false),
        ("g", "gm", true, false),
        ("g", "gm", false, true),
    ] {
        let case = format!("{moved} restart {restart} moved within {moved_within}");
        let into_x = match moved {
            "m" => file("x", "m", 7),
            _ => dir("x", "g"),
        };
        let back = match moved {
            "m" => file(ROOT, "m", 7),
            _ => dir(ROOT, "g"),
        };
        let extra = [
            ("g", dir(ROOT, "g")),
            ("gm", file("g", "gm", 8)),
            ("gh", dir("g", "h")),
        ];
        let recorded: [(NodeId, NodeId); 2] =
            [(moved.into(), "x".into()), (edited.into(), "x".into())];
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &extra,
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, moved, into_x.clone()),
                    [("move", moved.into())]
                );
                the_server_confirms_the_move(trees, commits, moved, into_x.clone(), 2, false);
                let stays = trees.state().discard_stays;
                assert!(
                    recorded.iter().all(|stay| stays.contains(stay)),
                    "{case}: {stays:?}"
                );
                if restart {
                    *trees = restarted(trees);
                    *commits = Commits::from_state(commits.state()).unwrap();
                    commits.hold(trees);
                    assert_eq!(trees.state().discard_stays, stays, "{case}");
                }
                another_device_places(trees, commits, moved, back.clone(), 3);
                assert_eq!(trees.state().discard_stays, stays, "{case}");
            },
            |trees, commits| {
                let (id, child): (NodeId, NodeId) = (moved.into(), edited.into());
                match moved_within {
                    true => another_device_commits_moves(
                        trees,
                        commits,
                        &[(&id, into_x.clone()), (&child, file("gh", "gm", 8))],
                        4,
                    ),
                    false => another_device_places(trees, commits, moved, into_x.clone(), 4),
                }
                if restart {
                    *trees = restarted(trees);
                    *commits = Commits::from_state(commits.state()).unwrap();
                    commits.hold(trees);
                }
                let left = trees.state().discard_stays;
                assert!(
                    recorded.iter().all(|stay| !left.contains(stay)),
                    "{case}: {left:?}"
                );
            },
        );
        assert_eq!(sent, [], "{case}");
        let sends = change_as_the_user(&mut trees, &edited.into(), UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, edited.to_string())],
            "{case}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn an_entry_another_device_creates_in_the_folder_after_the_answer_counts_as_another_devices_with_no_stay_recorded()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |_, _, _| {},
            |trees, commits| another_device_places(trees, commits, "n", file("x", "n", 5), 2),
        );
        assert_eq!(sent, [], "{restart}");
        assert!(folders_of(&trees.state().discard_stays, &"n".into()).is_empty());
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
        }
        let sends = change_as_the_user(&mut trees, &"n".into(), UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "n".to_string())],
            "{restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn a_folder_this_device_moves_in_that_the_server_renames_counts_as_this_devices_with_its_child() {
    for (from_the_log, edited) in [(false, "g"), (false, "gm"), (true, "g"), (true, "gm")] {
        let case = format!("from the log {from_the_log} {edited}");
        let extra = [("g", dir(ROOT, "g")), ("gm", file("g", "gm", 8))];
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &extra,
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "g", dir("x", "g")),
                    [("move", "g".into())]
                );
            },
            |trees, commits| {
                let result = OpResult {
                    code: op_result::Code::AppliedRenamed as i32,
                    node_id: "g".into(),
                    name: "g 2".into(),
                    ..Default::default()
                };
                (commits.received(CommitResult {
                    seq: 2,
                    results: vec![result],
                    ..Default::default()
                }))
                .unwrap();
                let mut remote = trees.remote().get(&"g".into()).unwrap().clone();
                (remote.shape, remote.meta_rev, remote.meta_edit_rev) = (dir("x", "g 2"), 2, 2);
                match from_the_log {
                    true => trees.set_remote("g".into(), remote),
                    false => trees.set_remote_made_here("g".into(), remote),
                }
                assert!(commits.acknowledge(trees, 2));
                trees.settle(commits.aliases());
                let stays = trees.state().discard_stays;
                assert!(
                    stays.contains(&("g".into(), "x".into())),
                    "{case}: {stays:?}"
                );
                assert!(
                    stays.contains(&("gm".into(), "x".into())),
                    "{case}: {stays:?}"
                );
            },
        );
        assert_eq!(sent, [], "{case}");
        assert_eq!(
            trees.synced().get(&"g".into()).unwrap().name,
            "g 2",
            "{case}"
        );
        let change = match edited {
            "g" => UserChange::Rename,
            _ => UserChange::Edit,
        };
        change_as_the_user(&mut trees, &edited.into(), change);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{case}: {sent:?} {:?}",
            local(&trees)
        );
    }
}

#[test]
fn a_restore_this_device_sends_into_the_folder_counts_as_this_devices_once_acknowledged() {
    const RESTORED: &str = "01a1eab7-d941-7d6a-9e0d-576a24408e70";
    for from_the_log in [false, true] {
        let restored = file("x", "t", 11);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                let mut trashed = remote_node(restored.clone());
                trashed.trashed = true;
                trees.set_remote(RESTORED.into(), trashed);
                trees.settle(commits.aliases());
                let intent = RestoreIntent::from_trash(RESTORED.into(), None, None).unwrap();
                assert!(commits.restore("restore t".into(), intent));
                assert_eq!(ops(commits, trees, ids), [("restore", RESTORED.to_owned())]);
            },
            |trees, commits| {
                let result = OpResult {
                    code: op_result::Code::Applied as i32,
                    node_id: RESTORED.into(),
                    ..Default::default()
                };
                (commits.received(CommitResult {
                    seq: 2,
                    results: vec![result],
                    ..Default::default()
                }))
                .unwrap();
                let mut remote = remote_node(restored.clone());
                (remote.meta_rev, remote.meta_edit_rev) = (2, 2);
                match from_the_log {
                    true => trees.set_remote(RESTORED.into(), remote),
                    false => trees.set_remote_made_here(RESTORED.into(), remote),
                }
                assert!(commits.acknowledge(trees, 2));
                trees.settle(commits.aliases());
                let stays = trees.state().discard_stays;
                assert!(
                    stays.contains(&(RESTORED.into(), "x".into())),
                    "{from_the_log}: {stays:?}"
                );
                trees.set_local(RESTORED.into(), local_node(restored.clone(), 501));
                trees.set_synced(RESTORED.into(), Some(restored.clone()));
                trees.settle(commits.aliases());
            },
        );
        assert_eq!(sent, [], "{from_the_log}");
        change_as_the_user(&mut trees, &RESTORED.into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{from_the_log}: {sent:?} {:?}",
            local(&trees)
        );
    }
}

#[test]
fn a_store_upgraded_while_a_first_link_question_is_open_counts_no_entry_as_another_devices_after_a_second_question_opens()
 {
    for (restart, second_first) in [(false, false), (true, false), (false, true)] {
        let case = format!("restart {restart} second first {second_first}");
        let both = [
            ("x", dir(ROOT, "server")),
            ("xd", dir("x", "inner")),
            ("m", file(ROOT, "m", 7)),
        ];
        let mut local_tree = vec![("f", dir(ROOT, "new"))];
        local_tree.extend(both.iter().cloned());
        let mut trees = trees(&local_tree, &both);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let moved_in = file("x", "m", 7);
        assert_eq!(
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", moved_in.clone()),
            [("move", "m".into())]
        );
        the_server_confirms_the_move(&mut trees, &mut commits, "m", moved_in, 2, false);
        let mut record = trees.remote().get(&"m".into()).unwrap().clone();
        (record.shape, record.meta_rev, record.meta_edit_rev) = (file(ROOT, "m", 7), 3, 3);
        trees.set_remote("m".into(), record);
        trees.settle(commits.aliases());
        let mut state = trees.state();
        state.discard_stays.clear();
        state.first_link_open.clear();
        trees = restarted_from(&trees, &[], state);
        commits = Commits::from_state(commits.state()).unwrap();
        commits.hold(&mut trees);
        let ignored = Some(RuleFile::parse(b"ignored\n"));
        trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
        let mut x = trees.local().get(&"x".into()).unwrap().clone();
        (x.shape.parent, x.shape.name) = ("f".into(), "ignored".into());
        trees.set_local_changed("x".into(), x);
        trees.settle(commits.aliases());
        let open_second = |trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen| {
            trees.set_local("g".into(), local_node(dir(ROOT, "other"), 400));
            trees.settle(commits.aliases());
            commits.first_link(trees, ids, NOW, PriorSync::Shown);
            trees.settle(commits.aliases());
        };
        if second_first {
            open_second(&mut trees, &mut commits, &mut ids);
        }
        let asked = (commits.questions().iter())
            .find(|open| !open.shown.contains_key(&"g".into()))
            .unwrap()
            .id
            .clone();
        assert!(commits.decide(&mut trees, &asked, Answer::Discard));
        if !second_first {
            open_second(&mut trees, &mut commits, &mut ids);
        }
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{case}");
        let second = (commits.questions().iter())
            .find(|open| open.answer.is_none())
            .expect("the second question waits")
            .clone();
        assert!(second.shown.contains_key(&"g".into()), "{case}");
        assert!(trees.state().first_link_open.is_empty(), "{case}");
        let mut after = restarted(&trees);
        let mut after_commits = Commits::from_state(commits.state()).unwrap();
        after_commits.hold(&mut after);
        assert!(
            after_commits.decide(&mut after, &second.id, Answer::Discard),
            "{case}"
        );
        assert_eq!(ops(&mut after_commits, &mut after, &mut ids), [], "{case}");
        assert!(after_commits.questions().is_empty(), "{case}");
        assert!(!after.state().discard_members.is_empty(), "{case}");
        change_as_the_user(&mut after, &"xd".into(), UserChange::Rename);
        quiet(&mut after, &mut after_commits, &mut ids);
        let closed = after.state();
        assert!(closed.discard_members.is_empty() && closed.discard_answered.is_empty());
        assert!(closed.first_link_open.is_empty(), "{case}");
        after.set_local("h".into(), local_node(dir(ROOT, "later"), 401));
        after.settle(after_commits.aliases());
        after_commits.first_link(&mut after, &mut ids, NOW, PriorSync::Shown);
        after.settle(after_commits.aliases());
        let later = after.state().first_link_open;
        assert_eq!(later.get(&"h".into()), Some(&true), "{case}: {later:?}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
            commits.hold(&mut trees);
        }
        change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{case}: {sent:?} {:?}",
            local(&trees)
        );
    }
}

#[test]
fn closing_the_inner_of_two_nested_first_link_questions_keeps_this_devices_stays_until_the_outer_closes()
 {
    let both = [
        ("x", dir(ROOT, "server")),
        ("xd", dir("x", "inner")),
        ("m", file(ROOT, "m", 7)),
    ];
    let mut local_tree = vec![("f", dir(ROOT, "new"))];
    local_tree.extend(both.iter().cloned());
    let mut trees = trees(&local_tree, &both);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    trees.set_local("g".into(), local_node(dir("f", "other"), 400));
    trees.settle(commits.aliases());
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let moved_in = file("xd", "m", 7);
    assert_eq!(
        this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", moved_in.clone()),
        [("move", "m".into())]
    );
    the_server_confirms_the_move(&mut trees, &mut commits, "m", moved_in, 2, false);
    let stays: BTreeSet<(NodeId, NodeId)> =
        [("m".into(), "x".into()), ("m".into(), "xd".into())].into();
    assert_eq!(trees.state().discard_stays, stays);
    let inner = (commits.questions().iter())
        .find(|asked| asked.shown.contains_key(&"g".into()))
        .unwrap()
        .id
        .clone();
    assert!(commits.decide(&mut trees, &inner, Answer::Publish));
    quiet(&mut trees, &mut commits, &mut ids);
    assert_eq!(commits.questions().len(), 1);
    assert_eq!(trees.state().discard_stays, stays);
    let outer = question(&commits).id;
    assert!(commits.decide(&mut trees, &outer, Answer::Publish));
    quiet(&mut trees, &mut commits, &mut ids);
    assert!(commits.questions().is_empty());
    assert!(trees.state().discard_stays.is_empty());
    assert!(trees.state().first_link_open.is_empty());
}

#[test]
fn this_devices_stays_keep_a_folder_taken_again_after_its_own_question_closes_while_another_is_open()
 {
    for restart in [false, true] {
        let both = [
            ("x", dir(ROOT, "server")),
            ("c", file("x", "child", 3)),
            ("m", file(ROOT, "m", 7)),
        ];
        let mut local_tree = vec![("f", dir(ROOT, "new"))];
        local_tree.extend(both.iter().cloned());
        let mut trees = trees(&local_tree, &both);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let moved_in = file("x", "m", 7);
        assert_eq!(
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", moved_in.clone()),
            [("move", "m".into())]
        );
        the_server_confirms_the_move(&mut trees, &mut commits, "m", moved_in, 2, false);
        let take_into = |trees: &mut Trees, commits: &Commits, holder: &str| {
            let ignored = Some(RuleFile::parse(b"ignored\n"));
            trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
            let mut x = trees.local().get(&"x".into()).unwrap().clone();
            (x.shape.parent, x.shape.name) = (holder.into(), "ignored".into());
            trees.set_local_changed("x".into(), x);
            trees.settle(commits.aliases());
        };
        take_into(&mut trees, &commits, "f");
        let own = question(&commits).id;
        assert!(commits.decide(&mut trees, &own, Answer::Discard));
        trees.set_local("g".into(), local_node(dir(ROOT, "other"), 400));
        trees.settle(commits.aliases());
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.settle(commits.aliases());
        assert_eq!(quiet(&mut trees, &mut commits, &mut ids), [], "{restart}");
        let other = question(&commits);
        assert_eq!(other.shown.keys().collect::<Vec<_>>(), [&NodeId::from("g")]);
        let stays: BTreeSet<(NodeId, NodeId)> = [("m".into(), "x".into())].into();
        assert_eq!(trees.state().discard_stays, stays, "{restart}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
            commits.hold(&mut trees);
        }
        take_into(&mut trees, &commits, "g");
        assert!(commits.decide(&mut trees, &other.id, Answer::Discard));
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), [], "{restart}");
        assert_eq!(trees.state().discard_stays, stays, "{restart}");
        change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        sent.sort();
        assert_eq!(
            sent,
            [
                ("create dir", "g".into()),
                ("move", "x".into()),
                ("update", "m".into())
            ],
            "{restart}: {:?}",
            local(&trees)
        );
    }
}

#[test]
fn entries_this_device_puts_in_a_folder_it_creates_that_the_server_merges_into_another_count_as_this_devices()
 {
    for (edited, restart) in [("m", false), ("m", true), ("n", false), ("n", true)] {
        let case = format!("{edited} restart {restart}");
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                trees.set_local("a".into(), local_node(dir("x", "box"), 410));
                trees.set_local("n".into(), local_node(file("a", "n", 5), 411));
                let node = trees.local().get(&"m".into()).unwrap().clone();
                trees.set_local_changed(
                    "m".into(),
                    LocalNode {
                        shape: file("a", "m", 7),
                        ..node
                    },
                );
                trees.settle(commits.aliases());
                let mut sent = ops(commits, trees, ids);
                sent.sort();
                assert_eq!(
                    sent,
                    [
                        ("create dir", "a".into()),
                        ("create file", "n".into()),
                        ("move", "m".into())
                    ],
                    "{case}"
                );
            },
            |trees, commits| {
                trees.set_remote("b".into(), remote_node(dir("x", "box")));
                let results = (commits.in_flight().unwrap().commit.ops.iter())
                    .map(|op| match &op.op {
                        Some(op::Op::CreateDir(_)) => OpResult {
                            code: op_result::Code::MergedInto as i32,
                            node_id: "b".into(),
                            ..Default::default()
                        },
                        _ => OpResult {
                            code: op_result::Code::Applied as i32,
                            ..Default::default()
                        },
                    })
                    .collect();
                (commits.received(CommitResult {
                    seq: 2,
                    results,
                    ..Default::default()
                }))
                .unwrap();
                let mut moved = trees.remote().get(&"m".into()).unwrap().clone();
                (moved.shape, moved.meta_rev, moved.meta_edit_rev) = (file("b", "m", 7), 2, 2);
                let mut created = remote_node(file("b", "n", 5));
                (created.meta_rev, created.meta_edit_rev) = (2, 2);
                for (id, remote) in [("m", moved), ("n", created)] {
                    match restart {
                        true => trees.set_remote(id.into(), remote),
                        false => trees.set_remote_made_here(id.into(), remote),
                    }
                }
                assert!(commits.acknowledge(trees, 2));
                trees.settle(commits.aliases());
                let stays = trees.state().discard_stays;
                for id in ["m", "n"] {
                    assert!(
                        stays.contains(&(id.into(), "x".into())),
                        "{case}: {stays:?}"
                    );
                }
                assert_eq!(trees.local().get(&"m".into()).unwrap().shape.parent, "b");
            },
        );
        assert_eq!(sent, [], "{case}");
        if restart {
            trees = restarted(&trees);
            commits = Commits::from_state(commits.state()).unwrap();
            commits.hold(&mut trees);
        }
        change_as_the_user(&mut trees, &edited.into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert!(
            sent.contains(&("create dir", "f".into())),
            "{case}: {sent:?} {:?}",
            local(&trees)
        );
    }
}

fn another_device_moves_ahead_of_the_disk(trees: &mut Trees, commits: &Commits) {
    let mut record = trees.remote().get(&"e".into()).unwrap().clone();
    (record.shape, record.meta_rev, record.meta_edit_rev) = (file("x", "e", 5), 2, 2);
    trees.set_remote("e".into(), record);
    trees.settle(commits.aliases());
}

fn the_server_applies(
    trees: &mut Trees,
    commits: &mut Commits,
    applied: &[(&str, Option<Shape>)],
    seq: u64,
) {
    let results = (applied.iter())
        .map(|(id, _)| OpResult {
            code: op_result::Code::Applied as i32,
            node_id: (*id).into(),
            ..Default::default()
        })
        .collect();
    (commits.received(CommitResult {
        seq,
        results,
        ..Default::default()
    }))
    .unwrap();
    for (id, shape) in applied {
        let held = trees.remote().get(&(*id).into()).cloned();
        let remote = match shape {
            Some(shape) => RemoteNode {
                shape: shape.clone(),
                meta_rev: seq,
                content_rev: seq,
                ..held.unwrap_or_else(|| remote_node(shape.clone()))
            },
            None => RemoteNode {
                trashed: true,
                ..held.unwrap()
            },
        };
        trees.set_remote_made_here((*id).into(), remote);
    }
    assert!(commits.acknowledge(trees, seq));
    trees.settle(commits.aliases());
}

fn the_next_effect(trees: &mut Trees, commits: &Commits, ids: &mut IdGen) -> NodeId {
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    let planned = effects.plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE);
    let record = planned.first().cloned().unwrap();
    let identity = FileIdentity {
        ino: 99,
        birth_ns: Some(0),
        handle: None,
    };
    (effects.done(
        trees,
        commits.in_flight(),
        &record.effect_id,
        Some(identity),
    ))
    .unwrap();
    record.node
}

fn restarted_if(restart: bool, trees: &mut Trees, commits: &mut Commits) {
    if restart {
        *trees = restarted(trees);
        *commits = Commits::from_state(commits.state()).unwrap();
    }
}

fn assert_the_users(
    sent: &mut Vec<(&'static str, String)>,
    change: (&'static str, &str),
    trees: &Trees,
) {
    sent.sort();
    let mut expected = vec![
        ("create dir", "f".to_owned()),
        ("move", "x".to_owned()),
        (change.0, change.1.to_owned()),
    ];
    expected.sort();
    assert_eq!(*sent, expected, "{:?}", local(trees));
    assert_eq!(trees.local().get(&"x".into()).unwrap().shape.parent, "f");
}

#[test]
fn changing_an_entry_another_device_moved_in_whose_copy_the_user_deleted_where_it_was_before_the_answer_makes_the_discarded_folder_the_users()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("e", file(ROOT, "e", 5))],
            |trees, commits, _| {
                another_device_moves_ahead_of_the_disk(trees, commits);
                trees.remove_local(&"e".into());
                trees.settle(commits.aliases());
            },
            |_, _| {},
        );
        assert_eq!(sent, []);
        assert_eq!(the_next_effect(&mut trees, &commits, &mut ids), "e");
        restarted_if(restart, &mut trees, &mut commits);
        let sends = change_as_the_user(&mut trees, &"e".into(), UserChange::Edit);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_the_users(&mut sent, (sends, "e"), &trees);
    }
}

#[test]
fn changing_an_entry_another_device_moved_in_whose_copy_the_user_edited_where_it_was_before_the_answer_makes_the_discarded_folder_the_users()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("e", file(ROOT, "e", 5))],
            |trees, commits, ids| {
                another_device_moves_ahead_of_the_disk(trees, commits);
                let sends = change_as_the_user(trees, &"e".into(), UserChange::Edit);
                assert_eq!(ops(commits, trees, ids), [(sends, "e".into())]);
                let edited = Shape {
                    content: Some(content(9)),
                    ..file("x", "e", 5)
                };
                the_server_applies(trees, commits, &[("e", Some(edited))], 3);
            },
            |_, _| {},
        );
        assert_eq!(sent, []);
        assert_eq!(the_next_effect(&mut trees, &commits, &mut ids), "e");
        restarted_if(restart, &mut trees, &mut commits);
        let sends = change_as_the_user(&mut trees, &"e".into(), UserChange::Rename);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_the_users(&mut sent, (sends, "e"), &trees);
    }
}

#[test]
fn an_entry_the_user_deleted_in_the_folder_before_the_answer_is_not_one_of_its_entries_and_its_delete_syncs()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[("e", file(ROOT, "e", 5))],
            |trees, commits, ids| {
                another_device_places(trees, commits, "e", file("x", "e", 5), 2);
                trees.remove_local(&"e".into());
                trees.settle(commits.aliases());
                assert_eq!(ops(commits, trees, ids), [("delete", "e".into())]);
                the_server_applies(trees, commits, &[("e", None)], 3);
            },
            |_, _| {},
        );
        assert_eq!(sent, []);
        assert_eq!(trees.state().discard_members.get(&"e".into()), None);
        restarted_if(restart, &mut trees, &mut commits);
        assert_eq!(quiet(&mut trees, &mut commits, &mut ids), []);
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            dir(ROOT, "server")
        );
    }
}

#[test]
fn an_entry_the_user_moved_out_of_the_folder_before_the_answer_is_not_one_of_its_entries() {
    for (deleted, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let out = file(ROOT, "child", 3);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                assert_eq!(
                    this_device_sends_a_move(trees, commits, ids, "c", out.clone()),
                    [("move", "c".into())]
                );
                if deleted {
                    trees.remove_local(&"c".into());
                    trees.settle(commits.aliases());
                }
            },
            |trees, commits| the_server_applies(trees, commits, &[("c", Some(out.clone()))], 2),
        );
        let delete: &[(&str, String)] = &[("delete", "c".into())];
        assert_eq!(sent, if deleted { delete } else { &[] });
        assert_eq!(trees.state().discard_members.get(&"c".into()), None);
        restarted_if(restart, &mut trees, &mut commits);
        let sends = match deleted {
            true => Vec::new(),
            false => vec![(
                change_as_the_user(&mut trees, &"c".into(), UserChange::Edit),
                "c".into(),
            )],
        };
        assert_eq!(quiet(&mut trees, &mut commits, &mut ids), sends);
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            dir(ROOT, "server")
        );
    }
}

fn a_server_entry_the_user_moved_into_another_held_folder(
    later: Option<Answer>,
    moved_out: &str,
) -> (Trees, Commits, IdGen, Vec<(&'static str, String)>) {
    let both = [
        ("x", dir(ROOT, "server")),
        ("c", file("x", "child", 3)),
        ("xd", dir("x", "inner")),
        ("xc", file("xd", "deeper", 4)),
        ("m", file(ROOT, "m", 7)),
    ];
    let mut local_tree = vec![("f", dir(ROOT, "new"))];
    if later.is_none() {
        local_tree.push(("g", dir(ROOT, "other")));
    }
    local_tree.extend(both.iter().cloned());
    let mut trees = trees(&local_tree, &both);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    if later.is_some() {
        trees.set_local("g".into(), local_node(dir(ROOT, "other"), 400));
        trees.settle(commits.aliases());
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
    }
    let ignored = Some(RuleFile::parse(b"ignored\n"));
    trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
    for (id, held) in [("x", "f"), (moved_out, "g")] {
        let mut node = trees.local().get(&id.into()).unwrap().clone();
        (node.shape.parent, node.shape.name) = (held.into(), "ignored".into());
        trees.set_local_changed(id.into(), node);
    }
    trees.settle(commits.aliases());
    let first = (commits.questions().iter())
        .find(|asked| later.is_none() || !asked.shown.contains_key(&"g".into()))
        .unwrap()
        .id
        .clone();
    assert!(commits.decide(&mut trees, &first, Answer::Discard));
    if let Some(answer) = later {
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert!(commits.decide(&mut trees, &question(&commits).id, answer));
    }
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(commits.questions().is_empty());
    (trees, commits, ids, sent)
}

#[test]
fn deleting_an_entry_the_discard_takes_back_into_the_folder_the_user_moved_it_out_of_makes_that_folder_the_users()
 {
    let (trees, ..) = a_server_entry_the_user_moved_into_another_held_folder(None, "xd");
    for id in ["xd", "xc"] {
        assert_eq!(
            trees.state().discard_members.get(&id.into()),
            Some(&"x".into())
        );
    }
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) =
            a_server_entry_the_user_moved_into_another_held_folder(None, "c");
        assert_eq!(sent, []);
        assert_eq!(the_next_effect(&mut trees, &commits, &mut ids), "c");
        assert_eq!(trees.local().get(&"c".into()).unwrap().shape.parent, "x");
        restarted_if(restart, &mut trees, &mut commits);
        trees.remove_local(&"c".into());
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_the_users(&mut sent, ("delete", "c"), &trees);
    }
}

#[test]
fn changing_an_entry_a_later_discard_takes_back_into_the_folder_the_user_moved_it_out_of_makes_that_folder_the_users()
 {
    for (edit, restart) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut trees, mut commits, mut ids, sent) =
            a_server_entry_the_user_moved_into_another_held_folder(Some(Answer::Discard), "c");
        assert_eq!(sent, []);
        assert_eq!(the_next_effect(&mut trees, &commits, &mut ids), "c");
        restarted_if(restart, &mut trees, &mut commits);
        let sends = match edit {
            true => change_as_the_user(&mut trees, &"c".into(), UserChange::Edit),
            false => {
                trees.remove_local(&"c".into());
                "delete"
            }
        };
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_the_users(&mut sent, (sends, "c"), &trees);
    }
}

#[test]
fn an_entry_the_user_moved_out_of_the_folder_into_a_held_folder_the_person_publishes_is_not_one_of_its_entries()
 {
    for restart in [false, true] {
        let (mut trees, mut commits, mut ids, sent) =
            a_server_entry_the_user_moved_into_another_held_folder(Some(Answer::Publish), "c");
        assert_eq!(sent, [("create dir", "g".into()), ("move", "c".into())]);
        assert_eq!(trees.state().discard_members.get(&"c".into()), None);
        let published = [
            ("g", Some(dir(ROOT, "other"))),
            ("c", Some(file("g", "ignored", 3))),
        ];
        the_server_applies(&mut trees, &mut commits, &published, 2);
        restarted_if(restart, &mut trees, &mut commits);
        let sends = change_as_the_user(&mut trees, &"c".into(), UserChange::Edit);
        assert_eq!(
            quiet(&mut trees, &mut commits, &mut ids),
            [(sends, "c".into())]
        );
        assert_eq!(
            trees.local().get(&"x".into()).unwrap().shape,
            dir(ROOT, "server")
        );
        assert_eq!(trees.local().get(&"c".into()).unwrap().shape.parent, "g");
    }
}

#[test]
fn changing_an_entry_another_device_moved_between_two_taken_along_folders_before_the_answer_makes_the_folder_it_moved_into_the_users()
 {
    for (change, restart) in [(UserChange::Edit, false), (UserChange::Rename, true)] {
        let both = vec![
            ("x", dir(ROOT, "server")),
            ("xd", dir("x", "inner")),
            ("c", file("x", "child", 3)),
            ("m", file(ROOT, "m", 7)),
            ("y", dir(ROOT, "other")),
            ("e", file("y", "e", 5)),
        ];
        let mut on_disk = vec![("f", dir(ROOT, "new"))];
        on_disk.extend(both.iter().cloned());
        let mut trees = trees(&on_disk, &both);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let mut record = trees.remote().get(&"e".into()).unwrap().clone();
        (record.shape, record.meta_rev, record.meta_edit_rev) = (file("x", "e", 5), 2, 2);
        trees.set_remote("e".into(), record);
        trees.settle(commits.aliases());
        let ignored = Some(RuleFile::parse(b"ignored\nignored2\n"));
        trees.set_rules(&[] as &[&str], Source::Koiignore, ignored);
        for (id, name) in [("x", "ignored"), ("y", "ignored2")] {
            let mut node = trees.local().get(&id.into()).unwrap().clone();
            (node.shape.parent, node.shape.name) = ("f".into(), name.into());
            trees.set_local_changed(id.into(), node);
        }
        trees.settle(commits.aliases());
        assert!(commits.decide(&mut trees, &asked.id, Answer::Discard));
        trees.set_rules(&[] as &[&str], Source::Koiignore, None);
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        while trees.local().get(&"e".into()).unwrap().shape.parent != "x" {
            the_next_effect(&mut trees, &commits, &mut ids);
        }
        restarted_if(restart, &mut trees, &mut commits);
        let sends = change_as_the_user(&mut trees, &"e".into(), change);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        sent.sort();
        let mut expected = vec![
            ("create dir", "f".to_owned()),
            ("move", "x".to_owned()),
            ("move", "y".to_owned()),
            (sends, "e".to_owned()),
        ];
        expected.sort();
        assert_eq!(sent, expected, "{restart}");
    }
}

#[test]
fn a_late_acknowledgement_of_this_devices_move_keeps_an_entry_another_device_then_moved_into_a_taken_along_folder_that_devices()
 {
    for (own_record_first, restart) in [(true, false), (false, true)] {
        let (mut trees, mut commits, mut ids, sent) =
            a_server_folder_this_device_discards(&[("a", dir(ROOT, "a"))], |_, _, _| {}, |_, _| {});
        assert_eq!(sent, []);
        let moved =
            this_device_sends_a_move(&mut trees, &mut commits, &mut ids, "m", file("a", "m", 7));
        assert_eq!(moved, [("move", "m".to_owned())]);
        if own_record_first {
            let mut own = trees.remote().get(&"m".into()).unwrap().clone();
            (own.shape, own.meta_rev, own.meta_edit_rev) = (file("a", "m", 7), 10, 10);
            trees.set_remote_made_here("m".into(), own);
            trees.settle(commits.aliases());
        }
        let mut other = trees.remote().get(&"m".into()).unwrap().clone();
        (other.shape, other.meta_rev, other.meta_edit_rev) = (file("x", "m", 7), 11, 11);
        trees.set_remote("m".into(), other);
        trees.settle(commits.aliases());
        let result = OpResult {
            code: op_result::Code::Applied as i32,
            node_id: "m".into(),
            ..Default::default()
        };
        (commits.received(CommitResult {
            seq: 10,
            results: vec![result],
            ..Default::default()
        }))
        .unwrap();
        assert!(commits.acknowledge(&mut trees, 11));
        trees.settle(commits.aliases());
        while trees.local().get(&"m".into()).unwrap().shape.parent != "x" {
            the_next_effect(&mut trees, &commits, &mut ids);
        }
        restarted_if(restart, &mut trees, &mut commits);
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [(sends, "m".to_owned())],
            "{own_record_first} {restart}"
        );
    }
}

#[test]
fn an_entry_this_device_moves_from_folder_to_folder_keeps_only_the_stays_of_the_folders_it_is_in() {
    let extra = [
        ("p1", dir(ROOT, "p1")),
        ("p2", dir("p1", "p2")),
        ("p3", dir(ROOT, "p3")),
    ];
    let (mut trees, mut commits, mut ids, sent) =
        a_server_folder_this_device_discards(&extra, |_, _, _| {}, |_, _| {});
    assert_eq!(sent, []);
    for (seq, folder, stays) in [
        (3, "p1", &["p1"][..]),
        (4, "p2", &["p1", "p2"]),
        (5, "p3", &["p3"]),
    ] {
        let moved = this_device_sends_a_move(
            &mut trees,
            &mut commits,
            &mut ids,
            "m",
            file(folder, "m", 7),
        );
        assert_eq!(moved, [("move", "m".to_owned())]);
        the_server_confirms_the_move(
            &mut trees,
            &mut commits,
            "m",
            file(folder, "m", 7),
            seq,
            false,
        );
        let kept: Vec<String> = (trees.state().discard_stays.iter())
            .filter(|(id, _)| *id == "m")
            .map(|(_, folder)| folder.to_string())
            .collect();
        assert_eq!(kept, stays, "into {folder}");
    }
}

#[test]
fn an_entry_whose_move_out_the_server_refuses_keeps_this_devices_stay_in_the_folder() {
    for restart in [false, true] {
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                let into_x = this_device_sends_a_move(trees, commits, ids, "m", moved_in.clone());
                assert_eq!(into_x, [("move", "m".into())]);
                the_server_confirms_the_move(trees, commits, "m", moved_in.clone(), 2, false);
                let out = this_device_sends_a_move(trees, commits, ids, "m", file(ROOT, "m", 7));
                assert_eq!(out, [("move", "m".into())]);
                let refused = OpResult {
                    code: op_result::Code::Rejected as i32,
                    reject_reason: "CYCLE".into(),
                    ..Default::default()
                };
                (commits.received(CommitResult {
                    seq: 3,
                    results: vec![refused],
                    ..Default::default()
                }))
                .unwrap();
                assert!(commits.acknowledge(trees, 3));
                trees.settle(commits.aliases());
                while trees.local().get(&"m".into()).unwrap().shape.parent != "x" {
                    the_next_effect(trees, commits, ids);
                }
            },
            |_, _| {},
        );
        assert_eq!(sent, [], "{restart}");
        restarted_if(restart, &mut trees, &mut commits);
        let stay = (NodeId::from("m"), NodeId::from("x"));
        assert!(trees.state().discard_stays.contains(&stay), "{restart}");
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let mut sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_the_users(&mut sent, (sends, "m"), &trees);
    }
}

#[test]
fn an_entry_another_device_moves_out_before_this_devices_move_in_is_acknowledged_is_another_devices()
 {
    for restart in [false, true] {
        let moved_in = file("x", "m", 7);
        let (mut trees, mut commits, mut ids, sent) = a_server_folder_this_device_discards(
            &[],
            |trees, commits, ids| {
                let into_x = this_device_sends_a_move(trees, commits, ids, "m", moved_in.clone());
                assert_eq!(into_x, [("move", "m".into())]);
                let mut own = trees.remote().get(&"m".into()).unwrap().clone();
                (own.shape, own.meta_rev, own.meta_edit_rev) = (moved_in.clone(), 2, 2);
                trees.set_remote_made_here("m".into(), own);
                let mut out = trees.remote().get(&"m".into()).unwrap().clone();
                (out.shape, out.meta_rev, out.meta_edit_rev) = (file(ROOT, "m", 7), 3, 3);
                trees.set_remote("m".into(), out);
                trees.settle(commits.aliases());
                let applied = OpResult {
                    code: op_result::Code::Applied as i32,
                    node_id: "m".into(),
                    ..Default::default()
                };
                (commits.received(CommitResult {
                    seq: 2,
                    results: vec![applied],
                    ..Default::default()
                }))
                .unwrap();
                assert!(commits.acknowledge(trees, 3));
                trees.settle(commits.aliases());
            },
            |trees, _| {
                assert_eq!(trees.synced().get(&"m".into()).unwrap().parent, "x");
                assert_eq!(trees.remote().live(&"m".into()).unwrap().parent, ROOT);
            },
        );
        assert_eq!(sent, [], "{restart}");
        restarted_if(restart, &mut trees, &mut commits);
        let stay = (NodeId::from("m"), NodeId::from("x"));
        assert!(!trees.state().discard_stays.contains(&stay), "{restart}");
        assert_eq!(trees.local().get(&"m".into()).unwrap().shape.parent, "x");
        let sends = change_as_the_user(&mut trees, &"m".into(), UserChange::Edit);
        let sent = quiet(&mut trees, &mut commits, &mut ids);
        assert_eq!(
            sent,
            [(sends, "m".to_owned())],
            "{restart}: {:?}",
            local(&trees)
        );
    }
}
