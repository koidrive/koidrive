// SPDX-License-Identifier: Apache-2.0

//! Local SQLite state for one synced space on one device, as the sync engine
//! design lays it out (docs/design/sync-engine.md §2.2 and §2.8):
//!
//! - the remote tree R: the `NodeState` records this device has applied, with
//!   the `(history_id, last_seq)` cursor;
//! - the synced tree S: per node, the shape on which disk and server last agreed;
//! - the local tree L: the disk as last observed, with file identities and the
//!   parked flag;
//! - the rest of the engine's state ([`EngineState`]): the commit in flight
//!   with its buffered result, the accepted counter, the halt, the effect in
//!   flight with the effect counter, the node sets and maps the trees, the
//!   commits and the effects keep, and the restores a person asked for with
//!   their outcomes;
//! - the restore requests the engine has not taken yet;
//! - the engine's ID generator state and the index of chunks cached on disk;
//! - the [`Link`]: the folder the store syncs and the device credential that
//!   writes it, and the questions the engine asked with their answers.
//!
//! The engine keeps the trees in memory and reports row-level changes after each
//! input; the driver writes them with the methods of [`Tx`] inside one
//! [`Store::transaction`]. A transaction either commits as a whole or leaves no
//! trace, including when the process is killed in the middle of it: the
//! database runs in WAL mode with `synchronous = FULL`, so a commit that
//! returned is durable and an interrupted one is rolled back the next time the
//! file is opened. On macOS the store also enables `fullfsync`, so each commit
//! ends with `F_FULLFSYNC` and survives power loss on drives that honor it;
//! `fsync` alone may leave the data in the drive's cache. Process-kill tests
//! cover the rollback, not power loss.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use engine::{EngineChanges, EngineState};
use koidrive_engine::commit::Base;
use koidrive_fs::FileId;
use koidrive_proto::v1::{Content, NodeState, VersionInfo};
use migrations::{LEGACY_STEPS, MIGRATIONS, NAMED};
use prost::Message;
pub use restore::AskedRestore;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

mod engine;
mod questions;
mod restore;

#[doc(hidden)]
pub mod migrations {
    pub struct Migration {
        pub name: &'static str,
        pub requires: &'static [&'static str],
        pub sql: &'static str,
    }

    include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

    pub fn register_functions(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        use koidrive_engine::names::same_name;
        use koidrive_engine::tree::same_content;
        use koidrive_proto::v1::Content;
        use rusqlite::functions::FunctionFlags;

        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        conn.create_scalar_function("same_name", 2, flags, |ctx| {
            Ok(same_name(&ctx.get::<String>(0)?, &ctx.get::<String>(1)?))
        })?;
        conn.create_scalar_function("same_content", 2, flags, |ctx| {
            let content = |at| {
                super::decode::<Content>(ctx.get(at)?)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
            };
            Ok(match (content(0)?, content(1)?) {
                (Some(a), Some(b)) => same_content(&a, &b),
                (a, b) => a.is_none() && b.is_none(),
            })
        })
    }

    pub const NAMED: u32 = 1000;
}

#[cfg(test)]
#[path = "../build/plan.rs"]
mod plan;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored protobuf does not decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error(
        "a newer Koidrive build updated the database with migration {0}, which this build does not know"
    )]
    UnknownMigration(String),
    #[error(
        "database schema version {0} comes from a development build of an unmerged store migration; delete this store file and link the folder again with `koi folder link`"
    )]
    DevelopmentSchema(u32),
    #[error("the database lacks migration {missing}; the daemon applies it when it starts")]
    SchemaTooOld { missing: String },
    #[error(
        "{} holds changes that are not in the database file yet; the daemon applies them when it starts",
        koidrive_fs::quoted(.0)
    )]
    LogNotApplied(PathBuf),
    #[error("the database did not accept the durability settings: {0:?}")]
    NotDurable(Durability),
    #[error("the stored engine state is invalid: {0}")]
    InvalidEngineState(String),
}

/// The connection settings that make a returned commit durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Durability {
    pub journal_mode: String,
    /// `PRAGMA synchronous`: 2 is `FULL`.
    pub synchronous: i64,
    pub fullfsync: bool,
}

