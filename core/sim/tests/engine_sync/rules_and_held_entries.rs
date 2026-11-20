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
            Some(PriorSync::Shown),
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
        Some(PriorSync::Shown),
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
        Some(PriorSync::Shown),
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
            Some(PriorSync::Shown),
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
                Some(PriorSync::Shown),
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
                Some(PriorSync::Shown),
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
                Some(PriorSync::Shown),
            )
            .unwrap();
        }
    }
}

#[test]
fn a_local_delete_keeps_a_recorded_entry_and_the_folders_above_it_as_the_driver_does() {
    let mut device = Device::new(Trees::new("root"));
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
                left_out_when_placed: false,
                content_from_server: false,
                shape: shape.clone(),
                identity,
                parked: false,
                chunks: Box::default(),
            },
        );
        device.trees.set_synced(id.into(), Some(shape.clone()));
        device.trees.set_remote(
            id.into(),
            RemoteNode { made_over: Vec::new(),
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
                chunks: Box::default(),
            },
        );
    }
    device.trees.ignore_local(&"m".into());
    device.act(&act(4, 0, 0, 0, 0));
    assert!(device.trees.local().get(&"d".into()).is_some());
    assert!(device.trees.local().get(&"m".into()).is_some());
    assert!(device.trees.local().get(&"c".into()).is_some());
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
            &seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, answer, None,
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
            &seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, answer, None,
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
        let ran = run_ignoring(&seed, &local, &remote, MAX_OPS, false, &during, cut, &rulings, Answer::Delete, None).unwrap();
        let root = ran.space.space_id().to_owned();
        let folders: Vec<_> = (ran.space.nodes().into_iter())
            .filter(|n| n.node_id != root && n.kind == 1 && n.status == 1)
            .map(|n| n.name)
            .collect();
        assert_eq!((ran.deletes, folders), (0, vec!["b".to_owned()]), "{cut:?}");
    }
}

#[test]
fn a_file_deleted_here_with_its_kept_folder_comes_back_after_another_device_renames_the_folder_and_the_rules_let_it_in()
 {
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
        let ran = run_ignoring(&seed, &local, &[], MAX_OPS, false, &during, cut, &rulings, Answer::Delete, None).unwrap();
        let root = ran.space.space_id().to_owned();
        let mut live: Vec<_> = (ran.space.nodes().into_iter())
            .filter(|n| n.node_id != root && n.status == 1)
            .map(|n| n.name)
            .collect();
        live.sort();
        assert_eq!(
            (ran.deletes, live),
            (0, vec!["b".to_owned(), "c.txt".to_owned()]),
            "{cut:?}"
        );
    }
}

#[test]
fn an_edit_after_another_device_purges_a_file_in_a_server_folder_in_a_discarded_folder_keeps_the_folder() {
    let seed = [act(0, 0, 0, 0, 0), act(1, 0, 1, 2, 0)];
    let local = [act(0, 0, 0, 6, 0)];
    let during = [
        (false, vec![act(6, 0, 0, 0, 0)]),
        (true, vec![act(8, 0, 0, 0, 0)]),
        (false, vec![act(6, 0, 0, 0, 0)]),
        (false, vec![act(7, 0, 0, 0, 0)]),
        (true, vec![act(9, 0, 0, 0, 1)]),
    ];
    let rulings = [
        Ruling::File(vec![]),
        Ruling::Exclude(vec![]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec!["*.txt"]),
        Ruling::Exclude(vec![]),
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
        Some(PriorSync::Shown),
    )
    .unwrap();
}

#[test]
fn a_server_folder_moved_into_a_discarded_folder_out_of_a_folder_deleted_here_settles() {
    let seed = [
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 5, 0),
        act(0, 0, 0, 0, 0),
        act(0, 0, 0, 0, 0),
        act(0, 0, 5125074381177199187, 3, 0),
        act(1, 0, 13620712548678903678, 0, 0),
        act(0, 0, 8770897390828022430, 0, 0),
    ];
    let local = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 5, 0),
        act(0, 0, 8593331880432161113, 0, 0),
        act(0, 0, 164401821770219029, 0, 0),
    ];
    let during = [
        (false, vec![act(0, 0, 18303126742269488846, 0, 0)]),
        (false, vec![act(0, 0, 15497506449051316537, 0, 0)]),
        (true, vec![act(8, 4965594241967384804, 8623223411207746126, 0, 0)]),
        (true, vec![act(4, 9813782166594930651, 23, 4, 3)]),
        (false, vec![act(7, 7278376959139293227, 13730584698140342588, 1, 3)]),
        (true, vec![act(9, 13181647493508412243, 6311867364256608202, 4, 1)]),
    ];
    let rulings = [
        Ruling::File(vec![]),
        Ruling::Exclude(vec!["c.txt"]),
        Ruling::Exclude(vec!["c.txt"]),
        Ruling::Exclude(vec!["c.txt"]),
        Ruling::Exclude(vec!["c.txt"]),
        Ruling::Exclude(vec![]),
    ];
    run_ignoring_answering_after(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        Cut::Built,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        2,
    )
    .unwrap();
}

#[test]
fn a_server_file_deleted_with_a_deep_chain_of_new_folders_is_acted_below_the_folder_above() {
    let mut device = Device::new(Trees::new("root"));
    let remote = |shape: koidrive_engine::tree::Shape, chunks: &[ChunkRef]| RemoteNode {
        made_over: Vec::new(),
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
        chunks: chunks.into(),
    };
    let folder = |parent: &str, name: &str| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    };
    let place = |device: &mut Device,
                 id: &str,
                 shape: koidrive_engine::tree::Shape,
                 chunks: &[ChunkRef]| {
        let identity = device.identity();
        device.trees.set_local(
            id.into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape,
                identity,
                parked: false,
                chunks: chunks.into(),
            },
        );
    };
    place(&mut device, "k", folder("root", "k"), &[]);
    device
        .trees
        .set_remote("k".into(), remote(folder("root", "k"), &[]));
    let mut parent = "k".to_string();
    for depth in 1..=8 {
        let id = format!("a{depth}");
        place(&mut device, &id, folder(&parent, &id), &[]);
        parent = id;
    }
    let (held, chunks) = split(Some(content(1, 0)));
    let file = koidrive_engine::tree::Shape {
        parent: NodeId::from(&parent),
        name: "f".into(),
        kind: Kind::File,
        content: held,
    };
    place(&mut device, "f", file, &chunks);
    let server = koidrive_engine::tree::Shape {
        parent: "root".into(),
        name: "f".into(),
        kind: Kind::File,
        content: held,
    };
    device.trees.set_remote("f".into(), remote(server, &chunks));
    device.act(&act(4, 0, 0, 0, 0));
    assert!(device.trees.local().get(&"f".into()).is_none());
    assert!(acted_below(
        &device,
        &"k".into(),
        &BTreeMap::new(),
        &BTreeMap::new()
    ));
}

fn a_file_in_a_taken_along_server_folder() -> (
    Device,
    BTreeMap<NodeId, koidrive_engine::tree::Shape>,
    BTreeMap<NodeId, koidrive_engine::tree::Shape>,
) {
    let mut device = Device::new(Trees::new("root"));
    let shape = |parent: &str, name: &str, kind: Kind, content| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: name.into(),
        kind,
        content,
    };
    let (held, chunks) = split(Some(content(1, 0)));
    let local = [
        ("h", shape("root", "h", Kind::Dir, None)),
        ("s", shape("h", "s", Kind::Dir, None)),
        ("f", shape("s", "f", Kind::File, held)),
    ];
    for (id, shape) in &local {
        let identity = device.identity();
        let chunks: Box<[ChunkRef]> = if *id == "f" {
            chunks[..].into()
        } else {
            Box::default()
        };
        device.trees.set_local(
            (*id).into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: shape.clone(),
                identity,
                parked: false,
                chunks,
            },
        );
    }
    for (id, server) in [
        ("s", shape("root", "s", Kind::Dir, None)),
        ("f", shape("root", "f", Kind::File, held)),
    ] {
        let chunks: Box<[ChunkRef]> = if id == "f" {
            chunks[..].into()
        } else {
            Box::default()
        };
        device.trees.set_remote(
            id.into(),
            RemoteNode {
                made_over: Vec::new(),
                shape: server,
                trashed: false,
                content_rev: 1,
                meta_rev: 1,
                meta_edit_rev: 1,
                trash_group: String::new(),
                version_id: String::new(),
                edit_rev: 1,
                pending_for: None,
                pending_by: None,
                chunks,
            },
        );
    }
    let discarded = BTreeMap::from([("h".into(), local[0].1.clone())]);
    let kept_with = BTreeMap::from([
        ("s".into(), local[1].1.clone()),
        ("f".into(), local[2].1.clone()),
    ]);
    (device, kept_with, discarded)
}

