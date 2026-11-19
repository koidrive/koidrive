// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "a new folder made while another folder's rename is held back gets a conflict name instead of its own (GUN-823)"]
fn a_new_folder_takes_its_name_when_sync_drains_before_the_held_back_rename_clears() {
    let (mut space, _) = seeded(&[act(0, 0, 0, 0, 0)]);
    let root = space.space_id().to_owned();
    let holder = nodes_with(&space, 1, Some(1))[0].clone();
    let (_, old) = RemoteNode::from_state(&space.node(&holder).unwrap()).unwrap();
    let mut trees = Trees::new(root.clone());
    trees.set_synced(NodeId::from(&holder), Some(old.shape.clone()));
    trees.set_local(
        NodeId::from(&holder),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: old.shape.clone(),
            identity: identity(1),
            parked: false,
        },
    );
    trees.set_remote(NodeId::from(&holder), old);
    trees.settle(&Default::default());
    trees.rules_scanned();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(55));
    let mut renamed = trees.local().get(&NodeId::from(&holder)).unwrap().clone();
    renamed.shape.name = "b".into();
    trees.set_local(NodeId::from(&holder), renamed);
    trees.settle(commits.aliases());
    commits
        .build(
            &mut trees,
            &mut ids,
            NOW as u64,
            space.history_id(),
            "me",
            space.head_seq(),
        )
        .unwrap();
    commits
        .failed(
            &mut trees,
            &Error {
                code: error::Code::LimitExceeded as i32,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(commits.problems().contains(&NodeId::from(&holder)));
    let new = ids.uuid_v7(NOW as u64);
    trees.set_local(
        NodeId::from(&new),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: koidrive_engine::tree::Shape {
                parent: NodeId::from(&root),
                name: "a".into(),
                kind: Kind::Dir,
                content: None,
            },
            identity: identity(2),
            parked: false,
        },
    );
    trees.settle(commits.aliases());
    let sent = commits
        .build(
            &mut trees,
            &mut ids,
            NOW as u64,
            space.history_id(),
            "me",
            space.head_seq(),
        )
        .unwrap()
        .clone();
    assert!(matches!(&sent.ops[0].op, Some(op::Op::CreateDir(c)) if c.pending_for == "a"));
    commit_served(&mut space, &mut trees, &mut commits, &sent);
    sync_until_quiet(&mut space, &mut trees, &mut commits, &mut ids);
    assert!(commits.clear_problem(&NodeId::from(&holder)));
    sync_until_quiet(&mut space, &mut trees, &mut commits, &mut ids);
    assert_eq!(space.node(&holder).unwrap().name, "b");
    assert_eq!(space.node(&new).unwrap().name, "a");
}

#[test]
#[ignore = "a new local file in a folder the server purged leaves the server instead of landing in the folder's live parent"]
fn a_new_local_file_in_a_folder_the_server_purged_lands_in_the_folders_live_parent() {
    for remote_first in [false, true] {
        let live = live_after_a_purge(&[act(0, 0, 0, 0, 0)], remote_first);
        let kinds: Vec<(i32, bool)> = (live.iter())
            .map(|(kind, at_root, _)| (*kind, *at_root))
            .collect();
        assert_eq!(
            kinds,
            vec![(koidrive_proto::v1::NodeKind::File as i32, true)],
            "remote_first={remote_first}"
        );
    }
}

#[test]
#[ignore = "a new local file in a folder the server purged leaves the server instead of taking a new name in the folder's live parent"]
fn a_new_local_file_in_a_purged_folder_takes_a_new_name_beside_a_file_of_its_name() {
    for remote_first in [false, true] {
        let live = live_after_a_purge(&[act(0, 0, 0, 0, 0), act(1, 0, 0, 3, 0)], remote_first);
        let file = koidrive_proto::v1::NodeKind::File as i32;
        assert!(
            live.len() == 2
                && (live.iter()).all(|(kind, at_root, _)| *kind == file && *at_root)
                && live.iter().any(|(_, _, name)| name == "c.txt")
                && live.iter().any(|(_, _, name)| name != "c.txt"),
            "remote_first={remote_first}: {live:?}"
        );
    }
}

fn live_after_a_purge(seed: &[Action], remote_first: bool) -> Vec<(i32, bool, String)> {
    let local = [act(1, 0, 1, 3, 0)];
    let gone = (false, vec![act(4, 0, 0, 0, 0), act(9, 0, 0, 0, 0)]);
    let during = if remote_first {
        vec![gone]
    } else {
        vec![(true, vec![act(2, 0, 0, 0, 1)]), gone]
    };
    let ran = run_cut(seed, &local, &[], MAX_OPS, false, &during, Cut::None)
        .unwrap_or_else(|failure| panic!("remote_first={remote_first}: {failure:?}"));
    let root = ran.space.space_id().to_owned();
    (ran.space.nodes().into_iter())
        .filter(|node| {
            node.node_id != root && node.status == koidrive_proto::v1::NodeStatus::Live as i32
        })
        .map(|node| (node.kind, node.parent_id == root, node.name))
        .collect()
}
