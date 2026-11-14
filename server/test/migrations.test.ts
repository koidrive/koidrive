// SPDX-License-Identifier: AGPL-3.0-or-later

import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import type { AccountDO } from "../src/account-do";
import { SESSION_LEASE_MS } from "../src/account/account";
import { MIGRATIONS as ACCOUNT_MIGRATIONS } from "../src/account/schema";
import { sha256 } from "../src/crypto";
import { OpResult_Code } from "../src/gen/koidrive/v1/commit_pb";
import { toHex, uuidv7 } from "../src/ids";
import type { Migration } from "../src/migrations";
import type { SpaceDO } from "../src/space-do";
import { MIGRATIONS as SPACE_MIGRATIONS } from "../src/space/schema";
import { client, op, OWNER, SPACE_KEY } from "./helpers";

const SPACE_SCHEMA_DIGESTS = [
  "f0dd32ba05d33cf9fb7f49c7d88d2e31834c31e90d2c04b67f4ff0a13b4308fe",
  "bb60e374153c4e445e5b82b2048364ba560f7bf665698c1e73d8d6c974fe088a",
  "2d050ddef36718340cf200f56a0b9c29cc36f63f79f496b2708794f267f2c14d",
  "4f47c847b7d5734cb4a21738c70e6a8f1837c5b44e73f81115afd6c38d188aa3",
  "bace53ef766c53ef40805288c138d854ac27628cd5674daf517d2abe81547c56",
  "857a54a49b2c3ee1fa6b96cd0aca389f4eb674174ccd90dd9de9bf1ee16c7ac2",
  "f3fd30e63a5e7d34587553333ac4d0e9f72403d7a6b2f7bb6030286b84facefc",
  "ebd4aca495e8c70c997dde3afad3cdb3e934b87353f9b176433266161c828873",
  "bbfcb7dcd31743e351d3d64695b532116b6b71ca27ac36aca6d007eb4f6f040f",
  "45b53d74672f87f32b4c3e30cdcbfce34514ef0a65dfd9782d3aae4d7ddd6288",
];
const ACCOUNT_SCHEMA_DIGESTS = [
  "cdd5988fc835119f5b6ad96ebde0be7f38aac621215c10844fdb4fb99f4a7fcf",
  "2c844373f6d282297d92a724cfdc83b62a30648e517e9f34c6e5c528e5437c7d",
  "b67a86b2ae0d362375f2879afc44654cdf43a636f02849f823296d467777e3e0",
  "81a059462fcc85e8d79e39486df0357cd8d2ca2b06c5ec5cc11b35f484b33185",
  "b6c36eba27d26386f256dc8d64bc63f97cd5d47c47b4f328fb608a1e21b6b0f8",
  "d1acc17396a3ec9113232584f4f72afeded27e49556b0dabe3fd241db9458ec4",
  "c8f022a3869613f651320f6d5d56c8d67398f2703761a4a4e99af009badfb090",
  "9b588dbfc49e370ed9f83bfe686d334d291250b3b818f61493eb9b6e9ad652d7",
  "aaec735e7ef47b5644e1f838dab896941f056da73eca6c6dcb65c3fd67cabac3",
  "6b5b1959385d6b4431dc9dd381a45519e6af594b2561538767c8a46a1a6f6e63",
];

type Stub = DurableObjectStub<SpaceDO> | DurableObjectStub<AccountDO>;
type Rows = Record<string, unknown>[];

interface StorageKind {
  name: string;
  steps: readonly Migration[];
  versionTable: string;
  digests: readonly string[];
  stub(name: string): Stub;
}

const SPACE: StorageKind = {
  name: "Space DO",
  steps: SPACE_MIGRATIONS,
  versionTable: "meta",
  digests: SPACE_SCHEMA_DIGESTS,
  stub: (name) => env.SPACE.get(env.SPACE.idFromName(name)),
};

