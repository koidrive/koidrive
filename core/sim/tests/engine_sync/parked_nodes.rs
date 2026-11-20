// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_rename_the_server_applies_under_another_name_is_not_a_reason_to_bring_the_node_back() {
    let (mut space, mut other) = seeded(&[act(0, 0, 0, 0, 0)]);
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let node = space.nodes().into_iter().find(|n| n.name == "a").unwrap();
    let (id, remote) = RemoteNode::from_state(&node).unwrap();
    let mut device = Device::new(Trees::new(root));
    let held = LocalNode {
        shape: remote.shape.clone(),
        identity: device.identity(),
        parked: false,
        chunks: Box::default(),
        left_out_when_placed: false,
        content_from_server: false,
    };
    device
        .trees
        .set_synced(id.clone(), Some(held.shape.clone()));
    device.trees.set_local(id.clone(), held);
    device.trees.set_remote(id.clone(), remote);
    device.act(&act(3, 0, 0, 2, 0));
    let seq = space.head_seq();
    let mut commits = Commits::default();
    device.trees.settle(commits.aliases());
    let sent = commits
        .build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            "me",
            seq,
        )
        .unwrap()
        .clone();
    assert!(
        matches!(&sent.ops[..], [Op { op: Some(op::Op::Move(m)), .. }] if m.node_id == id && m.new_name == "b")
    );
    remote_action(&mut space, &mut other, 2, &act(0, 0, 0, 2, 0));
    let result = space.commit(&session("me"), NOW, &sent).unwrap().result;
    assert_eq!(result.results[0].code(), op_result::Code::AppliedRenamed);
    for record in space
        .resume(&Resume {
            since_seq: seq,
            history_id: history.clone(),
            ..Default::default()
        })
        .unwrap()
    {
        device.receive(&record.changes, record.seq == result.seq);
    }
    commits.received(result.clone()).unwrap();
    assert!(commits.acknowledge(&mut device.trees, result.seq));
    device.acknowledged(&sent, &result.results);
    let mut effects = Effects::default();
    device.trees.settle(commits.aliases());
    for _ in 0..8 {
        let Some(effect) = effects
            .plan(
                &device.trees,
                None,
                &mut device.ids,
                NOW as u64,
                BatchLimit::ONE,
            )
            .first()
            .cloned()
        else {
            break;
        };
        let identity = device.identity();
        effects
            .done(&mut device.trees, None, &effect.effect_id, Some(identity))
            .unwrap();
        device.trees.settle(commits.aliases());
    }
    assert_eq!(
        device.trees.synced().live(&id),
        device.trees.local().live(&id)
    );
    assert_eq!(
        device.trees.synced().live(&id),
        device.trees.remote().live(&id)
    );
    let index = space
        .nodes()
        .into_iter()
        .filter(|n| n.node_id != space.space_id())
        .position(|n| n.node_id == id)
        .unwrap();
    remote_action(&mut space, &mut other, 2, &act(4, index, 0, 0, 0));
    for record in space
        .resume(&Resume {
            since_seq: result.seq,
            history_id: history,
            ..Default::default()
        })
        .unwrap()
    {
        device.receive(&record.changes, false);
    }
    device.trees.settle(commits.aliases());
    assert!(device.trees.remote().get(&id).unwrap().trashed);
    assert_eq!(
        device.trees.synced().live(&id),
        device.trees.local().live(&id)
    );
    assert!(revive_check(&device, &BTreeSet::from([id.clone()]), &id).is_err());
}

