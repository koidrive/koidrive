// SPDX-License-Identifier: Apache-2.0

#[path = "attribution/authors.rs"]
mod authors;

#[path = "attribution/model/mod.rs"]
mod model;

use model::Action::{
    self, Answer, Bootstrap, Build, Delete, Lose, Move, Record, Serve, ServeRenamed, TrashRestore,
    Write,
};
use model::{Budget, Rules, check, check_deep, settle, show};

#[test]
#[ignore]
fn show_from_env() {
    let trace: Vec<Action> = (std::env::var("MODEL_TRACE")
        .unwrap()
        .replace(' ', "")
        .replace("Unlose", "Unlose()")
        .split(')'))
    .filter(|step| !step.trim_matches(',').is_empty())
    .map(|step| {
        let (name, args) = step.trim_matches(',').split_once('(').unwrap();
        if name == "Unlose" {
            return Action::Unlose;
        }
        let args: Vec<usize> = args.split(',').map(|n| n.parse().unwrap()).collect();
        match name {
            "Write" => Write(args[0]),
            "Move" => Move(args[0]),
            "Delete" => Delete(args[0]),
            "TrashRestore" => TrashRestore(args[0]),
            "Build" => Build(args[0], args[1] as u8),
            "Serve" => Serve(args[0]),
            "ServeRenamed" => ServeRenamed(args[0]),
            "Answer" => Answer(args[0]),
            "Record" => Record(args[0]),
            "Lose" => Lose(args[0] as u8),
            "Bootstrap" => Bootstrap(args[0]),
            other => panic!("no action {other}"),
        }
    })
    .collect();
    show(
        Budget {
            created_by: std::env::var("MODEL_CREATED")
                .ok()
                .map(|d| d.parse().unwrap()),
            ..ANY
        },
        rules(&std::env::var("MODEL_RULES").unwrap_or("Spec".into())),
        &trace,
    );
}

const A: usize = 0;
const B: usize = 1;

const ANY: Budget = Budget {
    writes: [2, 2],
    moves: [2, 2],
    deletes: [1, 1],
    trash_restores: [1, 1],
    renames: 1,
    losses: 2,
    created_by: None,
};

fn rules(name: &str) -> Rules {
    match name {
        "Spec" => Rules::Spec,
        "MoveResultEndsContent" => Rules::MoveResultEndsContent,
        "MoveResultMarksContentResent" => Rules::MoveResultMarksContentResent,
        "LandedResendIsOwn" => Rules::LandedResendIsOwn,
        "ResendRecordEndsOwn" => Rules::ResendRecordEndsOwn,
        "NodeAuthor" => Rules::NodeAuthor,
        "ClaimAtSeq" => Rules::ClaimAtSeq,
        "LossForgetsDeletedFields" => Rules::LossForgetsDeletedFields,
        "DeleteSeesSynced" => Rules::DeleteSeesSynced,
        "ResendRevivesSeenValue" => Rules::ResendRevivesSeenValue,
        "DeleteDropsResends" => Rules::DeleteDropsResends,
        "DeleteNeedsSyncedLive" => Rules::DeleteNeedsSyncedLive,
        "SeqDelete" => Rules::SeqDelete,
        "SupersededDropsOwn" => Rules::SupersededDropsOwn,
        "TrashSettlesDelete" => Rules::TrashSettlesDelete,
        "DeleteSeesOnlyLocal" => Rules::DeleteSeesOnlyLocal,
        "LossKeepsStaleSeq" => Rules::LossKeepsStaleSeq,
        "OldServer" => Rules::OldServer,
        "OldDeleter" => Rules::OldDeleter,
        "DeletedAdoptsRecords" => Rules::DeletedAdoptsRecords,
        "BoundOnly" => Rules::BoundOnly,
        "RemovalTakesListedShape" => Rules::RemovalTakesListedShape,
        "HighestHeld" => Rules::HighestHeld,
        "DeleteSeesOnlyView" => Rules::DeleteSeesOnlyView,
        "BoundOnlyResendsSeen" => Rules::BoundOnlyResendsSeen,
        "TrashCoversView" => Rules::TrashCoversView,
        "EditSeesOnlyBase" => Rules::EditSeesOnlyBase,
        "ListedRemovalsInRecovery" => Rules::ListedRemovalsInRecovery,
        "LostCreateResends" => Rules::LostCreateResends,
        "NoLostPosition" => Rules::NoLostPosition,
        "AbsentDeleteResent" => Rules::AbsentDeleteResent,
        "ResentCreateFrom" => Rules::ResentCreateFrom,
        "RefusedKeepsLostPosition" => Rules::RefusedKeepsLostPosition,
        "RefusedTakesLostPosition" => Rules::RefusedTakesLostPosition,
        "CreateFixes" => Rules::CreateFixes,
        "AllFixes" => Rules::AllFixes,
        "HeldDeleteResent" => Rules::HeldDeleteResent,
        "HeldFixes" => Rules::HeldFixes,
        "HeldPositionFixes" => Rules::HeldPositionFixes,
        "Main" => Rules::Main,
        "RefusedAsMain" => Rules::RefusedAsMain,
        other => panic!("no rules {other}"),
    }
}

