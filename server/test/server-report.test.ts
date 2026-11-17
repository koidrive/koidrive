// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  SpaceInfoSchema,
  TokenRequest_Telemetry,
  TokenRequestSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { RpcRequestSchema } from "../src/gen/koidrive/v1/messages_pb";
import { reportBatch } from "../src/report/payload";
import { emptyUsage, FLUSH_MS, LATENCY_BUCKETS, latencyBucket, mergeUsage, type Usage, type UsageHours, UsageTally } from "../src/report/usage";

const BASE = "https://koidrive.test";
const HOUR_MS = 3_600_000;

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

async function post<S extends DescMessage>(path: string, sent: Uint8Array, token?: string, schema?: S) {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (token) headers.Authorization = "Bearer " + token;
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body: sent });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return { status: response.status, message: schema && response.ok ? fromBinary(schema, bytes) : undefined };
}

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

async function claim(): Promise<DeviceCredentials> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const code = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"))!.split(": ")[1]!;
  log.mockRestore();
  return (await post("/v1/claim", body(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop A", platform: "macos" }), undefined, DeviceCredentialsSchema)).message!;
}

async function pair(inviter: DeviceCredentials): Promise<DeviceCredentials> {
  const started = await post("/v1/pair/start", new Uint8Array(), inviter.access!.token, PairStartResponseSchema);
  const paired = await post("/v1/pair/complete", body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Laptop B", platform: "linux" }), undefined, DeviceCredentialsSchema);
  return paired.message!;
}

async function refresh(device: DeviceCredentials, telemetry: TokenRequest_Telemetry): Promise<string> {
  const answer = await post("/v1/token", body(TokenRequestSchema, { deviceId: device.deviceId, refreshCredential: device.refreshCredential, telemetry }));
  expect(answer.status).toBe(200);
  return device.deviceId;
}

let claimed: DeviceCredentials | undefined;
const ownerOnce = async () => (claimed ??= await claim());
const countVotes = (instance: AccountDO) => (instance as unknown as { countVotes(): void }).countVotes();
const reporting = () => runInDurableObject(account(), (instance: AccountDO) => instance.reporting());
const storedHours = () => runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT hour, usage FROM usage_hours").toArray());
const storedCounts = async () => (await storedHours()).map((row) => JSON.parse(String(row.usage)) as Usage).reduce((sum, usage) => mergeUsage(sum, usage), emptyUsage()).counts;

afterEach(() => {
  vi.restoreAllMocks();
});

describe("usage tally", () => {
  it("buckets latencies by powers of two", () => {
    expect([0, 0.5, 1, 2, 3, 4, 1000, 16_384, 1e9].map(latencyBucket)).toEqual([0, 0, 1, 2, 2, 3, 10, 15, 15]);
  });

  it("opens a batch with the first count after a hand-over, hands the batch over whole, and counts nothing once the server said it does not report", async () => {
    const tally = new UsageTally();
    const handed: UsageHours[] = [];
    const record = async (hours: UsageHours) => {
      handed.push(hours);
      return true;
    };
    expect(tally.count("get_server", "200", 3, 10 * HOUR_MS)).toBe(true);
    expect(tally.count("get_server", "200", 3, 10 * HOUR_MS + FLUSH_MS - 1)).toBe(false);
    await tally.handOver(record);
    expect(handed).toEqual([{ 10: { counts: { "get_server.200": 2 }, latency: { get_server: [0, 0, 2, ...new Array(LATENCY_BUCKETS - 3).fill(0)] } } }]);
    await tally.handOver(record);
    expect(handed).toHaveLength(1);
    expect(tally.count("get_server", "200", 3, 11 * HOUR_MS)).toBe(true);
    await tally.handOver(async () => false);
    expect(tally.count("get_server", "200", 3, 11 * HOUR_MS)).toBe(false);
    await tally.handOver(record);
    expect(handed).toHaveLength(1);
  });

  it("drops the counts it holds once the server says it stopped reporting", async () => {
    const tally = new UsageTally();
    const handed: UsageHours[] = [];
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    tally.reporting = false;
    tally.reporting = true;
    await tally.handOver(async (hours) => {
      handed.push(hours);
      return true;
    });
    expect(handed).toEqual([]);
  });

  it("carries the counts of a failed hand-over, and those made while it ran, in the next hand-over", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const tally = new UsageTally();
    const handed: UsageHours[] = [];
    expect(tally.count("get_server", "200", 3, 10 * HOUR_MS)).toBe(true);
    await tally.handOver(async () => {
      expect(tally.count("get_server", "503", 3, 10 * HOUR_MS)).toBe(true);
      throw new Error("account unreachable");
    });
    await tally.handOver(async (hours) => {
      handed.push(hours);
      return true;
    });
    expect(handed.map((hours) => hours[10]!.counts)).toEqual([{ "get_server.200": 1, "get_server.503": 1 }]);
  });
});

describe("report payload", () => {
  it("holds exactly these keys and only route and outcome names", () => {
    const usage = emptyUsage();
    usage.counts["put_chunk.503.retry_later.r2_rate_limited"] = 2;
    usage.counts["rpc_list_tree.ok"] = 5;
    usage.latency.put_chunk = new Array(LATENCY_BUCKETS).fill(1);
    const sent = reportBatch([{ hour: 490_000, id: "7d3c2a4e-0000-4000-8000-000000000001", usage }], { version: "0.1.0", protoMin: 1, protoMax: 1 }) as {
      batch: { properties: Record<string, unknown> }[];
    };
    expect(Object.keys(sent).sort()).toEqual(["api_key", "batch", "historical_migration"]);
    expect(sent.batch).toHaveLength(1);
    expect(Object.keys(sent.batch[0]!).sort()).toEqual(["event", "properties", "timestamp", "uuid"]);
    expect(sent.batch[0]).toMatchObject({ event: "server_report", uuid: "7d3c2a4e-0000-4000-8000-000000000001", timestamp: "2025-11-24T16:00:00.000Z" });
    expect(Object.keys(sent.batch[0]!.properties).sort()).toEqual([
      "$geoip_disable",
      "$process_person_profile",
      "c.put_chunk.503.retry_later.r2_rate_limited",
      "c.rpc_list_tree.ok",
      "distinct_id",
      "h.put_chunk",
      "proto_max",
      "proto_min",
      "schema",
      "server_version",
    ]);
    expect(sent.batch[0]!.properties).toMatchObject({ distinct_id: "7d3c2a4e-0000-4000-8000-000000000001", $process_person_profile: false, $geoip_disable: true, schema: 1 });
  });
});

describe("hourly server report", () => {
  it("follows the devices' choice, counts the hour's traffic and sends it once with no id of a device, space or person", async () => {
    const owner = await ownerOnce();
    const second = await pair(owner);
    expect(await reporting()).toBe(false);
    await refresh(owner, TokenRequest_Telemetry.ON);
    expect(await reporting()).toBe(true);
    await refresh(second, TokenRequest_Telemetry.OFF);
    expect(await reporting()).toBe(false);
    await refresh(second, TokenRequest_Telemetry.UNSPECIFIED);
    expect(await reporting()).toBe(true);
    await refresh(owner, TokenRequest_Telemetry.UNSPECIFIED);
    expect(await reporting()).toBe(false);
    await refresh(owner, TokenRequest_Telemetry.ON);
    await refresh(second, TokenRequest_Telemetry.ON);
    expect(await reporting()).toBe(true);

    const startMs = Date.now();
    const created = await post("/v1/spaces", body(CreateSpaceRequestSchema, { name: "Holiday photos" }), owner.access!.token, SpaceInfoSchema);
    const spaceId = created.message!.spaceId;
    const listed = await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }), owner.access!.token);
    expect(listed.status).toBe(200);
    await vi.waitFor(async () => {
      const keys = (await storedHours()).flatMap((row) => Object.keys(JSON.parse(String(row.usage)).counts));
      expect(keys).toEqual(expect.arrayContaining(["post_spaces.201", "rpc_list_tree.ok"]));
    });

    const sent: unknown[] = [];
    await runInDurableObject(account(), (instance: AccountDO) => {
      instance.clock = () => startMs + 3 * HOUR_MS;
      instance.sendReport = async (report) => {
        sent.push(report);
        return true;
      };
    });
    await runDurableObjectAlarm(account());
    expect(sent).toHaveLength(1);
    const text = JSON.stringify(sent[0]);
    for (const secret of [owner.deviceId, second.deviceId, spaceId, owner.userId, "Owner", "Laptop", "Holiday", owner.refreshCredential]) {
      expect(text).not.toContain(secret);
    }
    const properties = (sent[0] as { batch: { properties: Record<string, unknown> }[] }).batch.flatMap((event) => Object.keys(event.properties));
    for (const key of properties) expect(key).toMatch(/^(?:\$geoip_disable|\$process_person_profile|distinct_id|schema|server_version|proto_min|proto_max|c\.[a-z_]+\.[a-z0-9_.]+|h\.[a-z_]+)$/);
    expect(await storedHours()).toEqual([]);
  });

  it("hands over what a Space DO counted by its own timer when no request follows", async () => {
    const owner = await ownerOnce();
    await runInDurableObject(account(), (instance: AccountDO) => {
      instance.clock = () => Date.now();
    });
    await refresh(owner, TokenRequest_Telemetry.ON);
    expect(await reporting()).toBe(true);
    const created = await post("/v1/spaces", body(CreateSpaceRequestSchema, { name: "Quiet" }), owner.access!.token, SpaceInfoSchema);
    const spaceId = created.message!.spaceId;
    UsageTally.windowMs = 200;
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
    for (let reqId = 1n; reqId <= 5n; reqId++) {
      expect((await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token)).status).toBe(200);
    }
    await vi.waitFor(async () => expect((await storedCounts())["rpc_list_tree.ok"]).toBe(5), { timeout: 3000 });
  });

  it("drops the stored hours without sending them once a device turns reports off", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: unknown[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, (hour + 2) * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      await instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
      state.storage.sql.exec("UPDATE devices SET telemetry = ? WHERE rowid = (SELECT MIN(rowid) FROM devices)", TokenRequest_Telemetry.OFF);
      countVotes(instance);
      instance.clock = () => (hour + 2) * HOUR_MS;
      instance.sendReport = async (report) => {
        sent.push(report);
        return true;
      };
    });
    expect(await storedHours()).toHaveLength(1);
    await runDurableObjectAlarm(account());
    expect(sent).toEqual([]);
    expect(await storedHours()).toEqual([]);
    expect(await runInDurableObject(account(), (instance: AccountDO) => instance.recordUsage({ [hour]: emptyUsage() }))).toBe(false);
  });

  it("sends one server_report an hour even when a process hands over that hour's counts after it was sent", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: { batch: unknown[] }[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, hour * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      await instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
      instance.clock = () => (hour + 1) * HOUR_MS + 6 * 60_000;
      instance.sendReport = async (report) => {
        sent.push(report as { batch: unknown[] });
        return true;
      };
    });
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    await runInDurableObject(account(), async (instance: AccountDO) => {
      instance.clock = () => (hour + 1) * HOUR_MS + 20 * 60_000;
      await instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
    });
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(sent.flatMap((report) => report.batch)).toHaveLength(1);
  });

  it("counts an ON vote for 24 to 25 hours after its device last refreshed, and an OFF vote until the device replaces it or is revoked", async () => {
    const owner = await ownerOnce();
    const second = await pair(owner);
    const at = async (clockMs: number, step: () => Promise<unknown>) => {
      await runInDurableObject(account(), (instance: AccountDO) => {
        instance.clock = () => clockMs;
      });
      await step();
      return reporting();
    };
    const startMs = Date.now();
    expect(await at(startMs, () => refresh(owner, TokenRequest_Telemetry.ON))).toBe(true);
    expect(await at(startMs + 24 * HOUR_MS - 1, async () => {})).toBe(true);
    expect(await at(startMs + 25 * HOUR_MS + 60_000, async () => {})).toBe(false);
    expect(await at(startMs + 26 * HOUR_MS, () => refresh(owner, TokenRequest_Telemetry.ON))).toBe(true);
    expect(await at(startMs + 26 * HOUR_MS, () => refresh(second, TokenRequest_Telemetry.OFF))).toBe(false);
    expect(await at(startMs + 30 * 24 * HOUR_MS, () => refresh(owner, TokenRequest_Telemetry.ON))).toBe(false);
    expect(await at(startMs + 30 * 24 * HOUR_MS, () => runInDurableObject(account(), (instance: AccountDO) => instance.revokeDevice(second.deviceId)))).toBe(true);
  });

  it("reads the same rows for a session check however many devices the server has revoked", async () => {
    const owner = await ownerOnce();
    await refresh(owner, TokenRequest_Telemetry.ON);
    const rowsRead = () =>
      runInDurableObject(account(), async (instance: AccountDO, state) => {
        const sql = state.storage.sql;
        const exec = sql.exec.bind(sql);
        const cursors: { rowsRead: number }[] = [];
        const spy = vi.spyOn(sql, "exec").mockImplementation((query, ...bindings) => {
          const cursor = exec(query, ...bindings);
          cursors.push(cursor);
          return cursor;
        });
        try {
          await instance.checkSession(owner.deviceId);
        } finally {
          spy.mockRestore();
        }
        return cursors.reduce((sum, cursor) => sum + cursor.rowsRead, 0);
      });
    const before = await rowsRead();
    await runInDurableObject(account(), (_instance, state) => {
      for (let n = 0; n < 500; n++) {
        state.storage.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, issued_by, list_seq, revoked_at_ms) SELECT ?, user_id, 'old', 'linux', 1, x'00', NULL, 100000 + ?, 1 FROM devices LIMIT 1",
          "revoked-" + n,
          n,
        );
      }
    });
    expect(await rowsRead()).toBe(before);
  });

  it("keeps an hour whose send failed for the next hour, and drops it after a day", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const tries: number[] = [];
    let nowMs = (hour + 2) * HOUR_MS;
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, (hour + 2) * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      await instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
      instance.clock = () => nowMs;
      instance.sendReport = async () => {
        tries.push(nowMs);
        throw new Error("network down");
      };
    });
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runDurableObjectAlarm(account());
    expect(tries).toHaveLength(1);
    nowMs += HOUR_MS / 2;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(tries).toHaveLength(1);
    nowMs = (hour + 25) * HOUR_MS;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(tries).toHaveLength(2);
    expect(await storedHours()).toEqual([]);
  });
});