impl Durability {
    /// What [`Store::open`] requires on this platform.
    pub fn required() -> Self {
        Self {
            journal_mode: "wal".into(),
            synchronous: 2,
            fullfsync: cfg!(target_os = "macos"),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where this device stands in the space's commit log (spec §7.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    pub history_id: String,
    pub last_seq: u64,
    /// The commit token at `last_seq` (RFC 0013): the `commit_id` and `committed_at_ms` of the commit R applied
    /// there. Empty at seq 0, and in a store from before tokens until the device learns it.
    pub commit_id: String,
    pub committed_at_ms: i64,
    /// The server reported its log below `last_seq` (`AHEAD_OF_SERVER`), and no bootstrap has replaced R
    /// since: the server lost commits this device applied.
    pub ahead_of_server: bool,
}

impl Cursor {
    /// The seq and commit token R holds, as the base of a commit or a `Resume` (RFC 0013).
    pub fn base(&self) -> Base {
        Base {
            seq: self.last_seq,
            commit_id: self.commit_id.clone(),
            committed_at_ms: self.committed_at_ms,
        }
    }
}

/// A node's position, kind and content: what the synced tree records per node
/// and what each op of a commit sent. An empty `parent_id` marks the root;
/// `kind` holds a `NodeKind` value as in `NodeState`; `content` is set for files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shape {
    pub parent_id: String,
    pub name: String,
    pub kind: i32,
    pub content: Option<Content>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncedNode {
    pub node_id: String,
    pub shape: Shape,
}

/// A node of the local tree. `shape.name` is the exact name on disk. A parked
/// node sits in `.koidrive/parked/<node_id>` while `shape` keeps the
/// position the engine compares it at.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalNode {
    pub node_id: String,
    pub shape: Shape,
    pub identity: FileId,
    pub parked: bool,
}

/// The folder this store syncs and the credential that writes it. One device
/// credential writes a store, and a store belongs to one linked folder: a
/// driver refuses to sync a store whose `device_id` is not its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub space_id: String,
    pub space_name: String,
    /// The linked folder's absolute path.
    pub root: PathBuf,
    /// The device (`st_dev`) the folder was on when it was linked, for the
    /// root check of design §6.2.
    pub root_dev: u64,
    pub server_url: String,
    pub device_id: String,
    pub linked_at_ms: i64,
}

/// A question the engine asked the person (`AskUser`, design §1), with its
/// answer once given. The body is opaque to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredQuestion {
    pub question_id: String,
    pub asked_at_ms: i64,
    pub body: Vec<u8>,
    pub answer: Option<GivenAnswer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenAnswer {
    pub answer: String,
    pub answered_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedChunk {
    pub hash: [u8; 32],
    pub size: u64,
    pub last_used_ms: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database at `path`, creating it if needed, and applies any
    /// migrations it lacks. Fails with [`Error::UnknownMigration`] if a newer build
    /// already migrated it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        #[cfg(target_os = "macos")]
        conn.pragma_update(None, "fullfsync", true)?;
        let mut store = Self { conn };
        let durability = store.durability()?;
        if durability != Durability::required() {
            return Err(Error::NotDurable(durability));
        }
        migrate(&mut store.conn)?;
        Ok(store)
    }

    /// Opens the database at `path` to read it without writing anything: no migration, and no journal or shared-memory
    /// file. SQLite takes the file as unchanging, so the caller makes sure that no process writes it meanwhile. A
    /// database whose applied migrations differ from this build's is refused, and so is one whose write-ahead log
    /// still holds changes, which only a writable open applies.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut log = path.as_os_str().to_owned();
        log.push("-wal");
        let log = PathBuf::from(log);
        if std::fs::metadata(&log).is_ok_and(|log| log.len() > 0) {
            return Err(Error::LogNotApplied(log));
        }
        let mut uri = String::from("file:");
        for &byte in path.as_os_str().as_bytes() {
            match byte {
                b'/' | b'-' | b'_' | b'.' | b'~' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => {
                    uri.push(char::from(byte))
                }
                _ => uri.push_str(&format!("%{byte:02X}")),
            }
        }
        uri.push_str("?immutable=1");
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let (_, applied) = applied_migrations(&conn)?;
        if let Some(missing) = MIGRATIONS.iter().find(|m| !applied.contains(m.name)) {
            return Err(Error::SchemaTooOld {
                missing: missing.name.to_owned(),
            });
        }
        Ok(Self { conn })
    }

    /// The durability settings in effect on this connection.
    pub fn durability(&self) -> Result<Durability> {
        let pragma = |name: &str| {
            self.conn
                .pragma_query_value(None, name, |row| row.get::<_, rusqlite::types::Value>(0))
        };
        let journal_mode = match pragma("journal_mode")? {
            rusqlite::types::Value::Text(mode) => mode.to_ascii_lowercase(),
            other => format!("{other:?}"),
        };
        let integer = |value| match value {
            rusqlite::types::Value::Integer(n) => n,
            _ => -1,
        };
        Ok(Durability {
            journal_mode,
            synchronous: integer(pragma("synchronous")?),
            fullfsync: integer(pragma("fullfsync")?) == 1,
        })
    }

    /// Runs `f` in one write transaction. The transaction commits if `f`
    /// returns `Ok` and rolls back if it returns `Err` or panics.
    pub fn transaction<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&Tx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let tx = Tx(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(Error::from)?);
        let value = f(&tx)?;
        tx.0.commit().map_err(Error::from)?;
        Ok(value)
    }
}