#[test]
#[ignore]
fn explore_from_env() {
    let numbers: Vec<u8> = std::env::var("MODEL_BUDGET")
        .unwrap()
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    let budget = Budget {
        writes: [numbers[0], numbers[1]],
        moves: [numbers[2], numbers[3]],
        deletes: [numbers[4], numbers[5]],
        trash_restores: [numbers[6], numbers[7]],
        renames: numbers[8],
        losses: numbers[9],
        created_by: numbers.get(10).map(|&d| d as usize),
    };
    for name in std::env::var("MODEL_RULES").unwrap().split(',') {
        let start = std::time::Instant::now();
        let report = if std::env::var("MODEL_DEEP").is_ok() {
            check_deep(budget, rules(name))
        } else {
            check(budget, rules(name))
        };
        println!(
            "{name}: {} states, {} converged ({} with a spurious copy), {:?} in {:?}{}",
            report.states,
            report.converged,
            report.spurious,
            report.broken,
            start.elapsed(),
            if report.capped {
                ", stopped at MODEL_MAX_STATES"
            } else {
                ""
            }
        );
        if std::env::var("MODEL_CHART").is_ok() {
            for (field, before, label, after) in &report.steps {
                println!("{field:?}: {before} --{label}--> {after}");
            }
        }
    }
}

const SMALL: [[u8; 10]; 7] = [
    [0, 0, 0, 1, 1, 0, 0, 0, 0, 1],
    [0, 1, 0, 0, 1, 0, 0, 0, 0, 1],
    [0, 0, 0, 1, 1, 0, 0, 1, 0, 2],
    [0, 1, 0, 0, 1, 0, 0, 0, 0, 2],
    [1, 1, 0, 0, 1, 1, 0, 0, 0, 1],
    [0, 0, 1, 1, 1, 1, 0, 0, 0, 1],
    [0, 1, 1, 0, 1, 0, 0, 1, 1, 1],
];

const LARGE: [[u8; 10]; 6] = [
    [1, 1, 1, 1, 1, 1, 0, 0, 0, 1],
    [1, 1, 1, 0, 0, 1, 0, 1, 0, 1],
    [1, 0, 1, 1, 0, 1, 0, 0, 0, 2],
    [0, 1, 1, 0, 1, 0, 0, 1, 1, 2],
    [1, 1, 0, 0, 1, 1, 0, 0, 0, 2],
    [0, 0, 1, 1, 1, 1, 0, 0, 0, 2],
];

const CREATED: [[u8; 10]; 6] = [
    [0, 0, 0, 1, 1, 0, 0, 0, 0, 1],
    [0, 0, 0, 1, 1, 1, 0, 0, 0, 1],
    [1, 1, 0, 0, 1, 1, 0, 0, 0, 1],
    [0, 0, 1, 1, 1, 1, 0, 0, 0, 1],
    [1, 1, 1, 0, 0, 1, 0, 1, 0, 1],
    [1, 0, 1, 1, 0, 1, 0, 0, 0, 2],
];

fn budget(n: [u8; 10]) -> Budget {
    Budget {
        writes: [n[0], n[1]],
        moves: [n[2], n[3]],
        deletes: [n[4], n[5]],
        trash_restores: [n[6], n[7]],
        renames: n[8],
        losses: n[9],
        created_by: None,
    }
}

fn created(n: [u8; 10]) -> Budget {
    Budget {
        created_by: Some(A),
        ..budget(n)
    }
}

