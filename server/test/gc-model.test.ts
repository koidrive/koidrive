// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import fc from "fast-check";
import { beforeAll, expect, inject, it, vi } from "vitest";
import { ClaimRequestSchema, type DeviceCredentials, DeviceCredentialsSchema } from "../src/gen/koidrive/v1/auth_pb";
import { type UploadReceipt, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ChunkBundleResultSchema, ChunkBundleSchema, ChunkPlan_Action, Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { sha256 } from "../src/crypto";
import { fromHex, toHex, uuidv7 } from "../src/ids";
import { type Session, SpaceDO } from "../src/space-do";
import { ChunkState } from "../src/space/schema";
import { chunkKey, versionChunks, type VersionRow } from "../src/space/store";
import saved from "./gc-model.cases.json";
import { content, op, TestClient } from "./helpers";
import { simulatedPitr } from "./pitr";

declare module "vitest" {
  export interface ProvidedContext {
    batteryCases: string;
    deterministic: string;
  }
}

simulatedPitr.install();

const BASE = "https://koidrive.test";
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const CHUNK_KEY = /^spaces\/[^/]+\/chunks\//;
const TEXTS = ["alpha", "bravo", "charlie"];
const FILES = 2;
const LIST_PAGE = 2;

type Outcome = "ok" | "lostApplied" | "lostInflight";
type Retirement = "ok" | "refused" | "lostBefore" | "lostAfter";

type Pause = "head" | "put" | "retire" | "delete";

type Simple =
  | { t: "prepare"; chunks: number[] }
  | { t: "put"; plan: number; outcome: Outcome }
  | { t: "bundle"; plans: number[]; outcome: Outcome }
  | { t: "commit"; file: number; receipts: number[]; extra: number[] }
  | { t: "trash"; file: number }
  | { t: "drop"; version: number }
  | { t: "gc"; retire: Retirement; del: Outcome }
  | { t: "land"; flight: number }
  | { t: "export" }
  | { t: "crash" }
  | { t: "rewind"; point: number; from?: "newest" | "oldest" | "drop" }
  | { t: "targets"; chunk: number }
  | { t: "download"; target: number }
  | { t: "clock"; ms: number }
  | { t: "until"; mark: "grace" | "hold"; chunk: number; offsetMs: number };

type Step = Simple | { t: "race"; outer: Simple; at: Pause; inner: Simple[]; innerAt?: Pause };

type Flight = { put: string; bytes: Uint8Array; onlyIfAbsent: boolean } | { delete: string[] };

const r2 = {
  objects: new Map<string, { bytes: Uint8Array; uploadedMs: number }>(),
  flights: [] as Flight[],
  put: "ok" as Outcome,
  del: "ok" as Outcome,
  retire: "ok" as Retirement,
  retired: new Set<string>(),
  violations: [] as string[],
};

interface Gate {
  reached: boolean;
  released: boolean;
}

const pauses = new Map<Pause, Gate>();

async function until(done: () => boolean): Promise<void> {
  while (!done()) await new Promise((resolve) => setTimeout(resolve, 5));
}

async function pause(at: Pause): Promise<void> {
  const gate = pauses.get(at);
  if (!gate) return;
  pauses.delete(at);
  gate.reached = true;
  await until(() => gate.released);
}

interface World {
  spaceId: string;
  session: Session;
  files: string[];
  plans: { chunk: number; url: string }[];
  receipts: UploadReceipt[];
  targets: { chunk: number; url: string; expiresAtMs: number }[];
  points: number[];
  drops: number[];
  history: { atMs: number; keys: string[] }[];
}

let owner: DeviceCredentials;
const chunks: { hex: string; bytes: Uint8Array }[] = [];

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

function space(spaceId: string) {
  return env.SPACE.get(env.SPACE.idFromName(spaceId));
}

function land(flight: Flight): void {
  if ("delete" in flight) {
    for (const key of flight.delete) r2.objects.delete(key);
  } else if (!(flight.onlyIfAbsent && r2.objects.has(flight.put))) {
    r2.objects.set(flight.put, { bytes: flight.bytes, uploadedMs: Date.now() });
  }
}

function settle(flight: Flight, outcome: Outcome): void {
  if (outcome === "lostInflight") r2.flights.push(flight);
  else land(flight);
  if (outcome !== "ok") throw new Error("R2 did not answer");
}

function described(key: string) {
  const stored = r2.objects.get(key);
  if (!stored) return null;
  const { bytes, uploadedMs } = stored;
  return { key, size: bytes.length, uploaded: new Date(uploadedMs), etag: key, httpEtag: `"${key}"` };
}

async function retiredKeys(spaceId: string): Promise<Set<string>> {
  const rows = await runInDurableObject(account(), (_instance, state) =>
    state.storage.sql.exec("SELECT hash_hex, incarnation FROM chunk_retirements WHERE space_id = ?", spaceId).toArray(),
  );
  return new Set(rows.map((row) => chunkKey(spaceId, String(row.hash_hex), String(row.incarnation))));
}

function installModelR2(): void {
  const bucket = env.BUCKET;
  const real = { put: bucket.put.bind(bucket), get: bucket.get.bind(bucket), head: bucket.head.bind(bucket), delete: bucket.delete.bind(bucket), list: bucket.list.bind(bucket) };
  vi.spyOn(bucket, "put").mockImplementation((async (key: string, value: Parameters<R2Bucket["put"]>[1], options?: R2PutOptions) => {
    if (!CHUNK_KEY.test(key)) return real.put(key, value, options);
    const bytes = new Uint8Array(await new Response(value).arrayBuffer());
    if (options?.sha256 !== undefined && options.sha256 !== toHex(await sha256(bytes))) {
      throw new Error("put: The SHA-256 checksum you specified did not match what we received.");
    }
    await pause("put");
    const onlyIfAbsent = (options?.onlyIf as R2Conditional | undefined)?.etagDoesNotMatch === "*";
    if (onlyIfAbsent && r2.objects.has(key)) return null;
    settle({ put: key, bytes, onlyIfAbsent }, r2.put);
    return described(key);
  }) as never);
  vi.spyOn(bucket, "head").mockImplementation((async (key: string) => {
    if (!CHUNK_KEY.test(key)) return real.head(key);
    const found = described(key);
    await pause("head");
    return found;
  }) as never);
  vi.spyOn(bucket, "get").mockImplementation((async (key: string, options?: R2GetOptions) => {
    if (!CHUNK_KEY.test(key)) return real.get(key, options);
    const found = described(key);
    const bytes = r2.objects.get(key)?.bytes;
    return found && bytes ? { ...found, body: new Blob([bytes]).stream(), arrayBuffer: async () => bytes.slice().buffer } : null;
  }) as never);
  vi.spyOn(bucket, "delete").mockImplementation((async (keys: string | string[]) => {
    const all = typeof keys === "string" ? [keys] : keys;
    const others = all.filter((key) => !CHUNK_KEY.test(key));
    if (others.length > 0) await real.delete(others);
    const modelled = all.filter((key) => CHUNK_KEY.test(key));
    if (modelled.length === 0) return;
    const spaceId = modelled[0]!.split("/")[1]!;
    const retired = await retiredKeys(spaceId);
    for (const key of modelled) if (!retired.has(key)) r2.violations.push(`delete issued for ${key}, which the Account DO has not retired`);
    await pause("delete");
    settle({ delete: modelled }, r2.del);
  }) as never);
  vi.spyOn(bucket, "list").mockImplementation((async (options?: R2ListOptions) => {
    const prefix = options?.prefix ?? "";
    if (!CHUNK_KEY.test(prefix)) return real.list(options);
    const keys = [...r2.objects.keys()].filter((key) => key.startsWith(prefix) && key > (options?.cursor ?? "")).sort();
    const page = keys.slice(0, Math.min(options?.limit ?? 1000, LIST_PAGE));
    const truncated = page.length < keys.length;
    return { objects: page.map(described), truncated, ...(truncated ? { cursor: page.at(-1) } : {}), delimitedPrefixes: [] };
  }) as never);
}

function installRetirementOutcomes(): void {
  const prototype = SpaceDO.prototype as unknown as { account(...args: unknown[]): object };
  const directory = prototype.account;
  vi.spyOn(prototype, "account").mockImplementation(function (this: unknown, ...args: unknown[]) {
    const target = directory.apply(this, args) as Record<string | symbol, unknown> & {
      retireChunks(spaceId: string, objects: { hashHex: string; incarnation: string }[]): Promise<unknown>;
    };
    const retire = async (spaceId: string, objects: { hashHex: string; incarnation: string }[]) => {
      const outcome = (await target.retireChunks(spaceId, objects)) as { ok: boolean };
      if (outcome.ok) for (const { hashHex, incarnation } of objects) r2.retired.add(chunkKey(spaceId, hashHex, incarnation));
      return outcome;
    };
    const retireChunks = async (spaceId: string, objects: { hashHex: string; incarnation: string }[]) => {
      await pause("retire");
      switch (r2.retire) {
        case "ok":
          return retire(spaceId, objects);
        case "refused":
          return { ok: false, code: Error_Code.RETRY_LATER, message: "the model Account DO refused" };
        case "lostBefore":
          throw new Error("the Account DO did not answer");
        case "lostAfter":
          await retire(spaceId, objects);
          throw new Error("the Account DO did not answer");
      }
    };
    return new Proxy(target, {
      get(object, property) {
        if (property === "retireChunks") return retireChunks;
        const value = Reflect.get(object, property);
        return typeof value === "function" ? value.bind(object) : value;
      },
    });
  });
}

async function claimOwner(): Promise<DeviceCredentials> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  log.mockRestore();
  const request = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code: line!.split(": ")[1]!, userName: "Owner", deviceName: "Laptop", platform: "macos" }));
  const response = await exports.default.fetch(BASE + "/v1/claim", { method: "POST", headers: { "Content-Type": "application/x-protobuf" }, body: request });
  return fromBinary(DeviceCredentialsSchema, new Uint8Array(await response.arrayBuffer()));
}

