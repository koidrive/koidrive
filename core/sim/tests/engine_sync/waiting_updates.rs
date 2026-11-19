// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_server_file_moved_into_a_held_folder_while_its_s_update_waits_is_not_held() {
    let seed = [
        act(0, 0, 0, 2, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
    ];
    let local = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 3, 0, 0),
    ];
    let during = [
        (
            false,
            vec![act(4, 0, 0, 0, 0), act(4, 1, 0, 0, 0), act(1, 0, 0, 2, 0)],
        ),
        (true, vec![act(3, 0, 1, 0, 0), act(2, 1, 0, 0, 0)]),
        (true, vec![act(4, 0, 0, 0, 0)]),
    ];
    for answer in [Answer::Publish, Answer::Discard] {
        run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &during,
            Cut::None,
            &[],
            answer,
            Some(PriorSync::Shown),
        )
        .unwrap();
    }
}

#[test]
fn a_second_version_restore_waits_until_s_has_the_first() {
    let seed = [
        act(0, 0, 0, 2, 0),
        act(0, 0, 10193068882185749263, 0, 0),
        act(0, 0, 15322717350651070711, 2, 0),
        act(0, 0, 7458645619147002727, 0, 0),
        act(1, 0, 5780529924190365195, 0, 0),
        act(1, 0, 1139158932485848356, 0, 0),
        act(0, 0, 16173846703536834397, 0, 0),
    ];
    let local = [
        act(4, 6149728382812320256, 0, 0, 0),
        act(1, 0, 0, 3, 0),
        act(0, 0, 4089352059523475543, 0, 0),
        act(0, 0, 0, 0, 0),
    ];
    let remote = [
        act(4, 10301859415978009313, 0, 0, 0),
        act(2, 0, 0, 0, 0),
        act(2, 0, 0, 0, 0),
    ];
    let during = [
        (false, vec![act(3, 0, 0, 0, 0)]),
        (true, vec![act(6, 0, 0, 2, 0)]),
        (false, vec![act(3, 0, 0, 2, 0)]),
        (true, vec![act(6, 0, 0, 1, 0)]),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &remote, 2, true, &during, cut).unwrap();
        let outcomes: Vec<_> = ran.resolved.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes, [&Outcome::Restored; 2], "{cut:?}");
        let last = &ran.resolved[1].intent;
        let versions = versions(&ran.space, &last.node().to_string());
        let asked = versions
            .iter()
            .find(|v| Some(v.version_id.as_str()) == last.version_id());
        assert_eq!(versions[0].content, asked.unwrap().content, "{cut:?}");
    }
}

