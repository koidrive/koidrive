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
import { ClientMessageSchema, RpcRequestSchema, RpcResponseSchema, type ServerMessage, ServerMessageSchema } from "../src/gen/koidrive/v1/messages_pb";
import { ChunkBundleResultSchema, ChunkBundleSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { estimateMonthlyCost } from "../src/report/cost";
import { countedBucket, countedEgress, countedState, meteredObject, meteredReply, meteredStub } from "../src/report/meter";
import { reportBatch } from "../src/report/payload";
import type { SpaceDO } from "../src/space-do";
import { uuidv7 } from "../src/ids";
import { client, content, newSpace, op } from "./helpers";
import { CreateShareLinkRequestSchema, ShareLinkSchema } from "../src/gen/koidrive/v1/share_pb";
import {
  BATCH_MS,
  type Carried,
  COUNTERS,
  emptyUsage,
  GAUGES,
  LATENCY_BUCKETS,
  latencyBucket,
  mergeUsage,
  type ObjectStored,
  QUIET_MS,
  type Stored,
  type Usage,
  type UsageBatch,
  type UsageHours,
  UsageTally,
} from "../src/report/usage";

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
const storedTotal = async () => (await storedHours()).map((row) => JSON.parse(String(row.usage)) as Usage).reduce((sum, usage) => mergeUsage(sum, usage), emptyUsage());
const storedCounts = async () => (await storedTotal()).counts;
const storedObjects = () => runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT * FROM object_usage").toArray());
const billingOf = (hours: UsageHours) => Object.values(hours).reduce((sum, usage) => mergeUsage(sum, usage), emptyUsage()).billing;
const handedOver = async (tally: UsageTally) => {
  let handed: UsageHours = {};
  await tally.handOver(async ({ hours }) => {
    handed = hours;
    return true;
  });
  return handed;
};

const restorers: (() => unknown)[] = [];
const startReporting = (instance: AccountDO, state: DurableObjectState, atMs: number, seenMs = atMs) => {
  instance.clock = () => atMs;
  state.storage.sql.exec("UPDATE devices SET telemetry = ?", TokenRequest_Telemetry.UNSPECIFIED);
  countVotes(instance);
  state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, seenMs);
  countVotes(instance);
};
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
  const stored: ObjectStored[] = [];
  const failures = { left: 0 };
  const delay = { ms: 0 };
  const hooks = { entered: () => {} };
  prototype.recordUsage = async function (this: AccountDO, batch: UsageBatch) {
    hooks.entered();
    calls.push(batch.hours);
    stored.push(...(batch.stored ?? []));
    if (delay.ms > 0) await scheduler.wait(delay.ms);
    if (failures.left > 0) {
      failures.left--;
      throw new Error("account unreachable");
    }
    return original.call(this, batch);
  };
  restorers.push(() => {
    prototype.recordUsage = original;
  });
  return { calls, failures, stored, delay, hooks };
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
    const record = async ({ hours }: UsageBatch) => {
      handed.push(hours);
      return true;
    };
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    tally.count("get_server", "200", 3, 10 * HOUR_MS + 1);
    await tally.handOver(record);
    expect(handed).toEqual([{ 10: { counts: { "get_server.200": 2 }, latency: { get_server: [0, 0, 2, ...new Array(LATENCY_BUCKETS - 3).fill(0)] }, billing: {} } }]);
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
    const record = async ({ hours }: UsageBatch) => {
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

  it("counts while it doesn't know whether the server reports, hands nothing over until it knows, and lets only the Account DO turn a known flag", async () => {
    const tally = new UsageTally();
    const record = vi.fn(async () => true);
    const wait = vi.fn(async () => {});
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    await tally.settle(record, () => 11 * HOUR_MS, wait);
    expect(wait).not.toHaveBeenCalled();
    expect(record).not.toHaveBeenCalled();
    tally.receive({ reporting: true }, false);
    await tally.settle(record, () => 11 * HOUR_MS, wait);
    expect(record.mock.calls).toEqual([[{ openedMs: 10 * HOUR_MS, hours: { 10: { counts: { "get_server.200": 1 }, latency: { get_server: [0, 0, 1, ...new Array(LATENCY_BUCKETS - 3).fill(0)] }, billing: {} } } }]]);
    tally.count("get_server", "503", 3, 11 * HOUR_MS);
    tally.receive({ reporting: false }, false);
    expect(tally.reporting).toBe(true);
    tally.receive({ reporting: false }, true);
    tally.receive({ reporting: true }, false);
    expect(tally.reporting).toBe(false);
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
    await tally.handOver(async ({ hours }) => {
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
    await tally.handOver(async ({ hours }) => {
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
    await tally.handOver(async ({ hours }) => {
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

  it("sums billing counters within an hour, skips zeros and merges them like the counts", async () => {
    let nowMs = 10 * HOUR_MS;
    const tally = new UsageTally(() => nowMs);
    tally.reporting = true;
    tally.add("worker_requests", 1);
    nowMs += 5;
    tally.add("worker_requests", 1);
    tally.add("egress_bytes", 12);
    tally.add("egress_bytes", 0);
    nowMs = 11 * HOUR_MS;
    tally.add("do_rows_read", 7);
    const hours = await handedOver(tally);
    expect(hours[10]!.billing).toEqual({ worker_requests: 2, egress_bytes: 12 });
    expect(hours[11]!.billing).toEqual({ do_rows_read: 7 });
    expect(mergeUsage(hours[10]!, { counts: {}, latency: {}, billing: { worker_requests: 3, do_rows_read: 1 } }).billing).toEqual({ worker_requests: 5, egress_bytes: 12, do_rows_read: 1 });
    tally.reporting = false;
    tally.add("worker_requests", 1);
    tally.reporting = true;
    expect(await handedOver(tally)).toEqual({});
  });
});

describe("billing meters", () => {
  it("counts R2 operations by class and leaves deletes and aborts out", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    const bucket = countedBucket(env.BUCKET, tally);
    await bucket.put("meter/a", "one");
    await (await bucket.get("meter/a"))!.text();
    await bucket.head("meter/a");
    await bucket.list({ prefix: "meter/" });
    await bucket.delete("meter/a");
    const upload = await bucket.createMultipartUpload("meter/b");
    const part = await upload.uploadPart(1, "two");
    await upload.complete([part]);
    await bucket.resumeMultipartUpload("meter/c", (await bucket.createMultipartUpload("meter/c")).uploadId).abort();
    await bucket.delete("meter/b");
    expect(billingOf(await handedOver(tally))).toEqual({ r2_class_a: 6, r2_class_b: 2 });
  });

  it("counts every row each statement reads and writes, however the cursor is consumed", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    const expected = { read: 0, written: 0 };
    await runInDurableObject(env.ACCOUNT.get(env.ACCOUNT.idFromName("meter-rows")), (_instance, state) => {
      const sql = countedState(state, tally).storage.sql;
      const statements = [
        sql.exec("CREATE TABLE probe (n INTEGER PRIMARY KEY, label TEXT)"),
        sql.exec("INSERT INTO probe (n, label) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')"),
        sql.exec("UPDATE probe SET label = 'z' WHERE n > 2"),
      ];
      const all = sql.exec("SELECT * FROM probe");
      expect(all.toArray()).toHaveLength(4);
      const first = sql.exec("SELECT * FROM probe ORDER BY n");
      for (const row of first) if (row.n === 2) break;
      const one = sql.exec("SELECT COUNT(*) AS rows FROM probe");
      expect(one.one().rows).toBe(4);
      const deleted = sql.exec("DELETE FROM probe");
      for (const cursor of [...statements, all, first, one, deleted]) {
        expected.read += cursor.rowsRead;
        expected.written += cursor.rowsWritten;
      }
    });
    expect(expected.read).toBeGreaterThan(0);
    expect(expected.written).toBeGreaterThanOrEqual(10);
    const billing = billingOf(await handedOver(tally));
    expect(billing).toEqual({ do_rows_read: expected.read, do_rows_written: expected.written });
  });

  it("counts each call into an object once, WebSocket messages apart, returns a call's counts and active time in its reply, gives each event nobody waits on its counts, and sends a failed call's or a fetch's counts with the next reply", async () => {
    class Probe {
      async rpc(gate: Promise<void>): Promise<string> {
        await gate;
        return this.helper();
      }
      helper(): string {
        return "done";
      }
      async alarm(): Promise<void> {}
      async fetch(): Promise<Response> {
        return new Response(null, { status: 204 });
      }
      async webSocketMessage(): Promise<void> {}
      async webSocketClose(): Promise<void> {}
      fail(): never {
        throw new Error("refused");
      }
    }
    const tally = new UsageTally(() => 10 * HOUR_MS, "do_space_active_ms");
    tally.reporting = true;
    const ended: [string, boolean, UsageHours][] = [];
    const settle = (event: string, _args: unknown[], waited: boolean) => {
      ended.push([event, waited, waited ? {} : (tally.take()?.hours ?? {})]);
    };
    const probe = meteredObject(new Probe(), "space", tally, settle);
    const call = (method: string, args: unknown[] = []) => meteredReply(probe, "space", tally, settle, method, args);
    let nowMs = 10 * HOUR_MS;
    vi.spyOn(Date, "now").mockImplementation(() => nowMs);
    let open!: () => void;
    const first = call("rpc", [new Promise<void>((resolve) => (open = resolve))]);
    nowMs += 300;
    const second = await call("rpc", [Promise.resolve()]);
    expect({ value: second.value, reporting: second.reporting, billing: billingOf(second.usage!.hours) }).toEqual({ value: "done", reporting: true, billing: { do_space_requests: 2, do_space_active_ms: 300 } });
    nowMs += 700;
    open();
    expect(billingOf((await first).usage!.hours)).toEqual({ do_space_active_ms: 700 });
    await probe.alarm();
    await probe.webSocketMessage();
    await probe.webSocketClose();
    await expect(call("fail")).rejects.toThrow("refused");
    expect((await probe.fetch()).status).toBe(204);
    const after = await call("helper");
    expect({ value: after.value, billing: billingOf(after.usage!.hours) }).toEqual({ value: "done", billing: { do_space_requests: 3 } });
    await expect(call("alarm")).rejects.toThrow("no method alarm");
    await expect(call("constructor")).rejects.toThrow("no method constructor");
    expect(probe.helper()).toBe("done");
    expect(tally.take()).toBeUndefined();
    expect(ended.map(([event, waited, hours]) => [event, waited, billingOf(hours)])).toEqual([
      ["rpc", true, {}],
      ["rpc", true, {}],
      ["alarm", false, { do_space_requests: 1 }],
      ["webSocketMessage", false, { do_ws_messages: 1 }],
      ["webSocketClose", false, {}],
      ["fail", true, {}],
      ["fetch", true, {}],
      ["helper", true, {}],
    ]);
  });

  it("unwraps each reply at the stub and gives the caller the callee's counts and flag", async () => {
    const usage = { openedMs: 0, hours: { 10: { counts: {}, latency: {}, billing: { do_space_requests: 1 } } } };
    const calls: [string, unknown[]][] = [];
    const received: Carried[] = [];
    const stub = meteredStub(
      {
        metered: async (method: string, args: unknown[]) => {
          calls.push([method, args]);
          return { value: method + args.join(","), usage, reporting: true };
        },
        id: "probe",
      } as unknown as { shareInfo(rootId: string, expiresAtMs: number): Promise<string>; id: string },
      (carried) => received.push(carried),
    );
    expect(await stub.shareInfo("root", 5)).toBe("shareInforoot,5");
    expect(stub.id).toBe("probe");
    expect(calls).toEqual([["shareInfo", ["root", 5]]]);
    expect(received).toEqual([{ usage, reporting: true }]);
  });

  it("counts response bytes from Content-Length, or as a stream passes when the length is unknown", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    expect(await countedEgress(new Response("abc", { headers: { "Content-Length": "3" } }), tally).text()).toBe("abc");
    expect(await countedEgress(new Response(new Blob(["hello", " world"]).stream()), tally).text()).toBe("hello world");
    expect(countedEgress(new Response(null, { status: 204 }), tally).status).toBe(204);
    const billing = billingOf(await handedOver(tally));
    expect(billing).toEqual({ egress_bytes: 14 });
  });
});

describe("report payload", () => {
  it("holds exactly these keys and only route and outcome names", () => {
    const usage = emptyUsage();
    usage.counts["put_chunk.503.retry_later.r2_rate_limited"] = 2;
    usage.counts["rpc_list_tree.ok"] = 5;
    usage.latency.put_chunk = new Array(LATENCY_BUCKETS).fill(1);
    for (const counter of COUNTERS) usage.billing[counter] = 3;
    const stored = Object.fromEntries(GAUGES.map((gauge) => [gauge, 4])) as Stored;
    const sent = reportBatch([{ hour: 490_000, id: "7d3c2a4e-0000-4000-8000-000000000001", usage, stored }], { version: "0.1.0", protoMin: 1, protoMax: 1 }) as {
      batch: { properties: Record<string, unknown> }[];
    };
    expect(Object.keys(sent).sort()).toEqual(["api_key", "batch", "historical_migration"]);
    expect(sent.batch).toHaveLength(1);
    expect(Object.keys(sent.batch[0]!).sort()).toEqual(["event", "properties", "timestamp", "uuid"]);
    expect(sent.batch[0]).toMatchObject({ event: "server_report", uuid: "7d3c2a4e-0000-4000-8000-000000000001", timestamp: "2025-11-24T16:00:00.000Z" });
    expect(Object.keys(sent.batch[0]!.properties).sort()).toEqual([
      "$geoip_disable",
      "$process_person_profile",
      "b.do_account_active_ms",
      "b.do_account_requests",
      "b.do_rows_read",
      "b.do_rows_written",
      "b.do_space_active_ms",
      "b.do_space_requests",
      "b.do_stored_bytes",
      "b.do_ws_messages",
      "b.egress_bytes",
      "b.r2_class_a",
      "b.r2_class_b",
      "b.r2_objects",
      "b.r2_stored_bytes",
      "b.worker_requests",
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
    expect(sent.batch[0]!.properties).toMatchObject({ "b.worker_requests": 3, "b.r2_class_a": 3, "b.do_stored_bytes": 4, "b.r2_objects": 4 });
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
    for (let reqId = 1n; reqId <= 2n; reqId++) {
      expect((await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token)).status).toBe(200);
    }
    await vi.waitFor(async () => {
      const usages = (await storedHours()).map((row) => JSON.parse(String(row.usage)) as Usage);
      expect(usages.flatMap((usage) => Object.keys(usage.counts))).toEqual(expect.arrayContaining(["post_spaces.201", "rpc_list_tree.ok"]));
      expect(usages.flatMap((usage) => Object.keys(usage.billing))).toEqual(
        expect.arrayContaining(["worker_requests", "egress_bytes", "do_account_requests", "do_space_requests", "do_rows_read", "do_rows_written"]),
      );
      expect((await storedObjects()).map((row) => row.object)).toContain(env.SPACE.idFromName(spaceId).toString());
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
    const objectIds = [env.SPACE.idFromName(spaceId).toString(), env.ACCOUNT.idFromName("account").toString()];
    for (const secret of [owner.deviceId, second.deviceId, spaceId, owner.userId, "Owner", "Laptop", "Holiday", owner.refreshCredential, ...objectIds]) {
      expect(text).not.toContain(secret);
    }
    const events = (sent[0] as { batch: { properties: Record<string, unknown> }[] }).batch;
    const billingKey = `b\\.(?:${[...COUNTERS, ...GAUGES].join("|")})`;
    const allowed = new RegExp(`^(?:\\$geoip_disable|\\$process_person_profile|distinct_id|schema|server_version|proto_min|proto_max|c\\.[a-z_]+\\.[a-z0-9_.]+|h\\.[a-z_]+|${billingKey})$`);
    for (const key of events.flatMap((event) => Object.keys(event.properties))) expect(key).toMatch(allowed);
    for (const event of events) expect(event.properties["b.do_stored_bytes"]).toBeGreaterThan(0);
    const kept = await storedHours();
    expect(kept.map((row) => Number(row.hour))).toEqual([Math.floor((startMs + 3 * HOUR_MS) / HOUR_MS)]);
    expect((JSON.parse(String(kept[0]!.usage)) as Usage).billing.do_account_requests).toBe(1);
  });

  it("parks a Space DO's counts on its socket across a lost memory and hands each count over once, when its batch is due and when the hour alarm runs", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    let nowMs = Math.floor(Date.now() / HOUR_MS) * HOUR_MS + 60_000;
    await runInDurableObject(account(), (instance: AccountDO, state) => startReporting(instance, state, nowMs));
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
    await runInDurableObject(account(), (instance: AccountDO, state) => startReporting(instance, state, nowMs));
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
    await runInDurableObject(account(), (instance: AccountDO, state) => startReporting(instance, state, nowMs));
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

  it("drops what a Space DO parked and stores its flag as soon as an Account DO answer says reports are off, with no RPC after it", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    expect(await parkedCounts(stub)).toEqual([1]);
    await runInDurableObject(stub, (instance: object) => (instance as unknown as { received(carried: Carried): void }).received({ reporting: false }));
    expect(await parkedCounts(stub)).toEqual([0]);
    await evictDurableObject(stub);
    expect(await runInDurableObject(stub, (instance: object) => (instance as unknown as { traffic: UsageTally }).traffic.reporting)).toBe(false);
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

  it("hands a Space DO's counts from each HTTP RPC back to the Worker in its reply, with the stored sizes, and calls and arms nothing itself", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    for (let reqId = 1n; reqId <= 3n; reqId++) {
      expect((await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token)).status).toBe(200);
    }
    await vi.waitFor(async () => expect((await storedTotal()).billing.do_space_requests).toBe(3));
    expect(usage.stored.map((object) => object.object)).toContain(stub.id.toString());
    expect(await usageDueMs(stub)).toBeUndefined();
    expect(usage.calls.filter((hours) => Object.values(hours).every((counted) => Object.keys(counted.counts).length === 0))).toEqual([]);
  });

  it("counts each chunk a bundle stores as one R2 Class A write, as it counts a chunk uploaded alone", async () => {
    const { owner, spaceId } = await reportingSpace();
    const file = await content("first bundled chunk", "second bundled chunk", "third bundled chunk", "a chunk uploaded alone");
    const planned = await post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId: 1n, call: { case: "prepareUpload", value: { chunks: file.content.chunks } } }), owner.access!.token, RpcResponseSchema);
    const result = planned.message!.result;
    if (result.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const targets = result.value.chunks.map((chunk) => chunk.target!.url);
    await vi.waitFor(async () => expect((await storedCounts())["rpc_prepare_upload.ok"]).toBe(1));
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));

    const bundled = body(ChunkBundleSchema, { chunks: targets.slice(0, 3).map((target, i) => ({ target, data: file.bytes[i]! })) });
    const stored = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/chunks`, {
      method: "POST",
      body: bundled,
      headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(bundled.length), Authorization: "Bearer " + owner.access!.token },
    });
    expect(fromBinary(ChunkBundleResultSchema, new Uint8Array(await stored.arrayBuffer())).results.map((each) => each.status)).toEqual([200, 200, 200]);
    const alone = await exports.default.fetch(targets[3]!, { method: "PUT", body: file.bytes[3]!, headers: { "Content-Length": String(file.bytes[3]!.length) } });
    expect(alone.status).toBe(200);
    await vi.waitFor(async () => expect((await storedTotal()).billing.r2_class_a).toBe(4));
  });

  it("counts a Worker request whose body arrives slowly as one request and prices no Worker time", async () => {
    const { owner, spaceId } = await reportingSpace();
    await vi.waitFor(async () => expect((await storedTotal()).billing.worker_requests).toBeGreaterThan(0));
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
    const sent = body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } });
    const slow = new ReadableStream<Uint8Array>({
      async pull(controller) {
        await new Promise((resolve) => setTimeout(resolve, 1500));
        controller.enqueue(sent);
        controller.close();
      },
    });
    const answered = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/rpc`, {
      method: "POST",
      headers: { "Content-Type": "application/x-protobuf", Authorization: "Bearer " + owner.access!.token },
      body: slow,
    });
    expect(answered.status).toBe(200);
    await vi.waitFor(async () => expect((await storedCounts())["rpc_list_tree.ok"]).toBe(1));
    const billing = (await storedTotal()).billing;
    expect(Object.keys(billing).filter((name) => name.startsWith("worker_"))).toEqual(["worker_requests"]);
    expect(Object.keys(estimateMonthlyCost({ hours: 1, billing, stored: { do_stored_bytes: 0, r2_stored_bytes: 0, r2_objects: 0 } }).lines).filter((line) => line.startsWith("worker_"))).toEqual(["worker_requests"]);
  });

  it("counts each setAlarm as the one row written Cloudflare bills for it, in a Space DO and in the Account DO", async () => {
    const { stub: space } = await reportingSpace();
    const hour = Math.floor(Date.now() / HOUR_MS);
    const objects: { stub: DurableObjectStub; tally: string; due: [string, ...(string | number)[]]; undo: string }[] = [
      { stub: space, tally: "traffic", due: ["INSERT INTO schedule (task, due_ms) VALUES ('probe', ?)", Date.now() + HOUR_MS], undo: "DELETE FROM schedule WHERE task = 'probe'" },
      {
        stub: account(),
        tally: "tally",
        due: ["INSERT OR IGNORE INTO usage_hours (hour, report_id, usage) VALUES (?, 'alarm-probe', ?)", hour, JSON.stringify(emptyUsage())],
        undo: "DELETE FROM usage_hours WHERE report_id = 'alarm-probe'",
      },
    ];
    for (const { stub, tally: name, due, undo } of objects) {
      const written = await runInDurableObject(stub, async (instance, state) => {
        state.storage.sql.exec(...due);
        const tally = (instance as unknown as Record<string, UsageTally>)[name]!;
        tally.reporting = true;
        await handedOver(tally);
        await (instance as unknown as { armAlarm(): Promise<void> }).armAlarm();
        const handed = await handedOver(tally);
        state.storage.sql.exec(undo);
        return billingOf(handed).do_rows_written;
      });
      expect(written).toBe(1);
    }
  });

  it("sets no timer, calls nothing and arms nothing when an alarm wakes a Space DO while reports are off", async () => {
    await reportingSpace(TokenRequest_Telemetry.OFF);
    const usage = await watchUsage();
    const timers = vi.spyOn(globalThis, "setTimeout");
    const stub = env.SPACE.get(env.SPACE.newUniqueId());
    await runInDurableObject(stub, (_instance, state) => state.storage.setAlarm(Date.now() + 1));
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await scheduler.wait(100);
    expect(timers.mock.calls.filter(([, ms]) => (ms ?? 0) > 0)).toEqual([]);
    expect(usage.calls).toEqual([]);
    expect(await usageDueMs(stub)).toBeUndefined();
  });

  it("counts the time a Space DO waits for its hand-over as active time, because the hand-over runs inside the event and no timer holds the object", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    usage.delay.ms = 300;
    const socket = await connect(spaceId, owner, stub);
    const timers = vi.spyOn(globalThis, "setTimeout");
    UsageTally.batchMs = 0;
    await socket.listTree();
    await vi.waitFor(() => expect(spaceCalls(usage.calls)).toHaveLength(1));
    const parkedActiveMs = async () => billingOf(((await attachments(stub))[0]!.usage as { hours: UsageHours } | undefined)?.hours ?? {}).do_space_active_ms ?? 0;
    await vi.waitFor(async () => expect(await parkedActiveMs()).toBeGreaterThanOrEqual(300));
    expect(timers.mock.calls.filter(([, ms]) => (ms ?? 0) > 0)).toEqual([]);
    usage.delay.ms = 0;
    socket.ws.close(1000, "done");
    await vi.waitFor(async () => expect((await storedTotal()).billing.do_space_active_ms).toBeGreaterThanOrEqual(300));
  });

  it("stores the Account DO's own counts with the row it already writes for another process's batch", async () => {
    await reportingSpace();
    const hour = Math.floor(Date.now() / HOUR_MS);
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
    const counted: Usage = { counts: { "get_server.200": 1 }, latency: {}, billing: {} };
    const replies: Carried[] = [];
    expect(await meteredStub(account(), (carried) => replies.push(carried)).recordUsage({ openedMs: Date.now(), hours: { [hour]: counted } })).toBe(true);
    const rows = await storedHours();
    expect(rows).toHaveLength(1);
    const billing = (JSON.parse(String(rows[0]!.usage)) as Usage).billing;
    expect(billing.do_account_requests).toBe(1);
    expect(billing.do_rows_read).toBeGreaterThan(0);
    expect(replies).toHaveLength(1);
    expect(replies[0]!.reporting).toBe(true);
    expect(billingOf(replies[0]!.usage!.hours).do_rows_written).toBeGreaterThanOrEqual(1);
    expect(billingOf(replies[0]!.usage!.hours).do_account_requests).toBeUndefined();
  });

  it("drops the stored hours without sending them once a device turns reports off, even when reports turn on again before the send", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: unknown[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      startReporting(instance, state, hour * HOUR_MS + 60_000, (hour + 2) * HOUR_MS);
      await instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
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
    expect((await storedHours()).map((row) => Number(row.hour))).toEqual([hour + 2]);
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
  });

  it("drops what the Worker counted before reports stopped, even when they start again before its hand-over", async () => {
    UsageTally.quietMs = 1500;
    restorers.push(() => {
      UsageTally.quietMs = QUIET_MS;
    });
    const owner = await ownerOnce();
    await runInDurableObject(account(), (instance: AccountDO, state) => {
      instance.clock = () => Date.now();
      state.storage.sql.exec("DELETE FROM usage_hours");
    });
    await refresh(owner, TokenRequest_Telemetry.ON);
    const listed = await exports.default.fetch(BASE + "/v1/spaces", { headers: { Accept: "application/x-protobuf", Authorization: "Bearer " + owner.access!.token } });
    await listed.arrayBuffer();
    expect(listed.status).toBe(200);
    await refresh(owner, TokenRequest_Telemetry.OFF);
    await refresh(owner, TokenRequest_Telemetry.ON);
    await new Promise((resolve) => setTimeout(resolve, 4000));
    expect(Object.keys(await storedCounts()).filter((key) => key.startsWith("get_spaces"))).toEqual([]);
  });

  it("drops the Worker's counts at the refresh that turns reports off, with no hand-over after it", async () => {
    UsageTally.quietMs = 1500;
    restorers.push(() => {
      UsageTally.quietMs = QUIET_MS;
    });
    const owner = await ownerOnce();
    await runInDurableObject(account(), (instance: AccountDO) => {
      instance.clock = () => Date.now();
    });
    await refresh(owner, TokenRequest_Telemetry.ON);
    const usage = await watchUsage();
    const listed = await exports.default.fetch(BASE + "/v1/spaces", { headers: { Accept: "application/x-protobuf", Authorization: "Bearer " + owner.access!.token } });
    await listed.arrayBuffer();
    await refresh(owner, TokenRequest_Telemetry.OFF);
    await new Promise((resolve) => setTimeout(resolve, 4000));
    expect(usage.calls).toEqual([]);
  });

  it("drops what a Space DO parked before reports stopped, even when they start again before it hands the counts over", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    await socket.listTree();
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([2]));
    await refresh(owner, TokenRequest_Telemetry.OFF);
    await refresh(owner, TokenRequest_Telemetry.ON);
    socket.ws.close(1000, "done");
    await vi.waitFor(() => expect(listed(spaceCalls(usage.calls))).toBe(2));
    expect((await storedCounts())["rpc_list_tree.ok"]).toBeUndefined();
  });

  it("sends one server_report an hour even when a process hands over that hour's counts after it was sent", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const sent: { batch: unknown[] }[] = [];
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      state.storage.sql.exec("UPDATE devices SET telemetry = ?, last_seen_ms = ?", TokenRequest_Telemetry.ON, hour * HOUR_MS);
      countVotes(instance);
      instance.clock = () => hour * HOUR_MS + 60_000;
      await instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
      instance.clock = () => (hour + 1) * HOUR_MS + 6 * 60_000;
      instance.sendReport = async (report) => {
        sent.push(report as { batch: unknown[] });
        return true;
      };
    });
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    await runInDurableObject(account(), async (instance: AccountDO) => {
      instance.clock = () => (hour + 1) * HOUR_MS + 20 * 60_000;
      await instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
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
    expect((await storedHours()).map((row) => Number(row.hour))).not.toContain(hour - 30);
  });

  it("keeps a batch it stored once even when arming the report alarm fails, and arms the alarm with the next batch", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const stored = await runInDurableObject(account(), async (instance: AccountDO, state) => {
      startReporting(instance, state, hour * HOUR_MS + 60_000, hour * HOUR_MS);
      vi.spyOn(state.storage, "setAlarm").mockRejectedValue(new Error("storage busy"));
      return instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
    });
    expect(stored).toBe(true);
    expect(await storedCounts()).toEqual({ "get_server.200": 1 });
    const alarm = await runInDurableObject(account(), async (instance: AccountDO, state) => {
      vi.mocked(state.storage.setAlarm).mockRestore();
      await state.storage.deleteAlarm();
      await instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
      return state.storage.getAlarm();
    });
    expect(alarm).not.toBeNull();
    expect(await storedCounts()).toEqual({ "get_server.200": 2 });
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
  });

  it("keeps an hour whose send failed for the next hour, and drops it after a day", async () => {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const tries: number[][] = [];
    const triedHour = () => tries.filter((hours) => hours.includes(hour)).length;
    let nowMs = (hour + 2) * HOUR_MS;
    await runInDurableObject(account(), async (instance: AccountDO, state) => {
      startReporting(instance, state, hour * HOUR_MS + 60_000, (hour + 2) * HOUR_MS);
      await instance.recordUsage({ openedMs: instance.clock(), hours: { [hour]: { counts: { "get_server.200": 1 }, latency: {}, billing: {} } } });
      instance.clock = () => nowMs;
      instance.sendReport = async (report) => {
        tries.push((report as { batch: { timestamp: string }[] }).batch.map((event) => Date.parse(event.timestamp) / HOUR_MS));
        throw new Error("network down");
      };
    });
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runDurableObjectAlarm(account());
    expect(triedHour()).toBe(1);
    nowMs += HOUR_MS / 2;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(triedHour()).toBe(1);
    nowMs = (hour + 3) * HOUR_MS;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(triedHour()).toBe(2);
    nowMs = (hour + 25) * HOUR_MS;
    await runInDurableObject(account(), (instance: AccountDO) => instance.alarm());
    expect(triedHour()).toBe(2);
    expect((await storedHours()).map((row) => Number(row.hour))).not.toContain(hour);
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
  });
});

