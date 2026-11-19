// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::{Class, Trees};
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{BatchLimit, Effects};
use koidrive_engine::guard::{WINDOW_MS, Window};
use koidrive_engine::ids::{IdGen, uuid_v7};
use koidrive_engine::question::{Answer, Held, Limit, Question, QuestionKind, RecoveryGroup};
use koidrive_engine::state::CommitsState;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{CommitResult, OpResult, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const SEQ: u64 = 2;

fn shape(parent: &str, name: &str, kind: Kind) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind,
        content: (kind == Kind::File).then(|| Content {
            size: 3,
            manifest_digest: vec![1; 32],
            ..Default::default()
        }),
    }
}

fn space(loose: usize, inside: usize) -> Trees {
    let mut trees = Trees::new(ROOT);
    let mut nodes = vec![("d".to_owned(), shape(ROOT, "d", Kind::Dir))];
    nodes.extend((0..loose).map(|i| (format!("f{i}"), shape(ROOT, &format!("f{i}"), Kind::File))));
    nodes
        .extend((0..inside).map(|i| (format!("d/g{i}"), shape("d", &format!("g{i}"), Kind::File))));
    for (ino, (id, shape)) in nodes.into_iter().enumerate() {
        put_back(&mut trees, &id, shape.clone(), ino as u128 + 1);
        trees.set_synced(id.clone(), Some(shape.clone()));
        serve(&mut trees, id, shape);
    }
    trees.settle(&Default::default());
    trees
}

fn serve(trees: &mut Trees, id: String, shape: Shape) {
    trees.set_remote(
        id,
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
        },
    );
}

fn put_back(trees: &mut Trees, id: &str, shape: Shape, ino: u128) {
    let identity = FileIdentity {
        ino,
        birth_ns: Some(0),
        handle: None,
    };
    trees.set_local(
        id.into(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape,
            identity,
            parked: false,
        },
    );
}

fn remove(trees: &mut Trees, ids: &[&str]) {
    for id in ids {
        let below: Vec<String> = (trees.local().iter())
            .filter(|(other, _)| other.starts_with(&format!("{id}/")))
            .map(|(other, _)| other.clone())
            .collect();
        for other in below {
            trees.remove_local(&other);
        }
        trees.remove_local(id);
    }
    trees.settle(&Default::default());
}

fn root_files(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("f{i}")).collect()
}

fn build_at(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen, now: u64) -> Vec<op::Op> {
    let commit = commits.build(trees, ids, now, "history", "me", 7).cloned();
    trees.settle(commits.aliases());
    commit.map_or_else(Vec::new, |commit| {
        commit.ops.into_iter().map(|op| op.op.unwrap()).collect()
    })
}

fn deleted(ops: &[op::Op]) -> Vec<String> {
    let mut deleted: Vec<String> = (ops.iter())
        .filter_map(|op| match op {
            op::Op::Delete(delete) => Some(delete.node_id.clone()),
            _ => None,
        })
        .collect();
    deleted.sort();
    deleted
}

fn acknowledge(commits: &mut Commits, trees: &mut Trees, seq: u64) {
    let ops = commits.in_flight().unwrap().commit.ops.len();
    let ok = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    commits
        .received(CommitResult {
            seq,
            results: vec![ok; ops],
            changes: vec![],
            ..Default::default()
        })
        .unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle(commits.aliases());
}

fn asked(commits: &Commits) -> Option<&Question> {
    match commits.questions() {
        [] => None,
        [question] => Some(question),
        more => panic!("one question at most: {more:?}"),
    }
}

fn window(question: &Question) -> Window {
    let QuestionKind::MassDelete {
        deletions,
        affected_files,
        space_files,
        ..
    } = question.kind
    else {
        panic!("a mass-delete question: {question:?}");
    };
    Window {
        deletions,
        affected_files,
        space_files,
    }
}

fn exceeded(question: &Question) -> &[Limit] {
    match &question.kind {
        QuestionKind::MassDelete { exceeded, .. } => exceeded,
        _ => panic!("a mass-delete question: {question:?}"),
    }
}

fn held(commits: &Commits) -> Vec<String> {
    asked(commits)
        .map(|question| question.held.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn a_removed_folder_holding_most_of_the_space_is_held_while_the_rest_syncs() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    put_back(&mut trees, "new", shape(ROOT, "new", Kind::File), 99);
    trees.settle(commits.aliases());

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), Vec::<String>::new(), "nothing is deleted");
    assert!(
        matches!(&ops[..], [op::Op::CreateFile(create)] if create.node_id == "new"),
        "the new file is still uploaded: {ops:?}"
    );
    let question = asked(&commits).expect("a question");
    assert_eq!(exceeded(question), [Limit::Share]);
    assert_eq!(
        window(question),
        Window {
            deletions: 1,
            affected_files: 15,
            space_files: 25,
        },
        "a folder is one deletion of the files it holds"
    );
    assert_eq!(held(&commits), ["d"]);
    assert_eq!(question.held["d"].members.len(), 16);
}

