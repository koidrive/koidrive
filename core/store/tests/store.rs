// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::{Commits, Halt, InFlight, Reconnect};
use koidrive_engine::effects::{Base, BatchLimit, Dir, Displaced, Effect, EffectRecord, Location};
use koidrive_engine::guard::{Counted, GuardState, RemoteDeletion, Trash};
use koidrive_engine::ids::{IdGen, uuid_v7};
use koidrive_engine::question::{
    Answer, Held, Limit, Question, QuestionKind, RecoveryCause, RecoveryGroup, RecoveryTrigger,
    Revisions,
};
use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};
use koidrive_engine::state::{
    Agreed, CommitsState, EffectsState, FinishOrigin, MovedHere, PendingFinish, TreesState,
};
use koidrive_engine::tree::{
    FileIdentity, Kind, LocalNode as EngineLocalNode, RemoteNode, Shape as EngineShape,
};
use koidrive_fs::FileId;
use koidrive_proto::v1::{
    ChunkRef, Commit, CommitResult, Content, CreateFile, Delete, NodeKind, NodeState, NodeStatus,
    Op, OpResult, VersionInfo, Welcome, op, op_result,
};
use koidrive_store::migrations::{LEGACY_STEPS, MIGRATIONS, NAMED};
use koidrive_store::{
    AskedRestore, CachedChunk, Cursor, Durability, EngineState, Error, GivenAnswer, Link,
    LocalNode, Shape, Store, StoredQuestion, SyncedNode,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rusqlite::Connection;

fn content(size: u64) -> Content {
    Content {
        size,
        mtime_ns: 1_700_000_000_000_000_000,
        executable: true,
        chunks: vec![ChunkRef {
            hash: vec![7; 32],
            size: size as u32,
            encoding: 0,
        }],
        chunk_count: 1,
        manifest_digest: vec![9; 32],
        chunker: "fastcdc2020-1m-4m-16m".into(),
    }
}

fn file_shape(parent_id: &str, name: &str) -> Shape {
    Shape {
        parent_id: parent_id.into(),
        name: name.into(),
        kind: NodeKind::File as i32,
        content: Some(content(3)),
    }
}

fn dir_shape(parent_id: &str, name: &str) -> Shape {
    Shape {
        parent_id: parent_id.into(),
        name: name.into(),
        kind: NodeKind::Dir as i32,
        content: None,
    }
}

#[test]
fn migrations_apply_from_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    drop(store);

    let conn = Connection::open(&path).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        tables,
        [
            "chunk_cache",
            "commit_in_flight",
            "commit_in_flight_delete_members",
            "commit_in_flight_deletes",
            "commit_in_flight_restore_members",
            "commit_slots",
            "commit_state",
            "discarded_nodes",
            "dropped_edits",
            "edited_here",
            "effect_state",
            "effects_in_flight",
            "engine_state",
            "failed_effects",
            "guard_confirmed",
            "guard_confirmed_limits",
            "guard_confirmed_members",
            "guard_ledger",
            "guard_remote",
            "guard_remote_dismissed",
            "kept_originals",
            "link",
            "local_changes",
            "local_nodes",
            "migrations",
            "moved_here",
            "node_maps",
            "node_sets",
            "open_question_items",
            "open_question_members",
            "open_question_shapes",
            "open_questions",
            "past_positions",
            "pending_finishes",
            "published_nodes",
            "questions",
            "recorded_nodes",
            "remote_cursor",
            "remote_nodes",
            "removed_here",
            "replaced_versions",
            "resends",
            "restore_asked",
            "restore_intents",
            "restore_requests",
            "synced_nodes",
            "taken_answers",
            "trashed_here",
            "unsent_edits",
            "unsent_marks",
            "waiting_updates",
            "watched_contents",
            "watched_positions",
        ]
    );
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
}

#[test]
fn connection_uses_durable_commits() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state.db")).unwrap();
    let durability = store.durability().unwrap();
    assert_eq!(durability, Durability::required());
    assert_eq!(durability.journal_mode, "wal");
    assert_eq!(durability.synchronous, 2);
    assert_eq!(durability.fullfsync, cfg!(target_os = "macos"));
}

#[test]
fn reopening_keeps_data_and_applied_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
        ahead_of_server: true,
        commit_id: "c7".into(),
        committed_at_ms: 70,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_cursor(&cursor))
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.cursor()).unwrap(), Some(cursor));
}

#[test]
fn a_store_with_a_migration_this_build_does_not_know_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO migrations (name) VALUES ('from_a_newer_build')",
            [],
        )
        .unwrap();

    for opened in [Store::open(&path), Store::open_read_only(&path)] {
        match opened {
            Err(Error::UnknownMigration(name)) => assert_eq!(name, "from_a_newer_build"),
            other => panic!("expected UnknownMigration, got {:?}", other.err()),
        }
    }
}

#[test]
fn a_store_a_migration_branch_build_wrote_is_refused_with_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    populate(&path);
    for version in [LEGACY_STEPS as u32 + 1, NAMED - 1, NAMED + 1] {
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        for opened in [Store::open(&path), Store::open_read_only(&path)] {
            let error = opened.err().unwrap();
            assert!(matches!(error, Error::DevelopmentSchema(found) if found == version));
            assert!(
                error.to_string().ends_with(
                    "delete this store file and link the folder again with `koi folder link`"
                ),
                "{error}"
            );
        }
    }
}

#[test]
fn stores_opened_at_once_apply_each_migration_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let opened: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| Store::open(&path).map(drop)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in opened {
        result.unwrap();
    }
    assert_eq!(applied(&path), all_migrations());
}

#[test]
fn a_migration_recorded_after_the_open_read_the_table_is_not_applied_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    populate(&path);
    rewind(&path, LEGACY_STEPS - 2);
    let next = &MIGRATIONS[LEGACY_STEPS - 2];
    let last = &MIGRATIONS[LEGACY_STEPS - 1];
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE migrations (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;")
        .unwrap();
    for m in &MIGRATIONS[..LEGACY_STEPS - 2] {
        conn.execute("INSERT INTO migrations (name) VALUES (?1)", [m.name])
            .unwrap();
    }
    conn.execute_batch(&format!(
        "CREATE TRIGGER other_opener AFTER INSERT ON migrations WHEN NEW.name = '{}'
         BEGIN INSERT INTO migrations (name) VALUES ('{}'); END;",
        next.name, last.name
    ))
    .unwrap();
    conn.pragma_update(None, "user_version", NAMED).unwrap();
    drop(conn);

    drop(Store::open(&path).unwrap());
    assert_eq!(applied(&path), all_migrations());
}

#[test]
fn an_upgraded_store_carries_a_version_no_positional_build_supports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    populate(&path);
    rewind(&path, LEGACY_STEPS - 2);
    drop(Store::open(&path).unwrap());
    let version: u32 = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1000);
}

#[test]
fn a_read_only_open_refuses_a_store_that_lacks_a_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    populate(&path);
    let last = &MIGRATIONS[LEGACY_STEPS - 1];
    store_before(&path, last.name);
    match Store::open_read_only(&path) {
        Err(Error::SchemaTooOld { missing }) => assert_eq!(missing, last.name),
        other => panic!("expected SchemaTooOld, got {:?}", other.err()),
    }
    drop(Store::open(&path).unwrap());
    assert!(
        Store::open_read_only(&path)
            .unwrap()
            .transaction(|tx| tx.link())
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_store_from_any_past_version_upgrades_and_keeps_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.db");
    drop(Store::open(&fresh).unwrap());
    for version in 0..=LEGACY_STEPS {
        let path = dir.path().join(format!("{version}.db"));
        populate(&path);
        rewind(&path, version);
        let mut kept = every_row(&path);
        assert_eq!(kept.values().any(|(_, rows)| !rows.is_empty()), version > 0);
        let moved = moved_in_flight(&path, &mut kept);
        assert_eq!(moved.len(), usize::from(version >= 3));

        drop(Store::open(&path).unwrap());

        assert_eq!(applied(&path), all_migrations(), "from version {version}");
        assert_eq!(schema(&path), schema(&fresh), "from version {version}");
        let conn = Connection::open(&path).unwrap();
        for (table, (columns, rows)) in kept.into_iter().filter(|(_, (_, rows))| !rows.is_empty()) {
            assert_eq!(
                table_rows(&conn, &table, &columns),
                rows,
                "{table} from version {version}"
            );
        }
        let in_flight = ["position".to_owned(), "record".to_owned()];
        assert_eq!(
            table_rows(&conn, "effects_in_flight", &in_flight),
            moved,
            "effects_in_flight from version {version}"
        );
    }
}

#[test]
fn a_store_written_by_alpha_1_upgrades_and_keeps_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/alpha1.db"),
        &path,
    )
    .unwrap();
    let mut kept = every_row(&path);
    let (_, discarded) = kept.remove("discarded_nodes").unwrap();
    assert_eq!(discarded.len(), 1);
    let moved = moved_in_flight(&path, &mut kept);
    assert_eq!(moved.len(), 1);
    let fresh = dir.path().join("fresh.db");
    drop(Store::open(&fresh).unwrap());

    let mut store = Store::open(&path).unwrap();
    let mut expected = legacy_state();
    expected.trees.lost.clear();
    expected.trees.outside.remove("lost");
    expected.trees.resends.clear();
    expected.trees.replaced_versions.clear();
    expected.trees.discarded.clear();
    let upgraded = MovedHere {
        seq: 9,
        upgraded: true,
    };
    expected.trees.moved_here = BTreeMap::from([("moved".into(), upgraded)]);
    store
        .transaction(|tx| {
            assert_eq!(tx.engine_state()?, expected);
            assert_eq!(tx.link()?.unwrap().space_name, "Projects");
            assert_eq!(tx.cursor()?.unwrap().last_seq, 30);
            assert_eq!(tx.local_nodes()?.len(), 1);
            Ok::<_, Error>(())
        })
        .unwrap();
    drop(store);

    assert_eq!(applied(&path), all_migrations());
    assert_eq!(schema(&path), schema(&fresh));
    let conn = Connection::open(&path).unwrap();
    for (table, (columns, rows)) in kept {
        assert_eq!(table_rows(&conn, &table, &columns), rows, "{table}");
    }
    let in_flight = ["position".to_owned(), "record".to_owned()];
    assert_eq!(table_rows(&conn, "effects_in_flight", &in_flight), moved);
}

#[test]
fn migrations_that_touch_different_things_commute_and_ones_that_clash_do_not() {
    let commuting = [
        Step {
            name: "a_mark",
            requires: vec![],
            sql: "ALTER TABLE remote_nodes ADD COLUMN a_mark TEXT;",
        },
        Step {
            name: "b_mark",
            requires: vec![],
            sql: "ALTER TABLE remote_nodes ADD COLUMN b_mark INTEGER NOT NULL DEFAULT 0;",
        },
        Step {
            name: "c_ids",
            requires: vec![],
            sql: "CREATE TABLE c_ids (id TEXT PRIMARY KEY) STRICT;
                  INSERT INTO c_ids SELECT lower(hex(randomblob(8))) FROM synced_nodes;",
        },
        Step {
            name: "d_ids_at",
            requires: vec!["c_ids"],
            sql: "ALTER TABLE c_ids ADD COLUMN at_ms INTEGER;",
        },
    ];
    assert_eq!(
        differences(&commuting, &permutations(commuting.len())),
        Vec::<String>::new()
    );

    let clashing = [
        Step {
            name: "x_seen",
            requires: vec![],
            sql: "CREATE TABLE x_seen AS SELECT node_id FROM synced_nodes;",
        },
        Step {
            name: "y_forget",
            requires: vec![],
            sql: "DELETE FROM synced_nodes WHERE node_id = 'moved';",
        },
    ];
    assert_eq!(differences(&clashing, &permutations(2)).len(), 1);
    for (first, second) in [
        (
            "CREATE TABLE IF NOT EXISTS z_defaults (v TEXT DEFAULT 'a  b');",
            "CREATE TABLE IF NOT EXISTS z_defaults (v TEXT DEFAULT 'a b');",
        ),
        (
            "CREATE TABLE IF NOT EXISTS z_notes (v TEXT -- note\nNOT NULL);",
            "CREATE TABLE IF NOT EXISTS z_notes (v TEXT -- note NOT NULL\n);",
        ),
        (
            "CREATE VIEW IF NOT EXISTS z_view (a, b) AS SELECT 1, 2;",
            "CREATE VIEW IF NOT EXISTS z_view (b, a) AS SELECT 1, 2;",
        ),
        (
            "CREATE TABLE z_captured AS SELECT node_id, randomblob(8) AS nonce FROM synced_nodes;",
            "UPDATE synced_nodes SET node_id = 'renamed' WHERE node_id = 'moved';",
        ),
    ] {
        let pair = [
            Step {
                name: "z_first",
                requires: vec![],
                sql: first,
            },
            Step {
                name: "z_second",
                requires: vec![],
                sql: second,
            },
        ];
        assert_eq!(
            differences(&pair, &permutations(2)).len(),
            1,
            "{first} / {second}"
        );
    }
    let ordered = [
        Step {
            name: "x_seen",
            requires: vec![],
            sql: clashing[0].sql,
        },
        Step {
            name: "y_forget",
            requires: vec!["x_seen"],
            sql: clashing[1].sql,
        },
    ];
    assert_eq!(
        differences(&ordered, &permutations(2)),
        Vec::<String>::new()
    );
}

