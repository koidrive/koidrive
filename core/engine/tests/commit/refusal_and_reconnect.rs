// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_rejected_op_marks_a_problem_but_a_rejected_cycle_moves_the_node_back() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir(ROOT, "q"))),
        ),
        (
            "u",
            Some(file(ROOT, "u", 2)),
            Some(file(ROOT, "u", 1)),
            Some(remote(file(ROOT, "u", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("p", "q", "p"), update("u", 2)]);
    let rejected = |reason: &str| OpResult {
        reject_reason: reason.into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![rejected("CYCLE"), rejected("NOT_FOUND")],
    );
    assert_eq!(trees.synced().live(&"p".into()), Some(&dir(ROOT, "p")));
    assert!(trees.reverted().contains(&"p".into()));
    assert_eq!(commits.problems().iter().collect::<Vec<_>>(), ["u"]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(commits.clear_problem(&"u".into()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("u", 2)]);
    moves_back(&mut trees, &mut ids, "p");
}

#[test]
fn a_result_is_acknowledged_only_once_r_holds_its_records() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!commits.acknowledge(&mut trees, 9), "no result yet");
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(!commits.acknowledge(&mut trees, 4));
    assert_eq!(trees.synced().live(&"a".into()), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(
        commits.connected(&welcome(1)),
        Reconnect::Idle,
        "accepted, nothing to resend"
    );
    assert!(!commits.abandon(), "an accepted commit is never forgotten");
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!(trees.synced().live(&"a".into()), Some(&file(ROOT, "x", 1)));
    assert_eq!((commits.accepted(), commits.in_flight()), (1, None));
}

#[test]
fn a_policy_refusal_keeps_the_commit_and_outlasts_any_welcome() {
    use error::Code;
    let unknown = proto::Error {
        code: 99,
        ..Default::default()
    };
    let policy = [
        unknown,
        refusal(Code::Unspecified, 0),
        refusal(Code::QuotaExceeded, 0),
        refusal(Code::Forbidden, 0),
        refusal(Code::InvalidArgument, 0),
        refusal(Code::NotFound, 0),
        refusal(Code::ConfirmationRequired, 0),
    ];
    for error in policy {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
        let halt = Halt::Policy { code: error.code };
        assert_eq!(
            commits.failed(&mut trees, &error),
            Ok(Failure::Halted(halt)),
            "{error:?}"
        );
        let welcomes = [
            welcome(0),
            welcome(1),
            Welcome {
                read_only: true,
                ..welcome(0)
            },
            Welcome {
                history_id: "another".into(),
                ..welcome(0)
            },
        ];
        for welcome in welcomes {
            assert_eq!(commits.connected(&welcome), Reconnect::Idle, "{error:?}");
            assert_eq!(commits.halted(), Some(halt), "{error:?}");
        }
        assert!(!commits.abandon(), "{error:?}");
        assert_eq!(commits.in_flight().unwrap().commit, sent, "{error:?}");
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{error:?}");
    }
    for error in [
        refusal(Code::QuotaExceeded, 5_000),
        refusal(Code::RetryLater, 0),
    ] {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(
            commits.failed(&mut trees, &error),
            Ok(Failure::Resend),
            "{error:?}"
        );
        assert_eq!(
            commits.connected(&welcome(0)),
            Reconnect::Resend,
            "{error:?}"
        );
    }
}

/// A device under a policy halt keeps the refused commit whatever the `Welcome` says, until a
/// server counts fewer of its commits. That loss is handled as without the halt, once: the
/// counter takes the server's, the refused commit and the slots of the lost commits are dropped,
/// and R is listed again. The halt stays, and the next `Welcome` finds nothing lost.
#[test]
fn a_policy_halt_takes_a_server_loss_once() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let a = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        meta_rev: 4,
        meta_edit_rev: 4,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("a".into(), a);
    commits
        .received(result(4, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle(commits.aliases());
    trees.set_local("b".into(), local(file(ROOT, "y", 2), 98));
    trees.settle(commits.aliases());
    let refused = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::Forbidden, 0))
        .unwrap();
    let halt = Some(Halt::Policy {
        code: error::Code::Forbidden as i32,
    });
    assert_eq!(commits.held_through(), 4);
    assert_eq!(commits.connected(&welcome_holding(&commits, 1)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 4);
    assert_eq!(commits.in_flight().unwrap().commit, refused);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 0);
    assert_eq!((commits.accepted(), commits.in_flight()), (0, None));
    assert_eq!(commits.halted(), halt);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_policy_halt_lifted_to_resend_sends_the_same_commit_again() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::Forbidden, 0))
        .unwrap();
    assert_eq!(commits.lift(Lift::Resend), Ok(()));
    assert_eq!(commits.halted(), None);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Resend);
    assert_eq!(commits.in_flight().unwrap().commit, sent);
}

