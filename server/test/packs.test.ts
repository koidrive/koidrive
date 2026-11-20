// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type UploadReceipt, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import type { RpcResponse } from "../src/gen/koidrive/v1/messages_pb";
import { ChunkPlan_Action, Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { encodePack, readPackIndex } from "../src/chunk-pack";
import { sha256 } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import { HOLD_MS, SpaceDO } from "../src/space-do";
import { GC_BATCH, R2_DELETE_BATCH, R2_DELETE_ROUNDS } from "../src/space/limits";
import { ChunkState, PackState } from "../src/space/schema";
import { chunkKey, getMeta, packKey, setMeta } from "../src/space/store";
import { content, op, packFixture, spaceKeyOf, targetIncarnation, TestClient } from "./helpers";
import { account, BASE, createSpace, getOwner, put } from "./session";

const DAY = 24 * 3_600_000;

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

type Planned = { bytes: Uint8Array; hash: Uint8Array; incarnation: string; url: string };

async function packedSpace() {
  const creds = await getOwner();
  const spaceId = await createSpace(creds);
  const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
  const session = { deviceId: creds.deviceId, userId: creds.userId, deviceName: "Laptop A", kind: "device" as const, scope: null, role: "owner" as const, maxFormat: 1, origin: BASE };
  const device = new TestClient(stub, spaceId, session);
  const prepare = async (...texts: string[]) => {
    const made = await content(...texts);
    const result = await device.call({ case: "prepareUpload", value: { chunks: made.content.chunks } });
    if (result.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const chunks: Planned[] = made.bytes.map((bytes, i) => {
      const url = result.value.chunks[i]!.target?.url ?? "";
      return { bytes, hash: made.content.chunks[i]!.hash, incarnation: url && targetIncarnation(url), url };
    });
    return { content: made.content, chunks, actions: result.value.chunks.map((plan) => plan.action) };
  };
  const commit = (file: { content: Awaited<ReturnType<typeof content>>["content"] }, receipts: UploadReceipt[], by = device) =>
    by.commitRaw([op.create(uuidv7(), spaceId, `file ${uuidv7()}`, file.content)], { receipts });
  const otherDevice = () => new TestClient(stub, spaceId, { ...session, deviceId: uuidv7(), deviceName: "Laptop B" });
  return { spaceId, stub, device, spaceKey: await spaceKeyOf(stub), prepare, commit, otherDevice };
}

type Space = Awaited<ReturnType<typeof packedSpace>>;

async function storedPack(space: Space, chunks: Planned[], init: { packId?: string; expiresAtMs?: number } = {}) {
  const pack = await packFixture(space.spaceId, space.spaceKey, chunks, init);
  await env.BUCKET.put(pack.key, pack.bytes);
  return pack;
}

async function putSingle(chunk: Planned): Promise<UploadReceipt> {
  const response = await put({ url: chunk.url, method: "PUT" } as never, chunk.bytes);
  expect(response.status).toBe(200);
  return fromBinary(UploadReceiptSchema, new Uint8Array(await response.arrayBuffer()));
}

function outcome(result: RpcResponse["result"]): string | Error_Code | undefined {
  return result.case === "error" ? result.value.code : result.case;
}

function rowsOf(space: Space, chunks: Planned[]) {
  return runInDurableObject(space.stub, (_instance, state) =>
    chunks.map(({ hash }) => {
      const row = state.storage.sql.exec("SELECT state, incarnation, head_checked, pack_id, pack_offset, hold_until_ms FROM chunks WHERE hash = ?", hash).one();
      return { state: row.state, incarnation: row.incarnation, headChecked: row.head_checked, packId: row.pack_id, packOffset: row.pack_offset, holdUntilMs: row.hold_until_ms };
    }),
  );
}

function packsOf(space: Space) {
  return runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("SELECT * FROM packs ORDER BY pack_id").toArray());
}

async function until(done: () => boolean): Promise<void> {
  while (!done()) await new Promise((resolve) => setTimeout(resolve, 5));
}

function holdHead(key: string) {
  const head = env.BUCKET.head.bind(env.BUCKET);
  const gate = { reached: false, released: false };
  vi.spyOn(env.BUCKET, "head").mockImplementation((async (asked: string) => {
    if (asked === key) {
      gate.reached = true;
      await until(() => gate.released);
    }
    return head(asked);
  }) as never);
  return gate;
}

async function mixedFile() {
  const space = await packedSpace();
  const file = await space.prepare("packed-one", "packed-two", "single-three");
  const [a, b, c] = file.chunks as [Planned, Planned, Planned];
  const pack = await storedPack(space, [a, b]);
  expect(outcome(await space.commit(file, [...pack.receipts, await putSingle(c)]))).toBe("commit");
  return { space, a, b, c, pack };
}

describe("chunk packs: confirming locations (RFC 0032 section 3)", () => {
  it("records each v2 receipt's location and its pack's counts once, lands a commit that mixes packed and single chunks, and dedups both", async () => {
    const { space, a, b, c, pack } = await mixedFile();
    const live = { state: ChunkState.LIVE, headChecked: 0 };
    expect(await rowsOf(space, [a, b, c])).toMatchObject([
      { ...live, packId: pack.packId, packOffset: 0 },
      { ...live, packId: pack.packId, packOffset: a.bytes.length },
      { ...live, packId: null, packOffset: null },
    ]);
    expect(await packsOf(space)).toEqual([{ pack_id: pack.packId, size: pack.bytes.length, state: 2, live_count: 2, live_bytes: a.bytes.length + b.bytes.length, hold_until_ms: 0 }]);
    expect((await space.prepare("packed-one", "packed-two", "single-three")).actions).toEqual([ChunkPlan_Action.PRESENT, ChunkPlan_Action.PRESENT, ChunkPlan_Action.PRESENT]);
  });

  it("refuses a v2 receipt whose pack id, offset, pack size or incarnation differs from what was signed", async () => {
    const space = await packedSpace();
    const file = await space.prepare("tampered");
    const [chunk] = file.chunks as [Planned];
    const pack = await storedPack(space, [chunk]);
    const signed = pack.receipts[0]!;
    const [version, expires, packId, offset, packSize, mac] = signed.token.split(".") as string[];
    const tokens = [
      [version, expires, uuidv7(), offset, packSize, mac].join("."),
      [version, expires, packId, String(Number(offset) + 1), packSize, mac].join("."),
      [version, expires, packId, offset, String(Number(packSize) + 1), mac].join("."),
      (await packFixture(space.spaceId, space.spaceKey, [{ ...chunk, incarnation: "9.9" }], { packId: pack.packId })).receipts[0]!.token,
    ];
    for (const token of tokens) expect(outcome(await space.commit(file, [{ ...signed, token }]))).toBe(Error_Code.MISSING_CHUNKS);
    expect(await packsOf(space)).toEqual([]);
    expect(outcome(await space.commit(file, [signed]))).toBe("commit");
  });

  it("keeps the first location recorded when two packs hold one chunk", async () => {
    const space = await packedSpace();
    const file = await space.prepare("in-two-packs");
    const [chunk] = file.chunks as [Planned];
    const refused = await space.prepare("in-two-packs", "never-sent");
    const late = await storedPack(space, [chunk], { expiresAtMs: Date.now() - 1 });
    const early = await storedPack(space, [chunk]);
    const held = holdHead(late.key);
    const waiting = space.commit(file, late.receipts);
    await until(() => held.reached);
    expect(outcome(await space.commit(refused, early.receipts, space.otherDevice()))).toBe(Error_Code.MISSING_CHUNKS);
    held.released = true;
    expect(outcome(await waiting)).toBe("commit");
    expect(await rowsOf(space, [chunk])).toMatchObject([{ state: ChunkState.LIVE, packId: early.packId }]);
    expect((await packsOf(space)).map((row) => [row.pack_id, row.live_count])).toEqual([[early.packId, 1]]);
  });

  it("gives no location to a row that a HEAD check confirmed as a single object while the receipt was being checked", async () => {
    const space = await packedSpace();
    const file = await space.prepare("checked-single", "held-head");
    const [chunk, other] = file.chunks as [Planned, Planned];
    const refused = await space.prepare("checked-single", "never-sent");
    await putSingle(chunk);
    await putSingle(other);
    const pack = await storedPack(space, [chunk]);
    const held = holdHead(`spaces/${space.spaceId}/chunks/${toHex(other.hash).slice(0, 2)}/${toHex(other.hash)}/${other.incarnation}`);
    const waiting = space.commit(file, pack.receipts);
    await until(() => held.reached);
    expect(outcome(await space.commit(refused, [], space.otherDevice()))).toBe(Error_Code.MISSING_CHUNKS);
    held.released = true;
    expect(outcome(await waiting)).toBe("commit");
    expect(await rowsOf(space, [chunk])).toMatchObject([{ state: ChunkState.LIVE, headChecked: 1, packId: null }]);
    expect(await packsOf(space)).toEqual([]);
  });

  it("keeps a refused commit's v1 and v2 confirmations apart, then answers PRESENT for both with a fresh hold and lands without receipts", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const file = await space.prepare("single-part", "packed-part", "missing-part");
    const [single, packed] = file.chunks as [Planned, Planned];
    const pack = await storedPack(space, [packed]);
    expect(outcome(await space.commit(file, [await putSingle(single), ...pack.receipts]))).toBe(Error_Code.MISSING_CHUNKS);
    expect(await rowsOf(space, [single, packed])).toMatchObject([
      { state: ChunkState.PENDING, headChecked: 1, packId: null },
      { state: ChunkState.PENDING, headChecked: 0, packId: pack.packId },
    ]);

    vi.setSystemTime(Date.now() + 3 * 3_600_000);
    const resend = await space.prepare("single-part", "packed-part");
    expect(resend.actions).toEqual([ChunkPlan_Action.PRESENT, ChunkPlan_Action.PRESENT]);
    expect((await rowsOf(space, [single, packed])).map((row) => row.holdUntilMs)).toEqual([Date.now() + HOLD_MS, Date.now() + HOLD_MS]);
    expect(outcome(await space.commit(resend, []))).toBe("commit");
  });

  it("confirms 64 chunks from two packs of expired receipts with one HEAD per pack", async () => {
    const space = await packedSpace();
    const file = await space.prepare(...Array.from({ length: 64 }, (_, i) => `expired receipt ${i}`));
    const receipts: UploadReceipt[] = [];
    for (const part of [file.chunks.slice(0, 32), file.chunks.slice(32)]) receipts.push(...(await storedPack(space, part, { expiresAtMs: Date.now() - 1 })).receipts);
    const head = vi.spyOn(env.BUCKET, "head");
    expect(outcome(await space.commit(file, receipts))).toBe("commit");
    expect(head.mock.calls.map(([key]) => key.split("/")[2])).toEqual(["packs", "packs"]);
  });

  it("refuses a v2 receipt for a pack older than six days or a pack that is not live", async () => {
    const space = await packedSpace();
    const aged = await space.prepare("aged-pack");
    const drained = await space.prepare("drained-pack");
    const old = await storedPack(space, aged.chunks, { packId: uuidv7(Date.now() - 6 * DAY - 1000) });
    const leaving = await storedPack(space, drained.chunks);
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, ?, 3, 0, 0, 0)", leaving.packId, leaving.bytes.length);
    });
    expect(outcome(await space.commit(aged, old.receipts))).toBe(Error_Code.MISSING_CHUNKS);
    expect(outcome(await space.commit(drained, leaving.receipts))).toBe(Error_Code.MISSING_CHUNKS);
    expect(await rowsOf(space, [...aged.chunks, ...drained.chunks])).toMatchObject([{ packId: null }, { packId: null }]);
  });
});