describe("billing counts across evictions", () => {
  const listTree = (owner: DeviceCredentials, spaceId: string, reqId: bigint) =>
    post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token);
  const clearHours = () => runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
  const sumOf = (calls: UsageHours[]) => calls.flatMap((hours) => Object.values(hours)).reduce((sum, usage) => mergeUsage(sum, usage), emptyUsage());
  const leftover = (stub: SpaceStub) =>
    Promise.all([
      runInDurableObject(stub, (instance: object) => (instance as unknown as { traffic: UsageTally }).traffic.take() ?? null),
      runInDurableObject(account(), (instance: object) => (instance as unknown as { tally: UsageTally }).tally.take() ?? null),
    ]);

  async function ledger(stub: SpaceStub) {
    const seen = { read: 0, written: 0, alarms: 0, classA: 0, classB: 0, open: true, close: () => {} };
    await runInDurableObject(stub, (instance: object, state) => {
      const sql = Object.getPrototypeOf(state.storage.sql) as { exec(this: unknown, ...args: unknown[]): { rowsRead: number; rowsWritten: number } };
      const exec = sql.exec;
      const cursors: { rowsRead: number; rowsWritten: number }[] = [];
      vi.spyOn(sql, "exec").mockImplementation(function (this: unknown, ...args: unknown[]) {
        const cursor = exec.apply(this, args);
        if (seen.open) cursors.push(cursor);
        return cursor;
      });
      const storage = Object.getPrototypeOf(state.storage) as { setAlarm(this: unknown, ...args: unknown[]): Promise<void> };
      const setAlarm = storage.setAlarm;
      vi.spyOn(storage, "setAlarm").mockImplementation(function (this: unknown, ...args: unknown[]) {
        if (seen.open) seen.alarms++;
        return setAlarm.apply(this, args);
      });
      const bucket = (instance as unknown as { env: Env }).env.BUCKET as unknown as Record<string, (...args: unknown[]) => unknown>;
      for (const [name, counter] of [["put", "classA"], ["list", "classA"], ["createMultipartUpload", "classA"], ["get", "classB"], ["head", "classB"]] as const) {
        const original = bucket[name]!.bind(bucket);
        vi.spyOn(bucket, name).mockImplementation((...args: unknown[]) => {
          if (seen.open) seen[counter]++;
          return original(...args);
        });
      }
      seen.close = () => {
        seen.open = false;
        seen.read = cursors.reduce((sum, cursor) => sum + cursor.rowsRead, 0);
        seen.written = cursors.reduce((sum, cursor) => sum + cursor.rowsWritten, 0);
      };
    });
    return seen;
  }

  it("counts each HTTP RPC that wakes an evicted Space DO, and the time it ran", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    expect((await listTree(owner, spaceId, 1n)).status).toBe(200);
    await vi.waitFor(async () => expect((await storedCounts())["rpc_list_tree.ok"]).toBe(1));
    await clearHours();
    for (let reqId = 2n; reqId <= 4n; reqId++) {
      await evictDurableObject(stub);
      expect((await listTree(owner, spaceId, reqId)).status).toBe(200);
    }
    await vi.waitFor(async () => expect((await storedCounts())["rpc_list_tree.ok"]).toBe(3));
    const billing = (await storedTotal()).billing;
    expect({ do_space_requests: billing.do_space_requests, worker_requests: billing.worker_requests, ran: (billing.do_space_active_ms ?? 0) > 0 }).toEqual({ do_space_requests: 3, worker_requests: 3, ran: true });
  });

  it("counts every row, setAlarm and R2 operation of an alarm that wakes an evicted Space DO, and hands them over inside the alarm", async () => {
    const { spaceId, stub } = await reportingSpace();
    const laptop = await client({ spaceId, stub }, "Laptop");
    const file = await content("a file the export writes");
    await laptop.commit([op.create(uuidv7(), spaceId, "export.txt", file.content)], { receipts: await laptop.uploadParts(file) });
    await runInDurableObject(stub, async (_instance, state) => {
      const nowMs = Date.now();
      state.storage.sql.exec("INSERT INTO schedule (task, due_ms) VALUES ('gc', ?), ('export', ?) ON CONFLICT (task) DO UPDATE SET due_ms = excluded.due_ms", nowMs, nowMs);
      await state.storage.setAlarm(Date.now() + 1);
    });
    const usage = await watchUsage();
    const seen = await ledger(stub);
    usage.hooks.entered = () => seen.close();
    await evictDurableObject(stub);
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await vi.waitFor(() => expect(usage.calls).toHaveLength(1));
    const { billing } = sumOf(usage.calls);
    expect(seen.alarms).toBeGreaterThanOrEqual(1);
    expect(seen.classA).toBeGreaterThanOrEqual(1);
    expect({ ...billing, do_space_active_ms: undefined, do_account_active_ms: undefined }).toEqual({
      do_space_requests: 1,
      do_account_requests: 1,
      do_rows_read: seen.read,
      do_rows_written: seen.written + seen.alarms,
      r2_class_a: seen.classA,
      ...(seen.classB > 0 ? { r2_class_b: seen.classB } : {}),
      do_space_active_ms: undefined,
      do_account_active_ms: undefined,
    });
    expect(usage.stored.map((object) => object.object)).toEqual([stub.id.toString()]);
    expect(await leftover(stub)).toEqual([null, null]);
  });

  it("counts WebSocket messages that wake a hibernated Space DO, and hands them over when the last socket closes", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const usage = await watchUsage();
    const socket = await connect(spaceId, owner, stub);
    for (let n = 0; n < 3; n++) {
      await evictDurableObject(stub);
      await socket.listTree();
    }
    expect(await parkedCounts(stub)).toEqual([3]);
    socket.ws.close(1000, "done");
    await vi.waitFor(() => expect(spaceCalls(usage.calls)).toHaveLength(1));
    const handed = sumOf(spaceCalls(usage.calls));
    expect({ listed: handed.counts["rpc_list_tree.ok"], requests: handed.billing.do_space_requests, messages: handed.billing.do_ws_messages }).toEqual({ listed: 3, requests: 1, messages: 4 });
    expect(await leftover(stub)).toEqual([null, null]);
  });

  it("counts each session check that wakes an evicted Account DO", async () => {
    const { owner, stub } = await reportingSpace();
    await vi.waitFor(async () => expect((await storedCounts())["post_spaces.201"]).toBe(1));
    await clearHours();
    const usage = await watchUsage();
    for (let n = 1; n <= 3; n++) {
      await evictDurableObject(account());
      const listed = await exports.default.fetch(BASE + "/v1/devices", { headers: { Authorization: "Bearer " + owner.access!.token } });
      expect(listed.status).toBe(200);
      await listed.arrayBuffer();
      await vi.waitFor(async () => expect((await storedCounts())["get_devices.200"]).toBe(n));
    }
    await vi.waitFor(async () => expect((await storedTotal()).billing.do_account_requests).toBe(3 * 2 + usage.calls.length));
    expect(await leftover(stub)).toEqual([null, null]);
  });

  it("counts share-link calls and the Account DO's calls into an evicted Space DO", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    await vi.waitFor(async () => expect((await storedCounts())["post_spaces.201"]).toBe(1));
    await clearHours();
    await evictDurableObject(stub);
    const created = await post(`/v1/spaces/${spaceId}/links`, body(CreateShareLinkRequestSchema, { nodeId: spaceId }), owner.access!.token, ShareLinkSchema);
    expect(created.status).toBe(201);
    for (let n = 0; n < 2; n++) {
      await evictDurableObject(stub);
      await evictDurableObject(account());
      expect((await exports.default.fetch(`${BASE}/v1/share/${created.message!.linkId}`)).status).toBe(200);
    }
    await vi.waitFor(async () => expect((await storedTotal()).billing.do_space_requests).toBe(3));
    expect(await leftover(stub)).toEqual([null, null]);
  });

  it("keeps nothing in a Space DO's or the Account DO's memory once an HTTP RPC, a WebSocket message, an alarm or a socket close ends", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const settled = () => vi.waitFor(async () => expect(await leftover(stub)).toEqual([null, null]));
    expect((await listTree(owner, spaceId, 1n)).status).toBe(200);
    await settled();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    await settled();
    await runInDurableObject(stub, (_instance, state) => state.storage.setAlarm(Date.now() + 1));
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await settled();
    socket.ws.close(1000, "done");
    await vi.waitFor(async () => expect(await parkedCounts(stub)).toEqual([]));
    await settled();
    await runInDurableObject(stub, (_instance, state) => state.storage.setAlarm(Date.now() + 1));
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await settled();
  });

  it("calls, arms, parks and writes nothing for reports when reports are off and an alarm, an RPC or a session check wakes a fresh instance", async () => {
    const { owner, spaceId, stub } = await reportingSpace(TokenRequest_Telemetry.OFF);
    const usage = await watchUsage();
    const timers = vi.spyOn(globalThis, "setTimeout");
    const written: unknown[][] = [];
    await runInDurableObject(stub, (_instance, state) => {
      const sql = Object.getPrototypeOf(state.storage.sql) as { exec(this: unknown, ...args: unknown[]): unknown };
      const exec = sql.exec;
      vi.spyOn(sql, "exec").mockImplementation(function (this: unknown, ...args: unknown[]) {
        if (/usage_hours|object_usage|task = 'usage'|'usage'/.test(String(args[0])) || args.includes("reporting") || args.includes("r2_stored")) written.push(args);
        return exec.apply(this, args);
      });
    });
    await runInDurableObject(stub, (_instance, state) => state.storage.setAlarm(Date.now() + 1));
    await evictDurableObject(stub);
    await evictDurableObject(account());
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    await evictDurableObject(stub);
    await evictDurableObject(account());
    expect((await listTree(owner, spaceId, 1n)).status).toBe(200);
    await evictDurableObject(account());
    await refresh(owner, TokenRequest_Telemetry.OFF);
    await scheduler.wait(100);
    expect(usage.calls).toEqual([]);
    expect(timers.mock.calls.filter(([, ms]) => (ms ?? 0) > 0)).toEqual([]);
    const flagRead = (args: unknown[]) => args[0] === "SELECT value FROM meta WHERE key = ?" && args[1] === "reporting";
    expect(written.filter(flagRead).length).toBeGreaterThanOrEqual(2);
    expect(written.filter((args) => !flagRead(args))).toEqual([]);
    expect(await usageDueMs(stub)).toBeUndefined();
    expect(await storedHours()).toEqual([]);
    expect(await leftover(stub)).toEqual([null, null]);
  });
});

