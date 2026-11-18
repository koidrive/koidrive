// SPDX-License-Identifier: Apache-2.0

//! Matching a first link's local entries against the server's tree (docs/design/sync-engine.md
//! section 6.4).

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::{Commits, Failure, Halt, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, Effects, Stuck, stuck};
use koidrive_engine::first_link::{History, PriorSync, adopt};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::RuleFile;
use koidrive_engine::names::conflict_name;
use koidrive_engine::question::{Answer, Question, QuestionKind, RecoveryGroup};
use koidrive_engine::state::CommitsState;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, Names, RemoteNode, Shape};
use koidrive_proto::v1::{CommitResult, Error, OpResult, Welcome, error, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";

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

/// Trees with `local` in L and `remote` in R, and nothing in S, as on a first link.
fn trees(local: &[(&str, Shape)], remote: &[(&str, Shape)]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in local.iter().enumerate() {
        let identity = FileIdentity {
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let node = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
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
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle(&Default::default());
    trees
}

fn mark(trees: &mut Trees, id: &str, stem: &str) {
    let mut node = trees.remote().get(id).expect("in R").clone();
    node.pending_for = Some(stem.into());
    trees.set_remote(id.into(), node);
    trees.settle(&Default::default());
}

/// The local nodes by ID with their parent and name, in order.
fn local(trees: &Trees) -> Vec<(String, String, String)> {
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
    assert_eq!(trees.synced().live("r-a"), Some(&file(ROOT, "a", 1)));
}

fn written_at(name: &str, mtime_ns: i64) -> Shape {
    Shape {
        content: Some(Content {
            mtime_ns,
            ..content(1)
        }),
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
    };
    trees.set_remote("r-a".into(), trashed);
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    commits.first_link(&mut trees, &mut IdGen::new(7), NOW, PriorSync::Shown);
    let held: Vec<&String> = (commits.questions().iter())
        .flat_map(|question| question.holds())
        .collect();
    assert_eq!(held, ["l-a"]);
    assert!(trees.local().get("l-a").is_some());
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
    assert!(trees.local().get("r-docs").is_some());
    assert!(trees.local().get("r-same").is_some());
    assert_eq!(
        trees.local().get("l-edited").unwrap().shape.parent,
        "r-docs"
    );
    assert!(trees.synced().get("r-edited").is_none());
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"*.log\n")));
    assert_eq!(adopt(&mut trees, None), Vec::<String>::new());
    assert!(trees.local().get("r-logs").is_some());
    assert!(trees.local().get("l-cache").is_some());
    assert!(trees.synced().get("l-cache").is_none());
    assert!(trees.synced().get("l-old").is_none());
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
    let tops: Vec<&str> = asked.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-new", "l-notes"]);
    let mut held: Vec<&str> = asked.holds().map(String::as_str).collect();
    held.sort();
    assert_eq!(held, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert_eq!(asked.held["l-new"].files, 1);
    assert!(held.iter().all(|id| trees.held(id)));
    assert!(!trees.held("r-docs") && !trees.held("r-a"));

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
    };
    trees.set_remote("r-slides".into(), trashed);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
    trees.settle(commits.aliases());
    let held: Vec<&String> = (commits.questions().iter())
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
        trees.local().get("r-slides").is_some(),
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
        trees.local().get("r-a").is_some(),
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
        trees.remote().live("r-notes"),
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
        };
        trees.set_remote("r-deleted".into(), trashed);
        trees.settle(&Default::default());
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
        commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Fresh);
        trees.settle(commits.aliases());
        let asked = question(&commits);
        let mut held: Vec<&String> = asked.holds().collect();
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
    let mut adopted: Vec<&String> = trees.local().iter().map(|(id, _)| id).collect();
    adopted.sort();
    assert_eq!(adopted, ["r-dir", "r-keep"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_held_entry_that_is_edited_or_moved_stays_held_under_a_new_question() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);

    let mut edited = trees.local().get("l-b").unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_edit = question(&commits);
    assert_ne!(after_edit.id, asked.id);
    assert_eq!(
        after_edit.held.keys().collect::<Vec<_>>(),
        asked.held.keys().collect::<Vec<_>>()
    );

    let mut moved = trees.local().get("l-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("l-notes".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_move = question(&commits);
    assert_ne!(after_move.id, after_edit.id);
    let tops: Vec<&str> = after_move.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-new"], "notes is held inside new now");
    assert!(trees.held("l-notes"));
}

#[test]
fn an_entry_moved_out_of_a_held_folder_stays_held_as_an_item_of_its_own() {
    let (mut commits, mut trees, mut ids) = linked();
    let mut moved = trees.local().get("l-inside").unwrap().clone();
    moved.shape.parent = "r-docs".into();
    trees.set_local("l-inside".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let tops: Vec<String> = question(&commits).held.into_keys().collect();
    assert_eq!(tops, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert!(trees.held("l-inside"));
}

#[test]
fn a_held_entry_the_user_deletes_leaves_the_question_and_the_last_one_ends_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local("l-inside");
    trees.remove_local("l-new");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after = question(&commits);
    assert_ne!(after.id, asked.id);
    let tops: Vec<&str> = after.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-notes"]);

    trees.remove_local("l-b");
    trees.remove_local("l-notes");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.questions().is_empty());
}

#[test]
fn a_held_entry_the_rules_leave_out_for_a_while_stays_held_until_the_person_answers() {
    let (mut commits, mut trees, mut ids) = linked();
    let root: [&str; 0] = [];
    let held = ["l-b", "l-inside", "l-new", "l-notes"];
    trees.set_rules(&root, Some(RuleFile::parse(b"notes\nnew/\n")));
    assert!(trees.excluded("l-notes") && trees.excluded("l-new"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, None);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "nothing held is sent once the rules let it in again"
    );
    assert!(held.iter().all(|id| trees.held(id)));

    assert!(commits.decide(&question(&commits).id, Answer::Publish));
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
    Vec<(String, String, String)>,
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
        [("r-file".to_owned(), ROOT.to_owned(), "a.docx".to_owned())]
    );
}

