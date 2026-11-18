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
                left_out_when_placed: false,
                content_from_server: false,
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
        &BTreeMap::new(),
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

fn sent_move_check_of(parent: &str, name: &str, pending_for: &str) -> Result<(), TestCaseError> {
    let (_, held) = RemoteNode::from_state(&NodeState {
        node_id: "x".into(),
        parent_id: "home".into(),
        name: "a".into(),
        kind: 1,
        status: 1,
        ..Default::default()
    })
    .unwrap();
    let moved = Move {
        node_id: "x".into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
        pending_for: pending_for.into(),
        ..Default::default()
    };
    sent_move_check(&moved, &held.shape, None, None, "me", &BTreeMap::new())
}

fn owed_rename_check_with(
    synced_name: &str,
    change: impl FnOnce(&mut Move, &mut NodeState),
) -> Result<(), TestCaseError> {
    let mut state = NodeState {
        node_id: "x".into(),
        parent_id: "home".into(),
        name: "a (pending)".into(),
        kind: 1,
        status: 1,
        pending_for: "a".into(),
        pending_by: "me".into(),
        ..Default::default()
    };
    let (_, held) = RemoteNode::from_state(&state).unwrap();
    let synced = Shape {
        name: synced_name.into(),
        ..held.shape.clone()
    };
    let mut moved = Move {
        node_id: "x".into(),
        new_parent_id: "home".into(),
        new_name: "a".into(),
        ..Default::default()
    };
    change(&mut moved, &mut state);
    let (_, remote) = RemoteNode::from_state(&state).unwrap();
    sent_move_check(
        &moved,
        &held.shape,
        Some(&synced),
        Some(&remote),
        "me",
        &BTreeMap::new(),
    )
}

fn owed_rename_check(mark: &str, synced_name: &str) -> Result<(), TestCaseError> {
    owed_rename_check_with(synced_name, |_, state| state.pending_for = mark.into())
}

fn owed_rename_check_after(change: impl FnOnce(&mut Move, &mut NodeState)) -> Result<(), TestCaseError> {
    owed_rename_check_with("a (pending)", change)
}

#[test]
fn the_rename_a_pending_name_owes_passes_the_sent_move_check_while_the_disk_holds_the_pending_name() {
    assert!(owed_rename_check("a", "a (pending)").is_ok());
}

#[test]
fn a_rename_from_a_pending_name_fails_the_sent_move_check_unless_the_mark_and_every_tree_agree() {
    assert!(owed_rename_check("b", "a (pending)").is_err());
    assert!(owed_rename_check("a", "a").is_err());
}

#[test]
fn a_move_aside_for_another_name_fails_the_sent_move_check_while_the_disk_holds_the_pending_name() {
    assert!(owed_rename_check_after(|moved, _| moved.pending_for = "b".into()).is_err());
}

#[test]
fn a_rename_from_a_pending_name_into_another_folder_fails_the_sent_move_check() {
    assert!(owed_rename_check_after(|moved, _| moved.new_parent_id = "elsewhere".into()).is_err());
}

#[test]
fn a_rename_from_a_pending_name_fails_the_sent_move_check_while_r_holds_the_node_elsewhere() {
    assert!(owed_rename_check_after(|_, state| state.parent_id = "elsewhere".into()).is_err());
}

#[test]
fn a_rename_from_a_pending_name_fails_the_sent_move_check_while_r_holds_the_node_in_the_trash() {
    assert!(owed_rename_check_after(|_, state| state.status = 2).is_err());
}

#[test]
fn a_rename_from_a_pending_name_another_device_set_fails_the_sent_move_check() {
    assert!(owed_rename_check_after(|_, state| state.pending_by = "other".into()).is_err());
}

#[test]
fn a_move_to_the_place_the_disk_holds_passes_the_sent_move_check() {
    assert!(sent_move_check_of("home", "a", "").is_ok());
}

