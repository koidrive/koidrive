// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { vi } from "vitest";
import { type CommitResult, type Op, OpSchema, UploadReceiptSchema, type UploadReceipt } from "../src/gen/koidrive/v1/commit_pb";
import { type RpcRequest, RpcRequestSchema, type RpcResponse, RpcResponseSchema } from "../src/gen/koidrive/v1/messages_pb";
import { type ChunkRef, ChunkRefSchema, type Content, ContentSchema, type NodeState } from "../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, type Error as RpcError } from "../src/gen/koidrive/v1/rpc_pb";
import { sha256, signChunkGrant } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import type { Session, SpaceDO } from "../src/space-do";
import { MIGRATIONS } from "../src/space/schema";
import { chunkKey, encodeChunkList } from "../src/space/store";

export const ORIGIN = "https://koidrive.test";
export const OWNER = uuidv7();
export const SPACE_KEY = crypto.getRandomValues(new Uint8Array(32));
const encoder = new TextEncoder();

export async function newSpace(name = "Test space") {
  const spaceId = uuidv7();
  const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
  await stub.init({ spaceId, name, ownerUserId: OWNER, spaceKey: SPACE_KEY });
  return { spaceId, stub };
}

export function session(deviceName: string, overrides: Partial<Session> = {}): Session {
  return { deviceId: uuidv7(), userId: OWNER, deviceName, kind: "device", scope: null, role: "owner", maxFormat: 1, origin: ORIGIN, ...overrides };
}

export async function content(...parts: string[]): Promise<{ content: Content; bytes: Uint8Array[] }> {
  const bytes = parts.map((part) => encoder.encode(part));
  const chunks: ChunkRef[] = [];
  for (const part of bytes) chunks.push(create(ChunkRefSchema, { hash: await sha256(part), size: part.length }));
  return {
    bytes,
    content: create(ContentSchema, {
      size: BigInt(bytes.reduce((sum, part) => sum + part.length, 0)),
      mtimeNs: 1_700_000_000_000_000_000n,
      chunks,
      chunkCount: chunks.length,
      manifestDigest: await sha256(encodeChunkList(chunks)),
      chunker: "fastcdc2020-1m-4m-16m",
    }),
  };
}

export const op = {
  mkdir: (nodeId: string, parentId: string, name: string): Op =>
    create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId, name } } }),
  create: (nodeId: string, parentId: string, name: string, content: Content): Op =>
    create(OpSchema, { op: { case: "createFile", value: { nodeId, parentId, name, content } } }),
  update: (nodeId: string, content: Content): Op => create(OpSchema, { op: { case: "updateContent", value: { nodeId, content } } }),
  move: (nodeId: string, newParentId: string, newName: string): Op =>
    create(OpSchema, { op: { case: "move", value: { nodeId, newParentId, newName } } }),
  delete: (nodeId: string): Op => create(OpSchema, { op: { case: "delete", value: { nodeId } } }),
  restore: (nodeId: string, init: { versionId?: string; targetParentId?: string; name?: string } = {}): Op =>
    create(OpSchema, { op: { case: "restore", value: { nodeId, ...init } } }),
};

export function targetIncarnation(url: string): string {
  return new URL(url).searchParams.get("inc")!;
}

export async function storedObjects(spaceId: string, hash: Uint8Array | string): Promise<string[]> {
  const hex = typeof hash === "string" ? hash : toHex(hash);
  const listed = await env.BUCKET.list({ prefix: `spaces/${spaceId}/chunks/${hex.slice(0, 2)}/${hex}/` });
  return listed.objects.map((object) => object.key);
}

type Stub = Awaited<ReturnType<typeof newSpace>>["stub"];

export function freezeClock(instance: SpaceDO, atMs: number): void {
  let nowMs = atMs;
  instance.clock = () => nowMs;
  instance.sleep = async (ms) => {
    nowMs += ms;
  };
}

export async function revertIntentsMigration(stub: Stub): Promise<void> {
  await runInDurableObject(stub, (_instance, state) => {
    const sql = state.storage.sql;
    for (const table of ["field_intents", "trash_held", "tombstones"]) sql.exec(`DROP TABLE ${table}`);
    sql.exec("ALTER TABLE nodes DROP COLUMN status_before");
    sql.exec("ALTER TABLE nodes DROP COLUMN copy_mark");
    sql.exec("UPDATE meta SET value = ? WHERE key = 'schema_version'", MIGRATIONS.findIndex((step) => step.toString().includes("field_intents")));
  });
  await evictDurableObject(stub);
}

export function runSpaceAlarm(
  stub: Stub,
  atMs: number,
  options: { pageNodes?: number; intercept?: (key: string, put: () => Promise<unknown>) => Promise<unknown> } = {},
): Promise<void> {
  return runInDurableObject(stub, async (instance: SpaceDO) => {
    freezeClock(instance, atMs);
    if (options.pageNodes !== undefined) instance.snapshotPageNodes = options.pageNodes;
    const bucket = (instance as unknown as { env: Env }).env.BUCKET;
    const put = bucket.put.bind(bucket);
    const { intercept } = options;
    const spy = intercept
      ? vi.spyOn(bucket, "put").mockImplementation(((key: string, ...rest: [never, never]) => intercept(key, () => put(key, ...rest))) as never)
      : undefined;
    try {
      await instance.alarm();
    } finally {
      spy?.mockRestore();
    }
  });
}