#[test]
fn a_local_file_and_an_equal_server_file_that_arrive_in_separate_settles_are_linked() {
    let shape = written("a.docx", WRITTEN_NS);
    let mut local_first = trees(&[("l-file", shape.clone())], &[]);
    let remote = trees(&[], &[("r-file", shape.clone())])
        .remote()
        .get("r-file")
        .cloned();
    local_first.set_remote("r-file".into(), remote.unwrap());
    let mut remote_first = trees(&[], &[("r-file", shape.clone())]);
    remote_first.set_local("l-file".into(), local_node(shape, 1));
    for mut trees in [local_first, remote_first] {
        trees.settle(&Default::default());
        assert_eq!(
            local(&trees),
            [("r-file".to_owned(), ROOT.to_owned(), "a.docx".to_owned())]
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
    assert!(trees.local().get("l-file").is_none());
    assert!(trees.local().get("r-file").is_some());
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
    let mut fresh = Trees::new(ROOT);
    for (id, node) in trees.local().iter() {
        fresh.set_local(id.clone(), node.clone());
    }
    for (id, node) in trees.remote().iter() {
        fresh.set_remote(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        fresh.set_synced(id.clone(), Some(shape.clone()));
    }
    fresh.restore(trees.state()).unwrap();
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
    assert!(trees.synced().get("p").is_none());
    assert!(trees.local().get("l-file").is_some());
    place(&mut trees);
    trees.settle(&Default::default());
    assert!(trees.local().get("l-file").is_none());
    assert!(trees.local().get("r-file").is_some());
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
        assert!(trees.local().get("l-file").is_some());
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
    assert!(trees.local().get("l-file").is_none());
    assert!(trees.local().get("r-file").is_some());
}

#[test]
fn a_file_the_rules_stop_excluding_is_linked_to_an_equal_server_file_on_the_next_settle() {
    let root: [&str; 0] = [];
    let mut trees = trees(&[], &[]);
    trees.rules_scanned();
    trees.set_rules(&root, Some(RuleFile::parse(b"a.docx\n")));
    trees.set_local(
        "l-file".into(),
        local_node(written("a.docx", WRITTEN_NS), 1),
    );
    trees.set_remote("r-file".into(), remote_node(written("a.docx", WRITTEN_NS)));
    trees.settle(&Default::default());
    assert!(trees.excluded("l-file"));
    assert!(trees.local().get("l-file").is_some());

    trees.set_rules(&root, None);
    trees.settle(&Default::default());
    assert!(!trees.excluded("r-file"));
    assert_eq!(
        local(&trees),
        [("r-file".to_owned(), ROOT.to_owned(), "a.docx".to_owned())]
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
    trees.set_rules(&root, Some(RuleFile::parse(b"x/\n")));
    trees.set_local("p".into(), local_node(dir(ROOT, "x"), 1));
    trees.set_remote("p".into(), remote_node(dir(ROOT, "x")));
    trees.set_synced("p".into(), Some(dir(ROOT, "x")));
    let mut file = written("a.docx", WRITTEN_NS);
    file.parent = "p".into();
    trees.set_local("l-file".into(), local_node(file.clone(), 2));
    trees.set_remote("r-file".into(), remote_node(file));
    trees.settle(&Default::default());
    assert!(trees.excluded("l-file"));

    trees.set_local("p".into(), local_node(dir(ROOT, "y"), 1));
    trees.settle(&Default::default());
    assert!(!trees.excluded("l-file"));
    assert_eq!(
        local(&trees),
        [
            ("p".to_owned(), ROOT.to_owned(), "y".to_owned()),
            ("r-file".to_owned(), "p".to_owned(), "a.docx".to_owned()),
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
        [("r-file".to_owned(), ROOT.to_owned(), "a.docx".to_owned())]
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
        },
        parked: false,
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
    assert!(!commits.decide(&asked.id, Answer::Delete));
    assert!(commits.decide(&asked.id, Answer::Publish));
    assert!(
        !commits.decide(&asked.id, Answer::Publish),
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
    assert!(!trees.held("l-b"));
}

/// The server gives this device's create of `id` the conflict name `name`: R holds the file under
/// it at `seq`, and the commit's result names it.
fn renamed_create(commits: &mut Commits, trees: &mut Trees, id: &str, name: &str, seq: u64) {
    let shape = trees.local().get(id).unwrap().shape.clone();
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
    assert!(commits.decide(&question(&commits).id, Answer::Publish));
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
        },
        parked: false,
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
        (original.node_id.as_str(), copy.node_id.as_str()),
        ("r-later", "l-later")
    );
}

/// The effects of discarding `linked`'s entries and of the downloads that follow, applied as if
/// they succeeded, until none is left. Returns them, by node.
fn discarded(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(String, Effect)> {
    let asked = question(commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
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
    let removed: Vec<&str> = (done.iter())
        .filter(|(_, effect)| {
            matches!(effect, Effect::RemoveFile { .. } | Effect::RemoveDir { .. })
        })
        .map(|(node, _)| node.as_str())
        .collect();
    assert_eq!(removed.len(), 4, "{done:?}");
    let position = |id: &str| removed.iter().position(|node| *node == id);
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    let mut edited = trees.local().get("l-b").unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert_ne!(again.id, asked.id);
    assert_eq!(again.answer, None);
    assert!(trees.synced().get("l-b").is_none() && trees.held("l-b"));
}

#[test]
fn an_answer_to_a_replaced_question_changes_nothing() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local("l-b");
    ops(&mut commits, &mut trees, &mut ids);
    assert!(!commits.decide(&asked.id, Answer::Discard));
    assert!(trees.held("l-notes"));
}

#[test]
fn a_discarded_folder_the_user_puts_an_entry_in_is_asked_about_again() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed("l-new"));
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
    assert!(commits.decide(&again.id, Answer::Publish));
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
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let sub = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: dir("l-new", "sub"),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-sub".into(), sub);
    let mut moved = trees.local().get("r-notes").unwrap().clone();
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    let mut deleted = trees.remote().get("r-notes").unwrap().clone();
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    let mut deleted = trees.remote().get("r-notes").unwrap().clone();
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-sub", "later", 6),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed("l-sub") && trees.reclaimed("l-new"));
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
    assert!(commits.decide(&again.id, Answer::Publish));
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "inside", 5),
        identity: FileIdentity {
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
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
    assert!(commits.decide(&again.id, Answer::Publish));
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
    trees.set_rules_unknown("l-docs");
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    assert!(
        trees.local().get("r-docs").is_some(),
        "the folder is adopted"
    );
    assert!(trees.held("r-new"));
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
        },
        parked: false,
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.waiting().contains_key("l-A"));
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("pad");
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
    assert!(trees.waiting().contains_key("l-A"));
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
                },
                parked: false,
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
    assert!(trees.reclaimed("f"));
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
    assert_eq!(trees.waiting().get("c"), Some(&waiting));
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
    assert!(trees.reclaimed("f"));
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
    assert_eq!(trees.local().get("k").unwrap().shape, file("f", "kept", 3));
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
        };
        let local = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
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
    assert!(trees.reclaimed("f"));
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
    assert_eq!(trees.synced().live("f"), None);
    assert_eq!(trees.synced().live("c"), Some(&dir(ROOT, "a")));
    assert!(trees.waiting().contains_key("u"));
}

