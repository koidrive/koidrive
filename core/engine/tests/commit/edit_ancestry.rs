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
