// SPDX-License-Identifier: Apache-2.0

fn placement_check_after_a_move(
    cycle: bool,
    destination: &str,
    trashed_parent: bool,
) -> Result<(), TestCaseError> {
    let mut trees = Trees::new("root");
    for (id, local_parent, remote_parent) in [
        ("home", "root", "root"),
        ("elsewhere", "root", "root"),
        ("a", "root", if trashed_parent { "x" } else { "root" }),
        (
            "p",
            if trashed_parent { "a" } else { "root" },
            if cycle { "x" } else { "root" },
        ),
        ("x", "p", "home"),
    ] {
        let (id, remote) = RemoteNode::from_state(&NodeState {
            node_id: id.into(),
            parent_id: remote_parent.into(),
            name: id.into(),
            kind: 1,
            status: if id == "p" && trashed_parent { 2 } else { 1 },
            ..Default::default()
        })
        .unwrap();
        trees.set_synced(id.clone(), Some(remote.shape.clone()));
        trees.set_local(
            id.clone(),
            LocalNode {
                shape: koidrive_engine::tree::Shape {
                    parent: local_parent.into(),
                    ..remote.shape.clone()
                },
                identity: identity(1),
                parked: false,
            },
        );
        trees.set_remote(id, remote);
    }
    assert_eq!(trees.check(), Ok(()));
    trees.revert("x".into());
    let local = trees.local().get("x").unwrap();
    let placed = BTreeMap::from([("x".into(), position(&local.shape))]);
    let after = koidrive_engine::tree::Shape {
        parent: destination.into(),
        ..local.shape.clone()
    };
    let record = koidrive_engine::effects::EffectRecord {
        effect_id: "move".into(),
        node: "x".into(),
        counter: 1,
        effect: Effect::Move {
            from: Location::At {
                base: Base::Root,
                dirs: local_path(&trees, "p")
                    .into_iter()
                    .map(|name| koidrive_engine::effects::Dir {
                        name,
                        identity: identity(1),
                    })
                    .collect(),
                name: "x".into(),
            },
            identity: identity(1),
            to: Location::At {
                base: Base::Root,
                dirs: vec![koidrive_engine::effects::Dir {
                    name: destination.into(),
                    identity: identity(1),
                }],
                name: "x".into(),
            },
        },
        after: Some(after),
        recoveries: 0,
    };
    assert!(record.applies(&trees, None));
    let mut state = Effects::default().state();
    state.in_flight.push(record.clone());
    let mut effects = Effects::from_state(state).unwrap();
    let mut device = Device {
        trees,
        ids: IdGen::new(7),
        inos: 1,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::from(["x".into()]),
        acted: BTreeSet::from(["x".into()]),
        removed_from: BTreeMap::new(),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
    let result = placement_check(
        &device,
        &record,
        &BTreeMap::new(),
        &placed,
        &BTreeSet::from(["p".into()]),
    );
    if result.is_ok() {
        effects
            .done(
                &mut device.trees,
                None,
                &record.effect_id,
                Some(identity(1)),
            )
            .unwrap();
        assert_eq!(
            device.trees.local().get("x").unwrap().shape.parent,
            destination
        );
        assert_eq!(device.trees.check(), Ok(()));
    }
    result
}

#[test]
fn a_move_back_to_its_r_position_that_a_cycle_forced_passes_the_placement_check() {
    assert!(placement_check_after_a_move(true, "home", false).is_ok());
}

#[test]
fn a_move_out_of_the_folder_the_user_put_a_node_in_fails_the_placement_check() {
    let error = placement_check_after_a_move(false, "elsewhere", false).unwrap_err();
    assert!(error.to_string().contains("effects move it out"));
}

#[test]
fn a_cycle_excuses_no_move_but_the_one_back_to_r_from_the_placement_check() {
    assert!(placement_check_after_a_move(true, "elsewhere", false).is_err());
}

#[test]
fn a_folder_the_server_trashed_closes_no_cycle_for_the_placement_check() {
    assert!(placement_check_after_a_move(false, "home", true).is_err());
}

#[test]
fn a_first_link_holds_a_file_whose_name_the_space_holds_until_the_person_answers() {
    for answer in [Answer::Publish, Answer::Discard] {
        let seed = [act(1, 0, 0, 0, 0)];
        let local = [act(1, 0, 0, 0, 0), act(0, 0, 0, 2, 0)];
        let ran = run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &[],
            Cut::None,
            &[],
            answer,
            Some(PriorSync::Shown),
        )
        .unwrap();
        assert_eq!(ran.asked, 1, "{answer}");
        let live = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != ran.space.space_id())
            .count();
        let expected = if answer == Answer::Publish { 3 } else { 1 };
        assert_eq!(live, expected, "{answer}");
        assert_eq!(
            ran.removals,
            if answer == Answer::Discard { 2 } else { 0 },
            "{answer}"
        );
    }
}

#[test]
fn a_fresh_first_link_sends_a_file_whose_name_the_space_holds_as_a_conflict_copy_without_a_question() {
    let seed = [act(1, 0, 0, 0, 0)];
    let local = [act(1, 0, 0, 0, 0), act(0, 0, 0, 2, 0)];
    let ran = run_ignoring(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &[],
        Cut::None,
        &[],
        Answer::Publish,
        Some(PriorSync::Fresh),
    )
    .unwrap();
    assert_eq!((ran.asked, ran.conflicts), (0, 0));
    let live = (ran.space.nodes().into_iter())
        .filter(|node| node.status == 1 && node.node_id != ran.space.space_id())
        .count();
    assert_eq!((live, ran.removals), (3, 0));
}

