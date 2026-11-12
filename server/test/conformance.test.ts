// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import numbers from "../../spec/testdata/scenarios/runner-numbers-v1.json";
import hand from "../../spec/testdata/scenarios/space-conformance-v1.json?raw";
import sequences from "../../spec/testdata/scenarios/space-commit-sequences-v1.json?raw";
import { signChunkGrant, sha256 } from "../src/crypto";
import {
  type CommitResult,
  CommitResultSchema,
  type Op,
  OpResult_Code,
  OpSchema,
  type OpResult,
  OpResultSchema,
  type UploadReceipt,
  UploadReceiptSchema,
} from "../src/gen/koidrive/v1/commit_pb";
import { RpcRequestSchema, type RpcResponse, RpcResponseSchema } from "../src/gen/koidrive/v1/messages_pb";
import {
  type ChangeRecord,
  ChangeRecordSchema,
  type ChunkRef,
  ChunkRefSchema,
  type Content,
  ContentSchema,
  NodeKind,
  type NodeState,
  NodeStateSchema,
  NodeStatus,
  type VersionInfo,
  VersionInfoSchema,
} from "../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, Error_Code, type Error as RpcError } from "../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../src/ids";
import { GRANT_MS, type Role, type Session, type SpaceDO } from "../src/space-do";
import { chunkKey, encodeChunkList, getMeta, getNode, nodeState } from "../src/space/store";
import { OWNER, targetIncarnation } from "./helpers";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Obj = { [key: string]: Json };

const MAX_REPEAT = 65_536;
const MAX_U32 = 0xffffffff;

function plainIntegers(text: string): void {
  let i = 0;
  while (i < text.length) {
    if (text[i] === '"') {
      i++;
      while (i < text.length && text[i] !== '"') i += text[i] === "\\" ? 2 : 1;
      i++;
    } else if (/[-+0-9]/.test(text[i]!)) {
      const start = i;
      while (i < text.length && /[-+.eE0-9]/.test(text[i]!)) i++;
      const token = text.slice(start, i);
      const digits = token.startsWith("-") ? token.slice(1) : token;
      if (!/^(0|[1-9][0-9]*)$/.test(digits) || token === "-0") fail(`numbers must be written as plain integers, got ${token}`);
      if (BigInt(digits) > BigInt(Number.MAX_SAFE_INTEGER)) {
        fail(`numbers must be integers from -${Number.MAX_SAFE_INTEGER} to ${Number.MAX_SAFE_INTEGER}, got ${token}`);
      }
    } else i++;
  }
}

function parseDoc(text: string): Obj {
  plainIntegers(text);
  return JSON.parse(text) as Obj;
}

const doc = parseDoc(hand);
const files: [string, Obj][] = [
  ["space-conformance-v1.json", doc],
  ["space-commit-sequences-v1.json", parseDoc(sequences)],
];
const ORIGIN = "https://koidrive.test";
const CHUNKER = "fastcdc2020-1m-4m-16m";

class Failure extends Error {}

function fail(message: string): never {
  throw new Failure(message);
}

function show(value: Json | undefined): string {
  return JSON.stringify(value);
}

function object(value: Json | undefined, what: string): Obj {
  if (value === null || typeof value !== "object" || Array.isArray(value)) fail(`${what} must be an object, got ${show(value)}`);
  return value;
}

function keys(value: Json | undefined, what: string, allowed: string[]): Obj {
  const found = object(value, what);
  for (const key of Object.keys(found)) if (!allowed.includes(key)) fail(`unknown field ${key} in ${what}`);
  return found;
}

function opt<T>(value: Obj, field: string, kind: string, read: (v: Json) => T | undefined): T | undefined {
  if (!(field in value)) return undefined;
  const read_ = read(value[field]!);
  if (read_ === undefined) fail(`${field} must be ${kind}, got ${show(value[field])}`);
  return read_;
}

const optStr = (value: Obj, field: string) => opt(value, field, "a string", (v) => (typeof v === "string" ? v : undefined));
const optUint = (value: Obj, field: string) =>
  opt(value, field, "a non-negative integer", (v) => (typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : undefined));
const optInt = (value: Obj, field: string) =>
  opt(value, field, "an integer", (v) => (typeof v === "number" && Number.isSafeInteger(v) ? v : undefined));
const optU32 = (value: Obj, field: string) =>
  opt(value, field, "an integer from 0 to 4294967295", (v) => (typeof v === "number" && Number.isInteger(v) && v >= 0 && v <= MAX_U32 ? v : undefined));
const optRepeat = (value: Obj) =>
  opt(value, "repeat", `an integer from 1 to ${MAX_REPEAT}`, (v) => (typeof v === "number" && Number.isInteger(v) && v >= 1 && v <= MAX_REPEAT ? v : undefined));
const optBool = (value: Obj, field: string) => opt(value, field, "a boolean", (v) => (typeof v === "boolean" ? v : undefined));
const optList = (value: Obj, field: string) => opt(value, field, "a list", (v) => (Array.isArray(v) ? v : undefined));

function req<T>(found: T | undefined, field: string): T {
  if (found === undefined) fail(`missing field ${field}`);
  return found;
}

function every(value: Obj, fields: string[]): void {
  for (const field of fields) if (!(field in value)) fail(`missing field ${field}`);
}

function covered(schema: DescMessage, compared: string[]): void {
  for (const field of schema.fields) if (!compared.includes(field.localName)) fail(`${schema.typeName}.${field.name} is not compared`);
}

function within(prefix: string, check: () => void): void {
  try {
    check();
  } catch (error) {
    if (error instanceof Failure) fail(`${prefix}: ${error.message}`);
    throw error;
  }
}

const NODE_FIELDS = ["node", "parent", "name", "kind", "status", "content_rev", "meta_rev", "trash_group", "current", "pending_for", "pending_by"];
const CURRENT_FIELDS = ["version", "seq", "content", "chunks_inline", "device", "user", "created_at_ms"];

function strings(list: Json[], what: string): string[] {
  return list.map((v) => (typeof v === "string" ? v : fail(`${what} must list strings, got ${show(v)}`)));
}

function sameSet(a: Set<string>, b: Set<string>): boolean {
  return a.size === b.size && [...a].every((x) => b.has(x));
}