#[test]
fn a_reclaimed_folder_leaves_s_although_its_own_update_waits() {
    let trees = reclaim_with_waiting(
        &[("f", dir(ROOT, "a")), ("c", dir("f", "child"))],
        &[("c", dir(ROOT, "a"))],
        &[("c", dir(ROOT, "a")), ("f", dir("q", "a"))],
    );
    assert_eq!(trees.synced().live("f"), None);
    assert!(!trees.waiting().contains_key("f"));
}

#[test]
fn a_discarded_entry_the_rules_leave_out_for_a_while_goes_to_the_local_trash_once_they_let_it_in() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let parked = LocalNode {
        parked: true,
        ..trees.local().get("l-b").unwrap().clone()
    };
    trees.set_local("l-b".into(), parked);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Some(RuleFile::parse(b"b\n")));
    trees.settle(commits.aliases());
    assert!(trees.synced().get("l-b").is_none());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, None);
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
    assert!(trees.local().get("l-b").is_none(), "{done:?}");
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
) -> Vec<(String, Effect)> {
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Some(RuleFile::parse(b"a\n")));
    let mut effects = Effects::default();

    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(done.is_empty(), "{done:?}");
    let kept = trees.local().get("l-ss").unwrap();
    assert!(!kept.parked && kept.shape.same_position(&dir(ROOT, "ss")));
    assert!(trees.local().get("l-a").is_some());
    assert_eq!(
        stuck(&trees, &effects, commits.in_flight()),
        [
            ("r-c".to_owned(), Stuck::Behind("r-sharp-s".into())),
            ("r-sharp-s".to_owned(), Stuck::HeldBy("l-ss".into())),
        ]
        .into()
    );

    trees.set_rules(&root, None);
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
    assert!(commits.decide(&asked.id, Answer::Discard));
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
    assert!(commits.decide(&question(&commits).id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(
        &root,
        Some(RuleFile::parse(
            b"inside
",
        )),
    );
    let identity = FileIdentity {
        ino: 99,
        birth_ns: Some(0),
    };
    let later = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file("l-new", "later", 6),
        identity,
        parked: false,
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
    assert!(commits.decide(&again.id, Answer::Publish));
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
    assert_eq!(trees.local().get("l-inside").unwrap().shape.parent, "r-new");

    trees.set_rules(&root, None);
    trees.settle(commits.aliases());
    let sent = commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 2);
    assert!(sent.is_none(), "{sent:?}");
}

