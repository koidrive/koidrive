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
    assert!(acted_below(&device, &"k".into(), &BTreeMap::new()));
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

fn frozen_name_send(
    held: usize,
    claim: usize,
    folder: bool,
    text: u8,
) -> Result<(), TestCaseError> {
    let (space, _) = seeded(&[
        act(0, 0, 0, held, 0),
        act(0, 0, 0, 3, 0),
        act(u8::from(!folder), 0, 0, 2, text),
    ]);
    let root = space.space_id();
    let mut device = Device::new(Trees::new(root));
    let mut ids = BTreeMap::new();
    for state in space.nodes().iter().filter(|state| state.node_id != root) {
        let (id, remote) = RemoteNode::from_state(state).unwrap();
        ids.insert(state.name.clone(), id.clone());
        let identity = device.identity();
        device.trees.set_local(
            id.clone(),
            LocalNode {
                shape: remote.shape.clone(),
                identity,
                parked: false,
                chunks: remote.chunks.clone(),
                content_from_server: true,
                left_out_when_placed: false,
            },
        );
        device
            .trees
            .set_synced(id.clone(), Some(remote.shape.clone()));
        device.trees.set_remote(id, remote);
    }
    device.rule(&Ruling::Exclude(vec!["*.txt"]));
    let holder = &ids[NAMES[held]];
    let id = &ids["b"];
    let mut moved = device.trees.remote().get(holder).unwrap().clone();
    moved.shape.parent = ids["c.txt"].clone();
    moved.meta_rev += 1;
    device.trees.set_remote(holder.clone(), moved);
    let mut wanted = device.trees.remote().get(id).unwrap().clone();
    wanted.shape.name = NAMES[claim].into();
    wanted.meta_rev += 1;
    device.trees.set_remote(id.clone(), wanted.clone());
    let mut local = device.trees.local().get(id).unwrap().clone();
    local.shape = wanted.shape;
    device.trees.set_local_changed(id.clone(), local);

    let mut commits = Commits::default();
    let trees = &mut device.trees;
    trees.settle(commits.aliases());
    prop_assert_eq!(trees.check(), Ok(()));
    let mut before = trees.clone();
    let built = commits.build(trees, &mut device.ids, NOW as u64, "history", "me", 1u64);
    let sent = built.and_then(|commit| {
        commit.ops.iter().find_map(|op| match &op.op {
            Some(op::Op::Move(moved)) if moved.node_id == *id => Some(moved),
            _ => None,
        })
    });
    let Some(sent) = sent else {
        return Err(TestCaseError::fail(format!(
            "no rename for {}",
            NAMES[claim]
        )));
    };
    prop_assert_eq!(&sent.new_name, &conflict_name(NAMES[claim], "", NOW, 1));
    prop_assert!(!trees.name_left_out(id));
    sent_move_check(
        sent,
        &trees.local().get(id).unwrap().shape,
        trees.synced().get(id),
        trees.remote().get(id),
        "me",
        &BTreeMap::new(),
        sent_move_refusal(&before, trees, id).as_ref(),
    )?;
    before.set_synced(holder.clone(), None);
    prop_assert!(sent_move_refusal(&before, trees, id).is_none());
    Ok(())
}