const ACCOUNT: StorageKind = {
  name: "Account DO",
  steps: ACCOUNT_MIGRATIONS,
  versionTable: "settings",
  digests: ACCOUNT_SCHEMA_DIGESTS,
  stub: (name) => env.ACCOUNT.get(env.ACCOUNT.idFromName(name)),
};

interface OldState {
  label: string;
  applied: number;
  versioned: boolean;
}

function oldStates(kind: StorageKind): OldState[] {
  const states = Array.from({ length: kind.steps.length }, (_, applied) => ({ label: `schema version ${applied}`, applied, versioned: true }));
  if (kind === ACCOUNT) {
    states.push({ label: "unversioned storage from #8", applied: 1, versioned: false });
    states.push({ label: "unversioned storage from #25", applied: 2, versioned: false });
  }
  return states;
}

function userTables(sql: SqlStorage): string[] {
  return sql
    .exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE '_cf_%' AND name NOT LIKE 'sqlite_%' ORDER BY name")
    .toArray()
    .map((row) => String(row.name));
}

function schemaOf(sql: SqlStorage): string[] {
  return sql
    .exec("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE '_cf_%' AND name NOT LIKE 'sqlite_%' ORDER BY type, name")
    .toArray()
    .map((row) => `${row.type} ${row.name}: ${row.sql}`);
}

async function digestOf(sql: SqlStorage): Promise<string> {
  return toHex(await sha256(new TextEncoder().encode(schemaOf(sql).join("\n"))));
}

function versionOf(sql: SqlStorage, kind: StorageKind): number {
  return Number(sql.exec(`SELECT value FROM ${kind.versionTable} WHERE key = 'schema_version'`).one().value);
}

function rewrite(sql: SqlStorage, kind: StorageKind, state: OldState, version = state.applied): void {
  for (const table of userTables(sql)) sql.exec(`DROP TABLE "${table}"`);
  for (const step of kind.steps.slice(0, state.applied)) {
    if (typeof step === "string") sql.exec(step);
    else step(sql);
  }
  if (state.versioned && state.applied > 0) {
    sql.exec(`INSERT INTO ${kind.versionTable} (key, value) VALUES ('schema_version', ?)`, version);
  }
}

function columnsOf(sql: SqlStorage, table: string): { name: string; type: string }[] {
  return sql
    .exec("SELECT name, type FROM pragma_table_info(?)", table)
    .toArray()
    .map((row) => ({ name: String(row.name), type: String(row.type) }));
}

function readRows(sql: SqlStorage, table: string, columns: string[]): Rows {
  const list = columns.map((column) => `"${column}"`).join(", ");
  return sql
    .exec(`SELECT ${list} FROM "${table}" ORDER BY ${list}`)
    .toArray()
    .filter((row) => row.key !== "schema_version")
    .map((row) => Object.fromEntries(Object.entries(row).map(([key, value]) => [key, value instanceof ArrayBuffer ? toHex(new Uint8Array(value)) : value])));
}

function seed(sql: SqlStorage): Map<string, { columns: string[]; rows: Rows }> {
  const seeded = new Map<string, { columns: string[]; rows: Rows }>();
  for (const table of userTables(sql)) {
    const columns = columnsOf(sql, table);
    const values = columns.map(({ type }) =>
      /INT/i.test(type) ? 7 : /BLOB/i.test(type) ? new Uint8Array([1, 2, 3]).buffer : `seed-${table}`,
    );
    const names = columns.map(({ name }) => name);
    const placeholders = names.map(() => "?").join(", ");
    sql.exec(`INSERT INTO "${table}" (${names.map((name) => `"${name}"`).join(", ")}) VALUES (${placeholders})`, ...values);
    seeded.set(table, { columns: names, rows: readRows(sql, table, names) });
  }
  return seeded;
}

function inObject<R>(stub: Stub, callback: (sql: SqlStorage) => R | Promise<R>): Promise<R> {
  return runInDurableObject(stub as DurableObjectStub<SpaceDO>, (_instance, state) => callback(state.storage.sql));
}

