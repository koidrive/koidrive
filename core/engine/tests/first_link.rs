// SPDX-License-Identifier: Apache-2.0

//! Matching a first link's local entries against the server's tree (docs/design/sync-engine.md
//! section 6.4).

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::{Commits, Failure, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, Effects, Stuck, stuck};
use koidrive_engine::first_link::adopt;
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::RuleFile;
use koidrive_engine::question::{Answer, Question, QuestionKind, RecoveryGroup};
use koidrive_engine::state::CommitsState;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
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
            dev: 1,
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let node = LocalNode {
            shape: shape.clone(),
            identity,
            parked: false,
        };
        trees.set_local((*id).into(), node);
    }
    for (id, shape) in remote {
        let node = RemoteNode {
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle();
    trees
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
    trees.settle();
    assert!(trees.dirty().is_empty(), "{:?}", trees.dirty());
}

#[test]
fn a_first_link_holds_an_entry_like_one_in_the_servers_trash() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1))],
        &[("r-b", file(ROOT, "b", 2))],
    );
    let trashed = RemoteNode {
        shape: file(ROOT, "a", 1),
        trashed: true,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
    };
    trees.set_remote("r-a".into(), trashed);
    trees.settle();
    let mut commits = Commits::default();
    commits.first_link(&mut trees, &mut IdGen::new(7), NOW);
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
fn a_folder_two_pending_names_could_belong_to_is_held() {
    let mut trees = trees(
        &[("l-docs", dir(ROOT, "docs"))],
        &[
            ("r-one", dir(ROOT, "docs (pending)")),
            ("r-two", dir(ROOT, "docs (pending 2)")),
        ],
    );
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
    let Some(commit) = commits.build(trees, ids, NOW, HISTORY, 1) else {
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    assert!(commits.questions().is_empty());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-mine".to_owned())]
    );
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

/// An entry created after the first link, beside the held ones, is an ordinary create.
#[test]
fn an_entry_created_after_the_first_link_is_sent_at_once() {
    let (mut commits, mut trees, mut ids) = linked();
    let node = LocalNode {
        shape: file("r-docs", "later", 6),
        identity: FileIdentity {
            dev: 1,
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
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    assert!(commits.decide(&question(&commits).id, Answer::Publish));
    let created = [("create file", "l-notes".to_owned())];
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let head_moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&head_moved), Ok(Failure::Abandoned));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let mut commits = Commits::from_state(commits.state()).unwrap();
    renamed_create(&mut commits, &mut trees, "l-notes", "notes (conflict)", 2);
    assert_eq!(commits.questions(), []);

    let later = LocalNode {
        shape: file(ROOT, "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), later);
    trees.settle();
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-later".to_owned())]
    );
    let theirs = RemoteNode {
        shape: file(ROOT, "later", 7),
        trashed: false,
        content_rev: 3,
        meta_rev: 3,
        meta_edit_rev: 3,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 3,
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
        trees.settle();
        let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            dev: 1,
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
    trees.settle();
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
        shape: file("l-new", "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle();
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
            dev: 1,
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
        trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let sub = LocalNode {
        shape: dir("l-new", "sub"),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-sub".into(), sub);
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-sub".into();
    trees.set_local("r-notes".into(), moved);
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get("r-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("r-notes".into(), moved);
    let mut deleted = trees.remote().get("r-notes").unwrap().clone();
    deleted.trashed = true;
    trees.set_remote("r-notes".into(), deleted);
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
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
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        shape: file("l-sub", "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle();
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
            dev: 1,
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
        trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        shape: file("l-new", "inside", 5),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-inside".into(), node);
    trees.settle();
    let mut effects = Effects::default();
    let mut touched = Vec::new();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        touched.push(record.node.clone());
        let identity = FileIdentity {
            dev: 1,
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
        trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert_eq!(asked.shown.keys().collect::<Vec<_>>(), ["l-A"]);
    let pad = LocalNode {
        shape: file(ROOT, "pad", 3),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("pad".into(), pad);
    trees.settle();
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, [("create file", "pad".to_owned())]);
    let pad = RemoteNode {
        shape: file(ROOT, "pad", 3),
        trashed: false,
        content_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 2,
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
    trees.settle();
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
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
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
                shape,
                identity: FileIdentity {
                    dev: 1,
                    ino,
                    birth_ns: Some(0),
                },
                parked: false,
            },
        );
    }
    let remote = |shape: Shape| RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
    };
    trees.set_remote("p".into(), remote(dir(ROOT, "a")));
    trees.set_remote("c".into(), remote(dir("p", "child")));
    trees.settle();
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
        1,
    );
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.waiting().get("c"), Some(&waiting));
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
            dev: 1,
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let local = LocalNode {
            shape: shape.clone(),
            identity,
            parked: false,
        };
        trees.set_local((*id).into(), local);
    }
    for (id, shape) in remote {
        let node = RemoteNode {
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle();
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
    trees.settle();
    assert!(trees.synced().get("l-b").is_none());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, None);
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 200.. {
        trees.settle();
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
            dev: 1,
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
        trees.settle();
        assert_eq!(ops(commits, trees, ids), [], "{done:?}");
        let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            dev: 1,
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
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
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
    trees.settle();
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
    commits.first_link(&mut trees, &mut ids_gen, NOW);
    trees.settle();
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
        dev: 1,
        ino: 99,
        birth_ns: Some(0),
    };
    let later = LocalNode {
        shape: file("l-new", "later", 6),
        identity,
        parked: false,
    };
    trees.set_local("l-later".into(), later);
    let mut effects = Effects::default();
    for ino in 100.. {
        trees.settle();
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
        shape,
        trashed: false,
        content_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 2,
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
    trees.settle();
    let sent = commits.build(&mut trees, &mut ids, NOW, HISTORY, 2);
    assert!(sent.is_none(), "{sent:?}");
}

#[test]
fn a_file_the_user_took_back_from_a_discard_is_sent_again_once_the_server_loses_it() {
    let (mut commits, mut trees, mut ids) = linked();
    assert!(commits.decide(&question(&commits).id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let edit = |trees: &mut Trees, digest| {
        let mut node = trees.local().get("l-b").unwrap().clone();
        node.shape.content = Some(content(digest));
        trees.set_local("l-b".into(), node);
        trees.settle();
    };
    let applied = |commits: &mut Commits, trees: &mut Trees, seq, digest| {
        let node = RemoteNode {
            shape: file("r-docs", "b", digest),
            trashed: false,
            content_rev: seq,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: seq,
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
        let commit = commits.build(trees, ids, NOW, HISTORY, base);
        (commit.into_iter().flat_map(|commit| &commit.ops))
            .map(|op| match &op.op {
                Some(op::Op::CreateFile(create)) => ("create file", create.node_id.clone()),
                Some(op::Op::UpdateContent(update)) => ("update", update.node_id.clone()),
                other => panic!("unexpected op {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    edit(&mut trees, 7);
    assert_eq!(sent(&mut commits, &mut trees, &mut ids, 1), []);
    let again = question(&commits);
    assert!(again.answer.is_none());
    assert!(commits.decide(&again.id, Answer::Publish));
    assert_eq!(
        sent(&mut commits, &mut trees, &mut ids, 1),
        [("create file", "l-b".into())]
    );
    applied(&mut commits, &mut trees, 2, 7);
    edit(&mut trees, 2);
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
        0,
        0,
    );
    trees.settle();
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