#[test]
fn a_server_file_deleted_from_a_taken_along_folder_is_judged_by_where_it_was_at_the_discard() {
    let (mut device, kept_with, discarded) = a_file_in_a_taken_along_server_folder();
    device.act(&act(4, 0, 0, 0, 0));
    assert!(device.trees.local().get(&"f".into()).is_none());
    assert!(acted_below(&device, &"s".into(), &kept_with, &discarded));
}

#[test]
fn a_server_entry_counts_for_a_taken_along_folder_only_if_the_folder_held_it_at_the_discard() {
    for (at_discard, changes_the_folder) in
        [(Some("root"), false), (None, false), (Some("s"), true)]
    {
        let (mut device, kept_with, discarded) = a_file_in_a_taken_along_server_folder();
        let (g, s): (NodeId, NodeId) = ("g".into(), "s".into());
        let mut remote = device.trees.remote().get(&"f".into()).unwrap().clone();
        remote.shape.name = "g".into();
        if let Some(parent) = at_discard {
            remote.shape.parent = parent.into();
            device.trees.set_remote(g.clone(), remote.clone());
        }
        device.note_taken_along([&s]);
        remote.shape.parent = s.clone();
        remote.shape.name = "g2".into();
        device.trees.set_remote(g.clone(), remote);
        device.note_taken_along([]);
        device.acted.insert(g.clone());
        device.removed_from.insert(g, s.clone());
        assert_eq!(
            acted_below(&device, &s, &kept_with, &discarded),
            changes_the_folder,
            "{at_discard:?}"
        );
    }
}

#[test]
fn renaming_an_entry_another_device_brought_into_a_taken_along_folder_changes_that_entry_alone() {
    for (at_discard, brought, changes_the_folder) in [
        (false, true, false),
        (true, true, true),
        (false, false, true),
    ] {
        let (mut device, kept_with, discarded) = a_file_in_a_taken_along_server_folder();
        let (g, s): (NodeId, NodeId) = ("g".into(), "s".into());
        let mut remote = device.trees.remote().get(&"f".into()).unwrap().clone();
        remote.shape.parent = s.clone();
        remote.shape.name = "g".into();
        if at_discard {
            device.trees.set_remote(g.clone(), remote.clone());
        }
        device.note_taken_along([&s]);
        device.trees.set_remote(g.clone(), remote.clone());
        let mut local = device.trees.local().get(&"f".into()).unwrap().clone();
        local.shape = remote.shape.clone();
        local.shape.name = "g2".into();
        device.trees.set_local(g.clone(), local);
        if brought {
            device.placements += 1;
            let order = device.placements;
            (device.stays.entry(g.clone()).or_default()).insert("s".into(), (true, order));
        }
        device.acted.insert(g);
        assert_eq!(
            acted_below(&device, &s, &kept_with, &discarded),
            changes_the_folder,
            "{at_discard} {brought}"
        );
    }
}

#[test]
fn a_server_folder_holding_a_file_another_device_purged_since_is_still_a_server_copy() {
    let (mut device, _, _) = a_file_in_a_taken_along_server_folder();
    let (s, f): (NodeId, NodeId) = ("s".into(), "f".into());
    let server = device.trees.remote().get(&f).unwrap().shape.clone();
    device.trees.set_synced(f.clone(), Some(server));
    device.trees.remove_remote(&f);
    let (shown, mut kept) = (BTreeMap::new(), BTreeMap::new());
    assert!(server_copy(
        &device,
        &s,
        &shown,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &mut kept
    ));
    assert!(kept.contains_key(&f));
}

#[test]
fn a_post_purge_edit_below_a_purged_moved_in_folder_reclaims_the_discarded_parent() {
    let (mut device, kept_with, discarded) = a_file_in_a_taken_along_server_folder();
    let (h, s, f): (NodeId, NodeId, NodeId) = ("h".into(), "s".into(), "f".into());
    let mut server_file = device.trees.remote().get(&f).unwrap().clone();
    server_file.shape.parent = s.clone();
    device.trees.set_remote(f.clone(), server_file.clone());
    let server_dir = device.trees.remote().get(&s).unwrap().shape.clone();
    device.trees.set_synced(s.clone(), Some(server_dir));
    device.trees.set_synced(f.clone(), Some(server_file.shape));
    for id in [&f, &s] {
        let purged = koidrive_proto::v1::NodePurged {
            node_id: id.to_string(),
            ..Default::default()
        };
        let change = Some(change_record::Change::Purged(purged));
        device.receive(&[ChangeRecord { change }], false);
    }
    let asked = BTreeSet::new();
    assert!(!revive(&device, &discarded, &kept_with, &asked).contains(&h));
    device.act(&act(2, 0, 0, 0, 99));
    assert!(acted_below(&device, &s, &kept_with, &discarded));
    assert!(revive(&device, &discarded, &kept_with, &asked).contains(&h));
}

#[test]
fn a_removal_counts_against_a_folder_only_when_the_server_holds_the_entry_inside_it() {
    for (p_in, s_in, counts) in [("s", "root", true), ("root", "p", false)] {
        let mut device = Device::new(Trees::new("root"));
        place_dir(&mut device, "s", Some(("root", "s")), Some((s_in, "s")));
        place_dir(&mut device, "p", None, Some((p_in, "p")));
        let shape = device.trees.remote().live(&"p".into()).cloned();
        device.trees.set_synced("p".into(), shape);
        device.removed_from.insert("p".into(), "s".into());
        let removed = removed_by_user_from(&device, &"s".into());
        assert_eq!(removed, counts, "{p_in}");
    }
}

#[test]
fn renaming_away_from_a_conflict_name_the_device_applied_is_acted_below_the_folder() {
    let (mut device, mut kept_with, discarded) = a_file_in_a_taken_along_server_folder();
    let f: NodeId = "f".into();
    let mut remote = device.trees.remote().get(&f).unwrap().clone();
    remote.shape.parent = "s".into();
    remote.shape.name = conflict_name("a", "me", NOW, 1);
    let mut local_file = device.trees.local().get(&f).unwrap().clone();
    local_file.shape = remote.shape.clone();
    device.trees.set_local(f.clone(), local_file);
    device
        .trees
        .set_synced(f.clone(), Some(remote.shape.clone()));
    kept_with.insert(f.clone(), remote.shape.clone());
    device.trees.set_remote(f.clone(), remote);
    device.act(&act(3, 0, 2, 0, 0));
    assert_eq!(device.trees.local().get(&f).unwrap().shape.name, "a");
    assert!(acted_below(&device, &"s".into(), &kept_with, &discarded));
}

#[test]
fn any_server_conflict_name_the_device_has_not_applied_yet_is_no_change() {
    let marker = conflict_name("", "me", NOW, 1);
    for (name, attempt) in [
        ("a".to_string(), 4),
        ("a".to_string(), 12),
        (format!("a{marker}.txt"), 4),
        (format!("a.x{marker}"), 4),
    ] {
        let (mut device, kept_with, discarded) = a_file_in_a_taken_along_server_folder();
        let f: NodeId = "f".into();
        let mut remote = device.trees.remote().get(&f).unwrap().clone();
        remote.shape.parent = "s".into();
        remote.shape.name = conflict_name(&name, "me", NOW, attempt);
        device.act(&act(3, 0, 2, 0, 0));
        let mut local_file = device.trees.local().get(&f).unwrap().clone();
        local_file.shape.name = name;
        device.trees.set_local(f.clone(), local_file);
        device.trees.set_remote(f.clone(), remote);
        device.trees.revert(f);
        assert!(!acted_below(&device, &"s".into(), &kept_with, &discarded));
    }
}

fn place_dir(
    device: &mut Device,
    id: &str,
    local: Option<(&str, &str)>,
    server: Option<(&str, &str)>,
) {
    let shape = |(parent, name): (&str, &str)| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    };
    if let Some(at) = local {
        let identity = device.identity();
        device.trees.set_local(
            id.into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: shape(at),
                identity,
                parked: false,
                chunks: Box::default(),
            },
        );
    }
    if let Some(at) = server {
        device.trees.set_remote(
            id.into(),
            RemoteNode {
                made_over: Vec::new(),
                shape: shape(at),
                trashed: false,
                content_rev: 1,
                meta_rev: 1,
                meta_edit_rev: 1,
                trash_group: String::new(),
                version_id: String::new(),
                edit_rev: 1,
                pending_for: None,
                pending_by: None,
                chunks: Box::default(),
            },
        );
    }
}

