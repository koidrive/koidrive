// SPDX-License-Identifier: Apache-2.0

#[test]
fn triage_s3_gun823_a_new_folder_takes_its_name_when_sync_drains_before_the_held_back_rename_clears() {
    let (mut space, _) = seeded(&[act(0, 0, 0, 0, 0)]);
    let root = space.space_id().to_owned();
    let holder = nodes_with(&space, 1, Some(1))[0].clone();
    let (_, old) = RemoteNode::from_state(&space.node(&holder).unwrap()).unwrap();
    let mut trees = Trees::new(root.clone());
    trees.set_synced(holder.clone(), Some(old.shape.clone()));
    trees.set_local(
        holder.clone(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: old.shape.clone(),
            identity: identity(1),
            parked: false,
        },
    );
    trees.set_remote(holder.clone(), old);
    trees.settle();
    trees.rules_scanned();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(55));
    let mut renamed = trees.local().get(&holder).unwrap().clone();
    renamed.shape.name = "b".into();
    trees.set_local(holder.clone(), renamed);
    trees.settle();
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
    assert!(commits.problems().contains(&holder));
    let new = ids.uuid_v7(NOW as u64);
    trees.set_local(
        new.clone(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: koidrive_engine::tree::Shape {
                parent: root.clone(),
                name: "a".into(),
                kind: Kind::Dir,
                content: None,
            },
            identity: identity(2),
            parked: false,
        },
    );
    trees.settle();
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
    assert!(commits.clear_problem(&holder));
    sync_until_quiet(&mut space, &mut trees, &mut commits, &mut ids);
    assert_eq!(space.node(&holder).unwrap().name, "b");
    assert_eq!(space.node(&new).unwrap().name, "a");
}

#[test]
fn triage_s3_gun769_a_folder_the_server_purged_while_it_holds_a_new_local_file_settles() {
    unsafe { std::env::set_var("TRIAGE_S3_PURGE_DIRS", "1") };
    for remote_first in [false, true] {
        let seed = [act(0, 0, 0, 0, 0)];
        let local = [act(1, 0, 1, 3, 0)];
        let gone = (false, vec![act(4, 0, 0, 0, 0), act(7, 0, 0, 0, 0)]);
        let during = if remote_first { vec![gone] } else { vec![(true, vec![act(2, 0, 0, 0, 1)]), gone] };
        let ran = run_cut(&seed, &local, &[], MAX_OPS, false, &during, Cut::None);
        assert!(ran.is_ok(), "remote_first={remote_first}: {:?}", ran.err());
    }
}
