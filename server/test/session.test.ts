// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  DeviceInfo_Kind,
  DeviceListSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  RewindRequestSchema,
  ServerInfoSchema,
  SpaceInfoSchema,
  type SpaceList,
  SpaceListSchema,
  TokenRequestSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { CommitResultSchema, OpResult_Code, type UploadReceipt, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import {
  type ClientMessage,
  ClientMessageSchema,
  Notice_Kind,
  ResyncRequired_Reason,
  RpcRequestSchema,
  RpcResponseSchema,
  type ServerMessage,
  ServerMessageSchema,
  type Welcome,
} from "../src/gen/koidrive/v1/messages_pb";
import { ChunkRefSchema, type CommitRecord, ContentSchema } from "../src/gen/koidrive/v1/model_pb";
import { ChunkBundleResultSchema, ChunkBundleSchema, ChunkPlan_Action, Error_Cause, Error_Code, ErrorSchema, type Target } from "../src/gen/koidrive/v1/rpc_pb";
import { type Identity, type ServerKey, signAccessToken, verifyAccessToken } from "../src/auth";
import { fromBase64url, sha256 } from "../src/crypto";
import { ShareLinkListSchema } from "../src/gen/koidrive/v1/share_pb";
import { toHex, uuidv7, uuidv7Ms } from "../src/ids";
import { type Principal, policy } from "../src/policy";
import { Close, HOLD_MS, type SpaceDO } from "../src/space-do";
import { CallFailure } from "../src/space/commit";
import { MIGRATIONS as SPACE_MIGRATIONS } from "../src/space/schema";
import { chunkKey, encodeChunkList } from "../src/space/store";
import worker from "../src/worker";
import { countBundledChunks, decodeChunkBundle } from "../src/chunk-bundle";
import { BadRequest } from "../src/http";
import { type AccountDO } from "../src/account-do";
import { SESSION_LEASE_MS } from "../src/account/account";
import { content, freezeClock, op, storedObjects, targetIncarnation } from "./helpers";

const BASE = "https://koidrive.test";

afterEach(() => {
  vi.restoreAllMocks();
});

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

async function call<Res extends DescMessage>(
  path: string,
  init: { method?: string; token?: string; body?: Uint8Array },
  responseSchema: Res,
): Promise<{ status: number; message?: MessageShape<Res> }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return { status: response.status, message: response.ok ? fromBinary(responseSchema, bytes) : undefined };
}

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

async function send(path: string, init: { method?: string; token?: string; body?: Uint8Array }) {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  const error = !response.ok && response.headers.get("Content-Type") === "application/x-protobuf";
  return { status: response.status, code: error ? fromBinary(ErrorSchema, bytes).code : undefined };
}

let owner: DeviceCredentials | undefined;

async function getOwner(): Promise<DeviceCredentials> {
  owner ??= await claimOwner();
  return owner;
}

async function claimOwner(): Promise<DeviceCredentials> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  const code = line!.split(": ")[1]!;
  const claimed = await call("/v1/claim", { body: body(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop A", platform: "macos" }) }, DeviceCredentialsSchema);
  return claimed.message!;
}

async function pairDevice(inviter: DeviceCredentials, deviceName: string): Promise<DeviceCredentials> {
  const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
  const paired = await call("/v1/pair/complete", { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform: "linux" }) }, DeviceCredentialsSchema);
  return paired.message!;
}

async function createSpace(owner: DeviceCredentials): Promise<string> {
  const created = await call("/v1/spaces", { token: owner.access!.token, body: body(CreateSpaceRequestSchema, { name: `Home ${uuidv7()}` }) }, SpaceInfoSchema);
  expect(created.status).toBe(201);
  return created.message!.spaceId;
}

function passLeases(spaceId: string) {
  return runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
    const offsetMs = instance.clock() - Date.now() + SESSION_LEASE_MS + 1000;
    instance.clock = () => Date.now() + offsetMs;
  });
}

interface CommitToken {
  commitId: string;
  committedAtMs: bigint;
}

function tokenOf(record: CommitRecord): CommitToken {
  return { commitId: record.commitId, committedAtMs: record.committedAtMs };
}

class Socket {
  private readonly queue: ServerMessage[] = [];
  private readonly texts: string[] = [];
  private wake: (() => void) | undefined;
  private nextReq = 1n;
  closed: { code: number; reason: string } | undefined;
  historyId = "";
  acceptedSeq = 0n;

  private constructor(readonly ws: WebSocket) {
    ws.addEventListener("message", (event) => {
      if (typeof event.data === "string") this.texts.push(event.data);
      else this.queue.push(fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer)));
      this.wake?.();
    });
    ws.addEventListener("close", (event) => {
      this.closed = { code: event.code, reason: event.reason };
      this.wake?.();
    });
  }

  static async open(spaceId: string, token: string | undefined): Promise<Socket | number> {
    const headers: Record<string, string> = { Upgrade: "websocket" };
    if (token) headers.Authorization = "Bearer " + token;
    const response = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/ws", { headers });
    if (!response.webSocket) return response.status;
    response.webSocket.binaryType = "arraybuffer";
    response.webSocket.accept();
    return new Socket(response.webSocket);
  }

  static async connect(spaceId: string, creds: DeviceCredentials): Promise<Socket> {
    const socket = await Socket.open(spaceId, creds.access!.token);
    if (typeof socket === "number") throw new Error("upgrade failed with " + socket);
    return socket;
  }

  send(msg: MessageInitShape<typeof ClientMessageSchema>["msg"]): void {
    this.ws.send(toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg } as MessageInitShape<typeof ClientMessageSchema>)));
  }

  private async until<T>(take: () => T | undefined, what: string): Promise<T> {
    const deadline = Date.now() + 3000;
    for (;;) {
      const value = take();
      if (value !== undefined) return value;
      if (this.closed) throw new Error("socket closed while waiting for " + what);
      if (Date.now() > deadline) throw new Error("timed out waiting for " + what);
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, 50);
        this.wake = () => {
          clearTimeout(timer);
          resolve();
        };
      });
    }
  }

  next(): Promise<ServerMessage["msg"]> {
    return this.until(() => this.queue.shift()?.msg, "a message");
  }

  text(): Promise<string> {
    return this.until(() => this.texts.shift(), "a text frame");
  }

  async waitClosed(): Promise<{ code: number; reason: string }> {
    return this.until(() => this.closed, "close");
  }

  async hello(maxFormat = 1): Promise<Welcome> {
    this.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat } });
    const msg = await this.next();
    if (msg.case !== "welcome") throw new Error("expected Welcome, got " + msg.case);
    this.historyId = msg.value.historyId;
    this.acceptedSeq = msg.value.deviceSeq;
    return msg.value;
  }

  async resume(
    historyId: string,
    sinceSeq: bigint,
    since: CommitToken = { commitId: "", committedAtMs: 0n },
  ): Promise<{ records: CommitRecord[]; head: bigint; token: CommitToken }> {
    this.send({ case: "resume", value: { historyId, sinceSeq, sinceCommitId: since.commitId, sinceCommittedAtMs: since.committedAtMs } });
    const records: CommitRecord[] = [];
    for (;;) {
      const msg = await this.next();
      if (msg.case === "changes") records.push(...msg.value.commits);
      else if (msg.case === "caughtUp") {
        return { records, head: msg.value.headSeq, token: { commitId: msg.value.headCommitId, committedAtMs: msg.value.headCommittedAtMs } };
      }
      else throw new Error("unexpected " + msg.case);
    }
  }

  async rpc(call: MessageInitShape<typeof RpcRequestSchema>["call"]) {
    const reqId = this.nextReq++;
    this.send({ case: "rpc", value: { reqId, call } });
    const msg = await this.next();
    if (msg.case !== "rpc" || msg.value.reqId !== reqId) throw new Error("expected RpcResponse " + reqId + ", got " + msg.case);
    return msg.value.result;
  }

  slot() {
    return { historyId: this.historyId, deviceSeq: this.acceptedSeq + 1n };
  }

  async commit(baseSeq: bigint, ops: ReturnType<typeof op.mkdir>[], receipts: MessageInitShape<typeof UploadReceiptSchema>[] = []) {
    const slot = this.slot();
    const result = await this.rpc({ case: "commit", value: { commitId: uuidv7(), baseSeq, ops, receipts, ...slot } });
    if (result.case !== "commit") throw new Error("commit failed: " + (result.case === "error" ? result.value.message : result.case));
    this.acceptedSeq = slot.deviceSeq;
    return result.value;
  }

  async pushed(): Promise<CommitRecord[]> {
    const msg = await this.next();
    if (msg.case !== "changes") throw new Error("expected Changes, got " + msg.case);
    return msg.value.commits;
  }

  async start(): Promise<Welcome> {
    const welcome = await this.hello();
    await this.resume(welcome.historyId, 0n);
    return welcome;
  }
}

async function put(target: Target, bytes: Uint8Array): Promise<Response> {
  return exports.default.fetch(target.url, { method: target.method, body: bytes, headers: { "Content-Type": "application/x-protobuf" } });
}

async function prepared(socket: Socket, text: string) {
  const parts = await content(text);
  const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
  if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
  const receipts: UploadReceipt[] = [];
  for (const [i, chunkPlan] of plan.value.chunks.entries()) {
    if (chunkPlan.action !== ChunkPlan_Action.UPLOAD) continue;
    const response = await put(chunkPlan.target!, parts.bytes[i]!);
    expect(response.status).toBe(200);
    receipts.push(fromBinary(UploadReceiptSchema, new Uint8Array(await response.arrayBuffer())));
  }
  return { ...parts, plan: plan.value, receipts };
}

function nodeIds(records: CommitRecord[]): string[] {
  return records.flatMap((r) => r.changes.map((c) => (c.change.case === "node" ? c.change.value.nodeId : "")));
}

describe("owner claim, pairing and tokens", () => {
  it("claims the server once with the logged code, pairs a device and refreshes tokens", async () => {
    const before = await call("/v1/server", { method: "GET" }, ServerInfoSchema);
    expect(before.message!.claimed).toBe(false);
    const wrong = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(wrong.status).toBe(403);

    const owner = await getOwner();
    expect((await call("/v1/server", { method: "GET" }, ServerInfoSchema)).message!.claimed).toBe(true);
    expect((await exports.default.fetch(BASE + "/setup").then((r) => r.text())).includes("already has an owner")).toBe(true);
    const again = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(again.status).toBe(403);

    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    expect(started.message!.code).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}$/);
    const pairBody = body(PairCompleteRequestSchema, { code: started.message!.code.toLowerCase(), deviceName: "Desktop B", platform: "linux" });
    const paired = await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema);
    expect(paired.message!.userId).toBe(owner.userId);
    expect(paired.message!.deviceId).not.toBe(owner.deviceId);
    expect((await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema)).status).toBe(403);

    const refreshed = await call(
      "/v1/token",
      { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: paired.message!.refreshCredential }) },
      AccessTokenSchema,
    );
    expect(refreshed.message!.expiresAtMs).toBeGreaterThan(BigInt(Date.now()));
    const spaces = await call("/v1/spaces", { method: "GET", token: refreshed.message!.token }, SpaceListSchema);
    expect(spaces.status).toBe(200);

    const forged = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: "nope" }) }, AccessTokenSchema);
    expect(forged.status).toBe(401);
    expect((await call("/v1/spaces", { method: "GET" }, SpaceListSchema)).status).toBe(401);
    const token = owner.access!.token;
    const at = token.length - 10;
    const tampered = token.slice(0, at) + (token[at] === "A" ? "B" : "A") + token.slice(at + 1);
    expect((await call("/v1/spaces", { method: "GET", token: tampered }, SpaceListSchema)).status).toBe(401);
  });

  it("stops accepting a revoked device", async () => {
    const owner = await getOwner();
    const laptop = await pairDevice(owner, "Lost laptop");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, laptop);
    await socket.start();
    await account().revokeDevice(laptop.deviceId);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const refresh = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: laptop.deviceId, refreshCredential: laptop.refreshCredential }) }, AccessTokenSchema);
    expect(refresh.status).toBe(401);
  });

  it("refuses a revoked device's old access token on every path, also after the Space DO was evicted", async () => {
    const owner = await getOwner();
    const laptop = await pairDevice(owner, "Stolen laptop");
    const spaceId = await createSpace(owner);
    await account().revokeDevice(laptop.deviceId);
    expect(await Socket.open(spaceId, laptop.access!.token)).toBe(401);
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: laptop.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    expect(rpc.status).toBe(401);
    await evictDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)));
    expect(await Socket.open(spaceId, laptop.access!.token)).toBe(401);
  });

  it("keeps a revocation in the Space DO, so a missed Account DO check cannot reopen the space", async () => {
    const owner = await getOwner();
    const tablet = await pairDevice(owner, "Tablet");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await stub.closeDevice(tablet.deviceId);
    expect(await account().isDeviceActive(tablet.deviceId)).toBe(true);
    expect(await Socket.open(spaceId, tablet.access!.token)).toBe(401);
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: tablet.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    const result = rpc.message!.result;
    expect(result.case === "error" && result.value.code).toBe(Error_Code.UNAUTHENTICATED);
    const stored = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray());
    expect(stored).toEqual([{ device_id: tablet.deviceId }]);
  });

  it("closes a connection whose Reauth comes from a device revoked without a notice to the space", async () => {
    const owner = await getOwner();
    const phone = await pairDevice(owner, "Phone");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, phone);
    await socket.start();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), phone.deviceId);
    });
    await passLeases(spaceId);
    socket.send({ case: "reauth", value: { accessToken: phone.access!.token } });
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
  });

  it("refuses every socket message, a Commit included, once the Account DO revoked the device without a notice to the space", async () => {
    const owner = await getOwner();
    const watch = await pairDevice(owner, "Watch");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const live = await Socket.connect(spaceId, watch);
    await live.start();
    const fresh = await Socket.connect(spaceId, watch);
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), watch.deviceId);
    });
    expect(await account().isDeviceActive(watch.deviceId)).toBe(false);
    await passLeases(spaceId);

    fresh.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    expect(await fresh.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    live.send({ case: "rpc", value: { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "after revocation")] } } } });
    expect(await live.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    expect((await live.waitClosed()).code).toBe(Close.DEVICE_REVOKED);

    const stored = await runInDurableObject(stub, (_instance, state) => ({
      commits: state.storage.sql.exec("SELECT commit_id FROM commits").toArray(),
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(stored).toEqual({ commits: [], revoked: [{ device_id: watch.deviceId }] });
  });

  it("refuses a revoked device in the Space DO itself after eviction, without the Worker's check in front", async () => {
    const owner = await getOwner();
    const tv = await pairDevice(owner, "TV");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await stub.closeDevice(tv.deviceId);
    await evictDurableObject(stub);
    const identity = { userId: tv.userId, deviceId: tv.deviceId, deviceName: "TV", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const request = body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "x")] } } });
    const result = fromBinary(RpcResponseSchema, (await stub.httpRpc({ identity, origin: BASE }, request)).response).result;
    expect(result.case === "error" && result.value.code).toBe(Error_Code.UNAUTHENTICATED);
    const commits = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT seq FROM commits").toArray());
    expect(commits).toEqual([]);
  });

  it("closes only the asking socket with 1013 when the Account DO cannot answer, and keeps the space running", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Pad");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const asking = await Socket.connect(spaceId, pad);
    await asking.start();
    const other = await Socket.connect(spaceId, owner);
    await other.start();
    await passLeases(spaceId);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = prototype.checkSession;
    prototype.checkSession = async () => {
      throw new Error("injected Account DO failure");
    };
    try {
      asking.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      expect(await asking.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER } });
      expect((await asking.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
    } finally {
      prototype.checkSession = original;
    }
    expect((await other.rpc({ case: "listTree", value: {} })).case).toBe("tree");
    expect(other.closed).toBeUndefined();
    const revoked = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray());
    expect(revoked).toEqual([]);
  });

  it("migrates a schema-one database that already holds revoked_devices", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE meta SET value = 1 WHERE key = 'schema_version'");
    });
    await evictDurableObject(stub);
    const device = uuidv7();
    await stub.closeDevice(device);
    const after = await runInDurableObject(stub, (_instance, state) => ({
      version: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'schema_version'").one().value,
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(after).toEqual({ version: SPACE_MIGRATIONS.length, revoked: [{ device_id: device }] });
  });

  it("adds revoked_devices to a version-one Space database and keeps its data", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const socket = await Socket.connect(spaceId, owner);
    await socket.start();
    const dir = uuidv7();
    await socket.commit(0n, [op.mkdir(dir, spaceId, "kept")]);
    socket.ws.close(1000);
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("DROP TABLE revoked_devices");
      state.storage.sql.exec("UPDATE meta SET value = 1 WHERE key = 'schema_version'");
    });
    await evictDurableObject(stub);
    const device = uuidv7();
    await stub.closeDevice(device);
    const after = await runInDurableObject(stub, (_instance, state) => ({
      version: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'schema_version'").one().value,
      head: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value,
      commits: state.storage.sql.exec("SELECT seq FROM commits").toArray(),
      node: state.storage.sql.exec("SELECT name FROM nodes WHERE node_id = ?", dir).toArray(),
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(after).toEqual({ version: SPACE_MIGRATIONS.length, head: 1, commits: [{ seq: 1 }], node: [{ name: "kept" }], revoked: [{ device_id: device }] });
  });
});