#[test]
fn a_removal_is_judged_through_a_server_chain_longer_than_the_local_tree() {
    let mut device = Device::new(Trees::new("root"));
    place_dir(&mut device, "k", Some(("root", "k")), Some(("root", "k")));
    place_dir(&mut device, "e", None, Some(("root", "e")));
    device.removed_from.insert("e".into(), "k".into());
    let mut baseline = BTreeMap::new();
    let mut below = "e".to_string();
    for depth in (1..=8).rev() {
        let folder = format!("s{depth}");
        baseline.insert(below.into(), (&folder).into());
        below = folder;
    }
    baseline.insert(below.into(), "k".into());
    device
        .on_server_when_taken_along
        .insert("k".into(), baseline);
    device.acted.insert("e".into());
    let kept_with = BTreeMap::from([(
        "k".into(),
        device.trees.local().get(&"k".into()).unwrap().shape.clone(),
    )]);
    assert!(acted_below(
        &device,
        &"k".into(),
        &kept_with,
        &BTreeMap::new()
    ));
}

#[test]
fn a_change_inside_a_discarded_held_folder_is_not_acted_below_the_server_folder_that_holds_it() {
    let mut device = Device::new(Trees::new("root"));
    let folder = |parent: &str, name: &str| koidrive_engine::tree::Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    };
    for (id, parent) in [("s", "root"), ("h", "s"), ("n", "h")] {
        let identity = device.identity();
        device.trees.set_local(
            id.into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: folder(parent, id),
                identity,
                parked: false,
                chunks: Box::default(),
            },
        );
    }
    let server = device.trees.local().get(&"s".into()).unwrap().shape.clone();
    device.trees.set_remote(
        "s".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: server,
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
            chunks: Box::default(),
        },
    );
    let discarded = BTreeMap::from([("h".into(), folder("s", "h"))]);
    device.acted.insert("n".into());
    assert!(!acted_below(
        &device,
        &"s".into(),
        &BTreeMap::new(),
        &discarded
    ));
    device.acted.insert("h".into());
    assert!(acted_below(
        &device,
        &"s".into(),
        &BTreeMap::new(),
        &discarded
    ));
}

#[test]
fn a_first_link_discard_judges_a_taken_along_folder_against_what_it_held_at_the_discard() {
    let seed = [
        act(1, 0, 0, 3, 0),
        act(1, 0, 0, 3, 0),
        act(0, 0, 0, 2, 0),
        act(0, 0, 2735742931768910670, 1, 0),
    ];
    let local = [
        act(0, 0, 0, 3, 0),
        act(1, 0, 11875311943370024282, 5, 0),
        act(0, 0, 3047016019247648468, 0, 0),
        act(1, 0, 6661243561629110612, 2, 0),
        act(0, 0, 5628054386509540524, 2, 0),
    ];
    let (during, rulings) = the_server_drops_part_of_a_discarded_folder(
        vec![(
            true,
            vec![
                act(0, 0, 6560548476402552503, 0, 0),
                act(3, 12522830786365722604, 11093942931774494242, 0, 0),
            ],
        )],
        act(8, 0, 0, 0, 0),
        vec![(
            true,
            vec![act(3, 12633224309107184393, 13808278524712180971, 0, 0)],
        )],
        act(7, 3415821465429094374, 0, 0, 0),
        act(9, 0, 0, 0, 2),
        vec!["a"],
    );
    let ran = run_ignoring_answering_after(
        &seed,
        &local,
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
    let root = ran.space.space_id().to_owned();
    let live = koidrive_proto::v1::NodeStatus::Live as i32;
    let nodes: BTreeMap<String, _> = (ran.space.nodes().into_iter())
        .map(|node| (node.node_id.clone(), node))
        .collect();
    let taken = &nodes["01a1c2c0-39a0-796f-a8a3-82c9a31d6efe"];
    assert_eq!(
        (taken.status, taken.parent_id == root, taken.name.as_str()),
        (live, true, "b")
    );
    assert!(!nodes.contains_key("01a1c2c0-39a0-7768-bfda-be86cbbeaa11"));
    assert!(!nodes.contains_key("01a1c2c0-39a0-78c3-b5d8-1f333a1fb9e9"));
    assert_ne!(nodes["01a1c2c0-39a0-7c9f-92dd-37ded7172a70"].status, live);
    assert_eq!(ran.deletes, 1);
}

fn an_edit_moved_into_b(b_is_a_node: bool, moved: bool) -> Device {
    let mut device = Device::new(Trees::new("root"));
    device.rules.file = vec!["b"];
    device.rules.read(&mut device.trees);
    let b = b_is_a_node.then_some(("b", "p", "b", None));
    let entries = [
        Some(("a", "root", "a", None)),
        b,
        Some(("c", "a", "c.txt", Some(content(2, 0)))),
        Some(("p", "root", "p", None)),
    ];
    for (id, parent, name, content) in entries.into_iter().flatten() {
        let kind = content.as_ref().map_or(Kind::Dir, |_| Kind::File);
        let (content, chunks) = split(content);
        let shape = koidrive_engine::tree::Shape {
            parent: parent.into(),
            name: name.into(),
            kind,
            content,
        };
        let identity = device.identity();
        device.trees.set_local(
            id.into(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: shape.clone(),
                identity,
                parked: false,
                chunks,
            },
        );
        if id != "b" {
            device.trees.set_synced(id.into(), Some(shape));
        }
    }
    let (c, into) = if b_is_a_node { (2, 2) } else { (1, 3) };
    device.act(&act(2, c, 0, 0, 1));
    if !b_is_a_node {
        device.act(&act(0, 0, 2, 2, 0));
    }
    if moved {
        device.act(&act(3, c, into, 3, 0));
    }
    device
}

#[test]
fn a_folder_holds_on_disk_the_ignored_folder_the_user_moved_a_copy_into_and_not_the_copy_that_left()
{
    let device = an_edit_moved_into_b(false, true);
    let b = ("b".to_owned(), Some(Kind::Dir));
    assert_eq!(held_on_disk(&device, &"p".into(), &[]), [b]);
    assert_eq!(held_on_disk(&device, &"a".into(), &[]), []);
}

#[test]
fn a_node_moved_into_an_ignored_folder_that_is_a_node_is_placed_there() {
    let device = an_edit_moved_into_b(true, true);
    let c = "c".into();
    let moved = &device.trees.local().get(&c).unwrap().shape;
    assert_eq!(moved.parent, "b");
    assert_eq!(moved.name, "c.txt");
    assert!(device.trees.excluded(&c) && !device.trees.recorded_local(&c));
    assert!(device.ignored.is_empty());
}

#[test]
fn a_folder_made_where_the_rules_leave_out_only_folders_stays_off_the_modelled_disk() {
    let mut device = an_edit_moved_into_b(true, false);
    device.rules.file = vec!["b/"];
    device.rules.read(&mut device.trees);
    let before = device.trees.local().iter().count();
    device.act(&act(0, 0, 0, 2, 0));
    assert!(device.ignored.is_empty());
    assert_eq!(device.trees.local().iter().count(), before);
}

#[test]
fn a_node_moved_into_an_ignored_folder_that_is_no_node_keeps_its_entry() {
    let device = an_edit_moved_into_b(false, true);
    let c = "c".into();
    assert_eq!(device.trees.local().get(&c).unwrap().shape.parent, "a");
    assert!(device.trees.recorded_local(&c) && !device.on_disk(&c));
    assert_eq!(
        device.ignored[&unobserved_edit()].content,
        Some(key(&content(1, 1)))
    );
}

fn unobserved_edit() -> DiskPath {
    ("p".into(), vec!["b".into(), "c.txt".into()])
}

fn local_writes_check_after_an_edit(moved: bool) -> Result<(), TestCaseError> {
    let mut device = an_edit_moved_into_b(false, moved);
    device.trees.remove_local(&"c".into());
    let server = [("a".into(), None), ("c".into(), None)];
    let edit = written(std::iter::once(Some(key(&content(1, 1)))), 1);
    local_writes_check(&device, &server, &BTreeSet::new(), &edit)
}

#[test]
fn an_edit_whose_copy_the_user_moved_into_an_excluded_folder_is_kept() {
    assert!(local_writes_check_after_an_edit(true).is_ok());
}

#[test]
fn an_edit_that_left_l_outside_any_excluded_folder_is_a_lost_write() {
    let error = local_writes_check_after_an_edit(false).unwrap_err();
    assert!(error.to_string().contains("a local write was lost"));
}

#[test]
fn a_write_in_a_parked_folder_is_kept_even_if_its_old_parent_is_now_inside_it() {
    let mut device = Device::new(Trees::new("root"));
    for (ino, id, parent, name, kind, parked) in [
        (1, "parked", "inner", "c.txt", Kind::Dir, true),
        (2, "inner", "parked", "a", Kind::Dir, false),
        (3, "write", "inner", "a", Kind::File, false),
    ] {
        let (content, chunks) = split((kind == Kind::File).then(|| content(1, 0)));
        device.trees.set_local(
            id.into(),
            LocalNode {
                shape: Shape {
                    parent: parent.into(),
                    name: name.into(),
                    kind,
                    content,
                },
                identity: identity(ino),
                parked,
                left_out_when_placed: false,
                content_from_server: false,
                chunks,
            },
        );
        if kind == Kind::Dir {
            let (id, node) = RemoteNode::from_state(&NodeState {
                node_id: id.into(),
                parent_id: if parked { "root" } else { parent }.into(),
                name: name.into(),
                kind: koidrive_proto::v1::NodeKind::Dir as i32,
                status: koidrive_proto::v1::NodeStatus::Live as i32,
                ..Default::default()
            })
            .unwrap();
            let shape = node.shape.clone();
            device.trees.set_synced(id.clone(), Some(shape));
            device.trees.set_remote(id, node);
        }
    }
    device.rule(&Ruling::Include(vec!["a"]));
    let id = "write".into();
    assert_eq!(device.trees.check(), Ok(()));
    assert!(device.trees.ignored(&id) && !visible(&device.trees, &id));
    let writes = written([Some(key(&content(1, 0)))].into_iter(), 1);
    local_writes_check(&device, &[], &BTreeSet::new(), &writes).unwrap();
    device.off_disk.insert(id);
    assert!(local_writes_check(&device, &[], &BTreeSet::new(), &writes).is_err());
}

#[test]
fn an_edit_moved_into_a_folder_the_rules_let_in_counts_only_once_a_scan_finds_it() {
    let mut device = an_edit_moved_into_b(false, true);
    device.trees.remove_local(&"c".into());
    device.rules.file = Vec::new();
    device.rules.read(&mut device.trees);
    let server = [("a".into(), None), ("c".into(), None)];
    let edit = written(std::iter::once(Some(key(&content(1, 1)))), 1);
    let error = local_writes_check(&device, &server, &BTreeSet::new(), &edit).unwrap_err();
    assert!(error.to_string().contains("a local write was lost"));
}

#[test]
fn an_ignored_folder_the_rules_let_in_is_found_with_the_copy_the_user_moved_into_it() {
    let mut device = an_edit_moved_into_b(false, true);
    device.trees.remove_local(&"c".into());
    device.rule(&Ruling::File(vec![]));
    let local = device.trees.local();
    let (folder, _) = (local.iter())
        .find(|(_, node)| node.shape.parent == "p" && node.shape.name == "b")
        .unwrap();
    let found = (local.iter())
        .find(|(_, node)| node.shape.parent == *folder)
        .unwrap()
        .1;
    assert_eq!(found.shape.content, Some(key(&content(1, 1))));
    assert!(device.ignored.is_empty());
}

#[test]
fn an_edit_removed_replaced_or_moved_out_and_lost_does_not_count_as_kept() {
    let edit = written([Some(key(&content(1, 1)))].into_iter(), 1);
    let c = "c".into();
    for case in ["unlink", "replace", "move and drop", "disk copy only"] {
        let mut device = an_edit_moved_into_b(false, true);
        let path = unobserved_edit();
        assert!(local_writes_check(&device, &[], &BTreeSet::new(), &edit).is_ok());
        if case == "replace" {
            device.ignored.get_mut(&path).unwrap().content = Some(key(&content(2, 2)));
        } else {
            let entry = device.ignored.remove(&path).unwrap();
            if case == "move and drop" {
                device
                    .ignored
                    .insert(("a".into(), vec!["c.txt".into()]), entry);
                device.rescan();
                assert!(device.on_disk(&c));
                assert_eq!(
                    device.trees.local().get(&c).unwrap().shape.content,
                    Some(key(&content(1, 1)))
                );
            }
            match case {
                "disk copy only" => device.rescan(),
                _ => device.trees.remove_local(&c),
            }
        }
        let result = local_writes_check(&device, &[], &BTreeSet::new(), &edit);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("a local write was lost"),
            "{case}"
        );
    }
}