function expectation(step: Obj, onError: string[], onSuccess: string[]): Obj {
  if (!("expect" in step)) fail("a call step needs expect, even if empty");
  const expect = object(step.expect, "expect");
  if (optStr(expect, "error") !== undefined) keys(expect, "an error expect", ["error", ...onError]);
  else keys(expect, "expect", onSuccess);
  return expect;
}

function errorName(code: Error_Code): string {
  return Error_Code[code] ?? String(code);
}

function checkError(error: RpcError, code: string): void {
  if (errorName(error.code) !== code) fail(`error ${errorName(error.code)} (${error.message}), expected ${code}`);
}

interface Chunk {
  ref: ChunkRef;
  bytes: Uint8Array;
}

const encoder = new TextEncoder();

async function chunkList(list: Json[]): Promise<Chunk[]> {
  const out: Chunk[] = [];
  for (const entry of list) {
    const chunk = keys(entry, "a chunk", ["text", "counter_le32", "size", "repeat"]);
    const text = optStr(chunk, "text");
    const counter = optU32(chunk, "counter_le32");
    let pieces: Uint8Array[];
    if (text !== undefined && counter === undefined) pieces = [encoder.encode(text)];
    else if (text === undefined && counter !== undefined) {
      pieces = Array.from({ length: counter }, (_, i) => {
        const bytes = new Uint8Array(4);
        new DataView(bytes.buffer).setUint32(0, i, true);
        return bytes;
      });
    } else fail(`a chunk needs exactly one of text and counter_le32: ${show(entry)}`);
    const size = optU32(chunk, "size");
    const listed: Chunk[] = [];
    for (const bytes of pieces) listed.push({ ref: create(ChunkRefSchema, { hash: await sha256(bytes), size: size ?? bytes.length }), bytes });
    for (let i = 0; i < (optRepeat(chunk) ?? 1); i++) out.push(...listed);
  }
  return out;
}

interface SpaceUnderTest {
  spaceId: string;
  key: Uint8Array;
  historyId: string;
}

type Stub = DurableObjectStub<SpaceDO>;

class Run {
  private readonly nowMs: number;
  private readonly mtimeNs: bigint;
  private readonly spaces = new Map<string, SpaceUnderTest>();
  private readonly devices = new Map<string, Session>();
  private readonly contents = new Map<string, { content: Content; chunks: Chunk[] }>();
  private readonly wallets = new Map<string, UploadReceipt[]>();
  private readonly lastMissing = new Map<string, Uint8Array[]>();
  private readonly symbols = new Map<string, string>();
  private readonly values = new Map<string, string>();
  private readonly cursors = new Map<string, string>();
  private readonly slots = new Map<string, bigint>();
  private readonly sentSlots = new Map<string, bigint>();
  private readonly topSeq = new Map<string, bigint>();
  private reqId = 0n;

  private constructor(nowMs: number, mtimeNs: bigint) {
    this.nowMs = nowMs;
    this.mtimeNs = mtimeNs;
  }

  static async start(scenario: Obj, file: Obj): Promise<Run> {
    keys(scenario, "a scenario", ["name", "covers", "reference_only", "devices", "spaces", "contents", "steps"]);
    req(optStr(scenario, "name"), "name");
    optStr(scenario, "reference_only");
    optList(scenario, "covers");
    const nowMs = req(optInt(file, "now_ms"), "now_ms");
    const mtimeText = req(optStr(file, "mtime_ns"), "mtime_ns");
    if (!/^-?\d+$/.test(mtimeText) || BigInt.asIntN(64, BigInt(mtimeText)) !== BigInt(mtimeText)) fail("mtime_ns must be an int64 written as a string");
    const run = new Run(nowMs, BigInt(mtimeText));
    const names = "spaces" in scenario ? strings(req(optList(scenario, "spaces"), "spaces"), "spaces") : ["main"];
    for (const name of names) {
      const spaceId = uuidv7();
      const key = crypto.getRandomValues(new Uint8Array(32));
      const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
      await stub.init({ spaceId, name, ownerUserId: OWNER, spaceKey: key });
      const historyId = await runInDurableObject(stub, (_instance, state) => String(getMeta(state.storage.sql, "history_id")));
      run.spaces.set(name, { spaceId, key, historyId });
    }
    for (const [symbol, value] of Object.entries(object(scenario.devices, "devices"))) {
      const device = keys(value, "a device", ["name", "role", "max_format"]);
      const role = optStr(device, "role") ?? "owner";
      if (!["owner", "editor", "viewer"].includes(role)) fail(`unknown role ${role}`);
      run.devices.set(symbol, {
        deviceId: uuidv7(),
        userId: OWNER,
        deviceName: req(optStr(device, "name"), "name"),
        kind: "device",
        scope: null,
        role: role as Role,
        maxFormat: optU32(device, "max_format") ?? 1,
        origin: ORIGIN,
      });
    }
    if ("contents" in scenario) {
      for (const [symbol, value] of Object.entries(object(scenario.contents, "contents"))) {
        const def = keys(value, "a content", ["chunks", "size"]);
        const chunks = await chunkList(req(optList(def, "chunks"), "chunks"));
        const refs = chunks.map((chunk) => chunk.ref);
        const content = create(ContentSchema, {
          size: BigInt(optUint(def, "size") ?? refs.reduce((sum, ref) => sum + ref.size, 0)),
          mtimeNs: run.mtimeNs,
          executable: false,
          chunks: refs,
          chunkCount: refs.length,
          manifestDigest: await sha256(encodeChunkList(refs)),
          chunker: CHUNKER,
        });
        run.contents.set(symbol, { content, chunks });
      }
    }
    return run;
  }

  async steps(scenario: Obj): Promise<void> {
    for (const [i, step] of req(optList(scenario, "steps"), "steps").entries()) {
      try {
        await this.step(object(step, "a step"));
      } catch (error) {
        if (error instanceof Failure) fail(`step ${i + 1}: ${error.message}`);
        throw error;
      }
    }
  }

