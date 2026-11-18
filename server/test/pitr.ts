// SPDX-License-Identifier: AGPL-3.0-or-later

import { type Pitr, pitrBackend } from "../src/space/pitr";

type Row = Record<string, SqlStorageValue>;

export interface Dump {
  schema: { type: string; sql: string }[];
  rows: Map<string, Row[]>;
}

interface Snapshot {
  bookmark: string;
  atMs?: number;
  dump: Dump;
}

interface Simulated {
  snapshots: Snapshot[];
  pending?: string;
}

const objects = new Map<string, Simulated>();
let counter = 0;

function nextBookmark(): string {
  counter++;
  return "bookmark-" + String(counter).padStart(12, "0");
}

function userTables(sql: SqlStorage): { type: string; name: string; sql: string }[] {
  return sql
    .exec("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE '_cf_%' AND name NOT LIKE 'sqlite_%' ORDER BY type DESC")
    .toArray()
    .map((row) => ({ type: String(row.type), name: String(row.name), sql: String(row.sql) }));
}

function copy(row: Row): Row {
  return Object.fromEntries(Object.entries(row).map(([key, value]) => [key, value instanceof ArrayBuffer ? value.slice(0) : value]));
}

export function dump(sql: SqlStorage): Dump {
  const schema = userTables(sql);
  const rows = new Map<string, Row[]>();
  for (const { type, name } of schema) {
    if (type === "table") rows.set(name, sql.exec(`SELECT * FROM "${name}"`).toArray().map(copy));
  }
  return { schema: schema.map(({ type, sql }) => ({ type, sql })), rows };
}

export function load(sql: SqlStorage, from: Dump): void {
  for (const { type, name } of userTables(sql)) if (type === "table") sql.exec(`DROP TABLE "${name}"`);
  for (const { type, sql: create } of from.schema) if (type === "table") sql.exec(create);
  for (const [name, rows] of from.rows) {
    for (const row of rows) {
      const columns = Object.keys(row);
      sql.exec(
        `INSERT INTO "${name}" (${columns.map((c) => `"${c}"`).join(", ")}) VALUES (${columns.map(() => "?").join(", ")})`,
        ...columns.map((c) => row[c]!),
      );
    }
  }
  for (const { type, sql: create } of from.schema) if (type === "index") sql.exec(create);
}

function simulated(state: DurableObjectState): Simulated {
  const id = state.id.toString();
  let found = objects.get(id);
  if (!found) objects.set(id, (found = { snapshots: [] }));
  return found;
}

export const simulatedPitr = {
  dropArms: false,
  bookmarkFor: undefined as ((timestampMs: number) => string | undefined) | undefined,

  install(): void {
    pitrBackend.open = (state: DurableObjectState): Pitr => {
      const object = simulated(state);
      if (object.pending !== undefined) {
        const target = object.snapshots.find((snapshot) => snapshot.bookmark === object.pending)!;
        object.pending = undefined;
        state.storage.transactionSync(() => load(state.storage.sql, target.dump));
      }
      return {
        currentBookmark: async () => simulatedPitr.snapshot(state),
        bookmarkForTime: async (timestampMs) => {
          if (simulatedPitr.bookmarkFor) return simulatedPitr.bookmarkFor(timestampMs);
          const earlier = object.snapshots.filter((snapshot) => snapshot.atMs !== undefined && snapshot.atMs <= timestampMs);
          return earlier.at(-1)?.bookmark;
        },
        restoreOnNextStart: async (bookmark) => {
          if (!object.snapshots.some((snapshot) => snapshot.bookmark === bookmark)) throw new Error("unknown bookmark " + bookmark);
          if (!simulatedPitr.dropArms) object.pending = bookmark;
        },
      };
    };
  },

  snapshot(state: DurableObjectState, atMs?: number): string {
    const bookmark = nextBookmark();
    simulated(state).snapshots.push({ bookmark, atMs, dump: dump(state.storage.sql) });
    return bookmark;
  },
};