#[test]
fn deletions_below_the_limits_are_committed() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["f0", "f1", "f2"]);

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), root_files(0..3));
    assert_eq!(asked(&commits), None);
}

#[test]
fn half_of_the_space_is_not_more_than_half() {
    let mut trees = space(20, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let half = root_files(0..10);
    remove(
        &mut trees,
        &half.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops).len(), 10);
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_space_of_fewer_than_20_files_may_lose_all_of_them() {
    let mut trees = space(19, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let all = root_files(0..19);
    remove(
        &mut trees,
        &all.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops).len(), 19);
    assert_eq!(asked(&commits), None);
}

#[test]
fn more_than_1000_deletions_are_held_in_a_large_space_and_1000_are_not() {
    let names = root_files(0..1001);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut trees = space(5000, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &names[..1000]);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(deleted(&ops).len(), 1000);
    assert_eq!(asked(&commits), None);

    let mut trees = space(5000, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &names);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(deleted(&ops), Vec::<String>::new());
    let question = asked(&commits).expect("a question");
    assert_eq!(exceeded(question), [Limit::Deletions]);
    assert_eq!(question.held.len(), 1001, "every pending deletion is held");
}

fn ten_deleted_then_three() -> (Trees, Commits, IdGen) {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = root_files(0..10);
    remove(
        &mut trees,
        &first.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW)).len(),
        10
    );
    acknowledge(&mut commits, &mut trees, 1);
    remove(&mut trees, &["f10", "f11", "f12"]);
    (trees, commits, ids)
}

#[test]
fn deletions_add_up_within_the_window() {
    let (mut trees, mut commits, mut ids) = ten_deleted_then_three();

    let late = NOW + WINDOW_MS - 1;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, late)),
        Vec::<String>::new()
    );
    let question = asked(&commits).expect("13 of 25 files in the window");
    assert_eq!(
        window(question),
        Window {
            deletions: 13,
            affected_files: 13,
            space_files: 25,
        },
        "the space is counted as it was before the window's deletions"
    );
    let id = question.id.clone();

    let much_later = NOW + 10 * WINDOW_MS;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, much_later)),
        Vec::<String>::new()
    );
    assert_eq!(
        asked(&commits).unwrap().id,
        id,
        "a question stays until it is answered"
    );
}

fn make(trees: &mut Trees, made_at: u64, in_folder: bool, ino: u128) -> Vec<(String, Shape)> {
    let id = uuid_v7(made_at, ino);
    let name = format!("n{}", ino % 3);
    let made = match in_folder {
        true => vec![
            (id.clone(), shape(ROOT, &name, Kind::Dir)),
            (uuid_v7(made_at, ino + 1), shape(&id, "x", Kind::File)),
        ],
        false => vec![(id, shape(ROOT, &name, Kind::File))],
    };
    for (at, (id, shape)) in made.iter().enumerate() {
        put_back(trees, id, shape.clone(), ino + at as u128);
    }
    trees.settle(&Default::default());
    made
}

fn make_and_delete(
    (trees, commits, ids): (&mut Trees, &mut Commits, &mut IdGen),
    cycle: u64,
    made_at: u64,
    in_folder: bool,
) -> (String, Vec<String>) {
    let (now, seq) = (NOW + cycle, SEQ + 2 * cycle);
    let made = make(trees, made_at, in_folder, 1000 + 2 * u128::from(cycle));
    assert!(!build_at(commits, trees, ids, now).is_empty());
    acknowledge(commits, trees, seq);
    for (id, shape) in &made {
        serve(trees, id.clone(), shape.clone());
    }
    remove(
        trees,
        &made.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
    );
    let deleted = deleted(&build_at(commits, trees, ids, now));
    if commits.in_flight().is_some() {
        acknowledge(commits, trees, seq + 1);
    }
    (made[0].0.clone(), deleted)
}

#[test]
fn files_made_and_deleted_over_and_over_are_never_held() {
    for (loose, in_folder) in [(0, false), (0, true), (25, false), (25, true)] {
        let mut trees = space(loose, 0);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        for cycle in 0..60 {
            let state = (&mut trees, &mut commits, &mut ids);
            let (top, deleted) = make_and_delete(state, cycle, NOW + cycle, in_folder);

            let case = format!("cycle {cycle} of {loose} files, in a folder: {in_folder}");
            assert_eq!(deleted, [top], "{case}");
            assert_eq!(asked(&commits), None, "{case}");
        }
    }
}