#[test]
fn named_migrations_give_the_same_store_in_any_order() {
    let named: Vec<Step> = MIGRATIONS[LEGACY_STEPS..]
        .iter()
        .map(|m| Step {
            name: m.name,
            requires: m.requires.to_vec(),
            sql: m.sql,
        })
        .collect();
    let priorities = if named.len() <= 5 {
        permutations(named.len())
    } else {
        let shuffled = Just((0..named.len()).collect::<Vec<_>>()).prop_shuffle();
        let mut runner = TestRunner::deterministic();
        (0..64)
            .map(|_| shuffled.new_tree(&mut runner).unwrap().current())
            .collect()
    };
    assert_eq!(differences(&named, &priorities), Vec::<String>::new());
}

struct Step {
    name: &'static str,
    requires: Vec<&'static str>,
    sql: &'static str,
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    permutations(n - 1)
        .into_iter()
        .flat_map(|shorter| {
            (0..n).map(move |at| {
                let mut longer = shorter.clone();
                longer.insert(at, n - 1);
                longer
            })
        })
        .collect()
}

fn differences(steps: &[Step], priorities: &[Vec<usize>]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.db");
    populate(&base);
    rewind(&base, LEGACY_STEPS);
    let outcome = |name: &str, priority: &[usize]| {
        let mut order: Vec<&Step> = vec![];
        while order.len() < steps.len() {
            let next = priority
                .iter()
                .map(|&i| &steps[i])
                .find(|step| {
                    !order.iter().any(|placed| placed.name == step.name)
                        && step
                            .requires
                            .iter()
                            .all(|r| order.iter().any(|placed| placed.name == *r))
                })
                .unwrap();
            order.push(next);
        }
        let path = dir.path().join(name);
        std::fs::copy(&base, &path).unwrap();
        let conn = Connection::open(&path).unwrap();
        koidrive_store::migrations::register_functions(&conn).unwrap();
        conn.pragma_update(None, "synchronous", "OFF").unwrap();
        for step in &order {
            conn.execute_batch(step.sql).unwrap();
        }
        drop(conn);
        let names: Vec<&str> = order.iter().map(|step| step.name).collect();
        let (schema, rows) = (schema(&path), every_row(&path));
        (names, Connection::open(&path).unwrap(), schema, rows)
    };
    let by_name: Vec<usize> = (0..steps.len()).collect();
    let (_, by_name_db, schema_by_name, rows_by_name) = outcome("by_name.db", &by_name);
    let (_, again_db, _, _) = outcome("again.db", &by_name);
    let repeatable: BTreeMap<&String, Vec<String>> = rows_by_name
        .iter()
        .map(|(table, (columns, _))| {
            let kept = columns
                .iter()
                .filter(|column| {
                    let one = [(*column).clone()];
                    table_rows(&by_name_db, table, &one) == table_rows(&again_db, table, &one)
                })
                .cloned()
                .collect();
            (table, kept)
        })
        .collect();
    let mut found = vec![];
    for (n, priority) in priorities.iter().enumerate() {
        let (names, db, schema, rows) = outcome(&format!("{n}.db"), priority);
        let differs = schema != schema_by_name
            || !rows.keys().eq(rows_by_name.keys())
            || rows.iter().any(|(table, (_, rows))| {
                let columns = &repeatable[table];
                rows.len() != rows_by_name[table].1.len()
                    || (!columns.is_empty()
                        && table_rows(&db, table, columns)
                            != table_rows(&by_name_db, table, columns))
            });
        if differs {
            found.push(names.join(", "));
        }
    }
    found
}

fn applied(path: &Path) -> BTreeSet<String> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT name FROM migrations").unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn all_migrations() -> BTreeSet<String> {
    MIGRATIONS.iter().map(|m| m.name.to_owned()).collect()
}

fn legacy_state() -> EngineState {
    let full = engine_state();
    let mut state = EngineState::default();
    state.trees.dirty = full.trees.dirty;
    state.trees.unsearched = full.trees.unsearched;
    state.trees.reverted = full.trees.reverted;
    state.trees.parked_as = full.trees.parked_as;
    state.trees.waiting = full.trees.waiting;
    state.trees.watched = full.trees.watched;
    state.trees.watched_positions = full.trees.watched_positions;
    state.trees.trashed_here = full.trees.trashed_here;
    state.trees.discarded = full.trees.discarded;
    state.trees.outside = full.trees.outside;
    state.trees.lost = full.trees.lost;
    state.trees.displaced = full.trees.displaced;
    state.trees.edited_here = full.trees.edited_here;
    state.trees.unsent_edits = full.trees.unsent_edits;
    state.trees.resends = full.trees.resends;
    state.trees.replaced_versions = full.trees.replaced_versions;
    state.commits.accepted = full.commits.accepted;
    state.commits.in_flight = full.commits.in_flight;
    state.commits.problems = full.commits.problems;
    state.commits.halted = full.commits.halted;
    state.commits.cap = full.commits.cap;
    state.commits.split = full.commits.split;
    state.commits.aliases = full.commits.aliases;
    state.commits.restores = full.commits.restores;
    state.commits.resolved = full.commits.resolved;
    state.commits.slots = full.commits.slots;
    state.commits.questions = full.commits.questions;
    state.commits.published = full.commits.published;
    state.commits.guard = full.commits.guard;
    state.effects.in_flight = full.effects.in_flight;
    state.effects.cleared = full.effects.cleared;
    state.effects.failed = full.effects.failed;
    state
}

fn populate(path: &Path) {
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Koidrive"),
        root_dev: 66306,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
    };
    let moved = file_shape("root", "moved.txt");
    Store::open(path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&engine_state())?;
            tx.put_synced_node(&discarded_in_s())?;
            tx.put_synced_node(&SyncedNode {
                node_id: "moved".into(),
                shape: moved.clone(),
                pending_for: String::new(),
            })?;
            tx.put_local_node(&LocalNode {
                node_id: "moved".into(),
                shape: moved.clone(),
                identity: FileId {
                    dev: 1,
                    ino: 7,
                    birth_ns: Some(-1),
                },
                parked: false,
            })?;
            tx.put_remote_node(&NodeState {
                node_id: "moved".into(),
                parent_id: "root".into(),
                name: "moved.txt".into(),
                kind: NodeKind::File as i32,
                status: NodeStatus::Live as i32,
                content_rev: 2,
                meta_rev: 9,
                ..Default::default()
            })?;
            tx.set_cursor(&Cursor {
                history_id: "h1".into(),
                last_seq: 30,
                ahead_of_server: false,
                commit_id: "c30".into(),
                committed_at_ms: 300,
            })?;
            tx.set_link(&link)?;
            tx.put_question(&StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 10,
                body: b"{}".to_vec(),
                answer: None,
            })?;
            tx.put_chunk(&[1; 32], 100, 10)?;
            tx.set_id_generator(&[1, 2, 3])?;
            tx.request_restore(&id(103), &from_trash(1))?;
            Ok::<_, Error>(())
        })
        .unwrap();
}

fn store_before(path: &Path, migration: &str) {
    let position = MIGRATIONS.iter().position(|m| m.name == migration).unwrap();
    if position < LEGACY_STEPS {
        rewind(path, position);
        return;
    }
    let mut skipped = BTreeSet::from([migration]);
    for m in MIGRATIONS {
        if m.requires.iter().any(|r| skipped.contains(r)) {
            skipped.insert(m.name);
        }
    }
    let kept: Vec<_> = MIGRATIONS
        .iter()
        .filter(|m| !skipped.contains(m.name))
        .collect();
    rebuild(path, |conn| {
        for m in &kept {
            conn.execute_batch(m.sql).unwrap();
        }
        conn.execute_batch(
            "CREATE TABLE migrations (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;",
        )
        .unwrap();
        for m in &kept {
            conn.execute("INSERT INTO migrations (name) VALUES (?1)", [m.name])
                .unwrap();
        }
        conn.pragma_update(None, "user_version", NAMED).unwrap();
    });
}

fn rewind(path: &Path, version: usize) {
    rebuild(path, |conn| {
        for m in &MIGRATIONS[..version] {
            conn.execute_batch(m.sql).unwrap();
        }
        conn.pragma_update(None, "user_version", version as u32)
            .unwrap();
    });
}