describe("RFC 0006 principals, capabilities and device routes", () => {
  function refresh(creds: DeviceCredentials) {
    return call("/v1/token", { body: body(TokenRequestSchema, { deviceId: creds.deviceId, refreshCredential: creds.refreshCredential }) }, AccessTokenSchema);
  }

  function listDevices(token: string, cursor = "") {
    return call("/v1/devices" + (cursor ? "?cursor=" + cursor : ""), { method: "GET", token }, DeviceListSchema);
  }

  function updateDevice(deviceId: string, assignment: string, value: unknown) {
    return runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec(`UPDATE devices SET ${assignment} = ? WHERE device_id = ?`, value, deviceId);
    });
  }

  it("lists the user's principals in creation order with their issuer, and pages them by cursor", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Lister");
    expect(pad.credentialExpiresAtMs).toBe(0n);
    const listed = await listDevices(pad.access!.token);
    expect(listed.status).toBe(200);
    const byId = new Map(listed.message!.devices.map((device) => [device.deviceId, device]));
    expect(byId.get(owner.deviceId)).toMatchObject({ name: "Laptop A", platform: "macos", kind: DeviceInfo_Kind.DEVICE, issuedBy: "", current: false });
    expect(byId.get(pad.deviceId)).toMatchObject({
      name: "Lister",
      platform: "linux",
      kind: DeviceInfo_Kind.DEVICE,
      issuedBy: owner.deviceId,
      current: true,
      revokedAtMs: 0n,
      credentialExpiresAtMs: 0n,
    });
    expect(byId.get(pad.deviceId)!.createdAtMs).toBeGreaterThan(0n);
    const ids = listed.message!.devices.map((device) => device.deviceId);
    expect(ids.indexOf(owner.deviceId)).toBeLessThan(ids.indexOf(pad.deviceId));
    expect(ids.at(-1)).toBe(pad.deviceId);
    expect(listed.message!.nextCursor).toBe("");

    const extra = Array.from({ length: 150 }, () => uuidv7());
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, list_seq) VALUES (?, ?, 'Extra', 'linux', ?, ?, (SELECT MAX(list_seq) + 1 FROM devices))",
          id,
          owner.userId,
          Date.now(),
          new Uint8Array(32),
        );
      }
    });
    try {
      const pages: string[][] = [];
      let cursor = "";
      do {
        const page = await listDevices(pad.access!.token, cursor);
        pages.push(page.message!.devices.map((device) => device.deviceId));
        cursor = page.message!.nextCursor;
      } while (cursor);
      expect(pages[0]).toHaveLength(100);
      expect(pages.flat()).toEqual([...ids, ...extra]);
      expect((await send("/v1/devices?cursor=" + extra[0], { method: "GET", token: pad.access!.token })).status).toBe(400);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("finishes a listing that an AccountDO from before list_seq started, in device_id order", async () => {
    const owner = await getOwner();
    const extra = Array.from({ length: 150 }, () => uuidv7()).reverse();
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, list_seq) VALUES (?, ?, 'Extra', 'linux', ?, ?, (SELECT MAX(list_seq) + 1 FROM devices))",
          id,
          owner.userId,
          Date.now(),
          new Uint8Array(32),
        );
      }
    });
    try {
      const byDeviceId = await runInDurableObject(account(), (_instance, state) =>
        state.storage.sql.exec("SELECT device_id FROM devices WHERE user_id = ? ORDER BY device_id", owner.userId).toArray().map((row) => String(row.device_id)),
      );
      const olderFirstPage = byDeviceId.slice(0, 100);
      const continued: string[] = [];
      let cursor = olderFirstPage.at(-1)!;
      while (cursor) {
        const page = await account().listDevices({ userId: owner.userId, callerId: owner.deviceId, cursor });
        continued.push(...page.devices.map((device) => device.deviceId));
        cursor = page.nextCursor;
      }
      expect([...olderFirstPage, ...continued]).toEqual(byDeviceId);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("lists a device whose pairing was still storing it when a listing passed newer devices", async () => {
    const owner = await getOwner();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    let held = false;
    await runInDurableObject(account(), (instance) => {
      const target = instance as unknown as { accessToken: (...args: unknown[]) => Promise<unknown> };
      const accessToken = target.accessToken.bind(instance);
      vi.spyOn(target, "accessToken").mockImplementationOnce(async (...args) => {
        held = true;
        await gate;
        return accessToken(...args);
      });
    });
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const pending = call(
      "/v1/pair/complete",
      { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Slow", platform: "linux" }) },
      DeviceCredentialsSchema,
    );
    await vi.waitFor(() => expect(held).toBe(true));
    const newer: string[] = [];
    try {
      for (let i = 0; i < 101; i++) newer.push((await pairDevice(owner, "Newer")).deviceId);
      const first = await listDevices(owner.access!.token);
      expect(first.message!.nextCursor).not.toBe("");
      release();
      const paired = await pending;
      expect(paired.status).toBe(200);
      newer.push(paired.message!.deviceId);
      const listed = first.message!.devices.map((device) => device.deviceId);
      let cursor = first.message!.nextCursor;
      while (cursor) {
        const page = await listDevices(owner.access!.token, cursor);
        listed.push(...page.message!.devices.map((device) => device.deviceId));
        cursor = page.message!.nextCursor;
      }
      expect(listed).toEqual(expect.arrayContaining(newer));
      expect(new Set(listed).size).toBe(listed.length);
    } finally {
      release();
      await pending;
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("revokes a principal of the caller's user: its sockets close with 4003, its credential stops working, and a repeat changes nothing", async () => {
    const owner = await getOwner();
    const tv = await pairDevice(owner, "Revoked TV");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, tv);
    await socket.start();
    expect(await send("/v1/devices/" + tv.deviceId, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 204, code: undefined });
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    expect((await refresh(tv)).status).toBe(401);
    const listed = await listDevices(owner.access!.token);
    expect(listed.message!.devices.find((device) => device.deviceId === tv.deviceId)!.revokedAtMs).toBeGreaterThan(0n);
    expect((await send("/v1/devices/" + tv.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);

    const stranger = uuidv7();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec(
        "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash) VALUES (?, ?, 'Stranger', 'linux', ?, ?)",
        stranger,
        uuidv7(),
        Date.now(),
        new Uint8Array(32),
      );
    });
    for (const id of [uuidv7(), stranger]) {
      expect(await send("/v1/devices/" + id, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 404, code: Error_Code.NOT_FOUND });
    }
    const untouched = await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT revoked_at_ms FROM devices WHERE device_id = ?", stranger).one());
    expect(untouched.revoked_at_ms).toBeNull();

    expect((await send("/v1/devices", { method: "POST", token: owner.access!.token })).status).toBe(405);
    expect((await send("/v1/devices", { method: "GET" })).status).toBe(401);
  });

  it("refuses a lost device's refresh credential, old access token, socket and pairing code once another device revoked it (RFC 0009 section 2)", async () => {
    const owner = await getOwner();
    const lost = await pairDevice(owner, "Lost laptop");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, lost);
    await socket.start();
    const pairing = await call("/v1/pair/start", { token: lost.access!.token }, PairStartResponseSchema);
    expect((await send("/v1/spaces", { method: "GET", token: lost.access!.token })).status).toBe(200);

    expect(await send("/v1/devices/" + lost.deviceId, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 204, code: undefined });
    const unauthenticated = { status: 401, code: Error_Code.UNAUTHENTICATED };
    expect(await send("/v1/spaces", { method: "GET", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/devices", { method: "GET", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/pair/start", { token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/devices/" + owner.deviceId, { method: "DELETE", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/token", { body: body(TokenRequestSchema, { deviceId: lost.deviceId, refreshCredential: lost.refreshCredential }) })).toEqual(unauthenticated);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const invitee = body(PairCompleteRequestSchema, { code: pairing.message!.code, deviceName: "Thief's laptop", platform: "linux" });
    expect(await send("/v1/pair/complete", { body: invitee })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
    expect((await refresh(owner)).status).toBe(200);
  });

  it("lets a device revoke itself when it signs out, and refuses the last active device until it confirms (RFC 0009 section 5)", async () => {
    const owner = await getOwner();
    const spare = await pairDevice(owner, "Spare");
    expect(await send("/v1/devices/" + spare.deviceId, { method: "DELETE", token: spare.access!.token })).toEqual({ status: 204, code: undefined });
    expect((await refresh(spare)).status).toBe(401);

    const active = (await listDevices(owner.access!.token)).message!.devices.filter((device) => device.revokedAtMs === 0n && !device.current);
    for (const device of active) {
      expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
    }
    const listed = (await listDevices(owner.access!.token)).message!.devices;
    expect(listed.filter((device) => device.revokedAtMs === 0n).map((device) => device.deviceId)).toEqual([owner.deviceId]);

    for (const query of ["", "?confirm=yes"]) {
      const refused = await send("/v1/devices/" + owner.deviceId + query, { method: "DELETE", token: owner.access!.token });
      expect(refused).toEqual({ status: 409, code: Error_Code.CONFIRMATION_REQUIRED });
    }
    const after = (await listDevices(owner.access!.token)).message!.devices.find((device) => device.deviceId === owner.deviceId)!;
    expect(after.revokedAtMs).toBe(0n);
    expect((await refresh(owner)).status).toBe(200);
  });

  it("revokes the user's last active device once it confirms, and of two last devices signing out at once the second must confirm (RFC 0009 section 5)", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("last-device"));
    await runInDurableObject(stub, async (instance: import("../src/account-do").AccountDO) => {
      const code = (await instance.issueClaimCode())!;
      const claimed = await instance.claim({ code, userName: "Owner", deviceName: "Only", platform: "macos" });
      if (!claimed.ok) throw new Error(claimed.message);
      const only = claimed.value;
      const pairing = await instance.startPairing(only.deviceId);
      if (!pairing.ok) throw new Error(pairing.message);
      const paired = await instance.completePairing({ code: pairing.value.code, deviceName: "Second", platform: "linux" }, "test-client");
      if (!paired.ok) throw new Error(paired.message);
      const second = paired.value;
      const revoke = (device: DeviceCredentials, confirmLastDevice: boolean) =>
        instance.revokeUserDevice({ userId: only.userId, callerId: device.deviceId, deviceId: device.deviceId, confirmLastDevice });

      const both = await Promise.all([revoke(only, false), revoke(second, false)]);
      expect(both.map((outcome) => (outcome.ok ? "revoked" : Error_Code[outcome.code])).sort()).toEqual(["CONFIRMATION_REQUIRED", "revoked"]);
      const last = (await instance.isDeviceActive(only.deviceId)) ? only : second;
      expect(await instance.isDeviceActive(last.deviceId)).toBe(true);

      expect(await revoke(last, true)).toMatchObject({ ok: true });
      expect(await instance.isDeviceActive(last.deviceId)).toBe(false);
      expect(await instance.refreshAccess({ deviceId: last.deviceId, refreshCredential: last.refreshCredential })).toMatchObject({
        ok: false,
        code: Error_Code.UNAUTHENTICATED,
      });
    });
  });

  it("lets only one of two devices that revoke each other at the same time succeed, so one stays active (RFC 0009 section 5)", async () => {
    const owner = await getOwner();
    const [first, second] = [await pairDevice(owner, "First"), await pairDevice(owner, "Second")];
    const replies = await Promise.all([
      send("/v1/devices/" + second.deviceId, { method: "DELETE", token: first.access!.token }),
      send("/v1/devices/" + first.deviceId, { method: "DELETE", token: second.access!.token }),
    ]);
    expect(replies.map((reply) => reply.status).sort()).toEqual([204, 401]);
    const listed = (await listDevices(owner.access!.token)).message!.devices;
    const active = listed.filter((device) => [first.deviceId, second.deviceId].includes(device.deviceId) && device.revokedAtMs === 0n);
    expect(active).toHaveLength(1);
  });

  it("denies everything to a principal whose kind or scope this server does not know", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    for (const [assignment, value] of [["kind", "agent"], ["scope", "photos"]] as const) {
      const pad = await pairDevice(owner, "Unknown " + assignment);
      const socket = await Socket.connect(spaceId, pad);
      await socket.start();
      await updateDevice(pad.deviceId, assignment, value);
      await passLeases(spaceId);
      expect((await refresh(pad)).status, assignment).toBe(401);
      expect(await send("/v1/spaces", { method: "GET", token: pad.access!.token })).toEqual({ status: 401, code: Error_Code.UNAUTHENTICATED });
      expect((await send("/v1/pair/start", { token: pad.access!.token })).status).toBe(401);
      expect(await Socket.open(spaceId, pad.access!.token)).toBe(401);
      socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    }
    const inviter = await pairDevice(owner, "Inviter");
    const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
    await updateDevice(inviter.deviceId, "kind", "agent");
    const pairBody = body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Invitee", platform: "linux" });
    expect(await send("/v1/pair/complete", { body: pairBody })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
  });

  async function commitAsOwner(owner: DeviceCredentials, spaceId: string, historyId: string) {
    const deviceSeq = BigInt(Number((await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
      state.storage.sql.exec("SELECT COALESCE(MAX(device_seq), 0) AS seq FROM device_commits WHERE device_id = ?", owner.deviceId).one().seq)))) + 1n;
    const commit = { commitId: uuidv7(), historyId, deviceSeq, baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "Pushed " + deviceSeq)] };
    const result = await call(`/v1/spaces/${spaceId}/rpc`, { token: owner.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: commit } }) }, RpcResponseSchema);
    expect(result.message!.result.case).toBe("commit");
  }

  it("closes a passive connection with 4003 instead of pushing to it once its principal is denied", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const watcher = await Socket.connect(spaceId, owner);
    const { historyId } = await watcher.start();
    for (const [column, value] of [["kind", "agent"], ["scope", "photos"], ["expires_at_ms", 1]] as const) {
      const pad = await pairDevice(owner, "Passive " + column);
      const socket = await Socket.connect(spaceId, pad);
      await socket.start();
      await updateDevice(pad.deviceId, column, value);
      await passLeases(spaceId);
      await commitAsOwner(owner, spaceId, historyId);
      expect(await socket.next(), column).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      expect((await watcher.pushed()).length).toBe(1);
    }
  });

  it("closes passive connections with 1013 when the Account DO cannot check them before a push", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Unchecked");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    const { historyId } = await socket.start();
    const listed = await call(`/v1/spaces/${spaceId}/rpc`, { token: owner.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    expect(listed.message!.result.case).toBe("tree");
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = prototype.checkSessions;
    prototype.checkSessions = async () => {
      throw new Error("injected Account DO failure");
    };
    try {
      await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
        (instance as unknown as { leases: Map<string, unknown> }).leases.delete(pad.deviceId);
      });
      await commitAsOwner(owner, spaceId, historyId);
      expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER } });
    } finally {
      prototype.checkSessions = original;
    }
  });

  it("answers a device check the Account DO could not make with RETRY_LATER and the failure's cause, over HTTP RPC, the WebSocket upgrade and a share link", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Overloaded check");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = { checkSessions: prototype.checkSessions, shareResolve: prototype.shareResolve };
    const overloaded = async () => {
      throw Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true });
    };
    prototype.checkSessions = overloaded;
    prototype.shareResolve = overloaded;
    try {
      await runInDurableObject(stub, (instance: SpaceDO) => {
        (instance as unknown as { leases: Map<string, unknown> }).leases.clear();
      });
      const rpc = await call(`/v1/spaces/${spaceId}/rpc`, { token: pad.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
      expect(rpc.message!.result).toMatchObject({ case: "error", value: { code: Error_Code.RETRY_LATER, cause: Error_Cause.DO_OVERLOADED } });
      const upgrade = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/ws", { headers: { Upgrade: "websocket", Authorization: "Bearer " + pad.access!.token } });
      const refused = fromBinary(ErrorSchema, new Uint8Array(await upgrade.arrayBuffer()));
      expect([upgrade.status, refused.code, refused.cause]).toEqual([503, Error_Code.RETRY_LATER, Error_Cause.DO_OVERLOADED]);
      expect(await stub.shareContinue({ linkId: "link", client: "client" }, { historyId: "history", generation: 1 })).toMatchObject({
        ok: false,
        code: Error_Code.RETRY_LATER,
        status: 503,
        cause: Error_Cause.DO_OVERLOADED,
      });
    } finally {
      Object.assign(prototype, original);
    }
  });

  it("lets a principal back in once its row is supported again, because only a revocation is remembered", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Temporarily unknown");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    await socket.start();
    await updateDevice(pad.deviceId, "kind", "agent");
    await passLeases(spaceId);
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    await updateDevice(pad.deviceId, "kind", "device");
    await passLeases(spaceId);
    const reopened = await Socket.connect(spaceId, pad);
    await reopened.start();
    expect((await reopened.rpc({ case: "listTree", value: {} })).case).toBe("tree");
    const remembered = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
      state.storage.sql.exec("SELECT device_id FROM revoked_devices WHERE device_id = ?", pad.deviceId).toArray(),
    );
    expect(remembered).toEqual([]);
  });

  it("keeps a revocation notice that a space did not take, and delivers it on a repeat or from the alarm", async () => {
    const owner = await getOwner();
    const [repeated, alarmed] = [await pairDevice(owner, "Repeat"), await pairDevice(owner, "Alarm")];
    await createSpace(owner);
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(account(), async (instance: import("../src/account-do").AccountDO, state) => {
      const namespace = (instance as unknown as { env: Env }).env.SPACE;
      const delivered: string[] = [];
      let fail = true;
      const spy = vi.spyOn(namespace, "get").mockImplementation((() => ({
        metered: async (_method: string, [deviceId]: [string]) => {
          if (fail) throw new Error("injected close failure");
          delivered.push(deviceId);
          return { value: undefined };
        },
      })) as never);
      const pending = (deviceId: string) => state.storage.sql.exec("SELECT COUNT(*) AS n FROM revocation_notices WHERE device_id = ?", deviceId).one().n;
      instance.sessionLeaseMs = 0;
      try {
        for (const device of [repeated, alarmed]) {
          expect(await instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: device.deviceId, confirmLastDevice: false })).toMatchObject({ ok: true });
          expect(pending(device.deviceId)).toBeGreaterThan(0);
          expect((await instance.checkSession(device.deviceId)).revoked).toBe(true);
        }
        expect(await state.storage.getAlarm()).not.toBeNull();
        fail = false;
        await instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: repeated.deviceId, confirmLastDevice: false });
        expect(pending(repeated.deviceId)).toBe(0);
        expect(delivered).toContain(repeated.deviceId);
        state.storage.sql.exec("UPDATE revocation_notices SET next_try_ms = 0");
        await instance.alarm();
        expect(pending(alarmed.deviceId)).toBe(0);
        expect(delivered).toContain(alarmed.deviceId);
      } finally {
        spy.mockRestore();
        instance.sessionLeaseMs = SESSION_LEASE_MS;
      }
    });
  });

  it("arms the alarm when it stores a revocation, so a notice is delivered even if the object is evicted before delivering it", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Evicted before delivery");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    await socket.start();
    const stub = account();
    await runInDurableObject(stub, async (instance: import("../src/account-do").AccountDO, state) => {
      await state.storage.deleteAlarm();
      vi.spyOn(instance as unknown as { deliverNotices: () => Promise<void> }, "deliverNotices").mockRejectedValue(new Error("evicted before delivery"));
      await expect(instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: pad.deviceId, confirmLastDevice: false })).rejects.toThrow("evicted before delivery");
      expect(await state.storage.getAlarm()).not.toBeNull();
      state.storage.sql.exec("UPDATE revocation_notices SET next_try_ms = 0 WHERE device_id = ?", pad.deviceId);
    });
    await evictDurableObject(stub);
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const pending = await runInDurableObject(stub, (_instance, state) =>
      state.storage.sql.exec("SELECT COUNT(*) AS n FROM revocation_notices WHERE device_id = ?", pad.deviceId).one().n,
    );
    expect(pending).toBe(0);
  });

  it("never lets an access token outlive its credential, and refuses an expired credential", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Expiring");
    const expiresAtMs = Date.now() + 60_000;
    await updateDevice(pad.deviceId, "expires_at_ms", expiresAtMs);
    const token = await refresh(pad);
    expect(token.message!.expiresAtMs).toBe(BigInt(expiresAtMs));
    const listed = await listDevices(owner.access!.token);
    expect(listed.message!.devices.find((device) => device.deviceId === pad.deviceId)!.credentialExpiresAtMs).toBe(BigInt(expiresAtMs));
    await updateDevice(pad.deviceId, "expires_at_ms", Date.now() - 1);
    expect(await send("/v1/token", { body: body(TokenRequestSchema, { deviceId: pad.deviceId, refreshCredential: pad.refreshCredential }) })).toEqual({
      status: 401,
      code: Error_Code.UNAUTHENTICATED,
    });
    expect((await send("/v1/spaces", { method: "GET", token: token.message!.token })).status).toBe(401);
  });

  it("carries the principal's kind and scope in the token, reads a token without them as a full-scope device, and decides from the row", async () => {
    const owner = await getOwner();
    const claims = JSON.parse(new TextDecoder().decode(fromBase64url(owner.access!.token.split(".")[2]!)!));
    expect(claims).toMatchObject({ k: "device", s: null });
    const key = await account().serverKey();
    const nowMs = Date.now();
    const identity = { userId: owner.userId, deviceId: owner.deviceId, deviceName: "Laptop A", expiresAtMs: nowMs + 60_000 };
    const legacy = await signAccessToken(key, identity as Identity);
    expect(JSON.parse(new TextDecoder().decode(fromBase64url(legacy.split(".")[2]!)!))).not.toHaveProperty("k");
    expect(await verifyAccessToken(key, legacy, nowMs)).toEqual({ ...identity, kind: "device", scope: null });

    const claimed = await signAccessToken(key, { ...identity, kind: "agent", scope: "photos" });
    const asked: Principal[] = [];
    vi.spyOn(policy, "can").mockImplementation((principal) => {
      asked.push(principal);
      return true;
    });
    expect((await send("/v1/spaces", { token: claimed, body: body(CreateSpaceRequestSchema, { name: "Row decides" }) })).status).toBe(201);
    expect(asked).toEqual([expect.objectContaining({ deviceId: owner.deviceId, kind: "device", scope: null })]);
  });

  it("checks each capability before its action and refuses it with FORBIDDEN", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const spaceId = await createSpace(owner);
    const cases = [
      ["space.create", () => send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Refused" }) })],
      ["device.pair", () => send("/v1/pair/start", { token })],
      ["device.manage", () => send("/v1/devices", { method: "GET", token })],
      ["device.manage", () => send("/v1/devices/" + owner.deviceId, { method: "DELETE", token })],
      ["share.manage", () => send(`/v1/spaces/${spaceId}/links`, { method: "GET", token })],
      ["space.rewind", () => send(`/v1/spaces/${spaceId}/rewind`, { token, body: body(RewindRequestSchema, { restorePointMs: 1n, rewindId: uuidv7() }) })],
    ] as const;
    for (const [capability, request] of cases) {
      const can = vi.spyOn(policy, "can").mockImplementation((_principal, asked) => asked !== capability);
      expect(await request(), capability).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
      can.mockRestore();
    }
    expect((await refresh(owner)).status).toBe(200);
    expect((await call(`/v1/spaces/${spaceId}/links`, { method: "GET", token }, ShareLinkListSchema)).status).toBe(200);
  });
});

