// SPDX-License-Identifier: Apache-2.0

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