#[test]
fn a_parked_child_of_a_folder_renamed_in_an_earlier_commit_is_not_brought_back_from_the_trash() {
    let seed = [act(0, 0, 0, 0, 0), act(0, 0, 1, 5, 0)];
    let (mut space, mut other) = seeded(&seed);
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let nodes: Vec<_> = space
        .nodes()
        .into_iter()
        .filter(|n| n.node_id != root)
        .collect();
    let parent = nodes.iter().position(|n| n.name == "a").unwrap();
    let child = nodes.iter().position(|n| n.name == "ß").unwrap();
    let id = NodeId::from(&nodes[child].node_id);
    let ancestor = NodeId::from(&nodes[parent].node_id);
    let mut device = Device::new(Trees::new(root));
    for state in &nodes {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        device
            .trees
            .set_synced(id.clone(), Some(node.shape.clone()));
        let held = LocalNode {
            shape: node.shape.clone(),
            identity: device.identity(),
            parked: false,
            chunks: Box::default(),
            left_out_when_placed: false,
            content_from_server: false,
        };
        device.trees.set_local(id.clone(), held);
        device.trees.set_remote(id, node);
    }
    let seq = space.head_seq();
    device.act(&act(3, parent, 0, 2, 0));
    assert!(device.acted.contains(&ancestor));
    let session = session("me");
    let mut commits = Commits::default();
    device.trees.settle(commits.aliases());
    let renamed = commits
        .build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            "me",
            seq,
        )
        .cloned()
        .unwrap();
    assert_eq!(renamed.ops.len(), 1);
    assert!(
        matches!(&renamed.ops[0].op, Some(op::Op::Move(m)) if m.node_id == ancestor && m.new_name == "b")
    );
    let answer = space.commit(&session, NOW, &renamed).unwrap().result;
    device.receive(&answer.changes, true);
    let seq = answer.seq;
    device.acknowledged(&renamed, &answer.results);
    commits.received(answer).unwrap();
    assert!(commits.acknowledge(&mut device.trees, seq));
    device.trees.settle(commits.aliases());
    assert_eq!(
        device.trees.synced().live(&ancestor),
        device.trees.remote().live(&ancestor)
    );
    assert!(!device.relocated.contains_key(&ancestor));
    let mut local = device.trees.local().get(&id).unwrap().clone();
    local.parked = true;
    device.trees.set_local(id.clone(), local);
    let current: Vec<_> = space
        .nodes()
        .into_iter()
        .filter(|n| n.node_id != space.space_id())
        .collect();
    let child = current.iter().position(|n| n.node_id == id).unwrap();
    let parent = 1 + current
        .iter()
        .filter(|n| n.kind == 1)
        .position(|n| n.node_id == ancestor)
        .unwrap();
    remote_action(&mut space, &mut other, 2, &act(4, child, 0, 0, 0));
    remote_action(&mut space, &mut other, 2, &act(0, 0, parent, 6, 0));
    let backlog = space
        .resume(&Resume {
            since_seq: seq,
            history_id: history.clone(),
            ..Default::default()
        })
        .unwrap();
    for record in backlog {
        device.receive(&record.changes, false);
    }
    device.trees.set_synced(id.clone(), None);
    let holder = space.nodes().into_iter().find(|n| n.name == "ss").unwrap();
    let holder_id = NodeId::from(&holder.node_id);
    let mut held = device.trees.local().get(&id).unwrap().clone();
    held.parked = false;
    held.shape.name = "ss".into();
    held.identity = device.identity();
    device.trees.set_local(holder_id.clone(), held.clone());
    device.trees.set_synced(holder_id, Some(held.shape));
    device.trees.settle(commits.aliases());
    assert_eq!(device.trees.check(), Ok(()));
    assert!(device.trees.remote().get(&id).unwrap().trashed);
    assert!(device.trees.remote().live(&ancestor).is_some());
    let alone = BTreeSet::from([id.clone()]);
    assert!(revive_check(&device, &alone, &id).is_err());
    let result = commits
        .build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            "me",
            space.head_seq(),
        )
        .cloned();
    assert!(result.is_none());

    let current: Vec<_> = space
        .nodes()
        .into_iter()
        .filter(|n| n.node_id != space.space_id())
        .collect();
    let index = current.iter().position(|n| n.node_id == ancestor).unwrap();
    let before_trash = space.head_seq();
    remote_action(&mut space, &mut other, 2, &act(4, index, 0, 0, 0));
    for record in space
        .resume(&Resume {
            since_seq: before_trash,
            history_id: history,
            ..Default::default()
        })
        .unwrap()
    {
        device.receive(&record.changes, false);
    }
    device.trees.settle(commits.aliases());
    assert!(device.trees.remote().get(&ancestor).unwrap().trashed);
    let quiet = BTreeSet::from([ancestor.clone()]);
    assert!(revive_check(&device, &quiet, &ancestor).is_err());
}

#[test]
fn two_reverted_files_that_swap_names_converge() {
    let act = |kind, target, name, text| Action {
        kind,
        target,
        parent: 0,
        name,
        text,
    };
    let local = [act(1, 0, 0, 0), act(1, 0, 2, 1)];
    let remote = [act(1, 0, 0, 2), act(1, 0, 2, 3)];
    let swap = vec![
        act(3, 3, 3, 0),
        act(3, 0, 5, 0),
        act(3, 1, 2, 0),
        act(3, 2, 0, 0),
    ];
    let parked = run(&[], &local, &remote, MAX_OPS, false, &[(false, swap)]).unwrap();
    assert!(parked > 0, "no reverted file was parked");
}