#[test]
fn a_move_carries_the_physical_children_and_not_their_old_tree_entries() {
    let mut device = an_edit_moved_into_b(false, true);
    device.act(&act(0, 0, 1, 2, 0));
    device.act(&act(3, 0, 4, 0, 0));
    assert_eq!(
        device.ignored[&unobserved_edit()].content,
        Some(key(&content(1, 1)))
    );
    assert!(
        device
            .ignored
            .contains_key(&("p".into(), vec!["b".into(), "a".into(), "b".into()]))
    );
    assert!(!device.ignored.contains_key(&("a".into(), vec!["b".into()])));
    assert!(
        !device
            .ignored
            .contains_key(&("p".into(), vec!["b".into(), "a".into(), "c.txt".into()]))
    );
}

#[test]
fn two_moves_cannot_use_the_same_name_key_in_an_ignored_folder() {
    let mut device = an_edit_moved_into_b(false, true);
    device.act(&act(1, 0, 2, 5, 2));
    let before = device.ignored.len();
    device.act(&act(3, 0, 3, 4, 0));
    assert_eq!(device.ignored.len(), before);
    assert!(device.trees.local_child(&"p".into(), "ß").is_some());
}

#[test]
fn a_scan_of_a_newly_included_folder_leaves_a_still_excluded_child_on_disk() {
    let mut device = an_edit_moved_into_b(false, true);
    device.trees.remove_local(&"c".into());
    device.rule(&Ruling::File(vec!["/p/b/c.txt"]));
    let folder = device.trees.local_child(&"p".into(), "b").unwrap().clone();
    assert!(device.trees.local_child(&folder, "c.txt").is_none());
    assert_eq!(
        device.ignored[&(folder.clone(), vec!["c.txt".into()])].content,
        Some(key(&content(1, 1)))
    );
    device.rule(&Ruling::File(vec![]));
    assert!(device.trees.local_child(&folder, "c.txt").is_some());
    assert!(device.ignored.is_empty());
}

#[test]
fn deleting_around_a_hidden_copy_keeps_its_folder_and_deletes_included_children() {
    let mut device = an_edit_moved_into_b(false, true);
    device.rule(&Ruling::File(vec!["b", "a"]));
    device.act(&act(0, 0, 2, 1, 0));
    device.act(&act(1, 0, 2, 6, 0));
    let p = "p".into();
    assert!(device.trees.local_child(&p, "ss").is_some());
    assert!(device.ignored.contains_key(&(p.clone(), vec!["A".into()])));
    device.act(&act(4, 2, 0, 0, 0));
    assert!(device.trees.local().get(&p).is_some());
    assert!(device.trees.local_child(&p, "ss").is_none());
    assert!(device.ignored.contains_key(&(p, vec!["A".into()])));
    assert!(device.ignored.contains_key(&unobserved_edit()));
}

#[test]
fn a_hidden_copy_an_ancestor_rename_lets_in_is_found_by_the_next_listing() {
    let mut device = an_edit_moved_into_b(false, true);
    device.rule(&Ruling::File(vec!["/p/b"]));
    let c = "c".into();
    device.act(&act(3, 1, 0, 6, 0));
    device.trees.settle(&Default::default());
    device.list_moved();
    assert_eq!(local_path(&device.trees, &c), ["ss", "b", "c.txt"]);
    assert!(device.on_disk(&c) && device.ignored.is_empty());
}

