// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Sql } from "../host";
import { type Migration, migrate as runMigrations } from "../migrations";

export const MIGRATIONS: readonly Migration[] = [
  `
  CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
  CREATE TABLE members (user_id TEXT PRIMARY KEY, role TEXT NOT NULL, added_seq INTEGER NOT NULL);
  CREATE TABLE devices (device_id TEXT PRIMARY KEY, user_id TEXT NOT NULL,
    name TEXT NOT NULL, max_format INTEGER NOT NULL, last_seen_ms INTEGER);
  CREATE TABLE nodes (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL,
    kind INTEGER NOT NULL,
    status INTEGER NOT NULL,
    content_rev INTEGER NOT NULL, content_dev TEXT NOT NULL,
    meta_rev INTEGER NOT NULL, meta_dev TEXT NOT NULL,
    cur_version_id TEXT,
    trash_group TEXT, trashed_at_ms INTEGER
  );
  CREATE UNIQUE INDEX nodes_live_name ON nodes(parent_id, name_key) WHERE status = 1;
  CREATE INDEX nodes_parent ON nodes(parent_id);
  CREATE INDEX nodes_trashed ON nodes(trashed_at_ms) WHERE status = 2;
  CREATE TABLE versions (
    version_id TEXT PRIMARY KEY, node_id TEXT NOT NULL, seq INTEGER NOT NULL,
    size INTEGER NOT NULL, mtime_ns TEXT NOT NULL, executable INTEGER NOT NULL,
    chunk_count INTEGER NOT NULL, manifest_digest BLOB NOT NULL, chunker TEXT NOT NULL,
    chunks BLOB,
    device_id TEXT NOT NULL, user_id TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
    superseded_at_ms INTEGER
  );
  CREATE INDEX versions_node ON versions(node_id, seq);
  CREATE INDEX versions_superseded ON versions(superseded_at_ms) WHERE superseded_at_ms IS NOT NULL;
  CREATE TABLE version_chunk_pages (version_id TEXT NOT NULL, page INTEGER NOT NULL,
    chunks BLOB NOT NULL, PRIMARY KEY (version_id, page));
  CREATE TABLE chunks (
    hash BLOB PRIMARY KEY, size INTEGER NOT NULL,
    state INTEGER NOT NULL,
    refcount INTEGER NOT NULL DEFAULT 0,
    hold_until_ms INTEGER NOT NULL,
    zero_since_ms INTEGER
  );
  CREATE INDEX chunks_unreferenced ON chunks(state) WHERE refcount = 0;
  CREATE TABLE commits (
    seq INTEGER PRIMARY KEY, commit_id TEXT NOT NULL UNIQUE,
    device_id TEXT NOT NULL, user_id TEXT NOT NULL, committed_at_ms INTEGER NOT NULL,
    record BLOB NOT NULL,
    results BLOB NOT NULL
  );
  CREATE TABLE schedule (task TEXT PRIMARY KEY, due_ms INTEGER NOT NULL);
  `,
  `
  CREATE TABLE IF NOT EXISTS revoked_devices (device_id TEXT PRIMARY KEY, revoked_at_ms INTEGER NOT NULL);
  `,
  `
  CREATE TABLE IF NOT EXISTS device_commits (device_id TEXT PRIMARY KEY, device_seq INTEGER NOT NULL,
    commit_id TEXT NOT NULL, results BLOB NOT NULL);
  `,
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('chunks')").toArray().map((row) => row.name);
    if (!columns.includes("incarnation")) sql.exec("ALTER TABLE chunks ADD COLUMN incarnation TEXT");
    if (!columns.includes("head_checked")) sql.exec("ALTER TABLE chunks ADD COLUMN head_checked INTEGER NOT NULL DEFAULT 0");
    sql.exec("CREATE TABLE IF NOT EXISTS orphans (key TEXT PRIMARY KEY, hash BLOB NOT NULL, incarnation TEXT NOT NULL)");
  },
  `
  CREATE TABLE IF NOT EXISTS rewind_markers (rewind_id TEXT NOT NULL, attempt INTEGER NOT NULL, PRIMARY KEY (rewind_id, attempt));
  `,
  (sql) => {
    for (const table of ["commits", "device_commits"]) {
      const columns = sql.exec(`SELECT name FROM pragma_table_info('${table}')`).toArray().map((row) => row.name);
      if (!columns.includes("request_digest")) sql.exec(`ALTER TABLE ${table} ADD COLUMN request_digest BLOB`);
    }
  },
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('nodes')").toArray().map((row) => row.name);
    if (columns.includes("parent_rev")) return;
    sql.exec("ALTER TABLE nodes ADD COLUMN parent_rev INTEGER NOT NULL DEFAULT 0");
    sql.exec("ALTER TABLE nodes ADD COLUMN parent_dev TEXT NOT NULL DEFAULT ''");
    sql.exec("UPDATE nodes SET parent_rev = meta_rev");
  },
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('nodes')").toArray().map((row) => row.name);
    if (!columns.includes("edit_rev")) {
      sql.exec("ALTER TABLE nodes ADD COLUMN edit_rev INTEGER NOT NULL DEFAULT 0");
      sql.exec("ALTER TABLE nodes ADD COLUMN edit_dev TEXT NOT NULL DEFAULT ''");
      sql.exec("UPDATE nodes SET edit_rev = content_rev, edit_dev = content_dev");
    }
    for (const table of ["commits", "device_commits"]) {
      const held = sql.exec(`SELECT name FROM pragma_table_info('${table}')`).toArray().map((row) => row.name);
      if (!held.includes("digest_version")) sql.exec(`ALTER TABLE ${table} ADD COLUMN digest_version INTEGER NOT NULL DEFAULT 1`);
    }
  },
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('nodes')").toArray().map((row) => row.name);
    if (!columns.includes("meta_edit_rev")) sql.exec("ALTER TABLE nodes ADD COLUMN meta_edit_rev INTEGER");
    if (!columns.includes("meta_edit_dev")) sql.exec("ALTER TABLE nodes ADD COLUMN meta_edit_dev TEXT");
  },
  (sql) => {
    const columns = sql.exec("SELECT name FROM pragma_table_info('nodes')").toArray().map((row) => row.name);
    if (!columns.includes("pending_for")) sql.exec("ALTER TABLE nodes ADD COLUMN pending_for TEXT");
    if (!columns.includes("pending_by")) sql.exec("ALTER TABLE nodes ADD COLUMN pending_by TEXT");
  },
];

export function migrate(sql: Sql): void {
  runMigrations(sql, "meta", MIGRATIONS);
}

export const NodeKindValue = { DIR: 1, FILE: 2 } as const;
export const NodeStatusValue = { LIVE: 1, TRASHED: 2 } as const;
export const ChunkState = { PENDING: 1, LIVE: 2, DELETING: 3 } as const;