describe("reply latency", () => {
  const listTree = (owner: DeviceCredentials, spaceId: string, reqId: bigint) =>
    post(`/v1/spaces/${spaceId}/rpc`, body(RpcRequestSchema, { reqId, call: { case: "listTree", value: {} } }), owner.access!.token);
  const timed = async (call: () => Promise<unknown>) => {
    const startedMs = performance.now();
    await call();
    return performance.now() - startedMs;
  };
  const dropKept = () => runInDurableObject(account(), (instance: object) => (instance as unknown as { tally: UsageTally }).tally.takeAll());

  async function slowAccount(method: "checkSessions" | "recordUsage", delayMs: number) {
    const prototype = (await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance))) as Record<string, (...args: unknown[]) => Promise<unknown>>;
    const original = prototype[method]!;
    const calls = { started: 0, answered: 0 };
    prototype[method] = async function (this: AccountDO, ...args: unknown[]) {
      calls.started++;
      await scheduler.wait(delayMs);
      try {
        return await original.apply(this, args);
      } finally {
        calls.answered++;
      }
    };
    const restore = () => {
      prototype[method] = original;
    };
    restorers.push(restore);
    return { calls, restore };
  }

  it("answers an HTTP RPC as fast with reports on as with reports off while the Account DO takes 2 s to renew the RPC's lease", { timeout: 30_000 }, async () => {
    restorers.push(dropKept);
    const tookMs: number[] = [];
    for (const telemetry of [TokenRequest_Telemetry.OFF, TokenRequest_Telemetry.ON]) {
      const { owner, spaceId, stub } = await reportingSpace(telemetry);
      expect((await listTree(owner, spaceId, 1n)).status).toBe(200);
      const slow = await slowAccount("checkSessions", 2_000);
      await runInDurableObject(stub, (instance: SpaceDO) => {
        instance.clock = () => Date.now() + 3_000;
      });
      tookMs.push(await timed(async () => expect((await listTree(owner, spaceId, 2n)).status).toBe(200)));
      await vi.waitFor(() => expect(slow.calls).toEqual({ started: 1, answered: 1 }), { timeout: 5_000 });
      slow.restore();
    }
    const [off, on] = tookMs;
    expect(off).toBeLessThan(1_000);
    expect(on).toBeLessThan(1_000);
  });

  it("answers a failed HTTP RPC and a WebSocket upgrade as fast with reports on as with reports off while the Account DO takes 2 s to record usage", { timeout: 30_000 }, async () => {
    UsageTally.batchMs = 0;
    restorers.push(() => {
      UsageTally.batchMs = BATCH_MS;
    });
    const tookMs: { failed: number; upgrade: number }[] = [];
    for (const telemetry of [TokenRequest_Telemetry.OFF, TokenRequest_Telemetry.ON]) {
      const { owner, spaceId, stub } = await reportingSpace(telemetry);
      expect((await listTree(owner, spaceId, 1n)).status).toBe(200);
      const prototype = (await runInDurableObject(stub, (instance: SpaceDO) => Object.getPrototypeOf(instance))) as SpaceDO;
      const httpRpc = prototype.httpRpc;
      prototype.httpRpc = async () => {
        throw new Error("space failed");
      };
      restorers.push(() => {
        prototype.httpRpc = httpRpc;
      });
      const slow = await slowAccount("recordUsage", 2_000);
      const failed = await timed(async () => expect((await listTree(owner, spaceId, 2n)).status).toBeGreaterThanOrEqual(500));
      prototype.httpRpc = httpRpc;
      const upgrade = await timed(async () => {
        const response = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/ws`, { headers: { Upgrade: "websocket", Authorization: "Bearer " + owner.access!.token } });
        expect(response.status).toBe(101);
        response.webSocket!.accept();
        response.webSocket!.close(1000, "done");
      });
      tookMs.push({ failed, upgrade });
      await vi.waitFor(async () => expect(await runInDurableObject(stub, (_instance, state) => state.getWebSockets().length)).toBe(0), { timeout: 5_000 });
      await vi.waitFor(() => expect(slow.calls.answered).toBe(slow.calls.started), { timeout: 10_000 });
      slow.restore();
    }
    const [off, on] = tookMs;
    expect(off!.failed).toBeLessThan(1_000);
    expect(on!.failed).toBeLessThan(1_000);
    expect(off!.upgrade).toBeLessThan(1_000);
    expect(on!.upgrade).toBeLessThan(1_000);
  });

  it("keeps the counts of an Account DO call nobody waits on out of its reply, and stores them with the next batch it records and at its alarm", async () => {
    const { owner, spaceId } = await reportingSpace();
    restorers.push(dropKept);
    const hour = Math.floor(Date.now() / HOUR_MS);
    await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("DELETE FROM usage_hours"));
    const replies: Carried[] = [];
    const kept = meteredStub(account(), (carried) => replies.push(carried), false);
    await kept.checkSessions([owner.deviceId], spaceId);
    expect(replies).toHaveLength(1);
    expect(replies[0]!.usage).toBeUndefined();
    expect(replies[0]!.reporting).toBe(true);
    expect(await storedHours()).toEqual([]);
    expect(await meteredStub(account(), () => {}).recordUsage({ openedMs: Date.now(), hours: { [hour]: emptyUsage() } })).toBe(true);
    expect((await storedTotal()).billing.do_account_requests).toBe(2);
    await kept.checkSessions([owner.deviceId], spaceId);
    expect(replies[1]!.usage).toBeUndefined();
    expect(await runDurableObjectAlarm(account())).toBe(true);
    expect((await storedTotal()).billing.do_account_requests).toBe(4);
  });

  it("parks a background lease renewal's counts on an open socket when the answer comes after the event ended", { timeout: 30_000 }, async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    restorers.push(dropKept);
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    const parkedActiveMs = async () => billingOf(((await attachments(stub))[0]!.usage as { hours: UsageHours } | undefined)?.hours ?? {}).do_account_active_ms ?? 0;
    const before = await parkedActiveMs();
    const slow = await slowAccount("checkSessions", 500);
    await runInDurableObject(stub, (instance: SpaceDO) => {
      instance.clock = () => Date.now() + 3_000;
    });
    await socket.listTree();
    await vi.waitFor(() => expect(slow.calls).toEqual({ started: 1, answered: 1 }), { timeout: 5_000 });
    await vi.waitFor(async () => expect(await parkedActiveMs()).toBeGreaterThanOrEqual(before + 500));
    socket.ws.close(1000, "done");
  });
});

describe("reporting restarts", () => {
  it("drops a batch opened before reporting last started once it learns the start, and counts after it in a fresh batch", async () => {
    const tally = new UsageTally();
    tally.reporting = true;
    const handed: UsageBatch[] = [];
    tally.count("get_server", "200", 3, 10 * HOUR_MS);
    tally.count("get_server", "200", 3, 10 * HOUR_MS + 5);
    tally.receive({ reporting: true, startedMs: 10 * HOUR_MS + 10 }, false);
    tally.count("get_server", "201", 3, 10 * HOUR_MS + 20);
    tally.put({ openedMs: 10 * HOUR_MS + 1, hours: { 10: { counts: { "get_server.202": 1 }, latency: {}, billing: {} } } });
    tally.receive({ reporting: true, startedMs: 10 * HOUR_MS }, false);
    await tally.handOver(async (batch) => {
      handed.push(batch);
      return true;
    });
    expect(handed.map((batch) => [batch.openedMs, batch.hours[10]!.counts])).toEqual([[10 * HOUR_MS + 20, { "get_server.201": 1 }]]);
    expect(tally.startedMs).toBe(10 * HOUR_MS + 10);
  });

  it("counts what a Worker isolate counts after reporting restarts, even when it missed the stop", async () => {
    UsageTally.quietMs = 1500;
    restorers.push(() => {
      UsageTally.quietMs = QUIET_MS;
    });
    const owner = await ownerOnce();
    await runInDurableObject(account(), (instance: AccountDO, state) => {
      instance.clock = () => Date.now();
      state.storage.sql.exec("DELETE FROM usage_hours");
    });
    await refresh(owner, TokenRequest_Telemetry.ON);
    const list = async () => {
      const listed = await exports.default.fetch(BASE + "/v1/spaces", { headers: { Authorization: "Bearer " + owner.access!.token } });
      await listed.arrayBuffer();
      return listed.status;
    };
    expect(await list()).toBe(200);
    const flip = (telemetry: TokenRequest_Telemetry) =>
      runInDurableObject(account(), (instance: AccountDO, state) => {
        state.storage.sql.exec("UPDATE devices SET telemetry = ?", telemetry);
        countVotes(instance);
      });
    await flip(TokenRequest_Telemetry.OFF);
    await scheduler.wait(20);
    await flip(TokenRequest_Telemetry.ON);
    await scheduler.wait(20);
    expect(await list()).toBe(200);
    expect(await list()).toBe(200);
    await scheduler.wait(4000);
    expect((await storedCounts())["get_spaces.200"]).toBe(2);
  });

  it("drops what a Space DO parked before reporting restarted once an Account DO answer says so, keeps what it parks after, and remembers the start across eviction", async () => {
    const { owner, spaceId, stub } = await reportingSpace();
    const socket = await connect(spaceId, owner, stub);
    await socket.listTree();
    expect(await parkedCounts(stub)).toEqual([1]);
    const startedMs = Date.now() + 1;
    await runInDurableObject(stub, (instance: object) => (instance as unknown as { received(carried: Carried): void }).received({ reporting: true, startedMs }));
    expect(await parkedCounts(stub)).toEqual([0]);
    await socket.listTree();
    expect(await parkedCounts(stub)).toEqual([1]);
    await evictDurableObject(stub);
    expect(await runInDurableObject(stub, (instance: object) => (instance as unknown as { traffic: UsageTally }).traffic.startedMs)).toBe(startedMs);
    await runInDurableObject(stub, (instance: object) => (instance as unknown as { received(carried: Carried): void }).received({ reporting: true, startedMs }));
    expect(await parkedCounts(stub)).toEqual([1]);
  });
});