#[test]
fn a_move_aside_to_a_pending_name_for_the_name_the_disk_holds_passes_the_sent_move_check() {
    assert!(sent_move_check_of("home", "a (pending)", "a").is_ok());
}

#[test]
fn a_move_to_a_folder_the_disk_does_not_hold_it_in_fails_the_sent_move_check() {
    let error = sent_move_check_of("elsewhere", "a", "").unwrap_err();
    assert!(error.to_string().contains("is sent to a place the disk does not hold it at"));
    assert!(sent_move_check_of("elsewhere", "a (pending)", "a").is_err());
}

#[test]
fn a_move_to_a_name_the_disk_does_not_hold_fails_the_sent_move_check() {
    assert!(sent_move_check_of("home", "b", "").is_err());
    assert!(sent_move_check_of("home", "b (pending)", "b").is_err());
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

#[test]
fn a_discarded_folder_the_user_moves_an_entry_an_earlier_discard_took_along_into_is_the_users() {
    let seed = [act(1, 0, 0, 0, 0), act(1, 0, 0, 5, 0), act(0, 0, 0, 0, 0)];
    let local = [
        act(0, 0, 0, 0, 0),
        act(1, 0, 5710523212519727777, 0, 0),
        act(0, 0, 8362300977708182750, 2, 0),
    ];
    let during = [
        (true, vec![act(8, 0, 3967452738093421826, 0, 0)]),
        (
            false,
            vec![
                act(2, 266909642346921001, 0, 0, 0),
                act(4, 1422974510127056623, 0, 0, 0),
            ],
        ),
        (true, vec![act(0, 0, 6000348076165824698, 5, 0)]),
        (
            true,
            vec![act(3, 15276342541379635278, 11841328229179036497, 0, 0)],
        ),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    run_ignoring_answering_after(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &[Ruling::Exclude(vec!["c.txt"])],
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_discarded_folder_stays_off_the_server_when_the_user_moves_out_what_arrived_in_a_server_folder_it_takes_along() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(
            false,
            vec![
                act(4, 10608111819271174101, 0, 0, 0),
                act(0, 0, 12958059801695950876, 0, 0),
            ],
        )],
        act(8, 0, 0, 0, 0),
        vec![
            (
                false,
                vec![
                    act(0, 0, 1533563441121785454, 0, 0),
                    act(3, 2646866086397974328, 286365459004699858, 0, 0),
                ],
            ),
            (false, vec![act(0, 0, 7031929195995084547, 0, 0)]),
        ],
        act(7, 0, 0, 0, 0),
        act(9, 8782585485606220866, 0, 0, 3),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(1, 0, 0, 3, 0),
            act(0, 0, 0, 6, 0),
            act(0, 0, 163347753468072842, 0, 0),
            act(1, 0, 0, 0, 0),
            act(1, 0, 80214081026911650, 0, 0),
        ],
        &[act(0, 0, 0, 5, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_server_file_the_server_edited_and_trashed_after_it_was_written()
 {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![
            (
                false,
                vec![
                    act(3, 6563456750138208221, 6653762091013073807, 0, 0),
                    act(2, 76120783504708564, 0, 0, 0),
                ],
            ),
            (
                false,
                vec![
                    act(2, 279800153722622989, 0, 0, 0),
                    act(4, 6316084977119993380, 0, 0, 0),
                ],
            ),
        ],
        act(8, 14283514286341, 0, 0, 0),
        vec![
            (
                false,
                vec![act(4, 9102383114394317503, 0, 0, 0), act(2, 0, 0, 0, 0)],
            ),
            (
                false,
                vec![
                    act(0, 0, 2741078651781309836, 0, 0),
                    act(4, 11275118652569310417, 0, 0, 0),
                ],
            ),
        ],
        act(7, 17382083876922469, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(1, 0, 0, 5, 0),
            act(0, 0, 0, 2, 0),
            act(0, 0, 9266656165052801544, 5, 0),
            act(0, 0, 0, 0, 0),
            act(0, 0, 4558050171813961518, 0, 0),
            act(1, 0, 2032494584937929559, 0, 0),
            act(1, 0, 0, 0, 0),
        ],
        &[act(0, 0, 0, 5, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        9,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_leaves_a_server_folder_the_user_removed_an_entry_from_to_the_user() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![
            (false, vec![act(2, 0, 0, 0, 0)]),
            (
                true,
                vec![act(3, 2094582802364268140, 15272791550546786922, 2, 0)],
            ),
        ],
        act(8, 10093011241592231414, 0, 0, 0),
        vec![(true, vec![act(4, 16592146011263293529, 0, 0, 0)])],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["a"],
    );
    run_ignoring_answering_after(
        &[
            act(0, 0, 0, 2, 0),
            act(1, 0, 0, 0, 0),
            act(0, 0, 440234330134331381, 2, 0),
        ],
        &[act(0, 0, 0, 3, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        7,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_gives_back_the_folder_when_the_user_edits_inside_a_server_folder_it_took_along()
 {
    run_ignoring_answering_after(
        &[act(0, 0, 0, 5, 0), act(0, 0, 0, 0, 0)],
        &[
            act(0, 0, 0, 1, 0),
            act(0, 0, 3570165619780894286, 5, 0),
            act(1, 0, 687474002968895165, 0, 0),
        ],
        &[],
        MAX_OPS,
        false,
        &[
            (true, vec![act(8, 0, 0, 0, 0)]),
            (false, vec![act(0, 0, 0, 0, 0)]),
            (true, vec![act(9, 0, 0, 0, 1)]),
        ],
        Cut::None,
        &[
            Ruling::File(vec![]),
            Ruling::Include(vec!["a"]),
            Ruling::Exclude(vec!["a"]),
            Ruling::Exclude(vec![]),
        ],
        Answer::Discard,
        Some(PriorSync::Shown),
        3,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_server_folder_after_the_keep_rule_brings_back_a_file_deleted_in_it()
 {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(0, 0, 0, 3, 0)])],
        act(8, 0, 0, 0, 0),
        vec![
            (false, vec![act(1, 0, 0, 0, 0)]),
            (false, vec![act(4, 16998930721721672818, 0, 0, 0)]),
        ],
        act(7, 394369, 0, 0, 0),
        act(9, 0, 0, 0, 2),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(1, 0, 0, 0, 0),
            act(1, 0, 0, 0, 0),
            act(1, 0, 0, 0, 0),
            act(0, 0, 0, 0, 0),
            act(1, 0, 7214111189693794999, 0, 0),
            act(1, 0, 0, 0, 0),
            act(0, 0, 0, 0, 0),
        ],
        &[act(0, 0, 0, 0, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        9,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_leaves_a_server_folder_to_the_user_once_the_rules_stop_keeping_back_a_delete_in_it()
 {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![
            (false, vec![act(0, 0, 7102054750434410170, 2, 0)]),
            (
                true,
                vec![
                    act(3, 10671660887448527019, 10242677703864011517, 2, 0),
                    act(4, 12125739356159939665, 0, 0, 0),
                ],
            ),
        ],
        act(8, 0, 0, 0, 0),
        vec![(false, vec![act(2, 3394935901777304464, 0, 0, 0)])],
        act(7, 544876195332911984, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["a"],
    );
    run_ignoring_answering_after(
        &[
            act(1, 0, 0, 0, 0),
            act(1, 0, 0, 0, 0),
            act(0, 0, 0, 5, 0),
            act(1, 0, 12486383627201936880, 3, 0),
            act(1, 0, 9437286109419346847, 2, 0),
            act(0, 0, 2651891447982760259, 2, 0),
        ],
        &[
            act(0, 0, 0, 3, 0),
            act(1, 0, 1024634189188085383, 2, 0),
            act(1, 0, 3876941951795470939, 0, 0),
            act(1, 0, 9574629332979124862, 2, 0),
        ],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_removes_a_discarded_file_the_user_put_in_a_server_folder_it_took_along() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(3, 13192353822766416202, 9677610980065783011, 0, 0)])],
        act(8, 1455760940810843340, 0, 0, 0),
        vec![],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 1),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(1, 0, 0, 0, 0),
            act(0, 0, 0, 3, 0),
            act(1, 0, 0, 3, 0),
            act(0, 0, 1482717339917189330, 2, 0),
        ],
        &[
            act(0, 0, 0, 3, 0),
            act(0, 0, 7803206904060982520, 2, 0),
            act(0, 0, 11594374937869911794, 0, 0),
            act(0, 0, 1186265133151242328, 5, 0),
            act(0, 0, 20907952538896, 5, 0),
        ],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_server_folder_the_server_moves_with_a_discarded_file_in_it() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(false, vec![act(1, 0, 0, 0, 0)])],
        act(8, 1222051512298736028, 0, 0, 0),
        vec![(
            false,
            vec![
                act(0, 0, 6235880690034886446, 0, 0),
                act(3, 2325266322636419526, 9515598847448697626, 0, 0),
            ],
        )],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["ss/"],
    );
    run_ignoring_answering_after(
        &[
            act(0, 0, 0, 2, 0),
            act(1, 0, 1125575177470894621, 0, 0),
            act(0, 0, 2191591050819556085, 0, 0),
            act(1, 0, 0, 5, 0),
        ],
        &[
            act(0, 0, 0, 3, 0),
            act(1, 0, 4589810021220436427, 0, 0),
            act(1, 0, 7458629158884164442, 5, 0),
            act(0, 0, 5946956894036349662, 2, 0),
            act(1, 0, 7826072239891859908, 2, 0),
        ],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
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
            left_out_when_placed: false,
            content_from_server: false,
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

fn keep_rule_keeps_a_deleted_folder(server_renames_it: bool) -> bool {
    let mut device = Device {
        trees: Trees::new("root"),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::new(),
        acted: BTreeSet::from(["k".into()]),
        removed_from: BTreeMap::from([("k".into(), "root".into())]),
        kept_back: BTreeSet::new(),
        relocated: BTreeMap::new(),
    };
    let change = |name: &str| ChangeRecord {
        change: Some(change_record::Change::Node(NodeState {
            node_id: "k".into(),
            parent_id: "root".into(),
            name: name.into(),
            kind: 1,
            status: 1,
            ..Default::default()
        })),
    };
    device.receive(&[change("k")], false);
    let held = device.trees.remote().live("k").unwrap().clone();
    device.trees.set_local(
        "k".into(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: held,
            identity: identity(1),
            parked: false,
        },
    );
    device.trees.settle(&Default::default());
    device.trees.remove_local("k");
    device.receive(&[change(if server_renames_it { "a" } else { "k" })], false);
    note_kept_back(&mut device);
    device.kept_back.contains("k")
}

#[test]
fn the_keep_rule_keeps_back_a_delete_of_a_folder_the_server_renamed() {
    assert!(keep_rule_keeps_a_deleted_folder(true));
}

#[test]
fn the_keep_rule_lets_a_delete_through_when_the_server_left_the_folder_as_it_was() {
    assert!(!keep_rule_keeps_a_deleted_folder(false));
}

#[test]
fn the_keep_rule_keeps_back_a_folder_above_an_entry_it_already_kept_back() {
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
    let change = |id: &str, parent: &str, name: &str| ChangeRecord {
        change: Some(change_record::Change::Node(NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            kind: 1,
            status: 1,
            ..Default::default()
        })),
    };
    device.receive(&[change("k", "root", "k"), change("c", "k", "c")], false);
    for (id, ino) in [("k", 1), ("c", 2)] {
        let shape = device.trees.remote().live(id).unwrap().clone();
        let node = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape,
            identity: identity(ino),
            parked: false,
        };
        device.trees.set_local(id.into(), node);
    }
    device.trees.settle(&Default::default());
    device.trees.remove_local("c");
    device.removed_from.insert("c".into(), "k".into());
    device.acted.insert("c".into());
    device.receive(&[change("c", "k", "a")], false);
    note_kept_back(&mut device);
    assert!(device.kept_back.contains("c"));
    device.trees.set_synced("c".into(), None);
    device.trees.remove_local("k");
    device.removed_from.insert("k".into(), "root".into());
    device.acted.insert("k".into());
    note_kept_back(&mut device);
    assert!(device.kept_back.contains("k"));
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

#[test]
fn a_first_link_discard_takes_along_a_server_folder_the_user_renames_and_renames_back() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(3, 13043628290883745512, 452950137641576840, 4, 0)]), (true, vec![act(3, 739103967421887141, 2062317377113806989, 0, 0)])],
        act(8, 4049594328618023028, 0, 4, 0),
        vec![],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["*.txt"],
    );
    run_ignoring_answering_after(
        &[act(0, 0, 0, 0, 0), act(0, 0, 0, 5, 0)],
        &[act(0, 0, 0, 2, 0), act(1, 0, 1370762273860725108, 5, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_keeps_its_folder_when_the_keep_rule_brings_back_a_folder_another_device_moved_a_kept_file_into() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(0, 0, 1908899473287221145, 0, 0)]), (false, vec![act(3, 1261734324015559043, 1399270233953278280, 4, 0), act(3, 13290987726592160374, 9126886706708825695, 3, 0)])],
        act(8, 0, 4816891309826524868, 0, 0),
        vec![(false, vec![act(0, 0, 13845429190687857571, 0, 0), act(3, 4393672847939280122, 14553794976699681568, 0, 0)])],
        act(7, 1066001238954467340, 0, 0, 0),
        act(9, 1551125831013961667, 0, 0, 2),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[act(1, 0, 0, 3, 0), act(0, 0, 0, 3, 0), act(1, 0, 3210902777627476830, 0, 0)],
        &[act(0, 0, 0, 3, 0), act(0, 0, 1447839146262177289, 0, 0), act(1, 0, 1740775572683597341, 2, 0), act(0, 0, 11690145856306366989, 2, 0), act(1, 0, 0, 5, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_keeps_its_folder_when_the_keep_rule_brings_back_a_folder_for_a_file_deleted_while_excluded() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(0, 0, 1908899473287221145, 0, 0)]), (false, vec![act(3, 1261734324015559043, 1399270233953278280, 4, 0), act(3, 13290987726592160374, 9126886706708825695, 3, 0)])],
        act(8, 0, 13847608082324046912, 0, 0),
        vec![(true, vec![act(4, 2328167654866144165, 0, 0, 0)]), (false, vec![act(0, 0, 13845429190687857571, 0, 0), act(3, 4393672847939280122, 14553794976699681568, 0, 0)])],
        act(7, 1066001238954467340, 0, 0, 0),
        act(9, 1551125831013961667, 0, 0, 2),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[act(1, 0, 0, 3, 0), act(0, 0, 0, 3, 0), act(1, 0, 3210902777627476830, 0, 0)],
        &[act(0, 0, 0, 3, 0), act(0, 0, 1447839146262177289, 0, 0), act(1, 0, 1740775572683597341, 2, 0), act(0, 0, 11690145856306366989, 2, 0), act(1, 0, 0, 5, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_keeps_its_folder_when_another_device_moves_a_file_the_server_renamed_into_a_deleted_folder() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(false, vec![act(3, 15680877563213661197, 14518141476999504328, 0, 0)])],
        act(8, 3332313473884684837, 0, 0, 0),
        vec![(true, vec![act(3, 5451334418866865601, 4438273723159878528, 5, 0)]), (false, vec![act(3, 9362973579064359648, 2824538069448625041, 0, 0)])],
        act(7, 171470286317878745, 0, 0, 0),
        act(9, 7283399266307091194, 0, 0, 2),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[act(0, 0, 0, 3, 0), act(0, 0, 1055555136860197332, 5, 0), act(1, 0, 5557856237184014422, 0, 0), act(1, 0, 4159129869147441962, 0, 0), act(0, 0, 3360597633019559603, 0, 0), act(0, 0, 10997035567734722199, 0, 0), act(1, 0, 2676797848052099272, 0, 0)],
        &[act(0, 0, 0, 0, 0), act(1, 0, 8915017329082556, 3, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_folder_an_earlier_discard_gave_back_and_sent_leaves_a_later_discarded_folder_discarded() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(false, vec![act(4, 2655855693315775170, 0, 0, 0)]), (true, vec![act(1, 0, 1616403451123157839, 0, 0), act(3, 16898293393933491673, 16441079112380366162, 0, 0)])],
        act(8, 0, 0, 0, 0),
        vec![],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["*.txt"],
    );
    let ran = run_ignoring_answering_after(
        &[act(0, 0, 0, 1, 0), act(1, 0, 4664361095268115873, 0, 0), act(1, 0, 5015278945299757376, 0, 0), act(0, 0, 14596149430375813363, 0, 0), act(0, 0, 18179744410189147, 0, 0), act(0, 0, 65437919313017109, 0, 0)],
        &[act(0, 0, 0, 0, 0), act(0, 0, 0, 2, 0), act(0, 0, 6733077998261294296, 0, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
    assert_eq!(ran.asked, 2);
}

#[test]
fn a_discarded_entry_the_user_moves_under_an_excluded_name_leaves_its_folder_discarded() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![],
        act(8, 0, 13721232132711175410, 0, 0),
        vec![(true, vec![act(3, 8556790666456352007, 10349665207294808420, 5, 0)]), (true, vec![act(4, 3317891310307958236, 0, 0, 0)])],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["ss/"],
    );
    run_ignoring_answering_after(
        &[act(0, 0, 0, 3, 0), act(0, 0, 214362822890558879, 0, 0)],
        &[act(0, 0, 0, 2, 0), act(0, 0, 0, 5, 0), act(0, 0, 16449030456427620336, 0, 0), act(0, 0, 11373672789133557546, 0, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn an_entry_the_user_adds_while_a_question_holds_a_discarded_folder_leaves_it_discarded() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(4, 3153416352843863388, 0, 0, 0)]), (false, vec![act(4, 4822314848495969547, 0, 0, 0)])],
        act(8, 0, 14240183382636765481, 0, 0),
        vec![(true, vec![act(1, 0, 9706165151592842237, 0, 0)]), (true, vec![act(0, 0, 15412097922945134533, 0, 0)])],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["ss/"],
    );
    run_ignoring_answering_after(
        &[act(0, 0, 0, 2, 0), act(1, 0, 2288260264374171290, 2, 0), act(0, 0, 6238598908570521293, 0, 0), act(1, 0, 5395621292053666303, 0, 0), act(0, 0, 2275988685389933975, 0, 0), act(0, 0, 5289490273149297498, 0, 0), act(1, 0, 0, 0, 0)],
        &[act(0, 0, 0, 3, 0), act(0, 0, 8195093280767941914, 0, 0), act(0, 0, 365635496584458673, 3, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_server_folder_another_device_moves_a_file_the_user_deleted_elsewhere_into() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(4, 3193619043066901372, 0, 0, 0)])],
        act(8, 948219515280477812, 0, 0, 0),
        vec![(true, vec![act(4, 11118585874138123204, 0, 0, 0)]), (false, vec![act(3, 14204793114887675686, 18687495927258218, 0, 0)])],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[act(1, 0, 0, 2, 0), act(1, 0, 0, 0, 0), act(0, 0, 0, 5, 0), act(0, 0, 1066857648799286982, 5, 0), act(0, 0, 2949306108588062700, 0, 0), act(0, 0, 5900224687448801063, 3, 0)],
        &[act(1, 0, 0, 3, 0), act(0, 0, 0, 0, 0), act(0, 0, 17683535187996987751, 0, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_discarded_folder_the_user_takes_a_server_file_back_out_of_before_it_is_sent_stays_discarded() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(true, vec![act(2, 15194337304231427283, 0, 0, 0)])],
        act(8, 0, 0, 0, 0),
        vec![(true, vec![act(3, 2070138730741303007, 3516292705765972510, 2, 0)]), (true, vec![act(3, 8421402412044323369, 6391184908008750463, 0, 0)])],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["*.txt"],
    );
    run_ignoring_answering_after(
        &[act(1, 0, 0, 2, 0), act(1, 0, 0, 2, 0), act(0, 0, 0, 2, 0), act(0, 0, 0, 2, 0)],
        &[act(0, 0, 0, 0, 0), act(1, 0, 925400935235988677, 0, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_folder_the_server_excludes_after_the_user_deleted_an_entry() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![
            (true, vec![act(0, 0, 6143651230719131862, 0, 0)]),
            (true, vec![act(4, 3723815044277860101, 0, 0, 0)]),
        ],
        act(8, 0, 0, 0, 0),
        vec![
            (
                false,
                vec![
                    act(0, 0, 393246980653295408, 0, 0),
                    act(3, 2489304970407150330, 7897093711423239965, 0, 0),
                ],
            ),
            (true, vec![act(0, 0, 0, 0, 0)]),
        ],
        act(7, 0, 0, 0, 0),
        act(9, 0, 0, 0, 0),
        vec!["a"],
    );
    run_ignoring_answering_after(
        &[
            act(0, 0, 0, 3, 0),
            act(1, 0, 0, 3, 0),
            act(1, 0, 2711637186835934911, 2, 0),
            act(1, 0, 12044279749324718203, 2, 0),
            act(1, 0, 17518525026918653311, 2, 0),
        ],
        &[act(0, 0, 0, 2, 0), act(0, 0, 901096871437522843, 2, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        7,
    )
    .unwrap();
}

#[test]
fn a_first_link_discard_takes_along_a_folder_whose_deleted_entry_the_keep_rule_brings_back() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![],
        act(8, 0, 0, 0, 0),
        vec![
            (false, vec![act(4, 5902506458575076078, 0, 0, 0)]),
            (
                false,
                vec![act(3, 3178496309646421934, 8834845962153142654, 0, 0)],
            ),
        ],
        act(7, 138867428890129899, 0, 0, 0),
        act(9, 4899837622381071773, 0, 0, 2),
        vec!["*.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(0, 0, 0, 0, 0),
            act(1, 0, 6013222925077551386, 0, 0),
            act(0, 0, 4938777378806610997, 0, 0),
            act(1, 0, 12070172744303450150, 0, 0),
            act(1, 0, 0, 0, 0),
            act(0, 0, 2229239498280160517, 0, 0),
        ],
        &[act(0, 0, 0, 2, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        7,
    )
    .unwrap();
}

#[test]
fn a_discarded_folder_stays_discarded_when_the_keep_rule_brings_back_a_folder_the_user_deleted() {
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(
            false,
            vec![act(3, 15680877563213661197, 14518141476999504328, 0, 0)],
        )],
        act(8, 3332313473884684837, 0, 0, 0),
        vec![(
            false,
            vec![act(3, 9362973579064359648, 2824538069448625041, 0, 0)],
        )],
        act(7, 171470286317878745, 0, 0, 0),
        act(9, 7283403821086187110, 0, 0, 2),
        vec!["c.txt"],
    );
    run_ignoring_answering_after(
        &[
            act(0, 0, 0, 3, 0),
            act(0, 0, 1055555136860197332, 5, 0),
            act(1, 0, 5557856237184014422, 0, 0),
            act(1, 0, 4159129869147441962, 0, 0),
            act(0, 0, 3360597633019559603, 0, 0),
            act(0, 0, 10997035567734722199, 0, 0),
            act(1, 0, 2676797848052099272, 0, 0),
        ],
        &[act(0, 0, 0, 0, 0), act(1, 0, 8915017329082556, 3, 0)],
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        0,
    )
    .unwrap();
}