#[test]
fn a_first_link_that_shows_an_earlier_sync_keeps_a_file_the_space_deleted_and_purged_deleted() {
    let kept = [act(1, 0, 0, 2, 0), act(1, 0, 0, 3, 0), act(7, 1, 0, 0, 0)];
    let emptied = [act(1, 0, 0, 3, 0), act(7, 0, 0, 0, 0)];
    let local = [act(1, 0, 0, 3, 0)];
    for (seed, prior, answer, asked, live) in [
        (&kept[..], PriorSync::Shown, Answer::Discard, 1, vec!["b"]),
        (&kept[..], PriorSync::Fresh, Answer::Discard, 0, vec!["b", "c.txt"]),
        (&emptied[..], PriorSync::Shown, Answer::Discard, 1, vec![]),
        (&emptied[..], PriorSync::Fresh, Answer::Discard, 0, vec!["c.txt"]),
    ] {
        let ran = run_ignoring(
            seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &[],
            Cut::None,
            &[],
            answer,
            Some(prior),
        )
        .unwrap();
        assert_eq!(ran.asked, asked, "{prior:?}");
        let mut names: Vec<String> = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != ran.space.space_id())
            .map(|node| node.name)
            .collect();
        names.sort();
        assert_eq!(names, live, "{prior:?} {seed:?}");
    }
}

#[test]
fn a_discarded_folder_the_user_moved_a_synced_file_into_before_the_answer_converges() {
    let seed = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
        act(1, 0, 5494772186727268893, 0, 0),
    ];
    let local = [act(0, 0, 0, 1, 0)];
    let during = [(
        true,
        vec![act(3, 3247345582551114674, 732025268020294025, 0, 0)],
    )];
    run_ignoring(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &[],
        Answer::Discard,
        Some(PriorSync::Shown),
    )
    .unwrap();
}

fn keep_rule_walk_reaches_a_deleted_folder(server_took_the_move: bool) -> bool {
    let mut device = Device {
        trees: Trees::new("root"),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::new(),
        acted: BTreeSet::from(["k".into(), "m".into()]),
        removed_from: BTreeMap::from([("k".into(), "root".into())]),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
    let state = |id: &str, parent: &str| NodeState {
        node_id: id.into(),
        parent_id: parent.into(),
        name: id.into(),
        kind: 1,
        status: 1,
        ..Default::default()
    };
    let change = |id: &str, parent: &str| ChangeRecord {
        change: Some(change_record::Change::Node(state(id, parent))),
    };
    let moved = RemoteNode::from_state(&state("m", "root")).unwrap().1.shape;
    device.trees.set_local(
        "m".into(),
        LocalNode {
            shape: moved.clone(),
            identity: identity(1),
            parked: false,
        },
    );
    let from = RemoteNode::from_state(&state("m", "k")).unwrap().1.shape;
    device.relocate("m", &from, &moved);
    let mut changes = Vec::new();
    if server_took_the_move {
        changes.push(change("m", "root"));
    }
    changes.extend([change("k", "root"), change("m", "k"), change("u", "m")]);
    device.receive(&changes, false);
    note_kept_back(&mut device);
    device.kept_back.contains("k")
}

#[test]
fn the_keep_rule_walk_stops_at_a_folder_the_users_pending_move_takes_out() {
    assert!(!keep_rule_walk_reaches_a_deleted_folder(false));
}

#[test]
fn the_keep_rule_walk_passes_a_folder_whose_move_the_server_took_before_moving_it_back() {
    assert!(keep_rule_walk_reaches_a_deleted_folder(true));
}

#[test]
fn a_move_out_and_back_leaves_no_relocation_to_stop_the_keep_rule_walk() {
    let mut device = Device {
        trees: Trees::new("root"),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::new(),
        acted: BTreeSet::new(),
        removed_from: BTreeMap::new(),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
    let at = |parent: &str| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: "m".into(),
        kind: Kind::Dir,
        content: None,
    };
    device.relocate("m", &at("h"), &at("k"));
    assert!(device.relocated.contains_key("m"));
    device.relocate("m", &at("k"), &at("h"));
    assert!(!device.relocated.contains_key("m"));
    device.relocate("m", &at("h"), &at("k"));
    device.relocate("m", &at("c"), &at("h"));
    assert!(device.relocated.get("m").is_some_and(|moved| moved.from == at("c")));
}

#[test]
fn a_move_the_server_answers_at_another_place_is_pending_from_there() {
    let mut device = Device {
        trees: Trees::new("root"),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::new(),
        acted: BTreeSet::new(),
        removed_from: BTreeMap::new(),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
    let at = |parent: &str| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: "m".into(),
        kind: Kind::Dir,
        content: None,
    };
    let answer = ChangeRecord {
        change: Some(change_record::Change::Node(NodeState {
            node_id: "m".into(),
            parent_id: "k".into(),
            name: "m".into(),
            kind: 1,
            status: 1,
            ..Default::default()
        })),
    };
    device.relocate("m", &at("h"), &at("j"));
    device.receive(&[answer], true);
    device.relocate("m", &at("j"), &at("h"));
    assert!(device.relocated.get("m").is_some_and(|moved| moved.from == at("k")));
}