#[test]
fn files_whose_id_time_is_after_now_count_as_old() {
    let mut trees = space(0, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let later = NOW + 2 * WINDOW_MS;
    for cycle in 0..19 {
        let (top, deleted) =
            make_and_delete((&mut trees, &mut commits, &mut ids), cycle, later, false);
        assert_eq!(deleted, [top]);
    }

    let (_, deleted) = make_and_delete((&mut trees, &mut commits, &mut ids), 19, later, false);

    assert_eq!(deleted, Vec::<String>::new());
    assert_eq!(
        exceeded(asked(&commits).expect("20 of 20 files")),
        [Limit::Share]
    );
}

#[test]
fn new_files_count_while_their_deletion_waits() {
    let mut trees = space(0, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let made: Vec<String> = (0..30).map(|i| uuid_v7(NOW, i)).collect();
    for (ino, id) in made.iter().enumerate() {
        put_back(
            &mut trees,
            id,
            shape(ROOT, id, Kind::File),
            1000 + ino as u128,
        );
    }
    trees.settle(commits.aliases());
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    acknowledge(&mut commits, &mut trees, SEQ);
    for id in &made {
        serve(&mut trees, id.clone(), shape(ROOT, id, Kind::File));
    }
    remove(
        &mut trees,
        &made.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW)),
        Vec::<String>::new()
    );

    let question = asked(&commits).expect("30 of 30 files");
    assert_eq!(
        window(question),
        Window {
            deletions: 30,
            affected_files: 30,
            space_files: 30,
        }
    );
}

#[test]
fn deletions_after_the_window_count_on_their_own() {
    let (mut trees, mut commits, mut ids) = ten_deleted_then_three();

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);

    assert_eq!(deleted(&ops), root_files(10..13));
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_deletion_planned_again_counts_once() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let some = root_files(0..12);
    remove(
        &mut trees,
        &some.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW)).len(),
        12
    );

    let forgotten = CommitsState {
        in_flight: None,
        ..commits.state()
    };
    let mut commits = Commits::from_state(forgotten).unwrap();
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops).len(), 12);
    assert_eq!(asked(&commits), None);
    assert_eq!(commits.state().guard.ledger.len(), 12);
}

#[test]
fn a_clock_that_moved_back_keeps_deletions_in_the_window_longer() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = root_files(0..10);
    remove(
        &mut trees,
        &first.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);
    acknowledge(&mut commits, &mut trees, 1);

    remove(&mut trees, &["f10", "f11", "f12"]);
    let late = NOW + WINDOW_MS - 1;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, late)),
        Vec::<String>::new()
    );
    assert!(
        asked(&commits).is_some(),
        "counted as made at the earlier time"
    );
}

#[test]
fn deleting_answers_the_question_and_commits_without_counting_again() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(commits.decide(&id, Answer::Delete));
    assert!(!commits.decide(&id, Answer::Delete), "answered once");
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), ["d"]);
    assert_eq!(asked(&commits), None);
    acknowledge(&mut commits, &mut trees, 1);

    remove(&mut trees, &["f0"]);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 2);
    assert_eq!(
        deleted(&ops),
        ["f0"],
        "the confirmed deletion does not count"
    );
    assert!(
        commits.state().guard.confirmed.is_empty(),
        "done once acknowledged"
    );
}

#[test]
fn restoring_answers_the_question_and_downloads_the_held_nodes_again() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(commits.decide(&id, Answer::Restore));
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW + 1)),
        Vec::<String>::new()
    );

    for node in ["d", "d/g0", "d/g14"] {
        assert_eq!(trees.synced().live(node), None, "{node} left S");
        assert_eq!(trees.class(node), Class::Remote, "{node} is downloaded");
    }
    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

#[test]
fn more_deletions_replace_the_question_and_an_answer_to_the_old_one_does_nothing() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(held(&commits), ["d", "f0"]);
    assert_ne!(asked(&commits).unwrap().id, first);
    assert!(!commits.decide(&first, Answer::Delete));
    assert_eq!(held(&commits), ["d", "f0"]);
}

#[test]
fn putting_the_files_back_ends_the_question() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert!(asked(&commits).is_some());

    put_back(&mut trees, "d", shape(ROOT, "d", Kind::Dir), 1);
    for i in 0..15 {
        put_back(
            &mut trees,
            &format!("d/g{i}"),
            shape("d", &format!("g{i}"), Kind::File),
            100 + i,
        );
    }
    trees.settle(commits.aliases());
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