describe("chunk packs: reading (RFC 0032 section 5)", () => {
  it("serves a packed chunk's exact bytes and refuses a ticket of the other target kind or a changed range", async () => {
    const { space, a, b, c, pack } = await mixedFile();
    const result = await space.device.call({ case: "getDownloadTargets", value: { hashes: [a.hash, b.hash, c.hash] } });
    if (result.case !== "downloadTargets") throw new Error("getDownloadTargets failed");
    const [first, second, single] = result.value.targets.map(({ target }) => new URL(target!.url)) as [URL, URL, URL];
    expect([...second.searchParams.keys()]).toEqual(["pack", "off", "len", "ticket"]);
    expect([second.searchParams.get("pack"), second.searchParams.get("off"), second.searchParams.get("len")]).toEqual([pack.packId, String(a.bytes.length), String(b.bytes.length)]);
    for (const [url, chunk] of [[first, a], [second, b], [single, c]] as const) {
      const response = await exports.default.fetch(url);
      expect(response.status).toBe(200);
      expect(response.headers.get("Content-Length")).toBe(String(chunk.bytes.length));
      expect(new Uint8Array(await response.arrayBuffer())).toEqual(chunk.bytes);
    }

    const changed = (url: URL, params: Record<string, string>) => {
      const next = new URL(url);
      for (const [name, value] of Object.entries(params)) next.searchParams.set(name, value);
      return next;
    };
    const refused = [
      changed(second, { ticket: single.searchParams.get("ticket")! }),
      changed(second, { off: "0" }),
      changed(second, { len: String(b.bytes.length + 1) }),
      changed(single, { ticket: second.searchParams.get("ticket")! }),
    ];
    for (const url of refused) expect((await exports.default.fetch(url)).status).toBe(403);

    const get = env.BUCKET.get.bind(env.BUCKET);
    vi.spyOn(env.BUCKET, "get").mockImplementation((async (key: string, options?: R2GetOptions) => {
      const found = await get(key, options);
      return found && { size: found.size, body: new Blob([b.bytes.slice(1)]).stream() };
    }) as never);
    expect((await exports.default.fetch(second)).status).toBe(503);
  });
});

