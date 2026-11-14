// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import { SESSION_LEASE_MS } from "../src/account/account";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  RewindRequestSchema,
  RewindResultSchema,
  SpaceInfoSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import {
  ClientMessageSchema,
  ResyncRequired_Reason,
  RpcRequestSchema,
  RpcResponseSchema,
  type ServerMessage,
  ServerMessageSchema,
} from "../src/gen/koidrive/v1/messages_pb";
import { ChunkPlan_Action, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../src/ids";
import { Close, CONNECT_HEADER, type SpaceDO } from "../src/space-do";
import { RETIRED_HISTORY } from "../src/space/space";
import { ChunkState } from "../src/space/schema";
import { chunkKey } from "../src/space/store";
import { restoreFromR2, serverState } from "./export-reader";
import { content, op, runSpaceAlarm, TestClient, targetIncarnation, unknownOutcome } from "./helpers";
import { pitrBackend } from "../src/space/pitr";
import { simulatedPitr } from "./pitr";

simulatedPitr.install();

const BASE = "https://koidrive.test";
const MINUTE = 60_000;
const DAY = 24 * 3_600_000;

afterEach(() => {
  vi.restoreAllMocks();
  simulatedPitr.dropArms = false;
  simulatedPitr.bookmarkFor = undefined;
});

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

/** A new stub each time: a rewind restarts the Space DO, and a stub of the ended instance keeps failing. */
function space(spaceId: string) {
  return env.SPACE.get(env.SPACE.idFromName(spaceId));
}

async function call<Res extends DescMessage>(
  path: string,
  init: { token?: string; body?: Uint8Array; headers?: Record<string, string> },
  schema: Res,
) {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf", ...init.headers };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.headers?.Upgrade ? "GET" : "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return {
    status: response.status,
    message: response.ok ? (fromBinary(schema, bytes) as MessageShape<Res>) : undefined,
    error: response.ok || bytes.length === 0 ? undefined : fromBinary(ErrorSchema, bytes),
  };
}

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

let owner: DeviceCredentials | undefined;

async function getOwner(): Promise<DeviceCredentials> {
  if (owner) return owner;
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  const claimed = await call(
    "/v1/claim",
    { body: body(ClaimRequestSchema, { code: line!.split(": ")[1]!, userName: "Owner", deviceName: "Laptop A", platform: "macos" }) },
    DeviceCredentialsSchema,
  );
  owner = claimed.message!;
  return owner;
}

async function pairDevice(inviter: DeviceCredentials, deviceName: string): Promise<DeviceCredentials> {
  const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
  const paired = await call(
    "/v1/pair/complete",
    { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform: "linux" }) },
    DeviceCredentialsSchema,
  );
  return paired.message!;
}

async function newSpace() {
  const creds = await getOwner();
  const createdAtMs = Date.now();
  const created = await call("/v1/spaces", { token: creds.access!.token, body: body(CreateSpaceRequestSchema, { name: `Home ${crypto.randomUUID()}` }) }, SpaceInfoSchema);
  const spaceId = created.message!.spaceId;
  const device = () =>
    new TestClient(space(spaceId), spaceId, {
      deviceId: creds.deviceId,
      userId: creds.userId,
      deviceName: "Laptop A",
      kind: "device",
      scope: null,
      role: "owner",
      maxFormat: 1,
      origin: BASE,
    });
  return { spaceId, createdAtMs, creds, device };
}

function setAccountClock(atMs: number) {
  return runInDurableObject(account(), (instance: AccountDO) => {
    instance.clock = () => atMs;
  });
}

function snapshot(spaceId: string, atMs: number) {
  return runInDurableObject(space(spaceId), (_instance, state) => simulatedPitr.snapshot(state, atMs));
}

function spaceRows(spaceId: string, query: string, ...bindings: SqlStorageValue[]) {
  return runInDurableObject(space(spaceId), (_instance, state) => state.storage.sql.exec(query, ...bindings).toArray());
}

function rewindRecord(rewindId: string) {
  return runInDurableObject(
    account(),
    (_instance, state) => state.storage.sql.exec("SELECT * FROM rewinds WHERE rewind_id = ?", rewindId).toArray()[0],
  );
}

function rewind(token: string, spaceId: string, init: { restorePointMs: number; expectedHistoryId: string; rewindId?: string }) {
  const request = body(RewindRequestSchema, {
    restorePointMs: BigInt(init.restorePointMs),
    expectedHistoryId: init.expectedHistoryId,
    rewindId: init.rewindId ?? uuidv7(),
  });
  return call(`/v1/spaces/${spaceId}/rewind`, { token, body: request }, RewindResultSchema);
}