#[test]
fn a_parked_file_whose_server_update_waits_is_removed_when_the_server_deletes_it() {
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
            name: 3,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 971099645012382092,
            name: 2,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![Action {
        kind: 4,
        target: 1956793324523920083,
        parent: 0,
        name: 0,
        text: 0,
    }];
    let remote = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 2832855709947514548,
            name: 3,
            text: 0,
        },
        Action {
            kind: 4,
            target: 5522577497956667845,
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
        Action {
            kind: 3,
            target: 438823430391821631,
            parent: 5221277148646417130,
            name: 0,
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
                    kind: 3,
                    target: 5190931714523467330,
                    parent: 9928977605353883034,
                    name: 3,
                    text: 0,
                },
                Action {
                    kind: 2,
                    target: 5762732690084641497,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 2270514295357040306,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 0,
                    target: 0,
                    parent: 7086831897136368260,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 14243556372242850438,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_waiting_update_that_settling_replaced_does_not_hide_a_later_edit() {
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
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 5154893513058372197,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 3121141568318315682,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 15085739982097279210,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 12042687951629207403,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 805902510814196342,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 752737086628574172,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 2061236886198686334,
            name: 2,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 3,
            target: 11231637698525412811,
            parent: 4780766426531151849,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 11675596971973797087,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 7216849391709144393,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 13401659892108216031,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 6236013527037087150,
            name: 3,
            text: 1,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 7088491083489211429,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 17401249810093626761,
            parent: 231179896921208167,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 10774319552713167365,
            parent: 2638094422400810531,
            name: 2,
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
                    target: 158282335639317271,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 1121437384512356461,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 2,
                target: 3139751682532370971,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![
                Action {
                    kind: 4,
                    target: 6209302477719205039,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 2,
                    target: 4326100334342803349,
                    parent: 0,
                    name: 0,
                    text: 1,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_waiting_update_both_sides_moved_past_does_not_hold_back_an_edit() {
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
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 978237978856208785,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 4194549617926577008,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 8070810026119318711,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 17775129536871628555,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 3309602263058540387,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 1280557214040556292,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 4,
            target: 10356890765720543216,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1115092823749830263,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9079959440039050417,
            name: 2,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 4,
            target: 15907938580905265140,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 4229855130926676766,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 6810740145664341458,
            parent: 2110593086026663230,
            name: 5,
            text: 0,
        },
    ];
    let fits = 2;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![Action {
                kind: 1,
                target: 0,
                parent: 12430656056815562337,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 291780538349244884,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 1207142123533410396,
                parent: 14415471485496758628,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 2,
                target: 4350527627605106883,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_waiting_update_of_a_file_both_sides_deleted_is_dropped() {
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
            parent: 5575517051411596159,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9542798344174571143,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1134570969992643019,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 5217341420012805030,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 4,
            target: 14966158802491056936,
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
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 3639306566313460506,
            parent: 17827897833710994996,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 8247849472439318298,
            name: 2,
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
                    target: 2494586888620959721,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 0,
                    parent: 14654176779908604874,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 7730499601774269038,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 339548061254993100,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_waiting_folder_the_user_deleted_is_brought_back_for_a_file_moved_into_it() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 11079414779518565354,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 5832766238116930057,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 351730081019129726,
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
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 554097051787814230,
            parent: 0,
            name: 0,
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
                    kind: 2,
                    target: 3032609202697743113,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 2731698343566406169,
                    parent: 304348770870888498,
                    name: 2,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 3172150523246826267,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 9725551978640732566,
                    parent: 1209946197345948991,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 936650056549699008,
                    parent: 1061052580595229545,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 5,
                    text: 0,
                },
                Action {
                    kind: 0,
                    target: 0,
                    parent: 65492476661922611,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![Action {
                kind: 4,
                target: 600126837695760128,
                parent: 0,
                name: 0,
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
                    kind: 0,
                    target: 0,
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
fn a_removed_folder_the_server_revives_is_created_again_while_its_removal_waits() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 980740372666386433, 2, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 4216774371197345119, 0, 0),
        act(1, 0, 2882081015209338574, 3, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 15439864200780996302, 0, 0),
        act(0, 0, 9289856371756041619, 0, 0),
    ];
    let local = [
        act(0, 0, 1052871606481788707, 0, 0),
        act(3, 8922599260003921418, 12502836504135289280, 0, 0),
    ];
    let remote = [
        act(3, 1802799855454195272, 10126529284027281386, 0, 0),
        act(4, 8617856007053871336, 0, 0, 0),
    ];
    let during = [(true, vec![act(0, 0, 14856899963116643210, 0, 0)])];
    run(&seed, &local, &remote, 1000, true, &during).unwrap();
}

#[test]
fn a_folder_the_user_deleted_comes_back_for_a_waiting_file_the_server_moves_into_it() {
    let seed = [act(0, 0, 0, 2, 0), act(0, 0, 6274166174826663049, 0, 0)];
    let local = [act(0, 0, 4991715864022573885, 0, 0)];
    let remote = [
        act(1, 0, 8366538510828719079, 0, 0),
        act(2, 0, 0, 0, 0),
        act(0, 0, 544864474965852954, 0, 0),
    ];
    let during = vec![
        (
            true,
            vec![act(6, 0, 0, 0, 0), act(0, 0, 7211273767686463164, 0, 0)],
        ),
        (
            false,
            vec![
                act(4, 16841059892813868500, 0, 0, 0),
                act(1, 0, 343720692834072452, 2, 0),
            ],
        ),
        (true, vec![act(4, 16794273080645545114, 0, 0, 0)]),
        (
            false,
            vec![
                act(0, 0, 10316403760466809166, 0, 0),
                act(3, 16377241187031633676, 6363070014188718880, 0, 0),
            ],
        ),
    ];
    run(&seed, &local, &remote, 1, true, &during).unwrap();
}

#[test]
fn a_file_the_user_deleted_while_its_server_move_waited_in_s_is_deleted_on_the_server() {
    let seed = vec![
        Action {
            kind: 0,
            target: 4614810417318265143,
            parent: 4864179259767229626,
            name: 1,
            text: 2,
        },
        Action {
            kind: 0,
            target: 12119355532117708331,
            parent: 14757475804517505032,
            name: 5,
            text: 2,
        },
        Action {
            kind: 1,
            target: 6625108946950071779,
            parent: 9918011999374063130,
            name: 3,
            text: 2,
        },
        Action {
            kind: 0,
            target: 12576383093733275581,
            parent: 4466178185947277735,
            name: 3,
            text: 2,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 5819694219100739927,
            parent: 2036847718054668352,
            name: 6,
            text: 3,
        },
        Action {
            kind: 5,
            target: 2270324102383091549,
            parent: 7093835583652112558,
            name: 1,
            text: 2,
        },
        Action {
            kind: 1,
            target: 13656694315896170903,
            parent: 15331058797787251121,
            name: 1,
            text: 3,
        },
        Action {
            kind: 2,
            target: 1710332307205056776,
            parent: 7320500736078956321,
            name: 1,
            text: 2,
        },
        Action {
            kind: 0,
            target: 14507846936884681424,
            parent: 12527369109280507380,
            name: 4,
            text: 2,
        },
        Action {
            kind: 1,
            target: 8650148038972097623,
            parent: 6435848661095480790,
            name: 4,
            text: 2,
        },
    ];
    let remote = vec![
        Action {
            kind: 6,
            target: 807225587938069940,
            parent: 17863857293020369369,
            name: 5,
            text: 3,
        },
        Action {
            kind: 4,
            target: 13730757541944084961,
            parent: 11742315858831314384,
            name: 1,
            text: 1,
        },
        Action {
            kind: 0,
            target: 14107778014734974581,
            parent: 14200207690007878352,
            name: 4,
            text: 1,
        },
        Action {
            kind: 3,
            target: 5566379684988951548,
            parent: 4569525365923634314,
            name: 3,
            text: 3,
        },
    ];
    let fits = MAX_OPS;
    let fresh_dir_names = true;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 17702201595369440910,
                    parent: 11821905956357519784,
                    name: 0,
                    text: 2,
                },
                Action {
                    kind: 4,
                    target: 2762722298416582515,
                    parent: 8244077946071889904,
                    name: 3,
                    text: 3,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 13793337169377006733,
                    parent: 3592739378196482913,
                    name: 5,
                    text: 3,
                },
                Action {
                    kind: 4,
                    target: 7353629600874959317,
                    parent: 3775275498239273878,
                    name: 6,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 1401826479567066740,
                    parent: 10805880149766239377,
                    name: 6,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 5,
                    target: 3508424595374466801,
                    parent: 13652394482325166857,
                    name: 4,
                    text: 1,
                },
                Action {
                    kind: 0,
                    target: 12196094243527756545,
                    parent: 6472941235758237528,
                    name: 1,
                    text: 1,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 5237472451013669662,
                    parent: 3865121539495492564,
                    name: 3,
                    text: 2,
                },
                Action {
                    kind: 0,
                    target: 4275436719165289791,
                    parent: 10695791634680114475,
                    name: 1,
                    text: 2,
                },
                Action {
                    kind: 4,
                    target: 113406059743004084,
                    parent: 11482615632212085002,
                    name: 1,
                    text: 0,
                },
            ],
        ),
    ];
    let cut = Cut::Answered;
    run_cut(&seed, &local, &remote, fits, fresh_dir_names, &during, cut).unwrap();
}

#[test]
fn a_version_restore_of_a_file_deleted_here_waits_while_its_s_update_waits() {
    let seed = [act(0, 0, 0, 1, 0), act(0, 0, 1, 2, 0), act(0, 0, 2, 3, 3)];
    let local = [act(4, 2, 0, 2, 3), act(1, 0, 1, 2, 2)];
    let remote = [act(3, 2, 0, 0, 3)];
    let during = [
        (true, vec![act(0, 0, 0, 4, 1)]),
        (true, vec![act(6, 0, 0, 0, 1)]),
        (true, vec![act(4, 0, 0, 1, 3)]),
    ];
    for cut in CUTS {
        run_cut(&seed, &local, &remote, 1, false, &during, cut).unwrap();
    }
}