describe("chunk packs: packed rows across GC and rewinds", () => {
  it("keeps a packed row's location and incarnation when a rewind retires that incarnation", async () => {
    const space = await packedSpace();
    const file = await space.prepare("packed-rewound", "never-sent");
    const [chunk] = file.chunks as [Planned];
    const pack = await storedPack(space, [chunk]);
    expect(outcome(await space.commit(file, pack.receipts))).toBe(Error_Code.MISSING_CHUNKS);
    const nowMs = Date.now();
    const retired = [{ hashHex: toHex(chunk.hash), incarnation: chunk.incarnation }];
    await space.stub.rewindComplete({ rewindId: uuidv7(), attempt: 1, historyId: uuidv7(), rewindNumber: 1, restorePointMs: nowMs - 60_000, completedAtMs: nowMs, revokedDevices: [], retired, retiredPacks: [] });
    expect(await rowsOf(space, [chunk])).toMatchObject([{ state: ChunkState.PENDING, incarnation: chunk.incarnation, packId: pack.packId }]);
  });

  it("counts each pack as one stored object at its full size in the usage note", async () => {
    const space = await packedSpace();
    const file = await space.prepare(...Array.from({ length: 35 }, (_, i) => `usage note ${i}`));
    const pack = await storedPack(space, file.chunks.slice(0, 32));
    const singles = file.chunks.slice(32);
    const receipts = [...pack.receipts];
    for (const chunk of singles) receipts.push(await putSingle(chunk));
    expect(outcome(await space.commit(file, receipts))).toBe("commit");
    const noted = await runInDurableObject(space.stub, (instance, state) => {
      state.storage.sql.exec("DELETE FROM meta WHERE key = 'r2_stored'");
      const internals = instance as unknown as { storedHour?: number; noteStored(): void };
      internals.storedHour = undefined;
      internals.noteStored();
      return JSON.parse(String(getMeta(state.storage.sql, "r2_stored"))) as { objects: number; bytes: number };
    });
    expect({ objects: noted.objects, bytes: noted.bytes }).toEqual({ objects: 4, bytes: pack.bytes.length + singles.reduce((sum, chunk) => sum + chunk.bytes.length, 0) });
  });
});

function gc(space: Space, atMs: number) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
}

function retirements(spaceId: string) {
  return runInDurableObject(account(), (_instance, state) => ({
    chunks: state.storage.sql.exec("SELECT hash_hex FROM chunk_retirements WHERE space_id = ?", spaceId).toArray().map((row) => String(row.hash_hex)),
    packs: state.storage.sql.exec("SELECT pack_id FROM pack_retirements WHERE space_id = ?", spaceId).toArray().map((row) => String(row.pack_id)),
  }));
}