describe("RFC 0014 session leases", () => {
  async function withLease<T>(leaseMs: number, body: () => Promise<T>): Promise<T> {
    const set = (value: number) => runInDurableObject(account(), (instance: AccountDO) => {
      instance.sessionLeaseMs = value;
    });
    await set(leaseMs);
    try {
      return await body();
    } finally {
      await set(SESSION_LEASE_MS);
    }
  }

  async function loseNotices(spaceId: string): Promise<() => void> {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const original = prototype.closeDevice;
    prototype.closeDevice = async () => {
      throw new Error("injected lost revocation notice");
    };
    return () => {
      prototype.closeDevice = original;
    };
  }

  function revokeRowOnly(deviceId: string) {
    return runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), deviceId);
    });
  }

  function listTree(socket: Socket) {
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    return socket.next();
  }

  function ageLeases(spaceId: string, byMs: number) {
    return runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      freezeClock(instance, instance.clock() + byMs);
    });
  }

  async function holdAccount(method: "checkSession" | "checkSessions", held_calls = Infinity) {
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype[method] as (this: AccountDO, ...args: unknown[]) => Promise<unknown>;
    const asked: unknown[] = [];
    const { promise: held, resolve: release } = Promise.withResolvers<void>();
    (prototype as unknown as Record<string, unknown>)[method] = async function (this: AccountDO, ...args: unknown[]) {
      const call = asked.push(args[0]);
      const answer = await original.apply(this, args);
      if (call <= held_calls) await held;
      return answer;
    };
    return {
      asked,
      release,
      restore: () => {
        release();
        (prototype as unknown as Record<string, unknown>)[method] = original;
      },
    };
  }

  function renewalSettled(spaceId: string) {
    return vi.waitFor(() =>
      runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
        expect((instance as unknown as { renewing?: unknown }).renewing).toBeUndefined();
      }),
    );
  }

  async function connected(name: string) {
    const owner = await getOwner();
    const device = await pairDevice(owner, name);
    const spaceId = await createSpace(owner);
    await ageLeases(spaceId, 0);
    const socket = await Socket.connect(spaceId, device);
    await socket.start();
    return { owner, device, spaceId, socket };
  }



  async function slowAccount(delayMs: number) {
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const originals = { checkSession: prototype.checkSession, checkSessions: prototype.checkSessions };
    const calls = { checkSession: 0, checkSessions: 0 };
    const delay = () => new Promise((resolve) => setTimeout(resolve, delayMs));
    prototype.checkSession = async function (this: AccountDO, deviceId: string, spaceId?: string) {
      calls.checkSession++;
      await delay();
      return originals.checkSession.call(this, deviceId, spaceId);
    };
    prototype.checkSessions = async function (this: AccountDO, deviceIds: string[], spaceId: string) {
      calls.checkSessions++;
      await delay();
      return Object.fromEntries(await Promise.all(deviceIds.map(async (deviceId) => [deviceId, await originals.checkSession.call(this, deviceId, spaceId)])));
    };
    return {
      calls,
      restore: () => Object.assign(prototype, originals),
    };
  }

  let pool: DeviceCredentials[] | undefined;
  async function devicePool(): Promise<DeviceCredentials[]> {
    if (pool) return pool;
    const owner = await getOwner();
    const paired: DeviceCredentials[] = [];
    for (let i = 0; i < 24; i++) paired.push(await pairDevice(owner, "Pooled " + i));
    return (pool = paired);
  }

  async function steadyLoad(leaseMs: number, delayMs: number, forMs: number) {
    const spaceId = await createSpace(await getOwner());
    const creds = await devicePool();
    return withLease(leaseMs, async () => {
      const slow = await slowAccount(delayMs);
      try {
        const sockets = await Promise.all(creds.map((cred) => Socket.connect(spaceId, cred)));
        await Promise.all(sockets.map((socket) => socket.start()));
        const before = { ...slow.calls };
        const until = Date.now() + forMs;
        let replies = 0;
        await Promise.all(
          sockets.map(async (socket) => {
            while (Date.now() < until) if ((await listTree(socket)).case === "rpc") replies++;
          }),
        );
        for (const socket of sockets) socket.ws.close();
        return { replies, checks: slow.calls.checkSession - before.checkSession + slow.calls.checkSessions - before.checkSessions };
      } finally {
        slow.restore();
      }
    });
  }

  it("answers 24 devices from leases when checking them one at a time would outlast a lease", { timeout: 60_000 }, async () => {
    const { replies, checks } = await steadyLoad(1000, 50, 3000);
    expect(replies).toBeGreaterThan(1000);
    expect(checks * 100).toBeLessThan(replies);
  });

  it("checks 24 devices that connect at once without leases in a few Account DO calls", { timeout: 60_000 }, async () => {
    const spaceId = await createSpace(await getOwner());
    const creds = await devicePool();
    const slow = await slowAccount(50);
    try {
      const sockets = await Promise.all(creds.map((cred) => Socket.connect(spaceId, cred)));
      expect(slow.calls.checkSessions).toBeLessThanOrEqual(3);
      for (const socket of sockets) socket.ws.close();
    } finally {
      slow.restore();
    }
  });

  it("renews with a lapsed device's check every connected lease past half, and no lease short of half", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const [lapsed, pastHalf, fresh] = [await pairDevice(owner, "Lapsed"), await pairDevice(owner, "Past half"), await pairDevice(owner, "Fresh")];
    const sockets = await Promise.all([lapsed, pastHalf, fresh].map((device) => Socket.connect(spaceId, device)));
    for (const socket of sockets) await socket.start();
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      const leases = (instance as unknown as { leases: Map<string, { untilMs: number }> }).leases;
      leases.delete(lapsed.deviceId);
      leases.get(pastHalf.deviceId)!.untilMs = instance.clock() + SESSION_LEASE_MS * 0.4;
      leases.get(fresh.deviceId)!.untilMs = instance.clock() + SESSION_LEASE_MS * 0.9;
    });
    const check = await holdAccount("checkSessions");
    try {
      const reply = listTree(sockets[0]!);
      await vi.waitFor(() => expect(check.asked).toHaveLength(1));
      expect([...(check.asked[0] as string[])].sort()).toEqual([lapsed.deviceId, pastHalf.deviceId].sort());
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      check.restore();
    }
  });

  it("answers a Resume sent right after Hello by a socket without a lease in the order they were sent", async () => {
    const owner = await getOwner();
    const device = await pairDevice(owner, "Pipelined");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, device);
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      (instance as unknown as { leases: Map<string, unknown> }).leases.clear();
    });
    socket.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    socket.send({ case: "resume", value: { historyId: "none", sinceSeq: 0n, sinceCommitId: "", sinceCommittedAtMs: 0n } });
    expect((await socket.next()).case).toBe("welcome");
    expect((await socket.next()).case).not.toBe("goodbye");
  });

  it("renews in one blocking check every connected device whose lease has run out", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const devices = [await pairDevice(owner, "Lapsed A"), await pairDevice(owner, "Lapsed B"), await pairDevice(owner, "Lapsed C")];
    const sockets = await Promise.all(devices.map((device) => Socket.connect(spaceId, device)));
    for (const socket of sockets) await socket.start();
    await passLeases(spaceId);
    const check = await holdAccount("checkSessions");
    try {
      const reply = listTree(sockets[0]!);
      await vi.waitFor(() => expect(check.asked).toHaveLength(1));
      expect([...(check.asked[0] as string[])].sort()).toEqual(devices.map((device) => device.deviceId).sort());
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sockets[1]!)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sockets[2]!)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(check.asked).toHaveLength(1);
    } finally {
      check.restore();
    }
  });

  it("renews a lease past half its life in the background, so a commit does not wait for the Account DO", async () => {
    const { device, spaceId, socket } = await connected("Renewing writer");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), ...socket.slot(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "While renewing")] } } } });
      expect(await socket.next()).toMatchObject({ case: "rpc", value: { result: { case: "commit" } } });
      socket.acceptedSeq += 1n;
      await vi.waitFor(() => expect(renewal.asked).toEqual([[device.deviceId]]));
    } finally {
      renewal.restore();
    }
    await renewalSettled(spaceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype.checkSession;
    prototype.checkSession = async () => {
      throw new Error("injected Account DO outage");
    };
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      prototype.checkSession = original;
    }
  });

  it("renews only leases that an event used past half their life, so an idle device takes the cold path again", async () => {
    const { owner, device: writer, spaceId, socket } = await connected("Busy writer");
    const idle = await pairDevice(owner, "Idle reader");
    const idleSocket = await Socket.connect(spaceId, idle);
    await idleSocket.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toEqual([[writer.deviceId]]));
    } finally {
      renewal.restore();
    }
    await revokeRowOnly(idle.deviceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
    expect(await listTree(idleSocket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  });

  it("keeps one renewal in flight, and starts the next once it has answered", async () => {
    const { spaceId, socket } = await connected("One in flight");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(renewal.asked).toHaveLength(1);
      renewal.release();
      await renewalSettled(spaceId);
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
    } finally {
      renewal.restore();
    }
  });

  it("renews in one check every lease that was used past half while a renewal was in flight", async () => {
    const { owner, spaceId, socket } = await connected("Batch A");
    const b = await pairDevice(owner, "Batch B");
    const c = await pairDevice(owner, "Batch C");
    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    const sc = await Socket.connect(spaceId, c);
    await sc.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect(await listTree(sb)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sc)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      renewal.release();
      await renewalSettled(spaceId);
      expect(await listTree(sb)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
      expect([...(renewal.asked[1] as string[])].sort()).toEqual([b.deviceId, c.deviceId].sort());
    } finally {
      renewal.restore();
    }
  });

  it("does not renew a lease that was used past half but ended before the next renewal", async () => {
    const { owner, spaceId, socket } = await connected("Renewed writer");
    const x = await pairDevice(owner, "Ended before renewal");
    const sx = await Socket.connect(spaceId, x);
    await sx.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect(await listTree(sx)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      renewal.release();
      await renewalSettled(spaceId);
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
      expect(renewal.asked[1]).not.toContain(x.deviceId);
    } finally {
      renewal.restore();
    }
  });

  it("records a revocation that a renewal reports, so it outlasts the lease", async () => {
    const { device, spaceId, socket } = await connected("Revoked in renewal");
    await revokeRowOnly(device.deviceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    const revoked = () =>
      runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => state.storage.sql.exec("SELECT 1 FROM revoked_devices WHERE device_id = ?", device.deviceId).toArray().length);
    await vi.waitFor(async () => expect(await revoked()).toBe(1));
  });

  it("stores nothing from a failed renewal, so the lease runs out as before", async () => {
    const { device, spaceId, socket } = await connected("Failed renewal");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype.checkSessions;
    let calls = 0;
    prototype.checkSessions = async () => {
      calls++;
      throw new Error("injected renewal failure");
    };
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(calls).toBe(1));
    } finally {
      prototype.checkSessions = original;
    }
    await revokeRowOnly(device.deviceId);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
    expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  });

  it("sends a new renewal once a renewal that never answers has been out for a lease", async () => {
    const { spaceId, socket } = await connected("Hung renewal");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions", 1);
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(renewal.asked).toHaveLength(2);
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(3));
    } finally {
      renewal.restore();
    }
  });

  it("still waits for the Account DO once a lease has run out", async () => {
    const { spaceId, socket } = await connected("Expired lease");
    await passLeases(spaceId);
    const check = await holdAccount("checkSession");
    try {
      const reply = listTree(socket);
      const early = await Promise.race([reply.then(() => "answered"), new Promise((resolve) => setTimeout(() => resolve("waiting"), 300))]);
      expect(early).toBe("waiting");
      expect(check.asked).toHaveLength(1);
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      check.restore();
    }
  });

  it("refuses a device whose revocation notice arrived while a renewal of its lease was in flight", async () => {
    const { owner, device, spaceId, socket } = await connected("Revoked while renewing");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    const identity = { userId: device.userId, deviceId: device.deviceId, deviceName: "Revoked while renewing", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const ask = async () =>
      fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }))).response).result;
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
      expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
      renewal.release();
      await renewalSettled(spaceId);
      expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
    } finally {
      renewal.restore();
    }
  });

  it("keeps the answer to the check sent last when a renewal from before a cold check arrives after it", async () => {
    const { device, spaceId, socket } = await connected("Renewal overtaken");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions", 1);
    const identity = { userId: device.userId, deviceId: device.deviceId, deviceName: "Renewal overtaken", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const ask = async () =>
      fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }))).response).result;
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await runInDurableObject(account(), (_instance, state) => {
        state.storage.sql.exec("UPDATE devices SET expires_at_ms = ? WHERE device_id = ?", Date.now() - 1000, device.deviceId);
      });
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
      expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      renewal.release();
      await renewalSettled(spaceId);
      expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
    } finally {
      renewal.restore();
    }
  });

  it("ends a lease renewed from an answer that predates a revocation by the 204, when the space did not take the notice", async () => {
    await withLease(1000, async () => {
      const { owner, device, spaceId, socket } = await connected("Renewed before revocation");
      await ageLeases(spaceId, 600);
      const renewal = await holdAccount("checkSessions");
      const restore = await loseNotices(spaceId);
      try {
        expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
        await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
        const sentAtMs = Date.now();
        expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
        await ageLeases(spaceId, Date.now() - sentAtMs);
        renewal.release();
        await renewalSettled(spaceId);
        expect(socket.closed).toBeUndefined();
        expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      } finally {
        restore();
        renewal.restore();
      }
    });
  }, 30_000);

  it("lets no commit through that a device sends after the 204 of its revocation, during a burst of its commits", async () => {
    await withLease(1000, async () => {
      for (const notice of ["delivered", "lost"]) {
        const owner = await getOwner();
        const writer = await pairDevice(owner, "Burst " + notice);
        const spaceId = await createSpace(owner);
        const socket = await Socket.connect(spaceId, writer);
        await socket.start();
        const restore = notice === "lost" ? await loseNotices(spaceId) : () => {};
        try {
          let answered = false;
          const revoked = send("/v1/devices/" + writer.deviceId, { method: "DELETE", token: owner.access!.token }).then((reply) => {
            answered = true;
            return reply;
          });
          const late: string[] = [];
          for (let i = 1; i < 10_000; i++) {
            const commitId = uuidv7();
            if (answered) late.push(commitId);
            socket.send({ case: "rpc", value: { reqId: BigInt(i), call: { case: "commit", value: { commitId, ...socket.slot(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "Burst " + i)] } } } });
            const reply = await socket.next().catch(() => undefined);
            if (reply?.case !== "rpc" || reply.value.result.case !== "commit") break;
            socket.acceptedSeq += 1n;
          }
          expect((await revoked).status, notice).toBe(204);
          expect((await socket.waitClosed()).code, notice).toBe(Close.DEVICE_REVOKED);
          const stored = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
            state.storage.sql.exec("SELECT commit_id FROM commits").toArray().map((row) => String(row.commit_id)),
          );
          expect(stored.filter((commitId) => late.includes(commitId)), notice).toEqual([]);
        } finally {
          restore();
        }
      }
    });
  }, 30_000);

  it("holds the 204 of a revocation whose notice a space did not take until the space's lease has run out", async () => {
    await withLease(1000, async () => {
      const owner = await getOwner();
      const laptop = await pairDevice(owner, "Unreachable space");
      const spaceId = await createSpace(owner);
      const socket = await Socket.connect(spaceId, laptop);
      await socket.start();
      const restore = await loseNotices(spaceId);
      try {
        const startedAtMs = Date.now();
        expect((await send("/v1/devices/" + laptop.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
        expect(Date.now() - startedAtMs).toBeGreaterThanOrEqual(1000);
        expect(socket.closed).toBeUndefined();
        expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      } finally {
        restore();
      }
    });
  }, 30_000);

  it("asks the Account DO again once a lease has run out, and ends a lease when the device's credential expires", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const revoked = await pairDevice(owner, "Revoked unseen");
    const socket = await Socket.connect(spaceId, revoked);
    await socket.start();
    await revokeRowOnly(revoked.deviceId);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    await passLeases(spaceId);
    expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });

    const expiring = await pairDevice(owner, "Expiring");
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET expires_at_ms = ? WHERE device_id = ?", Date.now() + 2000, expiring.deviceId);
    });
    const fresh = await createSpace(owner);
    const agent = await Socket.connect(fresh, expiring);
    await agent.start();
    await new Promise((resolve) => setTimeout(resolve, 2500));
    expect(await listTree(agent)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  }, 30_000);

  it("starts a restarted Space DO with no leases", async () => {
    const owner = await getOwner();
    const tablet = await pairDevice(owner, "Restarted space");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const identity = { userId: tablet.userId, deviceId: tablet.deviceId, deviceName: "Tablet", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const request = body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } });
    const ask = async () => fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, request)).response).result;
    expect((await ask()).case).toBe("tree");
    await revokeRowOnly(tablet.deviceId);
    expect((await ask()).case).toBe("tree");
    await evictDurableObject(stub);
    expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
  });
});

