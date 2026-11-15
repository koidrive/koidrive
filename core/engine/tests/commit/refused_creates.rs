// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_create_refused_because_another_device_created_the_node_again_is_no_problem() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote(file(ROOT, "b", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    assert!(commits.problems().is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
}

#[test]
fn a_create_refused_because_another_device_created_and_deleted_the_node_again_is_no_problem() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    assert!(commits.problems().is_empty());
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_deleted_sends_an_edit_made_since() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn a_create_refused_for_a_node_another_device_renamed_and_deleted_follows_the_delete() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), trashed(file(ROOT, "b", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_created_edited_and_deleted_follows_the_delete() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "a", 1)));
    trees.settle();
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 2)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_deleted_sends_an_edit_made_since_to_its_bytes() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 2)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

fn resent_where_another_device_renamed_it(edit: Option<u8>) -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[synced("f", file(ROOT, "a", 1))]);
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 2));
    if let Some(digest) = edit {
        trees.set_local("f".into(), local(file(ROOT, "a", digest), 1));
    }
    let synced = trees.taken_from_others(|_| false);
    let untaken = trees.untaken_changes(|_| false);
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), untaken, records, 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(created_at(&commit), [("b".to_owned(), edit.unwrap_or(1))]);
    (trees, commits, ids)
}

fn id_exists() -> OpResult {
    OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    }
}

fn sends_the_edit_at_the_renamed_name(mut trees: Trees, mut commits: Commits, mut ids: IdGen) {
    acknowledge(&mut commits, &mut trees, vec![id_exists()]);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("f", 2)]);
    trees.set_remote_made_here("f".into(), remote_at(file(ROOT, "b", 2), 4));
    commits.received(result(4, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "b", 2)));
}

#[test]
fn a_resend_refused_for_a_node_another_device_renamed_and_deleted_revives_the_edit_at_that_name() {
    let (mut trees, commits, ids) = resent_where_another_device_renamed_it(Some(2));
    trees.set_remote("f".into(), trashed(file(ROOT, "b", 1)));
    sends_the_edit_at_the_renamed_name(trees, commits, ids);
}

#[test]
fn a_resend_refused_for_a_node_another_device_renamed_and_sent_again_sends_the_edit_at_that_name() {
    let (mut trees, commits, ids) = resent_where_another_device_renamed_it(Some(2));
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 3));
    sends_the_edit_at_the_renamed_name(trees, commits, ids);
}

#[test]
fn a_resend_refused_for_a_node_another_device_renamed_deleted_and_restored_sends_the_edit_at_that_name() {
    let (mut trees, commits, ids) = resent_where_another_device_renamed_it(Some(2));
    trees.set_remote("f".into(), trashed(file(ROOT, "b", 1)));
    trees.settle();
    trees.set_remote("f".into(), remote_at(file(ROOT, "b", 1), 3));
    sends_the_edit_at_the_renamed_name(trees, commits, ids);
}

#[test]
fn a_resend_refused_for_a_node_another_device_renamed_and_deleted_follows_the_delete() {
    let (mut trees, mut commits, mut ids) = resent_where_another_device_renamed_it(None);
    trees.set_remote("f".into(), trashed(file(ROOT, "b", 1)));
    acknowledge(&mut commits, &mut trees, vec![id_exists()]);
    assert_eq!(trees.class("f"), Class::Remote);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "f");
    assert!(trees.local().get("f").is_none());
}