#[test]
fn a_folder_deleted_around_only_a_moved_away_entry_leaves_nothing_on_disk() {
    let mut device = an_edit_moved_into_b(false, true);
    let a: NodeId = "a".into();
    device.act(&act(0, 0, 1, 2, 0));
    assert!(device.ignored.contains_key(&(a.clone(), vec!["b".into()])));
    device.act(&act(4, 0, 0, 0, 0));
    assert!(device.trees.local().get(&a).is_some() && !device.on_disk(&a));
    assert!(!device.ignored.keys().any(|(holder, _)| *holder == a));
    device.rule(&Ruling::File(vec![]));
    assert_eq!(local_path(&device.trees, &"c".into()), ["p", "b", "c.txt"]);
    assert!(device.trees.local().get(&a).is_none());
}

#[test]
fn a_left_out_folder_does_not_keep_a_file_waiting_if_a_numbered_conflict_name_is_included() {
    let mut device = Device::new(Trees::new("root"));
    let id = NodeId::from("file");
    let (content, chunks) = split(Some(content(1, 0)));
    device.trees.set_remote(
        id.clone(),
        RemoteNode {
            made_over: Vec::new(),
            shape: Shape {
                parent: "root".into(),
                name: "ss".into(),
                kind: Kind::File,
                content,
            },
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
            chunks,
        },
    );
    device.left_out_folders.insert("ss".into());
    device.rule(&Ruling::File(vec!["ss/", "* (conflict *-??-??)"]));
    assert!(!waits_on_left_out(&device, &id), "suffix 2 is included");
    device.rule(&Ruling::File(vec![
        "ss/",
        "* (conflict from unknown device *",
    ]));
    assert!(waits_on_left_out(&device, &id), "all suffixes are excluded");
}

#[test]
fn a_file_another_device_renames_to_a_folder_the_rules_leave_out_here_takes_a_conflict_name() {
    let parked = vec![(false, vec![act(0, 0, 0, 0, 0)])];
    for during in [vec![], parked] {
        let ran = run_ignoring(
            &[act(1, 0, 0, 0, 0)],
            &[act(0, 0, 0, 6, 0)],
            &[act(3, 0, 0, 6, 0)],
            MAX_OPS,
            false,
            &during,
            Cut::None,
            &[Ruling::Exclude(vec!["ss/"])],
            Answer::Discard,
            None,
        )
        .unwrap();
        let names: Vec<String> = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.kind == 2)
            .map(|node| node.name)
            .collect();
        assert_eq!(names, ["ss (conflict from unknown device 2026-11-09)"]);
    }
}

fn parked_file_beside_left_out_folder(conflict_rule: Option<&'static str>) -> Vec<String> {
    let patterns = |name| std::iter::once(name).chain(conflict_rule).collect();
    let ran = run_ignoring(
        &[act(1, 0, 0, 0, 0)],
        &[act(0, 0, 0, 6, 0), act(3, 0, 0, 3, 0)],
        &[act(3, 0, 0, 6, 0), act(0, 0, 0, 3, 0)],
        MAX_OPS,
        false,
        &[
            (
                false,
                vec![act(3, 487716592678468472, 4220595476152624058, 5, 0)],
            ),
            (false, vec![act(0, 0, 0, 0, 0)]),
        ],
        Cut::None,
        &[
            Ruling::Exclude(patterns("ss/")),
            Ruling::File(patterns("ss/")),
            Ruling::Exclude(patterns("*.txt")),
        ],
        Answer::Delete,
        None,
    )
    .unwrap();
    (ran.space.nodes().into_iter())
        .filter(|node| node.status == 1 && node.kind == 2)
        .map(|node| node.name)
        .collect()
}

#[test]
fn a_parked_file_whose_own_name_the_rules_leave_out_takes_a_conflict_name_when_the_disk_refuses_the_name_another_device_holds()
 {
    assert_eq!(
        parked_file_beside_left_out_folder(None),
        ["ß (conflict from unknown device 2026-11-09)"]
    );
}

#[test]
fn a_parked_file_whose_place_a_left_out_folder_took_goes_where_another_device_moved_it() {
    for name in [2, 3] {
        let ran = run_ignoring(
            &[act(1, 0, 0, 0, 0)],
            &[act(0, 0, 0, 6, 0)],
            &[act(3, 0, 0, 6, 0), act(3, 0, 0, 1, 0)],
            MAX_OPS,
            false,
            &[
                (true, vec![act(0, 0, 0, 0, 0)]),
                (false, vec![act(3, 0, 0, name, 0)]),
            ],
            Cut::None,
            &[Ruling::Exclude(vec!["ss/"]), Ruling::File(vec!["a"])],
            Answer::Delete,
            None,
        )
        .unwrap();
        let names: Vec<String> = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.kind == 2)
            .map(|node| node.name)
            .collect();
        assert_eq!(names, [NAMES[name]]);
    }
}

#[test]
fn a_parked_file_is_reported_when_the_rules_leave_out_every_conflict_name() {
    assert_eq!(
        parked_file_beside_left_out_folder(Some("* (conflict from unknown device *")),
        ["ß"]
    );
}

#[test]
fn a_synced_file_moved_into_a_new_folder_below_a_folder_renamed_out_on_the_server_is_sent_there() {
    let stories = [
        (
            vec![act(1, 0, 0, 0, 0), act(0, 0, 0, 0, 0)],
            vec![
                act(0, 0, 1439376498786130879, 0, 0),
                act(3, 1308106868217568845, 2757625929174465250, 0, 0),
            ],
            vec![
                act(3, 4302441764208420541, 5351028208616332562, 3, 0),
                act(0, 0, 0, 0, 0),
            ],
            vec![],
            vec![Ruling::Exclude(vec!["*.txt"])],
        ),
        (
            vec![act(0, 0, 0, 0, 0), act(1, 0, 3483375612010044288, 2, 0)],
            vec![act(0, 0, 449491083869513341, 0, 0)],
            vec![
                act(3, 753191141100480576, 419888708880354854, 2, 0),
                act(0, 0, 0, 0, 0),
                act(0, 0, 0, 2, 0),
            ],
            vec![(
                true,
                vec![act(3, 6273641293410837740, 14114747275198639883, 0, 0)],
            )],
            vec![Ruling::Include(vec!["a"])],
        ),
    ];
    for (seed, local, remote, during, rulings) in stories {
        let ran = run_ignoring(
            &seed,
            &local,
            &remote,
            MAX_OPS,
            true,
            &during,
            Cut::None,
            &rulings,
            Answer::Delete,
            None,
        )
        .unwrap();
        assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0));
    }
}
fn an_arrival(device: &mut Device, parent: &str) {
    device.receive(
        &[ChangeRecord {
            change: Some(change_record::Change::Node(NodeState {
                node_id: "arrival".into(),
                parent_id: parent.into(),
                name: "arrival".into(),
                kind: 1,
                status: 1,
                ..Default::default()
            })),
        }],
        false,
    );
}

#[test]
fn an_entry_the_user_creates_inside_an_arrival_makes_the_discarded_folder_the_users() {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    device.note_taken_along([&"s".into()]);
    an_arrival(&mut device, "s");
    place_dir(&mut device, "arrival", Some(("s", "arrival")), None);
    let arrival: NodeId = "arrival".into();
    let parent = 1 + device
        .trees
        .local()
        .iter()
        .filter(|(_, n)| n.shape.kind == Kind::Dir)
        .position(|(id, _)| id == &arrival)
        .unwrap();
    device.act(&act(0, 0, parent, 0, 0));
    let child = device.acted.iter().next().unwrap();
    assert_eq!(
        device.trees.local().get(child).unwrap().shape.parent,
        arrival
    );
    assert!(device.trees.remote().get(child).is_none());
    assert!(
        revive(&device, &discarded, &kept, &BTreeSet::new()).contains(&"h".into()),
        "creating a new entry under the arrival still changes s, and h should be reclaimed"
    );
}

#[test]
fn a_rename_of_an_arrival_another_device_then_moves_out_changes_that_entry_alone() {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (arrival, s): (NodeId, NodeId) = ("arrival".into(), "s".into());
    device.note_taken_along([&s]);
    an_arrival(&mut device, "s");
    place_dir(&mut device, "arrival", Some(("s", "arrival")), None);
    let fetched = device.trees.remote().live(&arrival).cloned();
    device.trees.set_synced(arrival.clone(), fetched);
    let parent = 1 + device
        .trees
        .local()
        .iter()
        .filter(|(_, n)| n.shape.kind == Kind::Dir)
        .position(|(id, _)| id == &s)
        .unwrap();
    let target = device
        .trees
        .local()
        .iter()
        .position(|(id, _)| id == &arrival)
        .unwrap();
    device.act(&act(3, target, parent, 0, 0));
    assert_eq!(device.trees.local().get(&arrival).unwrap().shape.parent, s);
    assert_ne!(
        device.trees.local().get(&arrival).unwrap().shape.name,
        "arrival"
    );
    assert!(!acted_below(&device, &s, &kept, &discarded));
    an_arrival(&mut device, "root");
    assert!(
        !revive(&device, &discarded, &kept, &BTreeSet::new()).contains(&"h".into()),
        "the user's outstanding rename was to an arrival, even after R moved the entry away"
    );
}