beforeAll(async () => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(Date.now() + DAY);
  for (const text of TEXTS) {
    const bytes = new TextEncoder().encode(text);
    chunks.push({ hex: toHex(await sha256(bytes)), bytes });
  }
  owner = await claimOwner();
  installModelR2();
  installRetirementOutcomes();
});

async function newWorld(): Promise<World> {
  vi.setSystemTime(Date.now() + DAY);
  r2.flights = [];
  r2.violations = [];
  const created = await account().createSpace({ userId: owner.userId, name: `gc ${uuidv7()}` });
  if (!created.ok) throw new Error(created.message);
  const spaceId = created.value.spaceId;
  const session: Session = { deviceId: owner.deviceId, userId: owner.userId, deviceName: "Laptop", kind: "device", scope: null, role: "owner", maxFormat: 1, origin: BASE };
  const world: World = { spaceId, session, files: Array.from({ length: FILES }, () => uuidv7()), plans: [], receipts: [], targets: [], points: [], drops: [], history: [] };
  await snapshot(world);
  return world;
}

function pick<T>(items: T[], index: number): T | undefined {
  return items.length === 0 ? undefined : items[items.length - 1 - (index % items.length)];
}

async function device(world: World): Promise<TestClient> {
  const client = new TestClient(space(world.spaceId), world.spaceId, world.session);
  const seqs = await runInDurableObject(space(world.spaceId), (instance, state) => ({
    head: BigInt(Number(state.storage.sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value)),
    accepted: (instance as unknown as { acceptedDeviceCommit(deviceId: string): { deviceSeq: bigint } }).acceptedDeviceCommit(world.session.deviceId).deviceSeq,
  }));
  client.baseSeq = seqs.head;
  client.acceptedSeq = seqs.accepted;
  return client;
}