#[test]
fn a_policy_halt_lifted_to_replan_plans_the_changes_into_a_new_commit_in_the_same_slot() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let refused = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::InvalidArgument, 0))
        .unwrap();
    assert_eq!(commits.lift(Lift::Replan), Ok(()));
    assert_eq!((commits.halted(), commits.in_flight()), (None, None));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let planned = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_ne!(planned.commit_id, refused.commit_id);
    assert_eq!(planned.device_seq, refused.device_seq);
    assert_eq!(ops(&planned), ops(&refused));
}

#[test]
fn a_lift_needs_a_policy_halt_and_leaves_one_that_asks_for_confirmation() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(commits.lift(Lift::Resend), Err(LiftError::NotHalted));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::ConfirmationRequired, 0))
        .unwrap();
    let halt = commits.halted();
    for lift in [Lift::Resend, Lift::Replan] {
        assert_eq!(commits.lift(lift), Err(LiftError::NeedsConfirmation));
        assert_eq!(commits.halted(), halt);
        assert_eq!(commits.in_flight().unwrap().commit, sent);
    }
    let read_only = Welcome {
        read_only: true,
        ..welcome(0)
    };
    let mut commits = Commits::default();
    commits.connected(&read_only);
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
    assert_eq!(commits.lift(Lift::Replan), Err(LiftError::NotHalted));
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
}