#[test]
fn a_held_file_changed_on_the_server_leaves_the_question_and_is_downloaded() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(
        &mut trees,
        &[
            "f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "d/g0", "d/g1", "d/g2",
        ],
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(held(&commits).len(), 13);

    let mut edited = shape(ROOT, "f0", Kind::File);
    edited.content.as_mut().unwrap().manifest_digest = vec![2; 32];
    trees.set_remote(
        "f0".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: edited,
            trashed: false,
            content_rev: 2,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 2,
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle(commits.aliases());
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    let question = asked(&commits).expect("12 of 25 files are still held");
    assert!(!question.held.contains_key("f0"));
    assert_eq!(question.held.len(), 12);
    assert_ne!(
        trees.class("f0"),
        Class::Local,
        "f0 is not a local delete any more"
    );
}

#[test]
fn nothing_is_counted_while_committing_has_stopped() {
    let mut trees = space(10, 15);
    let mut commits = Commits::from_state(CommitsState {
        halted: Some(koidrive_engine::commit::Halt::Recovery {
            cause: koidrive_engine::question::RecoveryCause::of(
                koidrive_engine::question::RecoveryTrigger::StoreRolledBack,
            ),
            policy: None,
        }),
        ..Default::default()
    })
    .unwrap();
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut IdGen::new(1), NOW);
    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

fn recovery_question(trees: &Trees) -> CommitsState {
    let members = (trees.synced().iter())
        .filter(|(id, _)| id.as_str() == "d" || id.starts_with("d/"))
        .map(|(id, _)| (id.clone(), None))
        .collect();
    CommitsState {
        questions: vec![Question {
            id: "other".into(),
            asked_at_ms: NOW,
            kind: QuestionKind::Recovery {
                group: RecoveryGroup::ChangedOnThisDevice,
                sent_in: None,
                cause: None,
            },
            held: [("d".into(), Held { members, files: 15 })].into(),
            answer: None,
            shown: Default::default(),
        }],
        ..Default::default()
    }
}

#[test]
fn nodes_an_open_question_of_any_kind_holds_get_no_op_and_are_not_counted() {
    let mut trees = space(10, 15);
    let mut commits = Commits::from_state(recovery_question(&trees)).unwrap();
    let mut ids = IdGen::new(1);
    remove(&mut trees, &["d", "f0"]);

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), ["f0"], "only the deletion no question holds");
    assert_eq!(commits.questions().len(), 1, "the guard does not ask");
    assert_eq!(commits.state().guard.ledger.len(), 1);
}

#[test]
fn an_answer_a_recovery_question_does_not_offer_changes_nothing() {
    let trees = space(10, 15);
    let state = recovery_question(&trees);
    let mut commits = Commits::from_state(state.clone()).unwrap();

    assert!(!commits.decide("other", Answer::Delete));
    assert!(!commits.decide("other", Answer::Keep));
    assert_eq!(commits.state(), state);
}

#[test]
fn an_answer_the_kind_does_not_offer_changes_nothing() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(!commits.decide(&id, Answer::Publish));
    assert_eq!(held(&commits), ["d"]);
}

#[test]
fn a_held_node_carries_the_revisions_r_held_when_it_was_asked() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);

    let members = &asked(&commits).unwrap().held["d"].members;
    assert!(
        members.values().all(|revisions| revisions
            .is_some_and(|revisions| revisions.content_rev == 1 && revisions.meta_rev == 1)),
        "{members:?}"
    );
}

#[test]
fn an_unanswered_question_keeps_holding_what_is_left_when_it_shrinks() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let gone = root_files(0..13);
    remove(
        &mut trees,
        &gone.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    for i in 0..7 {
        put_back(
            &mut trees,
            &format!("f{i}"),
            shape(ROOT, &format!("f{i}"), Kind::File),
            100 + i as u128,
        );
    }
    trees.settle(commits.aliases());
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(
        deleted(&ops),
        Vec::<String>::new(),
        "6 of 25 files are still held"
    );
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(window(question).exceeded(), []);
    assert_eq!(
        exceeded(question),
        [Limit::Share],
        "the new question keeps the limit the first one crossed"
    );
    let mut left = root_files(7..13);
    left.sort();
    assert_eq!(held(&commits), left);
}

#[test]
fn a_delete_answer_covers_only_the_deletions_its_question_listed() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    assert!(commits.decide(&first, Answer::Delete));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(
        deleted(&ops),
        Vec::<String>::new(),
        "the answer was about d alone"
    );
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(question.answer, None, "the old answer is dropped");
    assert_eq!(held(&commits), ["d", "f0"]);
}

#[test]
fn a_confirmed_folder_deleted_over_several_commits_takes_one_answer() {
    let mut trees = space(10, 15);
    let state = CommitsState {
        cap: Some(1),
        split: ["d".into()].into(),
        ..Default::default()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();
    assert!(commits.decide(&id, Answer::Delete));

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);
    assert_eq!(deleted(&ops).len(), 1, "one op per commit");
    for node in deleted(&ops) {
        let mut remote = trees.remote().get(&node).unwrap().clone();
        remote.trashed = true;
        trees.set_remote(node, remote);
    }
    trees.settle(commits.aliases());
    acknowledge(&mut commits, &mut trees, 8);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 2);

    assert_eq!(asked(&commits), None, "the rest of d is still confirmed");
    let next = deleted(&ops);
    assert!(
        !next.is_empty() && next.iter().all(|node| node.starts_with("d/")),
        "{next:?}"
    );
}