function nodeStatus(world: World, nodeId: string): Promise<number | undefined> {
  return runInDurableObject(space(world.spaceId), (_instance, state) => {
    const row = state.storage.sql.exec("SELECT status FROM nodes WHERE node_id = ?", nodeId).toArray()[0];
    return row === undefined ? undefined : Number(row.status);
  });
}

async function putChunk(url: string, bytes: Uint8Array): Promise<UploadReceipt | undefined> {
  const response = await exports.default.fetch(url, { method: "PUT", body: bytes, headers: { "Content-Length": String(bytes.length) } });
  const body = new Uint8Array(await response.arrayBuffer());
  return response.ok ? fromBinary(UploadReceiptSchema, body) : undefined;
}

async function act(world: World, step: Step): Promise<boolean> {
  const { spaceId } = world;
  switch (step.t) {
    case "prepare": {
      const refs = step.chunks.map((i) => ({ hash: fromHex(chunks[i]!.hex)!, size: chunks[i]!.bytes.length }));
      const result = await (await device(world)).call({ case: "prepareUpload", value: { chunks: refs } });
      if (result.case !== "prepareUpload") return false;
      for (const plan of result.value.chunks) {
        if (plan.action === ChunkPlan_Action.UPLOAD) world.plans.push({ chunk: chunks.findIndex((c) => c.hex === toHex(plan.hash)), url: plan.target!.url });
      }
      return true;
    }
    case "put": {
      const plan = pick(world.plans, step.plan);
      if (!plan) return false;
      r2.put = step.outcome;
      const receipt = await putChunk(plan.url, chunks[plan.chunk]!.bytes).finally(() => (r2.put = "ok"));
      if (!receipt) return false;
      world.receipts.push(receipt);
      return true;
    }
    case "bundle": {
      const chosen = new Map(step.plans.flatMap((i) => pick(world.plans, i) ?? []).map((plan) => [plan.chunk, plan]));
      if (chosen.size === 0) return false;
      const sent = toBinary(ChunkBundleSchema, create(ChunkBundleSchema, { chunks: [...chosen.values()].map((plan) => ({ target: plan.url, data: chunks[plan.chunk]!.bytes })) }));
      r2.put = step.outcome;
      const response = await exports.default
        .fetch(`${BASE}/v1/spaces/${spaceId}/chunks`, { method: "POST", body: sent, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(sent.length) } })
        .finally(() => (r2.put = "ok"));
      const body = new Uint8Array(await response.arrayBuffer());
      if (!response.ok) return false;
      for (const result of fromBinary(ChunkBundleResultSchema, body).results) if (result.receipt) world.receipts.push(result.receipt);
      return true;
    }
    case "commit": {
      const receipts = step.receipts.flatMap((i) => pick(world.receipts, i) ?? []);
      const used = new Set([...receipts.map((receipt) => chunks.findIndex((c) => c.hex === toHex(receipt.hash))), ...step.extra]);
      if (used.size === 0) return false;
      const parts = await content(...[...used].sort().map((i) => TEXTS[i]!));
      const nodeId = world.files[step.file]!;
      const status = await nodeStatus(world, nodeId);
      const change = status === undefined ? op.create(nodeId, spaceId, `file ${step.file}`, parts.content) : op.update(nodeId, parts.content);
      return (await (await device(world)).commitRaw([change], { receipts })).case === "commit";
    }
    case "trash": {
      const nodeId = world.files[step.file]!;
      if ((await nodeStatus(world, nodeId)) === undefined) return false;
      return (await (await device(world)).commitRaw([op.delete(nodeId)])).case === "commit";
    }
    case "drop":
      world.drops.push(Date.now());
      return runInDurableObject(space(spaceId), (instance, state) => {
        const superseded = state.storage.sql.exec("SELECT * FROM versions WHERE superseded_at_ms IS NOT NULL ORDER BY version_id").toArray() as unknown as VersionRow[];
        const version = pick(superseded, step.version);
        if (!version) return false;
        const dropping = instance as unknown as { dropVersion(version: VersionRow, nowMs: number): void };
        state.storage.transactionSync(() => dropping.dropVersion(version, Date.now()));
        return true;
      });
    case "gc":
      r2.retire = step.retire;
      r2.del = step.del;
      return runInDurableObject(space(spaceId), (instance) => instance.collectGarbage(Date.now()))
        .then(() => true)
        .catch(() => step.retire !== "ok" || step.del !== "ok")
        .finally(() => {
          r2.retire = "ok";
          r2.del = "ok";
        });
    case "land": {
      if (r2.flights.length === 0) return false;
      land(r2.flights.splice(step.flight % r2.flights.length, 1)[0]!);
      return true;
    }
    case "export":
      return runInDurableObject(space(spaceId), (instance) => (instance as unknown as { runExport(): Promise<void> }).runExport())
        .then(() => true)
        .catch(() => false);
    case "crash":
      await space(spaceId)
        .restart()
        .catch(() => undefined);
      return true;
    case "rewind": {
      const nowMs = Date.now();
      const lastDrop = world.drops.at(-1) ?? -Infinity;
      const points = [...new Set(world.points)].filter(
        (pointMs) => pointMs <= nowMs - MINUTE && pointMs >= nowMs - 29 * DAY && (step.from !== "drop" || pointMs < lastDrop),
      );
      const restorePointMs = step.from === "oldest" ? points[step.point % Math.max(points.length, 1)] : pick(points, step.point);
      if (restorePointMs === undefined) return false;
      await act(world, { t: "export" });
      const rewound = await account().rewind({ userId: owner.userId, spaceId, rewindId: uuidv7(), restorePointMs, expectedHistoryId: await historyId(world) });
      return rewound.ok && (await historyId(world)) === rewound.value.historyId;
    }
    case "targets": {
      const hash = fromHex(chunks[step.chunk]!.hex)!;
      const result = await (await device(world)).call({ case: "getDownloadTargets", value: { hashes: [hash] } });
      if (result.case !== "downloadTargets") return false;
      for (const { target } of result.value.targets) world.targets.push({ chunk: step.chunk, url: target!.url, expiresAtMs: Number(target!.expiresAtMs) });
      return true;
    }
    case "download": {
      const target = pick(world.targets, step.target);
      if (!target || Date.now() >= target.expiresAtMs) return false;
      const response = await exports.default.fetch(target.url);
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (response.status !== 200 || toHex(await sha256(bytes)) !== chunks[target.chunk]!.hex) {
        r2.violations.push(`download target ${target.url} answered ${response.status} before it expired`);
      }
      return true;
    }
    case "clock":
      vi.setSystemTime(Date.now() + step.ms);
      return true;
    case "until": {
      const markMs = await runInDurableObject(space(spaceId), (instance, state) => {
        const row = state.storage.sql.exec("SELECT hold_until_ms, zero_since_ms FROM chunks WHERE hash = ?", fromHex(chunks[step.chunk]!.hex)!).toArray()[0];
        if (!row) return undefined;
        if (step.mark === "hold") return Number(row.hold_until_ms);
        const graceDays = (instance as unknown as { settings(): { chunk_grace_days: number } }).settings().chunk_grace_days;
        return row.zero_since_ms === null ? undefined : Number(row.zero_since_ms) + graceDays * DAY;
      });
      if (markMs === undefined || markMs + step.offsetMs <= Date.now()) return false;
      vi.setSystemTime(markMs + step.offsetMs);
      return true;
    }
    case "race": {
      const outerGate: Gate = { reached: false, released: false };
      pauses.set(step.at, outerGate);
      let outerDone = false;
      const outer = act(world, step.outer).finally(() => (outerDone = true));
      await until(() => outerGate.reached || outerDone);
      pauses.clear();
      const outerOutcomes = { put: r2.put, retire: r2.retire, del: r2.del };
      for (const inner of step.inner.slice(0, -1)) await act(world, inner);
      const innerGate: Gate = { reached: false, released: false };
      if (step.innerAt !== undefined) pauses.set(step.innerAt, innerGate);
      let innerDone = false;
      const inner = act(world, step.inner.at(-1)!).finally(() => (innerDone = true));
      await until(() => innerGate.reached || innerDone);
      pauses.clear();
      const innerOutcomes = { put: r2.put, retire: r2.retire, del: r2.del };
      Object.assign(r2, outerOutcomes);
      outerGate.released = true;
      await outer;
      Object.assign(r2, innerOutcomes);
      innerGate.released = true;
      await inner;
      return outerGate.reached && (step.innerAt === undefined || innerGate.reached);
    }
  }
}