describe("RFC 0007 name limits and space list pages", () => {
  const refused = { status: 400, code: Error_Code.INVALID_ARGUMENT };
  const longest = "가".repeat(85);
  const tooLong = longest + "a";

  function listSpaces(token: string, cursor = "") {
    return call("/v1/spaces" + (cursor ? "?cursor=" + cursor : ""), { method: "GET", token }, SpaceListSchema);
  }

  async function spacePages(token: string, from = ""): Promise<SpaceList[]> {
    const pages: SpaceList[] = [];
    let cursor = from;
    do {
      const page = await listSpaces(token, cursor);
      expect(page.status).toBe(200);
      pages.push(page.message!);
      cursor = page.message!.nextCursor;
    } while (cursor);
    return pages;
  }

  it("refuses a claim whose user name, device name or platform is longer than 255 bytes before it checks the code", async () => {
    await getOwner();
    const claim = (init: MessageInitShape<typeof ClaimRequestSchema>) => send("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", ...init }) });
    expect(await claim({ userName: tooLong })).toEqual(refused);
    expect(await claim({ deviceName: tooLong })).toEqual(refused);
    expect(await claim({ platform: tooLong })).toEqual(refused);
    expect(await claim({ userName: longest, deviceName: longest, platform: longest })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
  });

  it("refuses a pairing whose device name or platform is longer than 255 bytes, and keeps the pairing code valid", async () => {
    const owner = await getOwner();
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const complete = (deviceName: string, platform: string) =>
      body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform });
    expect(await send("/v1/pair/complete", { body: complete(tooLong, "linux") })).toEqual(refused);
    expect(await send("/v1/pair/complete", { body: complete("Desk", tooLong) })).toEqual(refused);
    const paired = await call("/v1/pair/complete", { body: complete(longest, longest) }, DeviceCredentialsSchema);
    expect(paired.status).toBe(200);
    const listed = await call("/v1/devices", { method: "GET", token: paired.message!.access!.token }, DeviceListSchema);
    expect(listed.message!.devices.find((device) => device.current)).toMatchObject({ name: longest, platform: longest });
  });

  it("gives a paired device its user's name, and keeps its credentials within 4 KiB with the longest user and device names", async () => {
    const owner = await getOwner();
    expect(owner.userName).toBe("Owner");
    expect((await pairDevice(owner, "Phone")).userName).toBe("Owner");
    const setName = (name: string) =>
      runInDurableObject(account(), (_instance, state) => {
        state.storage.sql.exec("UPDATE users SET name = ? WHERE user_id = ?", name, owner.userId);
      });
    await setName("\u0001".repeat(255));
    try {
      const paired = await pairDevice(owner, "\u0001".repeat(255));
      expect(paired.userName).toBe("\u0001".repeat(255));
      expect(toBinary(DeviceCredentialsSchema, paired).length).toBeLessThanOrEqual(4096);
    } finally {
      await setName("Owner");
    }
  });

  it("keeps the credentials and the access token of a device with the longest name within 4 KiB", async () => {
    const owner = await getOwner();
    const paired = await pairDevice(owner, "\u0001".repeat(255));
    expect(toBinary(DeviceCredentialsSchema, paired).length).toBeLessThanOrEqual(4096);
    const refreshed = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: paired.deviceId, refreshCredential: paired.refreshCredential }) }, AccessTokenSchema);
    expect(refreshed.status).toBe(200);
    expect(toBinary(AccessTokenSchema, refreshed.message!).length).toBeLessThanOrEqual(4096);
  });

  it("refuses a space name longer than 255 bytes and creates one of exactly 255", async () => {
    const token = (await getOwner()).access!.token;
    expect(await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: tooLong }) })).toEqual(refused);
    const created = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: longest }) }, SpaceInfoSchema);
    expect(created.status).toBe(201);
    expect(created.message!.name).toBe(longest);
  });

  it("refuses a space name the owner already uses, compared exactly after trimming, when two creates race", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Shared ${uuidv7()}`;
    const createNamed = (text: string) => send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: text }) });
    const raced = await Promise.all([createNamed(name), createNamed(name)]);
    expect(raced.map((outcome) => outcome.status).sort()).toEqual([201, 409]);
    expect(await createNamed(` ${name}  `)).toEqual({ status: 409, code: Error_Code.INVALID_ARGUMENT });
    expect((await createNamed(name.toUpperCase())).status).toBe(201);
    const listed = (await spacePages(token)).flatMap((page) => page.spaces);
    expect(listed.filter((space) => space.name === name)).toHaveLength(1);
  });

  it("frees a space name again when the space behind it cannot be set up", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Broken ${uuidv7()}`;
    await runInDurableObject(account(), (instance) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      vi.spyOn(spaces, "get").mockImplementationOnce(
        () => ({ init: () => Promise.reject(new Error("the Space DO is unavailable")) }) as unknown as ReturnType<typeof spaces.get>,
      );
    });
    vi.spyOn(console, "error").mockImplementation(() => {});
    expect(await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) })).toEqual({ status: 500, code: Error_Code.INTERNAL });
    const listed = (await spacePages(token)).flatMap((page) => page.spaces);
    expect(listed.some((space) => space.name === name)).toBe(false);
    const created = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) }, SpaceInfoSchema);
    expect(created.status).toBe(201);
  });

  it("frees a space name when the Account DO aborts during the create, after its storage was flushed", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Interrupted ${uuidv7()}`;
    await runInDurableObject(account(), (instance, state) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      vi.spyOn(spaces, "get").mockImplementationOnce(
        () =>
          ({
            init: async () => {
              await state.storage.sync();
              state.abort("interrupted while the Space DO was being set up");
            },
          }) as unknown as ReturnType<typeof spaces.get>,
      );
    });
    await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) }).catch(() => undefined);
    expect((await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) })).status).toBe(201);
  });

  it("lists a space whose create was still storing it when a listing passed newer spaces", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const { promise: gate, resolve: release } = Promise.withResolvers<void>();
    const { promise: storing, resolve: entered } = Promise.withResolvers<string>();
    await runInDurableObject(account(), (instance) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      const get = spaces.get.bind(spaces);
      vi.spyOn(spaces, "get").mockImplementationOnce((id) => {
        const stub = get(id);
        return {
          metered: (method: string, args: [Parameters<typeof stub.init>[0]]) => {
            entered(args[0].spaceId);
            return gate.then(() => stub.metered(method, args));
          },
        } as unknown as typeof stub;
      });
    });
    const pending = call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Slow" }) }, SpaceInfoSchema);
    const newerMs = uuidv7Ms(await storing) + 1;
    const newer = Array.from({ length: 101 }, () => uuidv7(newerMs));
    try {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) {
          state.storage.sql.exec(
            "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version, list_seq) VALUES (?, ?, ?, ?, 1, (SELECT COALESCE(MAX(list_seq), 0) + 1 FROM spaces))",
            id,
            `Newer ${id}`,
            owner.userId,
            newerMs,
          );
        }
      });
      const first = await listSpaces(token);
      expect(first.message!.nextCursor).not.toBe("");
      expect(first.message!.spaces.some((space) => newer.includes(space.spaceId))).toBe(true);
      release();
      const created = await pending;
      expect(created.status).toBe(201);
      const later = await spacePages(token, first.message!.nextCursor);
      const listed = [...first.message!.spaces, ...later.flatMap((page) => page.spaces)].map((space) => space.spaceId);
      expect(listed).toContain(created.message!.spaceId);
      expect(listed).toEqual(expect.arrayContaining(newer));
      expect(new Set(listed).size).toBe(listed.length);
    } finally {
      release();
      await pending;
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) state.storage.sql.exec("DELETE FROM spaces WHERE space_id = ?", id);
      });
    }
  });

  it("lists the user's spaces in creation order and pages them by cursor, each page within 32 KiB", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    expect(await send("/v1/spaces?cursor=" + uuidv7(), { method: "GET", token })).toEqual(refused);
    const before = (await spacePages(token)).flatMap((page) => page.spaces.map((space) => space.spaceId));
    const extra = Array.from({ length: 150 }, () => uuidv7());
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version, list_seq) VALUES (?, ?, ?, ?, 1, (SELECT MAX(list_seq) + 1 FROM spaces))",
          id,
          "\u0001".repeat(255),
          owner.userId,
          Date.now(),
        );
      }
    });
    try {
      const pages = await spacePages(token);
      expect(pages[0]!.spaces).toHaveLength(100);
      expect(pages.flatMap((page) => page.spaces.map((space) => space.spaceId))).toEqual([...before, ...extra]);
      for (const page of pages) expect(toBinary(SpaceListSchema, page).length).toBeLessThanOrEqual(32 * 1024);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM spaces WHERE space_id = ?", id);
      });
    }
  });
});

describe("WebSocket session", () => {
  async function twoDevices() {
    const a = await getOwner();
    const b = await pairDevice(a, "Desktop B");
    const spaceId = await createSpace(a);
    return { a, b, spaceId, stub: env.SPACE.get(env.SPACE.idFromName(spaceId)) };
  }

  it("rejects a commit whose change records do not fit in one 1 MiB message and keeps every frame under 1 MiB", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const snapshot = () =>
      runInDurableObject(stub, (_instance, state) => {
        const all = (query: string) => state.storage.sql.exec(query).toArray();
        return {
          head: all("SELECT value FROM meta WHERE key = 'head_seq'"),
          commits: all("SELECT seq FROM commits"),
          nodes: all("SELECT node_id, name, status, meta_rev, content_rev FROM nodes ORDER BY node_id"),
          versions: all("SELECT version_id FROM versions"),
          chunks: all("SELECT hex(hash) AS hash, state, refcount FROM chunks ORDER BY hash"),
        };
      });
    const writer = await Socket.connect(spaceId, a);
    await writer.start();
    const reader = await Socket.connect(spaceId, b);
    await reader.start();
    const parts = await content(...Array.from({ length: 16_000 }, () => "x"));
    const plan = await writer.rpc({ case: "prepareUpload", value: { chunks: [parts.content.chunks[0]!] } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const uploaded = await put(plan.value.chunks[0]!.target!, parts.bytes[0]!);
    const receipts = [fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()))];
    const before = await snapshot();
    const both = await writer.rpc({
      case: "commit",
      value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "one", parts.content), op.create(uuidv7(), spaceId, "two", parts.content)], receipts, ...writer.slot() },
    });
    expect(both.case === "error" && both.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    expect(await snapshot()).toEqual(before);

    const frameSize = (msg: MessageInitShape<typeof ServerMessageSchema>["msg"]) => toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg })).length;
    for (const name of ["one", "two"]) {
      const result = await writer.commit(0n, [op.create(uuidv7(), spaceId, name, parts.content)], receipts);
      expect(frameSize({ case: "rpc", value: { reqId: 1n, result: { case: "commit", value: result } } })).toBeLessThanOrEqual(1024 * 1024);
      const live = await reader.next();
      expect(live.case).toBe("changes");
      expect(frameSize(live)).toBeLessThanOrEqual(1024 * 1024);
    }
    const late = await Socket.connect(spaceId, b);
    const welcome = await late.hello();
    late.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq: 0n } });
    const sizes: number[] = [];
    for (let msg = await late.next(); msg.case !== "caughtUp"; msg = await late.next()) sizes.push(frameSize(msg));
    expect(sizes).toHaveLength(2);
    expect(Math.max(...sizes)).toBeLessThanOrEqual(1024 * 1024);
  });

  it("answers a client's close at once with the client's code, so the client sees a clean close", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    for (const [code, reason] of [
      [1000, "bye"],
      [4000, "done"],
      [undefined, undefined],
    ] as const) {
      const socket = await Socket.connect(spaceId, owner);
      await socket.start();
      expect((await socket.rpc({ case: "listTree", value: {} })).case).toBe("tree");
      const startedMs = Date.now();
      const closed = new Promise<CloseEvent>((resolve) => socket.ws.addEventListener("close", resolve));
      socket.ws.close(code, reason);
      const event = await closed;
      expect({ code: event.code, wasClean: event.wasClean }).toEqual({ code: code ?? 1005, wasClean: true });
      expect(Date.now() - startedMs).toBeLessThan(2000);
    }
  });

  it("closes a connection on a message over 8 MiB before parsing it, and refuses an RPC body over 8 MiB", async () => {
    const { a, spaceId } = await twoDevices();
    const exact = await Socket.connect(spaceId, a);
    exact.ws.send(new Uint8Array(8 * 1024 * 1024));
    expect(await exact.waitClosed()).toMatchObject({ code: Close.PROTOCOL, reason: "malformed ClientMessage" });
    const over = await Socket.connect(spaceId, a);
    over.ws.send(new Uint8Array(8 * 1024 * 1024 + 1));
    expect(await over.waitClosed()).toMatchObject({ code: Close.PROTOCOL, reason: "message larger than 8 MiB" });
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: new Uint8Array(8 * 1024 * 1024 + 1) }, RpcResponseSchema);
    expect(rpc.status).toBe(413);
  });

  it("pages ListVersions, ListTree and ListTrash so that no reply exceeds 1 MiB over HTTP or WebSocket", async () => {
    const { a, spaceId } = await twoDevices();
    const writer = await Socket.connect(spaceId, a);
    await writer.start();
    const parts = await content(...Array.from({ length: 16_000 }, () => "x"));
    const plan = await writer.rpc({ case: "prepareUpload", value: { chunks: [parts.content.chunks[0]!] } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const uploaded = await put(plan.value.chunks[0]!.target!, parts.bytes[0]!);
    const receipts = [fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()))];
    const [one, two] = [uuidv7(), uuidv7()];
    await writer.commit(0n, [op.create(one, spaceId, "one", parts.content)], receipts);
    await writer.commit(1n, [op.update(one, parts.content)], receipts);
    await writer.commit(2n, [op.create(two, spaceId, "two", parts.content)], receipts);
    const frame = (result: MessageInitShape<typeof RpcResponseSchema>["result"]) =>
      toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 1n, result } } })).length;

    const pages = async (request: (cursor: string) => MessageInitShape<typeof RpcRequestSchema>["call"]) => {
      const seen: { http: number; socket: number; items: number }[] = [];
      for (let cursor = "", first = true; first || cursor !== ""; first = false) {
        const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", {
          method: "POST",
          headers: { Authorization: "Bearer " + a.access!.token },
          body: body(RpcRequestSchema, { reqId: 1n, call: request(cursor) }),
        });
        const httpBytes = new Uint8Array(await http.arrayBuffer());
        const result = await writer.rpc(request(cursor));
        expect(fromBinary(RpcResponseSchema, httpBytes).result.case).toBe(result.case);
        if (result.case !== "versions" && result.case !== "tree" && result.case !== "trash") throw new Error("unexpected " + result.case);
        const items = result.case === "versions" ? result.value.versions.length : result.value.nodes.length;
        seen.push({ http: httpBytes.length, socket: frame(result), items });
        cursor = result.value.nextCursor;
      }
      return seen;
    };
    const versions = await pages((cursor) => ({ case: "listVersions", value: { nodeId: one, cursor } }));
    expect(versions.map((p) => p.items)).toEqual([1, 1]);
    const tree = await pages((cursor) => ({ case: "listTree", value: { cursor } }));
    expect(tree.reduce((sum, p) => sum + p.items, 0)).toBe(3);
    expect(tree.length).toBeGreaterThan(1);
    await writer.commit(3n, [op.delete(one)]);
    await writer.commit(4n, [op.delete(two)]);
    const trash = await pages((cursor) => ({ case: "listTrash", value: { cursor } }));
    expect(trash.reduce((sum, p) => sum + p.items, 0)).toBe(2);
    for (const page of [...versions, ...tree, ...trash]) {
      expect(page.http).toBeLessThanOrEqual(1024 * 1024);
      expect(page.socket).toBeLessThanOrEqual(1024 * 1024);
    }
  });

  it("lists at most 1,000 missing chunks so the error fits in 1 MiB, and lists the rest once those are uploaded", async () => {
    const { a, spaceId, stub } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    const chunks = Array.from({ length: 32_768 }, (_, i) => {
      const hash = new Uint8Array(32);
      new DataView(hash.buffer).setUint32(0, i);
      return create(ChunkRefSchema, { hash, size: 1 });
    });
    const many = create(ContentSchema, { chunks, chunkCount: chunks.length, size: BigInt(chunks.length), manifestDigest: await sha256(encodeChunkList(chunks)) });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "missing", many)], ...socket.slot() } };
    const requestBytes = body(RpcRequestSchema, { reqId: 1n, call: request });
    expect(requestBytes.length).toBeLessThan(8 * 1024 * 1024);
    const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", { method: "POST", headers: { Authorization: "Bearer " + a.access!.token }, body: requestBytes });
    const httpBytes = new Uint8Array(await http.arrayBuffer());
    const viaHttp = fromBinary(RpcResponseSchema, httpBytes).result;
    const viaSocket = await socket.rpc(request);
    const wsBytes = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 1n, result: viaSocket } } })).length;
    for (const result of [viaHttp, viaSocket]) {
      expect(result.case === "error" && result.value.code).toBe(Error_Code.MISSING_CHUNKS);
      expect(result.case === "error" && result.value.missingChunks).toEqual(chunks.slice(0, 1000).map((chunk) => chunk.hash));
    }
    expect(httpBytes.length).toBeLessThanOrEqual(1024 * 1024);
    expect(wsBytes).toBeLessThanOrEqual(1024 * 1024);
    expect(await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT seq FROM commits").toArray())).toEqual([]);

    const parts = await content(...Array.from({ length: 1010 }, (_, i) => "chunk " + i));
    const commit = { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "big", parts.content)], ...socket.slot() };
    const receipts: MessageInitShape<typeof UploadReceiptSchema>[] = [];
    const listed = async () => {
      const result = await socket.rpc({ case: "commit", value: { ...commit, receipts } });
      return result.case === "error" && result.value.code === Error_Code.MISSING_CHUNKS ? result.value.missingChunks : result.case;
    };
    const uploadListed = async (hashes: Uint8Array[]) => {
      const indexes = hashes.map((hash) => parts.content.chunks.findIndex((chunk) => toHex(chunk.hash) === toHex(hash)));
      const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: indexes.map((i) => parts.content.chunks[i]!) } });
      if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
      for (const [n, chunk] of plan.value.chunks.entries()) {
        const uploaded = await put(chunk.target!, parts.bytes[indexes[n]!]!);
        receipts.push(fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer())));
      }
    };
    const first = await listed();
    expect(Array.isArray(first) && first.length).toBe(1000);
    await uploadListed(first as Uint8Array[]);
    const rest = await listed();
    expect(Array.isArray(rest) && rest.map(toHex)).toEqual(parts.content.chunks.slice(1000).map((chunk) => toHex(chunk.hash)));
    await uploadListed(rest as Uint8Array[]);
    expect(await listed()).toBe("commit");
  }, 30_000);

  it("replaces an error reply that would exceed 1 MiB with a generic LIMIT_EXCEEDED, over HTTP and WebSocket", async () => {
    const { a, spaceId, stub } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    const hashes = Array.from({ length: 40_000 }, (_, i) => {
      const hash = new Uint8Array(32);
      new DataView(hash.buffer).setUint32(0, i);
      return hash;
    });
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new CallFailure(Error_Code.MISSING_CHUNKS, "an error with more missing chunks than one message holds", hashes);
      };
    });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "listTree", value: {} };
    const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", {
      method: "POST",
      headers: { Authorization: "Bearer " + a.access!.token },
      body: body(RpcRequestSchema, { reqId: 1n, call: request }),
    });
    const httpBytes = new Uint8Array(await http.arrayBuffer());
    const viaSocket = await socket.rpc(request);
    const wsBytes = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 2n, result: viaSocket } } })).length;
    for (const result of [fromBinary(RpcResponseSchema, httpBytes).result, viaSocket]) {
      expect(result).toMatchObject({ case: "error", value: { code: Error_Code.LIMIT_EXCEEDED, message: "the error reply does not fit in one 1 MiB message", missingChunks: [] } });
    }
    expect(httpBytes.length).toBeLessThanOrEqual(1024 * 1024);
    expect(wsBytes).toBeLessThanOrEqual(1024 * 1024);
    expect(socket.closed).toBeUndefined();
  });

  it("closes a connection on any text message except ping, an oversized one included", async () => {
    const { a, spaceId } = await twoDevices();
    const big = await Socket.connect(spaceId, a);
    big.ws.send("ping");
    expect(await big.text()).toBe("pong");
    big.ws.send("x".repeat(8 * 1024 * 1024 + 1));
    big.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    expect(await big.next()).toMatchObject({ case: "goodbye", value: { code: Close.PROTOCOL } });
    expect((await big.waitClosed()).code).toBe(Close.PROTOCOL);
    const small = await Socket.connect(spaceId, a);
    small.ws.send("hello");
    expect((await small.waitClosed()).code).toBe(Close.PROTOCOL);
  });

  it("reports each device's highest accepted device_seq in Welcome", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    expect((await sa.start()).deviceSeq).toBe(0n);
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    expect((await (await Socket.connect(spaceId, a)).hello()).deviceSeq).toBe(2n);
    expect((await (await Socket.connect(spaceId, b)).hello()).deviceSeq).toBe(0n);
  });

  it("reports the commit token of the device's commit at device_seq in Welcome, also once the log no longer holds it", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    expect(await sa.start()).toMatchObject({ deviceSeq: 0n, deviceCommitId: "", deviceCommittedAtMs: 0n });
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    const second = await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const token = await runInDurableObject(stub, (_instance, state) => {
      const row = state.storage.sql.exec("SELECT commit_id, committed_at_ms FROM commits WHERE seq = 2").one();
      return { deviceCommitId: String(row.commit_id), deviceCommittedAtMs: BigInt(Number(row.committed_at_ms)) };
    });
    expect(token.deviceCommittedAtMs).toBe(second.committedAtMs);
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 2n, ...token });
    expect(await (await Socket.connect(spaceId, b)).hello()).toMatchObject({ deviceSeq: 0n, deviceCommitId: "", deviceCommittedAtMs: 0n });
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("DELETE FROM commits");
    });
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 2n, ...token });
  });

  it("reports no commit token in Welcome for a stored result from before commit tokens whose commit the log no longer holds", async () => {
    const { a, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    await sa.start();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => {
      const row = state.storage.sql.exec("SELECT results FROM device_commits").one();
      const result = fromBinary(CommitResultSchema, new Uint8Array(row.results as ArrayBuffer));
      result.committedAtMs = 0n;
      state.storage.sql.exec("UPDATE device_commits SET results = ?", toBinary(CommitResultSchema, result));
      state.storage.sql.exec("DELETE FROM commits");
    });
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 1n, deviceCommitId: "", deviceCommittedAtMs: 0n });
  });

  it("lets two devices commit and receive each other's changes", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    const welcome = await sa.start();
    expect(welcome).toMatchObject({ proto: 1, formatVersion: 1, headSeq: 0n, readOnly: false });
    await sb.start();

    const docs = uuidv7();
    const first = await sa.commit(0n, [op.mkdir(docs, spaceId, "Docs")]);
    expect(first.seq).toBe(1n);
    const atB = await sb.pushed();
    expect(atB.map((r) => r.seq)).toEqual([1n]);
    expect(atB[0]!.deviceId).toBe(a.deviceId);
    expect(nodeIds(atB)).toEqual([docs]);

    const notes = uuidv7();
    const second = await sb.commit(1n, [op.mkdir(notes, docs, "Notes")]);
    expect(second.seq).toBe(2n);
    expect(nodeIds(await sa.pushed())).toEqual([notes]);
  });

  it("resumes from the last seq after a dropped socket and across hibernation", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.start();
    let sb = await Socket.connect(spaceId, b);
    await sb.start();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    sb.ws.close(1000, "network drop");
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    await sa.commit(2n, [op.mkdir(uuidv7(), spaceId, "three")]);

    sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const caughtUp = await sb.resume(welcome.historyId, 1n);
    expect(caughtUp.records.map((r) => r.seq)).toEqual([2n, 3n]);
    expect(caughtUp.head).toBe(3n);

    await evictDurableObject(stub);
    await sa.commit(3n, [op.mkdir(uuidv7(), spaceId, "after hibernation")]);
    expect((await sb.pushed()).map((r) => r.seq)).toEqual([4n]);
  });

  it("turns a conflicting edit into a conflict copy and pushes it", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    await sa.start();
    await sb.start();
    const file = uuidv7();
    const v1 = await prepared(sa, "v1");
    await sa.commit(0n, [op.create(file, spaceId, "plan.txt", v1.content)], v1.receipts);
    await sb.pushed();

    const fromB = await prepared(sb, "B's edit");
    await sb.commit(1n, [op.update(file, fromB.content)], fromB.receipts);
    await sa.pushed();
    const fromA = await prepared(sa, "A's edit");
    const result = await sa.commit(1n, [op.update(file, fromA.content)], fromA.receipts);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(result.results[0]!.name).toMatch(/^plan \(conflict from Laptop A \d{4}-\d{2}-\d{2}\)\.txt$/);

    const pushed = await sb.pushed();
    const copy = pushed[0]!.changes[0]!.change;
    expect(copy.case === "node" && copy.value).toMatchObject({ nodeId: result.results[0]!.nodeId, name: result.results[0]!.name, parentId: spaceId });
  });

  it("asks for a resync on another history or a seq ahead of the server", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    const welcome = await socket.hello();
    socket.send({ case: "resume", value: { historyId: uuidv7(), sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.HISTORY_CHANGED } });
    socket.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq: 9n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.AHEAD_OF_SERVER } });
  });

  it("resumes from a token that names the commit at since_seq or from no token, and names the head's commit in CaughtUp (RFC 0013)", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.hello();
    expect((await sa.resume(welcome.historyId, 0n)).token).toEqual({ commitId: "", committedAtMs: 0n });
    for (const [i, name] of ["one", "two", "three"].entries()) await sa.commit(BigInt(i), [op.mkdir(uuidv7(), spaceId, name)]);

    const sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const all = await sb.resume(welcome.historyId, 0n);
    const tokens = all.records.map(tokenOf);
    expect(all.token).toEqual(tokens[2]);
    const fromOne = await sb.resume(welcome.historyId, 1n, tokens[0]);
    expect(fromOne.records.map((r) => r.seq)).toEqual([2n, 3n]);
    expect(fromOne.token).toEqual(tokens[2]);
    expect(await sb.resume(welcome.historyId, 3n, tokens[2])).toMatchObject({ records: [], head: 3n, token: tokens[2] });
    expect((await sb.resume(welcome.historyId, 1n)).records.map((r) => r.seq)).toEqual([2n, 3n]);
  });

  it("asks for a resync with AHEAD_OF_SERVER when the token at since_seq names another commit, at the head too (RFC 0013)", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.hello();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    const [one, two] = (await sa.resume(welcome.historyId, 0n)).records.map(tokenOf);

    const sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const forks: [bigint, CommitToken][] = [
      [1n, { ...one!, commitId: uuidv7() }],
      [1n, { ...one!, committedAtMs: one!.committedAtMs + 1n }],
      [2n, { ...two!, commitId: one!.commitId }],
      [2n, { ...two!, committedAtMs: two!.committedAtMs - 1n }],
    ];
    for (const [sinceSeq, since] of forks) {
      sb.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq, sinceCommitId: since.commitId, sinceCommittedAtMs: since.committedAtMs } });
      expect(await sb.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.AHEAD_OF_SERVER } });
    }
  });

  it("closes a connection whose token expired, and one that reauthenticates as another device", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    await sa.start();
    await runInDurableObject(stub, (instance: import("../src/space-do").SpaceDO) => instance.closeExpired(Date.now() + 16 * 60_000 + 1000));
    expect((await sa.waitClosed()).code).toBe(Close.TOKEN_EXPIRED);

    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    sb.send({ case: "reauth", value: { accessToken: b.access!.token } });
    sb.send({ case: "reauth", value: { accessToken: a.access!.token } });
    const goodbye = await sb.next();
    expect(goodbye).toMatchObject({ case: "goodbye", value: { code: Close.TOKEN_EXPIRED } });
  });

  it("requires Hello first, answers text pings, and refuses unknown users", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    socket.ws.send("ping");
    expect(await socket.text()).toBe("pong");
    socket.send({ case: "resume", value: { historyId: "", sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.PROTOCOL } });

    expect(await Socket.open(spaceId, undefined)).toBe(401);
    const stranger = uuidv7();
    await env.SPACE.get(env.SPACE.idFromName(stranger)).init({ spaceId: stranger, name: "x", ownerUserId: uuidv7(), spaceKey: new Uint8Array(32) });
    expect(await Socket.open(stranger, a.access!.token)).toBe(403);
  });

  it("serves the same RPCs over HTTP and pushes HTTP commits", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    const commit = body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "via http")], historyId: sb.historyId, deviceSeq: 1n } } });
    const response = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: commit }, RpcResponseSchema);
    expect(response.message!.result.case).toBe("commit");
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    const list = body(RpcRequestSchema, { reqId: 2n, call: { case: "listTree", value: {} } });
    const tree = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: list }, RpcResponseSchema);
    const page = tree.message!.result;
    expect(page.case === "tree" && page.value.nodes.map((n) => n.name).sort()).toEqual(["", "via http"]);
  });
});

describe("chunk relay", () => {
  async function connected() {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    return { a, spaceId, socket };
  }

  it("uploads through the Worker, commits with the receipt, and downloads the same bytes", async () => {
    const { a, spaceId, socket } = await connected();
    const b = await pairDevice(a, "Reader");
    const reader = await Socket.connect(spaceId, b);
    await reader.start();

    const file = await prepared(socket, "the bytes of a small file");
    expect(file.plan.chunks[0]!.target!.url.startsWith(BASE + "/v1/spaces/" + spaceId + "/chunks/")).toBe(true);
    expect(file.receipts).toHaveLength(1);
    const head = await env.BUCKET.head(chunkKey(spaceId, toHex(file.content.chunks[0]!.hash), targetIncarnation(file.plan.chunks[0]!.target!.url)));
    expect(head?.size).toBe(file.bytes[0]!.length);
    const nodeId = uuidv7();
    await socket.commit(0n, [op.create(nodeId, spaceId, "small.txt", file.content)], file.receipts);

    const pushed = await reader.pushed();
    const state = pushed[0]!.changes[0]!.change;
    const hash = state.case === "node" ? state.value.current!.content!.chunks[0]!.hash : new Uint8Array();
    const targets = await reader.rpc({ case: "getDownloadTargets", value: { hashes: [hash] } });
    if (targets.case !== "downloadTargets") throw new Error("getDownloadTargets failed");
    const response = await exports.default.fetch(targets.value.targets[0]!.target!.url);
    expect(response.status).toBe(200);
    const downloaded = new Uint8Array(await response.arrayBuffer());
    expect(toHex(await sha256(downloaded))).toBe(toHex(hash));

    const again = await socket.rpc({ case: "prepareUpload", value: { chunks: file.content.chunks } });
    expect(again.case === "prepareUpload" && again.value.chunks[0]!.action).toBe(ChunkPlan_Action.PRESENT);
  });

  it("rejects uploads with a bad ticket, a wrong length or bytes that do not match the hash", async () => {
    const { spaceId, socket } = await connected();
    const parts = await content("expected bytes");
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const target = plan.value.chunks[0]!.target!;
    const bytes = parts.bytes[0]!;
    const key = chunkKey(spaceId, toHex(parts.content.chunks[0]!.hash), targetIncarnation(target.url));

    const mac = new URL(target.url).searchParams.get("ticket")!.split(".")[2]!;
    const middle = target.url.lastIndexOf(mac) + Math.floor(mac.length / 2);
    const swapped = target.url[middle] === "A" ? "B" : "A";
    const tampered = { ...target, url: target.url.slice(0, middle) + swapped + target.url.slice(middle + 1) };
    expect(tampered.url).not.toBe(target.url);
    expect((await put(tampered, bytes)).status).toBe(403);
    expect((await put({ ...target, url: target.url.split("?")[0]! }, bytes)).status).toBe(403);
    const otherSpace = { ...target, url: target.url.replace(spaceId, uuidv7()) };
    expect((await put(otherSpace, bytes)).status).toBe(403);
    expect((await put(target, new Uint8Array(bytes.length + 1))).status).toBe(403);
    expect((await put(target, new Uint8Array(bytes.length))).status).toBe(400);
    expect(await env.BUCKET.head(key)).toBeNull();

    const download = await exports.default.fetch(target.url);
    expect(download.status).toBe(403);
    expect((await put(target, bytes)).status).toBe(200);
    expect((await put(target, bytes)).status).toBe(200);
  });

  async function preparedTicket() {
    const { spaceId, socket } = await connected();
    const parts = await content("review chunk " + uuidv7());
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const bytes = parts.bytes[0]!;
    const target = plan.value.chunks[0]!.target!;
    const url = target.url;
    const request = () => new Request(url, { method: "PUT", body: bytes, headers: { "Content-Length": String(bytes.length) } });
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    return { spaceId, socket, parts, target, request, stub, key: chunkKey(spaceId, toHex(parts.content.chunks[0]!.hash), targetIncarnation(url)) };
  }

  function relay(request: Request, put: (key: string, body: ReadableStream | null, options: R2PutOptions) => Promise<R2Object | null>) {
    const bucket = { put };
    return worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET: bucket } as unknown as Env, { waitUntil() {} } as unknown as ExecutionContext);
  }

  it("removes an object that a late PUT recreated after GC deleted its row", async () => {
    const { request, stub, key } = await preparedTicket();
    let entered!: () => void;
    let resume!: () => void;
    const started = new Promise<void>((resolve) => (entered = resolve));
    const released = new Promise<void>((resolve) => (resume = resolve));
    const pending = relay(request(), async (k, body, options) => {
      entered();
      await released;
      return env.BUCKET.put(k, body, options);
    });
    await started;
    const at = Date.now() + HOLD_MS + 60_000;
    const first = await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at));
    expect(first).toMatchObject({ deletedChunks: 1, sweptChunks: 0 });
    resume();
    expect((await pending).status).toBe(200);
    expect(await env.BUCKET.head(key)).not.toBeNull();
    const rows = () => runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT * FROM chunks").toArray());
    expect(await rows()).toHaveLength(0);

    const day = 24 * 3_600_000;
    expect(await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at + day))).toMatchObject({ sweptChunks: 0 });
    const sweep = await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at + 31 * day));
    expect(sweep).toMatchObject({ sweptChunks: 1, deletedChunks: 1 });
    expect(await env.BUCKET.head(key)).toBeNull();
    expect(await rows()).toHaveLength(0);
  });

  it("sweeps only objects whose incarnation no chunk row holds, once the grace has passed, and retires each before deleting it", async () => {
    const { socket, parts, target, stub, spaceId, key } = await preparedTicket();
    const uploaded = await put(target, parts.bytes[0]!);
    expect(uploaded.status).toBe(200);
    const receipt = fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()));
    await socket.commit(0n, [op.create(uuidv7(), spaceId, "kept", parts.content)], [receipt]);
    const hex = toHex(parts.content.chunks[0]!.hash);
    const otherIncarnation = chunkKey(spaceId, hex, "0.999999");
    await env.BUCKET.put(otherIncarnation, parts.bytes[0]!);
    const orphan = await content("orphan " + uuidv7());
    const orphanHex = toHex(orphan.content.chunks[0]!.hash);
    const orphanKey = chunkKey(spaceId, orphanHex, "0.1");
    await env.BUCKET.put(orphanKey, orphan.bytes[0]!);

    const gc = (atMs: number) => runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
    expect(await gc(Date.now())).toMatchObject({ sweptChunks: 0, deletedChunks: 0 });
    expect(await gc(Date.now() + 31 * 24 * 3_600_000)).toMatchObject({ sweptChunks: 2, deletedChunks: 2 });
    expect(await storedObjects(spaceId, hex)).toEqual([key]);
    expect(await storedObjects(spaceId, orphanHex)).toEqual([]);
    const retired = await runInDurableObject(account(), (_instance, state) =>
      state.storage.sql.exec("SELECT hash_hex, incarnation FROM chunk_retirements WHERE space_id = ? ORDER BY incarnation DESC", spaceId).toArray(),
    );
    expect(retired).toEqual([
      { hash_hex: hex, incarnation: "0.999999" },
      { hash_hex: orphanHex, incarnation: "0.1" },
    ]);
  });

  it("answers a checksum mismatch with 400 and storage failures with a retryable error", async () => {
    const { request, key } = await preparedTicket();
    const fail = (message: string) => relay(request(), async () => {
      throw new Error(message);
    });
    const decode = async (response: Response) => fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()));

    const mismatch = await fail("put: The SHA-256 checksum you specified did not match what we received.");
    expect(mismatch.status).toBe(400);
    expect((await decode(mismatch)).code).toBe(Error_Code.INVALID_ARGUMENT);
    const throttled = await fail("put: Reduce your concurrent request rate for the same object. (10058)");
    expect(throttled.status).toBe(429);
    expect(await decode(throttled)).toMatchObject({ code: Error_Code.RETRY_LATER, cause: Error_Cause.R2_RATE_LIMITED });
    const outage = await fail("put: We encountered an internal error. Please try again. (10001)");
    expect(outage.status).toBe(503);
    expect(await decode(outage)).toMatchObject({ code: Error_Code.RETRY_LATER, cause: Error_Cause.R2_UNAVAILABLE });
    expect(await env.BUCKET.head(key)).toBeNull();
  });

  function accountBinding(serverKey: () => Promise<ServerKey>) {
    return { idFromName: (name: string) => env.ACCOUNT.idFromName(name), get: () => ({ metered: async () => ({ value: await serverKey() }) }) };
  }

  it("reads the server key again on the next request after a failed Account DO call", async () => {
    const { request, key } = await preparedTicket();
    let calls = 0;
    const ACCOUNT = accountBinding(async () => {
      calls++;
      if (calls === 1) throw new Error("injected Account DO failure");
      return account().serverKey();
    });
    const send = () => worker.fetch(request() as Parameters<typeof worker.fetch>[0], { ...env, ACCOUNT } as unknown as Env, { waitUntil() {} } as unknown as ExecutionContext);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const failed = await send();
    expect(failed.status).toBe(500);
    expect(fromBinary(ErrorSchema, new Uint8Array(await failed.arrayBuffer()))).toMatchObject({ code: Error_Code.INTERNAL, cause: Error_Cause.EXCEPTION });
    expect((await send()).status).toBe(200);
    expect(calls).toBe(2);
    expect(await env.BUCKET.head(key)).not.toBeNull();
  });

  it("keeps no per-space state for unknown space ids and touches no storage before the ticket check", async () => {
    let calls = 0;
    const ACCOUNT = accountBinding(() => {
      calls++;
      return account().serverKey();
    });
    const BUCKET = new Proxy(
      {},
      {
        get: () => {
          throw new Error("R2 was used before the ticket check");
        },
      },
    );
    const relayEnv = { ...env, ACCOUNT, BUCKET } as unknown as Env;
    const send = (url: string, init?: RequestInit) => worker.fetch(new Request(url, init) as Parameters<typeof worker.fetch>[0], relayEnv, { waitUntil() {} } as unknown as ExecutionContext);
    for (let i = 0; i < 20; i++) {
      const url = `${BASE}/v1/spaces/${uuidv7()}/chunks/${"ab".repeat(32)}?inc=0.1&ticket=v1.9999999999999.AAAA`;
      expect((await send(url, { method: "PUT", body: new Uint8Array(4), headers: { "Content-Length": "4" } })).status).toBe(403);
      expect((await send(url)).status).toBe(403);
    }
    expect(calls).toBe(1);
  });
});

describe("chunk bundles", () => {
  async function planned(...parts: string[]) {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const socket = await Socket.connect(spaceId, a);
    const welcome = await socket.start();
    const file = await content(...parts);
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: file.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const targets = plan.value.chunks.map((chunk) => chunk.target!);
    const keys = file.content.chunks.map((chunk, i) => chunkKey(spaceId, toHex(chunk.hash), targetIncarnation(targets[i]!.url)));
    return { spaceId, socket, welcome, file, targets, keys };
  }

  const bundleBody = (chunks: { target: string; data: Uint8Array }[]) => body(ChunkBundleSchema, { chunks });
  const bundlePath = (spaceId: string) => `${BASE}/v1/spaces/${spaceId}/chunks`;

  async function sendBundle(spaceId: string, chunks: { target: string; data: Uint8Array }[], fetcher: (request: Request) => Promise<Response> = (request) => exports.default.fetch(request)) {
    const sent = bundleBody(chunks);
    const response = await fetcher(new Request(bundlePath(spaceId), { method: "POST", body: sent, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(sent.length) } }));
    const bytes = new Uint8Array(await response.arrayBuffer());
    return { status: response.status, results: response.ok ? fromBinary(ChunkBundleResultSchema, bytes).results : [], code: response.ok ? undefined : fromBinary(ErrorSchema, bytes).code };
  }

  it("is offered in Welcome, stores each chunk as its own upload, and a commit accepts the receipts", async () => {
    const { spaceId, socket, welcome, file, targets, keys } = await planned("first small file", "second small file", "third small file");
    expect(welcome.features).toContain("chunk_bundles");
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })));
    expect(sent.status).toBe(200);
    expect(sent.results.map((result) => [toHex(result.hash), result.status])).toEqual(file.content.chunks.map((chunk) => [toHex(chunk.hash), 200]));
    for (const [i, key] of keys.entries()) expect((await env.BUCKET.head(key))?.size).toBe(file.bytes[i]!.length);
    await socket.commit(0n, [op.create(uuidv7(), spaceId, "bundled.txt", file.content)], sent.results.map((result) => result.receipt!));

    const again = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })));
    expect(again.results.map((result) => result.status)).toEqual([200, 200, 200]);
    expect((await put(targets[0]!, file.bytes[0]!)).status).toBe(200);
  });

  it("answers each chunk with the status a PUT of it alone gets, and stores only the chunks that pass", async () => {
    const { spaceId, file, targets, keys } = await planned("bad ticket", "bad bytes", "longer than its ticket", "fine chunk");
    const mac = new URL(targets[0]!.url).searchParams.get("ticket")!.split(".")[2]!;
    const middle = targets[0]!.url.lastIndexOf(mac) + Math.floor(mac.length / 2);
    const tampered = targets[0]!.url.slice(0, middle) + (targets[0]!.url[middle] === "A" ? "B" : "A") + targets[0]!.url.slice(middle + 1);
    const chunks = [
      { target: tampered, data: file.bytes[0]! },
      { target: targets[1]!.url, data: new Uint8Array(file.bytes[1]!.length) },
      { target: targets[2]!.url, data: new Uint8Array(file.bytes[2]!.length + 1) },
      { target: targets[3]!.url, data: file.bytes[3]! },
    ];
    const sent = await sendBundle(spaceId, chunks);
    expect(sent.status).toBe(200);
    expect(sent.results.map((result) => result.status)).toEqual([403, 400, 403, 200]);
    expect(sent.results.map((result) => result.error?.code ?? Error_Code.UNSPECIFIED)).toEqual([Error_Code.FORBIDDEN, Error_Code.INVALID_ARGUMENT, Error_Code.FORBIDDEN, Error_Code.UNSPECIFIED]);
    expect(sent.results[3]!.receipt?.size).toBe(file.bytes[3]!.length);
    for (const key of keys.slice(0, 3)) expect(await env.BUCKET.head(key)).toBeNull();
    expect(await env.BUCKET.head(keys[3]!)).not.toBeNull();

    const alone = [
      await put({ ...targets[0]!, url: tampered }, chunks[0]!.data),
      await put(targets[1]!, chunks[1]!.data),
      await put(targets[2]!, chunks[2]!.data),
    ];
    expect(alone.map((response) => response.status)).toEqual([403, 400, 403]);
  });

  it("answers a chunk whose storage write fails with the PUT's retry status and still stores the others", async () => {
    const { spaceId, file, targets, keys } = await planned("throttled chunk", "unavailable chunk", "stored chunk");
    const failing = (request: Request) =>
      worker.fetch(request as Parameters<typeof worker.fetch>[0], {
        ...env,
        BUCKET: {
          put: (key: string, value: unknown, options: R2PutOptions) => {
            if (key === keys[0]) throw new Error("put: 429 too many requests (10058)");
            if (key === keys[1]) throw new Error("put: internal error");
            return env.BUCKET.put(key, value as ArrayBuffer, options);
          },
        },
      } as unknown as Env, {} as ExecutionContext);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })), failing);
    expect(sent.results.map((result) => [result.status, result.error?.code, result.error?.cause])).toEqual([
      [429, Error_Code.RETRY_LATER, Error_Cause.R2_RATE_LIMITED],
      [503, Error_Code.RETRY_LATER, Error_Cause.EXCEPTION],
      [200, undefined, undefined],
    ]);
    expect(await env.BUCKET.head(keys[2]!)).not.toBeNull();
  });

  it("refuses a malformed or oversized bundle whole and writes nothing", async () => {
    const { spaceId, file, targets } = await planned("one", "two");
    const BUCKET = new Proxy({}, { get: () => { throw new Error("R2 was used for a refused bundle"); } });
    const guarded = (request: Request) => worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET } as unknown as Env, {} as ExecutionContext);
    const valid = { target: targets[0]!.url, data: file.bytes[0]! };
    const cases: [string, { target: string; data: Uint8Array }[], number, Error_Code][] = [
      ["empty", [], 413, Error_Code.LIMIT_EXCEEDED],
      ["33 chunks", Array.from({ length: 33 }, () => valid), 413, Error_Code.LIMIT_EXCEEDED],
      ["a chunk twice", [valid, valid], 400, Error_Code.INVALID_ARGUMENT],
      ["another space", [valid, { target: targets[1]!.url.replace(spaceId, uuidv7()), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["not a chunk target", [valid, { target: bundlePath(spaceId), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["not a URL", [valid, { target: "chunk", data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["no incarnation", [valid, { target: targets[1]!.url.replace(/inc=[^&]*/, "inc=x"), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["an empty chunk", [valid, { target: targets[1]!.url, data: new Uint8Array(0) }], 413, Error_Code.LIMIT_EXCEEDED],
      ["a chunk over 1 MiB", [valid, { target: targets[1]!.url, data: new Uint8Array((1 << 20) + 1) }], 413, Error_Code.LIMIT_EXCEEDED],
    ];
    for (const [name, chunks, status, code] of cases) {
      const sent = await sendBundle(spaceId, chunks, guarded);
      expect([name, sent.status, sent.code]).toEqual([name, status, code]);
    }
    const raw = (init: RequestInit) => guarded(new Request(bundlePath(spaceId), { method: "POST", ...init }));
    expect((await raw({ body: new Uint8Array([0xff, 0xff, 0xff]), headers: { "Content-Length": "3" } })).status).toBe(400);
    expect((await raw({ body: new Uint8Array((4 << 20) + 1), headers: { "Content-Length": String((4 << 20) + 1) } })).status).toBe(413);
    expect((await raw({ body: new Uint8Array((4 << 20) + 1), headers: { "Content-Length": "10" } })).status).toBe(413);
    expect((await raw({ body: new ReadableStream({ start: (c) => { c.enqueue(new Uint8Array(4)); c.close(); } }), duplex: "half" } as RequestInit)).status).toBe(411);
    expect((await guarded(new Request(bundlePath(spaceId)))).status).toBe(405);
  });

  it("counts the chunks of a bundle before decoding it, so a body of millions of empty entries is refused cheaply", async () => {
    const { spaceId } = await planned("hostile");
    const BUCKET = new Proxy({}, { get: () => { throw new Error("R2 was used for a refused bundle"); } });
    const guarded = (request: Request) => worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET } as unknown as Env, {} as ExecutionContext);
    const post = async (bytes: Uint8Array) => {
      const response = await guarded(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Length": String(bytes.length) } }));
      return [response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code];
    };
    for (const n of [33, 1000, 100000, 2000000]) {
      const bytes = new Uint8Array(n * 2);
      for (let i = 0; i < n; i++) bytes[2 * i] = 0x0a;
      expect([n, ...(await post(bytes))]).toEqual([n, 413, Error_Code.LIMIT_EXCEEDED]);
    }
    const undecodable = new Uint8Array(33 * 3 + 3);
    for (let i = 0; i < 33; i++) undecodable.set([0x0a, 0x01, 0xff], 3 * i);
    undecodable.set([0xff, 0xff, 0xff], 99);
    expect(await post(undecodable)).toEqual([413, Error_Code.LIMIT_EXCEEDED]);
    expect(await post(new Uint8Array([0x0a, 0x05, 0x00]))).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    expect(await post(new Uint8Array([0x0b]))).toEqual([400, Error_Code.INVALID_ARGUMENT]);
  });

  it("refuses a chunks field with the wrong wire type before decoding, so it can't hide entries from the count", async () => {
    const { spaceId } = await planned("wrong wire type");
    const blocks = 200000;
    const bytes = new Uint8Array(8 + blocks * 20 + 3);
    bytes.set([9, 6, 24, 0, 24, 0, 24, 0], 0);
    for (let i = 0; i < blocks; i++) bytes.set([10, 18, 18, 16], 8 + i * 20);
    bytes.set([16, 16, 0], 8 + blocks * 20);
    expect(bytes.length).toBeLessThanOrEqual(4 << 20);
    expect(() => countBundledChunks(bytes)).toThrow(BadRequest);
    expect(() => countBundledChunks(new Uint8Array([0x0d, 0, 0, 0, 0]))).toThrow(BadRequest);
    expect(countBundledChunks(new Uint8Array([0x10, 0x01, 0x0a, 0x00]))).toBe(1);
    const response = await exports.default.fetch(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(bytes.length) } }));
    expect([response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
  });

  it("keeps no unknown fields of a bundle, at the top level or inside a chunk, when it decodes one", async () => {
    const { spaceId } = await planned("unknown fields");
    const varint = (n: number) => {
      const out: number[] = [];
      for (; n >= 0x80; n = Math.floor(n / 0x80)) out.push((n & 0x7f) | 0x80);
      return [...out, n];
    };
    const n = 2097150;
    const topLevel = new Uint8Array(2 + n * 2);
    topLevel[0] = 0x0a;
    for (let i = 0; i < n; i++) topLevel[2 + 2 * i] = 0x12;
    const m = n - 3;
    const inner = new Uint8Array(m * 2 + 3);
    for (let i = 0; i < m; i++) inner[2 * i] = 0x1a;
    inner.set([0x12, 0x01, 0x41], m * 2);
    const inChunk = new Uint8Array([0x0a, ...varint(inner.length), ...inner]);
    for (const bytes of [topLevel, inChunk]) {
      expect(bytes.length).toBeLessThanOrEqual(4 << 20);
      const decoded = decodeChunkBundle(bytes);
      expect(decoded.chunks).toHaveLength(1);
      expect([decoded.$unknown, decoded.chunks[0]!.$unknown]).toEqual([undefined, undefined]);
      const response = await exports.default.fetch(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(bytes.length) } }));
      expect(response.status).not.toBe(200);
      expect(fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code).not.toBe(Error_Code.INTERNAL);
    }
  });

  it("exports from the Worker module only entrypoints the runtime accepts", async () => {
    const exported = Object.entries(await import("../src/worker"));
    const entrypoint = (value: unknown) => typeof value === "function" || (typeof value === "object" && value !== null && "fetch" in value);
    expect(exported.filter(([, value]) => !entrypoint(value)).map(([name]) => name)).toEqual([]);
  });

  it("makes one storage write per chunk, at most six at once, for a full bundle", async () => {
    const parts = Array.from({ length: 32 }, (_, i) => `bundled chunk ${i} ${uuidv7()}`);
    const { spaceId, file, targets } = await planned(...parts);
    let open = 0;
    let peak = 0;
    let writes = 0;
    const counting = (request: Request) =>
      worker.fetch(request as Parameters<typeof worker.fetch>[0], {
        ...env,
        BUCKET: {
          put: async (key: string, value: unknown, options: R2PutOptions) => {
            writes++;
            peak = Math.max(peak, ++open);
            try {
              await new Promise((resolve) => setTimeout(resolve, 5));
              return await env.BUCKET.put(key, value as ArrayBuffer, options);
            } finally {
              open--;
            }
          },
        },
      } as unknown as Env, {} as ExecutionContext);
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })), counting);
    expect(sent.results.map((result) => result.status)).toEqual(Array(32).fill(200));
    expect(writes).toBe(32);
    expect(peak).toBe(6);
  });
});

describe("share links", () => {
  it("checks the password, locks after repeated failures, and honours expiry and revocation", async () => {
    const links = account();
    const spaceId = uuidv7();
    const nodeId = uuidv7();
    const open = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7() });
    expect(await links.openShareLink(open, undefined)).toEqual({ ok: true, value: { spaceId, nodeId } });
    expect(open).toMatch(/^[A-Za-z0-9_-]{22}$/);

    const locked = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), password: "correct horse" });
    const started = Date.now();
    expect((await links.openShareLink(locked, "correct horse")).ok).toBe(true);
    console.log("PBKDF2-SHA256 100000 iterations, one share-link verification through the Account DO: " + (Date.now() - started) + " ms");
    for (let i = 0; i < 5; i++) expect((await links.openShareLink(locked, "wrong")).ok).toBe(false);
    expect(await links.openShareLink(locked, "correct horse")).toMatchObject({ ok: false, message: "too many wrong passwords" });

    const row = await runInDurableObject(links, (_instance, state) =>
      state.storage.sql.exec("SELECT password_kdf, password_salt, password_hash, password_iterations FROM share_links WHERE link_id = ?", locked).one(),
    );
    expect(row.password_kdf).toBe("pbkdf2-sha256");
    expect(row.password_iterations).toBe(100_000);
    expect((row.password_salt as ArrayBuffer).byteLength).toBe(16);
    expect((row.password_hash as ArrayBuffer).byteLength).toBe(32);

    const expired = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), expiresAtMs: Date.now() - 1 });
    expect((await links.openShareLink(expired, undefined)).ok).toBe(false);
    await links.revokeShareLink(open);
    expect((await links.openShareLink(open, undefined)).ok).toBe(false);
  });

  it("counts consecutive failures per link, including a missing password, and starts over after success or a lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const codes = async (attempts: (string | undefined)[]) => {
      const out = [];
      for (const attempt of attempts) {
        const result = await links.openShareLink(link, attempt);
        out.push(result.ok ? "ok" : Error_Code[result.code]);
      }
      return out;
    };
    expect(await codes(["x", undefined, "x", "x", "pw", "x", "x", "x", "x"])).toEqual([
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok",
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED",
    ]);
    expect(await codes(["x", "pw", "x"])).toEqual(["UNAUTHENTICATED", "RETRY_LATER", "RETRY_LATER"]);
    const state = await runInDurableObject(links, (_instance, s) =>
      s.storage.sql.exec("SELECT failed_attempts FROM share_links WHERE link_id = ?", link).one(),
    );
    expect(state.failed_attempts).toBe(0);
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET locked_until_ms = ? WHERE link_id = ?", Date.now() - 1, link);
    });
    expect(await codes(["x", "x", "x", "x", "pw"])).toEqual(["UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok"]);
  });

  it("counts concurrent wrong passwords one by one, so they cannot get past the lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const results = await Promise.all(Array.from({ length: 12 }, () => links.openShareLink(link, "x")));
    const codes = results.map((r) => (r.ok ? "ok" : Error_Code[r.code]));
    expect(codes.filter((c) => c === "UNAUTHENTICATED")).toHaveLength(5);
    expect(codes.filter((c) => c === "RETRY_LATER")).toHaveLength(7);
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.RETRY_LATER });
  });

  it("refuses to open a link whose password scheme the server does not know", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET password_kdf = 'argon2id' WHERE link_id = ?", link);
    });
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.INTERNAL });
  });
});

describe("platform limits", () => {
  const DAY_MS = 86_400_000;
  const DAILY = "Exceeded allowed rows written in Durable Objects free tier.";
  const DAILY_MESSAGE = "the server reached a Workers Free daily limit; it resets at 00:00 UTC";
  const HOUR_MS = 3_600_000;
  const wait = (nowMs: number) => BigInt(nowMs % DAY_MS < HOUR_MS / 2 ? 300_000 : DAY_MS - (nowMs % DAY_MS));

  async function connected(sinceMidnightMs = HOUR_MS) {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    await runInDurableObject(stub, (instance: SpaceDO) => {
      const now = Date.now();
      instance.clock = () => now - (now % DAY_MS) - DAY_MS + sinceMidnightMs;
    });
    return { a, spaceId, stub, socket, retryAfterMs: wait(sinceMidnightMs) };
  }

  function failNextSpaceQuery(stub: DurableObjectStub<SpaceDO>, error: Error) {
    return runInDurableObject(stub, (_instance, state) => {
      vi.spyOn(state.storage.sql, "exec").mockImplementationOnce(() => {
        throw error;
      });
    });
  }

  it("answers an RPC that a daily limit stops with QUOTA_EXCEEDED and a wait until 00:00 UTC, over HTTP and WebSocket", async () => {
    const { a, spaceId, stub, socket, retryAfterMs } = await connected();
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new Error(DAILY);
      };
    });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "listTree", value: {} };
    const http = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: request }) }, RpcResponseSchema);
    const quota = { case: "error", value: { code: Error_Code.QUOTA_EXCEEDED, message: DAILY_MESSAGE, retryAfterMs } };
    expect(http.message!.result).toMatchObject(quota);
    expect(await socket.rpc(request)).toMatchObject(quota);
    expect(socket.closed).toBeUndefined();
    expect(logged.mock.calls.filter(([what]) => what === "space rpc failed").map(([, error]) => error)).toEqual(["Error: " + DAILY, "Error: " + DAILY]);
  });

  it.each([
    [10 * 60_000, 300_000n],
    [HOUR_MS / 2 - 1, 300_000n],
    [HOUR_MS / 2, BigInt(DAY_MS - HOUR_MS / 2)],
  ])("waits 5 minutes instead of a day in the first 30 minutes after 00:00 UTC: %i ms after it, %i ms", async (sinceMidnightMs, retryAfterMs) => {
    const { stub, socket } = await connected(sinceMidnightMs);
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new Error(DAILY);
      };
    });
    expect(await socket.rpc({ case: "listTree", value: {} })).toMatchObject({ case: "error", value: { code: Error_Code.QUOTA_EXCEEDED, retryAfterMs } });
  });

  it("answers a transient platform failure of an RPC with RETRY_LATER and any other error with INTERNAL, each with its cause", async () => {
    const { spaceId, stub, socket } = await connected();
    vi.spyOn(console, "error").mockImplementation(() => {});
    const retry = { code: Error_Code.RETRY_LATER, message: "the server is briefly unavailable; retry" };
    const internal = { code: Error_Code.INTERNAL, message: "internal error" };
    const others: [Error, Error_Cause, { code: Error_Code; message: string }][] = [
      [new Error("disk I/O error: SQLITE_IOERR"), Error_Cause.EXCEPTION, internal],
      [Object.assign(new Error("Durable Object reset because its code was updated."), { retryable: true }), Error_Cause.DO_UNAVAILABLE, retry],
      [Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true }), Error_Cause.DO_OVERLOADED, retry],
      [new Error("get: We encountered an internal error. Please try again. (10001)"), Error_Cause.R2_UNAVAILABLE, retry],
      [new Error("put: The specified bucket does not exist. (10006)"), Error_Cause.R2_ERROR, internal],
      [new Error("Too many subrequests."), Error_Cause.SUBREQUEST_LIMIT, internal],
      [new TypeError("Cannot read properties of undefined (reading 'nodeId')"), Error_Cause.EXCEPTION, internal],
      [new Error("the request failed (10001)"), Error_Cause.EXCEPTION, internal],
    ];
    for (const [error, cause, answer] of others) {
      await runInDurableObject(stub, (instance: SpaceDO) => {
        (instance as unknown as { listTree: () => never }).listTree = () => {
          throw error;
        };
      });
      expect(await socket.rpc({ case: "listTree", value: {} }), error.message).toMatchObject({
        case: "error",
        value: { ...answer, retryAfterMs: 0n, cause },
      });
    }
    expect(spaceId).toBeTruthy();
  });

  it("sends a Notice and closes the session when a daily limit stops a message before its RPC is read", async () => {
    const { stub, socket, retryAfterMs } = await connected();
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    await failNextSpaceQuery(stub, new Error(DAILY));
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    expect(await socket.next()).toMatchObject({ case: "notice", value: { kind: Notice_Kind.QUOTA_EXCEEDED, message: DAILY_MESSAGE, retryAfterMs } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER, reason: DAILY_MESSAGE } });
    expect((await socket.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
    expect(logged.mock.calls.find(([what]) => what === "space session stopped at a platform limit")?.[1]).toBe("Error: " + DAILY);
  });

  it("sends no Notice for any other error that stops a message before its RPC is read", async () => {
    const { stub, socket } = await connected();
    await failNextSpaceQuery(stub, new Error("disk I/O error: SQLITE_IOERR"));
    socket.send({ case: "rpc", value: { reqId: 99n, call: { case: "listTree", value: {} } } });
    expect(await socket.rpc({ case: "listTree", value: {} })).toMatchObject({ case: "tree" });
    expect(socket.closed).toBeUndefined();
  });

  it.each([
    "Exceeded allowed rows written in Durable Objects free tier.",
    "Exceeded allowed rows read in Durable Objects free tier.",
    "Exceeded allowed duration in Durable Objects free tier.",
    "Exceeded allowed volume of requests in Durable Objects free tier.",
    "database or disk is full: SQLITE_FULL",
  ])("answers a request the Account DO fails with %j with 429 QUOTA_EXCEEDED and a wait until 00:00 UTC", async (text) => {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    const exec = await runInDurableObject(account(), (_instance, state) =>
      vi.spyOn(state.storage.sql, "exec").mockImplementation(() => {
        throw new Error(text);
      }),
    );
    const refusal = async (path: string, init: RequestInit) => {
      const before = Date.now();
      const response = await exports.default.fetch(BASE + path, init);
      const after = Date.now();
      const error = fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()));
      const bounds = [wait(before), wait(after)].sort((x, y) => (x < y ? -1 : 1));
      expect(error.retryAfterMs).toBeGreaterThanOrEqual(bounds[0]!);
      expect(error.retryAfterMs).toBeLessThanOrEqual(bounds[1]!);
      return { status: response.status, code: error.code, message: error.message };
    };
    try {
      const upgrade = await refusal("/v1/spaces/" + spaceId + "/ws", { headers: { Upgrade: "websocket", Authorization: "Bearer " + a.access!.token } });
      const token = await refusal("/v1/token", {
        method: "POST",
        headers: { "Content-Type": "application/x-protobuf" },
        body: body(TokenRequestSchema, { deviceId: a.deviceId, refreshCredential: a.refreshCredential }),
      });
      const message = text.includes("SQLITE_FULL") ? "the server's storage is full" : DAILY_MESSAGE;
      expect([upgrade, token]).toEqual([0, 1].map(() => ({ status: 429, code: Error_Code.QUOTA_EXCEEDED, message })));
    } finally {
      exec.mockRestore();
    }
    expect(logged.mock.calls.filter(([what]) => what === "request stopped at a platform limit").map(([, error]) => error)).toEqual(["Error: " + text, "Error: " + text]);
  });

  it("answers a transient failure of the Account DO with 503 RETRY_LATER and any other error with 500 INTERNAL, each with its cause", async () => {
    const a = await getOwner();
    const failures: [Error, number, Error_Code, Error_Cause][] = [
      [new Error("disk I/O error: SQLITE_IOERR"), 500, Error_Code.INTERNAL, Error_Cause.EXCEPTION],
      [Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true }), 503, Error_Code.RETRY_LATER, Error_Cause.DO_OVERLOADED],
    ];
    for (const [thrown, status, code, cause] of failures) {
      const exec = await runInDurableObject(account(), (_instance, state) =>
        vi.spyOn(state.storage.sql, "exec").mockImplementation(() => {
          throw thrown;
        }),
      );
      try {
        const request = exports.default.fetch(BASE + "/v1/token", {
          method: "POST",
          headers: { "Content-Type": "application/x-protobuf" },
          body: body(TokenRequestSchema, { deviceId: a.deviceId, refreshCredential: a.refreshCredential }),
        });
        vi.spyOn(console, "error").mockImplementation(() => {});
        const response = await request;
        expect([response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()))], thrown.message).toMatchObject([status, { code, cause }]);
      } finally {
        exec.mockRestore();
      }
    }
  });
});