fn applied_migrations(conn: &Connection) -> Result<(u32, BTreeSet<String>)> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let applied: BTreeSet<String> = match version {
        NAMED => conn
            .prepare("SELECT name FROM migrations")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?,
        positional if positional as usize <= LEGACY_STEPS => MIGRATIONS[..positional as usize]
            .iter()
            .map(|m| m.name.to_owned())
            .collect(),
        other => return Err(Error::DevelopmentSchema(other)),
    };
    if let Some(unknown) = applied
        .iter()
        .find(|name| !MIGRATIONS.iter().any(|m| m.name == name.as_str()))
    {
        return Err(Error::UnknownMigration(unknown.clone()));
    }
    Ok((version, applied))
}

fn migrate(conn: &mut Connection) -> Result<()> {
    migrations::register_functions(conn)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (version, applied) = applied_migrations(&tx)?;
    if version != NAMED {
        tx.execute_batch("CREATE TABLE migrations (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;")?;
        for name in &applied {
            tx.execute("INSERT INTO migrations (name) VALUES (?1)", [name])?;
        }
        tx.pragma_update(None, "user_version", NAMED)?;
    }
    tx.commit()?;
    if MIGRATIONS.iter().all(|m| applied.contains(m.name)) {
        return Ok(());
    }
    for migration in MIGRATIONS {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let done: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM migrations WHERE name = ?1)",
            [migration.name],
            |row| row.get(0),
        )?;
        if !done {
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "INSERT INTO migrations (name) VALUES (?1)",
                [migration.name],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// An open transaction. Obtain one with [`Store::transaction`].
pub struct Tx<'a>(rusqlite::Transaction<'a>);