function chunkRow(space: Space, chunk: Planned) {
  return runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("SELECT state, refcount, incarnation, pack_id FROM chunks WHERE hash = ?", chunk.hash).toArray()[0]);
}

async function refusedPack(space: Space, texts: string[]) {
  const file = await space.prepare(...texts, "never-sent");
  const packed = file.chunks.slice(0, texts.length);
  const pack = await storedPack(space, packed);
  expect(outcome(await space.commit(file, pack.receipts))).toBe(Error_Code.MISSING_CHUNKS);
  return { packed, pack };
}

function failRetirePacks(answer: () => Promise<unknown>) {
  const prototype = SpaceDO.prototype as unknown as { account(...args: unknown[]): object };
  const directory = prototype.account;
  vi.spyOn(prototype, "account").mockImplementation(function (this: unknown, ...args: unknown[]) {
    const target = directory.apply(this, args);
    return new Proxy(target, {
      get(object, property) {
        if (property === "retirePacks") return answer;
        const value = Reflect.get(object, property);
        return typeof value === "function" ? value.bind(object) : value;
      },
    });
  });
}

function completeRewind(space: Space, retiredPacks: string[] | undefined, completedAtMs = Date.now()) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => instance.rewindComplete({
    rewindId: uuidv7(),
    attempt: 1,
    historyId: uuidv7(),
    rewindNumber: 1,
    restorePointMs: completedAtMs - 60_000,
    completedAtMs,
    revokedDevices: [],
    retired: [],
    ...(retiredPacks === undefined ? {} : { retiredPacks }),
  } as never));
}

describe("chunk packs: pack GC (RFC 0032 section 6)", () => {
  it("deletes an unreferenced packed row, then retires and deletes its pack once nothing points at it", async () => {
    const space = await packedSpace();
    const { packed, pack } = await refusedPack(space, ["gc-free-one"]);
    const report = await gc(space, Date.now() + 40 * DAY);
    expect({ deletedPacks: report.deletedPacks, row: await chunkRow(space, packed[0]!), packs: await packsOf(space) }).toEqual({ deletedPacks: 1, row: undefined, packs: [] });
    expect(await env.BUCKET.head(pack.key)).toBeNull();
    const retired = await retirements(space.spaceId);
    expect({ packedChunkRetired: retired.chunks.includes(toHex(packed[0]!.hash)), packs: retired.packs }).toEqual({ packedChunkRetired: false, packs: [pack.packId] });
  });

  it("lowers a pack's counts in the same pass that deletes one of its rows", async () => {
    const space = await packedSpace();
    const { packed, pack } = await refusedPack(space, ["gc-counts-kept", "gc-counts-dropped"]);
    const [kept, dropped] = packed as [Planned, Planned];
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, live_count: 2 }]);
    const keep = await space.prepare("gc-counts-kept");
    expect(outcome(await space.commit(keep, []))).toBe("commit");
    await gc(space, Date.now() + 40 * DAY);
    expect(await chunkRow(space, dropped)).toBeUndefined();
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.LIVE, live_count: 1, live_bytes: kept.bytes.length }]);
  });

  it("keeps a pack with no rows while it is 7 days old or younger, or while its hold lasts", async () => {
    const space = await packedSpace();
    const packMs = Date.now();
    const { pack } = await refusedPack(space, ["gc-young"]);
    await gc(space, packMs + 7 * DAY - 1);
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.LIVE, live_count: 0 }]);
    await runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("UPDATE packs SET hold_until_ms = ?", packMs + 20 * DAY));
    await gc(space, packMs + 20 * DAY);
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.LIVE }]);
    expect(await env.BUCKET.head(pack.key)).not.toBeNull();
    expect((await gc(space, packMs + 20 * DAY + 1)).deletedPacks).toBe(1);
  });

  it("frees at most one batch of packs per pass and reports more work", async () => {
    const space = await packedSpace();
    const packMs = Date.now() - 8 * DAY;
    await runInDurableObject(space.stub, (_instance, state) => {
      for (let i = 0; i <= GC_BATCH; i++) {
        state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, 1, ?, 0, 0, 0)", uuidv7(packMs + i), PackState.LIVE);
      }
    });
    expect(await gc(space, Date.now())).toMatchObject({ deletedPacks: GC_BATCH, more: true });
    expect(await gc(space, Date.now())).toMatchObject({ deletedPacks: 1 });
  });

  it("deletes at most four batches of a deleting backlog per pass and reports more work", async () => {
    const space = await packedSpace();
    const backlog = R2_DELETE_ROUNDS * R2_DELETE_BATCH + 1;
    await runInDurableObject(space.stub, (_instance, state) => {
      for (let i = 0; i < backlog; i++) {
        state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, 1, ?, 0, 0, 0)", uuidv7(), PackState.DELETING);
      }
    });
    failRetirePacks(async () => ({ ok: false, code: Error_Code.RETRY_LATER, message: "refused" }));
    await expect(gc(space, Date.now())).rejects.toThrow();
    vi.restoreAllMocks();
    expect(await gc(space, Date.now())).toMatchObject({ deletedPacks: backlog - 1, more: true });
    expect(await gc(space, Date.now())).toMatchObject({ deletedPacks: 1 });
  });

  it.each([
    ["refuses", async () => ({ ok: false, code: Error_Code.RETRY_LATER, message: "refused" })],
    ["does not answer", async () => Promise.reject(new Error("lost"))],
    ["runs code without retirePacks", async () => Promise.reject(new TypeError("retirePacks is not a function"))],
  ])("issues no pack delete while the Account DO %s, and finishes on the next pass", async (_, answer) => {
    const space = await packedSpace();
    const { pack } = await refusedPack(space, ["gc-retire"]);
    const deletes = vi.spyOn(env.BUCKET, "delete");
    failRetirePacks(answer);
    await expect(gc(space, Date.now() + 40 * DAY)).rejects.toThrow();
    expect(deletes.mock.calls.flat(2)).not.toContain(pack.key);
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.DELETING }]);
    vi.restoreAllMocks();
    expect((await gc(space, Date.now() + 40 * DAY)).deletedPacks).toBe(1);
    expect(await env.BUCKET.head(pack.key)).toBeNull();
  });

  it("sweeps a pack key with no packs row only after the grace period and with no rewind in it", async () => {
    const space = await packedSpace();
    const packId = uuidv7();
    const key = packKey(space.spaceId, packId);
    await env.BUCKET.put(key, "never confirmed");
    const uploadedMs = Date.now();
    expect((await gc(space, uploadedMs + 29 * DAY)).sweptPacks).toBe(0);
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("DELETE FROM meta WHERE key = 'pack_sweep_due_ms'");
      setMeta(state.storage.sql, "rewind_completed_ms", uploadedMs + 2 * DAY);
    });
    expect((await gc(space, uploadedMs + 31 * DAY)).sweptPacks).toBe(0);
    await runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM meta WHERE key = 'pack_sweep_due_ms'"));
    expect(await gc(space, uploadedMs + 33 * DAY)).toMatchObject({ sweptPacks: 1, deletedPacks: 1 });
    expect(await env.BUCKET.head(key)).toBeNull();
    expect((await retirements(space.spaceId)).packs).toEqual([packId]);
  });

  it("sweeps a single-object copy of a chunk whose row took a pack location", async () => {
    const space = await packedSpace();
    const file = await space.prepare("gc-both-copies");
    const [chunk] = file.chunks as [Planned];
    const pack = await storedPack(space, [chunk]);
    await putSingle(chunk);
    expect(outcome(await space.commit(file, pack.receipts))).toBe("commit");
    const single = chunkKey(space.spaceId, toHex(chunk.hash), chunk.incarnation);
    expect(await gc(space, Date.now() + 31 * DAY)).toMatchObject({ sweptChunks: 1, deletedPacks: 0 });
    expect(await env.BUCKET.head(single)).toBeNull();
    expect(await env.BUCKET.head(pack.key)).not.toBeNull();
  });

  it("frees a writing pack that no running compaction owns, at any age, and keeps an owned one", async () => {
    const space = await packedSpace();
    const [owned, unowned] = [uuidv7(), uuidv7()];
    await runInDurableObject(space.stub, (instance, state) => {
      for (const packId of [owned, unowned]) state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, 1, ?, 0, 0, 0)", packId, PackState.WRITING);
      (instance as unknown as { compacting: Set<string> }).compacting.add(owned);
    });
    expect((await gc(space, Date.now())).deletedPacks).toBe(1);
    expect(await packsOf(space)).toMatchObject([{ pack_id: owned, state: PackState.WRITING }]);
  });
});