#[test]
fn a_file_the_user_took_back_from_a_discard_is_sent_again_once_the_server_loses_it() {
    let (mut commits, mut trees, mut ids) = linked();
    assert!(commits.decide(&question(&commits).id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let edit = |trees: &mut Trees, commits: &Commits, digest| {
        let mut node = trees.local().get("l-b").unwrap().clone();
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
    assert!(commits.decide(&again.id, Answer::Publish));
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

    trees.remove_remote("l-b");
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
        },
        parked: false,
    }
}

struct LetIn {
    trees: Trees,
    sent: Vec<(&'static str, String)>,
    done: Vec<(String, Effect)>,
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
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let root: [&str; 0] = [];
    trees.set_rules(&root, Some(RuleFile::parse(b"e\n*.txt\nss/\nd/\n")));
    let mut remote = trees.remote().get(moved).unwrap().clone();
    remote.shape.parent = "r-ss".into();
    remote.meta_rev = 2;
    trees.set_remote(moved.into(), remote);
    trees.settle(commits.aliases());
    let mut local = trees.local().get(moved).unwrap().clone();
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
    assert!(commits.decide(&again.id, Answer::Discard));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(trees.local().get("l-later").is_none());
    after_discard(&mut trees);

    trees.set_rules(&root, None);
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
    assert!(trees.local().get("l-new").is_none());
    assert_eq!(
        trees.local().get("r-c").map(|node| &node.shape),
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
            assert_eq!(trees.synced().live("r-c"), None);
        },
        |_| {},
    );
    assert_eq!(sent, []);
    assert_eq!(
        trees.local().get("r-c").map(|node| &node.shape),
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
            let mut edited = trees.remote().get("r-c").unwrap().clone();
            edited.shape.content = Some(content(5));
            edited.content_rev = 3;
            edited.edit_rev = 3;
            trees.set_remote("r-c".into(), edited);
        },
    );
    assert_eq!(sent, []);
    assert!(trees.local().get("l-new").is_none());
    assert_eq!(
        trees.local().get("r-c").map(|node| &node.shape),
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
            let mut edited = trees.local().get("r-c").unwrap().clone();
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
    trees.set_rules(&root, Some(RuleFile::parse(b"e2\n")));
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    trees.settle(commits.aliases());
    let asked = question(&commits);
    assert!(!asked.holds().any(|id| id == "l-e2"));
    assert!(commits.decide(&asked.id, Answer::Discard));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);

    trees.set_rules(&root, None);
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(
        trees.local().get("l-e2").is_some(),
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    let mut trashed = trees.remote().get("x").unwrap().clone();
    trashed.trashed = true;
    trees.set_remote("x".into(), trashed);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut edited = trees.local().get("x").unwrap().clone();
    edited.shape.content = Some(content(8));
    trees.set_local_changed("x".into(), edited);
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert!(trees.edited("x"));
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
    trees.set_rules(&root, Some(RuleFile::parse(b"*.txt\n")));
    trees.settle(&Default::default());
    for id in ["f", "x", "y"] {
        let mut node = trees.remote().get(id).unwrap().clone();
        node.trashed = true;
        node.meta_rev = 2;
        node.trash_group = "g".into();
        trees.set_remote(id.into(), node);
    }
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    trees.set_rules(&root, None);
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(trees.local().get("x").is_none(), "{:?}", local(&trees));
}

#[test]
fn an_unchanged_excluded_subtree_another_device_deleted_is_not_brought_back() {
    let entries = [("f", dir(ROOT, "folder")), ("c", file("f", "child", 9))];
    let mut trees = trees(&entries, &entries);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"folder/\n")));
    for id in ["c", "f"] {
        let mut node = trees.remote().get(id).unwrap().clone();
        node.trashed = true;
        node.meta_rev = 2;
        trees.set_remote(id.into(), node);
    }
    trees.settle(commits.aliases());
    assert!(trees.check().is_ok());
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.is_empty(),
        "nothing changed locally, but a remote delete is undone: {sent:?}"
    );
    let mut effects = Effects::default();
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    assert!(
        trees.local().get("c").is_none() && trees.local().get("f").is_none(),
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut edited = trees.remote().get("x").unwrap().clone();
    edited.shape.content = Some(content(7));
    edited.content_rev = 2;
    edited.edit_rev = 2;
    trees.set_remote("x".into(), edited);
    trees.set_rules(&[] as &[&str], None);
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
    assert!(trees.local().get("f").is_none());
    assert_eq!(
        trees.local().get("x").unwrap().shape,
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], None);
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
    assert!(trees.local().get("f").is_none());
    assert_eq!(
        trees.local().get("x").unwrap().shape,
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.remove_local("g");
    trees.remove_local("h");
    trees.set_rules(&[] as &[&str], None);
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
    assert!(trees.local().get("f").is_none());
    assert_eq!(trees.local().get("h").unwrap().shape, dir(ROOT, "home"));
    assert_eq!(trees.local().get("x").unwrap().shape, dir("h", "server"));
    assert_eq!(trees.synced().get("x"), Some(&dir("h", "server")));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"*.txt\n")));
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"*.txt\nignored\n")));
    let mut moved = trees.local().get("z").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("z".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], None);
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
    assert!(trees.local().get("f").is_none());
    assert_eq!(trees.local().get("z").unwrap().shape, dir(ROOT, "server"));
    assert_eq!(trees.local().get("y").unwrap().shape, dir("z", "k.txt"));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    trees.set_rules(&[] as &[&str], None);
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
    assert!(trees.local().get("f").is_none());
    assert_eq!(
        trees.local().get("x").unwrap().shape,
        file(ROOT, "server", 9)
    );
    trees.remove_remote("x");
    trees.settle(commits.aliases());
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(
        sent.is_empty(),
        "the server dropped the file, but it is sent again: {sent:?}"
    );
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 300);
    assert!(trees.local().get("x").is_none());
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_synced("c".into(), Some(file("x", "child", 5)));
    trees.set_local("c".into(), local_node(file("x", "child", 5), 50));
    for id in changed {
        let mut node = trees.remote().get(id).unwrap().clone();
        change(&mut node);
        node.content_rev += 1;
        node.meta_rev += 1;
        node.edit_rev += 1;
        node.meta_edit_rev += 1;
        trees.set_remote((*id).into(), node);
    }
    trees.set_rules(&[] as &[&str], None);
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
    assert_eq!(trees.local().get("c").unwrap().shape, file("x", "child", 6));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], None);
    for (id, shape) in synced {
        trees.set_synced((*id).into(), Some(shape.clone()));
    }
    trees.settle(commits.aliases());
    (trees, commits, ids)
}