function historyId(world: World): Promise<string> {
  return runInDurableObject(space(world.spaceId), (_instance, state) => String(state.storage.sql.exec("SELECT value FROM meta WHERE key = 'history_id'").one().value));
}

async function snapshot(world: World): Promise<void> {
  const atMs = Date.now();
  const keys = await runInDurableObject(space(world.spaceId), (_instance, state) => {
    simulatedPitr.snapshot(state, atMs);
    return state.storage.sql.exec("SELECT hash, incarnation FROM chunks WHERE refcount > 0 AND incarnation IS NOT NULL").toArray().map((row) => chunkKey(world.spaceId, toHex(new Uint8Array(row.hash as ArrayBuffer)), String(row.incarnation)));
  });
  world.points.push(atMs);
  world.history.push({ atMs, keys });
}

interface ChunkRow {
  hex: string;
  state: number;
  refcount: number;
  incarnation: string | null;
  headChecked: boolean;
}

async function chunkRows(world: World): Promise<{ rows: ChunkRow[]; references: Map<string, number> }> {
  return runInDurableObject(space(world.spaceId), (_instance, state) => {
    const sql = state.storage.sql;
    const rows = sql
      .exec("SELECT hash, state, refcount, incarnation, head_checked FROM chunks")
      .toArray()
      .map((row) => ({
        hex: toHex(new Uint8Array(row.hash as ArrayBuffer)),
        state: Number(row.state),
        refcount: Number(row.refcount),
        incarnation: row.incarnation === null ? null : String(row.incarnation),
        headChecked: row.head_checked === 1,
      }));
    const references = new Map<string, number>();
    for (const version of sql.exec("SELECT * FROM versions").toArray() as unknown as VersionRow[]) {
      for (const hex of new Set(versionChunks(sql, version).map((chunk) => toHex(chunk.hash)))) references.set(hex, (references.get(hex) ?? 0) + 1);
    }
    return { rows, references };
  });
}

