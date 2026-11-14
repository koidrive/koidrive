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
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert!(trees.reverted().contains("p"));
    assert_eq!(commits.problems().iter().collect::<Vec<_>>(), ["u"]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(commits.clear_problem("u"));
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
    assert_eq!(trees.synced().live("a"), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(
        commits.connected(&welcome(1)),
        Reconnect::Idle,
        "accepted, nothing to resend"
    );
    assert!(!commits.abandon(), "an accepted commit is never forgotten");
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "x", 1)));
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
    trees.settle();
    trees.set_local("b".into(), local(file(ROOT, "y", 2), 98));
    trees.settle();
    let refused = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&mut trees, &refusal(error::Code::Forbidden, 0))
        .unwrap();
    let halt = Some(Halt::Policy {
        code: error::Code::Forbidden as i32,
    });
    assert_eq!(commits.held_through(), 4);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Idle);
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
        trees.settle();
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
            Failure::Halted(Halt::Recovery(RecoveryCause::of(
                RecoveryTrigger::SpaceRewound,
            ))),
            true,
        ),
        (
            Code::StaleDeviceState,
            Failure::Halted(Halt::Recovery(RecoveryCause::of(
                RecoveryTrigger::StoreRolledBack,
            ))),
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
        assert!(matches!(commits.halted(), Some(Halt::Recovery(_))));
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
    trees.remove_local("a");
    trees.settle();
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.settle();
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
    trees.remove_local("n");
    trees.settle();
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
    trees.settle();
    let mut effects = Effects::default();
    assert!(
        (effects.plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)).is_empty()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("c", ROOT, "y"), delete("n")]);
}