  private bind(symbol: string, value: string): void {
    const bound = this.symbols.get(symbol);
    if (bound !== undefined) {
      if (bound !== value) fail(`${symbol} is ${bound}, got ${value}`);
      return;
    }
    const other = this.values.get(value);
    if (other !== undefined) fail(`${symbol} got ${value}, which is already ${other}`);
    this.symbols.set(symbol, value);
    this.values.set(value, symbol);
  }

  private fixed(space: string, symbol: string): string | undefined {
    if (symbol === "root") return this.space(space).spaceId;
    return symbol.startsWith("=") ? symbol.slice(1) : undefined;
  }

  private id(space: string, symbol: string): string {
    const value = this.fixed(space, symbol) ?? this.symbols.get(symbol);
    if (value !== undefined) return value;
    const minted = uuidv7(this.nowMs);
    this.bind(symbol, minted);
    return minted;
  }

  private known(space: string, symbol: string): string {
    return this.fixed(space, symbol) ?? this.symbols.get(symbol) ?? fail(`unbound symbol ${symbol}`);
  }

  private expectId(space: string, symbol: string, value: string): void {
    const fixed = this.fixed(space, symbol);
    if (fixed === undefined) return this.bind(symbol, value);
    if (fixed !== value) fail(`expected ${fixed}, got ${value}`);
  }

  private knownSet(space: string, list: Json[], what: string): Set<string> {
    return new Set(strings(list, what).map((symbol) => this.known(space, symbol)));
  }

  private names(ids: Set<string>): string[] {
    return [...ids].map((id) => this.values.get(id) ?? id).sort();
  }

  private space(name: string): SpaceUnderTest {
    return this.spaces.get(name) ?? fail(`unknown space ${name}`);
  }

  private stub(name: string): Stub {
    return env.SPACE.get(env.SPACE.idFromName(this.space(name).spaceId));
  }

  private content(symbol: string) {
    return this.contents.get(symbol) ?? fail(`unknown content ${symbol}`);
  }

  private contentsOf(list: Json[]): Chunk[] {
    return strings(list, "contents").flatMap((symbol) => this.content(symbol).chunks);
  }

  private session(step: Obj): Session {
    const device = req(optStr(step, "device"), "device");
    return this.devices.get(device) ?? fail(`unknown device ${device}`);
  }

  private async rpc(
    space: string,
    session: Session,
    atMs: number,
    call: MessageInitShape<typeof RpcRequestSchema>["call"],
  ): Promise<RpcResponse["result"]> {
    const stub = this.stub(space);
    await runInDurableObject(stub, (instance: SpaceDO) => {
      instance.clock = () => atMs;
    });
    const request = create(RpcRequestSchema, { reqId: ++this.reqId, call });
    const response = fromBinary(RpcResponseSchema, await stub.rpc(session, toBinary(RpcRequestSchema, request)));
    if (response.result.case === undefined) fail("empty response");
    return response.result;
  }

  private async step(step: Obj): Promise<void> {
    const space = optStr(step, "space") ?? "main";
    const atMs = optInt(step, "at_ms") ?? this.nowMs;
    optStr(step, "reference_only");
    const call = ["device", "space", "at_ms", "reference_only", "expect"];
    if ("commit" in step) {
      keys(step, "a step", [...call, "commit"]);
      return this.commit(space, atMs, step);
    }
    if ("upload" in step) {
      keys(step, "a step", ["device", "space", "at_ms", "reference_only", "upload"]);
      const upload = keys(step.upload, "upload", ["contents", "last_missing"]);
      const session = this.session(step);
      const contents = optList(upload, "contents");
      const lastMissing = optBool(upload, "last_missing");
      let chunks: Chunk[];
      if (contents !== undefined && lastMissing === undefined) chunks = this.contentsOf(contents);
      else if (contents === undefined && lastMissing === true) {
        const listed = this.lastMissing.get(`${session.deviceId}/${space}`) ?? fail("no MISSING_CHUNKS error was listed yet");
        const known = new Map([...this.contents.values()].flatMap(({ chunks }) => chunks.map((chunk) => [toHex(chunk.ref.hash), chunk] as const)));
        chunks = listed.map((hash) => known.get(toHex(hash)) ?? fail(`listed hash ${toHex(hash)} is no known chunk`));
      } else fail("upload needs exactly one of contents and last_missing: true");
      const receipts = await this.upload(space, session, atMs, chunks);
      const wallet = `${session.deviceId}/${space}`;
      this.wallets.set(wallet, [...(this.wallets.get(wallet) ?? []), ...receipts]);
      return;
    }
    if ("prepare_upload" in step) {
      keys(step, "a step", [...call, "prepare_upload"]);
      return this.prepareUpload(space, atMs, step);
    }
    if ("list_versions" in step) {
      keys(step, "a step", [...call, "list_versions"]);
      return this.listVersions(space, atMs, step);
    }
    keys(step, "a step", ["space", "reference_only", "head", "nodes", "live"]);
    return this.checkState(space, step);
  }

  private async upload(space: string, session: Session, atMs: number, chunks: Chunk[]): Promise<UploadReceipt[]> {
    const distinct = [...new Map(chunks.map((chunk) => [`${toHex(chunk.ref.hash)}/${chunk.ref.size}`, chunk])).values()];
    const { spaceId, key } = this.space(space);
    const receipts: UploadReceipt[] = [];
    for (let i = 0; i < distinct.length; i += 1000) {
      const part = distinct.slice(i, i + 1000);
      const result = await this.rpc(space, session, atMs, { case: "prepareUpload", value: { chunks: part.map((chunk) => chunk.ref) } });
      if (result.case !== "prepareUpload") fail(`PrepareUpload failed: ${show(result.case === "error" ? result.value.message : result.case)}`);
      const stored = result.value.chunks.map(async (plan, j) => {
        if (plan.action !== ChunkPlan_Action.UPLOAD) return [];
        const { ref, bytes } = part[j]!;
        const object = { spaceId, hashHex: toHex(ref.hash), incarnation: targetIncarnation(plan.target!.url) };
        await env.BUCKET.put(chunkKey(spaceId, object.hashHex, object.incarnation), bytes.length === ref.size ? bytes : new Uint8Array(ref.size));
        const token = await signChunkGrant(key, "receipt", object, ref.size, atMs + GRANT_MS);
        return [create(UploadReceiptSchema, { hash: ref.hash, size: ref.size, token })];
      });
      receipts.push(...(await Promise.all(stored)).flat());
    }
    return receipts;
  }

