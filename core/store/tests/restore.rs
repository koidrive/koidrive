// SPDX-License-Identifier: Apache-2.0

//! A restore from its request to its outcome, with the database closed and opened again at every
//! point where the process can stop (the crash table of docs/design/sync-engine.md section 2.8).
//! The engine's commit side runs on what the store hands back each time, so the test fails if a
//! stop loses the restore, sends it a second time, or leaves it in two places. The numbers are the
//! rows of that table. Row 8, a stop during an effect that puts a restored node on disk, is the
//! effect recovery of section 5.3 and is covered in the simulation.

use std::path::Path;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Commits, Halt, Reconnect};
use koidrive_engine::ids::{IdGen, uuid_v7};
use koidrive_engine::restore::{Outcome, Resolved, RestoreId, RestoreIntent};
use koidrive_proto::v1::{CommitResult, OpResult, Welcome, op, op_result};
use koidrive_store::{EngineState, Error, Store};
use rusqlite::Connection;

const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";

/// The commit side as the store holds it.
fn load(db: &Path) -> Commits {
    let state = Store::open(db)
        .unwrap()
        .transaction(|tx| tx.engine_state())
        .unwrap();
    Commits::from_state(state.commits).unwrap()
}

fn save(db: &Path, commits: &Commits) {
    let state = EngineState {
        commits: commits.state(),
        ..Default::default()
    };
    Store::open(db)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
}

fn waiting(db: &Path) -> Vec<(RestoreId, RestoreIntent)> {
    Store::open(db)
        .unwrap()
        .transaction(|tx| tx.restore_requests())
        .unwrap()
}

/// What the task that runs the engine does with the inbox, in one transaction: it takes the
/// requests, queues them in the engine and saves the engine state.
fn take(db: &Path) {
    Store::open(db)
        .unwrap()
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let mut commits = Commits::from_state(state.commits).unwrap();
            for (id, intent) in tx.take_restore_requests()? {
                commits.restore(id, intent);
            }
            state.commits = commits.state();
            tx.set_engine_state(&state)
        })
        .unwrap();
}

/// A `Welcome` in which the server counts `device_seq` commits of this device.
fn welcome(device_seq: u64) -> Welcome {
    Welcome {
        history_id: HISTORY.into(),
        device_seq,
        ..Default::default()
    }
}

#[test]
fn a_restore_is_neither_lost_nor_sent_twice_wherever_the_process_stops() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let (root, node, request) = (uuid_v7(1, 0), uuid_v7(1, 1), uuid_v7(2, 1));
    let intent = RestoreIntent::from_trash(node, None, None).unwrap();
    let (mut trees, mut ids) = (Trees::new(root), IdGen::new(1));
    let queued = |commits: &Commits| commits.restores().keys().cloned().collect::<Vec<_>>();
    let sent = |commits: &Commits| commits.in_flight().map(|flight| flight.commit.clone());

    // 1. The process stops before the request's transaction commits. The caller got no ID.
    let stopped = Store::open(&db).unwrap().transaction(|tx| {
        tx.request_restore(&request, &intent)?;
        Err::<(), _>(Error::InvalidEngineState("the process stopped here".into()))
    });
    assert!(stopped.is_err());
    assert_eq!(waiting(&db), vec![]);

    // 2. The request is stored and nothing has taken it.
    Store::open(&db)
        .unwrap()
        .transaction(|tx| tx.request_restore(&request, &intent))
        .unwrap();
    assert_eq!(waiting(&db), vec![(request.clone(), intent.clone())]);
    assert_eq!(queued(&load(&db)), Vec::<String>::new());

    // 3. The engine took it. Taking again, as every start of the folder does, changes nothing.
    take(&db);
    take(&db);
    assert_eq!(waiting(&db), vec![]);
    assert_eq!(queued(&load(&db)), std::slice::from_ref(&request));
    let backup = dir.path().join("backup.db");
    let copy = Connection::open(&db).unwrap();
    copy.execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
        .unwrap();

    // 4. The commit is built and saved, and the process stops before it leaves. The server
    // does not count it, and it is sent as it was built.
    let mut commits = load(&db);
    let commit = commits
        .build(&mut trees, &mut ids, NOW, HISTORY, "me", 7)
        .cloned();
    let commit = commit.unwrap();
    assert_eq!(commit.ops[0].op, Some(op::Op::Restore(intent.op().clone())));
    save(&db, &commits);
    let mut commits = load(&db);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Resend);
    assert_eq!(sent(&commits), Some(commit.clone()));

    // 5. The server applied it and the response was lost. The same commit is sent again, and
    // the server answers a commit it knows with the stored result.
    let mut commits = load(&db);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Resend);
    assert_eq!(sent(&commits), Some(commit.clone()));
    assert_eq!(queued(&commits), std::slice::from_ref(&request));

    // 6. The result is saved and R has not reached its seq. Nothing is sent, and the restore
    // has no outcome yet.
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 8,
        results: vec![applied],
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    save(&db, &commits);
    let mut commits = load(&db);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Idle);
    assert!(!commits.acknowledge(&mut trees, 7));
    assert_eq!(
        (queued(&commits), commits.resolved()),
        (vec![request.clone()], &[][..])
    );

    // 7. The commit is acknowledged. The outcome is saved with the cleared commit and the
    // counter, and nothing is left to send.
    assert!(commits.acknowledge(&mut trees, 8));
    save(&db, &commits);
    let mut commits = load(&db);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Idle);
    assert_eq!((sent(&commits), commits.accepted()), (None, 1));
    assert_eq!(queued(&commits), Vec::<String>::new());
    let ended = Resolved {
        id: request.clone(),
        intent: intent.clone(),
        outcome: Outcome::Restored,
    };
    assert_eq!(commits.resolved(), [ended]);
    assert_eq!(
        commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 8),
        None
    );

    // 9. The store is replaced by the copy from before the commit was built. The server counts
    // a commit the copy does not know, so the space enters recovery, and the restore the copy
    // still holds is kept and not sent.
    let mut commits = load(&backup);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Recovery);
    save(&backup, &commits);
    let mut commits = load(&backup);
    assert!(matches!(commits.halted(), Some(Halt::Recovery(_))));
    assert_eq!(queued(&commits), [request]);
    assert_eq!(
        commits.build(&mut trees, &mut ids, NOW, HISTORY, "me", 8),
        None
    );
}
