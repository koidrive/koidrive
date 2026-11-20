// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type UploadReceipt, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import type { RpcResponse } from "../src/gen/koidrive/v1/messages_pb";
import { ChunkPlan_Action, Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../src/ids";
import { HOLD_MS, type SpaceDO } from "../src/space-do";
import { ChunkState } from "../src/space/schema";
import { getMeta } from "../src/space/store";
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

describe("chunk packs: what stays unchanged until pack GC (RFC 0032 section 11 item 3)", () => {
  it("never marks a packed row for deletion, however long it waits unreferenced", async () => {
    const space = await packedSpace();
    const file = await space.prepare("packed-orphan", "never-sent");
    const [chunk] = file.chunks as [Planned];
    const pack = await storedPack(space, [chunk]);
    expect(outcome(await space.commit(file, pack.receipts))).toBe(Error_Code.MISSING_CHUNKS);
    await runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(Date.now() + 40 * DAY));
    expect(await rowsOf(space, [chunk])).toMatchObject([{ state: ChunkState.PENDING, packId: pack.packId }]);
    const retired = await runInDurableObject(account(), (_instance, state) =>
      state.storage.sql.exec("SELECT hash_hex FROM chunk_retirements WHERE space_id = ?", space.spaceId).toArray().map((row) => row.hash_hex),
    );
    expect(retired).not.toContain(toHex(chunk.hash));
  });

  it("keeps a packed row's location and incarnation when a rewind retires that incarnation", async () => {
    const space = await packedSpace();
    const file = await space.prepare("packed-rewound", "never-sent");
    const [chunk] = file.chunks as [Planned];
    const pack = await storedPack(space, [chunk]);
    expect(outcome(await space.commit(file, pack.receipts))).toBe(Error_Code.MISSING_CHUNKS);
    const nowMs = Date.now();
    const retired = [{ hashHex: toHex(chunk.hash), incarnation: chunk.incarnation }];
    await space.stub.rewindComplete({ rewindId: uuidv7(), attempt: 1, historyId: uuidv7(), rewindNumber: 1, restorePointMs: nowMs - 60_000, completedAtMs: nowMs, revokedDevices: [], retired });
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