/// GUN-379. The server counts fewer commits of this device than the store accepted, so it lost some it had
/// accepted. The store continues from the server's count and drops the commit in flight, also on a read-only
/// device.
#[test]
fn a_server_count_below_the_accepted_one_is_taken_and_drops_the_commit_in_flight() {
    for read_only in [false, true] {
        let mut trees = trees(&[("n", Some(file(ROOT, "n", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        acknowledge(
            &mut commits,
            &mut trees,
            vec![outcome(op_result::Code::Applied)],
        );
        assert_eq!(commits.accepted(), 1);
        trees.set_local("m".into(), local(file(ROOT, "m", 2), 2));
        trees.settle(commits.aliases());
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commit.device_seq, 2);
        let welcome = Welcome {
            read_only,
            ..welcome(0)
        };
        assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
        assert_eq!(commits.accepted(), 0);
        assert!(commits.in_flight().is_none());
    }
}

#[test]
fn a_server_count_above_the_accepted_one_at_a_slot_an_earlier_loss_took_is_a_loss() {
    let mut trees = trees(&[("n", Some(file(ROOT, "n", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    for (seq, name) in [(3, "m"), (7, "o")] {
        build(&mut commits, &mut trees, &mut ids).unwrap();
        let applied = vec![outcome(op_result::Code::Applied)];
        commits.received(result(seq, applied)).unwrap();
        assert!(commits.acknowledge(&mut trees, seq));
        trees.settle(commits.aliases());
        trees.set_local(name.into(), local(file(ROOT, name, 2), seq.into()));
        trees.settle(commits.aliases());
    }
    assert_eq!((commits.accepted(), commits.held_through()), (2, 7));
    let mut twice = commits.clone();
    assert_eq!(twice.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!((twice.accepted(), twice.held_through()), (0, 0));
    let mut reissued = twice.clone();
    assert_eq!(twice.connected(&welcome_holding(&twice, 1)), Reconnect::AheadOfServer);
    assert_eq!((twice.accepted(), twice.held_through()), (1, 3));
    assert!(twice.lost_here(7) && !twice.lost_here(3));
    assert_eq!(twice.halted(), None);
    let another = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 3,
        ..welcome(1)
    };
    assert_eq!(reissued.connected(&another), Reconnect::AheadOfServer);
    assert_eq!((reissued.accepted(), reissued.held_through()), (1, 0));
    assert!(reissued.lost_here(7) && reissued.lost_here(3));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.connected(&welcome(3)), Reconnect::Recovery);
    assert!(matches!(commits.halted(), Some(Halt::Recovery { .. })));
}

/// GUN-444: a commit's result arrives, but before it is acknowledged the server loses it, and
/// its `Welcome` counts *w* = *a*. That is a loss, so the commit in flight is dropped and R is
/// listed again, instead of recovery starting.
#[test]
fn a_held_result_of_a_commit_the_server_lost_is_a_loss() {
    let mut trees = trees(&[("n", Some(file(ROOT, "n", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let applied = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, applied)).unwrap();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.halted(), None);
    assert!(commits.in_flight().is_none());
    assert_eq!(commits.accepted(), 0);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
}

#[test]
fn the_device_slot_is_kept_by_a_retry_and_reused_after_a_refusal() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::MissingChunks, 0)),
        Ok(Failure::Resend)
    );
    assert_eq!(commits.in_flight().unwrap().commit, first);
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::AheadOfServer, 0)),
        Ok(Failure::Resync)
    );
    assert_eq!(commits.in_flight(), None);
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_ne!(second.commit_id, first.commit_id);
    assert_eq!((second.device_seq, second.ops.clone()), (1, first.ops));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("b".into(), local(file(ROOT, "y", 2), 9));
    let third = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(third.device_seq, 2);
}

#[test]
fn errors_forget_the_commit_only_when_nothing_was_applied() {
    use error::Code;
    let cases = [
        (Code::MissingChunks, Failure::Resend, true),
        (Code::RetryLater, Failure::Resend, true),
        (Code::Unauthenticated, Failure::Resend, true),
        (Code::Internal, Failure::Resend, true),
        (Code::HeadMoved, Failure::Abandoned, false),
        (Code::UnsupportedVersion, Failure::Abandoned, false),
        (Code::AheadOfServer, Failure::Resync, false),
        (Code::ReadOnly, Failure::Halted(Halt::ReadOnly), false),
        (
            Code::HistoryMismatch,
            Failure::Halted(Halt::Recovery {
                cause: RecoveryCause::of(RecoveryTrigger::SpaceRewound),
                policy: None,
            }),
            true,
        ),
        (
            Code::StaleDeviceState,
            Failure::Halted(Halt::Recovery {
                cause: RecoveryCause::of(RecoveryTrigger::StoreRolledBack),
                policy: None,
            }),
            true,
        ),
    ];
    for (code, failure, kept) in cases {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(
            commits.failed(&mut trees, &refusal(code, 0)),
            Ok(failure),
            "{code:?}"
        );
        assert_eq!(commits.in_flight().is_some(), kept, "{code:?}");
        let halted = matches!(failure, Failure::Halted(_));
        assert_eq!(
            build(&mut commits, &mut trees, &mut ids).is_some(),
            !kept && !halted,
            "{code:?}"
        );
    }
}

#[test]
fn reconnecting_compares_the_device_counter_and_the_history() {
    let fresh = || {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        commits
    };
    assert_eq!(Commits::default().connected(&welcome(0)), Reconnect::Idle);
    assert_eq!(
        fresh().connected(&welcome(0)),
        Reconnect::Resend,
        "not accepted"
    );
    assert_eq!(
        fresh().connected(&welcome(1)),
        Reconnect::Resend,
        "accepted, response lost"
    );
    for w in [2, 5] {
        let mut commits = fresh();
        assert_eq!(commits.connected(&welcome(w)), Reconnect::Recovery);
        assert!(matches!(commits.halted(), Some(Halt::Recovery { .. })));
        assert!(commits.in_flight().is_some(), "kept as evidence");
        assert_eq!(commits.connected(&welcome(1)), Reconnect::Recovery);
    }
    let mut idle = Commits::default();
    assert_eq!(
        idle.connected(&welcome(1)),
        Reconnect::Recovery,
        "a store older than its commits"
    );
    let other = Welcome {
        history_id: "rewound".into(),
        ..welcome(0)
    };
    assert_eq!(fresh().connected(&other), Reconnect::Recovery);

    let mut commits = fresh();
    let read_only = Welcome {
        read_only: true,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&read_only), Reconnect::Idle);
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Resend);
    assert_eq!(commits.halted(), None);
}

#[test]
fn a_file_the_user_deletes_before_the_server_renames_its_create_is_deleted() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.remove_local(&"a".into());
    trees.settle(commits.aliases());
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    assert!(
        (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)).is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("a")]);
}