  private async prepareUpload(space: string, atMs: number, step: Obj): Promise<void> {
    const session = this.session(step);
    const body = keys(step.prepare_upload, "prepare_upload", ["chunks", "contents"]);
    const contents = optList(body, "contents");
    const listed = optList(body, "chunks");
    let chunks: Chunk[];
    if (contents !== undefined && listed === undefined) chunks = this.contentsOf(contents);
    else if (contents === undefined && listed !== undefined) chunks = await chunkList(listed);
    else fail("prepare_upload needs exactly one of chunks and contents");
    const expect = expectation(step, [], ["actions"]);
    const result = await this.rpc(space, session, atMs, { case: "prepareUpload", value: { chunks: chunks.map((chunk) => chunk.ref) } });
    const code = optStr(expect, "error");
    if (result.case === "error") {
      if (code === undefined) fail(`PrepareUpload failed: ${errorName(result.value.code)}`);
      return checkError(result.value, code);
    }
    if (code !== undefined) fail(`expected error ${code}, got a result`);
    if (result.case !== "prepareUpload") fail("unexpected response");
    const actions = result.value.chunks.map((plan) => ChunkPlan_Action[plan.action]);
    const wanted = strings(req(optList(expect, "actions"), "actions"), "actions");
    if (show(actions) !== show(wanted)) fail(`actions ${show(actions)}, expected ${show(wanted)}`);
  }