type Counterexample = (Budget, (String, Vec<Action>));

fn refuted(rules: Rules, budgets: impl IntoIterator<Item = Budget>) -> Option<Counterexample> {
    budgets.into_iter().find_map(|n| {
        let start = std::time::Instant::now();
        let deep = check_deep(n, rules);
        println!(
            "{rules:?} {n:?}: {} states in {:?}, broken: {}",
            deep.states,
            start.elapsed(),
            deep.broken.is_some()
        );
        let broken = deep.broken?;
        let shortest = if deep.states < 2_000_000 {
            check(n, rules).broken
        } else {
            None
        };
        Some((n, shortest.unwrap_or(broken)))
    })
}

#[test]
#[ignore]
fn refute_from_env() {
    for name in std::env::var("MODEL_RULES").unwrap().split(',') {
        println!("{name}: {:?}", refuted(rules(name), SMALL.map(budget)));
    }
}

fn refute(rules: Rules) {
    let found = refuted(rules, SMALL.map(budget))
        .or_else(|| refuted(rules, CREATED.map(created)))
        .or_else(|| refuted(rules, LARGE.map(budget)));
    println!("{rules:?} breaks: {found:?}");
    assert!(found.is_some(), "no budget breaks {rules:?}");
}

#[test]
#[ignore]
fn refute_move_result_ends_content() {
    refute(Rules::MoveResultEndsContent);
}

#[test]
#[ignore]
fn refute_move_result_marks_content_resent() {
    refute(Rules::MoveResultMarksContentResent);
}

#[test]
#[ignore]
fn refute_landed_resend_is_own() {
    refute(Rules::LandedResendIsOwn);
}

#[test]
#[ignore]
fn refute_resend_record_ends_own() {
    refute(Rules::ResendRecordEndsOwn);
}

#[test]
#[ignore]
fn refute_node_author() {
    refute(Rules::NodeAuthor);
}

#[test]
#[ignore]
fn refute_claim_at_seq() {
    refute(Rules::ClaimAtSeq);
}

#[test]
#[ignore]
fn refute_loss_forgets_deleted_fields() {
    refute(Rules::LossForgetsDeletedFields);
}

#[test]
#[ignore]
fn refute_delete_sees_synced() {
    refute(Rules::DeleteSeesSynced);
}

#[test]
#[ignore]
fn refute_resend_revives_seen_value() {
    refute(Rules::ResendRevivesSeenValue);
}

#[test]
#[ignore]
fn refute_delete_drops_resends() {
    refute(Rules::DeleteDropsResends);
}

#[test]
#[ignore]
fn refute_delete_needs_synced_live() {
    refute(Rules::DeleteNeedsSyncedLive);
}

#[test]
#[ignore]
fn refute_seq_delete() {
    refute(Rules::SeqDelete);
}

#[test]
#[ignore]
fn refute_superseded_drops_own() {
    refute(Rules::SupersededDropsOwn);
}

#[test]
#[ignore]
fn refute_trash_settles_delete() {
    refute(Rules::TrashSettlesDelete);
}

#[test]
#[ignore]
fn refute_delete_sees_only_local() {
    refute(Rules::DeleteSeesOnlyLocal);
}

#[test]
#[ignore]
fn refute_loss_keeps_stale_seq() {
    refute(Rules::LossKeepsStaleSeq);
}

#[test]
#[ignore]
fn refute_deleted_adopts_records() {
    refute(Rules::DeletedAdoptsRecords);
}

#[test]
#[ignore]
fn refute_bound_only() {
    refute(Rules::BoundOnly);
}

#[test]
#[ignore]
fn refute_listed_removals_in_recovery() {
    refute(Rules::ListedRemovalsInRecovery);
}

#[test]
#[ignore]
fn refute_removal_takes_listed_shape() {
    refute(Rules::RemovalTakesListedShape);
}

#[test]
#[ignore]
fn refute_highest_held() {
    refute(Rules::HighestHeld);
}

#[test]
#[ignore]
fn refute_delete_sees_only_view() {
    refute(Rules::DeleteSeesOnlyView);
}

#[test]
#[ignore]
fn refute_bound_only_resends_seen() {
    refute(Rules::BoundOnlyResendsSeen);
}

#[test]
#[ignore]
fn refute_trash_covers_view() {
    refute(Rules::TrashCoversView);
}

