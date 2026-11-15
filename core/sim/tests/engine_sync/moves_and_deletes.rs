// SPDX-License-Identifier: Apache-2.0

fn commit_served(space: &mut SpaceDo, trees: &mut Trees, commits: &mut Commits, request: &Commit) {
    let result = space.commit(&session("me"), NOW, request).unwrap().result;
    apply(trees, &result.changes);
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, space.head_seq()));
    trees.settle();
}

fn sync_until_quiet(
    space: &mut SpaceDo,
    trees: &mut Trees,
    commits: &mut Commits,
    ids: &mut IdGen,
) {
    let mut effects = Effects::default();
    for _ in 0..20 {
        trees.settle();
        if let Some(record) = effects
            .plan(trees, commits.in_flight(), ids, NOW as u64, BatchLimit::ONE)
            .first()
            .cloned()
        {
            effects
                .done(
                    trees,
                    commits.in_flight(),
                    &record.effect_id,
                    Some(identity(100)),
                )
                .unwrap();
            continue;
        }
        let sent = commits
            .build(
                trees,
                ids,
                NOW as u64,
                space.history_id(),
                "me",
                space.head_seq(),
            )
            .cloned();
        let Some(sent) = sent else {
            return;
        };
        commit_served(space, trees, commits, &sent);
    }
    panic!("did not drain");
}

#[test]
fn a_new_folder_merges_into_a_folder_that_unknown_rules_hold_away_from_its_name() {
    let (mut space, mut other_ids) = seeded(&[act(0, 0, 0, 0, 0), act(0, 0, 1, 0, 0)]);
    let root = space.space_id().to_owned();
    let holder = space
        .nodes()
        .into_iter()
        .find(|n| n.parent_id != root && n.node_id != root)
        .unwrap()
        .node_id;
    let parent = space.node(&holder).unwrap().parent_id;
    let mut trees = Trees::new(root.clone());
    for (index, state) in space
        .nodes()
        .iter()
        .filter(|n| n.node_id != root)
        .enumerate()
    {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        trees.set_synced(id.clone(), Some(node.shape.clone()));
        trees.set_local(
            id.clone(),
            LocalNode {
                shape: node.shape.clone(),
                identity: identity(index as u64 + 1),
                parked: false,
            },
        );
        trees.set_remote(id, node);
    }
    trees.settle();
    trees.rules_scanned();
    let all = nodes_with(&space, 1, None);
    let target = all.iter().position(|id| *id == holder).unwrap();
    remote_action(&mut space, &mut other_ids, 2, &act(3, target, 0, 2, 0));
    for state in space.nodes().iter().filter(|n| n.node_id != root) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        trees.set_remote(id, node);
    }
    trees.set_rules_unknown(&parent);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(66));
    let new = ids.uuid_v7(NOW as u64);
    trees.set_local(
        new.clone(),
        LocalNode {
            shape: koidrive_engine::tree::Shape {
                parent: root.clone(),
                name: "b".into(),
                kind: Kind::Dir,
                content: None,
            },
            identity: identity(9),
            parked: false,
        },
    );
    trees.settle();
    assert!(trees.held(&holder));
    assert!(commits.problems().is_empty());
    assert_eq!(space.node(&holder).unwrap().name, "b");
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
    commit_served(&mut space, &mut trees, &mut commits, &sent);
    sync_until_quiet(&mut space, &mut trees, &mut commits, &mut ids);
    assert!(space.node(&new).is_none(), "{:?}", space.node(&new));
}

#[test]
fn a_new_folder_takes_its_name_once_a_held_back_rename_frees_it() {
    let (mut space, _) = seeded(&[act(0, 0, 0, 0, 0)]);
    let root = space.space_id().to_owned();
    let holder = nodes_with(&space, 1, Some(1))[0].clone();
    let (_, old) = RemoteNode::from_state(&space.node(&holder).unwrap()).unwrap();
    let mut trees = Trees::new(root.clone());
    trees.set_synced(holder.clone(), Some(old.shape.clone()));
    trees.set_local(
        holder.clone(),
        LocalNode {
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
    assert!(commits.clear_problem(&holder));
    sync_until_quiet(&mut space, &mut trees, &mut commits, &mut ids);
    assert_eq!(space.node(&holder).unwrap().name, "b");
    assert_eq!(space.node(&new).unwrap().name, "a");
}

#[test]
fn a_deleted_folder_comes_back_with_the_same_node_ids() {
    let seed = a_folder_with_a_file();
    let (space, _) = seeded(&seed);
    let nodes = nodes_with(&space, 1, None);
    let folder = nodes_with(&space, 1, Some(1))[0].clone();
    let file = nodes_with(&space, 1, Some(2))[0].clone();
    let folder_at = nodes.iter().position(|id| *id == folder).unwrap();
    for cut in CUTS {
        let delete = [act(4, folder_at, 0, 0, 0)];
        let restore = (true, vec![act(5, folder_at, 0, 0, 0)]);
        let ran = run_cut(&seed, &[], &delete, MAX_OPS, false, &[restore], cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].intent.node(), folder, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 1, None), nodes, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 2, None), Vec::<String>::new());
        assert_eq!(version_digests(&ran.space, &file).len(), 1, "{cut:?}");
    }
}

#[test]
fn a_node_moved_into_a_merged_folder_follows_it_on_disk() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6967822464045520996,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9526621794622326438,
            name: 1,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 10553703446551236695,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 16106826873077682970,
            name: 2,
            text: 0,
        },
        Action {
            kind: 3,
            target: 880737998146561905,
            parent: 5552030480946143737,
            name: 1,
            text: 0,
        },
    ];
    let remote = vec![];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![Action {
                kind: 3,
                target: 443641311708994000,
                parent: 3064515660927608345,
                name: 5,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edited_renamed_file_deleted_remotely_is_uploaded_as_a_new_file() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 0,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 2278650491807642941,
            parent: 9127562380887983576,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 10772034385298179478,
            name: 3,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 12531602759342590661,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 283388978725368259,
                    parent: 0,
                    name: 4,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 1,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 2,
                target: 2943695200610841979,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 1,
                target: 0,
                parent: 0,
                name: 3,
                text: 0,
            }],
        ),
        (
            false,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 6986862736185568223,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_node_moved_into_a_merged_folder_follows_a_remote_move() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 3,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 4,
            target: 3390051005637832722,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 3573038702653260728,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 761663922910750675,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 3824710055192453609,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 7106800540313915127,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 69233815001145249,
                    parent: 11870616208155072313,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 4672328705971433533,
                    parent: 9740151841705065456,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 1845576325211218462,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 4,
                    target: 10069492413079588418,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 17076322275721598278,
                    parent: 13906480724080066383,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 10335766898245895506,
                parent: 4894905971208633107,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edit_the_other_device_deleted_gives_way_to_the_folder_it_made_at_that_name() {
    let seed = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 95372568677471977, 0, 0),
    ];
    let local = [
        act(0, 0, 10639126546080908428, 0, 0),
        act(0, 0, 2083088007821781, 0, 0),
        act(5, 0, 0, 0, 3),
    ];
    let during = [
        (
            false,
            vec![act(4, 4134113597317913042, 0, 0, 0), act(3, 0, 0, 3, 0)],
        ),
        (true, vec![act(2, 0, 0, 0, 0)]),
        (false, vec![act(4, 0, 0, 0, 0), act(0, 0, 0, 3, 0)]),
    ];
    run_cut(&seed, &local, &[], 1, false, &during, Cut::None).unwrap();
}