async function rpc(token: string, spaceId: string, init: MessageInitShape<typeof RpcRequestSchema>["call"]) {
  const response = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/rpc`, {
    method: "POST",
    headers: { Authorization: "Bearer " + token, "Content-Type": "application/x-protobuf" },
    body: body(RpcRequestSchema, { reqId: 1n, call: init }),
  });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return { status: response.status, result: response.ok ? fromBinary(RpcResponseSchema, bytes).result : undefined };
}

async function socket(spaceId: string, token: string) {
  const response = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/ws`, {
    headers: { Upgrade: "websocket", Authorization: "Bearer " + token },
  });
  const ws = response.webSocket!;
  ws.binaryType = "arraybuffer";
  ws.accept();
  const received: ServerMessage["msg"][] = [];
  const state: { closed?: number } = {};
  ws.addEventListener("message", (event) => received.push(fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer)).msg));
  ws.addEventListener("close", (event) => {
    state.closed = event.code;
  });
  const send = (msg: MessageInitShape<typeof ClientMessageSchema>["msg"]) =>
    ws.send(toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg } as MessageInitShape<typeof ClientMessageSchema>)));
  const until = async <T>(take: () => T | undefined): Promise<T> => {
    for (let i = 0; i < 60; i++) {
      const value = take();
      if (value !== undefined) return value;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error("timed out");
  };
  send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
  const welcome = await until(() => received.find((msg) => msg.case === "welcome"));
  return { ws, received, state, send, until, welcome: welcome.value };
}

async function createFile(device: TestClient, parentId: string, name: string, text: string) {
  const parts = await content(text);
  const nodeId = uuidv7();
  await device.commit([op.create(nodeId, parentId, name, parts.content)], { receipts: await device.uploadParts(parts) });
  return { nodeId, parts };
}