async function check(world: World): Promise<void> {
  const violations = r2.violations.splice(0);
  const retired = new Set([...(await retiredKeys(world.spaceId)), ...r2.retired]);
  const { rows, references } = await chunkRows(world);
  for (const row of rows) {
    const key = row.incarnation === null ? undefined : chunkKey(world.spaceId, row.hex, row.incarnation);
    if (row.state !== ChunkState.DELETING && key !== undefined && retired.has(key)) violations.push(`chunk ${row.hex} is in state ${row.state} on the retired key ${key}`);
    if (row.state === ChunkState.LIVE || (row.state === ChunkState.PENDING && row.headChecked)) {
      const stored = key === undefined ? undefined : r2.objects.get(key);
      if (!stored) violations.push(`chunk ${row.hex} is in state ${row.state} but R2 has no object at ${key ?? "its missing incarnation"}`);
      else if (toHex(await sha256(stored.bytes)) !== row.hex) violations.push(`R2 holds other bytes at ${key}`);
    }
    if (row.refcount > 0 && row.state !== ChunkState.LIVE) violations.push(`chunk ${row.hex} has refcount ${row.refcount} in state ${row.state}`);
    if (row.refcount !== (references.get(row.hex) ?? 0)) violations.push(`chunk ${row.hex} has refcount ${row.refcount}, but ${references.get(row.hex) ?? 0} versions use it`);
  }
  for (const { atMs, keys } of world.history) {
    if (atMs < Date.now() - 29 * DAY) continue;
    for (const key of keys) if (retired.has(key) || !r2.objects.has(key)) violations.push(`the state at ${atMs} has a used chunk on ${key}, which is ${retired.has(key) ? "retired" : "gone"}`);
  }
  for (const hex of references.keys()) if (!rows.some((row) => row.hex === hex)) violations.push(`a version uses chunk ${hex}, which has no row`);
  if (violations.length > 0) throw new Error(violations.join("\n"));
}