#[test]
fn a_restore_answer_brings_back_its_question_and_holds_a_deletion_made_since() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    assert!(commits.decide(&first, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    assert_eq!(trees.class("d/g0"), Class::Remote, "d is downloaded");
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(held(&commits), ["f0"]);
}

#[test]
fn a_restore_answer_brings_back_held_children_whose_folder_was_put_back() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    put_back(&mut trees, "d", shape(ROOT, "d", Kind::Dir), 100);
    trees.settle(commits.aliases());
    assert!(commits.decide(&id, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    for node in ["d/g0", "d/g14"] {
        assert_eq!(trees.class(node), Class::Remote, "{node} is downloaded");
    }
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_restore_answer_holds_a_node_deleted_under_its_folder_since() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    let added = shape("d", "new", Kind::File);
    trees.set_synced("d/new".into(), Some(added.clone()));
    trees.set_remote(
        "d/new".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: added,
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
    trees.settle(commits.aliases());
    assert!(commits.decide(&id, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    assert_eq!(held(&commits), ["d/new"], "the answer did not cover d/new");
}

#[test]
fn a_confirmed_folder_whose_rest_changed_after_some_commits_is_asked_about_again() {
    let mut trees = space(10, 15);
    let state = CommitsState {
        cap: Some(1),
        split: ["d".into()].into(),
        ..Default::default()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();
    assert!(commits.decide(&id, Answer::Delete));
    for step in 0..3 {
        let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1 + step);
        assert!(!deleted(&ops).is_empty());
        for node in deleted(&ops) {
            let mut remote = trees.remote().get(&node).unwrap().clone();
            remote.trashed = true;
            trees.set_remote(node, remote);
        }
        trees.settle(commits.aliases());
        acknowledge(&mut commits, &mut trees, 8 + step);
    }
    let left: Vec<String> = (trees.synced().iter())
        .filter(|(id, node)| id.starts_with("d/") && node.kind == Kind::File)
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(left.len(), 8, "8 of 18 files are left, below the limits");

    let mut remote = trees.remote().get(&left[0]).unwrap().clone();
    remote.meta_rev += 1;
    trees.set_remote(left[0].clone(), remote);
    trees.settle(commits.aliases());
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 5);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    let question = asked(&commits).expect("the rest of d is held again");
    let QuestionKind::MassDelete { exceeded, .. } = &question.kind else {
        panic!("a mass-delete question: {question:?}");
    };
    assert_eq!(exceeded, &[Limit::Share], "the limit the person confirmed");
}

fn trash_remotely(trees: &mut Trees, ids: &[&str]) {
    let gone: Vec<(String, Shape, &str)> = (trees.synced().iter())
        .filter(|(id, _)| trees.remote().live(id).is_some())
        .filter_map(|(id, shape)| {
            let top = (ids.iter()).find(|top| *id == **top || id.starts_with(&format!("{top}/")));
            top.map(|top| (id.clone(), shape.clone(), *top))
        })
        .collect();
    for (id, shape, top) in gone {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape,
            trashed: true,
            content_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: top.into(),
            version_id: String::new(),
            edit_rev: 2,
            pending_for: None,
            pending_by: None,
        };
        trees.set_remote(id, node);
    }
    trees.settle(&Default::default());
}

fn apply_effects(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen, now: u64) {
    let mut effects = Effects::default();
    while let Some(record) = effects
        .plan(trees, commits.in_flight(), ids, now, BatchLimit::ONE)
        .first()
        .cloned()
    {
        if let Some(removal) = effects
            .done(trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, trees, SEQ, ids, now);
        }
        trees.settle(commits.aliases());
    }
}

fn notice(commits: &Commits) -> Option<(&Question, Window, &[Limit])> {
    commits
        .questions()
        .iter()
        .find_map(|question| match &question.kind {
            QuestionKind::RemoteMassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } => {
                let window = Window {
                    deletions: *deletions,
                    affected_files: *affected_files,
                    space_files: *space_files,
                };
                Some((question, window, &exceeded[..]))
            }
            _ => None,
        })
}

#[test]
fn another_device_making_and_deleting_files_over_and_over_opens_no_notice() {
    let mut trees = space(0, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    for cycle in 0..60 {
        let now = NOW + cycle;
        let id = uuid_v7(now, u128::from(cycle));
        let made = shape(ROOT, "a", Kind::File);
        put_back(&mut trees, &id, made.clone(), 1000 + u128::from(cycle));
        trees.set_synced(id.clone(), Some(made.clone()));
        serve(&mut trees, id.clone(), made);
        trees.settle(commits.aliases());
        trash_remotely(&mut trees, &[&id]);

        apply_effects(&mut commits, &mut trees, &mut ids, now);

        assert_eq!(trees.local().get(&id), None, "cycle {cycle}");
        assert_eq!(
            notice(&commits).map(|(_, window, _)| window),
            None,
            "cycle {cycle}"
        );
    }
}

#[test]
fn another_devices_deletion_of_most_of_the_space_is_applied_and_noticed() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);

    apply_effects(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(trees.local().get("d"), None, "the folder is removed");
    assert_eq!(commits.questions().len(), 1);
    let (question, window, exceeded) = notice(&commits).expect("a notice");
    assert_eq!(
        window,
        Window {
            deletions: 1,
            affected_files: 15,
            space_files: 25,
        },
        "the folder counts as one deletion"
    );
    assert_eq!(exceeded, [Limit::Share]);
    assert!(question.held.is_empty(), "a notice holds nothing");
    assert_eq!(question.kind.answers(), [Answer::Dismiss]);
}

#[test]
fn an_open_notice_counts_every_removal_until_it_is_dismissed() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW);
    let first = notice(&commits).unwrap().0.id.clone();

    trash_remotely(&mut trees, &["f0", "f1", "f2", "f3", "f4"]);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);

    let (question, window, exceeded) = notice(&commits).expect("the notice");
    assert_eq!(question.id, first, "the same notice");
    assert_eq!(
        window,
        Window {
            deletions: 6,
            affected_files: 20,
            space_files: 25,
        },
        "the folder still counts after its window ended"
    );
    assert_eq!(exceeded, [Limit::Share]);
}