#[test]
fn a_folder_the_user_empties_and_deletes_before_the_server_renames_its_create_is_deleted() {
    let mut trees = trees(&[
        ("n", Some(dir(ROOT, "x")), None, None),
        ("c", Some(dir("n", "y")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_local("c".into(), local(dir(ROOT, "y"), 2));
    trees.remove_local(&"n".into());
    trees.settle(commits.aliases());
    trees.set_remote("n".into(), remote(dir(ROOT, "x (conflict)")));
    trees.set_remote("c".into(), remote(dir("n", "y")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            OpResult {
                name: "x (conflict)".into(),
                ..outcome(op_result::Code::AppliedRenamed)
            },
            outcome(op_result::Code::Applied),
        ],
    );
    trees.settle(commits.aliases());
    let mut effects = Effects::default();
    assert!(
        (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)).is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("c", ROOT, "y"), delete("n")]);
}

#[test]
fn a_file_left_out_of_a_commit_holds_back_no_other_change() {
    let mut trees = trees(&[
        (
            "a",
            Some(file(ROOT, "a", 2)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "b",
            Some(file(ROOT, "b", 2)),
            Some(file(ROOT, "b", 1)),
            Some(remote(file(ROOT, "b", 1))),
        ),
        (
            "c",
            Some(file(ROOT, "c", 2)),
            Some(file(ROOT, "c", 1)),
            Some(remote(file(ROOT, "c", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [update("a", 2), update("b", 2), update("c", 2)]
    );
    assert!(commits.leave_out(&"b".into()));
    assert_eq!(commits.in_flight(), None);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2), update("c", 2)]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert!(!commits.leave_out(&"a".into()), "no commit in flight");
    assert_eq!(commits.problems().iter().collect::<Vec<_>>(), ["b"]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(commits.clear_problem(&"b".into()));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("b", 2)]);
}

fn given_up_at(commits: Commits, slot: u64, commit_id: &str) -> Commits {
    let mut state = commits.state();
    state
        .given_up
        .entry(slot)
        .or_default()
        .push(commit_id.into());
    Commits::from_state(state).unwrap()
}

fn kept_slots() -> Commits {
    Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, slot(3)), (2, slot(7))].into(),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn the_servers_token_at_its_count_decides_which_kept_slot_bounds_what_it_holds() {
    let mut commits = kept_slots();
    assert_eq!(commits.connected(&welcome_holding(&commits, 2)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 7);
    assert_eq!(commits.connected(&welcome(2)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 7);
    let reissued = Welcome {
        device_commit_id: "commit-at-7".into(),
        device_committed_at_ms: 8,
        ..welcome(2)
    };
    assert_eq!(commits.connected(&reissued), Reconnect::Idle);
    assert_eq!(commits.held_through(), 3);
    assert_eq!(commits.connected(&welcome_holding(&commits, 2)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 7);

    let mut lost = kept_slots();
    assert_eq!(lost.connected(&welcome_holding(&lost, 1)), Reconnect::AheadOfServer);
    assert_eq!((lost.accepted(), lost.held_through()), (1, 3));
    assert!(lost.lost_here(7) && !lost.lost_here(3));

    let mut elsewhere = kept_slots();
    let another = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 3,
        ..welcome(1)
    };
    assert_eq!(elsewhere.connected(&another), Reconnect::Recovery);
    assert!(matches!(elsewhere.halted(), Some(Halt::Recovery { .. })));
    assert_eq!(elsewhere.accepted(), 2);
    assert!(elsewhere.lost_here(7) && elsewhere.lost_here(3));
}

#[test]
fn a_store_copied_before_the_listing_bounds_by_the_kept_slot_below_one_the_server_gave_out_again() {
    let mut copy = kept_slots();
    assert_eq!(copy.connected(&welcome_holding(&copy, 1)), Reconnect::AheadOfServer);
    assert_eq!((copy.accepted(), copy.held_through()), (1, 3));
    let given_again = Welcome {
        device_commit_id: "sent-again".into(),
        device_committed_at_ms: 9,
        ..welcome(2)
    };
    assert_eq!(copy.connected(&given_again), Reconnect::AheadOfServer);
    assert_eq!((copy.accepted(), copy.held_through()), (2, 3));
    assert!(copy.lost_here(7) && !copy.lost_here(3));
    assert_eq!(copy.halted(), None);
}

#[test]
fn a_slot_kept_without_a_token_keeps_the_bound_of_the_newest_kept_slot() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 1,
        slots: [(
            1,
            Slot {
                seq: 3,
                ..Default::default()
            },
        )]
        .into(),
        given_up: [(2, vec!["another".into()])].into(),
        ..Default::default()
    })
    .unwrap();
    let mut ids = IdGen::new(1);
    let empty = Welcome {
        device_commit_id: String::new(),
        device_committed_at_ms: 0,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&empty), Reconnect::Idle);
    assert_eq!(commits.held_through(), 3);
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .received(CommitResult {
            committed_at_ms: 50,
            ..result(5, vec![outcome(op_result::Code::Applied)])
        })
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!((commits.accepted(), commits.held_through()), (2, 5));
    assert_eq!(commits.connected(&welcome(2)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 5);
    let another = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 50,
        ..welcome(2)
    };
    assert_eq!(commits.connected(&another), Reconnect::Idle);
    assert_eq!(commits.held_through(), 3);
    assert_eq!(commits.connected(&welcome_holding(&commits, 2)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 5);
}

#[test]
fn a_result_applied_after_a_welcome_with_another_token_bounds_by_its_own_slot() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let mut commits = given_up_at(kept_slots(), 2, "another");
    let mut ids = IdGen::new(1);
    let another = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 7,
        ..welcome(2)
    };
    assert_eq!(commits.connected(&another), Reconnect::Idle);
    assert_eq!(commits.held_through(), 3);
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    assert_eq!((commits.accepted(), commits.held_through()), (3, 9));
}

#[test]
fn a_welcome_that_holds_another_commit_at_the_slot_of_a_held_result_is_a_loss() {
    let held = || {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        commits
            .received(CommitResult {
                committed_at_ms: 90,
                ..result(9, vec![outcome(op_result::Code::Applied)])
            })
            .unwrap();
        (trees, commits, commit.commit_id)
    };
    let (_, mut commits, _) = held();
    let another = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 90,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&another), Reconnect::AheadOfServer);
    assert!(commits.in_flight().is_none());
    assert_eq!((commits.accepted(), commits.held_through()), (1, 0));

    for token in [None, Some(90)] {
        let (mut trees, mut commits, commit_id) = held();
        let holding = Welcome {
            device_commit_id: token.map_or(String::new(), |_| commit_id),
            device_committed_at_ms: token.unwrap_or(0),
            ..welcome(1)
        };
        assert_eq!(commits.connected(&holding), Reconnect::Idle);
        assert!(commits.acknowledge(&mut trees, 9));
        assert_eq!((commits.accepted(), commits.held_through()), (1, 9));
    }
}