describe("space rewind (RFC 0003)", () => {
  it("restores the state at the restore point under a new history, and answers a retried rewind_id with the stored result", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    const file = await createFile(a, spaceId, "notes.txt", "first");
    const p1 = createdAtMs + MINUTE;
    await snapshot(spaceId, p1);
    await a.commit([op.update(file.nodeId, (await content("second")).content)], { receipts: await a.upload((await content("second")).content) });
    const previous = await a.history();
    const before = await socket(spaceId, token);

    await setAccountClock(createdAtMs + 5 * MINUTE);
    const rewindId = uuidv7();
    const done = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: previous, rewindId });
    expect(done.status).toBe(200);
    const result = done.message!;
    expect(result).toMatchObject({ restoreSeq: 1n, restorePointMs: BigInt(p1), previousHistoryId: previous, previousHeadSeq: 2n });
    expect(result.historyId).not.toBe(previous);
    await before.until(() => before.state.closed);
    expect(before.state.closed).toBe(Close.SPACE_REWOUND);
    expect(before.received.at(-1)).toMatchObject({ case: "goodbye", value: { code: Close.SPACE_REWOUND } });

    const restored = await device().tree();
    expect(restored.get(file.nodeId)?.current?.content?.size).toBe(5n);
    const meta = Object.fromEntries((await spaceRows(spaceId, "SELECT key, value FROM meta")).map((row) => [row.key, row.value]));
    expect(meta).toMatchObject({ history_id: result.historyId, head_seq: 1, rewind_number: 1, restore_seq: 1, restore_point_ms: p1 });
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
    expect(await rewindRecord(rewindId)).toMatchObject({ phase: "completed", attempt: 1, rewind_number: 1 });

    const again = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: previous, rewindId });
    expect(again.status).toBe(200);
    expect(again.message).toEqual(result);
    const changed = await rewind(token, spaceId, { restorePointMs: p1 + 1, expectedHistoryId: previous, rewindId });
    expect([changed.status, changed.error?.code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    const stale = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: previous });
    expect([stale.status, stale.error?.code]).toEqual([409, Error_Code.HISTORY_MISMATCH]);

    const old = await device().commitRaw([op.mkdir(uuidv7(), spaceId, "late")], { historyId: previous });
    expect(old).toMatchObject({ case: "error", value: { code: Error_Code.HISTORY_MISMATCH } });
    const after = await socket(spaceId, token);
    expect(after.welcome).toMatchObject({ historyId: result.historyId, restoreSeq: 1n, restorePointMs: BigInt(p1) });
    after.send({ case: "resume", value: { historyId: previous, sinceSeq: 2n } });
    const resync = await after.until(() => after.received.find((msg) => msg.case === "resyncRequired"));
    expect(resync.value).toMatchObject({ reason: ResyncRequired_Reason.HISTORY_CHANGED, restoreSeq: 1n, restorePointMs: BigInt(p1) });
    after.ws.close();

    const plan = await rpc(token, spaceId, { case: "prepareUpload", value: { chunks: (await content("third " + uuidv7())).content.chunks } });
    if (plan.result?.case !== "prepareUpload") throw new Error("prepareUpload failed");
    expect(targetIncarnation(plan.result.value.chunks[0]!.target!.url)).toMatch(/^1\./);

    const b = device();
    b.acceptedSeq = 1n;
    await b.commit([op.mkdir(uuidv7(), spaceId, "after first rewind")], { baseSeq: 1n });
    const p2 = createdAtMs + 6 * MINUTE;
    await snapshot(spaceId, p2);
    await b.commit([op.mkdir(uuidv7(), spaceId, "discarded")]);
    await setAccountClock(createdAtMs + 10 * MINUTE);
    const inside = await rewind(token, spaceId, { restorePointMs: createdAtMs + 5 * MINUTE, expectedHistoryId: result.historyId });
    expect([inside.status, inside.error?.code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    await runSpaceAlarm(space(spaceId), createdAtMs + 10 * MINUTE);
    const second = await rewind(token, spaceId, { restorePointMs: p2, expectedHistoryId: result.historyId });
    expect(second.status).toBe(200);
    expect(second.message).toMatchObject({ restoreSeq: 2n, previousHistoryId: result.historyId, previousHeadSeq: 3n });
    expect(await spaceRows(spaceId, "SELECT value FROM meta WHERE key = 'rewind_number'")).toEqual([{ value: 2 }]);
  });

  it("refuses a revoked device, a caller who is not the owner, and restore points outside the admissible range, recording nothing", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const history = await device().history();
    const lost = await pairDevice(creds, "Lost phone");
    await account().revokeDevice(lost.deviceId);
    await setAccountClock(createdAtMs + 2 * DAY);
    expect((await rewind(lost.access!.token, spaceId, { restorePointMs: createdAtMs + DAY, expectedHistoryId: history })).status).toBe(401);
    const stranger = await account().rewind({
      userId: uuidv7(),
      spaceId,
      rewindId: uuidv7(),
      restorePointMs: createdAtMs + DAY,
      expectedHistoryId: history,
    });
    expect(stranger).toMatchObject({ ok: false, code: Error_Code.FORBIDDEN });

    const token = creds.access!.token;
    for (const restorePointMs of [createdAtMs + 2 * DAY - 30_000, createdAtMs - 1]) {
      const refused = await rewind(token, spaceId, { restorePointMs, expectedHistoryId: history });
      expect([refused.status, refused.error?.code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    }
    await setAccountClock(createdAtMs + 30 * DAY);
    const tooOld = await rewind(token, spaceId, { restorePointMs: createdAtMs + DAY - MINUTE, expectedHistoryId: history });
    expect([tooOld.status, tooOld.error?.code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    const malformed = await rewind(token, spaceId, { restorePointMs: createdAtMs + 2 * DAY, expectedHistoryId: history, rewindId: "not-a-uuid" });
    expect(malformed.status).toBe(400);
    const recorded = await runInDurableObject(account(), (_instance, state) =>
      state.storage.sql.exec("SELECT * FROM rewinds WHERE space_id = ?", spaceId).toArray(),
    );
    expect(recorded).toEqual([]);
  });

  it("holds the space closed until a restore that did not run is retried as a new attempt and completes", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "kept.txt", "kept");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await a.commit([op.mkdir(uuidv7(), spaceId, "dropped")]);
    const history = await a.history();
    const open = await socket(spaceId, token);

    simulatedPitr.dropArms = true;
    await setAccountClock(createdAtMs + 5 * MINUTE);
    const rewindId = uuidv7();
    const running = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId });
    expect([running.status, running.error?.code]).toEqual([503, Error_Code.RETRY_LATER]);
    expect(running.error?.message).toMatch(/restore did not run/);
    await open.until(() => open.state.closed);
    expect(open.state.closed).toBe(Close.SPACE_REWOUND);
    expect((await rewindRecord(rewindId))!.attempt).toBeGreaterThanOrEqual(2);

    expect((await rpc(token, spaceId, { case: "listTree", value: {} })).status).toBe(503);
    expect((await call(`/v1/spaces/${spaceId}/ws`, { token, headers: { Upgrade: "websocket" } }, ErrorSchema)).status).toBe(503);
    expect(await account().retireChunks(spaceId, [{ hashHex: "00".repeat(32), incarnation: "0.1" }])).toMatchObject({
      ok: false,
      code: Error_Code.RETRY_LATER,
    });
    const another = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history });
    expect([another.status, another.error?.code]).toEqual([503, Error_Code.RETRY_LATER]);

    const schedule = await spaceRows(spaceId, "SELECT task, due_ms FROM schedule");
    const ran = await runInDurableObject(space(spaceId), async (instance: SpaceDO, state) => {
      instance.clock = () => Date.now() + 2 * DAY;
      await instance.alarm();
      return state.storage.getAlarm();
    });
    expect(ran).toBeLessThanOrEqual(Date.now() + 2 * DAY + 10 * MINUTE);
    expect(await spaceRows(spaceId, "SELECT task, due_ms FROM schedule")).toEqual(schedule);

    simulatedPitr.dropArms = false;
    await runDurableObjectAlarm(account());
    const done = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId });
    expect(done.status).toBe(200);
    expect(done.message).toMatchObject({ restoreSeq: 1n, previousHeadSeq: 2n });
    expect(await rewindRecord(rewindId)).toMatchObject({ phase: "completed" });
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
    expect((await rpc(token, spaceId, { case: "listTree", value: {} })).status).toBe(200);
  });

  it("fails without changing the space when the bookmark is not earlier than the marker, or the restore point leaves the PITR window", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "a.txt", "a");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    const history = await a.history();
    await setAccountClock(createdAtMs + 5 * MINUTE);

    simulatedPitr.bookmarkFor = () => "~ later than every marker";
    const late = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history });
    expect([late.status, late.error?.code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    simulatedPitr.bookmarkFor = undefined;
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
    expect(await a.history()).toBe(history);

    simulatedPitr.dropArms = true;
    const rewindId = uuidv7();
    expect((await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId })).status).toBe(503);
    await setAccountClock(point + 30 * DAY + 1);
    await runDurableObjectAlarm(account());
    const expired = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId });
    expect([expired.status, expired.error?.message]).toEqual([400, "the restore point left the PITR window before the restore ran"]);
    expect(await rewindRecord(rewindId)).toMatchObject({ phase: "failed" });
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
    expect(await device().history()).toBe(history);
    expect((await rpc(token, spaceId, { case: "listTree", value: {} })).status).toBe(200);
  });

  it("applies the chunk rules and the revocations at completion, and replays the receipt on a repeated completion", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    const retired = await createFile(a, spaceId, "retired.txt", "retired " + uuidv7());
    const deleting = await createFile(a, spaceId, "deleting.txt", "deleting " + uuidv7());
    const spare = await content("spare " + uuidv7());
    await a.upload(spare.content);
    const hashOf = (parts: { content: { chunks: { hash: Uint8Array }[] } }) => parts.content.chunks[0]!.hash;
    const key = (parts: { content: { chunks: { hash: Uint8Array }[] } }) => hashOf(parts).slice().buffer;
    const [retiredRow] = await spaceRows(spaceId, "SELECT incarnation FROM chunks WHERE hash = ?", key(retired.parts));
    await spaceRows(spaceId, "UPDATE chunks SET state = ? WHERE hash = ?", ChunkState.DELETING, key(deleting.parts));
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await spaceRows(spaceId, "UPDATE chunks SET state = ? WHERE hash = ?", ChunkState.LIVE, key(deleting.parts));
    expect(
      await account().retireChunks(spaceId, [{ hashHex: toHex(hashOf(retired.parts)), incarnation: String(retiredRow!.incarnation) }]),
    ).toMatchObject({ ok: true });
    const phone = await pairDevice(creds, "Old phone");
    await account().revokeDevice(phone.deviceId);
    const history = await a.history();

    const completedAtMs = Date.now() + 2 * DAY;
    await setAccountClock(completedAtMs);
    const rewindId = uuidv7();
    expect((await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId })).status).toBe(200);

    const chunk = async (parts: { content: { chunks: { hash: Uint8Array }[] } }) =>
      (await spaceRows(spaceId, "SELECT state, refcount, incarnation, hold_until_ms FROM chunks WHERE hash = ?", key(parts)))[0];
    expect(await chunk(retired.parts)).toMatchObject({ state: ChunkState.PENDING, refcount: 1, incarnation: null });
    expect(await chunk(deleting.parts)).toMatchObject({ state: ChunkState.PENDING, refcount: 1, incarnation: null });
    expect(await chunk(spare)).toMatchObject({ state: ChunkState.PENDING, refcount: 0, hold_until_ms: completedAtMs + 30 * DAY });
    expect((await chunk(spare))!.incarnation).toMatch(/^0\./);
    expect(await spaceRows(spaceId, "SELECT device_id FROM revoked_devices WHERE device_id = ?", phone.deviceId)).toHaveLength(1);

    const download = await rpc(token, spaceId, { case: "getDownloadTargets", value: { hashes: [hashOf(retired.parts)] } });
    expect(download.result).toMatchObject({ case: "error", value: { code: Error_Code.MISSING_CHUNKS } });
    const plan = await rpc(token, spaceId, { case: "prepareUpload", value: { chunks: retired.parts.content.chunks } });
    if (plan.result?.case !== "prepareUpload") throw new Error("prepareUpload failed");
    expect(plan.result.value.chunks[0]).toMatchObject({ action: ChunkPlan_Action.UPLOAD });
    const fresh = targetIncarnation(plan.result.value.chunks[0]!.target!.url);
    expect(fresh).toMatch(/^1\./);
    expect(fresh).not.toBe(retiredRow!.incarnation);

    const orphan = chunkKey(spaceId, "ab".repeat(32), "0.424242");
    await env.BUCKET.put(orphan, "orphan");
    const sweep = (atMs: number) => runInDurableObject(space(spaceId), (instance: SpaceDO) => instance.collectGarbage(atMs));
    expect((await sweep(Date.now() + 31 * DAY)).sweptChunks).toBe(0);
    await spaceRows(spaceId, "DELETE FROM meta WHERE key = 'orphan_sweep_due_ms'");
    expect((await sweep(completedAtMs + 31 * DAY)).sweptChunks).toBe(3);

    const record = (await rewindRecord(rewindId))!;
    const replay = await space(spaceId).rewindComplete({
      rewindId,
      attempt: Number(record.attempt),
      historyId: "replayed",
      rewindNumber: 9,
      restorePointMs: point,
      completedAtMs: completedAtMs + DAY,
      revokedDevices: [],
      retired: [],
    });
    expect(replay).toMatchObject({ state: "completed", receipt: { rewindId, historyId: record.history_id, restoreSeq: 2, completedAtMs } });
    expect(await device().history()).toBe(record.history_id);
  });

  it("refuses a commit authenticated before the rewind whose body arrives after the restore, at every Space DO entry point", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    const history = await a.history();
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await a.commit([op.mkdir(uuidv7(), spaceId, "after the point")]);

    const held = new TransformStream<Uint8Array, Uint8Array>();
    const writer = held.writable.getWriter();
    const pending = exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/rpc`, {
      method: "POST",
      headers: { Authorization: "Bearer " + token, "Content-Type": "application/x-protobuf" },
      body: held.readable,
    });
    await new Promise((resolve) => setTimeout(resolve, 100));

    const prototype = await runInDurableObject(space(spaceId), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const complete = prototype.rewindComplete;
    prototype.rewindComplete = async () => {
      throw new Error("injected completion failure");
    };
    const rewindId = uuidv7();
    try {
      await setAccountClock(createdAtMs + 5 * MINUTE);
      expect((await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId })).status).toBe(503);
      expect(await rewindRecord(rewindId)).toMatchObject({ phase: "arming" });
      expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
      expect(await spaceRows(spaceId, "SELECT value FROM meta WHERE key = 'head_seq'")).toEqual([{ value: 0 }]);

      const late = uuidv7();
      const commit = body(RpcRequestSchema, {
        reqId: 1n,
        call: {
          case: "commit",
          value: { commitId: uuidv7(), baseSeq: 0n, historyId: history, deviceSeq: 1n, ops: [op.mkdir(late, spaceId, "late")] },
        },
      });
      await writer.write(commit);
      await writer.close();
      const response = await pending;
      expect(response.status).toBe(200);
      expect(fromBinary(RpcResponseSchema, new Uint8Array(await response.arrayBuffer())).result).toMatchObject({
        case: "error",
        value: { code: Error_Code.RETRY_LATER },
      });

      const connect = {
        identity: { userId: creds.userId, deviceId: creds.deviceId, deviceName: "Laptop A", expiresAtMs: Date.now() + MINUTE, kind: "device", scope: null },
        origin: BASE,
      };
      const direct = fromBinary(RpcResponseSchema, await space(spaceId).httpRpc(connect, commit));
      expect(direct.result).toMatchObject({ case: "error", value: { code: Error_Code.RETRY_LATER } });
      const upgrade = await space(spaceId).fetch(
        new Request(`${BASE}/v1/spaces/${spaceId}/ws`, { headers: { Upgrade: "websocket", [CONNECT_HEADER]: JSON.stringify(connect) } }),
      );
      expect(upgrade.status).toBe(503);
      expect(await spaceRows(spaceId, "SELECT value FROM meta WHERE key = 'head_seq'")).toEqual([{ value: 0 }]);
      expect(await spaceRows(spaceId, "SELECT node_id FROM nodes WHERE node_id = ?", late)).toEqual([]);
    } finally {
      prototype.rewindComplete = complete;
    }

    await runDurableObjectAlarm(account());
    const done = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId });
    expect(done.status).toBe(200);
    expect(done.message).toMatchObject({ restoreSeq: 0n, previousHeadSeq: 1n });
  });

  it("exports the new history under its own prefix from restore_seq, and refuses a further rewind until its descriptor is durable", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "kept.txt", "kept");
    const p1 = createdAtMs + MINUTE;
    await snapshot(spaceId, p1);
    await a.commit([op.mkdir(uuidv7(), spaceId, "dropped")]);
    const previous = await a.history();
    await runSpaceAlarm(space(spaceId), Date.now() + 5 * MINUTE);
    const files = async (prefix: string) => (await env.BUCKET.list({ prefix })).objects.map((object) => [object.key, object.etag]);
    const oldPrefix = `spaces/${spaceId}/meta/${previous}/`;
    const oldFiles = await files(oldPrefix);
    expect(oldFiles.map(([key]) => key)).toEqual([`${oldPrefix}log/${"1".padStart(20, "0")}-${"2".padStart(20, "0")}.fblog`, `${oldPrefix}space.json`]);

    const completedAtMs = createdAtMs + 5 * MINUTE;
    await setAccountClock(completedAtMs);
    const first = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: previous });
    expect(first.status).toBe(200);
    const { historyId, restoreSeq } = first.message!;
    const prefix = `spaces/${spaceId}/meta/${historyId}/`;
    expect(await files(prefix)).toEqual([]);

    const p2 = completedAtMs + MINUTE;
    await snapshot(spaceId, p2);
    await setAccountClock(completedAtMs + 5 * MINUTE);
    const refuse = async () => {
      const early = await rewind(token, spaceId, { restorePointMs: p2, expectedHistoryId: historyId });
      expect([early.status, early.error?.code, early.error?.message]).toEqual([503, Error_Code.RETRY_LATER, "the space's current history is not exported to R2 yet"]);
    };
    await refuse();
    await runSpaceAlarm(space(spaceId), completedAtMs, { intercept: unknownOutcome((key) => key.endsWith("/space.json"), true) });
    expect((await files(prefix)).map(([key]) => key)).toContain(prefix + "space.json");
    await refuse();
    expect(await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT * FROM rewinds WHERE space_id = ?", spaceId).toArray())).toHaveLength(1);

    await runSpaceAlarm(space(spaceId), completedAtMs + 2 * MINUTE);
    const snap = `${prefix}snap/${String(restoreSeq).padStart(20, "0")}/`;
    expect((await files(prefix)).map(([key]) => key)).toEqual([snap + "manifest.json", snap + "pages/0.fbsnap", prefix + "space.json"]);
    expect(await (await env.BUCKET.get(prefix + "space.json"))!.json()).toMatchObject({ history_id: historyId, rewind_number: 1 });
    expect(await (await env.BUCKET.get(snap + "manifest.json"))!.json()).toMatchObject({ history_id: historyId, start_seq: 1, end_seq: 1 });

    const b = device();
    b.acceptedSeq = 1n;
    await b.commit([op.mkdir(uuidv7(), spaceId, "after the rewind")], { baseSeq: 1n });
    await runSpaceAlarm(space(spaceId), completedAtMs + 10 * MINUTE);
    expect((await files(prefix + "log/")).map(([key]) => key)).toEqual([`${prefix}log/${"2".padStart(20, "0")}-${"2".padStart(20, "0")}.fblog`]);
    expect(await files(oldPrefix)).toEqual(oldFiles);
    const restored = await restoreFromR2(spaceId);
    const server = await serverState(space(spaceId));
    expect([restored.descriptor.history_id, restored.headSeq]).toEqual([historyId, server.headSeq]);
    expect(restored.nodes).toEqual(server.nodes);

    expect((await rewind(token, spaceId, { restorePointMs: p2, expectedHistoryId: historyId })).status).toBe(200);
  });

  it("closes the space to a connection that holds a lease from before the admission, while the marker is not yet written (RFC 0014)", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "notes.txt", "first");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await a.commit([op.mkdir(uuidv7(), spaceId, "after the point")]);
    const history = await a.history();
    const leased = await socket(spaceId, token);

    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(space(spaceId), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const prepare = prototype.rewindPrepare;
    prototype.rewindPrepare = async () => {
      throw new Error("injected prepare failure");
    };
    try {
      await setAccountClock(createdAtMs + 5 * MINUTE);
      const rewindId = uuidv7();
      expect((await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId })).status).toBe(503);
      expect(await rewindRecord(rewindId)).toMatchObject({ phase: "admitted" });
      expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
      leased.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      await leased.until(() => leased.state.closed);
      expect(leased.state.closed).toBe(Close.SPACE_REWOUND);
      expect(leased.received.some((msg) => msg.case === "rpc")).toBe(false);
    } finally {
      prototype.rewindPrepare = prepare;
    }
  });

  it("refuses a restore point before the space's point-in-time history begins, recording nothing and leaving the space open", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "a.txt", "a");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point + MINUTE);
    const history = await a.history();
    await setAccountClock(createdAtMs + 5 * MINUTE);

    const refused = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history });
    expect([refused.status, refused.error?.code, refused.error?.message]).toEqual([
      400,
      Error_Code.INVALID_ARGUMENT,
      "restore_point_ms is before the space's point-in-time history begins",
    ]);
    const recorded = await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT * FROM rewinds WHERE space_id = ?", spaceId).toArray());
    expect(recorded).toEqual([]);
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
    expect((await rpc(token, spaceId, { case: "listTree", value: {} })).status).toBe(200);
  });

  it("logs the error the Space DO raised when the admission cannot read the space, and answers retry later", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "a.txt", "a");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    const history = await a.history();
    await setAccountClock(createdAtMs + 5 * MINUTE);

    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    simulatedPitr.bookmarkFor = () => {
      throw new RangeError("no bookmark at that time");
    };
    const refused = await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history });
    expect([refused.status, refused.error?.code]).toEqual([503, Error_Code.RETRY_LATER]);
    expect(logged.mock.calls).toContainEqual(["rewind admission failed", "RangeError: no bookmark at that time"]);
  });

  it("answers the space closed to a check the Space DO makes while the admission waits for rewindInfo (RFC 0014)", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "notes.txt", "first");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await a.commit([op.mkdir(uuidv7(), spaceId, "after the point")]);
    const history = await a.history();
    const leased = await socket(spaceId, token);

    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(space(spaceId), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const [info, prepare] = [prototype.rewindInfo, prototype.rewindPrepare];
    let inside = false;
    let release!: () => void;
    const held = new Promise<void>((resolve) => (release = resolve));
    prototype.rewindInfo = async function (this: SpaceDO, restorePointMs: number) {
      const answer = info.call(this, restorePointMs);
      inside = true;
      await held;
      return answer;
    };
    prototype.rewindPrepare = async () => {
      throw new Error("injected prepare failure");
    };
    try {
      await setAccountClock(createdAtMs + 5 * MINUTE);
      const rewindId = uuidv7();
      const admission = rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId });
      await leased.until(() => inside || undefined);
      leased.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      await leased.until(() => leased.state.closed);
      expect(leased.state.closed).toBe(Close.SPACE_REWOUND);
      expect(leased.received.some((msg) => msg.case === "rpc")).toBe(false);
      release();
      expect((await admission).status).toBe(503);
      expect(await rewindRecord(rewindId)).toMatchObject({ phase: "admitted" });
    } finally {
      release();
      prototype.rewindInfo = info;
      prototype.rewindPrepare = prepare;
    }
  });

  it.each(["running", "ended"])("drops a lease renewal that was in flight when the admission closed the space, with the lease %s at admission (RFC 0014)", async (atAdmission) => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "notes.txt", "first");
    const point = createdAtMs + MINUTE;
    await snapshot(spaceId, point);
    await a.commit([op.mkdir(uuidv7(), spaceId, "after the point")]);
    const history = await a.history();
    const leased = await socket(spaceId, token);
    const age = (byMs: number) =>
      runInDurableObject(space(spaceId), (instance: SpaceDO) => {
        const atMs = instance.clock() + byMs;
        instance.clock = () => atMs;
      });
    const listTree = async (reqId: bigint) => {
      leased.send({ case: "rpc", value: { reqId, call: { case: "listTree", value: {} } } });
      return leased.until(() => leased.state.closed ?? received(reqId));
    };
    const received = (reqId: bigint) => leased.received.find((msg) => msg.case === "rpc" && msg.value.reqId === reqId);
    await age(SESSION_LEASE_MS + 1000);
    expect(await listTree(1n)).toMatchObject({ case: "rpc" });
    await age(SESSION_LEASE_MS * 0.6);

    vi.spyOn(console, "error").mockImplementation(() => {});
    const accountPrototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const spacePrototype = await runInDurableObject(space(spaceId), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const [checkSessions, prepare] = [accountPrototype.checkSessions, spacePrototype.rewindPrepare];
    let renewing = false;
    const { promise: held, resolve: release } = Promise.withResolvers<void>();
    accountPrototype.checkSessions = async function (this: AccountDO, deviceIds: string[], askedSpaceId: string) {
      const answers = await checkSessions.call(this, deviceIds, askedSpaceId);
      renewing = true;
      await held;
      return answers;
    };
    spacePrototype.rewindPrepare = async () => {
      throw new Error("injected prepare failure");
    };
    try {
      expect(await listTree(2n)).toMatchObject({ case: "rpc" });
      await leased.until(() => renewing || undefined);
      if (atAdmission === "ended") await age(SESSION_LEASE_MS * 0.5);
      await setAccountClock(createdAtMs + 5 * MINUTE);
      const rewindId = uuidv7();
      expect((await rewind(token, spaceId, { restorePointMs: point, expectedHistoryId: history, rewindId })).status).toBe(503);
      expect(await rewindRecord(rewindId)).toMatchObject({ phase: "admitted" });
      release();
      await vi.waitFor(() =>
        runInDurableObject(space(spaceId), (instance: SpaceDO) => {
          expect((instance as unknown as { renewing?: unknown }).renewing).toBeUndefined();
        }),
      );
      expect(await listTree(3n)).toBe(Close.SPACE_REWOUND);
      expect(received(3n)).toBeUndefined();
    } finally {
      release();
      accountPrototype.checkSessions = checkSessions;
      spacePrototype.rewindPrepare = prepare;
    }
  });

  it("refuses every connection while the space's storage holds a history that a rewind replaced", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const a = device();
    await createFile(a, spaceId, "notes.txt", "first");
    const p1 = createdAtMs + MINUTE;
    const beforeRewind = await snapshot(spaceId, p1);
    const previous = await a.history();
    await setAccountClock(createdAtMs + 5 * MINUTE);
    const done = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: previous });
    expect(done.status).toBe(200);
    (await socket(spaceId, token)).ws.close();

    await runInDurableObject(space(spaceId), (_instance, state) => pitrBackend.open(state).restoreOnNextStart(beforeRewind));
    await runInDurableObject(space(spaceId), (instance: SpaceDO) => instance.restart()).catch(() => undefined);
    expect(await spaceRows(spaceId, "SELECT value FROM meta WHERE key = 'history_id'")).toEqual([{ value: previous }]);

    const upgrade = await call(`/v1/spaces/${spaceId}/ws`, { token, headers: { Upgrade: "websocket" } }, ErrorSchema);
    expect([upgrade.status, upgrade.error?.code]).toEqual([409, Error_Code.HISTORY_MISMATCH]);
    expect(upgrade.error?.message).toContain("RFC 0003 section 4");
    const listed = await rpc(token, spaceId, { case: "listTree", value: {} });
    expect(listed.result).toMatchObject({ case: "error", value: { code: Error_Code.HISTORY_MISMATCH } });
  });

  it("writes one marker per attempt however often preparing repeats, and removes them when the rewind fails", async () => {
    const { spaceId } = await newSpace();
    const rewindId = uuidv7();
    for (let i = 0; i < 2; i++) await space(spaceId).rewindPrepare({ rewindId, attempt: 1, bookmark: "" });
    expect(await spaceRows(spaceId, "SELECT rewind_id, attempt FROM rewind_markers")).toEqual([{ rewind_id: rewindId, attempt: 1 }]);
    const refused = await device(spaceId).call({ case: "listTree", value: {} });
    expect(refused).toMatchObject({ case: "error", value: { code: Error_Code.RETRY_LATER } });
    await space(spaceId).rewindAbort(rewindId);
    expect(await spaceRows(spaceId, "SELECT * FROM rewind_markers")).toEqual([]);
  });
});

function device(spaceId: string) {
  return new TestClient(space(spaceId), spaceId, {
    deviceId: uuidv7(),
    userId: owner!.userId,
    deviceName: "Probe",
    kind: "device",
    scope: null,
    role: "owner",
    maxFormat: 1,
    origin: BASE,
  });
}

describe("a deploy that runs one Durable Object on the old code", () => {
  function answerWithoutHistory(spaceId: string, without: boolean) {
    return runInDurableObject(space(spaceId), (instance: SpaceDO) => {
      const target = instance as unknown as { account: () => AccountStub; realAccount?: () => AccountStub };
      target.realAccount ??= target.account.bind(instance);
      const real = target.realAccount;
      target.account = !without
        ? real
        : () =>
            new Proxy(real(), {
              get(stub, prop) {
                const methods = stub as unknown as Record<string | symbol, (...args: unknown[]) => Promise<unknown>>;
                if (prop !== "checkSessions") return (...args: unknown[]) => methods[prop]!(...args);
                return async (...args: unknown[]) => {
                  const answers = (await methods.checkSessions!(...args)) as Record<string, Record<string, unknown>>;
                  return Object.fromEntries(Object.entries(answers).map(([id, { historyId: _old, ...check }]) => [id, check]));
                };
              },
            });
    });
  }

  async function upgrade(spaceId: string, token: string) {
    const response = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/ws`, { headers: { Upgrade: "websocket", Authorization: "Bearer " + token } });
    await response.arrayBuffer();
    response.webSocket?.accept();
    response.webSocket?.close();
    return response.status;
  }

  for (const old of [false, true]) {
    it(`asks an Account DO that answers ${old ? "without" : "with"} the space's history again on every connect, and serves once it answers`, async () => {
      const { spaceId, creds } = await newSpace();
      const token = creds.access!.token;
      await answerWithoutHistory(spaceId, old);
      for (let i = 0; i < 2; i++) {
        expect(await upgrade(spaceId, token)).toBe(old ? 503 : 101);
        const listed = await rpc(token, spaceId, { case: "listTree", value: {} });
        if (old) expect(listed.result).toMatchObject({ case: "error", value: { code: Error_Code.RETRY_LATER } });
        else expect(listed.result?.case).not.toBe("error");
      }
      await answerWithoutHistory(spaceId, false);
      expect(await upgrade(spaceId, token)).toBe(101);
    });
  }

  it("keeps a space closed to a Space DO that asks without its history", async () => {
    const { spaceId, createdAtMs, creds, device } = await newSpace();
    const token = creds.access!.token;
    const history = await device().history();
    expect(await account().isSpaceClosed(spaceId, history)).toBe(false);
    expect(await account().isSpaceClosed(spaceId)).toBe(true);
    const p1 = createdAtMs + MINUTE;
    await snapshot(spaceId, p1);
    await setAccountClock(createdAtMs + 5 * MINUTE);
    const done = await rewind(token, spaceId, { restorePointMs: p1, expectedHistoryId: history });
    expect(await account().isSpaceClosed(spaceId, done.message!.historyId)).toBe(false);
    expect(await account().isSpaceClosed(spaceId, history)).toBe(true);
    expect(await account().isSpaceClosed(spaceId)).toBe(true);
  });

  it("closes a socket with a reason too long for a close frame, keeping the whole reason in its Goodbye", async () => {
    const { spaceId, creds } = await newSpace();
    const open = await socket(spaceId, creds.access!.token);
    expect(new TextEncoder().encode(RETIRED_HISTORY).length).toBeGreaterThan(123);
    await runInDurableObject(space(spaceId), (instance: SpaceDO, state) => {
      const target = instance as unknown as { goodbye(ws: WebSocket, code: number, reason: string): void };
      for (const ws of state.getWebSockets()) target.goodbye(ws, Close.TRY_AGAIN_LATER, RETIRED_HISTORY);
    });
    await open.until(() => open.state.closed);
    expect(open.state.closed).toBe(Close.TRY_AGAIN_LATER);
    expect(open.received.at(-1)).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER, reason: RETIRED_HISTORY } });
  });
});

type AccountStub = ReturnType<typeof account>;