#[test]
fn dismissing_the_notice_closes_it_and_counting_starts_again() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW);
    let id = notice(&commits).unwrap().0.id.clone();

    assert!(
        !commits.decide(&id, Answer::Delete),
        "a notice takes only dismiss"
    );
    assert!(commits.decide(&id, Answer::Dismiss));
    assert_eq!(commits.questions(), []);
    assert!(!commits.decide(&id, Answer::Dismiss), "the notice is gone");

    trash_remotely(&mut trees, &["f0"]);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);
    assert_eq!(commits.questions(), [], "1 of 10 files crosses no limit");
}

#[test]
fn the_rest_of_a_folder_removed_after_its_notice_was_dismissed_is_not_noticed_again() {
    let mut trees = space(30, 50);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    let mut effects = Effects::default();
    while notice(&commits).is_none() {
        let record = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
            .expect("a removal");
        if let Some(removal) = effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, &mut trees, SEQ, &mut ids, NOW);
        }
        trees.settle(commits.aliases());
    }
    let (question, window, _) = notice(&commits).unwrap();
    assert!(
        window.affected_files < 50,
        "the folder is still being removed"
    );
    let id = question.id.clone();
    assert!(commits.decide(&id, Answer::Dismiss));

    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(trees.local().get("d"), None, "the folder is removed");
    assert_eq!(
        commits.questions(),
        [],
        "the same deletion is not noticed again"
    );

    let later = root_files(0..20);
    trash_remotely(
        &mut trees,
        &later.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 2);
    let (_, window, exceeded) = notice(&commits).expect("a new deletion is counted");
    assert_eq!(window.deletions, 20);
    assert_eq!(exceeded, [Limit::Share]);
}

#[test]
fn a_folder_restored_after_its_notice_was_dismissed_is_noticed_when_deleted_again() {
    let mut trees = space(0, 50);
    let original: Vec<(String, Shape)> = (trees.synced().iter())
        .map(|(id, shape)| (id.clone(), shape.clone()))
        .collect();
    let set_remote = |trees: &mut Trees, trashed: bool, rev: u64| {
        for (id, shape) in &original {
            let node = RemoteNode {
                made_over: Vec::new(),
                shape: shape.clone(),
                trashed,
                content_rev: rev,
                meta_rev: rev,
                meta_edit_rev: rev,
                version_id: String::new(),
                trash_group: if trashed {
                    "later".into()
                } else {
                    String::new()
                },
                edit_rev: rev,
                pending_for: None,
                pending_by: None,
            };
            trees.set_remote(id.clone(), node);
        }
        trees.settle(&Default::default());
    };
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(1), Effects::default());
    trash_remotely(&mut trees, &["d"]);
    while notice(&commits).is_none() {
        let record = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
            .expect("a removal");
        if let Some(removal) = effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, &mut trees, SEQ, &mut ids, NOW);
        }
        trees.settle(commits.aliases());
    }
    let first = notice(&commits).unwrap().0.id.clone();
    assert!(commits.decide(&first, Answer::Dismiss));

    set_remote(&mut trees, false, 3);
    let mut ino = 1000;
    while let Some(record) = effects
        .plan(
            &trees,
            commits.in_flight(),
            &mut ids,
            NOW + 1,
            BatchLimit::ONE,
        )
        .first()
        .cloned()
    {
        ino += 1;
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        if let Some(removal) = (effects.done(
            &mut trees,
            commits.in_flight(),
            &record.effect_id,
            Some(identity),
        ))
        .unwrap()
        {
            commits.count_remote(removal, &mut trees, SEQ, &mut ids, NOW + 1);
        }
        trees.settle(commits.aliases());
    }
    let files = (trees.local().iter()).filter(|(_, node)| node.shape.kind == Kind::File);
    assert_eq!(files.count(), 50, "the restored folder is downloaded again");
    assert_eq!(notice(&commits), None);

    set_remote(&mut trees, true, 4);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 2);

    let (next, window, _) = notice(&commits).expect("the later deletion is noticed");
    assert_ne!(next.id, first);
    assert_eq!((window.deletions, window.affected_files), (1, 50));
}

