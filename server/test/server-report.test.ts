// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
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
import { ClientMessageSchema, RpcRequestSchema, type ServerMessage, ServerMessageSchema } from "../src/gen/koidrive/v1/messages_pb";
import { reportBatch } from "../src/report/payload";
import type { SpaceDO } from "../src/space-do";
import { client, newSpace } from "./helpers";
import { emptyUsage, LATENCY_BUCKETS, latencyBucket, mergeUsage, QUIET_MS, type Usage, type UsageHours, UsageTally } from "../src/report/usage";

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

const restorers: (() => unknown)[] = [];
const spaceStub = (spaceId: string) => env.SPACE.get(env.SPACE.idFromName(spaceId));
type SpaceStub = ReturnType<typeof spaceStub>;
const nextHour = (ms: number) => (Math.floor(ms / HOUR_MS) + 1) * HOUR_MS;
const spaceCalls = (calls: UsageHours[]) => calls.filter((hours) => Object.values(hours).some((usage) => Object.keys(usage.counts).some((key) => key.startsWith("rpc_"))));
const listed = (calls: UsageHours[]) => calls.flatMap((hours) => Object.values(hours)).reduce((sum, usage) => sum + (usage.counts["rpc_list_tree.ok"] ?? 0), 0);
const usageDueMs = (stub: SpaceStub) =>
  runInDurableObject(stub, (_instance, state) => {
    const row = state.storage.sql.exec("SELECT due_ms FROM schedule WHERE task = 'usage'").toArray()[0];
    return row ? Number(row.due_ms) : undefined;
  });
const attachments = (stub: SpaceStub) => runInDurableObject(stub, (_instance, state) => state.getWebSockets().map((ws) => ws.deserializeAttachment() as Record<string, unknown>));
const parkedCounts = async (stub: SpaceStub) =>
  (await attachments(stub)).map((attachment) => listed([((attachment.usage as { hours: UsageHours } | undefined) ?? { hours: {} }).hours]));

async function reportingSpace(telemetry = TokenRequest_Telemetry.ON) {
  const owner = await ownerOnce();
  await runInDurableObject(account(), (instance: AccountDO, state) => {
    instance.clock = () => Date.now();
    state.storage.sql.exec("DELETE FROM usage_hours");
  });
  await refresh(owner, telemetry);
  expect(await reporting()).toBe(telemetry === TokenRequest_Telemetry.ON);
  const created = await post("/v1/spaces", body(CreateSpaceRequestSchema, { name: "Usage " + crypto.randomUUID() }), owner.access!.token, SpaceInfoSchema);
  const spaceId = created.message!.spaceId;
  return { owner, spaceId, stub: spaceStub(spaceId) };
}

async function watchUsage() {
  const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
  const original = prototype.recordUsage;
  const calls: UsageHours[] = [];
  const failures = { left: 0 };
  prototype.recordUsage = async function (this: AccountDO, hours: UsageHours) {
    calls.push(hours);
    if (failures.left > 0) {
      failures.left--;
      throw new Error("account unreachable");
    }
    return original.call(this, hours);
  };
  restorers.push(() => {
    prototype.recordUsage = original;
  });
  return { calls, failures };
}

async function useClock(stub: SpaceStub, now: () => number) {
  await runInDurableObject(account(), (instance: AccountDO) => {
    instance.clock = now;
    instance.sendReport = async () => false;
  });
  await runInDurableObject(stub, (instance: SpaceDO) => {
    instance.clock = now;
  });
  restorers.push(() =>
    runInDurableObject(account(), (instance: AccountDO) => {
      instance.clock = () => Date.now();
    }),
  );
}