for (const kind of [SPACE, ACCOUNT]) {
  describe(`${kind.name} storage migrations`, () => {
    it("pins the schema each step produces, so that merged steps stay unchanged", async () => {
      const digests: string[] = [];
      for (let applied = 1; applied <= kind.steps.length; applied++) {
        const state = { label: "", applied, versioned: true };
        digests.push(await inObject(kind.stub(`digest-${applied}`), (sql) => (rewrite(sql, kind, state), digestOf(sql))));
      }
      const hint = `append the digest of a new step and never change an existing one; this build's digests: ${JSON.stringify(digests)}`;
      expect(digests, hint).toEqual(kind.digests);
    });

    for (const state of oldStates(kind)) {
      it(`upgrades ${state.label} to the current schema once, keeping every row`, async () => {
        const current = await inObject(kind.stub(`fresh-${state.label}`), (sql) => schemaOf(sql));
        const stub = kind.stub(`upgrade-${state.label}`);
        const seeded = await inObject(stub, (sql) => (rewrite(sql, kind, state), seed(sql)));
        expect(seeded.size > 0).toBe(state.applied > 0);

        for (let restart = 0; restart < 2; restart++) {
          await evictDurableObject(stub);
          const after = await inObject(stub, (sql) => ({
            schema: schemaOf(sql),
            version: versionOf(sql, kind),
            rows: [...seeded].map(([table, { columns }]) => [table, readRows(sql, table, columns)]),
          }));
          expect(after.schema).toEqual(current);
          expect(after.version).toBe(kind.steps.length);
          expect(after.rows).toEqual([...seeded].map(([table, { rows }]) => [table, rows]));
        }
      });
    }

    it("refuses storage that a newer release wrote", async () => {
      const stub = kind.stub("from-a-newer-release");
      const newer = kind.steps.length + 1;
      await inObject(stub, (sql) => rewrite(sql, kind, { label: "", applied: kind.steps.length, versioned: true }, newer));
      await evictDurableObject(stub);
      await expect(inObject(stub, () => undefined)).rejects.toThrow(`storage schema version ${newer} is newer than this server knows`);
    });
  });
}