  private ops(space: string, list: Json[]): { ops: Op[]; used: string[] } {
    const ops: Op[] = [];
    const used: string[] = [];
    for (const entry of list) {
      const fields = object(entry, "an op");
      if ("repeat" in fields) {
        keys(fields, "repeat", ["repeat", "op"]);
        const n = req(optUint(fields, "repeat"), "repeat");
        const one = this.ops(space, [fields.op!]);
        for (let i = 0; i < n; i++) ops.push(...one.ops);
        used.push(...one.used);
        continue;
      }
      const kinds = Object.keys(fields);
      if (kinds.length !== 1) fail(`an op must have exactly one kind: ${show(entry)}`);
      const kind = kinds[0]!;
      if (kind === "create_dir_chain") {
        const body = keys(fields[kind], kind, ["prefix", "parent", "count"]);
        const prefix = req(optStr(body, "prefix"), "prefix");
        let parentId = this.id(space, req(optStr(body, "parent"), "parent"));
        for (let i = 1; i <= req(optUint(body, "count"), "count"); i++) {
          const symbol = `${prefix}${i}`;
          const nodeId = this.id(space, symbol);
          ops.push(create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId, name: symbol } } }));
          parentId = nodeId;
        }
        continue;
      }
      const allowed: Record<string, string[]> = {
        create_dir: ["node", "parent", "name", "pending_for"],
        move: ["node", "parent", "name", "pending_for", "from"],
        create_file: ["node", "parent", "name", "content"],
        update_content: ["node", "content", "resend"],
        delete: ["node", "requires"],
        restore: ["node", "version", "parent", "name"],
        empty: [],
      };
      if (!(kind in allowed)) fail(`unknown op ${kind}`);
      const body = keys(fields[kind], kind, allowed[kind]!);
      if (kind === "empty") {
        ops.push(create(OpSchema, {}));
        continue;
      }
      const nodeId = this.id(space, req(optStr(body, "node"), "node"));
      const parent = optStr(body, "parent");
      const parentId = parent === undefined ? "" : this.id(space, parent);
      const name = optStr(body, "name") ?? "";
      const pendingFor = optStr(body, "pending_for") ?? "";
      const content = () => {
        const symbol = req(optStr(body, "content"), "content");
        used.push(symbol);
        return this.content(symbol).content;
      };
      let op: Op;
      if (kind === "create_dir") op = create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId, name, pendingFor } } });
      else if (kind === "create_file") op = create(OpSchema, { op: { case: "createFile", value: { nodeId, parentId, name, content: content() } } });
      else if (kind === "update_content") op = create(OpSchema, { op: { case: "updateContent", value: { nodeId, content: content(), resend: optBool(body, "resend") ?? false } } });
      else if (kind === "move") {
        const from = body.from === undefined ? undefined : keys(body.from, "from", ["parent", "name"]);
        const place = from && { parentId: this.id(space, req(optStr(from, "parent"), "parent")), name: req(optStr(from, "name"), "name") };
        op = create(OpSchema, { op: { case: "move", value: { nodeId, newParentId: parentId, newName: name, pendingFor, from: place } } });
      }
      else if (kind === "delete") {
        const found = "requires" in body ? keys(body.requires, "requires", ["node", "content_rev", "meta_rev"]) : undefined;
        const requires = found && {
          nodeId: this.id(space, req(optStr(found, "node"), "node")),
          contentRev: BigInt(req(optUint(found, "content_rev"), "content_rev")),
          metaRev: BigInt(req(optUint(found, "meta_rev"), "meta_rev")),
        };
        op = create(OpSchema, { op: { case: "delete", value: { nodeId, requires } } });
      }
      else {
        const version = optStr(body, "version");
        const versionId = version === undefined ? "" : this.id(space, version);
        op = create(OpSchema, { op: { case: "restore", value: { nodeId, versionId, targetParentId: parentId, name } } });
      }
      ops.push(op);
    }
    return { ops, used };
  }

  private async commit(space: string, atMs: number, step: Obj): Promise<void> {
    const session = this.session(step);
    const body = keys(step.commit, "commit", [
      "commit_id",
      "base_seq",
      "history_id",
      "device_seq",
      "require_head",
      "ops",
      "upload",
      "receipts_from",
      "receipt_size_delta",
    ]);
    const { ops, used } = this.ops(space, req(optList(body, "ops"), "ops"));
    const commitSymbol = optStr(body, "commit_id");
    const commitId = commitSymbol === undefined ? uuidv7(this.nowMs) : this.id(space, commitSymbol);
    const upload = optBool(body, "upload") ?? true;
    if (upload && ("receipts_from" in body || "receipt_size_delta" in body))
      fail("receipts_from and receipt_size_delta apply only with upload false");
    let receipts: UploadReceipt[];
    if (!upload) {
      const from = optStr(body, "receipts_from") ?? space;
      const delta = optU32(body, "receipt_size_delta") ?? 0;
      receipts = (this.wallets.get(`${session.deviceId}/${from}`) ?? []).map((r) => create(UploadReceiptSchema, { ...r, size: r.size + delta }));
    } else {
      receipts = await this.upload(
        space,
        session,
        atMs,
        used.flatMap((symbol) => this.content(symbol).chunks),
      );
    }
    const device = `${session.deviceId}/${space}`;
    const history = optStr(body, "history_id");
    const historyId = history === undefined ? this.space(space).historyId : this.id(space, history);
    const given = optUint(body, "device_seq");
    const sent = this.sentSlots.get(`${device}/${commitId}`);
    const deviceSeq = given === undefined ? (sent ?? (this.slots.get(device) ?? 0n) + 1n) : BigInt(given);
    if (sent === undefined) this.sentSlots.set(`${device}/${commitId}`, deviceSeq);
    const expect = expectation(step, ["missing", "missing_some"], ["seq", "results", "changed", "response"]);
    const result = await this.rpc(space, session, atMs, {
      case: "commit",
      value: {
        commitId,
        baseSeq: BigInt(req(optUint(body, "base_seq"), "base_seq")),
        ops,
        receipts,
        historyId,
        deviceSeq,
        requireHead: optBool(body, "require_head") ?? false,
      },
    });
    const code = optStr(expect, "error");
    if (result.case === "error") {
      const error = result.value;
      if (error.missingChunks.length > 0) this.lastMissing.set(device, error.missingChunks);
      if (code === undefined) fail(`commit failed with ${errorName(error.code)}: ${error.message}`);
      checkError(error, code);
      if ("missing_some" in expect) {
        const some = keys(expect.missing_some, "missing_some", ["contents", "count", "total"]);
        const from = new Set(this.contentsOf(req(optList(some, "contents"), "contents")).map((chunk) => toHex(chunk.ref.hash)));
        const count = req(optUint(some, "count"), "count");
        if (error.missingChunks.length !== count) fail(`${error.missingChunks.length} missing chunks listed, expected ${count}`);
        const stray = error.missingChunks.find((hash) => !from.has(toHex(hash)));
        if (stray) fail(`missing chunk ${toHex(stray)} is not in contents`);
        const total = String(req(optUint(some, "total"), "total"));
        if (/\d+/.exec(error.message)?.[0] !== total) fail(`message ${show(error.message)} does not give the total ${total} first`);
      }
      const missing = optList(expect, "missing");
      if (missing !== undefined) {
        const want = new Set(this.contentsOf(missing).map((chunk) => toHex(chunk.ref.hash)));
        const got = new Set(error.missingChunks.map(toHex));
        if (!sameSet(got, want)) fail(`missing_chunks differ: ${got.size} listed`);
      }
      return;
    }
    if (code !== undefined) fail(`expected error ${code}, got a result`);
    if (result.case !== "commit") fail("unexpected response");
    if (result.value.seq > (this.topSeq.get(space) ?? 0n)) {
      this.topSeq.set(space, result.value.seq);
      this.slots.set(device, deviceSeq);
    }
    const committed = result.value;
    if ("response" in expect) {
      keys(expect, "a full expect", ["response"]);
      this.checkResponse(space, expect.response!, committed);
      return;
    }
    const seq = optUint(expect, "seq");
    if (seq !== undefined && committed.seq !== BigInt(seq)) fail(`seq ${committed.seq}, expected ${seq}`);
    const results = optList(expect, "results");
    if (results !== undefined) {
      if (results.length !== committed.results.length) fail(`${committed.results.length} results, expected ${results.length}`);
      for (const [i, want] of results.entries()) {
        try {
          this.checkResult(space, want, committed.results[i]!, false);
        } catch (error) {
          if (error instanceof Failure) fail(`result ${i + 1}: ${error.message}`);
          throw error;
        }
      }
    }
    const changed = optList(expect, "changed");
    if (changed !== undefined) {
      const want = this.knownSet(space, changed, "changed");
      const got = new Set(committed.changes.flatMap((change) => (change.change.case === "node" ? [change.change.value.nodeId] : [])));
      if (!sameSet(got, want)) fail(`changed ${show(this.names(got))}, expected ${show(this.names(want))}`);
    }
  }

  private checkResponse(space: string, wanted: Json, got: CommitResult): void {
    const fields = ["seq", "committed_at_ms", "results", "changes"];
    const want = keys(wanted, "response", fields);
    every(want, fields);
    covered(CommitResultSchema, ["seq", "committedAtMs", "results", "changes"]);
    const seq = req(optUint(want, "seq"), "seq");
    if (got.seq !== BigInt(seq)) fail(`seq ${got.seq}, expected ${seq}`);
    const at = req(optInt(want, "committed_at_ms"), "committed_at_ms");
    if (got.committedAtMs !== BigInt(at)) fail(`committed_at_ms ${got.committedAtMs}, expected ${at}`);
    const results = req(optList(want, "results"), "results");
    if (results.length !== got.results.length) fail(`${got.results.length} results, expected ${results.length}`);
    for (const [i, result] of results.entries()) within(`result ${i + 1}`, () => this.checkResult(space, result, got.results[i]!, true));
    const changes = req(optList(want, "changes"), "changes");
    if (changes.length !== got.changes.length) fail(`${got.changes.length} changes, expected ${changes.length}`);
    for (const [i, change] of changes.entries()) within(`change ${i + 1}`, () => this.checkChange(space, change, got.changes[i]!));
  }

  private checkChange(space: string, wanted: Json, got: ChangeRecord): void {
    const want = keys(wanted, "a change", ["node"]);
    covered(ChangeRecordSchema, ["node", "purged", "space", "member"]);
    const change = got.change;
    if (change.case !== "node") fail(`${change.case === undefined ? "an empty change" : `a ${change.case} change`}, expected a node`);
    if (want.node === undefined) fail("missing field node");
    this.checkNodeState(space, want.node, change.value, true);
  }

  private checkResult(space: string, wanted: Json, got: OpResult, complete: boolean): void {
    const fields = ["code", "node", "name", "reason"];
    const want = keys(wanted, "a result", fields);
    if (complete) every(want, fields);
    covered(OpResultSchema, ["code", "nodeId", "name", "rejectReason"]);
    const code = OpResult_Code[got.code] ?? "?";
    const wantCode = req(optStr(want, "code"), "code");
    if (code !== wantCode) fail(`${code} ${show(got.rejectReason)}, expected ${wantCode}`);
    const name = optStr(want, "name");
    if (name !== undefined && got.name !== name) fail(`name ${show(got.name)}, expected ${show(name)}`);
    const reason = optStr(want, "reason");
    if (reason !== undefined && got.rejectReason !== reason) fail(`reason ${show(got.rejectReason)}, expected ${reason}`);
    const node = optStr(want, "node");
    if (node !== undefined) this.expectId(space, node, got.nodeId);
  }

  private device(symbol: string): Session {
    return this.devices.get(symbol) ?? fail(`unknown device ${symbol}`);
  }

  private checkCurrent(space: string, wanted: Json, got: VersionInfo): void {
    const want = keys(wanted, "a current version", CURRENT_FIELDS);
    every(want, CURRENT_FIELDS);
    covered(VersionInfoSchema, ["versionId", "seq", "content", "deviceId", "userId", "createdAtMs"]);
    within("version", () => this.expectId(space, req(optStr(want, "version"), "version"), got.versionId));
    const seq = req(optUint(want, "seq"), "seq");
    if (got.seq !== BigInt(seq)) fail(`seq ${got.seq}, expected ${seq}`);
    const at = req(optInt(want, "created_at_ms"), "created_at_ms");
    if (got.createdAtMs !== BigInt(at)) fail(`created_at_ms ${got.createdAtMs}, expected ${at}`);
    const device = this.device(req(optStr(want, "device"), "device"));
    if (got.deviceId !== device.deviceId) fail(`device_id ${got.deviceId}, expected ${device.deviceName}'s`);
    const user = this.device(req(optStr(want, "user"), "user"));
    if (got.userId !== user.userId) fail(`user_id ${got.userId}, expected ${user.userId}`);
    const inline = req(optBool(want, "chunks_inline"), "chunks_inline");
    const content = want.content;
    if (content === null) {
      if (got.content) fail("a content, expected none");
    } else if (typeof content !== "string") fail(`content must be a string or null, got ${show(content)}`);
    else if (!got.content) fail(`no content, expected ${content}`);
    else this.checkFullContent(got.content, content, inline);
  }

  private checkFullContent(got: Content, symbol: string, inline: boolean): void {
    covered(ContentSchema, ["size", "mtimeNs", "executable", "chunks", "chunkCount", "manifestDigest", "chunker"]);
    covered(ChunkRefSchema, ["hash", "size", "encoding"]);
    const want = this.content(symbol).content;
    const listed = inline ? want.chunks : [];
    const sameChunks =
      got.chunks.length === listed.length &&
      got.chunks.every((chunk, i) => toHex(chunk.hash) === toHex(listed[i]!.hash) && chunk.size === listed[i]!.size && chunk.encoding === listed[i]!.encoding);
    const fields: [string, boolean][] = [
      ["size", got.size === want.size],
      ["mtime_ns", got.mtimeNs === want.mtimeNs],
      ["executable", got.executable === want.executable],
      ["chunks", sameChunks],
      ["chunk_count", got.chunkCount === want.chunkCount],
      ["manifest_digest", toHex(got.manifestDigest) === toHex(want.manifestDigest)],
      ["chunker", got.chunker === want.chunker],
    ];
    const differ = fields.filter(([, same]) => !same).map(([field]) => field);
    if (differ.length > 0) fail(`content differs from ${symbol} in ${differ.join(", ")}`);
  }

  private checkContent(got: Content | undefined, symbol: string): void {
    const want = this.content(symbol).content;
    if (!got) fail(`no content, expected ${symbol}`);
    const same =
      got.size === want.size &&
      toHex(got.manifestDigest) === toHex(want.manifestDigest) &&
      got.executable === want.executable &&
      got.mtimeNs === this.mtimeNs;
    if (!same) fail(`content differs from ${symbol}`);
  }

  private checkVersion(space: string, got: VersionInfo, wanted: Json): void {
    const want = keys(wanted, "a version", ["version", "seq", "content"]);
    const seq = optUint(want, "seq");
    if (seq !== undefined && got.seq !== BigInt(seq)) fail(`version seq ${got.seq}, expected ${seq}`);
    const version = optStr(want, "version");
    if (version !== undefined) this.expectId(space, version, got.versionId);
    const content = optStr(want, "content");
    if (content !== undefined) this.checkContent(got.content, content);
  }

  private async listVersions(space: string, atMs: number, step: Obj): Promise<void> {
    const session = this.session(step);
    const body = keys(step.list_versions, "list_versions", ["node", "cursor", "limit"]);
    let cursor = "";
    if (typeof body.cursor === "string") {
      if (!body.cursor.startsWith("=")) fail(`a cursor string must be a = literal, got ${show(body.cursor)}`);
      cursor = body.cursor.slice(1);
    } else if ("cursor" in body) {
      const pair = keys(body.cursor, "cursor", ["returned"]);
      const name = req(optStr(pair, "returned"), "returned");
      cursor = this.cursors.get(name) ?? fail(`no returned cursor ${name}`);
    }
    const nodeId = this.id(space, req(optStr(body, "node"), "node"));
    const expect = expectation(step, [], ["count", "versions", "next_cursor"]);
    const result = await this.rpc(space, session, atMs, { case: "listVersions", value: { nodeId, cursor, limit: optU32(body, "limit") ?? 0 } });
    const code = optStr(expect, "error");
    if (result.case === "error") {
      if (code === undefined) fail(`ListVersions failed: ${errorName(result.value.code)}`);
      return checkError(result.value, code);
    }
    if (code !== undefined) fail(`expected error ${code}, got a result`);
    if (result.case !== "versions") fail("unexpected response");
    const list = result.value;
    const count = optUint(expect, "count");
    if (count !== undefined && list.versions.length !== count) fail(`${list.versions.length} versions, expected ${count}`);
    const versions = optList(expect, "versions");
    if (versions !== undefined) {
      if (versions.length !== list.versions.length) fail(`${list.versions.length} versions, expected ${versions.length}`);
      for (const [i, want] of versions.entries()) this.checkVersion(space, list.versions[i]!, want);
    }
    if (!("next_cursor" in expect)) return;
    if (expect.next_cursor === "") {
      if (list.nextCursor !== "") fail(`next_cursor ${show(list.nextCursor)}, expected the last page`);
      return;
    }
    const pair = keys(expect.next_cursor, "next_cursor", ["bind"]);
    const name = req(optStr(pair, "bind"), "bind");
    if (list.nextCursor === "") fail("next_cursor is empty, expected another page");
    if (this.cursors.has(name)) fail(`cursor ${name} is bound twice`);
    this.cursors.set(name, list.nextCursor);
  }

  private read<T>(space: string, read: (sql: SqlStorage) => T): Promise<T> {
    return runInDurableObject(this.stub(space), (_instance, state) => read(state.storage.sql));
  }

  private async checkState(space: string, step: Obj): Promise<void> {
    let known = false;
    const head = optUint(step, "head");
    if (head !== undefined) {
      known = true;
      const got = await this.read(space, (sql) => Number(getMeta(sql, "head_seq")));
      if (got !== head) fail(`head_seq ${got}, expected ${head}`);
    }
    const nodes = optList(step, "nodes");
    if (nodes !== undefined) {
      known = true;
      for (const want of nodes) await this.checkNode(space, want);
    }
    const live = optList(step, "live");
    if (live !== undefined) {
      known = true;
      const root = this.space(space).spaceId;
      const got = new Set(
        await this.read(space, (sql) =>
          sql
            .exec("SELECT node_id FROM nodes WHERE status = ? AND node_id != ?", NodeStatus.LIVE, root)
            .toArray()
            .map((row) => String(row.node_id)),
        ),
      );
      const want = this.knownSet(space, live, "live");
      if (!sameSet(got, want)) fail(`live nodes ${show(this.names(got))}, expected ${show(this.names(want))}`);
    }
    if (!known) fail(`a step does nothing: ${show(step)}`);
  }

  private async checkNode(space: string, wanted: Json): Promise<void> {
    const want = object(wanted, "a node");
    const symbol = req(optStr(want, "node"), "node");
    const id = this.known(space, symbol);
    const got: NodeState | undefined = await this.read(space, (sql) => {
      const node = getNode(sql, id);
      return node && nodeState(sql, node);
    });
    const absent = optBool(want, "absent");
    if (absent !== undefined) {
      if (!absent) fail(`${symbol}: absent may only be true`);
      keys(want, "an absent node", ["node", "absent"]);
      if (got) fail(`${symbol} exists, expected it absent`);
      return;
    }
    if (!got) fail(`${symbol} does not exist`);
    this.checkNodeState(space, want, got, "current" in want);
  }

  private checkNodeState(space: string, wanted: Json, got: NodeState, complete: boolean): void {
    const want = keys(wanted, "a node", complete ? [...NODE_FIELDS, "edit_rev", "meta_edit_rev"] : [...NODE_FIELDS, "absent", "content", "edit_rev", "meta_edit_rev"]);
    if (complete) every(want, NODE_FIELDS);
    covered(NodeStateSchema, ["nodeId", "parentId", "name", "kind", "status", "contentRev", "metaRev", "trashGroup", "current", "pendingFor", "pendingBy", "editRev", "metaEditRev"]);
    const symbol = req(optStr(want, "node"), "node");
    within("node", () => this.expectId(space, symbol, got.nodeId));
    const differs = (field: string, value: unknown) => fail(`${symbol}.${field} is ${String(value)}, expected ${show(want[field])}`);
    const parent = optStr(want, "parent");
    if (parent !== undefined) {
      try {
        this.expectId(space, parent, got.parentId);
      } catch (error) {
        if (error instanceof Failure) fail(`${symbol}.parent: ${error.message}`);
        throw error;
      }
    }
    const name = optStr(want, "name");
    if (name !== undefined && got.name !== name) differs("name", got.name);
    const kind = optStr(want, "kind");
    if (kind !== undefined && NodeKind[got.kind] !== kind) differs("kind", NodeKind[got.kind]);
    const status = optStr(want, "status");
    if (status !== undefined && NodeStatus[got.status] !== status) differs("status", NodeStatus[got.status]);
    const contentRev = optUint(want, "content_rev");
    if (contentRev !== undefined && got.contentRev !== BigInt(contentRev)) differs("content_rev", got.contentRev);
    const metaRev = optUint(want, "meta_rev");
    if (metaRev !== undefined && got.metaRev !== BigInt(metaRev)) differs("meta_rev", got.metaRev);
    const editRev = optUint(want, "edit_rev");
    if (editRev !== undefined && got.editRev !== BigInt(editRev)) differs("edit_rev", got.editRev);
    const metaEditRev = optUint(want, "meta_edit_rev");
    if (metaEditRev !== undefined && got.metaEditRev !== BigInt(metaEditRev)) differs("meta_edit_rev", got.metaEditRev);
    if ("trash_group" in want) {
      const group = want.trash_group;
      if (group === null) {
        if (got.trashGroup !== "") differs("trash_group", got.trashGroup);
      } else if (typeof group === "string") {
        try {
          this.expectId(space, group, got.trashGroup);
        } catch (error) {
          if (error instanceof Failure) fail(`${symbol}.trash_group: ${error.message}`);
          throw error;
        }
      } else fail(`trash_group must be a string or null, got ${show(group)}`);
    }
    if ("pending_for" in want) {
      const pendingFor = want.pending_for;
      if (pendingFor !== null && typeof pendingFor !== "string") fail(`pending_for must be a string or null, got ${show(pendingFor)}`);
      if (got.pendingFor !== (pendingFor ?? "")) differs("pending_for", got.pendingFor);
    }
    if ("pending_by" in want) {
      const pendingBy = want.pending_by;
      if (pendingBy !== null && typeof pendingBy !== "string") fail(`pending_by must be a device or null, got ${show(pendingBy)}`);
      if (got.pendingBy !== (pendingBy === null ? "" : this.device(pendingBy).deviceId)) differs("pending_by", got.pendingBy);
    }
    const content = optStr(want, "content");
    if (content !== undefined) {
      try {
        this.checkContent(got.current?.content, content);
      } catch (error) {
        if (error instanceof Failure) fail(`${symbol}: ${error.message}`);
        throw error;
      }
    }
    if ("current" in want) {
      const current = want.current;
      if (current === null) {
        if (got.current) fail(`${symbol} has a current version, expected none`);
      } else if (!got.current) fail(`${symbol} has no current version, expected one`);
      else within(`${symbol}.current`, () => this.checkCurrent(space, current!, got.current!));
    }
  }
}