#[test]
fn a_welcome_that_holds_the_record_of_the_commit_in_flight_bounds_by_its_seq() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commits.own_record(9, &commit.commit_id, 90));
    let holding = Welcome {
        device_commit_id: commit.commit_id.clone(),
        device_committed_at_ms: 90,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&holding), Reconnect::Resend);
    assert_eq!((commits.accepted(), commits.held_through()), (0, 9));
}

fn held_at(committed_at_ms: i64) -> (Trees, Commits, String) {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .received(CommitResult {
            committed_at_ms,
            ..result(9, vec![outcome(op_result::Code::Applied)])
        })
        .unwrap();
    (trees, commits, commit.commit_id)
}

fn naming(commit_id: &str, committed_at_ms: i64) -> Welcome {
    Welcome {
        device_commit_id: commit_id.to_owned(),
        device_committed_at_ms: committed_at_ms,
        ..welcome(1)
    }
}

#[test]
fn a_result_without_a_time_bounds_at_its_slot_for_a_welcome_that_names_its_commit_with_one() {
    let (mut trees, mut commits, commit_id) = held_at(0);
    assert!(commits.acknowledge(&mut trees, 9));
    assert_eq!(commits.connected(&naming(&commit_id, 90)), Reconnect::Idle);
    assert_eq!((commits.accepted(), commits.held_through()), (1, 9));
}