describe("upgraded objects keep working", () => {
  for (const state of oldStates(SPACE)) {
    it(`commits to a space whose storage was at ${state.label}`, async () => {
      const spaceId = uuidv7();
      const stub = SPACE.stub(spaceId) as DurableObjectStub<SpaceDO>;
      await inObject(stub, (sql) => rewrite(sql, SPACE, state));
      await evictDurableObject(stub);
      await stub.init({ spaceId, name: "Upgraded", ownerUserId: OWNER, spaceKey: SPACE_KEY });
      const device = await client({ spaceId, stub }, "Laptop");
      await device.commit([op.mkdir(uuidv7(), spaceId, "Docs")]);
      expect([...(await device.tree()).values()].map((node) => node.name).sort()).toEqual(["", "Docs"]);
    });
  }

  it("keeps what is under a folder moved before parent_rev existed when another device deletes the folder it moved into", async () => {
    const spaceId = uuidv7();
    const stub = SPACE.stub(spaceId) as DurableObjectStub<SpaceDO>;
    await stub.init({ spaceId, name: "Upgraded", ownerUserId: OWNER, spaceKey: SPACE_KEY });
    const laptop = await client({ spaceId, stub }, "Laptop");
    const desk = await client({ spaceId, stub }, "Desk");
    const [moved, inside, target] = [uuidv7(), uuidv7(), uuidv7()];
    await laptop.commit([op.mkdir(moved, spaceId, "a"), op.mkdir(inside, moved, "inside"), op.mkdir(target, spaceId, "b")]);
    desk.baseSeq = laptop.baseSeq;
    await laptop.commit([op.move(moved, target, "a")]);
    await inObject(stub, (sql) => {
      sql.exec("ALTER TABLE nodes DROP COLUMN parent_rev");
      sql.exec("ALTER TABLE nodes DROP COLUMN parent_dev");
      const beforeParentRev = SPACE_MIGRATIONS.findIndex((step) => step.toString().includes("parent_rev"));
      sql.exec("UPDATE meta SET value = ? WHERE key = 'schema_version'", beforeParentRev);
    });
    await evictDurableObject(stub);
    const deleted = await desk.commit([op.delete(target)]);
    expect(deleted.results.map((result) => result.code)).toEqual([OpResult_Code.SUPERSEDED]);
    expect([...(await desk.tree()).values()].map((node) => node.name).sort()).toEqual(["", "a", "b", "inside"]);
  });

  for (const state of oldStates(ACCOUNT)) {
    it(`claims a deployment whose Account DO storage was at ${state.label}`, async () => {
      const stub = ACCOUNT.stub(`claim-${state.label}`) as DurableObjectStub<AccountDO>;
      await inObject(stub, (sql) => rewrite(sql, ACCOUNT, state));
      await evictDurableObject(stub);
      const code = await stub.issueClaimCode();
      const claimed = await stub.claim({ code: code!, userName: "Owner", deviceName: "Laptop", platform: "macos" });
      expect(claimed.ok).toBe(true);
      expect(await stub.isClaimed()).toBe(true);
    });
  }

  it("lists the spaces stored before RFC 0007 in the order they were stored, and a new space after them", async () => {
    const stub = ACCOUNT.stub("space-list") as DurableObjectStub<AccountDO>;
    const older = [uuidv7(), uuidv7()].sort().reverse();
    await inObject(stub, (sql) => {
      rewrite(sql, ACCOUNT, { label: "", applied: 5, versioned: true });
      for (const spaceId of older) {
        sql.exec("INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version) VALUES (?, 'Old', ?, 1, 1)", spaceId, OWNER);
      }
    });
    await evictDurableObject(stub);
    const created = await stub.createSpace({ userId: OWNER, name: "New" });
    if (!created.ok) throw new Error(created.message);
    const listed = await stub.listSpaces({ userId: OWNER, after: 0 });
    expect(listed.spaces.map((space) => space.spaceId)).toEqual([...older, created.value.spaceId]);
    expect(listed.nextCursor).toBe("");
  });

  it("lists the devices stored before the device list_seq step in the order they were stored", async () => {
    const stub = ACCOUNT.stub("device-list") as DurableObjectStub<AccountDO>;
    const older = [uuidv7(), uuidv7()].sort().reverse();
    await inObject(stub, (sql) => {
      rewrite(sql, ACCOUNT, { label: "", applied: 6, versioned: true });
      for (const deviceId of older) {
        sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash) VALUES (?, ?, 'Laptop', 'macos', 1, ?)",
          deviceId,
          OWNER,
          new Uint8Array(32),
        );
      }
    });
    await evictDurableObject(stub);
    const listed = await stub.listDevices({ userId: OWNER, callerId: "", cursor: "" });
    expect(listed.devices.map((device) => device.deviceId)).toEqual(older);
    expect(listed.nextCursor).toBe("");
  });

  it("makes every device stored before RFC 0006 a full-scope device that never expires", async () => {
    const stub = ACCOUNT.stub("principals") as DurableObjectStub<AccountDO>;
    const deviceId = uuidv7();
    await inObject(stub, (sql) => {
      rewrite(sql, ACCOUNT, { label: "", applied: 3, versioned: true });
      sql.exec(
        "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash) VALUES (?, ?, 'Laptop', 'macos', 1, ?)",
        deviceId,
        OWNER,
        new Uint8Array(32),
      );
    });
    await evictDurableObject(stub);
    expect(await stub.checkSession(deviceId)).toEqual({ active: true, revoked: false, closed: false, kind: "device", scope: null, leaseMs: SESSION_LEASE_MS });
    const row = await inObject(stub, (sql) => sql.exec("SELECT kind, scope, expires_at_ms, issued_by FROM devices WHERE device_id = ?", deviceId).one());
    expect(row).toEqual({ kind: "device", scope: null, expires_at_ms: null, issued_by: null });
  });
});