fn the_discard_holds_once_the_server_entry_comes_back(
    remote: &[(&str, Shape)],
    synced: &[(&str, Shape)],
) -> Vec<(String, String, String)> {
    let (mut trees, mut commits, mut ids) =
        a_server_entry_deleted_inside_a_taken_along_folder(remote, synced);
    assert!(!trees.reclaimed("f"));
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
        assert!(trees.reclaimed("f"), "{remote:?}");
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\nhidden\n")));
    for (id, parent, name) in [("x", "f", "ignored"), ("z", "y", "hidden")] {
        let mut moved = trees.local().get(id).unwrap().clone();
        moved.shape.parent = parent.into();
        moved.shape.name = name.into();
        trees.set_local_changed(id.into(), moved);
    }
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"hidden\n")));
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed("f"));
    run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    assert!(trees.local().get("f").is_none());
    assert_eq!(trees.local().get("x").unwrap().shape, dir(ROOT, "server"));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
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
            },
            parked: false,
        },
    );
    trees.set_rules(&[] as &[&str], None);
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
        Some(RuleFile::parse(
            b"ignored/
",
        )),
    );
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    change(&mut trees);
    trees.set_rules(&[] as &[&str], None);
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
    assert_eq!(trees.local().get("x").unwrap().shape, dir("f", "ignored"));
    (sent, trees)
}

#[test]
fn a_file_deleted_inside_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(|trees| {
        trees.remove_local("c")
    });
    assert!(sent.contains(&("delete", "c".into())), "{sent:?}");
    assert!(trees.local().get("c").is_none());
}

#[test]
fn a_server_folder_the_user_deletes_with_its_file_in_a_discarded_folder_stays_gone_here_in_either_order()
 {
    for order in [["c", "x"], ["x", "c"]] {
        let (sent, trees) =
            a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
                for id in order {
                    trees.remove_local(id);
                }
            });
        assert_eq!(sent, [], "{order:?}");
        assert!(trees.local().get("x").is_none(), "{order:?}");
        assert!(trees.local().get("c").is_none(), "{order:?}");
        assert_eq!(trees.remote().live("c"), Some(&file("x", "child", 9)));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    let mut away = trees.remote().get("x").unwrap().clone();
    away.shape = dir("p", "server");
    trees.set_remote("x".into(), away.clone());
    away.shape = file("x", "child", 5);
    trees.set_remote("c".into(), away);
    trees.settle(commits.aliases());
    assert_eq!(trees.synced().get("x"), None);
    trees.set_rules(&[] as &[&str], None);
    let mut effects = Effects::default();
    for ino in 100..150 {
        trees.settle(commits.aliases());
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        if trees.local().get("c").is_some() {
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
    assert_eq!(trees.local().get("x").unwrap().shape, dir("f", "ignored"));
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
fn a_file_the_user_deletes_after_it_arrived_in_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            trees.remove_local("c");
        });
    assert_eq!(
        sent,
        [("create dir", "f".to_owned()), ("move", "x".to_owned())]
    );
    assert_eq!(trees.local().get("x").unwrap().shape, dir("f", "ignored"));
    assert!(trees.local().get("c").is_none());
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_leaves_the_folder_discarded()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get("c").unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(trees.local().get("x").unwrap().shape, dir("p", "server"));
    assert!(trees.local().get("f").is_none());
    assert_eq!(
        trees.local().get("c").unwrap().shape,
        file(ROOT, "moved", 5)
    );
}