#[test]
fn an_open_notice_holds_back_neither_uploads_nor_local_deletions() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW);
    let id = notice(&commits).unwrap().0.id.clone();
    put_back(&mut trees, "new", shape(ROOT, "new", Kind::File), 999);
    trees.remove_local("f0");
    trees.settle(commits.aliases());

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert!(
        ops.iter()
            .any(|op| matches!(op, op::Op::CreateFile(create) if create.node_id == "new")),
        "{ops:?}"
    );
    assert_eq!(deleted(&ops), ["f0"]);
    assert_eq!(notice(&commits).unwrap().0.id, id);
}

#[test]
fn a_dismissed_folder_stays_dismissed_when_r_forgets_its_trashed_nodes() {
    let mut trees = space(30, 50);
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(1), Effects::default());
    trash_remotely(&mut trees, &["d"]);
    while notice(&commits).is_none() {
        let record = effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
            .expect("a removal");
        if let Some(removal) = effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, &mut trees, SEQ, &mut ids, NOW);
        }
        trees.settle(commits.aliases());
    }
    let id = notice(&commits).unwrap().0.id.clone();
    assert!(commits.decide(&id, Answer::Dismiss));
    let trashed: Vec<String> = (trees.remote().iter())
        .filter(|(_, node)| node.trashed)
        .map(|(id, _)| id.clone())
        .collect();
    for id in &trashed {
        trees.remove_remote(id);
    }
    trees.settle(commits.aliases());

    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(trees.local().get("d"), None, "the folder is removed");
    assert_eq!(
        commits.questions(),
        [],
        "the same deletion is not noticed again"
    );
}

fn forget_trashed(trees: &mut Trees) -> Vec<(String, RemoteNode)> {
    let trashed: Vec<(String, RemoteNode)> = (trees.remote().iter())
        .filter(|(_, node)| node.trashed)
        .map(|(id, node)| (id.clone(), node.clone()))
        .collect();
    for (id, _) in &trashed {
        trees.remove_remote(id);
    }
    trees.settle(&Default::default());
    trashed
}

fn dismiss_while_removing(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) {
    let mut effects = Effects::default();
    while notice(commits).is_none() {
        let record = (effects
            .plan(trees, commits.in_flight(), ids, NOW, BatchLimit::ONE)
            .first()
            .cloned())
        .expect("a removal");
        if let Some(removal) = effects
            .done(trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, trees, SEQ, ids, NOW);
        }
        trees.settle(commits.aliases());
    }
    let (question, window, _) = notice(commits).unwrap();
    assert!(
        window.affected_files < 50,
        "the folder is still being removed"
    );
    let id = question.id.clone();
    assert!(commits.decide(&id, Answer::Dismiss));
}

#[test]
fn a_dismissed_folder_stays_dismissed_when_r_learns_its_trashed_nodes() {
    let mut trees = space(0, 50);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    let trashed = forget_trashed(&mut trees);
    dismiss_while_removing(&mut commits, &mut trees, &mut ids);
    for (id, node) in trashed {
        trees.set_remote(id, node);
    }
    trees.settle(commits.aliases());

    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(trees.local().get("d"), None, "the folder is removed");
    assert_eq!(
        commits.questions(),
        [],
        "the same deletion is not noticed again"
    );
}

#[test]
fn a_folder_trashed_again_after_a_dismissal_without_its_record_is_noticed() {
    let mut trees = space(0, 50);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    let trashed = forget_trashed(&mut trees);
    dismiss_while_removing(&mut commits, &mut trees, &mut ids);
    let left = (trees.local().iter()).filter(|(_, node)| node.shape.kind == Kind::File);
    let left = left.count() as u64;
    for (id, node) in trashed {
        let node = RemoteNode {
            meta_rev: 4,
            meta_edit_rev: 4,
            trash_group: "again".into(),
            ..node
        };
        trees.set_remote(id, node);
    }
    trees.settle(commits.aliases());

    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);

    let (_, window, _) = notice(&commits).expect("the later deletion is noticed");
    assert_eq!((window.deletions, window.affected_files), (1, left));
}