async function connect(spaceId: string, device: DeviceCredentials, stub: SpaceStub) {
  const response = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/ws`, { headers: { Upgrade: "websocket", Authorization: "Bearer " + device.access!.token } });
  const ws = response.webSocket!;
  ws.binaryType = "arraybuffer";
  const inbox: ServerMessage["msg"][] = [];
  const waiting: ((msg: ServerMessage["msg"]) => void)[] = [];
  ws.addEventListener("message", (event) => {
    const msg = fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer)).msg;
    const take = waiting.shift();
    if (take) take(msg);
    else inbox.push(msg);
  });
  ws.accept();
  const send = (msg: MessageInitShape<typeof ClientMessageSchema>["msg"]) => ws.send(toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg } as MessageInitShape<typeof ClientMessageSchema>)));
  const next = () => (inbox.length > 0 ? Promise.resolve(inbox.shift()!) : new Promise<ServerMessage["msg"]>((resolve) => waiting.push(resolve)));
  send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
  expect((await next()).case).toBe("welcome");
  await runInDurableObject(stub, (_instance, state) => {
    for (const socket of state.getWebSockets()) {
      const attachment = socket.deserializeAttachment() as { identity: { expiresAtMs: number } };
      attachment.identity.expiresAtMs = Date.now() + 365 * 24 * HOUR_MS;
      socket.serializeAttachment(attachment);
    }
  });
  let reqId = 0n;
  return {
    ws,
    async listTree() {
      send({ case: "rpc", value: { reqId: ++reqId, call: { case: "listTree", value: {} } } });
      const msg = await next();
      expect(msg.case === "rpc" && msg.value.result.case).toBe("tree");
    },
  };
}

afterEach(async () => {
  for (const restore of restorers.splice(0)) await restore();
  vi.restoreAllMocks();
});

describe("usage tally", () => {
  it("buckets latencies by powers of two", () => {
    expect([0, 0.5, 1, 2, 3, 4, 1000, 16_384, 1e9].map(latencyBucket)).toEqual([0, 0, 1, 2, 2, 3, 10, 15, 15]);
  });

  it("hands the counts over whole, once, and counts nothing once the server said it does not report", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    const handed: UsageHours[] = [];
    const record = async (hours: UsageHours) => {
      handed.push(hours);
      return true;
    };
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    tally.count("get_server", "200", 3, 10 * HOUR_MS + 1);
    await tally.handOver(record);
    expect(handed).toEqual([{ 10: { counts: { "get_server.200": 2 }, latency: { get_server: [0, 0, 2, ...new Array(LATENCY_BUCKETS - 3).fill(0)] } } }]);
    await tally.handOver(record);
    expect(handed).toHaveLength(1);
    tally.count("get_server", "200", 3, 11 * HOUR_MS);
    await tally.handOver(async () => false);
    tally.count("get_server", "200", 3, 11 * HOUR_MS);
    await tally.handOver(record);
    expect(handed).toHaveLength(1);
  });

  it("hands a busy process's counts over once every 10 minutes and when an hour ends, and an idle one's 25 seconds after its last count, and never more than once per request", async () => {
    UsageTally.quietMs = QUIET_MS;
    let nowMs = 10 * HOUR_MS;
    const timers: { atMs: number; fire: () => void }[] = [];
    const wait = (ms: number) => new Promise<void>((fire) => timers.push({ atMs: nowMs + ms, fire }));
    const flush = async () => {
      for (let tick = 0; tick < 20; tick++) await Promise.resolve();
    };
    const runUntil = async (endMs: number) => {
      for (;;) {
        timers.sort((one, two) => one.atMs - two.atMs);
        const next = timers[0];
        if (!next || next.atMs > endMs) break;
        timers.shift();
        nowMs = next.atMs;
        next.fire();
        await flush();
      }
      nowMs = endMs;
    };
    const tally = new UsageTally();
    tally.reporting = true;
    const handed: { atMs: number; counts: number }[] = [];
    const record = async (hours: UsageHours) => {
      handed.push({ atMs: nowMs, counts: Object.values(hours).reduce((sum, usage) => sum + (usage.counts["get_server.200"] ?? 0), 0) });
      return true;
    };
    const request = async (atMs: number) => {
      await runUntil(atMs);
      tally.count("get_server", "200", 3, nowMs);
      void tally.settle(record, () => nowMs, wait);
      await flush();
    };

    for (let second = 0; second < 2 * 3600; second++) await request(10 * HOUR_MS + second * 1000);
    await runUntil(13 * HOUR_MS);
    expect(handed.reduce((sum, one) => sum + one.counts, 0)).toBe(2 * 3600);
    expect(handed).toHaveLength(12);
    expect(handed.at(-1)!.atMs).toBeLessThanOrEqual(12 * HOUR_MS - 1000 + QUIET_MS);
    expect(handed.filter((one) => one.atMs <= 11 * HOUR_MS).reduce((sum, one) => sum + one.counts, 0)).toBe(3600);

    handed.length = 0;
    for (let burst = 0; burst < 5; burst++) {
      for (let second = 0; second < 10; second++) await request(13 * HOUR_MS + burst * 5 * 60_000 + second * 1000);
    }
    await runUntil(14 * HOUR_MS);
    expect(handed.map((one) => one.counts)).toEqual([10, 10, 10, 10, 10]);

    handed.length = 0;
    for (let n = 0; n < 120; n++) await request(14 * HOUR_MS + n * 30_000);
    await runUntil(15 * HOUR_MS);
    expect(handed).toHaveLength(120);
    expect(handed.every((one) => one.counts === 1)).toBe(true);

    handed.length = 0;
    for (let n = 0; n < 180; n++) await request(15 * HOUR_MS + n * 20_000);
    await runUntil(16 * HOUR_MS + HOUR_MS);
    expect(handed.reduce((sum, one) => sum + one.counts, 0)).toBe(180);
    expect(handed.length).toBeLessThanOrEqual(7);
  });

  it("counts nothing until it knows the server reports, and nothing once it knows it doesn't", async () => {
    const tally = new UsageTally();
    const record = vi.fn(async () => true);
    const wait = vi.fn(async () => {});
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    await tally.settle(record, () => 11 * HOUR_MS, wait);
    expect(wait).not.toHaveBeenCalled();
    tally.reporting = true;
    await tally.settle(record, () => 11 * HOUR_MS, wait);
    expect(record).not.toHaveBeenCalled();
    tally.count("get_server", "503", 3, 11 * HOUR_MS);
    await tally.settle(record, () => 12 * HOUR_MS, wait);
    expect(record.mock.calls).toEqual([[{ 11: { counts: { "get_server.503": 1 }, latency: { get_server: [0, 0, 1, ...new Array(LATENCY_BUCKETS - 3).fill(0)] } } }]]);
    tally.reporting = false;
    tally.count("get_server", "200", 3, 12 * HOUR_MS);
    tally.reporting = true;
    await tally.settle(record, () => 13 * HOUR_MS, wait);
    expect(record).toHaveBeenCalledTimes(1);
  });

  it("drops the counts it holds once the server says it stopped reporting", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
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
    tally.reporting = true;
    const handed: UsageHours[] = [];
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    await tally.handOver(async () => {
      tally.count("get_server", "503", 3, 10 * HOUR_MS);
      throw new Error("account unreachable");
    });
    await tally.handOver(async (hours) => {
      handed.push(hours);
      return true;
    });
    expect(handed.map((hours) => hours[10]!.counts)).toEqual([{ "get_server.200": 1, "get_server.503": 1 }]);
  });

  it("drops a failed hand-over's counts when reporting stopped and started again while it ran", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const tally = new UsageTally();
    tally.reporting = true;
    const handed: UsageHours[] = [];
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    await tally.handOver(async () => {
      tally.reporting = false;
      tally.reporting = true;
      throw new Error("account unreachable");
    });
    tally.count("get_server", "201", 3, 10 * HOUR_MS);
    await tally.handOver(async (hours) => {
      handed.push(hours);
      return true;
    });
    expect(handed.map((hours) => hours[10]!.counts)).toEqual([{ "get_server.201": 1 }]);
  });

  it("keeps a newer OFF answer when an older hand-over answers ON late", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    await tally.handOver(async () => {
      tally.reporting = false;
      return true;
    });
    expect(tally.reporting).toBe(false);
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

  it("parks a Space DO's counts on its socket across a lost memory and hands each count over once, when its batch is due and when the hour alarm runs", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    let nowMs = Math.floor(Date.now() / HOUR_MS) * HOUR_MS + 60_000;
    await useClock(stub, () => nowMs);
    const socket = await connect(spaceId, owner, stub);
    const openedMs = nowMs;
    for (let n = 0; n < 3; n++, nowMs += 60_000) await socket.listTree();
    await vi.waitFor(async () => expect(await usageDueMs(stub)).toBe(nextHour(openedMs) + 60_000));
    expect(spaceCalls(usage.calls)).toEqual([]);

    await evictDurableObject(stub);
    await useClock(stub, () => nowMs);
    for (let n = 0; n < 2; n++, nowMs += 60_000) await socket.listTree();
    nowMs = openedMs + 10 * 60_000;
    await socket.listTree();
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(6));
    expect(spaceCalls(usage.calls)).toHaveLength(1);

    for (let n = 0; n < 4; n++, nowMs += 60_000) await socket.listTree();
    await evictDurableObject(stub);
    nowMs = (await usageDueMs(stub))!;
    await useClock(stub, () => nowMs);
    await runInDurableObject(stub, (instance: SpaceDO) => instance.alarm());
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(10));
    expect(spaceCalls(usage.calls)).toHaveLength(2);
    expect((await storedCounts())["rpc_list_tree.ok"]).toBe(10);
    expect(await usageDueMs(stub)).toBeUndefined();
  });

  it("hands a busy socket's counts over about six times an hour, and every count arrives", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    let nowMs = Math.floor(Date.now() / HOUR_MS) * HOUR_MS + 60_000;
    await useClock(stub, () => nowMs);
    const socket = await connect(spaceId, owner, stub);
    for (let minute = 0; minute < 120; minute++, nowMs += 60_000) {
      const dueMs = await usageDueMs(stub);
      if (dueMs !== undefined && dueMs <= nowMs) await runInDurableObject(stub, (instance: SpaceDO) => instance.alarm());
      await socket.listTree();
    }
    nowMs = (await usageDueMs(stub))!;
    await runInDurableObject(stub, (instance: SpaceDO) => instance.alarm());
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(120));
    expect((await storedCounts())["rpc_list_tree.ok"]).toBe(120);
    expect(spaceCalls(usage.calls).length).toBeLessThanOrEqual(2 * 7 + 1);
  });

  it("passes a closing socket's parked counts to another open socket, and hands them over when the last one closes", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    const first = await connect(spaceId, owner, stub);
    const second = await connect(spaceId, owner, stub);
    await first.listTree();
    await first.listTree();
    await second.listTree();
    first.ws.close(1000, "done");
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([3]));
    expect(spaceCalls(usage.calls)).toEqual([]);
    second.ws.close(1000, "done");
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(3));
    expect(spaceCalls(usage.calls)).toHaveLength(1);
    expect((await storedCounts())["rpc_list_tree.ok"]).toBe(3);
  });

  it("parks the counts of a failed hand-over again and hands them over once with the next one", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    let nowMs = Math.floor(Date.now() / HOUR_MS) * HOUR_MS + 60_000;
    await useClock(stub, () => nowMs);
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    usage.failures.left = 1;
    nowMs += 10 * 60_000;
    await socket.listTree();
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([2]));
    await socket.listTree();
    await vi.waitFor(() => expect(spaceCalls(usage.calls)).toHaveLength(2));
    expect((await storedCounts())["rpc_list_tree.ok"]).toBe(3);
    expect(await parkedCounts(stub)).toEqual([0]);
  });

  it("hands everything over instead of parking past 8 KiB on one socket", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    const socket = await connect(spaceId, owner, stub);
    await runInDurableObject(stub, (_instance, state) => {
      const ws = state.getWebSockets()[0]!;
      const attachment = ws.deserializeAttachment() as { usage?: { openedMs: number; hours: UsageHours } };
      const counts: Record<string, number> = {};
      for (let n = 0; JSON.stringify({ ...attachment, usage: { openedMs: Date.now(), hours: { [Math.floor(Date.now() / HOUR_MS)]: { counts, latency: {} } } } }).length < 8150; n++) counts["rpc_pad_" + n + ".ok"] = 1;
      ws.serializeAttachment({ ...attachment, usage: { openedMs: Date.now(), hours: { [Math.floor(Date.now() / HOUR_MS)]: { counts, latency: {} } } } });
    });
    await socket.listTree();
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(1));
    expect(Object.keys(spaceCalls(usage.calls)[0]![Math.floor(Date.now() / HOUR_MS)]!.counts).length).toBeGreaterThan(100);
    expect(await parkedCounts(stub)).toEqual([0]);
    await socket.listTree();
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([1]));
    expect(spaceCalls(usage.calls)).toHaveLength(1);
  });

  it("counts an HTTP RPC the Space DO refuses before it runs, by kind and refusal code", async () => {
    const { stub } = await reportingSpace();
    const identity = { userId: crypto.randomUUID(), deviceId: crypto.randomUUID(), deviceName: "Stranger", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const { counted } = await stub.httpRpc({ identity, origin: BASE }, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }));
    expect({ route: counted.route, outcome: counted.outcome }).toEqual({ route: "rpc_list_tree", outcome: "forbidden" });
  });

  it("counts an HTTP RPC in the Worker's batch, so a Space DO with no socket open neither calls nor arms anything for it", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    for (let reqId = 1n; reqId <= 3n; reqId++) {
      expect((await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token)).status).toBe(200);
    }
    await vi.waitFor(async () => expect((await storedCounts())["rpc_list_tree.ok"]).toBe(3));
    expect(await usageDueMs(stub)).toBeUndefined();
  });

  it("arms nothing and calls nothing while a Space DO doesn't know whether the server reports", async () => {
    const usage = await watchUsage();
    const space = await newSpace();
    const laptop = await client(space, "Laptop");
    for (let n = 0; n < 3; n++) expect((await laptop.call({ case: "listTree", value: {} })).case).toBe("tree");
    expect(await usageDueMs(space.stub)).toBeUndefined();
    expect(spaceCalls(usage.calls)).toEqual([]);
  });

  it("drops what a Space DO parked once the Account DO answers that the server stopped reporting, and parks nothing after", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    await refresh(owner, TokenRequest_Telemetry.OFF);
    UsageTally.batchMs = 0;
    await socket.listTree();
    await vi.waitFor(() => expect(spaceCalls(usage.calls)).toHaveLength(1));
    await socket.listTree();
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([0]));
    expect(spaceCalls(usage.calls)).toHaveLength(1);
    expect((await storedCounts())["rpc_list_tree.ok"]).toBeUndefined();
  });

  it("drops what a Space DO parked as soon as a session check says reports are off, with no RPC after it", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    expect(await parkedCounts(stub)).toEqual([1]);
    await runInDurableObject(stub, (instance: object) => {
      const space = instance as unknown as { leases: Map<string, { check: object }>; storeLease(deviceId: string, check: object, askedAtMs: number): void };
      space.storeLease(owner.deviceId, { ...space.leases.get(owner.deviceId)!.check, reporting: false }, Date.now() + 1);
    });
    expect(await parkedCounts(stub)).toEqual([0]);
  });

  it("calls nothing, arms nothing and leaves the sockets as they were while reports are off, in the Worker and in a Space DO", async () => {
    const { owner, spaceId, stub } = await reportingSpace(TokenRequest_Telemetry.OFF);
    const usage = await watchUsage();
    const socket = await connect(spaceId, owner, stub);
    const before = await attachments(stub);
    await socket.listTree();
    await evictDurableObject(stub);
    await socket.listTree();
    expect((await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId: 9n, call: { case: "listTree", value: {} } }), owner.access!.token)).status).toBe(200);
    expect(await attachments(stub)).toEqual(before);
    socket.ws.close(1000, "done");
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(usage.calls).toEqual([]);
    expect(await usageDueMs(stub)).toBeUndefined();
    expect(before[0]).not.toHaveProperty("usage");
  });

  it("drops the stored hours without sending them once a device turns reports off, even when reports turn on again before the send", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: unknown[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, (hour + 2) * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      await instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
      state.storage.sql.exec("UPDATE devices SET telemetry = ? WHERE rowid = (SELECT MIN(rowid) FROM devices)", TokenRequest_Telemetry.OFF);
      countVotes(instance);
      expect(state.storage.sql.exec("SELECT hour FROM usage_hours").toArray()).toEqual([]);
      state.storage.sql.exec("UPDATE devices SET telemetry = ?", TokenRequest_Telemetry.ON);
      countVotes(instance);
      instance.clock = () => (hour + 2) * HOUR_MS;
      instance.sendReport = async (report) => {
        sent.push(report);
        return true;
      };
    });
    await runDurableObjectAlarm(account());
    expect(sent).toEqual([]);
    expect(await storedHours()).toEqual([]);
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

  it("ends an ON vote at the first full hour 24 hours after its device's last refresh, and moves that end with each refresh", async () => {
    const owner = await ownerOnce();
    const at = async (clockMs: number, step: () => Promise<unknown>) => {
      await runInDurableObject(account(), (instance: AccountDO) => {
        instance.clock = () => clockMs;
      });
      await step();
      return reporting();
    };
    await runInDurableObject(account(), (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?", TokenRequest_Telemetry.UNSPECIFIED);
      countVotes(instance);
    });
    const hourMs = (Math.floor(Date.now() / HOUR_MS) + 1) * HOUR_MS;
    expect(await at(hourMs + 15 * 60_000, () => refresh(owner, TokenRequest_Telemetry.ON))).toBe(true);
    expect(await at(hourMs + 25 * HOUR_MS - 1, async () => {})).toBe(true);
    expect(await at(hourMs + 25 * HOUR_MS, async () => {})).toBe(false);
    expect(await at(hourMs + 24 * HOUR_MS + 5 * 60_000, () => refresh(owner, TokenRequest_Telemetry.ON))).toBe(true);
    expect(await at(hourMs + 49 * HOUR_MS - 1, async () => {})).toBe(true);
    expect(await at(hourMs + 49 * HOUR_MS, async () => {})).toBe(false);
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

  it("never sends an hour older than a day, even one a failed send kept", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: unknown[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, hour * HOUR_MS);
      countVotes(instance);
      state.storage.sql.exec("INSERT INTO usage_hours (hour, report_id, usage, tried_at_ms) VALUES (?, ?, ?, ?)", hour - 30, crypto.randomUUID(), JSON.stringify({ counts: { "get_server.200": 1 }, latency: {} }), (hour - 2) * HOUR_MS);
      instance.clock = () => hour * HOUR_MS + 60_000;
      instance.sendReport = async (report) => {
        sent.push(report);
        return true;
      };
      await instance.alarm();
    });
    expect(sent).toEqual([]);
    expect(await storedHours()).toEqual([]);
  });

  it("keeps a batch it stored once even when arming the report alarm fails", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const stored = await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, hour * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      vi.spyOn(state.storage, "setAlarm").mockRejectedValue(new Error("storage busy"));
      return instance.recordUsage({ [hour]: { counts: { "get_server.200": 1 }, latency: {} } });
    });
    expect(stored).toBe(true);
    expect(await storedCounts()).toEqual({ "get_server.200": 1 });
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
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
    nowMs = (hour + 3) * HOUR_MS;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(tries).toHaveLength(2);
    nowMs = (hour + 25) * HOUR_MS;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(tries).toHaveLength(2);
    expect(await storedHours()).toEqual([]);
  });
});