#[test]
fn a_rename_of_an_entry_that_came_with_an_arrival_changes_that_entry_alone_after_another_device_moves_it_out()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    place_dir(&mut device, "arrival", None, Some(("root", "arrival")));
    place_dir(&mut device, "child", None, Some(("arrival", "child")));
    device.note_taken_along([&"s".into()]);
    an_arrival(&mut device, "s");
    place_dir(&mut device, "arrival", Some(("s", "arrival")), None);
    let synced = device.trees.remote().live(&"child".into()).cloned();
    device.trees.set_synced("child".into(), synced);
    place_dir(&mut device, "child", Some(("arrival", "renamed")), None);
    device.acted_on("child".into());
    assert!(!acted_below(&device, &"s".into(), &kept, &discarded));
    device.receive(
        &[ChangeRecord {
            change: Some(change_record::Change::Node(NodeState {
                node_id: "child".into(),
                parent_id: "root".into(),
                name: "child".into(),
                kind: 1,
                status: 1,
                ..Default::default()
            })),
        }],
        false,
    );
    assert!(!revive(&device, &discarded, &kept, &BTreeSet::new()).contains(&"h".into()));
}

fn another_device_commits(
    space: &mut SpaceDo,
    ids: &mut IdGen,
    ops: Vec<op::Op>,
) -> Vec<ChangeRecord> {
    let other = session("other");
    let commit = Commit {
        commit_id: ids.uuid_v7(NOW as u64),
        history_id: space.history_id().to_owned(),
        device_seq: space.device_seq(&other.device_id) + 1,
        base_seq: space.head_seq(),
        ops: (ops.into_iter())
            .map(|op| Op {
                op: Some(op),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let result = space.commit(&other, NOW, &commit).unwrap().result;
    assert!((result.results.iter()).all(|outcome| outcome.code == op_result::Code::Applied as i32));
    result.changes
}

fn folder_moves(
    ids: &mut IdGen,
    final_parent_of_a: Option<&str>,
) -> (SpaceDo, [String; 4], [Vec<ChangeRecord>; 2]) {
    let mut space = SpaceDo::new(5, NOW);
    let root = space.space_id().to_owned();
    let [a, b, s] = [0; 3].map(|_| ids.uuid_v7(NOW as u64).to_string());
    let dir = |id: &str, parent: &str, name: &str| {
        op::Op::CreateDir(CreateDir {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            ..Default::default()
        })
    };
    let mv = |id: &str, parent: &str, name: &str| {
        op::Op::Move(Move {
            node_id: id.into(),
            new_parent_id: parent.into(),
            new_name: name.into(),
            ..Default::default()
        })
    };
    let seed = another_device_commits(
        &mut space,
        ids,
        vec![dir(&a, &root, "a"), dir(&b, &a, "b"), dir(&s, &root, "s")],
    );
    let last = final_parent_of_a.map_or(b.clone(), |_| s.clone());
    let changes = another_device_commits(
        &mut space,
        ids,
        vec![mv(&a, &root, "a2"), mv(&b, &root, "b"), mv(&a, &last, "a2")],
    );
    (space, [root, a, b, s], [seed, changes])
}

#[test]
fn a_batch_that_moves_a_folder_into_its_former_child_records_the_arrival_once_it_has_applied() {
    let mut ids = IdGen::new(7);
    let (_, [root, a, b, _], [seed, changes]) = folder_moves(&mut ids, None);
    let mut device = Device::new(Trees::new(root));
    device.receive(&seed, false);
    device.note_taken_along([&b.as_str().into()]);
    device.receive(&changes, false);
    let stays = device.stays.get(&a.as_str().into());
    assert!(
        stays
            .and_then(|stays| stays.get(&b.as_str().into()))
            .is_some_and(|(by_other, _)| *by_other)
    );
}

#[test]
fn a_child_a_batch_moves_out_before_its_folder_arrives_is_not_an_arrival() {
    let mut ids = IdGen::new(7);
    let (_, [root, a, b, s], [seed, changes]) = folder_moves(&mut ids, Some("s"));
    let mut device = Device::new(Trees::new(root.clone()));
    let (folder, child): (NodeId, NodeId) = (s.as_str().into(), b.as_str().into());
    device.receive(&seed, false);
    device.note_taken_along([&folder]);
    device.receive(&changes, false);
    let synced = device.trees.remote().live(&child).cloned();
    device.trees.set_synced(child.clone(), synced);
    place_dir(&mut device, "h", Some((&root, "h")), None);
    place_dir(&mut device, &s, Some(("h", "s")), None);
    place_dir(&mut device, &a, Some((&s, "a2")), None);
    place_dir(&mut device, &b, Some((&s, "b")), None);
    device.acted_on(child);
    let shape = |id: &NodeId| device.trees.local().get(id).unwrap().shape.clone();
    let held: NodeId = "h".into();
    let kept = BTreeMap::from([(folder.clone(), shape(&folder))]);
    let discarded = BTreeMap::from([(held.clone(), shape(&held))]);
    assert!(revive(&device, &discarded, &kept, &BTreeSet::new()).contains(&held));
}

#[test]
fn moving_in_an_entry_another_device_brought_in_and_took_out_again_makes_the_discarded_folder_the_users()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (s, arrival): (NodeId, NodeId) = ("s".into(), "arrival".into());
    device.note_taken_along([&s]);
    for parent in ["s", "root"] {
        an_arrival(&mut device, parent);
        place_dir(&mut device, "arrival", Some((parent, "arrival")), None);
        let synced = device.trees.remote().live(&arrival).cloned();
        device.trees.set_synced(arrival.clone(), synced);
    }
    assert!(!acted_below(&device, &s, &kept, &discarded));
    place_dir(&mut device, "arrival", Some(("s", "arrival")), None);
    device.acted_on(arrival);
    assert!(revive(&device, &discarded, &kept, &BTreeSet::new()).contains(&"h".into()));
}

#[test]
fn renaming_an_entry_another_device_adds_between_the_answer_and_the_build_changes_that_entry_alone()
{
    for between in [true, false] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let (s, arrival): (NodeId, NodeId) = ("s".into(), "arrival".into());
        let at_answer = device.on_server();
        let answered = (at_answer.clone(), device.placements);
        if between {
            an_arrival(&mut device, "s");
            device.note_taken_along_at_answer([&s], &answered);
        } else {
            device.note_taken_along_at_answer([&s], &answered);
            an_arrival(&mut device, "s");
        }
        place_dir(&mut device, "arrival", Some(("s", "arrival")), None);
        let synced = device.trees.remote().live(&arrival).cloned();
        device.trees.set_synced(arrival.clone(), synced);
        place_dir(&mut device, "arrival", Some(("s", "renamed")), None);
        device.acted_on(arrival);
        assert!(!acted_below(&device, &s, &kept, &discarded), "{between}");
    }
}

#[test]
fn renaming_an_entry_this_device_moved_in_and_the_server_confirmed_after_the_answer_changes_the_folder()
 {
    for confirmed_after_answer in [true, false] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let (s, g): (NodeId, NodeId) = ("s".into(), "g".into());
        place_dir(&mut device, "g", Some(("s", "g")), Some(("root", "g")));
        let confirm = |device: &mut Device| {
            let state = NodeState {
                node_id: "g".into(),
                parent_id: "s".into(),
                name: "g".into(),
                kind: 1,
                status: 1,
                ..Default::default()
            };
            let change = Some(change_record::Change::Node(state));
            device.receive(&[ChangeRecord { change }], true);
        };
        if !confirmed_after_answer {
            confirm(&mut device);
        }
        let at_answer = device.on_server();
        let answered = (at_answer.clone(), device.placements);
        if confirmed_after_answer {
            confirm(&mut device);
        }
        device.note_taken_along_at_answer([&s], &answered);
        let synced = device.trees.remote().live(&g).cloned();
        device.trees.set_synced(g.clone(), synced);
        place_dir(&mut device, "g", Some(("s", "renamed")), None);
        device.acted_on(g);
        assert!(
            acted_below(&device, &s, &kept, &discarded),
            "{confirmed_after_answer}"
        );
    }
}