async function quiesce(world: World): Promise<void> {
  for (const flight of r2.flights.splice(0)) land(flight);
  for (let pass = 0; pass < 3; pass++) {
    await act(world, { t: "clock", ms: 31 * DAY });
    for (let runs = 0; ; runs++) {
      if (runs === 100) throw new Error("garbage collection still reports more work after 100 runs");
      const report = await runInDurableObject(space(world.spaceId), (instance) => instance.collectGarbage(Date.now()));
      await check(world);
      if (!report.more) break;
    }
  }
  const { rows } = await chunkRows(world);
  const left = rows.filter((row) => row.state !== ChunkState.LIVE || row.refcount === 0).map((row) => `${row.hex} in state ${row.state}`);
  const keys = new Set(rows.flatMap((row) => (row.incarnation === null ? [] : [chunkKey(world.spaceId, row.hex, row.incarnation)])));
  const garbage = [...r2.objects.keys()].filter((key) => key.startsWith(`spaces/${world.spaceId}/`) && !keys.has(key));
  expect({ left, garbage }).toEqual({ left: [], garbage: [] });
}

const index = fc.oneof({ weight: 3, arbitrary: fc.constant(0) }, fc.nat({ max: 15 }));
const chunkSet = fc.uniqueArray(fc.integer({ min: 0, max: TEXTS.length - 1 }), { minLength: 1, maxLength: 2 });
const outcome = fc.oneof({ weight: 4, arbitrary: fc.constant<Outcome>("ok") }, fc.constantFrom<Outcome>("lostApplied", "lostInflight"));
const prepare = fc.record({ t: fc.constant("prepare" as const), chunks: chunkSet });
const put = fc.record({ t: fc.constant("put" as const), plan: index, outcome });
const commit = fc.record({
  t: fc.constant("commit" as const),
  file: fc.nat({ max: FILES - 1 }),
  receipts: fc.uniqueArray(index, { maxLength: 2 }),
  extra: fc.uniqueArray(fc.nat({ max: TEXTS.length - 1 }), { maxLength: 1 }),
});
const collect = fc.record({ t: fc.constant("gc" as const), retire: fc.constantFrom<Retirement>("ok", "ok", "refused", "lostBefore", "lostAfter"), del: outcome });
const drop = fc.record({ t: fc.constant("drop" as const), version: index });
const clock = fc.record({ t: fc.constant("clock" as const), ms: fc.constantFrom(MINUTE, MINUTE, MINUTE, 30 * MINUTE, 2 * HOUR, DAY, 7 * DAY, 31 * DAY) });
const pausePoint = fc.constantFrom<Pause>("head", "put", "retire", "delete");
const simple: fc.Arbitrary<Simple> = fc.oneof(
  { weight: 3, arbitrary: prepare },
  { weight: 6, arbitrary: put },
  { weight: 1, arbitrary: fc.record({ t: fc.constant("bundle" as const), plans: fc.array(index, { minLength: 1, maxLength: 3 }), outcome }) },
  { weight: 4, arbitrary: commit },
  { weight: 1, arbitrary: fc.record({ t: fc.constant("trash" as const), file: fc.nat({ max: FILES - 1 }) }) },
  { weight: 3, arbitrary: drop },
  { weight: 4, arbitrary: collect },
  { weight: 2, arbitrary: fc.record({ t: fc.constant("land" as const), flight: index }) },
  { weight: 2, arbitrary: fc.constant({ t: "export" as const }) },
  { weight: 1, arbitrary: fc.constant({ t: "crash" as const }) },
  { weight: 3, arbitrary: fc.record({ t: fc.constant("rewind" as const), point: index, from: fc.constantFrom("newest" as const, "oldest" as const, "drop" as const) }) },
  { weight: 2, arbitrary: fc.record({ t: fc.constant("targets" as const), chunk: fc.nat({ max: TEXTS.length - 1 }) }) },
  { weight: 2, arbitrary: fc.record({ t: fc.constant("download" as const), target: index }) },
  { weight: 4, arbitrary: clock },
  {
    weight: 2,
    arbitrary: fc.record({
      t: fc.constant("until" as const),
      mark: fc.constantFrom("grace" as const, "hold" as const),
      chunk: fc.nat({ max: TEXTS.length - 1 }),
      offsetMs: fc.constantFrom(-6 * DAY, -2 * DAY, -MINUTE, MINUTE, 2 * DAY),
    }),
  },
);
const step: fc.Arbitrary<Step> = fc.oneof(
  { weight: 9, arbitrary: simple },
  {
    weight: 1,
    arbitrary: fc.record(
      {
        t: fc.constant("race" as const),
        outer: fc.oneof(commit, collect, put),
        at: pausePoint,
        inner: fc.array(fc.oneof(prepare, put, commit, collect, drop, clock), { minLength: 1, maxLength: 2 }),
        innerAt: pausePoint,
      },
      { requiredKeys: ["t", "outer", "at", "inner"] },
    ),
  },
);
const steps = fc.array(step, { minLength: 1, maxLength: 60, size: "max" });