describe("chunk packs: compaction (RFC 0032 section 7)", () => {
  async function sparseSource(space: Space, live: string[], dead: string[]) {
    vi.setSystemTime(Date.now() + 1000);
    const file = await space.prepare(...live);
    const unused = await space.prepare(...dead);
    const pack = await storedPack(space, [...file.chunks, ...unused.chunks]);
    expect(outcome(await space.commit(file, pack.receipts.slice(0, file.chunks.length)))).toBe("commit");
    return { pack, chunks: file.chunks, unused: unused.chunks };
  }

  function holdGet(key: string) {
    const get = env.BUCKET.get.bind(env.BUCKET);
    const gate = { reached: false, released: false };
    vi.spyOn(env.BUCKET, "get").mockImplementation((async (asked: string, options?: R2GetOptions) => {
      if (asked === key && !gate.reached) {
        gate.reached = true;
        await until(() => gate.released);
      }
      return get(asked, options);
    }) as never);
    return gate;
  }

  async function download(space: Space, chunk: Planned): Promise<Uint8Array> {
    const result = await space.device.call({ case: "getDownloadTargets", value: { hashes: [chunk.hash] } });
    if (result.case !== "downloadTargets") throw new Error("getDownloadTargets failed");
    const response = await exports.default.fetch(result.value.targets[0]!.target!.url);
    return new Uint8Array(await response.arrayBuffer());
  }

  const dead = (name: string) => `${name}-`.repeat(40);

  it("rewrites a pack under half live into a smaller output and never picks a fully live pack of 1-byte chunks", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const full = await sparseSource(space, ["a", "b", "c"], []);
    const sparse = await sparseSource(space, ["threshold-kept"], [dead("threshold-dead")]);
    vi.setSystemTime(Date.now() + 8 * DAY);
    const nowMs = Date.now();
    await gc(space, nowMs);
    const packs = await packsOf(space);
    const output = packs.find((pack) => pack.pack_id !== full.pack.packId && pack.pack_id !== sparse.pack.packId)!;
    expect(packs.find((pack) => pack.pack_id === full.pack.packId)).toMatchObject({ state: PackState.LIVE, live_count: 3 });
    expect(packs.find((pack) => pack.pack_id === sparse.pack.packId)).toMatchObject({ state: PackState.DRAINED, live_count: 0, live_bytes: 0, hold_until_ms: nowMs + 30 * DAY });
    expect(output).toMatchObject({ state: PackState.LIVE, live_count: 1, live_bytes: sparse.chunks[0]!.bytes.length });
    expect(Number(output.size)).toBeLessThan(sparse.pack.bytes.length);
    expect(await rowsOf(space, sparse.chunks)).toMatchObject([{ packId: output.pack_id, packOffset: 0 }]);
    expect(await download(space, sparse.chunks[0]!)).toEqual(sparse.chunks[0]!.bytes);
  });

  it("moves nothing when its output was freed while the pass waited on R2", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const sparse = await sparseSource(space, ["freed-kept"], [dead("freed-dead")]);
    vi.setSystemTime(Date.now() + 8 * DAY);
    const gate = holdGet(sparse.pack.key);
    const pass = gc(space, Date.now());
    await until(() => gate.reached);
    await runInDurableObject(space.stub, (instance) => {
      const owners = (instance as unknown as { compacting: Set<string> }).compacting;
      owners.clear();
      owners.add("another pass");
    });
    await gc(space, Date.now());
    gate.released = true;
    await pass;
    expect(await rowsOf(space, sparse.chunks)).toMatchObject([{ packId: sparse.pack.packId }]);
    expect(await packsOf(space)).toMatchObject([{ pack_id: sparse.pack.packId, state: PackState.LIVE, live_count: 1 }]);
  });

  it("frees its output and leaves the source alone when a row lands on the source after the plan", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const sparse = await sparseSource(space, ["landed-kept"], [dead("landed-late")]);
    vi.setSystemTime(Date.now() + 8 * DAY);
    const gate = holdGet(sparse.pack.key);
    const pass = gc(space, Date.now());
    await until(() => gate.reached);
    const [late] = sparse.unused as [Planned];
    const offset = sparse.chunks[0]!.bytes.length;
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec(
        "INSERT INTO chunks (hash, size, state, refcount, hold_until_ms, incarnation, head_checked, pack_id, pack_offset) VALUES (?, ?, ?, 0, ?, ?, 0, ?, ?) ON CONFLICT (hash) DO UPDATE SET state = excluded.state, hold_until_ms = excluded.hold_until_ms, incarnation = excluded.incarnation, pack_id = excluded.pack_id, pack_offset = excluded.pack_offset",
        late.hash,
        late.bytes.length,
        ChunkState.PENDING,
        Date.now() + DAY,
        late.incarnation,
        sparse.pack.packId,
        offset,
      );
      state.storage.sql.exec("UPDATE packs SET live_count = live_count + 1, live_bytes = live_bytes + ? WHERE pack_id = ?", late.bytes.length, sparse.pack.packId);
    });
    gate.released = true;
    await pass;
    expect(await rowsOf(space, [...sparse.chunks, late])).toMatchObject([{ packId: sparse.pack.packId }, { packId: sparse.pack.packId, packOffset: offset }]);
    const packs = await packsOf(space);
    expect(packs.find((pack) => pack.pack_id === sparse.pack.packId)).toMatchObject({ state: PackState.LIVE, live_count: 2 });
    expect(packs.filter((pack) => pack.pack_id !== sparse.pack.packId)).toMatchObject([{ state: PackState.DELETING, live_count: 0 }]);
  });

  it("leaves out and logs a source that is missing or whose planned bytes do not match their hash", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const corrupt = await sparseSource(space, ["corrupt-kept"], [dead("corrupt-dead")]);
    const missing = await sparseSource(space, ["missing-kept"], [dead("missing-dead")]);
    const intact = await sparseSource(space, ["intact-kept"], [dead("intact-dead")]);
    const flipped = corrupt.pack.bytes.slice();
    flipped[0]! ^= 1;
    await env.BUCKET.put(corrupt.pack.key, flipped);
    await env.BUCKET.delete(missing.pack.key);
    const errors = vi.spyOn(console, "error").mockImplementation(() => undefined);
    vi.setSystemTime(Date.now() + 8 * DAY);
    await gc(space, Date.now());
    const packs = await packsOf(space);
    const state = (packId: string) => packs.find((pack) => pack.pack_id === packId)?.state;
    expect([state(corrupt.pack.packId), state(missing.pack.packId), state(intact.pack.packId)]).toEqual([PackState.LIVE, PackState.LIVE, PackState.DRAINED]);
    const output = packs.find((pack) => ![corrupt, missing, intact].some((source) => source.pack.packId === pack.pack_id))!;
    const index = await readPackIndex(env.BUCKET as never, packKey(space.spaceId, String(output.pack_id)));
    expect(index.map((entry) => toHex(entry.hash))).toEqual([toHex(intact.chunks[0]!.hash)]);
    expect(errors.mock.calls.map((call) => call.slice(1))).toEqual(
      expect.arrayContaining([
        [space.spaceId, corrupt.pack.packId, toHex(corrupt.chunks[0]!.hash)],
        [space.spaceId, missing.pack.packId],
      ]),
    );
  });

  it.each([
    ["throws", async () => Promise.reject(new Error("R2 did not answer"))],
    ["is refused by its precondition", async () => null],
  ])("frees the output and keeps every row on its source when the output write %s", async (_, answer) => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const sparse = await sparseSource(space, ["failed-kept"], [dead("failed-dead")]);
    const put = env.BUCKET.put.bind(env.BUCKET);
    vi.spyOn(env.BUCKET, "put").mockImplementation((async (key: string, value: never, options?: R2PutOptions) => (key.includes("/packs/") ? answer() : put(key, value, options))) as never);
    vi.setSystemTime(Date.now() + 8 * DAY);
    await gc(space, Date.now()).catch(() => undefined);
    expect(await rowsOf(space, sparse.chunks)).toMatchObject([{ packId: sparse.pack.packId }]);
    const packs = await packsOf(space);
    expect(packs.find((pack) => pack.pack_id === sparse.pack.packId)).toMatchObject({ state: PackState.LIVE, live_count: 1 });
    expect(packs.filter((pack) => pack.pack_id !== sparse.pack.packId)).toMatchObject([{ state: PackState.DELETING }]);
  });

  it("leaves an output write that lands after its output was freed to the pack sweep", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    await sparseSource(space, ["late-kept"], [dead("late-dead")]);
    const put = env.BUCKET.put.bind(env.BUCKET);
    let held: { key: string; land: () => Promise<unknown> } | undefined;
    vi.spyOn(env.BUCKET, "put").mockImplementation((async (key: string, value: never, options?: R2PutOptions) => {
      if (held || !key.includes("/packs/")) return put(key, value, options);
      held = { key, land: () => put(key, value, options) };
      throw new Error("R2 did not answer");
    }) as never);
    vi.setSystemTime(Date.now() + 8 * DAY);
    await expect(gc(space, Date.now())).rejects.toThrow("R2 did not answer");
    await gc(space, Date.now());
    await held!.land();
    expect((await packsOf(space)).map((pack) => pack.pack_id)).not.toContain(held!.key.split("/").at(-1));
    vi.setSystemTime(Date.now() + 31 * DAY);
    await runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM meta WHERE key = 'pack_sweep_due_ms'"));
    await gc(space, Date.now());
    expect(await env.BUCKET.head(held!.key)).toBeNull();
  });

  it("reads at most 16 sources and writes at most 8 outputs per pass, and reports more work when it used 16", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const sourceIds: string[] = [];
    for (let source = 0; source < 17; source++) {
      const chunks = await Promise.all(
        [0, 1, 2, 3].map(async (i) => {
          const data = new Uint8Array(i < 2 ? (1 << 20) - 64 : 1 << 20);
          new DataView(data.buffer).setUint32(0, source * 4 + i);
          return { hash: await sha256(data), data };
        }),
      );
      const packId = uuidv7(Date.now() - 8 * DAY + source);
      const bytes = encodePack(chunks);
      await env.BUCKET.put(packKey(space.spaceId, packId), bytes);
      await runInDurableObject(space.stub, (_instance, state) => {
        const live = chunks.slice(0, 2);
        state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, ?, ?, 2, ?, 0)", packId, bytes.length, PackState.LIVE, live.reduce((sum, chunk) => sum + chunk.data.length, 0));
        let offset = 0;
        for (const { hash, data } of live) {
          state.storage.sql.exec(
            "INSERT INTO chunks (hash, size, state, refcount, hold_until_ms, incarnation, pack_id, pack_offset) VALUES (?, ?, ?, 1, 0, '1.1', ?, ?)",
            hash,
            data.length,
            ChunkState.LIVE,
            packId,
            offset,
          );
          offset += data.length;
        }
      });
      sourceIds.push(packId);
    }
    const gets = vi.spyOn(env.BUCKET, "get");
    const puts = vi.spyOn(env.BUCKET, "put");
    const packCalls = (spy: { mock: { calls: unknown[][] } }) => spy.mock.calls.filter(([key]) => String(key).includes("/packs/")).length;
    const first = await gc(space, Date.now());
    expect({ gets: packCalls(gets), puts: packCalls(puts), more: first.more }).toEqual({ gets: 16, puts: 8, more: true });
    const second = await gc(space, Date.now());
    expect({ gets: packCalls(gets), puts: packCalls(puts), more: second.more }).toEqual({ gets: 17, puts: 9, more: false });
    const packs = await packsOf(space);
    expect(packs.filter((pack) => sourceIds.includes(String(pack.pack_id))).every((pack) => pack.state === PackState.DRAINED)).toBe(true);
    expect(packs.filter((pack) => !sourceIds.includes(String(pack.pack_id))).map((pack) => `${pack.state}:${pack.live_count}`).sort()).toEqual([`${PackState.LIVE}:2`, ...Array(8).fill(`${PackState.LIVE}:4`)]);
  }, 120_000);

  it("puts at most 4,096 chunks in one output", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const space = await packedSpace();
    const sourceIds: string[] = [];
    for (let source = 0; source < 2; source++) {
      const live = await Promise.all(
        Array.from({ length: 2049 }, async (_, i) => {
          const data = Uint8Array.of(source, i >> 8, i & 255);
          return { hash: await sha256(data), data };
        }),
      );
      const unused = new Uint8Array(1 << 18).fill(source + 1);
      const bytes = encodePack([...live, { hash: await sha256(unused), data: unused }]);
      const packId = uuidv7(Date.now() - 8 * DAY + source);
      await env.BUCKET.put(packKey(space.spaceId, packId), bytes);
      await runInDurableObject(space.stub, (_instance, state) => {
        state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, ?, ?, ?, ?, 0)", packId, bytes.length, PackState.LIVE, live.length, live.length * 3);
        for (const [i, { hash }] of live.entries()) {
          state.storage.sql.exec("INSERT INTO chunks (hash, size, state, refcount, hold_until_ms, incarnation, pack_id, pack_offset) VALUES (?, 3, ?, 1, 0, '1.1', ?, ?)", hash, ChunkState.LIVE, packId, i * 3);
        }
      });
      sourceIds.push(packId);
    }
    const { more } = await gc(space, Date.now());
    const packs = await packsOf(space);
    expect({
      more,
      sources: packs.filter((pack) => sourceIds.includes(String(pack.pack_id))).map((pack) => pack.state),
      outputs: packs.filter((pack) => !sourceIds.includes(String(pack.pack_id))).map((pack) => `${pack.state}:${pack.live_count}`),
    }).toEqual({ more: false, sources: [PackState.DRAINED, PackState.DRAINED], outputs: [`${PackState.LIVE}:2049`, `${PackState.LIVE}:2049`] });
  }, 120_000);
});