fn rebuild(path: &Path, migrate: impl FnOnce(&Connection)) {
    let rebuilt = path.with_extension("rebuilt");
    let conn = Connection::open(&rebuilt).unwrap();
    koidrive_store::migrations::register_functions(&conn).unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    migrate(&conn);
    conn.execute("ATTACH ?1 AS original", [path.to_str().unwrap()])
        .unwrap();
    for table in tables(&conn) {
        let original = columns(&conn, "original", &table);
        let shared: Vec<String> = columns(&conn, "main", &table)
            .into_iter()
            .filter(|column| original.contains(column))
            .map(|column| format!("\"{column}\""))
            .collect();
        if shared.is_empty() {
            continue;
        }
        let shared = shared.join(", ");
        conn.execute_batch(&format!(
            "INSERT INTO main.\"{table}\" ({shared}) SELECT {shared} FROM original.\"{table}\""
        ))
        .unwrap();
    }
    if columns(&conn, "main", "effect_state").contains(&"in_flight".to_owned())
        && !columns(&conn, "original", "effects_in_flight").is_empty()
    {
        conn.execute_batch(
            "UPDATE main.effect_state SET in_flight =
                (SELECT record FROM original.effects_in_flight WHERE position = 0)",
        )
        .unwrap();
    }
    conn.execute_batch("DETACH original").unwrap();
    drop(conn);
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
    std::fs::rename(rebuilt, path).unwrap();
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM main.sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != 'migrations' ORDER BY name",
        )
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn columns(conn: &Connection, schema: &str, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1, ?2) ORDER BY name")
        .unwrap();
    stmt.query_map([table, schema], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn table_rows(conn: &Connection, table: &str, columns: &[String]) -> Vec<String> {
    let list: Vec<String> = columns.iter().map(|c| format!("\"{c}\"")).collect();
    let mut stmt = conn
        .prepare(&format!("SELECT {} FROM \"{table}\"", list.join(", ")))
        .unwrap();
    let mut rows: Vec<String> = stmt
        .query_map([], |row| {
            (0..columns.len())
                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                .collect::<Result<Vec<_>, _>>()
                .map(|values| format!("{values:?}"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort();
    rows
}

fn moved_in_flight(
    path: &Path,
    kept: &mut BTreeMap<String, (Vec<String>, Vec<String>)>,
) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let Some((columns, rows)) = kept.get_mut("effect_state") else {
        return Vec::new();
    };
    if !columns.contains(&"in_flight".to_owned()) {
        return Vec::new();
    }
    columns.retain(|column| column != "in_flight");
    *rows = table_rows(&conn, "effect_state", columns);
    conn.execute_batch(
        "CREATE TEMP TABLE moved AS
            SELECT 0 AS position, in_flight AS record FROM effect_state WHERE in_flight IS NOT NULL",
    )
    .unwrap();
    table_rows(&conn, "moved", &["position".into(), "record".into()])
}

fn every_row(path: &Path) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let conn = Connection::open(path).unwrap();
    tables(&conn)
        .into_iter()
        .map(|table| {
            let columns = columns(&conn, "main", &table);
            let rows = table_rows(&conn, &table, &columns);
            (table, (columns, rows))
        })
        .collect()
}

fn schema(path: &Path) -> BTreeMap<String, Vec<String>> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT type || ' ' || name, sql FROM sqlite_schema
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .map(|row| {
        let (object, sql) = row.unwrap();
        let sorted = object.starts_with("table ");
        (object, definitions(&sql, sorted))
    })
    .collect()
}

fn definitions(sql: &str, sorted: bool) -> Vec<String> {
    let mut pieces = vec![];
    let mut piece = String::new();
    let (mut depth, mut quote, mut closed) = (0, None, false);
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(end) = quote {
            piece.push(c);
            if c == end {
                quote = None;
            }
            continue;
        }
        let blank = match c {
            '-' if chars.peek() == Some(&'-') => {
                chars.by_ref().find(|&c| c == '\n');
                true
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                chars
                    .by_ref()
                    .find(|&c| std::mem::replace(&mut last, c) == '*' && c == '/');
                true
            }
            c => c.is_whitespace(),
        };
        if blank {
            if !piece.is_empty() && !piece.ends_with(' ') {
                piece.push(' ');
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                piece.push(c);
            }
            '[' => {
                quote = Some(']');
                piece.push(c);
            }
            '(' if depth == 0 && !closed => {
                pieces.push(std::mem::take(&mut piece));
                depth = 1;
            }
            ',' if depth == 1 && !closed => pieces.push(std::mem::take(&mut piece)),
            ')' if depth == 1 && !closed => {
                pieces.push(std::mem::take(&mut piece));
                closed = true;
            }
            '(' => {
                depth += 1;
                piece.push(c);
            }
            ')' => {
                depth -= 1;
                piece.push(c);
            }
            _ => piece.push(c),
        }
    }
    pieces.push(piece);
    let mut pieces: Vec<String> = pieces.iter().map(|p| p.trim_end().to_owned()).collect();
    if sorted && pieces.len() > 2 {
        let tail = pieces.len() - 1;
        pieces[1..tail].sort();
    }
    pieces
}

#[test]
fn three_trees_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let remote_root = NodeState {
        node_id: "root".into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        ..Default::default()
    };
    let remote_file = NodeState {
        node_id: "f".into(),
        parent_id: "root".into(),
        name: "b.txt".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Trashed as i32,
        content_rev: 4,
        meta_rev: 3,
        meta_edit_rev: 2,
        trash_group: "4:0".into(),
        current: Some(VersionInfo {
            version_id: "v1".into(),
            seq: 4,
            content: Some(content(3)),
            device_id: "d1".into(),
            user_id: "u1".into(),
            created_at_ms: 1_700_000_000_000,
        }),
        edit_rev: 0,
        pending_for: "a.txt".into(),
        pending_by: "d1".into(),
    };
    let synced = [
        SyncedNode {
            node_id: "f".into(),
            shape: file_shape("root", "b.txt"),
            pending_for: String::new(),
        },
        SyncedNode {
            node_id: "root".into(),
            shape: dir_shape("", ""),
            pending_for: String::new(),
        },
    ];
    let local = [
        LocalNode {
            node_id: "f".into(),
            shape: file_shape("root", "b\u{301}.txt"),
            identity: FileId {
                dev: u64::MAX,
                ino: 1 << 63,
                birth_ns: Some(1_700_000_000_123_456_789),
            },
            parked: false,
        },
        LocalNode {
            node_id: "p".into(),
            shape: dir_shape("root", "swap"),
            identity: FileId {
                dev: 16_777_232,
                ino: 42,
                birth_ns: None,
            },
            parked: true,
        },
    ];

    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_remote_node(&remote_file)?;
            tx.put_remote_node(&remote_root)?;
            for node in &synced {
                tx.put_synced_node(node)?;
            }
            for node in &local {
                tx.put_local_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(
                tx.remote_nodes()?,
                [remote_file.clone(), remote_root.clone()]
            );
            assert_eq!(tx.synced_nodes()?, synced);
            assert_eq!(tx.local_nodes()?, local);

            assert!(tx.remove_remote_node("f")?);
            assert!(tx.remove_synced_node("f")?);
            assert!(tx.remove_local_node("p")?);
            assert!(!tx.remove_local_node("p")?);
            assert_eq!(tx.remote_nodes()?, std::slice::from_ref(&remote_root));
            assert_eq!(tx.synced_nodes()?, synced[1..]);
            assert_eq!(tx.local_nodes()?, local[..1]);
            Ok::<_, Error>(())
        })
        .unwrap();
}

fn identity(ino: u128) -> FileIdentity {
    FileIdentity {
        dev: u64::MAX,
        ino,
        birth_ns: ino.is_multiple_of(2).then_some(-1),
    }
}

fn from_commit(slot: u64, shape: Option<EngineShape>) -> Waiting {
    Waiting {
        shape,
        origin: Origin::Commit(slot),
    }
}

fn engine_shape(parent: &str, name: &str) -> EngineShape {
    EngineShape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(3)),
    }
}

fn revisions(content_rev: u64, meta_rev: u64) -> Revisions {
    Revisions {
        content_rev,
        meta_rev,
    }
}

fn deletion(top: &str, trash: Trash) -> RemoteDeletion {
    RemoteDeletion {
        top: top.into(),
        trash,
    }
}

/// The trash of a delete whose tombstones R holds, the first delete of the commit at \`seq\`.
fn group(seq: u64) -> Trash {
    Trash::Group {
        group: format!("{seq}:0"),
        seq,
    }
}

fn unknown(no_later_than: u64) -> Trash {
    Trash::Unknown { no_later_than }
}

fn nodes<const N: usize>(ids: [&str; N]) -> BTreeSet<String> {
    ids.into_iter().map(String::from).collect()
}

/// A node, version or restore ID.
fn id(n: u128) -> String {
    uuid_v7(1, n)
}

fn from_trash(node: u128) -> RestoreIntent {
    RestoreIntent::from_trash(id(node), None, None).unwrap()
}

/// A restore from the trash to a named position, and one of a version.
fn placed_and_version() -> [RestoreIntent; 2] {
    let placed = RestoreIntent::from_trash(id(2), Some(id(3)), Some("back.txt".into()));
    [
        placed.unwrap(),
        RestoreIntent::version(id(4), id(5)).unwrap(),
    ]
}

/// An engine state with every field set, and every kind of effect, location, restore and
/// restore outcome.
fn engine_state() -> EngineState {
    let in_docs = |name: &str| Location::At {
        base: Base::Root,
        dirs: vec![Dir {
            name: "docs".into(),
            identity: identity(2),
        }],
        name: name.into(),
    };
    let in_parked = Location::At {
        base: Base::Parked {
            node: "p".into(),
            identity: identity(3),
        },
        dirs: vec![],
        name: "inner.txt".into(),
    };
    let displaced = Displaced {
        at: in_docs("old.txt"),
        identity: identity(4),
        content: content(5),
    };
    let commit = Commit {
        commit_id: "c1".into(),
        base_seq: 12,
        ops: vec![
            Op {
                op: Some(op::Op::CreateFile(CreateFile {
                    node_id: "n".into(),
                    parent_id: "root".into(),
                    name: "new.txt".into(),
                    content: Some(content(3)),
                })),
            },
            Op {
                op: Some(op::Op::Delete(Delete {
                    node_id: "old".into(),
                    requires: None,
                })),
            },
            Op {
                op: Some(op::Op::Restore(from_trash(1).op().clone())),
            },
        ],
        history_id: "h1".into(),
        device_seq: 5,
        require_head: true,
        ..Default::default()
    };
    let [placed, version] = placed_and_version();
    let outcomes = [
        Outcome::Restored,
        Outcome::Renamed {
            name: "back (conflict).txt".into(),
        },
        Outcome::Copied {
            node: id(6),
            name: "copy.txt".into(),
        },
        Outcome::Rejected(Rejection::NotFound),
        Outcome::Rejected(Rejection::ReadOnly),
        Outcome::Rejected(Rejection::Other("LATER_REASON".into())),
        Outcome::DoesNotFit,
    ];
    let resolved = outcomes
        .into_iter()
        .enumerate()
        .map(|(n, outcome)| Resolved {
            id: id(900 - n as u128),
            intent: [&placed, &version][n % 2].clone(),
            outcome,
        });
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    EngineState {
        trees: TreesState {
            dirty: nodes(["a", "b"]),
            unsearched: nodes(["b"]),
            reverted: nodes(["r"]),
            parked_as: BTreeMap::from([("p".into(), "p-before".into())]),
            waiting: BTreeMap::from([
                (
                    "w".into(),
                    from_commit(4, Some(engine_shape("root", "w.txt"))),
                ),
                ("gone".into(), from_commit(4, None)),
            ]),
            watched: BTreeMap::from([("edited".into(), true), ("kept".into(), false)]),
            watched_positions: BTreeMap::from([
                (
                    "moved".into(),
                    Some(Agreed {
                        rev: 9,
                        parent: "root".into(),
                        name: "moved.txt".into(),
                    }),
                ),
                ("placed".into(), None),
            ]),
            trashed_here: BTreeMap::from([("t".into(), 5)]),
            removed_here: BTreeMap::from([("r".into(), 6)]),
            discarded: BTreeMap::from([("d".into(), engine_shape("root", "d.txt"))]),
            outside: nodes(["kept", "lost"]),
            lost: nodes(["lost"]),
            displaced: BTreeMap::from([(
                "displaced".into(),
                FileIdentity {
                    dev: u64::MAX,
                    ino: 9,
                    birth_ns: Some(-1),
                },
            )]),
            edited_here: BTreeMap::from([("e".into(), 6)]),
            moved_here: BTreeMap::from([
                (
                    "m".into(),
                    MovedHere {
                        seq: 7,
                        upgraded: false,
                    },
                ),
                (
                    "n".into(),
                    MovedHere {
                        seq: 8,
                        upgraded: true,
                    },
                ),
            ]),
            past_positions: BTreeMap::from([(
                "moved".into(),
                vec![
                    Agreed {
                        rev: 6,
                        parent: "root".into(),
                        name: "old.txt".into(),
                    },
                    Agreed {
                        rev: 8,
                        parent: String::new(),
                        name: "before.txt".into(),
                    },
                ],
            )]),
            unsent_edits: nodes(["u"]),
            dropped_edits: nodes(["d"]),
            resends: nodes(["u"]),
            local_changes: nodes(["u"]),
            replaced_versions: BTreeMap::from([(
                "e".into(),
                vec![
                    "0190a6b2-0000-7000-8000-000000000001".into(),
                    "0190a6b2-0000-7000-8000-000000000002".into(),
                ],
            )]),
            kept_originals: BTreeMap::from([("t".into(), "kept".into())]),
            unsent_marks: BTreeMap::from([("m".into(), "M".into())]),
            pending_finishes: BTreeMap::new(),
        },
        commits: CommitsState {
            accepted: 4,
            in_flight: Some(InFlight {
                commit,
                deletes: BTreeMap::from([
                    (
                        "old".into(),
                        vec!["old".into(), "old-child".into(), "old-grandchild".into()],
                    ),
                    ("empty".into(), vec![]),
                ]),
                restores: BTreeMap::new(),
                result: Some(CommitResult {
                    seq: 30,
                    results: vec![applied.clone(), applied.clone(), applied],
                    changes: vec![],
                    ..Default::default()
                }),
            }),
            problems: nodes(["rejected"]),
            halted: Some(Halt::Policy { code: 42 }),
            cap: Some(8),
            split: nodes(["big"]),
            aliases: BTreeMap::from([("twin".into(), "kept".into())]),
            restores: BTreeMap::from([
                (id(100), from_trash(1)),
                (id(101), placed.clone()),
                (id(102), version.clone()),
            ]),
            resolved: resolved.collect(),
            slots: BTreeMap::from([(3, 7), (4, 9)]),
            questions: vec![
                Question {
                    id: "q1".into(),
                    asked_at_ms: 7,
                    kind: QuestionKind::MassDelete {
                        deletions: 1001,
                        affected_files: 1002,
                        space_files: 5000,
                        exceeded: vec![Limit::Deletions, Limit::Share],
                    },
                    held: BTreeMap::from([(
                        "held".into(),
                        Held {
                            members: BTreeMap::from([
                                ("held".into(), Some(revisions(3, 4))),
                                ("held-child".into(), Some(revisions(5, 6))),
                                ("held-file".into(), Some(revisions(7, 8))),
                            ]),
                            files: 2,
                        },
                    )]),
                    answer: Some(Answer::Restore),
                    shown: BTreeMap::new(),
                },
                Question {
                    id: "q2".into(),
                    asked_at_ms: 8,
                    kind: QuestionKind::Recovery {
                        group: RecoveryGroup::CreatedOnThisDevice,
                        sent_in: None,
                        cause: None,
                    },
                    held: BTreeMap::from([(
                        "local".into(),
                        Held {
                            members: BTreeMap::from([("local".into(), None)]),
                            files: 1,
                        },
                    )]),
                    answer: None,
                    shown: BTreeMap::from([(
                        "local".into(),
                        EngineShape {
                            parent: "adopted".into(),
                            name: "draft.txt".into(),
                            kind: Kind::File,
                            content: Some(content(9)),
                        },
                    )]),
                },
                Question {
                    id: "q3".into(),
                    asked_at_ms: 9,
                    kind: QuestionKind::RemoteMassDelete {
                        deletions: 2,
                        affected_files: 30,
                        space_files: 40,
                        exceeded: vec![Limit::Share],
                    },
                    held: BTreeMap::new(),
                    answer: None,
                    shown: BTreeMap::new(),
                },
            ],
            published: nodes(["published"]),
            guard: GuardState {
                ledger: BTreeMap::from([
                    ("held".into(), Counted { at_ms: 5, files: 2 }),
                    ("kept".into(), Counted { at_ms: 6, files: 1 }),
                ]),
                confirmed: BTreeMap::from([(
                    "yes".into(),
                    Held {
                        members: BTreeMap::from([("yes".into(), Some(revisions(1, 2)))]),
                        files: 1,
                    },
                )]),
                confirmed_limits: BTreeSet::from([Limit::Share]),
                remote: BTreeMap::from([
                    (
                        deletion("gone", group(4)),
                        Counted {
                            at_ms: 7,
                            files: 29,
                        },
                    ),
                    (
                        deletion("gone-file", unknown(6)),
                        Counted { at_ms: 8, files: 1 },
                    ),
                ]),
                dismissed: BTreeSet::from([
                    deletion("seen", group(3)),
                    deletion("forgotten", unknown(5)),
                ]),
            },
        },
        effects: EffectsState {
            in_flight: vec![EffectRecord {
                effect_id: "e1".into(),
                node: "n".into(),
                counter: 3,
                effect: Effect::WriteFile {
                    at: in_parked,
                    content: content(7),
                    replaces: Some(displaced.clone()),
                },
                after: Some(engine_shape("p", "inner.txt")),
            }],
            cleared: 2,
            failed: BTreeMap::from([
                (
                    "c".into(),
                    Effect::CreateDir {
                        at: Location::Parked("c".into()),
                    },
                ),
                (
                    "m".into(),
                    Effect::Move {
                        from: in_docs("a"),
                        identity: identity(6),
                        to: Location::Parked("m".into()),
                    },
                ),
                ("f".into(), Effect::RemoveFile { displaced }),
                (
                    "d".into(),
                    Effect::RemoveDir {
                        at: in_docs("empty"),
                        identity: identity(8),
                    },
                ),
            ]),
        },
    }
}