#[test]
fn a_held_result_without_a_time_is_no_loss_for_a_welcome_that_names_its_commit_with_one() {
    let (mut trees, mut commits, commit_id) = held_at(0);
    assert_eq!(commits.connected(&naming(&commit_id, 90)), Reconnect::Idle);
    assert!(commits.in_flight().is_some());
    assert!(commits.acknowledge(&mut trees, 9));
    assert_eq!((commits.accepted(), commits.held_through()), (1, 9));
}

#[test]
fn a_welcome_that_names_this_devices_commit_at_another_time_holds_another_application_of_it() {
    let (_, mut held, commit_id) = held_at(90);
    assert_eq!(held.connected(&naming(&commit_id, 91)), Reconnect::AheadOfServer);
    assert!(held.in_flight().is_none());
    assert_eq!((held.accepted(), held.held_through()), (1, 0));

    let (mut trees, mut acknowledged, commit_id) = held_at(90);
    assert!(acknowledged.acknowledge(&mut trees, 9));
    assert_eq!(acknowledged.connected(&naming(&commit_id, 91)), Reconnect::Idle);
    assert_eq!((acknowledged.accepted(), acknowledged.held_through()), (1, 0));
    assert_eq!(acknowledged.connected(&naming(&commit_id, 90)), Reconnect::Idle);
    assert_eq!(acknowledged.held_through(), 9);
}

#[test]
fn a_result_without_a_time_keeps_the_time_the_commits_record_gave_its_slot() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commits.own_record(9, &commit.commit_id, 90));
    commits.received(result(9, vec![outcome(op_result::Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    assert_eq!(commits.connected(&naming(&commit.commit_id, 91)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 0);
    assert_eq!(commits.connected(&naming(&commit.commit_id, 90)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 9);
}

#[test]
#[ignore = "GUN-1012: a Welcome names only the slot at its count, so a token that differs there cannot tell a shared slot below it from one the server gave out again (RFC 0029)"]
fn a_server_that_gave_every_kept_slot_out_again_holds_none_of_them() {
    let mut commits = Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, slot(4)), (2, slot(5))].into(),
        ..Default::default()
    })
    .unwrap();
    let reissued = Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 2,
        ..welcome(2)
    };
    assert_eq!(commits.connected(&reissued), Reconnect::Idle);
    assert_eq!(
        commits.held_through(),
        0,
        "held_through counts slot 1, which the server gave out again too"
    );
}

#[test]
#[ignore = "GUN-1012: one slot token cannot tell a commit that replaced this store's lost commit from one a rolled-back copy never saw, so the copy bounds below the slot the server holds its edit at (RFC 0029)"]
fn a_copy_rolled_back_past_its_resend_does_not_send_its_edit_again() {
    let mut trees = trees(&[synced("n", file(ROOT, "n", 1))]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.settle(commits.aliases());
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let edited = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        ..remote(file(ROOT, "n", 2))
    };
    trees.set_remote_edited_here("n".into(), edited);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle(commits.aliases());
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);

    let resent = Welcome {
        device_commit_id: "resent".into(),
        ..welcome(1)
    };
    assert_eq!(commits.connected(&resent), Reconnect::AheadOfServer);
    let (trashed_here, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records(|seq| commits.lost_here(seq));
    let deleted = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 3,
        meta_edit_rev: 3,
        trash_group: "3:0".into(),
        ..trashed(file(ROOT, "n", 2))
    };
    trees.set_listed_remote("n".into(), deleted);
    trees.forget_lost(
        trashed_here,
        edited,
        synced,
        moved,
        untaken,
        records,
        &Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    commits.forget_lost_slots();
    trees.settle(commits.aliases());
    let sent = build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert_eq!(
        sent_for(&sent, "n"),
        Vec::<&op::Op>::new(),
        "the copy sends the edit of n again over the delete"
    );
}

