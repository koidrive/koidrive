// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { expect, vi } from "vitest";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  SpaceInfoSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { type UploadReceipt, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ClientMessageSchema, RpcRequestSchema, type ServerMessage, ServerMessageSchema, type Welcome } from "../src/gen/koidrive/v1/messages_pb";
import { type CommitRecord } from "../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, ErrorSchema, type Target } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import { type SpaceDO } from "../src/space-do";
import { SESSION_LEASE_MS } from "../src/account/account";
import { content, op } from "./helpers";

export const BASE = "https://koidrive.test";

export function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

export async function call<Res extends DescMessage>(
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

export function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

export async function send(path: string, init: { method?: string; token?: string; body?: Uint8Array }) {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  const error = !response.ok && response.headers.get("Content-Type") === "application/x-protobuf";
  return { status: response.status, code: error ? fromBinary(ErrorSchema, bytes).code : undefined };
}

let owner: DeviceCredentials | undefined;

export async function getOwner(): Promise<DeviceCredentials> {
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

export async function pairDevice(inviter: DeviceCredentials, deviceName: string): Promise<DeviceCredentials> {
  const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
  const paired = await call("/v1/pair/complete", { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform: "linux" }) }, DeviceCredentialsSchema);
  return paired.message!;
}

export async function createSpace(owner: DeviceCredentials): Promise<string> {
  const created = await call("/v1/spaces", { token: owner.access!.token, body: body(CreateSpaceRequestSchema, { name: `Home ${uuidv7()}` }) }, SpaceInfoSchema);
  expect(created.status).toBe(201);
  return created.message!.spaceId;
}

export function passLeases(spaceId: string) {
  return runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
    const offsetMs = instance.clock() - Date.now() + SESSION_LEASE_MS + 1000;
    instance.clock = () => Date.now() + offsetMs;
  });
}

export interface CommitToken {
  commitId: string;
  committedAtMs: bigint;
}

export function tokenOf(record: CommitRecord): CommitToken {
  return { commitId: record.commitId, committedAtMs: record.committedAtMs };
}

export class Socket {
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

export async function put(target: Target, bytes: Uint8Array): Promise<Response> {
  return exports.default.fetch(target.url, { method: target.method, body: bytes, headers: { "Content-Type": "application/x-protobuf" } });
}

export async function prepared(socket: Socket, text: string) {
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

export function nodeIds(records: CommitRecord[]): string[] {
  return records.flatMap((r) => r.changes.map((c) => (c.change.case === "node" ? c.change.value.nodeId : "")));
}