#[test]
fn engine_state_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let state = engine_state();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.engine_state()).unwrap(),
        EngineState::default()
    );
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    store
        .transaction(|tx| {
            tx.set_engine_state(&EngineState::default())?;
            assert_eq!(tx.engine_state()?, EngineState::default());
            Ok::<_, Error>(())
        })
        .unwrap();
}

/// The tables of the engine state besides the nodes of R, L and S.
const ENGINE_TABLES: [&str; 38] = [
    "node_sets",
    "node_maps",
    "waiting_updates",
    "commit_state",
    "commit_in_flight",
    "commit_in_flight_deletes",
    "commit_in_flight_delete_members",
    "commit_in_flight_restore_members",
    "watched_contents",
    "trashed_here",
    "recorded_nodes",
    "commit_slots",
    "edited_here",
    "replaced_versions",
    "resends",
    "local_changes",
    "kept_originals",
    "effect_state",
    "effects_in_flight",
    "failed_effects",
    "restore_intents",
    "open_questions",
    "open_question_items",
    "open_question_members",
    "guard_ledger",
    "guard_confirmed",
    "guard_confirmed_members",
    "guard_remote",
    "guard_remote_dismissed",
    "guard_confirmed_limits",
    "watched_positions",
    "unsent_edits",
    "moved_here",
    "past_positions",
    "dropped_edits",
    "removed_here",
    "unsent_marks",
    "pending_finishes",
];

/// Counts each row inserted, updated or deleted in the engine's tables from now on in the table
/// `writes`. The triggers are stored in the database, so they count the writes of every connection.
fn count_engine_writes(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE writes (n INTEGER NOT NULL); INSERT INTO writes VALUES (0);")
        .unwrap();
    for table in ENGINE_TABLES {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "CREATE TRIGGER {table}_{event} AFTER {event} ON {table}
                 BEGIN UPDATE writes SET n = n + 1; END;"
            ))
            .unwrap();
        }
    }
}

fn engine_writes(path: &Path) -> u64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("SELECT n FROM writes", [], |row| row.get(0))
        .unwrap()
}

/// [`engine_state`] with `n` more nodes in each node set, node map, the waiting updates and the
/// failed effects.
fn engine_state_of(n: usize) -> EngineState {
    let mut state = engine_state();
    for i in 0..n {
        let node = format!("node-{i}");
        state.trees.dirty.insert(node.clone());
        state.trees.unsearched.insert(node.clone());
        state.trees.reverted.insert(node.clone());
        state
            .trees
            .parked_as
            .insert(node.clone(), format!("parked-{i}"));
        let shape = engine_shape("p", &format!("{i}.txt"));
        state
            .trees
            .waiting
            .insert(node.clone(), from_commit(4, Some(shape)));
        state.commits.problems.insert(node.clone());
        state.commits.split.insert(node.clone());
        state
            .commits
            .aliases
            .insert(node.clone(), format!("alias-{i}"));
        let effect = Effect::CreateDir {
            at: Location::Parked(node.clone()),
        };
        state.effects.failed.insert(node, effect);
    }
    state
}

/// A removal that applies another device's deletion changes the count of one deletion, and only
/// that row is written, however many deletions the guard holds.
#[test]
fn a_counted_remote_removal_writes_its_row_alone() {
    let mut writes = Vec::new();
    for n in [100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut before = engine_state();
        let guard = &mut before.commits.guard;
        for i in 0..n {
            let counted = Counted { at_ms: 1, files: 1 };
            let top = format!("top-{i}");
            guard.remote.insert(deletion(&top, group(i)), counted);
            let old = format!("old-{i}");
            guard.dismissed.insert(deletion(&old, unknown(i)));
        }
        let mut after = before.clone();
        let counted = after
            .commits
            .guard
            .remote
            .get_mut(&deletion("top-0", group(0)));
        counted.unwrap().files += 1;
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&before))
            .unwrap();
        count_engine_writes(&path);

        let mut store = Store::open(&path).unwrap();
        store
            .transaction(|tx| tx.update_engine_state(&before, &after))
            .unwrap();
        writes.push(engine_writes(&path));
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
    }
    assert_eq!(writes, [1, 1]);
}

#[test]
fn an_engine_state_update_writes_rows_for_the_change_alone() {
    let mut writes = Vec::new();
    for n in [100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let before = engine_state_of(n);
        let mut after = before.clone();
        after.trees.dirty.remove("node-0");
        after.trees.dirty.insert("node-new".into());
        after
            .trees
            .parked_as
            .insert("node-1".into(), "parked-new".into());
        after.trees.waiting.remove("node-2");
        after
            .trees
            .waiting
            .insert("node-3".into(), from_commit(4, None));
        after.commits.problems.insert("node-new".into());
        after.commits.accepted += 1;
        after.commits.in_flight = None;
        after.commits.resolved.pop();
        after.effects.cleared += 1;
        after.effects.in_flight = Vec::new();
        after.effects.failed.remove("node-4");
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&before))
            .unwrap();
        count_engine_writes(&path);

        let mut store = Store::open(&path).unwrap();
        store
            .transaction(|tx| tx.update_engine_state(&before, &after))
            .unwrap();
        writes.push(engine_writes(&path));
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
        store
            .transaction(|tx| tx.update_engine_state(&after, &after))
            .unwrap();
        assert_eq!(
            engine_writes(&path),
            writes[writes.len() - 1],
            "an unchanged state writes no row"
        );
    }
    assert!(writes[0] > 0);
    assert_eq!(writes[0], writes[1], "rows written for 100 and 1000 nodes");
}

/// Each part of the state reaches the store through an update: from a store that never held one,
/// through changes of each part, back to the default state.
#[test]
fn engine_state_updates_store_every_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let full = engine_state();
    let mut changed = full.clone();
    changed.commits.halted = Some(Halt::Policy { code: 3 });
    changed.commits.cap = Some(2);
    changed.commits.in_flight.as_mut().unwrap().result = None;
    changed.commits.restores.clear();
    changed.effects.in_flight = Vec::new();
    changed.trees.waiting.clear();
    let guard = &mut changed.commits.guard;
    guard.confirmed_limits.insert(Limit::Deletions);
    guard.remote.remove(&deletion("gone-file", unknown(6)));
    guard
        .remote
        .values_mut()
        .for_each(|counted| counted.files += 1);
    let counted = Counted { at_ms: 9, files: 3 };
    guard.remote.insert(deletion("new", unknown(9)), counted);
    guard.dismissed.remove(&deletion("seen", group(3)));
    guard.dismissed.insert(deletion("later", group(8)));
    let states = [full.clone(), changed, full, EngineState::default()];
    let mut stored = EngineState::default();
    store
        .transaction(|tx| {
            for state in &states {
                tx.update_engine_state(&stored, state)?;
                assert_eq!(&tx.engine_state()?, state);
                stored = state.clone();
            }
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_halt_survives_a_reopen_with_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    for halted in [
        None,
        Some(Halt::ReadOnly),
        Some(Halt::Recovery(RecoveryCause::rewind(4, 1_700_000_000_000))),
        Some(Halt::Recovery(RecoveryCause::of(
            RecoveryTrigger::StoreOlderThanDisk,
        ))),
        Some(Halt::Policy { code: 7 }),
    ] {
        let mut state = engine_state();
        state.commits.halted = halted;
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();
        let stored = Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.engine_state())
            .unwrap();
        assert_eq!(stored.commits.halted, halted);
        assert_eq!(stored.commits.in_flight, state.commits.in_flight);
    }
}