#[test]
#[ignore]
fn refute_edit_sees_only_base() {
    refute(Rules::EditSeesOnlyBase);
}

#[test]
#[ignore]
fn refute_no_lost_position() {
    refute(Rules::NoLostPosition);
}

#[test]
#[ignore]
fn refute_absent_delete_resent() {
    refute(Rules::AbsentDeleteResent);
}

#[test]
#[ignore]
fn refute_all_fixes() {
    refute(Rules::AllFixes);
}

#[test]
#[ignore]
fn refute_held_fixes() {
    refute(Rules::HeldFixes);
}

#[test]
#[ignore]
fn refute_held_position_fixes() {
    refute(Rules::HeldPositionFixes);
}

#[test]
#[ignore]
fn refute_refused_as_main() {
    refute(Rules::RefusedAsMain);
}

#[test]
#[ignore]
fn refute_main() {
    refute(Rules::Main);
}

fn exhaustive(n: [u8; 10], rules: Rules) {
    exhaustive_from(budget(n), rules);
}

fn exhaustive_from(n: Budget, rules: Rules) {
    let start = std::time::Instant::now();
    let report = check_deep(n, rules);
    println!(
        "{rules:?} {n:?}: {} states, {} converged ({} with a spurious copy) in {:?}",
        report.states,
        report.converged,
        report.spurious,
        start.elapsed()
    );
    assert_eq!(report.broken, None);
    assert!(!report.capped, "MODEL_MAX_STATES stopped the check");
}