fn a_dir_record(id: &str, parent: &str, meta_rev: u64) -> ChangeRecord {
    ChangeRecord {
        change: Some(change_record::Change::Node(NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: id.into(),
            kind: 1,
            status: 1,
            meta_rev,
            ..Default::default()
        })),
    }
}

fn renames_after_sync(device: &mut Device, id: &str, parent: &str) {
    let node: NodeId = id.into();
    let synced = device.trees.remote().live(&node).cloned();
    device.trees.set_synced(node.clone(), synced);
    place_dir(device, id, Some((parent, "renamed")), None);
    device.acted_on(node);
}

#[test]
fn renaming_an_entry_this_device_moved_in_after_another_device_changed_it_in_place_changes_the_folder()
 {
    for edited_by_another in [false, true] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let s: NodeId = "s".into();
        place_dir(&mut device, "g", Some(("s", "g")), Some(("root", "g")));
        let answered = (device.on_server(), device.placements);
        if edited_by_another {
            device.receive(&[a_dir_record("g", "root", 2)], false);
        }
        device.receive(&[a_dir_record("g", "s", 3)], true);
        device.note_taken_along_at_answer([&s], &answered);
        renames_after_sync(&mut device, "g", "s");
        assert!(
            acted_below(&device, &s, &kept, &discarded),
            "{edited_by_another}"
        );
    }
}

#[test]
fn renaming_an_entry_this_device_moved_in_after_another_device_edits_it_in_place_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "g", Some(("s", "g")), Some(("root", "g")));
    device.note_taken_along([&s]);
    device.receive(&[a_dir_record("g", "s", 2)], true);
    device.receive(&[a_dir_record("g", "s", 3)], false);
    renames_after_sync(&mut device, "g", "s");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_arrival_this_device_moved_out_and_back_changes_the_folder() {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    device.note_taken_along([&s]);
    device.receive(&[a_dir_record("g", "s", 1)], false);
    for (parent, meta_rev) in [("root", 2), ("s", 3)] {
        place_dir(&mut device, "g", Some((parent, "g")), None);
        device.receive(&[a_dir_record("g", parent, meta_rev)], true);
    }
    renames_after_sync(&mut device, "g", "s");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_this_device_put_in_a_folder_before_another_device_brought_the_folder_in_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "x", Some(("root", "x")), Some(("root", "x")));
    place_dir(&mut device, "g", Some(("x", "g")), Some(("root", "g")));
    device.receive(&[a_dir_record("g", "x", 2)], true);
    device.note_taken_along([&s]);
    place_dir(&mut device, "x", Some(("s", "x")), None);
    device.receive(&[a_dir_record("x", "s", 2)], false);
    renames_after_sync(&mut device, "g", "x");
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_arrival_this_device_moved_within_the_folder_changes_that_entry_alone() {
    for (came_with_a_folder, confirmed) in
        [(true, false), (true, true), (false, false), (false, true)]
    {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let s: NodeId = "s".into();
        place_dir(&mut device, "m", Some(("s", "m")), Some(("s", "m")));
        device.note_taken_along([&s]);
        let (from, to) = if came_with_a_folder {
            device.receive(
                &[a_dir_record("a", "s", 1), a_dir_record("c", "a", 1)],
                false,
            );
            place_dir(&mut device, "a", Some(("s", "a")), None);
            let synced = device.trees.remote().live(&"a".into()).cloned();
            device.trees.set_synced("a".into(), synced);
            ("a", "s")
        } else {
            device.receive(&[a_dir_record("c", "s", 1)], false);
            ("s", "m")
        };
        place_dir(&mut device, "c", Some((from, "c")), None);
        let synced = device.trees.remote().live(&"c".into()).cloned();
        device.trees.set_synced("c".into(), synced);
        place_dir(&mut device, "c", Some((to, "c")), None);
        if confirmed {
            device.receive(&[a_dir_record("c", to, 2)], true);
        } else {
            device.acted_on("c".into());
        }
        renames_after_sync(&mut device, "c", to);
        assert!(
            !acted_below(&device, &s, &kept, &discarded),
            "{came_with_a_folder} {confirmed}"
        );
    }
}

#[test]
fn renaming_an_entry_that_came_with_an_arrival_after_this_device_moved_it_out_and_back_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    device.note_taken_along([&s]);
    device.receive(
        &[a_dir_record("a", "s", 1), a_dir_record("c", "a", 1)],
        false,
    );
    place_dir(&mut device, "a", Some(("s", "a")), None);
    for (parent, meta_rev) in [("a", 1), ("root", 2), ("s", 3)] {
        place_dir(&mut device, "c", Some((parent, "c")), None);
        if meta_rev > 1 {
            device.receive(&[a_dir_record("c", parent, meta_rev)], true);
        }
    }
    renames_after_sync(&mut device, "c", "s");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_another_device_adds_in_the_answer_window_and_this_device_moves_within_the_folder_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "m", Some(("s", "m")), Some(("s", "m")));
    let answered = (device.on_server(), device.placements);
    device.receive(&[a_dir_record("c", "s", 1)], false);
    place_dir(&mut device, "c", Some(("m", "c")), None);
    device.receive(&[a_dir_record("c", "m", 2)], true);
    device.note_taken_along_at_answer([&s], &answered);
    renames_after_sync(&mut device, "c", "m");
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_this_device_moved_into_an_outside_folder_another_device_then_brings_in_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "y", Some(("root", "y")), Some(("root", "y")));
    place_dir(&mut device, "c", Some(("y", "c")), Some(("root", "c")));
    device.note_taken_along([&s]);
    device.receive(&[a_dir_record("c", "y", 2)], true);
    place_dir(&mut device, "y", Some(("s", "y")), None);
    device.receive(&[a_dir_record("y", "s", 2)], false);
    renames_after_sync(&mut device, "c", "y");
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_arrival_this_device_moved_into_a_folder_it_then_brought_in_changes_the_folder() {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "y", Some(("root", "y")), Some(("root", "y")));
    device.note_taken_along([&s]);
    device.receive(&[a_dir_record("c", "s", 1)], false);
    place_dir(&mut device, "c", Some(("y", "c")), None);
    device.receive(&[a_dir_record("c", "y", 2)], true);
    place_dir(&mut device, "y", Some(("s", "y")), None);
    device.receive(&[a_dir_record("y", "s", 2)], true);
    renames_after_sync(&mut device, "c", "y");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_the_user_moved_in_that_another_device_moved_to_the_same_place_changes_the_folder()
 {
    for other_first in [true, false] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let (s, e): (NodeId, NodeId) = ("s".into(), "e".into());
        place_dir(&mut device, "e", Some(("root", "e")), Some(("root", "e")));
        device.note_taken_along([&s]);
        let before = device.trees.local().get(&e).unwrap().shape.clone();
        place_dir(&mut device, "e", Some(("s", "e")), None);
        let after = device.trees.local().get(&e).unwrap().shape.clone();
        device.relocate(&e, &before, &after);
        let (first, second) = (a_dir_record("e", "s", 2), a_dir_record("e", "s", 3));
        if other_first {
            device.receive(&[first], false);
            device.receive(&[second], true);
        } else {
            device.receive(&[first], true);
            device.receive(&[second], false);
        }
        renames_after_sync(&mut device, "e", "s");
        assert!(acted_below(&device, &s, &kept, &discarded), "{other_first}");
    }
}

#[test]
fn renaming_an_entry_another_device_put_in_a_folder_before_this_device_brought_the_folder_in_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "x", Some(("root", "x")), Some(("root", "x")));
    place_dir(&mut device, "g", Some(("x", "g")), Some(("root", "g")));
    device.receive(&[a_dir_record("g", "x", 2)], false);
    device.note_taken_along([&s]);
    place_dir(&mut device, "x", Some(("s", "x")), None);
    device.receive(&[a_dir_record("x", "s", 2)], true);
    renames_after_sync(&mut device, "g", "x");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_another_device_moved_into_a_folder_this_device_then_brought_in_changes_the_folder()
 {
    for in_window in [false, true] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let s: NodeId = "s".into();
        place_dir(&mut device, "y", Some(("s", "y")), Some(("root", "y")));
        place_dir(&mut device, "m", Some(("y", "m")), Some(("root", "m")));
        let answered = (device.on_server(), device.placements);
        if !in_window {
            device.note_taken_along([&s]);
        }
        device.receive(&[a_dir_record("m", "y", 2)], false);
        device.receive(&[a_dir_record("y", "s", 2)], true);
        if in_window {
            device.note_taken_along_at_answer([&s], &answered);
        }
        renames_after_sync(&mut device, "m", "y");
        assert!(acted_below(&device, &s, &kept, &discarded), "{in_window}");
    }
}