#[test]
fn a_server_count_below_the_accepted_one_starts_recovery_when_its_token_names_another_commit() {
    let kept = |commit_id: &str| Slot {
        seq: 3,
        commit_id: commit_id.into(),
        committed_at_ms: 30,
    };
    let at_slot_1 = |commit_id: &str, committed_at_ms: i64| Welcome {
        device_commit_id: commit_id.into(),
        device_committed_at_ms: committed_at_ms,
        ..welcome(1)
    };
    let cases = [
        (kept("one"), at_slot_1("one", 30), Reconnect::AheadOfServer),
        (kept("one"), at_slot_1("", 0), Reconnect::AheadOfServer),
        (kept(""), at_slot_1("another", 30), Reconnect::AheadOfServer),
        (kept("one"), at_slot_1("another", 30), Reconnect::Recovery),
        (kept("one"), at_slot_1("one", 31), Reconnect::AheadOfServer),
    ];
    for (slot_1, welcome, reconnect) in cases {
        let mut commits = Commits::from_state(CommitsState {
            accepted: 2,
            slots: [(1, slot_1), (2, slot(7))].into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(commits.connected(&welcome), reconnect, "{welcome:?}");
        let restarted = Commits::from_state(commits.state()).unwrap();
        for commits in [&commits, &restarted] {
            let recovering = matches!(commits.halted(), Some(Halt::Recovery { .. }));
            assert_eq!(recovering, reconnect == Reconnect::Recovery, "{welcome:?}");
        }
    }
}

#[test]
fn a_restore_that_brings_back_a_commit_this_device_gave_its_slot_to_before_a_loss_starts_no_recovery()
 {
    let mut commits = Commits::from_state(CommitsState {
        accepted: 1,
        slots: [(1, given(0))].into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    commits.forget_lost_slots();
    let mut commits = Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, given(1)), (2, slot(7))].into(),
        ..commits.state()
    })
    .unwrap();
    assert_eq!(commits.connected(&at_slot_1(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.accepted(), 1);
}

#[test]
fn a_restore_that_brings_back_a_commit_a_slot_gave_up_more_than_the_kept_number_of_losses_ago_starts_recovery()
 {
    let mut state = CommitsState::default();
    for loss in 0..=GIVEN_UP_KEPT {
        let mut commits = Commits::from_state(CommitsState {
            accepted: 1,
            slots: [(1, given(loss))].into(),
            ..state
        })
        .unwrap();
        assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
        commits.forget_lost_slots();
        state = commits.state();
    }
    for loss in 0..=GIVEN_UP_KEPT {
        let mut commits = Commits::from_state(CommitsState {
            accepted: 2,
            slots: [(1, given(GIVEN_UP_KEPT + 1)), (2, slot(7))].into(),
            ..state.clone()
        })
        .unwrap();
        let reconnect = match loss {
            0 => Reconnect::Recovery,
            _ => Reconnect::AheadOfServer,
        };
        assert_eq!(
            commits.connected(&at_slot_1(loss)),
            reconnect,
            "loss {loss}"
        );
    }
}

fn given(loss: usize) -> Slot {
    Slot {
        seq: 3,
        commit_id: format!("commit-{loss}"),
        committed_at_ms: 30 + loss as i64,
    }
}

fn at_slot_1(loss: usize) -> Welcome {
    Welcome {
        device_commit_id: format!("commit-{loss}"),
        device_committed_at_ms: 30 + loss as i64,
        ..welcome(1)
    }
}

#[test]
fn a_recovery_forgets_the_commits_its_slots_were_given_before_a_loss() {
    let mut commits = Commits::from_state(CommitsState {
        accepted: 1,
        slots: [(1, given(0))].into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    commits.forget_lost_slots();
    assert!(!commits.state().given_up.is_empty());
    let cause = RecoveryCause::of(RecoveryTrigger::SpaceRewound);
    commits.enter_recovery(cause);
    let (mut trees, mut ids) = (trees(&[]), IdGen::new(1));
    commits.recover(
        &mut trees,
        0,
        false,
        &cause,
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    assert!(commits.state().given_up.is_empty());
    let mut commits = Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, given(1)), (2, slot(7))].into(),
        ..commits.state()
    })
    .unwrap();
    assert_eq!(commits.connected(&at_slot_1(0)), Reconnect::Recovery);
}

#[test]
fn a_restore_that_brings_back_a_commit_built_again_at_its_slot_starts_no_recovery() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(&mut trees, &refusal(error::Code::AheadOfServer, 0)),
        Ok(Failure::Resync)
    );
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!((first.device_seq, second.device_seq), (1, 1));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let state = commits.state();
    let mut commits = Commits::from_state(CommitsState {
        accepted: 2,
        slots: [(1, state.slots[&1].clone()), (2, slot(7))].into(),
        ..state
    })
    .unwrap();
    let welcome = Welcome {
        device_commit_id: first.commit_id,
        device_committed_at_ms: 30,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
}