#[test]
fn a_file_moved_out_of_a_server_folder_in_a_discarded_folder_leaves_the_folder_discarded() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
        let mut moved = trees.local().get("c").unwrap().clone();
        moved.shape.parent = ROOT.into();
        trees.set_local_changed("c".into(), moved);
    });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(trees.local().get("x").unwrap().shape, dir(ROOT, "server"));
    assert!(trees.local().get("f").is_none());
    assert_eq!(
        trees.local().get("c").unwrap().shape,
        file(ROOT, "child", 9)
    );
}

#[test]
fn a_file_edited_inside_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users() {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(|trees| {
        let mut edited = trees.local().get("c").unwrap().clone();
        edited.shape.content = Some(content(4));
        trees.set_local_changed("c".into(), edited);
    });
    assert!(sent.contains(&("update", "c".into())), "{sent:?}");
    assert_eq!(trees.local().get("c").unwrap().shape, file("x", "child", 4));
}

#[test]
fn a_file_the_server_dropped_and_edited_here_inside_a_server_folder_in_a_discarded_folder_makes_the_folder_the_users()
 {
    let (_, trees) = a_server_folder_in_a_discarded_folder_reclaimed_after_the_answer(|trees| {
        trees.remove_remote("c");
        let mut edited = trees.local().get("c").unwrap().clone();
        edited.shape.content = Some(content(4));
        trees.set_local_changed("c".into(), edited);
    });
    assert_eq!(trees.local().get("c").unwrap().shape, file("x", "child", 4));
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(commits.questions().is_empty());
    let mut trashed = trees.remote().get("c").unwrap().clone();
    trashed.trashed = true;
    trees.set_remote("c".into(), trashed);
    trees.remove_remote("c");
    let mut edited = trees.local().get("c").unwrap().clone();
    edited.shape.content = Some(content(8));
    trees.set_local_changed("c".into(), edited);
    trees.set_rules(&[] as &[&str], None);
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    assert!(!trees.reclaimed("f"));

    let server = trees.remote().get("x").unwrap().clone();
    let added = RemoteNode {
        shape: dir("x", "added"),
        ..server.clone()
    };
    trees.set_remote("d".into(), added);
    let mut moved = trees.local().get("u").unwrap().clone();
    moved.shape.parent = "x".into();
    trees.set_local_changed("u".into(), moved);
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert!(trees.reclaimed("f"));
    let mut effects = Effects::default();
    let download = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "d");
    let identity = FileIdentity {
        ino: 901,
        birth_ns: Some(0),
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
    let mut moved = trees.local().get("u").unwrap().clone();
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
    assert!(trees.local().get("u").unwrap().parked);

    assert!(trees.reclaimed("f"));
    let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE);
    assert!(
        (planned.iter()).all(|record| record.node != "x"),
        "the server folder leaves the folder that is the user's again: {planned:?}"
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored/\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());

    let server = trees.remote().get("x").unwrap().clone();
    let added = RemoteNode {
        shape: dir("x", "added"),
        ..server
    };
    trees.set_remote("d".into(), added);
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed("f"));
    let mut effects = Effects::default();
    let download = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE))
        .first()
        .cloned()
        .unwrap();
    assert_eq!(download.node, "d");
    let identity = FileIdentity {
        ino: 902,
        birth_ns: Some(0),
    };
    (effects.done(
        &mut trees,
        commits.in_flight(),
        &download.effect_id,
        Some(identity),
    ))
    .unwrap();
    trees.settle(commits.aliases());
    let mut moved = trees.local().get("d").unwrap().clone();
    moved.shape.parent = ROOT.into();
    trees.set_local_changed("d".into(), moved);
    trees.settle(commits.aliases());

    assert!(!trees.reclaimed("f"));
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
        assert_eq!(trees.synced().get("r-a"), None, "{mtime_ns}");
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
            trees.synced().live("r-a"),
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
        assert_eq!(trees.synced().get("r-a"), None, "{local_mtime_ns}");
    }
}

#[test]
fn two_writes_with_the_same_bytes_and_a_copied_sub_second_mtime_are_linked() {
    let (sent, trees) = sent_beside(1_794_268_740_123_456_789, 1_794_268_740_123_456_789);
    assert_eq!(sent, []);
    assert_eq!(
        trees.synced().live("r-a"),
        Some(&written("a", 1_794_268_740_123_456_789))
    );
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_trashes_it()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get("c").unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
            let mut trashed = trees.remote().get("c").unwrap().clone();
            trashed.trashed = true;
            trees.set_remote("c".into(), trashed);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get("c").unwrap().shape,
        file(ROOT, "moved", 5)
    );
}

#[test]
fn a_file_moved_out_of_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_trashes_it()
 {
    let (sent, trees) = a_server_folder_in_a_discarded_folder_changed_after_the_answer(|trees| {
        let mut moved = trees.local().get("c").unwrap().clone();
        moved.shape.parent = ROOT.into();
        trees.set_local_changed("c".into(), moved);
        let mut trashed = trees.remote().get("c").unwrap().clone();
        trashed.trashed = true;
        trees.set_remote("c".into(), trashed);
    });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get("c").unwrap().shape,
        file(ROOT, "child", 9)
    );
}