const minute: Step = { t: "clock", ms: MINUTE };
const day: Step = { t: "clock", ms: DAY };
const week: Step = { t: "clock", ms: 7 * DAY };
const gc: Step = { t: "gc", retire: "ok", del: "ok" };
const uploadBothThenA: Step[] = [{ t: "prepare", chunks: [0, 1] }, { t: "put", plan: 0, outcome: "ok" }, { t: "put", plan: 1, outcome: "ok" }];
const scenarios: Step[][] = [
  [
    minute,
    { t: "prepare", chunks: [0, 1] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "commit", file: 1, receipts: [0], extra: [] },
    minute,
    { t: "put", plan: 1, outcome: "ok" },
    { t: "commit", file: 0, receipts: [0], extra: [] },
    minute,
    day,
    { t: "rewind", point: 3 },
    gc,
    { t: "rewind", point: 0 },
  ],
  [
    minute,
    ...uploadBothThenA,
    { t: "commit", file: 0, receipts: [0], extra: [] },
    { t: "commit", file: 0, receipts: [1], extra: [] },
    { t: "drop", version: 0 },
    week,
    week,
    week,
    { t: "commit", file: 1, receipts: [], extra: [1] },
    week,
    day,
    day,
    day,
    gc,
    { t: "rewind", point: 3 },
  ],
  [
    minute,
    { t: "prepare", chunks: [0] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "commit", file: 0, receipts: [0], extra: [] },
    week,
    day,
    { t: "clock", ms: 2 * DAY },
    { t: "rewind", point: 6 },
    week,
    week,
    week,
    gc,
    { t: "rewind", point: 4 },
  ],
  [
    minute,
    ...uploadBothThenA,
    { t: "commit", file: 0, receipts: [0], extra: [] },
    { t: "commit", file: 0, receipts: [1], extra: [] },
    minute,
    { t: "drop", version: 0 },
    day,
    gc,
    { t: "rewind", point: 1 },
  ],
  [
    minute,
    { t: "prepare", chunks: [0] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "clock", ms: 2 * DAY },
    { t: "race", outer: { t: "commit", file: 0, receipts: [], extra: [0] }, at: "head", inner: [gc], innerAt: "retire" },
  ],
  [
    minute,
    { t: "prepare", chunks: [0] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "clock", ms: 2 * DAY },
    { t: "race", outer: { t: "commit", file: 0, receipts: [], extra: [0] }, at: "head", inner: [gc, { t: "prepare", chunks: [0] }] },
  ],
  [
    minute,
    ...uploadBothThenA,
    { t: "commit", file: 0, receipts: [], extra: [0] },
    { t: "commit", file: 0, receipts: [], extra: [1] },
    minute,
    { t: "drop", version: 0 },
    { t: "clock", ms: 26 * DAY },
    gc,
  ],
  [
    minute,
    ...uploadBothThenA,
    { t: "commit", file: 0, receipts: [], extra: [1] },
    minute,
    { t: "clock", ms: 4 * DAY },
    { t: "commit", file: 1, receipts: [], extra: [0] },
    minute,
    { t: "clock", ms: 24 * DAY },
    { t: "rewind", point: 3 },
    { t: "clock", ms: 3 * DAY },
    gc,
  ],
  [
    minute,
    { t: "prepare", chunks: [0] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "clock", ms: 2 * DAY },
    { t: "gc", retire: "ok", del: "lostInflight" },
    gc,
    { t: "clock", ms: 8 * DAY },
    { t: "prepare", chunks: [1] },
    { t: "put", plan: 0, outcome: "ok" },
    { t: "clock", ms: 2 * DAY },
    gc,
    { t: "rewind", point: 7 },
  ],
];