#[test]
fn folders_revived_above_a_parked_node_are_brought_back() {
    let seed = vec![
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
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 5801581838394722479,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6476457337238582140,
            name: 0,
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
            parent: 4564459193690538441,
            name: 4,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6344555226843187279,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 1911679536950536619,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 10497572882504026615,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 7649716111743945145,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 11369682697835578077,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 9257115507352902041,
            parent: 14556351700413880842,
            name: 3,
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
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 14758948430815737821,
            parent: 277029637290519871,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![(
        false,
        vec![
            Action {
                kind: 0,
                target: 0,
                parent: 101087396655831711,
                name: 0,
                text: 0,
            },
            Action {
                kind: 4,
                target: 7391292373329721386,
                parent: 0,
                name: 0,
                text: 0,
            },
        ],
    )];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edited_reverted_file_parked_under_a_folder_deleted_remotely_is_kept() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 620503160258612587,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 6503485206416241607,
            parent: 3276521438384508601,
            name: 5,
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
            kind: 4,
            target: 6817887910542140471,
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
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 1247370204288523498,
                name: 5,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 47517915818211563,
                parent: 1344201196089021663,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 12752295931867524494,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_folder_renamed_onto_the_name_a_parked_folder_needs_is_parked_too() {
    let seed = vec![Action {
        kind: 0,
        target: 0,
        parent: 0,
        name: 5,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 1370376148349361013,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1015448658534533405,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 0,
            parent: 4747322177713459370,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 0,
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
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 0,
                name: 5,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 3,
                target: 0,
                parent: 5182897700414857884,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_parked_file_forked_from_a_node_the_server_deleted_is_created_after_its_commit_is_split() {
    let seed = [act(1, 0, 0, 3, 0)];
    let local = [act(1, 0, 0, 3, 1)];
    let during = vec![
        (false, vec![act(2, 0, 0, 0, 0)]),
        (
            false,
            vec![
                act(0, 0, 0, 0, 0),
                act(3, 7779810202192050055, 202987497655873243, 5, 0),
            ],
        ),
        (
            true,
            vec![
                act(0, 0, 5081664043081684955, 5, 0),
                act(2, 6321014178578916138, 0, 0, 0),
            ],
        ),
        (false, vec![act(4, 4791664463415640549, 0, 0, 0)]),
    ];
    run_ignoring(
        &seed,
        &local,
        &[],
        1,
        false,
        &during,
        Cut::None,
        &[],
        Answer::Publish,
        Some(PriorSync::Shown),
    )
    .unwrap();
}

#[test]
fn a_blocked_unpark_home_onto_a_name_an_ignored_entry_holds_is_excused() {
    let mut device = Device::new(Trees::new("root"));
    device.rule(&Ruling::File(vec!["ss/"]));
    let id = NodeId::from("e");
    let (content, chunks) = split(Some(content(1, 1)));
    let identity = device.identity();
    device.trees.set_local_changed(
        id.clone(),
        LocalNode {
            shape: Shape {
                parent: "root".into(),
                name: "ss".into(),
                kind: Kind::File,
                content,
            },
            identity,
            parked: true,
            left_out_when_placed: false,
            content_from_server: false,
            chunks,
        },
    );
    device.ignored.insert(
        ("root".into(), vec!["ß".into()]),
        DiskEntry {
            kind: Kind::Dir,
            content: None,
            chunks: Box::default(),
            original: None,
        },
    );
    device.trees.settle(&Default::default());
    let (mut effects, mut failed) = (Effects::default(), BTreeMap::new());
    let record = effects.plan(
        &device.trees,
        None,
        &mut device.ids,
        NOW as u64,
        BatchLimit::ONE,
    )[0]
    .clone();
    assert!(matches!(
        &record.effect,
        Effect::Move {
            from: Location::Parked(_),
            ..
        }
    ));
    assert!(fail_on_ignored_name(
        &mut device,
        &mut effects,
        &record,
        &mut failed
    ));
    let at = still_planned_target(&device, &effects, &failed, None, &id).unwrap();
    assert!(device.disk_name_taken(&at.parent, &[], &at.name));
}