#[test]
fn a_folder_restored_after_a_dismissal_without_its_record_is_noticed_when_deleted_again() {
    let mut trees = space(0, 50);
    let original: Vec<(String, Shape)> = (trees.synced().iter())
        .map(|(id, shape)| (id.clone(), shape.clone()))
        .collect();
    let set_remote = |trees: &mut Trees, trashed: bool, rev: u64| {
        for (id, shape) in &original {
            let node = RemoteNode {
                made_over: Vec::new(),
                shape: shape.clone(),
                trashed,
                content_rev: rev,
                meta_rev: rev,
                meta_edit_rev: rev,
                version_id: String::new(),
                trash_group: if trashed {
                    "later".into()
                } else {
                    String::new()
                },
                edit_rev: rev,
                pending_for: None,
                pending_by: None,
            };
            trees.set_remote(id.clone(), node);
        }
        trees.settle(&Default::default());
    };
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    forget_trashed(&mut trees);
    dismiss_while_removing(&mut commits, &mut trees, &mut ids);

    set_remote(&mut trees, false, 3);
    let mut effects = Effects::default();
    let mut ino = 1000;
    while let Some(record) = effects
        .plan(
            &trees,
            commits.in_flight(),
            &mut ids,
            NOW + 1,
            BatchLimit::ONE,
        )
        .first()
        .cloned()
    {
        ino += 1;
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        if let Some(removal) = (effects.done(
            &mut trees,
            commits.in_flight(),
            &record.effect_id,
            Some(identity),
        ))
        .unwrap()
        {
            commits.count_remote(removal, &mut trees, 3, &mut ids, NOW + 1);
        }
        trees.settle(commits.aliases());
    }
    let files = (trees.local().iter()).filter(|(_, node)| node.shape.kind == Kind::File);
    assert_eq!(files.count(), 50, "the restored folder is downloaded again");
    assert_eq!(notice(&commits), None);

    set_remote(&mut trees, true, 4);
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 2);

    let (_, window, _) = notice(&commits).expect("the later deletion is noticed");
    assert_eq!((window.deletions, window.affected_files), (1, 50));
}

fn nested() -> Trees {
    let mut nodes = vec![
        ("p".to_owned(), shape(ROOT, "p", Kind::Dir)),
        ("p/f".to_owned(), shape("p", "f", Kind::Dir)),
    ];
    nodes.extend((0..10).map(|i| (format!("r{i}"), shape(ROOT, &format!("r{i}"), Kind::File))));
    nodes.extend((0..6).map(|i| (format!("p/q{i}"), shape("p", &format!("q{i}"), Kind::File))));
    let inner = (0..40).map(|i| {
        (
            format!("p/f/g{i}"),
            shape("p/f", &format!("g{i}"), Kind::File),
        )
    });
    nodes.extend(inner);
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in nodes.into_iter().enumerate() {
        put_back(&mut trees, &id, shape.clone(), ino as u128 + 1);
        trees.set_synced(id.clone(), Some(shape.clone()));
        let node = RemoteNode {
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
        };
        trees.set_remote(id, node);
    }
    trees.settle(&Default::default());
    trees
}

#[test]
fn a_folder_deleted_above_a_dismissed_one_counts_only_its_own_files() {
    let mut trees = nested();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["p/f"]);
    dismiss_while_removing(&mut commits, &mut trees, &mut ids);
    trash_remotely(&mut trees, &["p"]);

    apply_effects(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(trees.local().get("p"), None, "the folder is removed");
    assert_eq!(commits.questions(), []);
}

#[test]
fn a_folder_removal_that_stops_past_the_window_is_noticed() {
    let mut trees = space(40, 60);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trash_remotely(&mut trees, &["d"]);
    let mut effects = Effects::default();
    for _ in 0..30 {
        let record = (effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned())
        .expect("a removal");
        if let Some(removal) = effects
            .done(&mut trees, commits.in_flight(), &record.effect_id, None)
            .unwrap()
        {
            commits.count_remote(removal, &mut trees, SEQ, &mut ids, NOW);
        }
        trees.settle(commits.aliases());
    }
    assert_eq!(notice(&commits), None, "30 of 100 files cross no limit");

    apply_effects(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);

    assert_eq!(trees.local().get("d"), None, "the folder is removed");
    let (_, window, _) = notice(&commits).expect("the folder is noticed");
    assert_eq!((window.deletions, window.affected_files), (1, 60));
}

#[test]
fn removals_whose_window_ended_open_no_notice() {
    let mut trees = space(30, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = root_files(0..10);
    trash_remotely(
        &mut trees,
        &first.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    apply_effects(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(commits.questions(), [], "10 of 30 files");

    let second = root_files(10..20);
    trash_remotely(
        &mut trees,
        &second.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    apply_effects(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);
    assert_eq!(
        commits.questions(),
        [],
        "10 of the 20 files left when the first 10 left the window"
    );
}

#[test]
fn a_notice_names_every_limit_the_removals_crossed() {
    let mut trees = space(1001, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let all = root_files(0..1001);
    trash_remotely(
        &mut trees,
        &all.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    apply_effects(&mut commits, &mut trees, &mut ids, NOW);

    let (_, window, exceeded) = notice(&commits).expect("a notice");
    assert_eq!(window.deletions, 1001);
    assert_eq!(exceeded, [Limit::Deletions, Limit::Share]);
}