const cases = Number(inject("batteryCases") || 100);
const settings = { numRuns: cases, examples: (saved as Step[][]).map((sequence): [Step[]] => [sequence]), ...(inject("deterministic") === "true" ? { seed: 0 } : {}) };

async function run(sequence: Step[], directed = false): Promise<World> {
  const world = await newWorld();
  for (const [i, next] of sequence.entries()) {
    vi.setSystemTime(Date.now() + 1000);
    if (!(await act(world, next)) && directed) throw new Error(`directed step ${i}, ${JSON.stringify(next)}, did not take effect`);
    await snapshot(world);
    await check(world);
  }
  return world;
}

it("runs each directed scenario with every step taking effect", async () => {
  for (const scenario of scenarios) await quiesce(await run(scenario, true));
}, 600_000);

it("keeps every confirmed chunk's object until retirement, whatever order R2, the Account DO, crashes and rewinds take", async () => {
  await fc.assert(fc.asyncProperty(steps, async (sequence) => void (await run(sequence))), settings);
}, 600_000);

it("deletes every chunk and object nothing uses once garbage collection runs undisturbed", async () => {
  await fc.assert(fc.asyncProperty(steps, async (sequence) => quiesce(await run(sequence))), { ...settings, numRuns: Math.ceil(cases / 3) });
}, 600_000);