#[test]
fn a_file_the_user_moves_out_after_it_arrived_in_a_server_folder_in_a_discarded_folder_keeps_the_move_when_another_device_moves_and_trashes_it()
 {
    let (sent, trees) =
        a_file_that_arrived_in_a_server_folder_in_a_discarded_folder_after(|trees| {
            let mut moved = trees.local().get("c").unwrap().clone();
            moved.shape.parent = ROOT.into();
            moved.shape.name = "moved".into();
            trees.set_local_changed("c".into(), moved);
            let mut trashed = trees.remote().get("c").unwrap().clone();
            trashed.shape.parent = ROOT.into();
            trashed.shape.name = "remote-moved".into();
            trashed.trashed = true;
            trees.set_remote("c".into(), trashed);
        });
    assert_eq!(sent, [("move", "c".to_owned())]);
    assert_eq!(
        trees.local().get("c").unwrap().shape,
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

    assert!(trees.synced().get("f").is_some());
    trees.set_remote("p".into(), remote_node(dir(ROOT, "b")));
    let mut p = trees.local().get("f").unwrap().clone();
    p.shape = dir(ROOT, "b");
    p.identity.ino = 10;
    trees.set_local("p".into(), p);
    trees.set_synced("p".into(), Some(dir(ROOT, "b")));
    let mut c = trees.local().get("c").unwrap().clone();
    c.shape = dir("p", "child");
    trees.set_local("c".into(), c);
    trees.set_synced("c".into(), Some(dir("p", "child")));
    trees.settle(commits.aliases());
    let first = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 1)
        .cloned();
    for q in commits.questions().to_vec() {
        commits.decide(&q.id, Answer::Publish);
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
    let k = trees.local().get("k").unwrap();
    assert_eq!((k.shape.clone(), k.identity.ino), (file("f", "kept", 3), 3));
    assert!(sent.contains(&("move", "k".to_owned())), "{sent:?}");
    assert!(sent.iter().all(|(op, _)| *op != "create file"), "{sent:?}");
}

fn one_cycle_after_the_rules_let_in_a_server_folder_a_discard_took_along(
    files: usize,
) -> std::time::Duration {
    let names: Vec<String> = (0..files).map(|i| format!("y{i}")).collect();
    let server: Vec<(&str, Shape)> = std::iter::once(("x", dir(ROOT, "server")))
        .chain(names.iter().map(|name| (name.as_str(), file("x", name, 9))))
        .collect();
    let local: Vec<(&str, Shape)> = std::iter::once(("f", dir(ROOT, "new")))
        .chain(server.iter().cloned())
        .collect();
    let mut trees = trees(&local, &server);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let asked = question(&commits);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    let started = std::time::Instant::now();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let planned = Effects::default()
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .to_vec();
    let took = started.elapsed();
    assert!(
        planned.iter().any(|record| record.node == "x"),
        "{planned:?}"
    );
    took
}

#[test]
fn a_server_folder_a_discard_took_along_is_planned_in_linear_time_once_the_rules_let_it_in() {
    let (two_hundred, two_thousand) = (
        one_cycle_after_the_rules_let_in_a_server_folder_a_discard_took_along(200),
        one_cycle_after_the_rules_let_in_a_server_folder_a_discard_took_along(2000),
    );
    assert!(
        two_thousand < two_hundred * 30,
        "a build and a plan with 2,000 taken-along files took {two_thousand:?}, and with 200 {two_hundred:?}"
    );
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.set_local("n".into(), local_node(file("f", "n", 6), 99));
    trees.settle(commits.aliases());
    assert!(trees.reclaimed("f"));
    assert!(ops(&mut commits, &mut trees, &mut ids).is_empty());
    trees.remove_local("n");
    trees.settle(commits.aliases());
    assert!(!trees.reclaimed("f"));
    let mut effects = Effects::default();
    let mut done = Vec::new();
    if lift_after_effects {
        done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    }
    trees.set_rules(&[] as &[&str], None);
    done.extend(run_effects(
        &mut effects,
        &mut commits,
        &mut trees,
        &mut ids,
        200,
    ));
    assert!(commits.questions().is_empty());
    assert!(trees.local().get("f").is_none(), "{done:?}");
    assert_eq!(
        trees.local().get("x").unwrap().shape,
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
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
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
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    let done = run_effects(
        &mut Effects::default(),
        &mut commits,
        &mut trees,
        &mut ids,
        100,
    );
    assert!(trees.local().get("f").is_none(), "{done:?}");
    assert_eq!(
        trees.local().get("x").unwrap().shape,
        file(ROOT, "server", 9)
    );
}

#[test]
fn a_second_discard_takes_along_a_server_file_the_rules_let_in_while_the_folder_was_asked_about_again()
 {
    let (mut commits, mut trees, mut ids, asked) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_local("n".into(), local_node(file("f", "n", 6), 99));
    trees.settle(commits.aliases());
    assert!(trees.reclaimed("f"));
    let mut effects = Effects::default();
    run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 100);
    let again = question(&commits);
    assert!(again.id != asked.id && again.answer.is_none());
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question(&commits).id, Answer::Discard));
    let done = run_effects(&mut effects, &mut commits, &mut trees, &mut ids, 200);
    assert!(trees.local().get("f").is_none(), "{done:?}");
    assert!(trees.local().get("n").is_none(), "{done:?}");
    assert_eq!(
        trees.local().get("x").unwrap().shape,
        file(ROOT, "server", 9)
    );
}