impl Tx<'_> {
    pub fn cursor(&self) -> Result<Option<Cursor>> {
        Ok(self
            .0
            .query_row(
                "SELECT history_id, last_seq, ahead_of_server, commit_id, committed_at_ms FROM remote_cursor
                 WHERE id = 1",
                [],
                |row| {
                    Ok(Cursor {
                        history_id: row.get(0)?,
                        last_seq: row.get(1)?,
                        ahead_of_server: row.get(2)?,
                        commit_id: row.get(3)?,
                        committed_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_cursor(&self, cursor: &Cursor) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO remote_cursor
             (id, history_id, last_seq, ahead_of_server, commit_id, committed_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)",
            params![
                cursor.history_id,
                cursor.last_seq,
                cursor.ahead_of_server,
                cursor.commit_id,
                cursor.committed_at_ms
            ],
        )?;
        Ok(())
    }

    /// Every node of the remote tree, live and trashed, ordered by `node_id`.
    pub fn remote_nodes(&self) -> Result<Vec<NodeState>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, status, content_rev, meta_rev, trash_group, current,
                    edit_rev, meta_edit_rev
             FROM remote_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                NodeState {
                    node_id: row.get(0)?,
                    parent_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    name: row.get(2)?,
                    kind: row.get(3)?,
                    status: row.get(4)?,
                    content_rev: row.get(5)?,
                    meta_rev: row.get(6)?,
                    trash_group: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    current: None,
                    edit_rev: row.get(9)?,
                    meta_edit_rev: row.get::<_, Option<u64>>(10)?.unwrap_or_default(),
                },
                row.get::<_, Option<Vec<u8>>>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (mut node, current) = row?;
            node.current = decode(current)?;
            Ok(node)
        })
        .collect()
    }

    /// Inserts or replaces the remote node with `node.node_id`.
    pub fn put_remote_node(&self, node: &NodeState) -> Result<()> {
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO remote_nodes
                 (node_id, parent_id, name, kind, status, content_rev, meta_rev, trash_group, current,
                  edit_rev, meta_edit_rev)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&node.parent_id),
                node.name,
                node.kind,
                node.status,
                node.content_rev,
                node.meta_rev,
                non_empty(&node.trash_group),
                node.current.as_ref().map(VersionInfo::encode_to_vec),
                node.edit_rev,
                node.meta_edit_rev,
            ])?;
        Ok(())
    }

    /// Removes the remote node, for example after `NodePurged`. Returns whether
    /// it existed.
    pub fn remove_remote_node(&self, node_id: &str) -> Result<bool> {
        self.remove("remote_nodes", node_id)
    }

    /// Removes every node of the remote tree, before a bootstrap replaces it.
    pub fn clear_remote_nodes(&self) -> Result<()> {
        self.0.execute("DELETE FROM remote_nodes", [])?;
        Ok(())
    }

    /// The device that wrote the current version of the remote node, if R holds the node with one.
    pub fn remote_writer(&self, node_id: &str) -> Result<Option<String>> {
        let current: Option<Option<Vec<u8>>> = self
            .0
            .query_row(
                "SELECT current FROM remote_nodes WHERE node_id = ?1",
                [node_id],
                |row| row.get(0),
            )
            .optional()?;
        let version = decode::<VersionInfo>(current.flatten())?;
        Ok(version.map(|version| version.device_id))
    }

    /// How many nodes the remote tree holds, live and trashed.
    pub fn remote_node_count(&self) -> Result<u64> {
        Ok(self
            .0
            .query_row("SELECT COUNT(*) FROM remote_nodes", [], |row| row.get(0))?)
    }

    /// Every node of the synced tree, ordered by `node_id`.
    pub fn synced_nodes(&self) -> Result<Vec<SyncedNode>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content FROM synced_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, ShapeRow::read(row, 1)?))
        })?;
        rows.map(|row| {
            let (node_id, shape) = row?;
            Ok(SyncedNode {
                node_id,
                shape: shape.into_shape()?,
            })
        })
        .collect()
    }

    pub fn put_synced_node(&self, node: &SyncedNode) -> Result<()> {
        let shape = &node.shape;
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO synced_nodes (node_id, parent_id, name, kind, content)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&shape.parent_id),
                shape.name,
                shape.kind,
                shape.content.as_ref().map(Content::encode_to_vec),
            ])?;
        Ok(())
    }

    /// Removes the synced node (S := ⊥). Returns whether it existed.
    pub fn remove_synced_node(&self, node_id: &str) -> Result<bool> {
        self.remove("synced_nodes", node_id)
    }

    /// Every node of the local tree, ordered by `node_id`.
    pub fn local_nodes(&self) -> Result<Vec<LocalNode>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content, dev, ino, ino_high, birth_ns, parked
             FROM local_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ShapeRow::read(row, 1)?,
                FileId {
                    dev: row.get::<_, i64>(5)? as u64,
                    ino: ino_from_columns(row.get(6)?, row.get(7)?),
                    birth_ns: row.get(8)?,
                },
                row.get::<_, bool>(9)?,
            ))
        })?;
        rows.map(|row| {
            let (node_id, shape, identity, parked) = row?;
            Ok(LocalNode {
                node_id,
                shape: shape.into_shape()?,
                identity,
                parked,
            })
        })
        .collect()
    }

    /// Inserts or replaces the local node. `dev` is stored as its two's-complement `i64` bit
    /// pattern and `ino` as the bit patterns of its low and high 64 bits, so every value round-trips.
    pub fn put_local_node(&self, node: &LocalNode) -> Result<()> {
        let shape = &node.shape;
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO local_nodes
                 (node_id, parent_id, name, kind, content, dev, ino, ino_high, birth_ns, parked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&shape.parent_id),
                shape.name,
                shape.kind,
                shape.content.as_ref().map(Content::encode_to_vec),
                node.identity.dev as i64,
                ino_columns(node.identity.ino).0,
                ino_columns(node.identity.ino).1,
                node.identity.birth_ns,
                node.parked,
            ])?;
        Ok(())
    }

    pub fn remove_local_node(&self, node_id: &str) -> Result<bool> {
        self.remove("local_nodes", node_id)
    }

    /// The engine's ID generator state, opaque to the store.
    pub fn id_generator(&self) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .query_row(
                "SELECT id_generator FROM engine_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_id_generator(&self, state: &[u8]) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO engine_state (id, id_generator) VALUES (1, ?1)",
            [state],
        )?;
        Ok(())
    }

    /// Records that the chunk is cached, or refreshes its size and last use.
    pub fn put_chunk(&self, hash: &[u8; 32], size: u64, now_ms: i64) -> Result<()> {
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO chunk_cache (hash, size, last_used_ms) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![hash.as_slice(), size, now_ms])?;
        Ok(())
    }

    pub fn chunk(&self, hash: &[u8; 32]) -> Result<Option<CachedChunk>> {
        Ok(self
            .0
            .query_row(
                "SELECT hash, size, last_used_ms FROM chunk_cache WHERE hash = ?1",
                [hash.as_slice()],
                read_chunk,
            )
            .optional()?)
    }

    /// Updates the chunk's last use. Returns whether it is cached.
    pub fn touch_chunk(&self, hash: &[u8; 32], now_ms: i64) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE chunk_cache SET last_used_ms = ?2 WHERE hash = ?1",
            params![hash.as_slice(), now_ms],
        )? > 0)
    }

    pub fn remove_chunk(&self, hash: &[u8; 32]) -> Result<bool> {
        Ok(self
            .0
            .execute("DELETE FROM chunk_cache WHERE hash = ?1", [hash.as_slice()])?
            > 0)
    }

    /// Total bytes of all cached chunks.
    pub fn chunk_cache_size(&self) -> Result<u64> {
        Ok(self.0.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM chunk_cache",
            [],
            |row| row.get(0),
        )?)
    }

    /// Up to `limit` cached chunks, least recently used first: the eviction order.
    pub fn least_recently_used(&self, limit: usize) -> Result<Vec<CachedChunk>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT hash, size, last_used_ms FROM chunk_cache ORDER BY last_used_ms, hash LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], read_chunk)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn link(&self) -> Result<Option<Link>> {
        Ok(self
            .0
            .query_row(
                "SELECT space_id, space_name, root, root_dev, server_url, device_id, linked_at_ms
                 FROM link WHERE id = 1",
                [],
                |row| {
                    Ok(Link {
                        space_id: row.get(0)?,
                        space_name: row.get(1)?,
                        root: PathBuf::from(OsStr::from_bytes(&row.get::<_, Vec<u8>>(2)?)),
                        root_dev: row.get(3)?,
                        server_url: row.get(4)?,
                        device_id: row.get(5)?,
                        linked_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_link(&self, link: &Link) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO link
             (id, space_id, space_name, root, root_dev, server_url, device_id, linked_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                link.space_id,
                link.space_name,
                link.root.as_os_str().as_bytes(),
                link.root_dev,
                link.server_url,
                link.device_id,
                link.linked_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Every question, answered or not, oldest first.
    pub fn questions(&self) -> Result<Vec<StoredQuestion>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, asked_at_ms, body, answer, answered_at_ms
             FROM questions ORDER BY asked_at_ms, question_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let answer: Option<String> = row.get(3)?;
            let answered_at_ms: Option<i64> = row.get(4)?;
            Ok(StoredQuestion {
                question_id: row.get(0)?,
                asked_at_ms: row.get(1)?,
                body: row.get(2)?,
                answer: answer
                    .zip(answered_at_ms)
                    .map(|(answer, answered_at_ms)| GivenAnswer {
                        answer,
                        answered_at_ms,
                    }),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts or replaces the question with `question.question_id`.
    pub fn put_question(&self, question: &StoredQuestion) -> Result<()> {
        let (answer, answered_at_ms) = match &question.answer {
            Some(given) => (Some(given.answer.as_str()), Some(given.answered_at_ms)),
            None => (None, None),
        };
        self.0.execute(
            "INSERT OR REPLACE INTO questions
             (question_id, asked_at_ms, body, answer, answered_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                question.question_id,
                question.asked_at_ms,
                question.body,
                answer,
                answered_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Records the answer to an unanswered question. Returns false, changing
    /// nothing, when the question does not exist or already has an answer.
    pub fn answer_question(&self, question_id: &str, answer: &GivenAnswer) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE questions SET answer = ?2, answered_at_ms = ?3
             WHERE question_id = ?1 AND answer IS NULL",
            params![question_id, answer.answer, answer.answered_at_ms],
        )? > 0)
    }

    /// Replaces how a stored question is shown, keeping any answer it was given.
    pub fn reshow_question(&self, question_id: &str, body: &[u8]) -> Result<()> {
        self.0.execute(
            "UPDATE questions SET body = ?2 WHERE question_id = ?1",
            params![question_id, body],
        )?;
        Ok(())
    }

    /// Removes an answered question whose answer the engine took, and keeps the question with its answer, so that the
    /// same answer given again can be told from one to a question the engine dropped. Returns false, changing nothing,
    /// when the question has no answer.
    pub fn take_answer(&self, question_id: &str) -> Result<bool> {
        let kept = self.0.execute(
            "INSERT OR REPLACE INTO taken_answers (question_id, asked_at_ms, body, answer, answered_at_ms)
             SELECT question_id, asked_at_ms, body, answer, answered_at_ms FROM questions
             WHERE question_id = ?1 AND answer IS NOT NULL",
            [question_id],
        )?;
        if kept > 0 {
            self.remove_question(question_id)?;
        }
        Ok(kept > 0)
    }

    /// The question whose answer the engine took, with that answer, as [`Tx::take_answer`] kept it.
    pub fn taken_question(&self, question_id: &str) -> Result<Option<StoredQuestion>> {
        Ok(self
            .0
            .query_row(
                "SELECT question_id, asked_at_ms, body, answer, answered_at_ms
                 FROM taken_answers WHERE question_id = ?1",
                [question_id],
                |row| {
                    Ok(StoredQuestion {
                        question_id: row.get(0)?,
                        asked_at_ms: row.get(1)?,
                        body: row.get(2)?,
                        answer: Some(GivenAnswer {
                            answer: row.get(3)?,
                            answered_at_ms: row.get(4)?,
                        }),
                    })
                },
            )
            .optional()?)
    }

    pub fn remove_question(&self, question_id: &str) -> Result<bool> {
        Ok(self.0.execute(
            "DELETE FROM questions WHERE question_id = ?1",
            [question_id],
        )? > 0)
    }

    fn remove(&self, table: &str, node_id: &str) -> Result<bool> {
        Ok(self
            .0
            .prepare_cached(&format!("DELETE FROM {table} WHERE node_id = ?1"))?
            .execute([node_id])?
            > 0)
    }
}

/// The shape columns `parent_id, name, kind, content` starting at column
/// `first`, with the content still encoded.
struct ShapeRow {
    shape: Shape,
    content: Option<Vec<u8>>,
}

impl ShapeRow {
    fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            shape: Shape {
                parent_id: row.get::<_, Option<String>>(first)?.unwrap_or_default(),
                name: row.get(first + 1)?,
                kind: row.get(first + 2)?,
                content: None,
            },
            content: row.get(first + 3)?,
        })
    }

    fn into_shape(self) -> Result<Shape> {
        let mut shape = self.shape;
        shape.content = decode(self.content)?;
        Ok(shape)
    }
}

fn decode<M: Message + Default>(bytes: Option<Vec<u8>>) -> Result<Option<M>> {
    Ok(bytes.map(|bytes| M::decode(bytes.as_slice())).transpose()?)
}

fn read_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedChunk> {
    Ok(CachedChunk {
        hash: row.get(0)?,
        size: row.get(1)?,
        last_used_ms: row.get(2)?,
    })
}

fn ino_columns(ino: u128) -> (i64, i64) {
    (ino as u64 as i64, (ino >> 64) as u64 as i64)
}

fn ino_from_columns(low: i64, high: i64) -> u128 {
    (u128::from(high as u64) << 64) | u128::from(low as u64)
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}