export function unknownOutcome(match: (key: string) => boolean, landed: boolean) {
  let failed = false;
  return async (key: string, put: () => Promise<unknown>) => {
    if (failed || !match(key)) return put();
    failed = true;
    if (landed) await put();
    throw new Error("the write's outcome is unknown");
  };
}

export interface CommitInit {
  baseSeq?: bigint;
  receipts?: UploadReceipt[];
  commitId?: string;
  historyId?: string;
  deviceSeq?: bigint;
  requireHead?: boolean;
  baseCommitId?: string;
  baseCommittedAtMs?: bigint;
}

export class TestClient {
  baseSeq = 0n;
  acceptedSeq = 0n;
  private readonly slots = new Map<string, bigint>();
  private readonly bases = new Map<string, bigint>();
  nowMs: number | undefined;
  private nextReq = 1n;

  constructor(
    readonly stub: Stub,
    readonly spaceId: string,
    readonly session: Session,
  ) {}

  async call(call: MessageInitShape<typeof RpcRequestSchema>["call"]): Promise<RpcResponse["result"]> {
    const request: RpcRequest = create(RpcRequestSchema, { reqId: this.nextReq++, call });
    const response = fromBinary(RpcResponseSchema, await this.stub.rpc(this.session, toBinary(RpcRequestSchema, request)));
    if (response.reqId !== request.reqId) throw new Error("req_id mismatch");
    return response.result;
  }

  async upload(...contents: Content[]): Promise<UploadReceipt[]> {
    return this.uploadBytes(contents.flatMap((c) => c.chunks), new Map());
  }

  async uploadParts(parts: { content: Content; bytes: Uint8Array[] }): Promise<UploadReceipt[]> {
    const bytes = new Map(parts.content.chunks.map((chunk, i) => [toHex(chunk.hash), parts.bytes[i]!]));
    return this.uploadBytes(parts.content.chunks, bytes);
  }

  private async uploadBytes(chunks: ChunkRef[], bytes: Map<string, Uint8Array>): Promise<UploadReceipt[]> {
    const result = await this.call({ case: "prepareUpload", value: { chunks } });
    if (result.case !== "prepareUpload") throw new Error(`prepareUpload failed: ${JSON.stringify(result.value)}`);
    const receipts: UploadReceipt[] = [];
    for (const plan of result.value.chunks) {
      if (plan.action !== ChunkPlan_Action.UPLOAD) continue;
      const hex = toHex(plan.hash);
      const chunk = chunks.find((c) => toHex(c.hash) === hex)!;
      const object = { spaceId: this.spaceId, hashHex: hex, incarnation: targetIncarnation(plan.target!.url) };
      await env.BUCKET.put(chunkKey(this.spaceId, hex, object.incarnation), bytes.get(hex) ?? new Uint8Array(chunk.size));
      const token = await signChunkGrant(SPACE_KEY, "receipt", object, chunk.size, (this.nowMs ?? Date.now()) + 3_600_000);
      receipts.push(create(UploadReceiptSchema, { hash: plan.hash, size: chunk.size, token }));
    }
    return receipts;
  }

  history(): Promise<string> {
    return runInDurableObject(this.stub, (_instance, state) => String(state.storage.sql.exec("SELECT value FROM meta WHERE key = 'history_id'").one().value));
  }

  async commitRaw(ops: Op[], init: CommitInit = {}): Promise<RpcResponse["result"]> {
    const commitId = init.commitId ?? uuidv7();
    const slot = this.slots.get(commitId) ?? this.acceptedSeq + 1n;
    this.slots.set(commitId, slot);
    const deviceSeq = init.deviceSeq ?? slot;
    const baseSeq = init.baseSeq ?? this.bases.get(commitId) ?? this.baseSeq;
    if (!this.bases.has(commitId)) this.bases.set(commitId, baseSeq);
    const result = await this.call({
      case: "commit",
      value: {
        commitId,
        baseSeq,
        ops,
        receipts: init.receipts ?? [],
        historyId: init.historyId ?? (await this.history()),
        deviceSeq,
        requireHead: init.requireHead ?? false,
        baseCommitId: init.baseCommitId ?? "",
        baseCommittedAtMs: init.baseCommittedAtMs ?? 0n,
      },
    });
    if (result.case === "commit" && deviceSeq > this.acceptedSeq) this.acceptedSeq = deviceSeq;
    return result;
  }

  async commit(ops: Op[], init: CommitInit = {}): Promise<CommitResult> {
    const result = await this.commitRaw(ops, init);
    if (result.case !== "commit") throw new Error(`commit failed: ${JSON.stringify((result.value as RpcError | undefined)?.message)}`);
    if (result.value.seq > this.baseSeq) this.baseSeq = result.value.seq;
    return result.value;
  }

  async tree(): Promise<Map<string, NodeState>> {
    const nodes = new Map<string, NodeState>();
    let cursor = "";
    do {
      const result = await this.call({ case: "listTree", value: { cursor, limit: 2 } });
      if (result.case !== "tree") throw new Error("listTree failed");
      for (const node of result.value.nodes) nodes.set(node.nodeId, node);
      cursor = result.value.nextCursor;
    } while (cursor);
    return nodes;
  }
}

export async function client(space: { spaceId: string; stub: Stub }, deviceName: string, overrides: Partial<Session> = {}) {
  return new TestClient(space.stub, space.spaceId, session(deviceName, overrides));
}