#[test]
fn renaming_an_entry_that_left_the_folder_with_its_ancestor_and_came_back_with_this_devices_move_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "z", Some(("root", "z")), Some(("root", "z")));
    device.note_taken_along([&s]);
    device.receive(
        &[a_dir_record("y", "s", 1), a_dir_record("m", "y", 1)],
        false,
    );
    place_dir(&mut device, "y", Some(("z", "y")), None);
    place_dir(&mut device, "m", Some(("y", "m")), None);
    device.receive(&[a_dir_record("y", "z", 2)], false);
    for id in ["y", "m"] {
        let synced = device.trees.remote().live(&id.into()).cloned();
        device.trees.set_synced(id.into(), synced);
    }
    place_dir(&mut device, "z", Some(("s", "z")), None);
    device.receive(&[a_dir_record("z", "s", 2)], true);
    renames_after_sync(&mut device, "m", "y");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_this_device_brought_in_after_another_device_moves_it_out_unsynced_changes_the_folder()
 {
    for later in [None, Some("z"), Some("root")] {
        let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
        let s: NodeId = "s".into();
        place_dir(&mut device, "z", Some(("root", "z")), Some(("root", "z")));
        place_dir(&mut device, "y", Some(("z", "y")), Some(("z", "y")));
        device.note_taken_along([&s]);
        place_dir(&mut device, "y", Some(("s", "y")), None);
        device.receive(&[a_dir_record("y", "s", 2)], true);
        let synced = device.trees.remote().live(&"y".into()).cloned();
        device.trees.set_synced("y".into(), synced);
        device.receive(&[a_dir_record("y", "z", 3)], false);
        if let Some(parent) = later {
            device.receive(&[a_dir_record("y", parent, 4)], false);
        }
        place_dir(&mut device, "y", Some(("s", "renamed")), None);
        device.acted_on("y".into());
        assert!(acted_below(&device, &s, &kept, &discarded), "{later:?}");
    }
}

#[test]
fn renaming_an_entry_the_user_moved_in_that_another_device_moved_elsewhere_in_the_folder_first_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (s, e): (NodeId, NodeId) = ("s".into(), "e".into());
    for id in ["a", "b"] {
        place_dir(&mut device, id, Some(("s", id)), Some(("s", id)));
    }
    place_dir(&mut device, "e", Some(("root", "e")), Some(("root", "e")));
    device.note_taken_along([&s]);
    let before = device.trees.local().get(&e).unwrap().shape.clone();
    place_dir(&mut device, "e", Some(("a", "e")), None);
    let after = device.trees.local().get(&e).unwrap().shape.clone();
    device.relocate(&e, &before, &after);
    device.receive(&[a_dir_record("e", "b", 2)], false);
    device.receive(&[a_dir_record("e", "a", 3)], true);
    renames_after_sync(&mut device, "e", "a");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_that_came_with_an_arrival_after_this_device_moves_it_within_the_folder_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "y", Some(("root", "y")), Some(("root", "y")));
    place_dir(&mut device, "c", Some(("y", "c")), Some(("y", "c")));
    place_dir(&mut device, "m", Some(("s", "m")), Some(("s", "m")));
    device.note_taken_along([&s]);
    place_dir(&mut device, "y", Some(("s", "y")), None);
    device.receive(&[a_dir_record("y", "s", 2)], false);
    place_dir(&mut device, "c", Some(("m", "c")), None);
    device.receive(&[a_dir_record("c", "m", 2)], true);
    renames_after_sync(&mut device, "c", "m");
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_arrival_another_device_moves_into_a_folder_this_device_brought_in_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let s: NodeId = "s".into();
    place_dir(&mut device, "y", Some(("root", "y")), Some(("root", "y")));
    device.note_taken_along([&s]);
    device.receive(&[a_dir_record("a", "s", 1)], false);
    place_dir(&mut device, "y", Some(("s", "y")), None);
    device.receive(&[a_dir_record("y", "s", 2)], true);
    place_dir(&mut device, "a", Some(("y", "a")), None);
    device.receive(&[a_dir_record("a", "y", 2)], false);
    renames_after_sync(&mut device, "a", "y");
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn deleting_an_entry_another_device_moves_out_between_the_answer_and_the_build_changes_the_folder()
{
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (s, e): (NodeId, NodeId) = ("s".into(), "e".into());
    place_dir(&mut device, "e", Some(("s", "e")), Some(("s", "e")));
    let answered = (device.on_server(), device.placements);
    device.receive(&[a_dir_record("e", "root", 2)], false);
    device.note_taken_along_at_answer([&s], &answered);
    device.trees.remove_local(&e);
    device.removed_from.insert(e.clone(), s.clone());
    device.acted_on(e);
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn deleting_an_entry_another_device_brings_in_with_its_folder_in_the_answer_window_after_this_device_moves_it_within_changes_that_entry_alone()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (s, e): (NodeId, NodeId) = ("s".into(), "e".into());
    place_dir(&mut device, "a", Some(("root", "a")), Some(("root", "a")));
    place_dir(&mut device, "e", Some(("a", "e")), Some(("a", "e")));
    place_dir(&mut device, "m", Some(("s", "m")), Some(("s", "m")));
    let answered = (device.on_server(), device.placements);
    device.receive(&[a_dir_record("a", "s", 2)], false);
    place_dir(&mut device, "e", Some(("m", "e")), None);
    device.receive(&[a_dir_record("e", "m", 2)], true);
    device.note_taken_along_at_answer([&s], &answered);
    device.trees.remove_local(&e);
    device.removed_from.insert(e.clone(), "m".into());
    device.acted_on(e);
    assert!(!acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn renaming_an_entry_the_user_moved_in_from_a_folder_another_device_then_brings_in_changes_the_folder()
 {
    let (mut device, kept, discarded) = a_file_in_a_taken_along_server_folder();
    let (s, e): (NodeId, NodeId) = ("s".into(), "e".into());
    place_dir(&mut device, "a", Some(("root", "a")), Some(("root", "a")));
    place_dir(&mut device, "e", Some(("a", "e")), Some(("a", "e")));
    device.note_taken_along([&s]);
    let before = device.trees.local().get(&e).unwrap().shape.clone();
    place_dir(&mut device, "e", Some(("s", "e")), None);
    let after = device.trees.local().get(&e).unwrap().shape.clone();
    device.relocate(&e, &before, &after);
    place_dir(&mut device, "a", Some(("s", "a")), None);
    device.receive(&[a_dir_record("a", "s", 2)], false);
    device.receive(&[a_dir_record("e", "s", 2)], true);
    renames_after_sync(&mut device, "e", "s");
    assert!(acted_below(&device, &s, &kept, &discarded));
}

#[test]
fn a_write_held_by_a_node_displaced_during_an_action_stays_counted() {
    let mut trees = Trees::new("root");
    let file = NodeId::from("f");
    let written = content(1, 1);
    let shape = koidrive_engine::tree::Shape {
        parent: "root".into(),
        name: "a".into(),
        kind: Kind::File,
        content: Some(ContentKey::try_from(&written).unwrap()),
    };
    let identity = FileIdentity {
        ino: 1,
        birth_ns: Some(1),
        handle: None,
    };
    trees.set_local(
        file.clone(),
        LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity,
            parked: false,
            chunks: written.chunks.clone().into(),
        },
    );
    trees.set_synced(file.clone(), Some(shape.clone()));
    trees.set_remote(
        file.clone(),
        RemoteNode {
            made_over: Vec::new(),
            chunks: written.chunks.clone().into(),
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
    trees.ignore_local(&file);
    let mut local_writes = BTreeSet::from([written.manifest_digest.clone()]);
    let mut remote_writes = BTreeSet::new();
    let before = held(&trees);
    trees.displace_local(&file);
    assert!(trees.local().get(&file).is_none());
    let after = held(&trees);
    rewritten(
        &before,
        &after,
        &trees,
        [&mut local_writes, &mut remote_writes],
    );
    assert!(local_writes.contains(&written.manifest_digest));
}