#[test]
fn a_policy_halt_outlasts_a_restart_and_resends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let halt = Halt::Policy { code: 42 };
    let state = EngineState {
        commits: CommitsState {
            accepted: 4,
            in_flight: Some(InFlight {
                commit: Commit {
                    commit_id: "c1".into(),
                    history_id: "h1".into(),
                    device_seq: 5,
                    ..Default::default()
                },
                deletes: BTreeMap::new(),
                restores: BTreeMap::new(),
                result: None,
            }),
            halted: Some(halt),
            ..Default::default()
        },
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();

    let stored = Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.engine_state())
        .unwrap();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    assert_eq!(commits.halted(), Some(halt));
    for device_seq in [4, 5] {
        let welcome = Welcome {
            history_id: "h1".into(),
            device_seq,
            read_only: false,
            ..Default::default()
        };
        assert_eq!(commits.connected(&welcome), Reconnect::Idle);
        assert_eq!(commits.halted(), Some(halt));
    }
}

#[test]
fn a_restore_keeps_what_was_asked_for_while_the_store_holds_the_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let [placed, version] = placed_and_version();
    let asked = |path: &str, version_saved_at_ms| AskedRestore {
        path: Some(path.into()),
        version_saved_at_ms,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.request_restore(&id(100), &placed)?;
            tx.note_asked_restore(&id(100), &asked("docs/back.txt", None))?;
            tx.request_restore(&id(101), &version)?;
            tx.note_asked_restore(&id(101), &asked("docs/report.docx", Some(2_000)))
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            let both = BTreeMap::from([
                (id(100), asked("docs/back.txt", None)),
                (id(101), asked("docs/report.docx", Some(2_000))),
            ]);
            assert_eq!(tx.asked_restores()?, both);
            tx.take_restore_requests()?;
            assert_eq!(tx.asked_restores()?, both);
            tx.request_restore(&id(102), &version)?;
            tx.note_asked_restore(&id(102), &asked("notes.txt", Some(3_000)))?;
            assert_eq!(
                tx.asked_restores()?,
                BTreeMap::from([(id(102), asked("notes.txt", Some(3_000)))])
            );
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_restore_request_waits_until_it_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let [placed, version] = placed_and_version();
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            tx.request_restore(&id(101), &version)?;
            tx.request_restore(&id(100), &placed)
        })
        .unwrap();
    let lost = store.transaction(|tx| {
        tx.request_restore(&id(102), &from_trash(1))?;
        Err::<(), _>(Error::InvalidEngineState("the process stopped here".into()))
    });
    assert!(lost.is_err());
    drop(store);

    let requests = vec![(id(100), placed), (id(101), version)];
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.restore_requests()?, requests, "in request order");
            // Saving the engine state leaves the requests the engine has not taken.
            tx.set_engine_state(&engine_state())?;
            assert_eq!(tx.restore_requests()?, requests);
            assert_eq!(tx.engine_state()?, engine_state());

            assert_eq!(tx.take_restore_requests()?, requests);
            assert_eq!(tx.restore_requests()?, vec![]);
            assert_eq!(tx.take_restore_requests()?, vec![]);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn the_engines_restores_are_read_without_the_rest_of_its_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let state = engine_state();
    store
        .transaction(|tx| {
            assert_eq!(tx.restores()?, (BTreeMap::new(), vec![]));
            tx.set_engine_state(&state)?;
            let (queued, resolved) = tx.restores()?;
            assert_eq!(queued, state.commits.restores);
            assert_eq!(resolved, state.commits.resolved);
            assert!(!state.commits.problems.is_empty());
            assert_eq!(tx.problem_nodes()?, state.commits.problems);
            Ok::<_, Error>(())
        })
        .unwrap();
}

/// A database of the schema before the cursor records a server that is behind keeps its cursor,
/// which records none.
#[test]
fn a_cursor_stored_before_it_could_record_a_server_behind_it_records_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
        ahead_of_server: true,
        commit_id: "c7".into(),
        committed_at_ms: 70,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_cursor(&cursor))
        .unwrap();
    store_before(&path, "05_ahead_of_server");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let stored = store.transaction(|tx| tx.cursor()).unwrap();
    assert_eq!(
        stored,
        Some(Cursor {
            ahead_of_server: false,
            commit_id: String::new(),
            committed_at_ms: 0,
            ..cursor
        })
    );
}

/// A database of the schema before S's content is watched for the commit in flight keeps its
/// engine state, with nothing watched.
#[test]
fn a_store_from_before_contents_were_watched_opens_with_nothing_watched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "06_watched_contents");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.watched.clear();
    before.trees.trashed_here.clear();
    before.trees.discarded.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    before.commits.slots.clear();
    before.commits.questions.clear();
    before.commits.guard = GuardState::default();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before the edits of this device are recorded keeps its engine state,
/// with no edit recorded.
#[test]
fn a_store_from_before_edits_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.commits.questions.pop();
    before.commits.guard.remote.clear();
    before.commits.guard.dismissed.clear();
    before.commits.guard.confirmed_limits.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "11_edited_here");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before the deletes and commit slots of this device are recorded keeps
/// its engine state, with neither recorded.
#[test]
fn a_store_from_before_deletes_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "07_trashed_here_and_commit_slots");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.trashed_here.clear();
    before.trees.discarded.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    before.commits.slots.clear();
    before.commits.questions.clear();
    before.commits.guard = GuardState::default();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before the mass-delete guard keeps its engine state, with no open
/// question and nothing in the guard's window.
#[test]
fn a_store_from_before_the_guard_opens_with_an_empty_guard() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "08_open_questions_and_guard");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.discarded.clear();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    before.commits.questions.clear();
    before.commits.guard = GuardState::default();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before questions kept the local shapes they showed keeps its open
/// questions, each showing none, and records no discarded node.
#[test]
fn a_store_from_before_questions_showed_local_shapes_keeps_its_questions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.commits.questions.pop();
    before.commits.guard.remote.clear();
    before.commits.guard.dismissed.clear();
    before.commits.guard.confirmed_limits.clear();
    for question in &mut before.commits.questions {
        question.shown.clear();
    }
    before.trees.discarded.clear();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "09_open_question_shapes_and_discarded_nodes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before the nodes moved into an ignored folder are stored keeps its engine state, with
/// none recorded, and then stores and loads the ones the engine records.
#[test]
fn a_store_from_before_nodes_in_ignored_folders_were_stored_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.commits.questions.pop();
    before.commits.guard.remote.clear();
    before.commits.guard.dismissed.clear();
    before.commits.guard.confirmed_limits.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "10_recorded_nodes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    assert!(!after.trees.outside.is_empty() && !after.trees.displaced.is_empty());
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

/// A database of the schema before the guard counted other devices' deletions keeps its engine
/// state, with none counted.
#[test]
fn a_store_from_before_remote_removals_were_counted_opens_with_none_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.commits.questions.pop();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "12_guard_remote");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.watched_positions.clear();
    before.commits.guard.remote.clear();
    before.commits.guard.dismissed.clear();
    before.commits.guard.confirmed_limits.clear();
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before S's position is watched for the commit in flight keeps its
/// engine state, with no position watched.
#[test]
fn a_store_from_before_positions_were_watched_opens_with_none_watched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "13_watched_positions");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.watched_positions.clear();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// Each waiting S update keeps where it came from: a commit's slot, or no commit.
#[test]
fn waiting_updates_keep_their_origin() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let mut state = engine_state();
    state.trees.waiting = BTreeMap::from([
        (
            "kept".into(),
            from_commit(3, Some(engine_shape("root", "k.txt"))),
        ),
        ("removed".into(), from_commit(1, None)),
        (
            "other".into(),
            Waiting {
                shape: Some(engine_shape("root", "o.txt")),
                origin: Origin::Other,
            },
        ),
    ]);
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}

/// A database of the schema before waiting updates kept their origin gives each the slot of the
/// newest acknowledged commit, except the update of a node the person discarded on a first link,
/// which no commit made.
#[test]
fn a_store_from_before_waiting_updates_kept_their_origin_takes_the_accepted_slot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.waiting = BTreeMap::from([
        (
            "w".into(),
            from_commit(4, Some(engine_shape("root", "w.txt"))),
        ),
        (
            "d".into(),
            Waiting {
                shape: Some(engine_shape("root", "d.txt")),
                origin: Origin::Other,
            },
        ),
    ]);
    assert!(before.trees.discarded.contains_key("d"));
    assert_eq!(before.commits.accepted, 4);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&SyncedNode {
                node_id: "moved".into(),
                shape: file_shape("root", "moved.txt"),
                pending_for: String::new(),
            })?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "14_waiting_update_slots");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

fn discarded_in_s() -> SyncedNode {
    SyncedNode {
        node_id: "d".into(),
        shape: file_shape("root", "d.txt"),
        pending_for: String::new(),
    }
}

/// A database of the schema before commit tokens keeps its cursor, with no token at its seq, so its next Resume
/// claims none (RFC 0013).
#[test]
fn a_cursor_stored_before_commit_tokens_opens_with_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
        ahead_of_server: false,
        commit_id: "c7".into(),
        committed_at_ms: 70,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_cursor(&cursor))
        .unwrap();
    store_before(&path, "16_commit_tokens");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(
        store.transaction(|tx| tx.cursor()).unwrap(),
        Some(Cursor {
            commit_id: String::new(),
            committed_at_ms: 0,
            ..cursor
        })
    );
}

/// A database of schema 14, which kept only the revision of a watched position, takes the position
/// from S, which held it whenever a revision was recorded. A revision whose node S does not hold
/// is dropped, and the commit in flight takes what it sent for that node.
#[test]
fn a_store_from_before_watched_positions_were_kept_takes_them_from_s() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.insert(
        "lost".into(),
        Some(Agreed {
            rev: 4,
            parent: "root".into(),
            name: "lost.txt".into(),
        }),
    );
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.put_synced_node(&SyncedNode {
                node_id: "moved".into(),
                shape: file_shape("root", "moved.txt"),
                pending_for: String::new(),
            })?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    drop(store);
    store_before(&path, "15_watched_position_shapes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    before.trees.watched_positions.insert("lost".into(), None);
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

#[test]
fn a_store_from_before_lost_folders_were_marked_opens_with_none_marked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "22_recorded_nodes_lost");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.lost.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    assert!(!after.trees.lost.is_empty());
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

