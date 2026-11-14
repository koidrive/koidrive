// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_discarded_entry_the_rules_leave_out_for_a_while_stays_discarded_once_they_let_it_in() {
    let stories = [
        (
            vec![act(0, 0, 0, 2, 0)],
            vec![
                act(0, 0, 0, 0, 0),
                act(1, 0, 4673039444926547263, 2, 0),
                act(0, 0, 13266476644652416267, 0, 0),
            ],
            vec![
                (false, vec![act(0, 0, 0, 0, 0)]),
                (
                    true,
                    vec![act(3, 479216393097091650, 3374702992589535782, 5, 0)],
                ),
                (false, vec![act(0, 0, 0, 0, 0)]),
            ],
            vec![
                Ruling::Include(vec!["a"]),
                Ruling::File(vec![]),
                Ruling::File(vec![]),
                Ruling::Include(vec!["ß"]),
            ],
        ),
        (
            vec![act(1, 0, 0, 2, 0), act(0, 0, 0, 0, 0), act(0, 0, 0, 0, 0)],
            vec![
                act(0, 0, 0, 2, 0),
                act(0, 0, 1161139680209233225, 2, 0),
                act(0, 0, 4190000043942595171, 3, 0),
            ],
            vec![
                (false, vec![act(0, 0, 0, 0, 0)]),
                (false, vec![act(0, 0, 0, 3, 0)]),
                (false, vec![act(0, 0, 0, 0, 0)]),
                (false, vec![act(0, 0, 0, 0, 0)]),
            ],
            vec![
                Ruling::File(vec![]),
                Ruling::File(vec![]),
                Ruling::File(vec!["c.txt"]),
                Ruling::Include(vec!["ß"]),
                Ruling::Include(vec![]),
            ],
        ),
    ];
    for (seed, local, during, rulings) in stories {
        run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &during,
            Cut::None,
            &rulings,
            Answer::Discard,
            true,
        )
        .unwrap();
    }
}