#[test]
fn a_restore_that_brings_back_a_commit_a_reconnect_dropped_from_flight_starts_no_recovery() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let mut commits = Commits::from_state(CommitsState {
        accepted: 1,
        slots: [(1, given(0))].into(),
        ..Default::default()
    })
    .unwrap();
    let dropped = build(&mut commits, &mut trees, &mut IdGen::new(1)).unwrap();
    assert_eq!(dropped.device_seq, 2);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert!(commits.in_flight().is_none());
    let state = commits.state();
    let mut commits = Commits::from_state(CommitsState {
        accepted: 3,
        slots: [(1, given(0)), (2, given(1)), (3, slot(7))].into(),
        ..state
    })
    .unwrap();
    let welcome = Welcome {
        device_commit_id: dropped.commit_id,
        device_committed_at_ms: 30,
        ..welcome(2)
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
}

fn another_at(slot: u64) -> Welcome {
    Welcome {
        device_commit_id: "another".into(),
        device_committed_at_ms: 30,
        ..welcome(slot)
    }
}

#[test]
fn a_server_count_at_the_accepted_one_naming_a_commit_the_store_does_not_know_starts_recovery() {
    let mut commits = kept_slots();
    assert_eq!(commits.connected(&another_at(2)), Reconnect::Recovery);
    assert!(matches!(commits.halted(), Some(Halt::Recovery { .. })));
    let mut given_up = given_up_at(kept_slots(), 2, "another");
    assert_eq!(given_up.connected(&another_at(2)), Reconnect::Idle);
}

#[test]
fn a_commit_waiting_for_its_result_is_not_resent_when_the_server_names_another_commit_at_the_accepted_slot()
 {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let mut commits = kept_slots();
    build(&mut commits, &mut trees, &mut IdGen::new(1)).unwrap();
    assert_eq!(commits.connected(&another_at(2)), Reconnect::Recovery);
    assert!(matches!(commits.halted(), Some(Halt::Recovery { .. })));
}

#[test]
fn a_commit_holding_its_result_is_not_dropped_when_the_server_names_another_commit_at_the_accepted_slot()
 {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let mut commits = kept_slots();
    build(&mut commits, &mut trees, &mut IdGen::new(1)).unwrap();
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert_eq!(commits.connected(&another_at(2)), Reconnect::Recovery);
    assert!(matches!(commits.halted(), Some(Halt::Recovery { .. })));
}

#[test]
fn a_recovery_keeps_the_policy_halt_it_interrupted() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::ConfirmationRequired, 0))
        .unwrap();
    let halt = commits.halted();
    let cause = RecoveryCause::rewind(0, 0);
    commits.enter_recovery(cause);
    let rewound = Welcome {
        history_id: "rewound".into(),
        ..welcome(0)
    };
    assert_eq!(commits.connected(&rewound), Reconnect::Recovery);
    let (history, marks) = (History::new(), Marks::new());
    commits.recover(
        &mut trees, 0, false, &cause, &history, &marks, &mut ids, NOW,
    );
    assert_eq!(commits.halted(), halt);
    assert_eq!(
        commits.lift(Lift::Resend),
        Err(LiftError::NeedsConfirmation)
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_recovery_on_a_read_only_device_ends_read_only() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let rewound = Welcome {
        history_id: "rewound".into(),
        read_only: true,
        ..welcome(0)
    };
    assert_eq!(commits.connected(&rewound), Reconnect::Recovery);
    assert_eq!(commits.connected(&rewound), Reconnect::Recovery);
    let Some(Halt::Recovery { cause, .. }) = commits.halted() else {
        panic!("the history differs, so the device recovers");
    };
    let (history, marks) = (History::new(), Marks::new());
    commits.recover(&mut trees, 0, true, &cause, &history, &marks, &mut ids, NOW);
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}