#[test]
fn publishing_sends_a_server_file_the_user_moved_in_while_the_rules_left_it_out_only_with_its_folder()
 {
    let (mut commits, mut trees, mut ids, _) =
        a_folder_with_a_server_file_the_user_moved_in_while_the_rules_left_it_out();
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question(&commits).id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
    assert!(sent.contains(&("move", "x".to_owned())), "{sent:?}");
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
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"server\n")));
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], None);
    trees.settle(commits.aliases());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question(&commits).id, Answer::Discard));
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
        assert!(trees.local().get("mine").is_none());
        assert_eq!(
            trees.local().get("x").map(|node| &node.shape),
            Some(&file(ROOT, "server", 9))
        );
        set_rules(&mut trees, rules);
        (commits, trees, ids)
    }

    fn set_rules(trees: &mut Trees, rules: Option<&[u8]>) {
        trees.set_rules(&[] as &[&str], rules.map(RuleFile::parse));
        trees.settle(&Default::default());
    }

    fn scan(trees: &mut Trees, id: &str, shape: Shape) {
        let mut node = trees.local().get(id).unwrap().clone();
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
        assert!(commits.decide(&id, Answer::Discard));
        let sent = settle(&mut commits, &mut trees, &mut ids);
        (sent, trees)
    }

    fn kept_and_sent(sent: &[(&'static str, String)], trees: &Trees) {
        assert!(trees.local().get("f").is_some(), "{sent:?}");
        assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
        assert!(sent.contains(&("move", "x".to_owned())), "{sent:?}");
    }

    fn taken_along(sent: &[(&'static str, String)], trees: &Trees, digest: u8) {
        assert!(sent.is_empty(), "{sent:?}");
        assert!(trees.local().get("f").is_none(), "{sent:?}");
        assert_eq!(
            trees.local().get("x").unwrap().shape,
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
            assert!(c.decide(&first.id, Answer::Discard));
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
            let mut node = t.remote().get("x").unwrap().clone();
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
            assert!(t.local().get("f").is_none(), "{sent:?}");
        }
    }

    #[test]
    fn while_left_out_is_taken_along_after_the_server_moves_it() {
        for origin in ORIGINS {
            let (mut c, mut t, mut i) = asked(Some(b"ignored\n"), origin);
            scan(&mut t, "x", file("f", "ignored", 9));
            quiet(&mut c, &mut t, &mut i);
            let mut node = t.remote().get("x").unwrap().clone();
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
                t.local().get("x").unwrap().shape,
                file(ROOT, "elsewhere", 9)
            );
        }
    }

    fn edited_on_the_server(t: &mut Trees) {
        let mut node = t.remote().get("x").unwrap().clone();
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
            assert!(t.local().get("f").is_none());
            assert_eq!(t.local().get("x").unwrap().shape, file(ROOT, "server", 10));
            assert_eq!(t.synced().live("x"), Some(&file(ROOT, "server", 10)));
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
            let mut node = t.local().get("x").unwrap().clone();
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
            let mut node = t.remote().get("x").unwrap().clone();
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
            assert!(t.local().get("f").is_some(), "{sent:?}");
            assert_eq!(
                t.local().get("x").map(|node| node.shape.content.clone()),
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
            let left_out = |t: &Trees, id: &str| t.local().get(id).unwrap().left_out_when_placed;
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
                t.local().get("x").unwrap().shape,
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
        };
        t.set_local(
            "copy".into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: copied.clone(),
                identity,
                parked: false,
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
            },
        );
        t.settle(&Default::default());
        assert_eq!(settle(&mut c, &mut t, &mut i), []);
        assert!(t.local().get("copy").is_none());
        set_rules(&mut t, Some(b"ignored\n"));
        let mut moved = t.local().get("x").unwrap().clone();
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
    fn a_server_folder_the_user_deletes_from_before_the_discard_applies_stays_where_the_user_put_it()
     {
        let (mut c, mut t, mut i) = server_folder_moved_in_while_left_out();
        assert!(c.decide(&question(&c).id, Answer::Discard));
        t.remove_local("sc");
        t.settle(&Default::default());
        let mut sent = settle(&mut c, &mut t, &mut i);
        assert_eq!(sent, [("delete", "sc".to_owned())]);
        let mut trashed = t.remote().get("sc").unwrap().clone();
        trashed.trashed = true;
        trashed.meta_rev = 2;
        trashed.meta_edit_rev = 2;
        t.set_remote("sc".into(), trashed);
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
        c.received(result).unwrap();
        assert!(c.acknowledge(&mut t, 2));
        t.settle(&Default::default());
        sent.extend(settle(&mut c, &mut t, &mut i));
        assert_eq!(
            t.local().get("sf").map(|node| &node.shape),
            Some(&dir("f", "ignored")),
            "{sent:?}"
        );
        assert!(sent.contains(&("create dir", "f".to_owned())), "{sent:?}");
        assert!(sent.contains(&("move", "sf".to_owned())), "{sent:?}");
    }
}