#[test]
fn a_discarded_folder_the_user_moves_a_server_folder_into_is_sent_without_what_the_rules_leave_out()
{
    let seed = [
        act(0, 0, 0, 1, 0),
        act(0, 0, 1589386142481282302, 3, 0),
        act(1, 0, 337024389968201276, 0, 0),
    ];
    let local = [
        act(0, 0, 0, 5, 0),
        act(0, 0, 5550223642493787941, 0, 0),
        act(0, 0, 818620885751826322, 3, 0),
    ];
    let during = [
        (
            true,
            vec![act(3, 8557033530543547932, 53783006680222902, 2, 0)],
        ),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::Include(vec!["a", "ß"]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::File(vec![]),
    ];
    run_ignoring(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        true,
    )
    .unwrap();
}

#[test]
fn a_discarded_folder_parked_where_an_excluded_folder_holds_its_name_goes_to_the_local_trash() {
    let seed = [act(0, 0, 0, 5, 0), act(0, 0, 0, 0, 0)];
    let local = [
        act(0, 0, 0, 6, 0),
        act(0, 0, 371774925241157815, 0, 0),
        act(0, 0, 930660811337546809, 2, 0),
    ];
    let during = [
        (false, vec![act(0, 0, 0, 0, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::Include(vec!["ß"]),
        Ruling::Exclude(vec![]),
        Ruling::File(vec!["a"]),
        Ruling::Include(vec!["a"]),
    ];
    run_ignoring(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &rulings,
        Answer::Discard,
        true,
    )
    .unwrap();
}

#[test]
fn an_edited_parked_file_whose_place_here_is_taken_and_whose_server_place_is_excluded_stays_reported()
 {
    let seed = [
        act(0, 0, 0, 5, 0),
        act(0, 0, 231951485502570629, 2, 0),
        act(0, 0, 8120665025396950181, 0, 0),
        act(0, 0, 1064671299086725321, 3, 0),
        act(0, 0, 376187089449230377, 0, 0),
        act(1, 0, 294211349795495575, 0, 0),
    ];
    let local = [
        act(1, 0, 0, 5, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
        act(0, 0, 5455886572231891135, 0, 0),
    ];
    let during = [
        (true, vec![act(0, 0, 0, 0, 0)]),
        (
            false,
            vec![
                act(0, 0, 3733456458247703335, 5, 0),
                act(3, 4675952882751711745, 9164493832926334025, 0, 0),
            ],
        ),
        (false, vec![act(0, 0, 0, 0, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::File(vec![]),
        Ruling::Exclude(vec!["/b"]),
        Ruling::Include(vec!["a"]),
        Ruling::Include(vec![]),
        Ruling::Exclude(vec!["a"]),
    ];
    for cut in CUTS {
        let answer = Answer::Publish;
        run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &during,
            cut,
            &rulings,
            answer,
            true,
        )
        .unwrap();
    }
}

#[test]
fn a_folder_deleted_here_that_holds_excluded_files_stays_on_the_server_and_settles() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 4494598521765109016, 5, 0),
        act(1, 0, 0, 2, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
    ];
    let local = [act(0, 0, 0, 0, 0)];
    let during = [
        (
            false,
            vec![
                act(3, 3256290584448709780, 2072661791950199871, 3, 0),
                act(3, 8910861855723507387, 766653952569640727, 3, 0),
            ],
        ),
        (true, vec![act(4, 116364754780478432, 0, 0, 0)]),
    ];
    let rulings = [Ruling::Exclude(vec!["a"]), Ruling::Exclude(vec!["*.txt"])];
    for fits in [MAX_OPS, 1] {
        for cut in CUTS {
            let answer = Answer::Publish;
            run_ignoring(
                &seed,
                &local,
                &[],
                fits,
                false,
                &during,
                cut,
                &rulings,
                answer,
                true,
            )
            .unwrap();
        }
    }
}

#[test]
fn a_discarded_held_folder_that_holds_an_entry_excluded_after_it_was_made_settles() {
    let seed = [
        act(1, 0, 0, 3, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 3288518272854797638, 0, 0),
        act(0, 0, 7168129648560792379, 0, 0),
        act(1, 0, 4972809369289148530, 2, 0),
    ];
    let local = [act(0, 0, 0, 2, 0)];
    let during = [
        (true, vec![act(0, 0, 7704125438278671205, 5, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::File(vec![]),
        Ruling::File(vec![]),
        Ruling::Exclude(vec!["ss/"]),
    ];
    for fits in [MAX_OPS, 1] {
        for cut in CUTS {
            let answer = Answer::Discard;
            run_ignoring(
                &seed,
                &local,
                &[],
                fits,
                false,
                &during,
                cut,
                &rulings,
                answer,
                true,
            )
            .unwrap();
        }
    }
}

#[test]
fn a_folder_deleted_here_that_holds_excluded_files_comes_back_for_a_folder_the_other_device_adds() {
    let seed = [act(1, 0, 0, 2, 0)];
    let local = [act(0, 0, 0, 0, 0), act(0, 0, 2677639742671539445, 5, 0)];
    let during = [
        (false, vec![act(0, 0, 0, 2, 0)]),
        (true, vec![act(4, 2493412687189262816, 0, 0, 0)]),
        (false, vec![act(0, 0, 10330013182353619089, 0, 0)]),
    ];
    let rulings = [
        Ruling::Include(vec!["a"]),
        Ruling::File(vec![]),
        Ruling::File(vec!["ss/"]),
    ];
    for fits in [MAX_OPS, 1] {
        for cut in CUTS {
            let answer = Answer::Publish;
            run_ignoring(
                &seed,
                &local,
                &[],
                fits,
                false,
                &during,
                cut,
                &rulings,
                answer,
                true,
            )
            .unwrap();
        }
    }
}

#[test]
fn a_local_delete_keeps_a_recorded_entry_and_the_folders_above_it_as_the_driver_does() {
    let mut device = Device {
        trees: Trees::new("root"),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names: false,
        rules: Rules::default(),
        touched: BTreeSet::new(),
    };
    for (id, parent, name) in [("d", "root", "d"), ("m", "d", "m"), ("c", "m", "c")] {
        let shape = koidrive_engine::tree::Shape {
            parent: parent.into(),
            name: name.into(),
            kind: Kind::Dir,
            content: None,
        };
        let identity = device.identity();
        device.trees.set_local(
            id.into(),
            LocalNode {
                shape: shape.clone(),
                identity,
                parked: false,
            },
        );
        device.trees.set_synced(id.into(), Some(shape.clone()));
        device.trees.set_remote(
            id.into(),
            RemoteNode {
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
    device.trees.ignore_local("m");
    device.act(&act(4, 0, 0, 0, 0));
    assert!(device.trees.local().get("d").is_some());
    assert!(device.trees.local().get("m").is_some());
    assert!(device.trees.local().get("c").is_some());
}

#[test]
fn a_folder_brought_back_inside_a_folder_the_rules_exclude_here_is_left_out() {
    let seed = [
        act(0, 0, 0, 2, 0),
        act(1, 0, 1423286438289531559, 0, 0),
        act(0, 0, 10880074063369139784, 0, 0),
        act(0, 0, 2339271216845459220, 0, 0),
        act(0, 0, 9235770013735250324, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 20707279234150786, 0, 0),
        act(0, 0, 10878909224886797202, 0, 0),
        act(1, 0, 37475922255184027, 0, 0),
    ];
    let local = [
        act(4, 9017015455031306602, 0, 0, 0),
        act(0, 0, 991922665228092267, 5, 0),
        act(3, 4100218986585080613, 8173643921442050310, 0, 0),
    ];
    let remote = [
        act(1, 0, 3762238378559001317, 0, 0),
        act(0, 0, 14392790914454771113, 2, 0),
    ];
    let during = [
        (false, vec![act(0, 0, 0, 0, 0)]),
        (false, vec![act(0, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::File(vec![]),
        Ruling::Exclude(vec!["ss/"]),
        Ruling::File(vec![]),
    ];
    for cut in CUTS {
        let answer = Answer::Delete;
        run_ignoring(
            &seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, answer, false,
        )
        .unwrap();
    }
}

#[test]
fn a_server_entry_below_a_folder_moved_out_of_what_the_rules_include_here_is_left_out() {
    let seed = [act(0, 0, 0, 2, 0), act(0, 0, 7184675023852569860, 0, 0)];
    let local = [
        act(0, 0, 2196176128997887001, 0, 0),
        act(3, 7566706047339877485, 172849414804194726, 0, 0),
    ];
    let remote = [act(1, 0, 0, 0, 0)];
    let during = [(
        false,
        vec![
            act(4, 526189589210477396, 0, 0, 0),
            act(0, 0, 3559868529946001299, 0, 0),
        ],
    )];
    let rulings = [Ruling::File(vec![]), Ruling::Include(vec!["b/"])];
    for cut in CUTS {
        let answer = Answer::Delete;
        run_ignoring(
            &seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, answer, false,
        )
        .unwrap();
    }
}

#[test]
fn a_folder_deleted_here_that_another_device_renamed_comes_back_once_nothing_excluded_is_left_in_it() {
    let seed = [act(0, 0, 0, 6, 0)];
    let local = [act(4, 0, 0, 0, 0)];
    let remote = [act(1, 0, 1, 3, 0)];
    let during = [
        (false, vec![act(3, 0, 0, 2, 0)]),
        (true, vec![act(2, 0, 0, 0, 0)]),
        (false, vec![act(4, 1, 0, 0, 0)]),
        (true, vec![act(2, 0, 0, 0, 0)]),
    ];
    let rulings = [Ruling::Exclude(vec!["*.txt"])];
    for cut in CUTS {
        let ran = run_ignoring(&seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, Answer::Delete, false).unwrap();
        let root = ran.space.space_id().to_owned();
        let folders: Vec<_> = (ran.space.nodes().into_iter())
            .filter(|n| n.node_id != root && n.kind == 1 && n.status == 1)
            .map(|n| n.name)
            .collect();
        assert_eq!((ran.deletes, folders), (0, vec!["b".to_owned()]), "{cut:?}");
    }
}

#[test]
fn a_file_deleted_here_with_its_kept_folder_stays_deleted_after_another_device_renames_the_folder_and_the_rules_let_it_in() {
    let seed = [act(0, 0, 0, 6, 0), act(1, 0, 1, 3, 0)];
    let local = [act(4, 0, 0, 0, 0)];
    let during = [
        (false, vec![act(3, 0, 0, 2, 0)]),
        (true, vec![act(2, 0, 0, 0, 0)]),
        (true, vec![act(2, 0, 0, 0, 0)]),
    ];
    let rulings = [
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec![]),
    ];
    for cut in CUTS {
        let ran = run_ignoring(&seed, &local, &[], MAX_OPS, false, &during, cut, &rulings, Answer::Delete, false).unwrap();
        let root = ran.space.space_id().to_owned();
        let live: Vec<_> = (ran.space.nodes().into_iter())
            .filter(|n| n.node_id != root && n.status == 1)
            .map(|n| n.name)
            .collect();
        assert_eq!(live, vec!["b".to_owned()], "{cut:?}");
    }
}