#[test]
#[ignore]
fn exhaustive_every_user_event_one_restore() {
    exhaustive(LARGE[0], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_every_user_event_one_restore() {
    exhaustive(LARGE[0], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_trash_restore_one_restore() {
    exhaustive(LARGE[1], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_trash_restore_one_restore() {
    exhaustive(LARGE[1], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_moves_and_a_delete_two_restores() {
    exhaustive(LARGE[2], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_moves_and_a_delete_two_restores() {
    exhaustive(LARGE[2], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_renames_and_trash_restore_two_restores() {
    exhaustive(LARGE[3], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_renames_and_trash_restore_two_restores() {
    exhaustive(LARGE[3], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_writes_and_deletes_two_restores() {
    exhaustive(LARGE[4], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_writes_and_deletes_two_restores() {
    exhaustive(LARGE[4], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_moves_and_deletes_two_restores() {
    exhaustive(LARGE[5], Rules::Spec);
}

#[test]
#[ignore]
fn old_server_moves_and_deletes_two_restores() {
    exhaustive(LARGE[5], Rules::OldServer);
}

#[test]
#[ignore]
fn old_deleter_writes_and_deletes_one_restore() {
    exhaustive([1, 1, 0, 0, 1, 1, 0, 0, 0, 1], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_renames_and_trash_restore_one_restore() {
    exhaustive([0, 1, 1, 0, 1, 0, 0, 1, 1, 1], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_every_user_event_one_restore() {
    exhaustive(LARGE[0], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_trash_restore_one_restore() {
    exhaustive(LARGE[1], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_moves_and_a_delete_two_restores() {
    exhaustive(LARGE[2], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_renames_and_trash_restore_two_restores() {
    exhaustive(LARGE[3], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_writes_and_deletes_two_restores() {
    exhaustive(LARGE[4], Rules::OldDeleter);
}

#[test]
#[ignore]
fn old_deleter_moves_and_deletes_two_restores() {
    exhaustive(LARGE[5], Rules::OldDeleter);
}

#[test]
#[ignore]
fn exhaustive_created_a_move_and_a_delete_one_restore() {
    exhaustive_from(created(CREATED[0]), Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_created_a_move_and_deletes_one_restore() {
    exhaustive_from(created(CREATED[1]), Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_created_writes_and_deletes_one_restore() {
    exhaustive_from(created(CREATED[2]), Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_created_moves_and_deletes_one_restore() {
    exhaustive_from(created(CREATED[3]), Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_created_a_peer_edit_one_restore() {
    exhaustive_from(created(CREATED[4]), Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_created_writes_moves_and_a_delete_two_restores() {
    exhaustive_from(created(CREATED[5]), Rules::Spec);
}

#[test]
fn a_small_budget_breaks_no_rule() {
    exhaustive([0, 1, 0, 1, 1, 0, 0, 0, 0, 1], Rules::Spec);
}

const MADE_BY_A: Budget = Budget {
    created_by: Some(A),
    ..ANY
};

fn fixed(by: &[Rules], trace: &[Action], broken: &str) {
    if let Err(broken) = settle(MADE_BY_A, Rules::Spec, trace) {
        panic!("{broken}");
    }
    self::broken(by, trace, broken);
}

fn broken(by: &[Rules], trace: &[Action], broken: &str) {
    for rules in by {
        let found = settle(MADE_BY_A, *rules, trace).unwrap_err();
        assert!(found.contains(broken), "{rules:?}: {found}");
    }
}

#[test]
fn a_delete_stays_when_a_resent_create_claims_a_renamed_position() {
    fixed(
        &[Rules::Main],
        &[
            Move(A),
            Build(A, 1),
            Delete(A),
            ServeRenamed(A),
            Answer(A),
            Record(B),
            Lose(1),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Bootstrap(A),
            Answer(B),
            Record(B),
        ],
        "came undone",
    );
}

#[test]
fn a_refused_lost_position_create_leaves_the_file_in_place() {
    fixed(
        &[Rules::Main, Rules::RefusedAsMain],
        &[
            Move(A),
            Build(A, 1),
            Write(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Serve(B),
            Answer(B),
            Record(B),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Delete(A),
            Bootstrap(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Build(A, 1),
            Serve(A),
            Serve(B),
            Answer(B),
            Record(B),
            Record(B),
            Record(B),
        ],
        "holds Position Tok(0) as Own",
    );
}

#[test]
fn a_delete_of_a_lost_node_stays_when_another_device_resends_its_create() {
    fixed(
        &[Rules::Main],
        &[
            Delete(A),
            Lose(1),
            Bootstrap(A),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
        ],
        "came undone",
    );
}

#[test]
fn a_delete_while_a_lost_position_create_is_in_flight_survives_its_landing() {
    let trace = [
        Move(B),
        Lose(1),
        Bootstrap(B),
        Build(B, 1),
        Serve(B),
        Bootstrap(A),
        Build(A, 1),
        Serve(A),
        Answer(A),
        Record(A),
        Record(B),
        Record(B),
        Lose(1),
        Bootstrap(A),
        Answer(B),
        Bootstrap(B),
        Build(B, 1),
        Delete(B),
        Serve(B),
        Record(A),
        Answer(B),
        Record(B),
    ];
    if let Err(broken) = settle(MADE_BY_A, Rules::Spec, &trace) {
        panic!("{broken}");
    }
}

#[test]
fn a_delete_stays_across_a_restore_and_back() {
    fixed(
        &[Rules::Main],
        &[Delete(A), Lose(1), Bootstrap(A), Action::Unlose, Record(A)],
        "came undone",
    );
}

#[test]
fn a_trash_restore_settles_the_delete_its_trash_replaced() {
    settle(
        MADE_BY_A,
        Rules::Spec,
        &[
            Delete(B),
            Build(B, 1),
            Write(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Delete(A),
            Build(A, 1),
            Serve(A),
            Serve(B),
            Answer(A),
            Answer(B),
            TrashRestore(B),
        ],
    )
    .unwrap();
}

#[test]
fn a_delete_overrides_a_move_resent_after_its_create_was_refused() {
    broken(
        &[Rules::AllFixes],
        &[
            Move(A),
            Build(A, 1),
            Write(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Serve(B),
            Answer(B),
            Record(B),
            Lose(1),
            Bootstrap(B),
            Build(B, 1),
            Delete(B),
            Serve(B),
            Bootstrap(A),
            Build(A, 2),
            Serve(A),
            Answer(A),
            Record(A),
            Answer(B),
            Record(B),
            Record(B),
            Build(B, 1),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
        ],
        "device 0's Position Tok(2) is lost",
    );
}

#[test]
fn a_delete_built_before_a_resent_move_lands_overrides_it() {
    broken(
        &[Rules::HeldFixes],
        &[
            Move(A),
            Build(A, 1),
            Write(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Serve(B),
            Answer(B),
            Record(B),
            Lose(1),
            Bootstrap(B),
            Build(B, 1),
            Delete(B),
            Serve(B),
            Bootstrap(A),
            Build(A, 2),
            Answer(B),
            Record(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
            Record(B),
        ],
        "device 0's Position Tok(2) is lost",
    );
}

#[test]
fn a_delete_after_a_move_overrides_the_position_its_resent_create_carries() {
    broken(
        &[Rules::HeldPositionFixes],
        &[
            Move(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Bootstrap(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Serve(B),
            Record(B),
            Lose(1),
            Answer(B),
            Bootstrap(B),
            Build(B, 1),
            Move(B),
            Delete(B),
            Serve(B),
            Bootstrap(A),
            Build(A, 1),
            Answer(B),
            Record(B),
            Build(B, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
            Record(B),
        ],
        "device 0's Position Tok(2) is lost",
    );
}

fn handled(trace: &[Action], broken_by: &[Rules]) {
    if let Err(broken) = settle(ANY, Rules::Spec, trace) {
        panic!("{broken}");
    }
    for rules in broken_by {
        assert!(
            settle(ANY, *rules, trace).is_err(),
            "{rules:?} handles the trace too"
        );
    }
}

#[test]
fn a_resend_whose_record_arrives_before_its_result_stays_a_resend_after_a_second_loss() {
    handled(
        &[
            Move(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Serve(A),
            Record(A),
            Answer(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Delete(B),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Serve(A),
            Answer(A),
            Record(A),
        ],
        &[Rules::LandedResendIsOwn],
    );
}

#[test]
fn a_write_whose_resend_another_device_landed_first_survives_a_second_loss() {
    handled(
        &[
            Write(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Bootstrap(B),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Lose(2),
            Bootstrap(B),
            Bootstrap(A),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
        ],
        &[],
    );
}

#[test]
fn a_write_kept_in_a_conflict_copy_survives_a_loss() {
    handled(
        &[
            Write(A),
            Write(B),
            Build(B, 1),
            Build(A, 1),
            Serve(B),
            Serve(A),
            Answer(A),
            Record(A),
            Record(A),
            Answer(B),
            Record(B),
            Record(B),
            Lose(2),
            Bootstrap(A),
            Bootstrap(B),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
        ],
        &[],
    );
}

#[test]
fn a_resend_another_device_deleted_and_restored_before_its_result_stays_a_resend() {
    handled(
        &[
            Move(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Serve(A),
            Delete(B),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            TrashRestore(B),
            Record(B),
            Record(A),
            Record(A),
            Record(A),
            Answer(A),
            Lose(1),
            Bootstrap(A),
            Bootstrap(B),
        ],
        &[Rules::LandedResendIsOwn],
    );
}

#[test]
fn a_resend_under_a_server_name_follows_a_later_delete() {
    handled(
        &[
            Move(B),
            Build(B, 1),
            Serve(B),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            ServeRenamed(A),
            Answer(A),
            Record(A),
            Answer(B),
            Delete(B),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
        ],
        &[],
    );
}

#[test]
fn a_loss_forgets_only_the_fields_learned_after_it() {
    handled(
        &[
            Write(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Move(B),
            Record(B),
            Build(B, 1),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
            Lose(2),
            Bootstrap(A),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
        ],
        &[],
    );
}

#[test]
fn another_devices_content_stays_its_own_beside_this_users_move() {
    handled(
        &[
            Write(B),
            Build(B, 1),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
            Move(A),
            Lose(1),
            Bootstrap(A),
            Bootstrap(B),
            Build(A, 2),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
        ],
        &[Rules::NodeAuthor],
    );
}

#[test]
fn an_authors_lost_move_beats_a_delete_that_did_not_see_it() {
    handled(
        &[
            Move(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Delete(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
        ],
        &[],
    );
}

#[test]
fn a_move_result_leaves_an_edit_the_cap_left_out_unsent() {
    handled(
        &[
            Move(A),
            Write(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 2),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
        ],
        &[Rules::MoveResultEndsContent],
    );
}

#[test]
fn a_delete_in_a_listing_does_not_take_back_an_unsent_edit() {
    handled(
        &[
            Delete(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            TrashRestore(B),
            Record(B),
            Record(A),
            Record(A),
            Write(A),
            Lose(2),
            Bootstrap(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Bootstrap(B),
        ],
        &[],
    );
}

#[test]
fn resent_content_a_resent_move_landed_without_gives_way_to_a_later_delete() {
    handled(
        &[
            Write(B),
            Move(B),
            Build(B, 2),
            Serve(B),
            Record(A),
            Answer(B),
            Record(B),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Delete(B),
            Bootstrap(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
        ],
        &[],
    );
}

#[test]
fn a_user_write_while_a_resent_move_waits_beats_a_later_delete() {
    handled(
        &[
            Write(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Move(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Write(A),
            Serve(A),
            Answer(A),
            Record(A),
            Bootstrap(B),
            Delete(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
        ],
        &[],
    );
}

#[test]
fn a_user_write_while_a_resent_move_of_resent_content_waits_beats_a_later_delete() {
    handled(
        &[
            Write(B),
            Move(B),
            Build(B, 2),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Lose(1),
            Bootstrap(A),
            Build(A, 1),
            Write(A),
            Serve(A),
            Answer(A),
            Record(A),
            Bootstrap(B),
            Delete(B),
            Build(B, 1),
            Serve(B),
            Answer(B),
            Record(B),
            Record(A),
            Build(A, 1),
            Serve(A),
            Answer(A),
            Record(A),
            Record(B),
        ],
        &[Rules::MoveResultMarksContentResent],
    );
}

#[test]
fn a_restored_older_backup_does_not_turn_a_rewritten_value_into_a_conflict() {
    handled(
        &[
            Write(1),
            Build(1, 1),
            Serve(1),
            Record(1),
            Answer(1),
            Delete(1),
            Build(1, 1),
            Serve(1),
            Record(1),
            Answer(1),
            Record(0),
            Write(0),
            Record(0),
            Build(0, 1),
            Serve(0),
            Lose(1),
            Bootstrap(1),
            Build(1, 1),
            Answer(0),
            Bootstrap(0),
            Action::Unlose,
            Record(1),
            Record(1),
            Record(1),
            Serve(1),
            Record(1),
            Answer(1),
            Record(0),
            Delete(0),
            Record(0),
            Record(0),
            Record(0),
            Build(0, 1),
            Serve(0),
            Record(1),
            Record(0),
            Answer(0),
        ],
        &[],
    );
}

#[test]
fn an_old_server_restore_does_not_let_a_carried_position_overwrite_a_later_move() {
    let trace = [
        Delete(1),
        Build(1, 1),
        Serve(1),
        Record(1),
        Answer(1),
        Move(0),
        Record(0),
        Build(0, 1),
        Serve(0),
        Record(1),
        Move(1),
        Build(1, 1),
        Serve(1),
        Record(1),
        Answer(1),
        Record(0),
        Record(0),
        Answer(0),
        Lose(2),
        Bootstrap(1),
        Build(1, 1),
        Write(0),
        Bootstrap(0),
        Build(0, 2),
        Serve(0),
        Record(1),
        Serve(1),
        Record(1),
        Answer(1),
        Record(0),
        Answer(0),
        Action::Unlose,
        Bootstrap(1),
        Build(1, 1),
        Serve(1),
        Record(1),
        Answer(1),
        Bootstrap(0),
    ];
    let budget = budget([1, 0, 1, 1, 0, 1, 0, 0, 0, 2]);
    if let Err(broken) = settle(budget, Rules::OldServer, &trace) {
        panic!("{broken}");
    }
    let broken = settle(budget, Rules::BoundOnlyResendsSeen, &trace).unwrap_err();
    assert!(broken.contains("is lost"), "{broken}");
}

#[test]
fn a_trash_that_saw_less_than_the_delete_does_not_settle_it() {
    handled(
        &[
            Write(1),
            Build(1, 1),
            Delete(1),
            Write(0),
            Build(0, 1),
            Serve(1),
            Record(1),
            Answer(1),
            Build(1, 1),
            Serve(1),
            Record(1),
            Answer(1),
            Record(0),
            Record(0),
            Serve(0),
            Lose(2),
            Bootstrap(1),
            Build(1, 1),
            Answer(0),
            Bootstrap(0),
            Action::Unlose,
            Record(1),
            Record(1),
            Serve(1),
            Record(1),
            Answer(1),
            Delete(0),
            Record(0),
            Record(0),
            Record(0),
            Build(0, 1),
            Serve(0),
            Record(1),
            Record(0),
            Answer(0),
        ],
        &[Rules::TrashCoversView],
    );
}