describe("chunk packs: rewind completion (RFC 0032 section 6 rule 5)", () => {
  it("takes the location off every row on a retired pack and makes the pack deleting", async () => {
    const space = await packedSpace();
    const { packed, pack } = await refusedPack(space, ["rewind-retired"]);
    await completeRewind(space, [pack.packId]);
    expect(await chunkRow(space, packed[0]!)).toEqual({ state: ChunkState.PENDING, refcount: 0, incarnation: null, pack_id: null });
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.DELETING, live_count: 0, live_bytes: 0 }]);
  });

  it.each([PackState.DELETING, PackState.WRITING])("makes an unretired restored pack in state %i live, with its counts recounted and the completion hold", async (restored) => {
    const space = await packedSpace();
    const { packed, pack } = await refusedPack(space, ["rewind-unretired"]);
    await runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec("UPDATE packs SET state = ?, live_count = 0, live_bytes = 0", restored));
    const completedAtMs = Date.now();
    await completeRewind(space, [], completedAtMs);
    expect(await packsOf(space)).toMatchObject([{ pack_id: pack.packId, state: PackState.LIVE, live_count: 1, live_bytes: packed[0]!.bytes.length, hold_until_ms: completedAtMs + 30 * DAY }]);
  });

  it("refuses a completion that lists no retired packs and changes nothing", async () => {
    const space = await packedSpace();
    const { pack } = await refusedPack(space, ["rewind-old-account"]);
    const before = await packsOf(space);
    await expect(completeRewind(space, undefined)).rejects.toThrow("retired packs");
    expect(await packsOf(space)).toEqual(before);
    expect(before).toMatchObject([{ pack_id: pack.packId, hold_until_ms: 0 }]);
  });
});

