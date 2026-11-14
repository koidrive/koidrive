// SPDX-License-Identifier: Apache-2.0

#[path = "attribution/model.rs"]
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
        .split(')'))
    .filter(|step| !step.trim_matches(',').is_empty())
    .map(|step| {
        let (name, args) = step.trim_matches(',').split_once('(').unwrap();
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
        ANY,
        rules(&std::env::var("MODEL_RULES").unwrap_or("Spec".into())),
        &trace,
    );
}

const A: usize = 0;
const B: usize = 1;

const SMALL: Budget = Budget {
    writes: [1, 1],
    moves: [1, 0],
    deletes: [0, 1],
    trash_restores: [0, 0],
    renames: 0,
    losses: 1,
};

const ANY: Budget = Budget {
    writes: [2, 2],
    moves: [2, 2],
    deletes: [1, 1],
    trash_restores: [1, 1],
    renames: 1,
    losses: 2,
};

fn rules(name: &str) -> Rules {
    match name {
        "Spec" => Rules::Spec,
        "MoveResultEndsContent" => Rules::MoveResultEndsContent,
        "MoveResultMarksContentResent" => Rules::MoveResultMarksContentResent,
        "LandedResendIsOwn" => Rules::LandedResendIsOwn,
        "ResendRecordEndsOwn" => Rules::ResendRecordEndsOwn,
        "NodeAuthor" => Rules::NodeAuthor,
        "TrashedRecordProves" => Rules::TrashedRecordProves,
        "ClaimAtSeq" => Rules::ClaimAtSeq,
        "LossForgetsDeletedFields" => Rules::LossForgetsDeletedFields,
        "SupersededDeleteGivesUp" => Rules::SupersededDeleteGivesUp,
        "DeleteSeesSynced" => Rules::DeleteSeesSynced,
        "ResendRevivesSeenValue" => Rules::ResendRevivesSeenValue,
        "DeleteDropsResends" => Rules::DeleteDropsResends,
        "StaleTrashProves" => Rules::StaleTrashProves,
        "DeleteNeedsSyncedLive" => Rules::DeleteNeedsSyncedLive,
        "SeqDelete" => Rules::SeqDelete,
        "SupersededDropsOwn" => Rules::SupersededDropsOwn,
        "TrashSettlesDelete" => Rules::TrashSettlesDelete,
        "DeleteSeesOnlyLocal" => Rules::DeleteSeesOnlyLocal,
        "LossKeepsStaleSeq" => Rules::LossKeepsStaleSeq,
        "OldServer" => Rules::OldServer,
        "OldDeleter" => Rules::OldDeleter,
        "DeletedAdoptsRecords" => Rules::DeletedAdoptsRecords,
        "RevivalMarksPosition" => Rules::RevivalMarksPosition,
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
    };
    for name in std::env::var("MODEL_RULES").unwrap().split(',') {
        let start = std::time::Instant::now();
        let report = if std::env::var("MODEL_DEEP").is_ok() {
            check_deep(budget, rules(name))
        } else {
            check(budget, rules(name))
        };
        println!(
            "{name}: {} states, {} converged, {:?} in {:?}",
            report.states,
            report.converged,
            report.broken,
            start.elapsed()
        );
        if std::env::var("MODEL_CHART").is_ok() {
            for (field, before, label, after) in &report.steps {
                println!("{field:?}: {before} --{label}--> {after}");
            }
        }
    }
}

#[test]
#[ignore]
fn refute_from_env() {
    let budgets = [
        [0, 0, 0, 1, 1, 0, 0, 0, 0, 1],
        [0, 1, 0, 0, 1, 0, 0, 0, 0, 1],
        [0, 0, 0, 1, 1, 0, 0, 1, 0, 2],
        [0, 1, 0, 0, 1, 0, 0, 0, 0, 2],
        [1, 1, 0, 0, 1, 1, 0, 0, 0, 1],
        [0, 0, 1, 1, 1, 1, 0, 0, 0, 1],
        [0, 1, 1, 0, 1, 0, 0, 1, 1, 1],
        [1, 1, 1, 1, 1, 1, 0, 0, 0, 1],
    ];
    for name in std::env::var("MODEL_RULES").unwrap().split(',') {
        let found = budgets.iter().find_map(|n| {
            let budget = Budget {
                writes: [n[0], n[1]],
                moves: [n[2], n[3]],
                deletes: [n[4], n[5]],
                trash_restores: [n[6], n[7]],
                renames: n[8],
                losses: n[9],
            };
            let deep = check_deep(budget, rules(name));
            let broken = deep.broken?;
            let shortest = if deep.states < 20_000_000 {
                check(budget, rules(name)).broken
            } else {
                None
            };
            Some((n, shortest.unwrap_or(broken)))
        });
        println!("{name}: {found:?}");
        use std::io::Write as _;
        std::io::stdout().flush().unwrap();
    }
}

fn exhaustive(budget: [u8; 10], rules: Rules) {
    let budget = Budget {
        writes: [budget[0], budget[1]],
        moves: [budget[2], budget[3]],
        deletes: [budget[4], budget[5]],
        trash_restores: [budget[6], budget[7]],
        renames: budget[8],
        losses: budget[9],
    };
    let start = std::time::Instant::now();
    let report = check_deep(budget, rules);
    println!(
        "{rules:?} {budget:?}: {} states, {} converged in {:?}",
        report.states,
        report.converged,
        start.elapsed()
    );
    assert_eq!(report.broken, None);
}

#[test]
#[ignore]
fn exhaustive_every_user_event_one_restore() {
    exhaustive([1, 1, 1, 1, 1, 1, 0, 0, 0, 1], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_trash_restore_one_restore() {
    exhaustive([1, 1, 1, 0, 0, 1, 0, 1, 0, 1], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_moves_and_a_delete_two_restores() {
    exhaustive([1, 0, 1, 1, 0, 1, 0, 0, 0, 2], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_renames_and_trash_restore_two_restores() {
    exhaustive([0, 1, 1, 0, 1, 0, 0, 1, 1, 2], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_writes_and_deletes_two_restores() {
    exhaustive([1, 1, 0, 0, 1, 1, 0, 0, 0, 2], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_moves_and_deletes_two_restores() {
    exhaustive([0, 0, 1, 1, 1, 1, 0, 0, 0, 2], Rules::Spec);
}

#[test]
#[ignore]
fn exhaustive_old_server() {
    exhaustive([1, 1, 0, 0, 1, 1, 0, 0, 0, 1], Rules::OldServer);
    exhaustive([0, 1, 1, 0, 1, 0, 0, 1, 1, 1], Rules::OldServer);
}

#[test]
#[ignore]
fn exhaustive_old_deleter() {
    exhaustive([1, 1, 0, 0, 1, 1, 0, 0, 0, 1], Rules::OldDeleter);
    exhaustive([0, 1, 1, 0, 1, 0, 0, 1, 1, 1], Rules::OldDeleter);
}

#[test]
fn explore() {
    let start = std::time::Instant::now();
    let report = check(SMALL, Rules::Spec);
    println!(
        "{} states, {} converged, {:?} in {:?}",
        report.states,
        report.converged,
        report.broken,
        start.elapsed()
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