async function run(scenario: Json, file: Obj = doc): Promise<string | undefined> {
  try {
    const found = object(scenario, "a scenario");
    const runner = await Run.start(found, file);
    await runner.steps({
      ...found,
      steps: req(optList(found, "steps"), "steps").filter((step) => !(object(step, "a step").reference_only !== undefined)),
    });
    return undefined;
  } catch (error) {
    if (error instanceof Failure) return error.message;
    throw error;
  }
}

for (const [path, file] of files) {
  keys(file, "the file", ["format", "version", "spec", "unicode", "now_ms", "mtime_ns", "conventions", "scenarios"]);
  describe(`space conformance (spec/testdata/scenarios/${path})`, () => {
    it("reads format koidrive.space-conformance version 1", () => {
      expect([file.format, file.version]).toEqual(["koidrive.space-conformance", 1]);
    });

    for (const scenario of req(optList(file, "scenarios"), "scenarios")) {
      const { name, reference_only } = object(scenario, "a scenario");
      const test = reference_only === undefined ? it : it.skip;
      test(String(name), { timeout: 120_000 }, async () => {
        expect(await run(scenario, file)).toBeUndefined();
      });
    }
  });
}

const scenarios = req(optList(doc, "scenarios"), "scenarios");

describe("conformance runner self-tests", () => {
  const base = scenarios[0]!;
  const long = scenarios.find((s) => object(s, "a scenario").name === "a-long-missing-chunk-list-is-capped-and-the-resent-commit-applies")!;
  const mutate = async (scenario: Json, edit: (s: { steps: Obj[] }) => void): Promise<string | undefined> => {
    const copy = structuredClone(scenario) as unknown as { steps: Obj[] };
    edit(copy);
    return run(copy as unknown as Json);
  };
  const cases: [string, Json, (s: { steps: Obj[] }) => void, string][] = [
    [
      "a literal ID in an expectation is compared",
      base,
      (s) => void (object((s.steps[2]!.nodes as Json[])[0], "node").parent = "=not-the-root"),
      "step 3: f1.parent: expected not-the-root",
    ],
    [
      "an assertion of the wrong type fails",
      base,
      (s) => void (object(s.steps[0]!.expect, "expect").seq = "999"),
      "step 1: seq must be a non-negative integer",
    ],
    [
      "an unknown assertion key fails",
      base,
      (s) => {
        const expect = object(s.steps[0]!.expect, "expect");
        expect.sqe = expect.seq!;
        delete expect.seq;
      },
      "step 1: unknown field sqe in expect",
    ],
    [
      "a field for the other branch fails",
      base,
      (s) => void (object(s.steps[0]!.expect, "expect").missing = 123),
      "step 1: unknown field missing in expect",
    ],
    ["a wrong numeric assertion fails", base, (s) => void (object(s.steps[0]!.expect, "expect").seq = 999), "step 1: seq 1, expected 999"],
    [
      "a total that is only the listed count fails",
      long,
      (s) => void (object(object(s.steps[0]!.expect, "expect").missing_some, "missing_some").total = 1000),
      "does not give the total 1000 first",
    ],
    ["an unknown scenario field fails", base, (s) => void ((s as unknown as Obj).covres = ["R1"]), "unknown field covres in a scenario"],
    ["an unknown step field fails", base, (s) => void (s.steps[0]!.devcie = "a"), "step 1: unknown field devcie in a step"],
    ["an unknown commit field fails", base, (s) => void (object(s.steps[0]!.commit, "commit").base = 0), "step 1: unknown field base in commit"],
    [
      "an unknown op field fails",
      base,
      (s) => void (object(object((object(s.steps[0]!.commit, "commit").ops as Json[])[0], "op").create_file, "create_file").mode = "x"),
      "step 1: unknown field mode in create_file",
    ],
    [
      "an unknown result field fails",
      base,
      (s) => void (object((object(s.steps[0]!.expect, "expect").results as Json[])[0], "result").nmae = "x"),
      "step 1: result 1: unknown field nmae in a result",
    ],
    ["an unknown node field fails", base, (s) => void (object((s.steps[2]!.nodes as Json[])[0], "node").stauts = "LIVE"), "step 3: unknown field stauts in a node"],
  ];
  for (const [name, scenario, edit, reason] of cases) {
    it(name, async () => {
      expect(await mutate(scenario, edit)).toContain(reason);
    });
  }
});

describe("number parity with the Rust runner (spec/testdata/scenarios/runner-numbers-v1.json)", () => {
  it("the fixture with repeat 1024.0 fails", () => {
    const fixture = hand.replace('"repeat": 1024\n', '"repeat": 1024.0\n');
    expect(fixture).not.toBe(hand);
    expect(() => parseDoc(fixture)).toThrow("numbers must be written as plain integers, got 1024.0");
  });
  for (const entry of numbers.cases) {
    it(`repeat-${entry.repeat}`, async () => {
      const text = `{"name": "repeat", "devices": {"a": {"name": "A"}}, "contents": {"c": {"chunks": [{"text": "x", "repeat": ${entry.repeat}}]}}, "steps": []}`;
      let got: string | undefined;
      try {
        got = await run(parseDoc(text));
      } catch (error) {
        if (!(error instanceof Failure)) throw error;
        got = error.message;
      }
      if (entry.error === null) expect(got).toBeUndefined();
      else expect(got).toContain(entry.error);
    });
  }
});