describe("chunk packs: which confirmation a row takes (RFC 0032 section 3)", () => {
  it.each(["HEAD", "v1 receipt"] as const)("a late %s confirmation must not mark a newly packed row as single", async (kind) => {
    const space = await packedSpace();
    const winner = await space.prepare("both-formats", "inner-missing");
    const outer = await space.prepare("both-formats", "outer-missing");
    const [common, outerMissing] = outer.chunks as [Planned, Planned];
    const pack = await storedPack(space, [common]);
    const single = await putSingle(common);
    const key = new URL((kind === "HEAD" ? common : outerMissing).url);
    const hash = key.pathname.split("/").at(-1)!;
    const held = holdHead(`spaces/${space.spaceId}/chunks/${hash.slice(0, 2)}/${hash}/${key.searchParams.get("inc")}`);
    const waiting = space.commit(outer, kind === "HEAD" ? [] : [single]);
    try {
      await vi.waitFor(() => expect(held.reached).toBe(true));
      expect(outcome(await space.commit(winner, pack.receipts, space.otherDevice()))).toBe(Error_Code.MISSING_CHUNKS);
      expect(await rowsOf(space, [common])).toMatchObject([{ state: ChunkState.PENDING, packId: pack.packId, headChecked: 0 }]);
    } finally {
      held.released = true;
      await waiting;
    }
    expect(outcome(await waiting)).toBe(Error_Code.MISSING_CHUNKS);
    expect(await rowsOf(space, [common])).toMatchObject([{ state: ChunkState.PENDING, packId: pack.packId, headChecked: 0 }]);
  });

  it("skips an ineligible pack and accepts the next eligible receipt for the same chunk", async () => {
    const space = await packedSpace();
    const file = await space.prepare("in-ineligible-and-eligible-packs");
    const rejected = await storedPack(space, file.chunks);
    const accepted = await storedPack(space, file.chunks);
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("INSERT INTO packs (pack_id, size, state, live_count, live_bytes, hold_until_ms) VALUES (?, ?, 3, 0, 0, 0)", rejected.packId, rejected.bytes.length);
    });
    expect(outcome(await space.commit(file, [...rejected.receipts, ...accepted.receipts]))).toBe("commit");
    expect(await rowsOf(space, file.chunks)).toMatchObject([{ packId: accepted.packId }]);
  });

  it("keeps receipt order when the first eligible pack needs a HEAD", async () => {
    const space = await packedSpace();
    const file = await space.prepare("expired-and-fresh");
    const expired = await storedPack(space, file.chunks, { expiresAtMs: Date.now() - 1000 });
    const fresh = await storedPack(space, file.chunks);
    expect(outcome(await space.commit(file, [...expired.receipts, ...fresh.receipts]))).toBe("commit");
    expect(await rowsOf(space, file.chunks)).toMatchObject([{ packId: expired.packId }]);
  });
});
