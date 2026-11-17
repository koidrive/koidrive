// SPDX-License-Identifier: Apache-2.0

fn ancestry_node(digest: u8, id: &str, made_over: &[&str]) -> RemoteNode {
    RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        version_id: id.into(),
        made_over: made_over.iter().map(|id| (*id).into()).collect(),
        ..remote(file(ROOT, "a", digest))
    }
}

fn ancestry_resend() -> Trees {
    let mut trees = trees(&[]);
    trees.set_remote("f".into(), ancestry_node(1, "v1", &[]));
    trees.set_local("f".into(), local(file(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
    trees.settle();
    let synced = trees.taken_from_others(|_| false);
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, &Default::default(), 0, 0);
    trees.settle();
    trees
}

#[test]
fn a_resend_gives_way_to_a_version_made_over_the_value_it_carries() {
    let mut trees = ancestry_resend();
    trees.set_remote("f".into(), ancestry_node(2, "v2", &["v1"]));
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_resend_keeps_its_value_beside_a_version_made_over_another_value() {
    let mut trees = ancestry_resend();
    trees.set_remote("f".into(), ancestry_node(2, "v2", &["other"]));
    trees.settle();
    assert_eq!(trees.class("f"), Class::Concurrent);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 1), "{:?}", ops(&commit));
}

#[test]
fn a_new_client_on_an_old_server_resends_a_lost_value_when_no_ancestry_comes_back() {
    let mut trees = ancestry_resend();
    let state = proto::NodeState {
        node_id: "f".into(),
        parent_id: ROOT.into(),
        name: "a".into(),
        kind: proto::NodeKind::File as i32,
        status: proto::NodeStatus::Live as i32,
        content_rev: 2,
        meta_rev: 1,
        edit_rev: 2,
        current: Some(proto::VersionInfo {
            version_id: "v2".into(),
            content: Some(content(2)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let decoded = proto::NodeState::decode(state.encode_to_vec().as_slice()).unwrap();
    let (id, node) = RemoteNode::from_state(&decoded).unwrap();
    assert!(node.made_over.is_empty());
    trees.set_remote(id, node);
    trees.settle();
    assert_eq!(trees.class("f"), Class::Concurrent);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 1), "{:?}", ops(&commit));
}

#[test]
fn an_edit_carries_the_server_version_its_local_value_was_made_over() {
    let mut trees = trees(&[]);
    trees.set_remote("f".into(), ancestry_node(1, "v1", &[]));
    trees.set_local("f".into(), local(file(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
    trees.settle();
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let [op::Op::UpdateContent(update)] = sent.as_slice() else {
        panic!("{:?}", ops(&commit));
    };
    assert_eq!(update.made_over, ["v1"]);
}

#[test]
fn a_users_create_after_a_loss_names_the_value_it_was_made_over() {
    let mut trees = ancestry_resend();
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let [op::Op::CreateFile(create)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(create.made_over, ["v1"]);
    assert!(create.source_version_id.is_empty());
}

#[test]
fn a_resend_create_names_the_server_version_of_the_value_it_carries() {
    let mut trees = ancestry_resend();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let [op::Op::CreateFile(create)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(create.source_version_id, "v1");
    assert!(uploads(&sent, 1));
}

fn without_a_base(digest: u8, source: Option<(&str, &[&str])>, node: RemoteNode) -> Trees {
    use koidrive_engine::state::{ContentProvenance, ContentVersion};
    let mut trees = Trees::new(ROOT);
    trees.set_remote("f".into(), node);
    trees.set_local("f".into(), local(file(ROOT, "a", digest), 1));
    let provenance = source.map(|(version_id, made_over)| {
        let local = ContentVersion {
            version_id: version_id.into(),
            made_over: made_over.iter().map(|id| (*id).into()).collect(),
            own: false,
        };
        let provenance = ContentProvenance {
            synced: None,
            local: Some(local),
            edit_bases: Vec::new(),
            unversioned_bases: Vec::new(),
        };
        ("f".into(), provenance)
    });
    let state = TreesState {
        dirty: ["f".into()].into(),
        content_provenance: provenance.into_iter().collect(),
        ..Default::default()
    };
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    trees
}

#[test]
fn a_file_at_the_version_r_was_made_over_takes_r_when_s_holds_no_base() {
    let mut trees = without_a_base(1, Some(("v1", &[])), ancestry_node(2, "v2", &["v1"]));
    assert_eq!(trees.class("f"), Class::Concurrent);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids);
    assert!(commit.is_none(), "{:?}", commit.map(|commit| ops(&commit)));
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_renamed_file_at_the_version_r_was_made_over_keeps_its_name_when_s_holds_no_base() {
    let mut trees = without_a_base(1, Some(("v1", &[])), ancestry_node(2, "v2", &["v1"]));
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1), 1));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "b")]);
}

#[test]
fn a_file_at_a_version_made_over_r_updates_r_when_s_holds_no_base() {
    let mut trees = without_a_base(2, Some(("v2", &["v1"])), ancestry_node(1, "v1", &[]));
    assert_eq!(trees.class("f"), Class::Concurrent);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    let [op::Op::UpdateContent(update)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(update.node_id, "f");
    assert_eq!(update.made_over, ["v1"]);
    assert!(uploads(&sent, 2), "{sent:?}");
}

#[test]
fn a_file_of_unknown_ancestry_beside_r_keeps_both_values_when_s_holds_no_base() {
    let cases = [
        (None, ancestry_node(2, "v2", &["v1"])),
        (Some(("v1", &[][..])), ancestry_node(2, "v2", &[])),
        (Some(("v1", &[][..])), ancestry_node(2, "v2", &["other"])),
        (Some(("v1", &["v0"][..])), ancestry_node(2, "", &[])),
    ];
    for (source, node) in cases {
        let mut trees = without_a_base(1, source, node);
        assert_eq!(trees.class("f"), Class::Concurrent);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let sent = ops(&commit);
        let [op::Op::CreateFile(create)] = sent.as_slice() else {
            panic!("{source:?} {sent:?}");
        };
        assert_ne!(create.node_id, "f");
        assert!(uploads(&sent, 1), "{sent:?}");
    }
}

fn held_by(id: &str, shape: Shape, made_over: &[&str]) -> RemoteNode {
    RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        version_id: id.into(),
        made_over: made_over.iter().map(|id| (*id).into()).collect(),
        ..remote(shape)
    }
}

fn a_lost_write(resent: bool, before: impl FnOnce(&mut Trees)) -> Trees {
    let mut trees = trees(&[]);
    trees.set_remote("f".into(), held_by("v1", written(ROOT, "a", 1), &["v0"]));
    trees.set_local("f".into(), local(written(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(written(ROOT, "a", 1)));
    trees.settle();
    before(&mut trees);
    trees.settle();
    let synced = (trees.taken_from_others(|_| false).into_iter())
        .filter(|_| resent)
        .collect();
    let records = trees.remote_records(|_| false);
    trees.remove_remote("f");
    trees.forget_lost(Vec::new(), Vec::new(), synced, Vec::new(), Vec::new(), records, &Default::default(), 0, 0);
    trees.settle();
    trees
}

fn a_copy() -> RemoteNode {
    held_by("c1", written(ROOT, "b", 1), &["v0"])
}

fn copied(trees: &mut Trees, copy: RemoteNode) {
    trees.set_local("g".into(), local(copy.shape.clone(), 2));
    trees.set_synced("g".into(), Some(copy.shape.clone()));
    trees.set_remote("g".into(), copy);
    trees.settle();
}

fn copied_late(mut trees: Trees) -> Trees {
    trees.set_remote("g".into(), a_copy());
    trees.settle();
    trees
}

fn sent_f(trees: &Trees) -> Vec<op::Op> {
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let Some(commit) = build(&mut commits, &mut trees.clone(), &mut ids) else {
        return Vec::new();
    };
    sent_for(&ops(&commit), "f").into_iter().cloned().collect()
}

fn resent_create(sent: &[op::Op]) -> bool {
    matches!(sent, [op::Op::CreateFile(create)] if create.source_version_id == "v1")
}

#[test]
fn a_lost_write_another_server_file_holds_is_not_sent_again() {
    let trees = a_lost_write(true, |trees| copied(trees, a_copy()));
    assert_eq!(sent_f(&trees), []);
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_lost_write_is_sent_again_beside_a_server_file_of_other_ancestry() {
    let copy = held_by("c1", written(ROOT, "b", 1), &["other"]);
    let sent = sent_f(&a_lost_write(true, |trees| copied(trees, copy)));
    assert!(resent_create(&sent), "{sent:?}");
}

#[test]
fn a_lost_write_is_sent_again_when_this_device_deletes_the_file_holding_it() {
    let trees = a_lost_write(true, |trees| {
        copied(trees, a_copy());
        trees.remove_local("g");
    });
    let sent = sent_f(&trees);
    assert!(resent_create(&sent), "{sent:?}");
}

#[test]
fn a_lost_write_is_sent_again_when_this_device_edits_the_file_holding_it() {
    let trees = a_lost_write(true, |trees| {
        copied(trees, a_copy());
        trees.set_local_changed("g".into(), local(written(ROOT, "b", 2), 2));
    });
    let sent = sent_f(&trees);
    assert!(resent_create(&sent), "{sent:?}");
}

#[test]
fn a_lost_write_is_sent_again_when_this_device_writes_the_same_bytes_over_the_file_holding_it() {
    let mut rewritten = written(ROOT, "b", 1);
    rewritten.content.as_mut().expect("a file").mtime_ns += 1;
    let trees = a_lost_write(true, |trees| {
        copied(trees, a_copy());
        trees.set_local_changed("g".into(), local(rewritten, 2));
    });
    let sent = sent_f(&trees);
    assert!(resent_create(&sent), "{sent:?}");
}

#[test]
fn a_lost_write_is_not_sent_again_once_a_server_file_this_device_never_held_holds_it() {
    let trees = copied_late(a_lost_write(true, |_| {}));
    assert_eq!(trees.state().resends, ["f".to_owned()].into());
    assert_eq!(sent_f(&trees), []);
}

#[test]
fn a_lost_create_is_not_sent_again_once_another_server_file_holds_its_write() {
    let trees = a_lost_write(false, |_| {});
    assert_eq!(trees.state().lost_creates, ["f".to_owned()].into());
    assert_eq!(sent_f(&copied_late(trees)), []);
}

#[test]
fn a_lost_write_is_sent_as_an_edit_once_the_server_holds_its_file_again() {
    let mut trees = copied_late(a_lost_write(true, |_| {}));
    trees.set_remote("f".into(), held_by("v0", written(ROOT, "a", 0), &[]));
    trees.settle();
    let sent = sent_f(&trees);
    let [op::Op::UpdateContent(update)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(update.made_over, ["v0"]);
    assert!(uploads(&sent, 1), "{sent:?}");
}

#[test]
fn a_lost_write_is_sent_again_when_this_device_holds_an_unsent_edit_of_the_file_holding_it() {
    let trees = a_lost_write(true, |trees| {
        trees.set_local("g".into(), local(written(ROOT, "b", 3), 2));
        trees.set_synced("g".into(), Some(written(ROOT, "b", 3)));
        trees.set_remote("g".into(), a_copy());
        let mut state = trees.state();
        state.unsent_edits.insert("g".into());
        state.dirty.insert("g".into());
        trees.restore(state).unwrap();
        trees.rules_scanned();
    });
    let sent = sent_f(&trees);
    assert!(resent_create(&sent), "{sent:?}");
}