/// A database of the schema before a first link's published entries were recorded keeps its
/// engine state, with none recorded, and records them from then on.
#[test]
fn a_store_from_before_published_entries_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "17_published_nodes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    assert!(!after.commits.published.is_empty());
    before.trees.lost.clear();
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_unsent_edits_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "19_unsent_edits");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.unsent_edits.clear();
    before.trees.lost.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    assert!(!after.trees.unsent_edits.is_empty());
    before.trees.unsent_marks.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_moves_were_recorded_records_the_positions_its_commits_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    let node = |id: &str, name: &str, meta_rev: u64, status: NodeStatus| NodeState {
        node_id: id.into(),
        parent_id: "root".into(),
        name: name.into(),
        kind: NodeKind::File as i32,
        status: status as i32,
        content_rev: 1,
        meta_rev,
        current: Some(VersionInfo {
            content: Some(content(3)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let remote = [
        node("own", "own.txt", 7, NodeStatus::Live),
        node("other", "other.txt", 8, NodeStatus::Live),
        node("moved", "moved.txt", 9, NodeStatus::Live),
        node("gone", "gone.txt", 9, NodeStatus::Trashed),
        node("nfd", "\u{d55c}.txt", 7, NodeStatus::Live),
    ];
    let synced = [
        ("own", "own.txt"),
        ("other", "other.txt"),
        ("moved", "since.txt"),
        ("nfd", "\u{1112}\u{1161}\u{11ab}.txt"),
        ("gone", "gone.txt"),
    ]
    .map(|(id, name)| SyncedNode {
        node_id: id.into(),
        shape: file_shape("root", name),
        pending_for: String::new(),
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&before)?;
            for node in &remote {
                tx.put_remote_node(node)?;
            }
            for node in &synced {
                tx.put_synced_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "moved_here");

    let mut store = Store::open(&path).unwrap();
    let after = before.clone();
    let upgraded = |seq| MovedHere {
        seq,
        upgraded: true,
    };
    before.trees.moved_here = BTreeMap::from([
        ("own".into(), upgraded(7)),
        ("moved".into(), upgraded(9)),
        ("nfd".into(), upgraded(7)),
    ]);
    before.trees.past_positions.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn an_own_move_acknowledged_before_the_upgrade_is_sent_again_after_a_later_loss() {
    let remote = |name: &str, meta_rev| RemoteNode {
        shape: engine_shape("root", name),
        trashed: false,
        content_rev: 1,
        meta_rev,
        meta_edit_rev: meta_rev,
        edit_rev: 0,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
    };
    let local = |name: &str| EngineLocalNode {
        shape: engine_shape("root", name),
        identity: FileIdentity {
            dev: 1,
            ino: 1,
            birth_ns: Some(0),
        },
        parked: false,
    };
    let mut trees = Trees::new("root");
    trees.set_remote("n".into(), remote("x", 1));
    trees.set_local("n".into(), local("x"));
    trees.set_synced("n".into(), Some(engine_shape("root", "x")));
    trees.set_local("n".into(), local("y"));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = commits.build(&mut trees, &mut ids, 1_794_268_740_000, "history", "me", 1);
    assert!(sent.is_some_and(|commit| matches!(
        &commit.ops[..],
        [Op {
            op: Some(op::Op::Move(_)),
            ..
        }]
    )));
    trees.set_remote("n".into(), remote("y", 2));
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    commits
        .received(CommitResult {
            seq: 2,
            results: vec![applied],
            ..Default::default()
        })
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let state = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    let n = NodeState {
        node_id: "n".into(),
        parent_id: "root".into(),
        name: "y".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 2,
        current: Some(VersionInfo {
            content: Some(content(3)),
            ..Default::default()
        }),
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&state)?;
            tx.put_remote_node(&n)?;
            tx.put_synced_node(&SyncedNode {
                node_id: "n".into(),
                shape: file_shape("root", "y"),
                pending_for: String::new(),
            })
        })
        .unwrap();
    store_before(&path, "moved_here");

    let state = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    trees.restore(state.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(state.commits).unwrap();
    let welcome = Welcome {
        history_id: "history".into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    let moved = trees.moved_here();
    trees.set_remote("n".into(), remote("x", 1));
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        moved,
        Vec::new(),
        Default::default(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let resent = commits.build(&mut trees, &mut ids, 1_794_268_740_001, "history", "me", 1);
    let moved_to_y = |op: &Op| matches!(&op.op, Some(op::Op::Move(moved)) if moved.new_name == "y");
    assert!(resent.is_some_and(|commit| commit.ops.iter().any(moved_to_y)));
}

#[test]
fn a_store_from_before_inodes_took_128_bits_keeps_every_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = legacy_state();
    state.trees.replaced_versions.clear();
    state.trees.resends.clear();
    let local =
        [("a", u64::MAX), ("b", 1 << 63), ("c", 42), ("d", 0)].map(|(node_id, ino)| LocalNode {
            node_id: node_id.into(),
            shape: file_shape("root", node_id),
            identity: FileId {
                dev: u64::MAX,
                ino: ino.into(),
                birth_ns: Some(-1),
            },
            parked: false,
        });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&state)?;
            for node in &local {
                tx.put_local_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "20_ino_high");
    let conn = Connection::open(&path).unwrap();
    let stored: i64 = conn
        .query_row(
            "SELECT ino FROM local_nodes WHERE node_id = 'a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, -1, "the low 64 bits keep their encoding");
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    store
        .transaction(|tx| {
            assert_eq!(tx.local_nodes()?, local);
            let mut state = state.clone();
            state.trees.lost.clear();
            state.trees.local_changes = nodes(["a", "b", "c", "d"]);
            state.trees.unsent_marks.clear();
            assert_eq!(tx.engine_state()?, state);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_store_that_recorded_pending_names_opens_owing_each_rename_r_and_s_still_hold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let live = |node_id: &str, name: &str| NodeState {
        node_id: node_id.into(),
        parent_id: "root".into(),
        name: name.into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 2,
        meta_rev: 2,
        ..Default::default()
    };
    let trashed = |node_id: &str, name: &str| NodeState {
        status: NodeStatus::Trashed as i32,
        trash_group: "t".into(),
        ..live(node_id, name)
    };
    let synced = |node_id: &str, name: &str| SyncedNode {
        node_id: node_id.into(),
        shape: dir_shape("root", name),
        pending_for: String::new(),
    };
    let remote = [
        live("d", "A (pending)"),
        live("e", "B"),
        trashed("g", "C (pending)"),
    ];
    let held = [
        synced("d", "A (pending)"),
        synced("e", "B (pending)"),
        synced("g", "C (pending)"),
    ];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for node in &remote {
                tx.put_remote_node(node)?;
            }
            for node in &held {
                tx.put_synced_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO pending_names VALUES
             ('d', 'A (pending)', 'A', 1), ('e', 'B (pending)', 'B', 1),
             ('g', 'C (pending)', 'C', 1), ('h', 'D (pending)', 'D', 0);",
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let recorded: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'pending_names'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 0);
    assert_eq!(store.transaction(|tx| tx.remote_nodes()).unwrap(), remote);
    assert_eq!(store.transaction(|tx| tx.synced_nodes()).unwrap(), held);
    let mut state = EngineState::default();
    state.trees.pending_finishes.insert(
        "d".into(),
        PendingFinish {
            parent: "root".into(),
            pending: "A (pending)".into(),
            name: "A".into(),
            origin: FinishOrigin::Legacy,
            disk_first: false,
            base: None,
            sending: false,
        },
    );
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    state.trees.unsent_marks.insert("f".into(), "C".into());
    state.commits.in_flight = Some(InFlight {
        restores: Default::default(),
        commit: Commit::default(),
        deletes: BTreeMap::new(),
        result: None,
    });
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    state.trees.pending_finishes.clear();
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}

#[test]
fn a_rename_a_store_recorded_before_pending_marks_is_finished_after_the_upgrade_though_another_node_holds_its_name()
 {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::RemoteNode;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let at = |name: &str| EngineShape {
        parent: "root".into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    };
    let held = [("d", "A (pending)"), ("x", "A")];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for (node_id, name) in held {
                tx.put_remote_node(&NodeState {
                    node_id: node_id.into(),
                    parent_id: "root".into(),
                    name: name.into(),
                    kind: NodeKind::Dir as i32,
                    status: NodeStatus::Live as i32,
                    content_rev: 2,
                    meta_rev: 2,
                    ..Default::default()
                })?;
                tx.put_synced_node(&SyncedNode {
                    node_id: node_id.into(),
                    shape: dir_shape("root", name),
                    pending_for: String::new(),
                })?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("INSERT INTO pending_names VALUES ('d', 'A (pending)', 'A', 1);")
        .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    let (mut trees, mut commits) = store
        .transaction(|tx| {
            let mut trees = Trees::new("root");
            for node in tx.remote_nodes()? {
                let (id, remote) = RemoteNode::from_state(&node).unwrap();
                trees.set_remote(id, remote);
            }
            for node in tx.synced_nodes()? {
                trees.set_synced(node.node_id.clone(), Some(at(&node.shape.name)));
                trees.set_synced_mark(
                    node.node_id,
                    Some(node.pending_for).filter(|mark| !mark.is_empty()),
                );
            }
            for (ino, (node_id, name)) in (1..).zip(held) {
                trees.set_local(
                    node_id.into(),
                    koidrive_engine::tree::LocalNode {
                        shape: at(name),
                        identity: FileIdentity {
                            dev: 1,
                            ino,
                            birth_ns: Some(0),
                        },
                        parked: false,
                    },
                );
            }
            let state = tx.engine_state()?;
            trees.restore(state.trees).unwrap();
            trees.rules_scanned();
            trees.settle();
            Ok::<_, Error>((trees, Commits::from_state(state.commits).unwrap()))
        })
        .unwrap();
    for listed in store.transaction(|tx| tx.remote_nodes()).unwrap() {
        let (id, remote) = RemoteNode::from_state(&listed).unwrap();
        trees.set_remote(id, remote);
    }
    trees.settle();
    let sent = commits
        .build(
            &mut trees,
            &mut IdGen::new(1),
            1_794_268_740_000,
            "history",
            "d1",
            7,
        )
        .expect("the recorded rename is sent");
    assert!(
        matches!(
            sent.ops.as_slice(),
            [Op { op: Some(op::Op::Move(moved)) }] if moved.node_id == "d" && moved.new_name == "A"
        ),
        "{:?}",
        sent.ops
    );
}

#[test]
fn a_store_that_recorded_a_pending_name_its_commit_in_flight_sends_opens_owing_the_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let create = Op {
        op: Some(op::Op::CreateDir(koidrive_proto::v1::CreateDir {
            node_id: "d".into(),
            parent_id: "root".into(),
            name: "A (pending)".into(),
            ..Default::default()
        })),
    };
    let mut state = EngineState::default();
    state.commits.in_flight = Some(InFlight {
        restores: Default::default(),
        commit: Commit {
            commit_id: "c1".into(),
            device_seq: 1,
            ops: vec![create],
            ..Default::default()
        },
        deletes: BTreeMap::new(),
        result: None,
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&state)?;
            tx.put_local_node(&LocalNode {
                node_id: "d".into(),
                shape: dir_shape("root", "A (pending)"),
                identity: FileId {
                    dev: 1,
                    ino: 7u64.into(),
                    birth_ns: None,
                },
                parked: false,
            })
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("INSERT INTO pending_names VALUES ('d', 'A (pending)', 'A', 0);")
        .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    state.trees.pending_finishes.insert(
        "d".into(),
        PendingFinish {
            parent: "root".into(),
            pending: "A (pending)".into(),
            name: "A".into(),
            origin: FinishOrigin::Legacy,
            disk_first: false,
            base: None,
            sending: false,
        },
    );
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}

#[test]
fn an_inode_wider_than_64_bits_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let wide = |ino: u128| FileIdentity {
        dev: 1,
        ino,
        birth_ns: None,
    };
    let mut state = engine_state();
    state.trees.displaced.insert("wide".into(), wide(u128::MAX));
    state.effects.failed.insert(
        "w".into(),
        Effect::Move {
            from: Location::At {
                base: Base::Parked {
                    node: "p".into(),
                    identity: wide(1 << 64),
                },
                dirs: vec![Dir {
                    name: "sub".into(),
                    identity: wide((1 << 127) | 1),
                }],
                name: "a".into(),
            },
            identity: wide(u128::MAX - 1),
            to: Location::Parked("w".into()),
        },
    );
    let local = [("a", u128::MAX), ("b", 1 << 64)].map(|(node_id, ino)| LocalNode {
        node_id: node_id.into(),
        shape: file_shape("root", node_id),
        identity: FileId {
            dev: 1,
            ino,
            birth_ns: None,
        },
        parked: false,
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&state)?;
            for node in &local {
                tx.put_local_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.local_nodes()?, local);
            assert_eq!(tx.engine_state()?, state);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_recovery_stored_before_its_cause_was_opens_as_a_rolled_back_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = engine_state();
    state.commits.halted = Some(Halt::Recovery(RecoveryCause::rewind(4, 1_700_000_000_000)));
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "recovery_cause");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    state.commits.halted = Some(Halt::Recovery(RecoveryCause::of(
        RecoveryTrigger::StoreRolledBack,
    )));
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}

#[test]
fn a_store_from_before_replaced_versions_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "23_replaced_versions");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    assert!(!after.trees.replaced_versions.is_empty());
    assert!(!after.trees.resends.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_discarded_shapes_were_recorded_takes_them_from_the_waiting_update_or_s() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    let in_s = engine_shape("root", "d.txt");
    let waiting = engine_shape("root", "w.txt");
    before.trees.discarded = BTreeMap::from([
        ("d".into(), in_s.clone()),
        ("w".into(), waiting.clone()),
        ("l".into(), engine_shape("root", "l.txt")),
        ("gone".into(), engine_shape("root", "gone.txt")),
        ("lost".into(), engine_shape("root", "lost.txt")),
    ]);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.put_synced_node(&SyncedNode {
                node_id: "w".into(),
                shape: file_shape("root", "w-before.txt"),
                pending_for: String::new(),
            })?;
            for (node_id, name) in [("d", "d-edited.txt"), ("l", "l.txt")] {
                tx.put_local_node(&LocalNode {
                    node_id: node_id.into(),
                    shape: file_shape("root", name),
                    identity: FileId {
                        dev: 1,
                        ino: 1,
                        birth_ns: Some(0),
                    },
                    parked: false,
                })?;
            }
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "discarded_shapes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let mut after = store.transaction(|tx| tx.engine_state()).unwrap();
    let asked = after.commits.questions.pop().unwrap();
    assert_eq!(
        (asked.kind, asked.answer),
        (
            QuestionKind::Recovery {
                group: RecoveryGroup::CreatedOnThisDevice,
                sent_in: None,
                cause: None,
            },
            None
        )
    );
    assert_eq!(
        asked.held,
        BTreeMap::from([(
            "l".into(),
            Held {
                members: BTreeMap::from([("l".into(), None)]),
                files: 1,
            }
        )])
    );
    assert_eq!(
        asked.shown,
        BTreeMap::from([("l".into(), engine_shape("root", "l.txt"))])
    );
    before.trees.discarded = BTreeMap::from([("d".into(), in_s), ("w".into(), waiting)]);
    assert_eq!(after, before);
}

#[test]
fn a_file_discarded_before_shapes_were_recorded_that_s_no_longer_holds_is_asked_about_again() {
    for edited in [false, true] {
        a_discarded_file_that_s_no_longer_holds_is_asked_about_again(edited);
    }
}

fn a_discarded_file_that_s_no_longer_holds_is_asked_about_again(edited: bool) {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::effects::Effects;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::{LocalNode as Entry, RemoteNode};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut trees = Trees::new("root");
    let identity = FileIdentity {
        dev: 1,
        ino: 1,
        birth_ns: Some(0),
    };
    trees.set_local(
        "secret".into(),
        Entry {
            shape: engine_shape("root", "secret.txt"),
            identity,
            parked: false,
        },
    );
    trees.set_remote(
        "other".into(),
        RemoteNode {
            shape: engine_shape("root", "other.txt"),
            trashed: false,
            meta_rev: 1,
            meta_edit_rev: 1,
            content_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    let mut commits = Commits::default();
    let mut ids = IdGen::new(333);
    commits.first_link(&mut trees, &mut ids, 1);
    let question = commits.questions()[0].id.clone();
    assert!(commits.decide(&question, Answer::Discard));
    assert!(
        commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .is_none()
    );
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        0,
        0,
    );
    trees.settle();
    assert!(trees.synced().get("secret").is_none());
    assert!(!trees.waiting().contains_key("secret"));
    let mut now = file_shape("root", "secret.txt");
    if edited {
        now.content = Some(content(6));
        let mut entry = trees.local().get("secret").unwrap().clone();
        entry.shape.content = Some(content(6));
        trees.set_local("secret".into(), entry);
    }
    let before = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        effects: EffectsState::default(),
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_local_node(&LocalNode {
                node_id: "secret".into(),
                shape: now,
                identity: FileId {
                    dev: 1,
                    ino: 1,
                    birth_ns: Some(0),
                },
                parked: false,
            })?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "discarded_shapes");

    let after = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    trees.restore(after.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(after.commits).unwrap();
    commits.hold(&mut trees);
    let built = commits
        .build(&mut trees, &mut ids, 1, "history", "me", 1)
        .cloned();
    assert!(built.is_none(), "a discarded file was uploaded: {built:?}");
    let mut effects = Effects::default();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
        .first()
        .cloned()
    {
        assert_ne!(record.node, "secret", "edited: {edited}, {record:?}");
        let made = FileIdentity { ino: 3, ..identity };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(made),
            )
            .unwrap();
        trees.settle();
    }
    assert!(trees.held("secret"));
    let asked = commits.questions().last().unwrap();
    assert_eq!(
        asked.shown["secret"].content,
        trees.local().get("secret").unwrap().shape.content
    );
    let answer = if edited {
        Answer::Publish
    } else {
        Answer::Discard
    };
    assert!(commits.decide(&asked.id.clone(), answer));
    let built = commits
        .build(&mut trees, &mut ids, 1, "history", "me", 1)
        .cloned();
    let created = built.is_some_and(|commit| {
        (commit.ops.iter()).any(|operation| {
            matches!(&operation.op, Some(op::Op::CreateFile(create)) if create.node_id == "secret")
        })
    });
    assert_eq!(created, edited);
    if !edited {
        let removed = effects
            .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
            .first()
            .cloned()
            .unwrap();
        assert_eq!(removed.node, "secret");
        assert!(matches!(removed.effect, Effect::RemoveFile { .. }));
    }
}

/// A database of the schema before restores gains the two restore tables, empty, and keeps the
/// engine state it held.
#[test]
fn a_version_3_store_opens_with_empty_restore_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = legacy_state();
    before.commits.published.clear();
    before.trees.unsent_edits.clear();
    before.commits.in_flight.as_mut().unwrap().commit.ops.pop();
    before.trees.watched.clear();
    before.trees.trashed_here.clear();
    before.trees.discarded.clear();
    before.trees.edited_here.clear();
    before.trees.replaced_versions.clear();
    before.trees.resends.clear();
    before.trees.watched_positions.clear();
    before.commits.slots.clear();
    before.commits.restores.clear();
    before.commits.resolved.clear();
    before.commits.questions.clear();
    before.commits.guard = GuardState::default();
    before.trees.outside.clear();
    before.trees.lost.clear();
    before.trees.displaced.clear();
    before.trees.unsent_marks.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "04_restores");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    store
        .transaction(|tx| {
            assert_eq!(tx.engine_state()?, before);
            assert_eq!(tx.restore_requests()?, vec![]);
            tx.request_restore(&id(100), &from_trash(1))?;
            tx.set_engine_state(&legacy_state())?;
            assert_eq!(tx.engine_state()?, legacy_state());
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_stored_restore_the_server_would_refuse_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO restore_intents (restore_id, node_id) VALUES ('r', 'not-a-uuid')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO restore_requests (restore_id, node_id) VALUES ('r', 'not-a-uuid')",
        [],
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    let loaded = store.transaction(|tx| tx.engine_state());
    assert!(
        matches!(loaded, Err(Error::InvalidEngineState(_))),
        "{loaded:?}"
    );
    let taken = store.transaction(|tx| tx.take_restore_requests());
    assert!(
        matches!(taken, Err(Error::InvalidEngineState(_))),
        "{taken:?}"
    );
}

#[test]
fn id_generator_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.id_generator()?, None);
            tx.set_id_generator(&[1, 2, 3])
        })
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.id_generator()).unwrap(),
        Some(vec![1, 2, 3])
    );
}

#[test]
fn failed_transaction_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let synced = SyncedNode {
        node_id: "root".into(),
        shape: dir_shape("", ""),
        pending_for: String::new(),
    };

    let result: Result<(), Error> = store.transaction(|tx| {
        tx.put_synced_node(&synced)?;
        tx.set_cursor(&Cursor {
            history_id: "h1".into(),
            last_seq: 1,
            ahead_of_server: false,
            ..Default::default()
        })?;
        Err(Error::InvalidEngineState("rolled back".into()))
    });
    assert!(result.is_err());

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.transaction(|tx| {
            tx.put_synced_node(&synced)?;
            panic!("crash inside the transaction");
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        });
    }));
    assert!(panicked.is_err());

    store
        .transaction(|tx| {
            assert_eq!(tx.synced_nodes()?, []);
            assert_eq!(tx.cursor()?, None);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn chunk_cache_tracks_size_and_eviction_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);

    store
        .transaction(|tx| {
            tx.put_chunk(&a, 100, 10)?;
            tx.put_chunk(&b, 200, 20)?;
            tx.put_chunk(&c, 300, 30)?;
            assert!(tx.touch_chunk(&a, 40)?);
            assert!(!tx.touch_chunk(&[9; 32], 40)?);
            assert_eq!(tx.chunk_cache_size()?, 600);
            assert_eq!(
                tx.chunk(&a)?,
                Some(CachedChunk {
                    hash: a,
                    size: 100,
                    last_used_ms: 40
                })
            );
            let order: Vec<_> = tx
                .least_recently_used(2)?
                .into_iter()
                .map(|chunk| chunk.hash)
                .collect();
            assert_eq!(order, [b, c]);
            assert!(tx.remove_chunk(&b)?);
            assert_eq!(tx.chunk(&b)?, None);
            assert_eq!(tx.chunk_cache_size()?, 400);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_read_only_open_reads_the_link_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state #1?.db");
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Koidrive"),
        root_dev: 66306,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    let listing = || {
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                (
                    entry.file_name(),
                    meta.modified().unwrap(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    };
    let before = listing();

    let read = Store::open_read_only(&path)
        .unwrap()
        .transaction(|tx| tx.link())
        .unwrap();

    assert_eq!(read, Some(link.clone()));
    assert_eq!(listing(), before);

    let mut writer = Store::open(&path).unwrap();
    writer
        .transaction(|tx| {
            tx.set_link(&Link {
                space_name: "Renamed".into(),
                ..link.clone()
            })
        })
        .unwrap();
    match Store::open_read_only(&path) {
        Err(Error::LogNotApplied(log)) => assert!(log.ends_with("state #1?.db-wal")),
        other => panic!("expected LogNotApplied, got {:?}", other.err()),
    }
}

#[test]
fn link_and_questions_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Koidrive"),
        root_dev: 66306,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
    };
    let question = |id: &str, at: i64| StoredQuestion {
        question_id: id.into(),
        asked_at_ms: at,
        body: format!("{{\"id\":\"{id}\"}}").into_bytes(),
        answer: None,
    };
    let answer = GivenAnswer {
        answer: "restore".into(),
        answered_at_ms: 30,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            assert_eq!(tx.link()?, None);
            tx.set_link(&link)?;
            tx.put_question(&question("q2", 20))?;
            tx.put_question(&question("q1", 10))?;
            assert!(tx.answer_question("q2", &answer)?);
            assert!(!tx.answer_question(
                "q2",
                &GivenAnswer {
                    answer: "delete".into(),
                    answered_at_ms: 40
                }
            )?);
            assert!(!tx.answer_question("missing", &answer)?);
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.link()?, Some(link.clone()));
            let questions = tx.questions()?;
            assert_eq!(questions[0], question("q1", 10));
            assert_eq!(questions[1].question_id, "q2");
            assert_eq!(questions[1].answer, Some(answer.clone()));
            assert!(tx.remove_question("q1")?);
            assert!(!tx.remove_question("q1")?);
            assert_eq!(tx.questions()?.len(), 1);
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.taken_question("q2")?, None);
            let q2 = (tx.questions()?.into_iter())
                .find(|question| question.question_id == "q2")
                .unwrap();
            tx.put_question(&question("q3", 30))?;
            assert!(
                !tx.take_answer("q3")?,
                "an unanswered question has nothing to take"
            );
            assert!(tx.take_answer("q2")?);
            assert_eq!(tx.taken_question("q2")?, Some(q2));
            let left: Vec<String> = (tx.questions()?.into_iter())
                .map(|question| question.question_id)
                .collect();
            assert_eq!(left, ["q3"]);
            Ok::<_, Error>(())
        })
        .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        (store.transaction(|tx| tx.taken_question("q2")).unwrap())
            .and_then(|question| question.answer),
        Some(answer.clone()),
        "a taken answer outlives the store's reopening"
    );
}

#[test]
fn remote_tree_can_be_counted_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    store
        .transaction(|tx| {
            for id in ["a", "b", "c"] {
                tx.put_remote_node(&NodeState {
                    node_id: id.into(),
                    parent_id: "root".into(),
                    name: id.into(),
                    kind: NodeKind::Dir as i32,
                    status: NodeStatus::Live as i32,
                    ..Default::default()
                })?;
            }
            assert_eq!(tx.remote_node_count()?, 3);
            tx.clear_remote_nodes()?;
            assert_eq!(tx.remote_node_count()?, 0);
            assert!(tx.remote_nodes()?.is_empty());
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_discarded_folder_is_not_sent_while_a_question_asks_again_about_what_it_holds() {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::effects::Effects;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::{LocalNode as Entry, RemoteNode};

    for answer in [Answer::Discard, Answer::Publish] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut trees = Trees::new("root");
        let folder = EngineShape {
            parent: "root".into(),
            name: "dir".into(),
            kind: Kind::Dir,
            content: None,
        };
        for (ino, (id, shape)) in [("dir", folder), ("c", engine_shape("dir", "c.txt"))]
            .into_iter()
            .enumerate()
        {
            let identity = FileIdentity {
                dev: 1,
                ino: ino as u128 + 1,
                birth_ns: Some(0),
            };
            let entry = Entry {
                shape,
                identity,
                parked: false,
            };
            trees.set_local(id.into(), entry);
        }
        trees.set_remote(
            "other".into(),
            RemoteNode {
                shape: engine_shape("root", "other.txt"),
                trashed: false,
                meta_rev: 1,
                meta_edit_rev: 1,
                content_rev: 1,
                trash_group: String::new(),
                version_id: String::new(),
                edit_rev: 1,
                pending_for: None,
                pending_by: None,
            },
        );
        trees.settle();
        let mut commits = Commits::default();
        let mut ids = IdGen::new(335);
        commits.first_link(&mut trees, &mut ids, 1);
        let question = commits.questions()[0].id.clone();
        assert!(commits.decide(&question, Answer::Discard));
        assert!(
            commits
                .build(&mut trees, &mut ids, 1, "history", "me", 1)
                .is_none()
        );
        trees.set_synced("c".into(), None);
        trees.settle();
        assert!(trees.synced().get("dir").is_some());
        assert!(trees.synced().get("c").is_none());
        let before = EngineState {
            trees: trees.state(),
            commits: commits.state(),
            effects: EffectsState::default(),
        };
        Store::open(&path)
            .unwrap()
            .transaction(|tx| {
                tx.put_synced_node(&SyncedNode {
                    node_id: "dir".into(),
                    shape: dir_shape("root", "dir"),
                    pending_for: String::new(),
                })?;
                for (ino, (id, shape)) in [
                    ("dir", dir_shape("root", "dir")),
                    ("c", file_shape("dir", "c.txt")),
                ]
                .into_iter()
                .enumerate()
                {
                    tx.put_local_node(&LocalNode {
                        node_id: id.into(),
                        shape,
                        identity: FileId {
                            dev: 1,
                            ino: ino as u128 + 1,
                            birth_ns: Some(0),
                        },
                        parked: false,
                    })?;
                }
                tx.set_engine_state(&before)
            })
            .unwrap();
        store_before(&path, "discarded_shapes");

        let after = (Store::open(&path).unwrap())
            .transaction(|tx| tx.engine_state())
            .unwrap();
        trees.restore(after.trees).unwrap();
        trees.rules_scanned();
        let mut commits = Commits::from_state(after.commits).unwrap();
        commits.hold(&mut trees);
        assert!(trees.held("c") && !trees.held("dir"));
        let built = commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .cloned();
        assert!(built.is_none(), "sent before the answer: {built:?}");
        let mut effects = Effects::default();
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
            .first()
        {
            assert!(record.node != "dir" && record.node != "c", "{record:?}");
        }

        let asked = commits.questions().last().unwrap().id.clone();
        assert!(commits.decide(&asked, answer));
        let sent: Vec<String> = commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .into_iter()
            .flat_map(|commit| &commit.ops)
            .filter_map(|op| match &op.op {
                Some(op::Op::CreateDir(create)) => Some(create.node_id.clone()),
                Some(op::Op::CreateFile(create)) => Some(create.node_id.clone()),
                _ => None,
            })
            .collect();
        let expected: &[&str] = match answer {
            Answer::Publish => &["dir", "c"],
            _ => &[],
        };
        assert_eq!(sent, expected, "{answer:?}");
    }
}

#[test]
fn a_store_from_before_effects_were_planned_in_batches_keeps_its_effect_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let before = engine_state();
    assert_eq!(before.effects.in_flight.len(), 1);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "effects_in_flight");
    let stored: Option<Vec<u8>> = Connection::open(&path)
        .unwrap()
        .query_row("SELECT in_flight FROM effect_state", [], |row| row.get(0))
        .unwrap();
    assert!(stored.is_some());

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    let mut after = before.clone();
    let mut second = after.effects.in_flight[0].clone();
    second.effect_id = "e2".into();
    second.node = "n2".into();
    after.effects.in_flight.push(second);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_local_changes_were_recorded_records_each_local_node_s_holds_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let local = |node_id: &str, shape: Shape, ino: u64| LocalNode {
        node_id: node_id.into(),
        shape,
        identity: FileId {
            dev: 1,
            ino: ino.into(),
            birth_ns: None,
        },
        parked: false,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for (node_id, name) in [("same", "same.txt"), ("moved", "a.txt")] {
                tx.put_synced_node(&SyncedNode {
                    pending_for: String::new(),
                    node_id: node_id.into(),
                    shape: file_shape("root", name),
                })?;
            }
            tx.put_local_node(&local("same", file_shape("root", "same.txt"), 1))?;
            tx.put_local_node(&local("moved", file_shape("root", "b.txt"), 2))?;
            tx.put_local_node(&local("new", file_shape("root", "new.txt"), 3))?;
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "local_changes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let state = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(state.trees.local_changes, nodes(["moved", "new"]));
}

#[test]
fn a_store_from_before_meta_edit_revisions_reads_each_remote_node_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let node = NodeState {
        node_id: "n".into(),
        parent_id: "root".into(),
        name: "a".into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 3,
        meta_edit_rev: 2,
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.put_remote_node(&node))
        .unwrap();
    store_before(&path, "remote_nodes_meta_edit_rev");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let read = store.transaction(|tx| tx.remote_nodes()).unwrap();
    assert_eq!(
        read,
        [NodeState {
            meta_edit_rev: 0,
            ..node.clone()
        }]
    );
    store.transaction(|tx| tx.put_remote_node(&node)).unwrap();
    assert_eq!(store.transaction(|tx| tx.remote_nodes()).unwrap(), [node]);
}

#[test]
fn a_store_from_before_kept_originals_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "kept_originals");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.kept_originals.clear();
    assert!(!after.trees.kept_originals.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_removals_here_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "removed_here");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let mut after = before.clone();
    let flight = after.commits.in_flight.as_mut().unwrap();
    flight.restores = BTreeMap::from([(2, vec!["r".into(), "r-child".into()])]);
    before.trees.removed_here.clear();
    assert!(!after.trees.removed_here.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_store_from_before_edit_revisions_were_stored_reads_each_as_its_content_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let node = NodeState {
        node_id: "f".into(),
        parent_id: "root".into(),
        name: "b.txt".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: 4,
        meta_rev: 3,
        edit_rev: 2,
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.put_remote_node(&node))
        .unwrap();
    store_before(&path, "remote_nodes_edit_rev");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let nodes = store.transaction(|tx| tx.remote_nodes()).unwrap();
    assert_eq!(
        nodes,
        [NodeState {
            edit_rev: 4,
            ..node
        }]
    );
}

#[test]
fn a_store_from_before_local_changes_were_recorded_records_no_node_l_holds_as_s_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let local = |node_id: &str, shape: Shape, ino: u64| LocalNode {
        node_id: node_id.into(),
        shape,
        identity: FileId {
            dev: 1,
            ino: ino.into(),
            birth_ns: None,
        },
        parked: false,
    };
    let synced = file_shape("root", "m.txt");
    let mut touched = synced.clone();
    if let Some(content) = touched.content.as_mut() {
        content.mtime_ns += 1;
    }
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&SyncedNode {
                pending_for: String::new(),
                node_id: "name".into(),
                shape: file_shape("root", "\u{d55c}.txt"),
            })?;
            tx.put_local_node(&local(
                "name",
                file_shape("root", "\u{1112}\u{1161}\u{11ab}.txt"),
                1,
            ))?;
            tx.put_synced_node(&SyncedNode {
                pending_for: String::new(),
                node_id: "mtime".into(),
                shape: synced,
            })?;
            tx.put_local_node(&local("mtime", touched, 2))?;
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "local_changes");
    let mut store = Store::open(&path).unwrap();
    let state = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(state.trees.local_changes, nodes::<0>([]));
}

#[test]
fn a_store_from_before_dropped_edits_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "dropped_edits");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.dropped_edits.clear();
    assert!(!after.trees.dropped_edits.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn an_edit_in_flight_in_a_store_from_before_dropped_edits_were_recorded_is_sent_again_after_a_loss()
{
    let shape = |name: &str, size| EngineShape {
        content: Some(content(size)),
        ..engine_shape("root", name)
    };
    let remote = |shape, rev, version_id: &str| RemoteNode {
        pending_by: None,
        pending_for: None,
        shape,
        trashed: false,
        content_rev: rev,
        meta_rev: rev,
        meta_edit_rev: rev,
        edit_rev: rev,
        trash_group: String::new(),
        version_id: version_id.into(),
    };
    let local = |shape, ino| EngineLocalNode {
        shape,
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    };
    let mut trees = Trees::new("root");
    for (id, ino) in [("n", 1), ("p", 2)] {
        trees.set_remote(id.into(), remote(shape(id, 1), 1, "v1"));
        trees.set_local(id.into(), local(shape(id, 1), ino));
        trees.set_synced(id.into(), Some(shape(id, 1)));
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    trees.set_local("p".into(), local(shape("p", 2), 2));
    trees.settle();
    assert!(
        commits
            .build(&mut trees, &mut ids, 1_794_268_740_000, "history", "me", 1)
            .is_some()
    );
    trees.set_remote("p".into(), remote(shape("p", 2), 2, "v2"));
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    commits
        .received(CommitResult {
            seq: 2,
            results: vec![applied],
            ..Default::default()
        })
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_remote("n".into(), remote(shape("b", 2), 3, "v2"));
    trees.set_local("n".into(), local(shape("b", 2), 1));
    trees.settle();
    trees.set_local("n".into(), local(shape("b", 3), 1));
    trees.settle();
    let edit = |op: &Op| {
        matches!(&op.op, Some(op::Op::UpdateContent(update))
            if update.node_id == "n" && update.content == Some(content(3)) && !update.resend)
    };
    let sent = commits.build(&mut trees, &mut ids, 1_794_268_740_001, "history", "me", 1);
    assert!(sent.is_some_and(|commit| commit.ops.len() == 1 && edit(&commit.ops[0])));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let saved = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&saved))
        .unwrap();
    store_before(&path, "dropped_edits");

    let mut store = Store::open(&path).unwrap();
    let stored = store.transaction(|tx| tx.engine_state()).unwrap();
    assert!(stored.trees.dropped_edits.is_empty());
    assert_eq!(stored, saved);
    trees.restore(stored.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    let in_flight = commits.edited_in_flight();
    assert_eq!(in_flight, ["n"]);
    let welcome = Welcome {
        history_id: "history".into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    trees.record_dropped_edits(in_flight);
    let dropped = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        ..Default::default()
    };
    store
        .transaction(|tx| tx.update_engine_state(&saved, &dropped))
        .unwrap();
    drop(store);

    let stored = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    assert_eq!(stored, dropped);
    trees.restore(stored.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
    let records = trees.remote_records();
    trees.set_listed_remote("n".into(), remote(shape("n", 1), 1, "v1"));
    trees.set_listed_remote("p".into(), remote(shape("p", 1), 1, "v1"));
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        records,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let copy = |op: &Op| matches!(&op.op, Some(op::Op::CreateFile(create)) if create.content == Some(content(3)));
    let resent = commits.build(&mut trees, &mut ids, 1_794_268_740_002, "history", "me", 1);
    assert!(
        resent.is_some_and(|commit| commit.ops.iter().any(edit) && !commit.ops.iter().any(copy))
    );
}
