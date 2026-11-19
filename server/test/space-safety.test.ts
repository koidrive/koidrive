// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { OpResult_Code } from "../src/gen/koidrive/v1/commit_pb";
import { ChunkRefSchema, ContentSchema, NodeStatus } from "../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { sha256 } from "../src/crypto";
import { fromHex, toHex, uuidv7 } from "../src/ids";
import type { SpaceDO } from "../src/space-do";
import { chunkKey, encodeChunkList, rows, type VersionRow, versionChunks } from "../src/space/store";
import { client, content, freezeClock, newSpace, op, storedObjects, type TestClient } from "./helpers";

const DAY = 24 * 3_600_000;
const HOUR = 3_600_000;
const encoder = new TextEncoder();

type Space = Awaited<ReturnType<typeof newSpace>>;

async function setup() {
  const space = await newSpace();
  return { ...space, a: await client(space, "Laptop A"), b: await client(space, "Desktop B"), root: space.spaceId };
}

function setClock(space: Space, atMs: number) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => {
    freezeClock(instance, atMs);
  });
}

function gc(space: Space, atMs: number) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
}

function table(space: Space, query: string) {
  return runInDurableObject(space.stub, (_instance, state) => state.storage.sql.exec(query).toArray());
}

async function expectRefcountsMatchVersions(space: Space) {
  const { stored, counted } = await runInDurableObject(space.stub, (_instance, state) => {
    const sql = state.storage.sql;
    const counted = new Map<string, number>();
    for (const version of rows<VersionRow>(sql.exec("SELECT * FROM versions"))) {
      for (const hex of new Set(versionChunks(sql, version).map((chunk) => toHex(chunk.hash)))) {
        counted.set(hex, (counted.get(hex) ?? 0) + 1);
      }
    }
    const stored = new Map(sql.exec("SELECT hash, refcount FROM chunks").toArray().map((row) => [toHex(new Uint8Array(row.hash as ArrayBuffer)), Number(row.refcount)]));
    return { stored, counted };
  });
  for (const [hex, refcount] of stored) expect([hex, refcount]).toEqual([hex, counted.get(hex) ?? 0]);
  for (const hex of counted.keys()) expect(stored.has(hex)).toBe(true);
}

async function expectRetainedBytes(space: Space) {
  const hashes = await runInDurableObject(space.stub, (_instance, state) => {
    const sql = state.storage.sql;
    const all = new Set<string>();
    for (const version of rows<VersionRow>(sql.exec("SELECT * FROM versions"))) {
      for (const chunk of versionChunks(sql, version)) all.add(toHex(chunk.hash));
    }
    return [...all].map((hex) => ({ hex, state: sql.exec("SELECT state FROM chunks WHERE hash = ?", fromHex(hex)!).one().state }));
  });
  for (const { hex, state } of hashes) {
    expect([hex, state]).toEqual([hex, 2]);
    expect(await storedObjects(space.spaceId, hex)).toHaveLength(1);
  }
}

async function createFile(c: TestClient, parentId: string, name: string, ...text: string[]) {
  const parts = await content(...text);
  const nodeId = uuidv7();
  const result = await c.commit([op.create(nodeId, parentId, name, parts.content)], { receipts: await c.uploadParts(parts) });
  return { nodeId, result, parts };
}

async function newContent(c: TestClient, ...text: string[]) {
  const parts = await content(...text);
  return { content: parts.content, receipts: await c.uploadParts(parts) };
}

function interceptDeletes(space: Space, onDelete: (keys: string[], real: (keys: string[]) => Promise<void>) => Promise<void>) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => {
    const real = (keys: string[]) => env.BUCKET.delete(keys);
    const bucket = { list: env.BUCKET.list.bind(env.BUCKET), delete: (keys: string[]) => onDelete(keys, real) };
    const original = (instance as unknown as { env: Env }).env;
    (instance as unknown as { env: unknown }).env = new Proxy(original, { get: (target, key) => (key === "BUCKET" ? bucket : Reflect.get(target, key)) });
  });
}

describe("review regressions", () => {
  it("rejects one hash listed with two sizes, within a manifest, across ops and in PrepareUpload", async () => {
    const space = await setup();
    const parts = await content("abc");
    const receipts = await space.a.uploadParts(parts);
    const chunk = parts.content.chunks[0]!;
    const chunks = [{ ...chunk, size: 2 }, chunk];
    const twoSizes = create(ContentSchema, { size: 5n, chunks, chunkCount: 2, manifestDigest: await sha256(encodeChunkList(chunks)) });
    const within = await space.a.commitRaw([op.create(uuidv7(), space.root, "bad", twoSizes)], { receipts });
    expect(within.case === "error" && within.value.code).toBe(Error_Code.INVALID_ARGUMENT);

    const short = [{ ...chunk, size: 2 }];
    const other = create(ContentSchema, { size: 2n, chunks: short, chunkCount: 1, manifestDigest: await sha256(encodeChunkList(short)) });
    const across = await space.a.commitRaw([op.create(uuidv7(), space.root, "ok", parts.content), op.create(uuidv7(), space.root, "bad", other)], { receipts });
    expect(across.case === "error" && across.value.code).toBe(Error_Code.INVALID_ARGUMENT);

    const prepare = await space.a.call({ case: "prepareUpload", value: { chunks } });
    expect(prepare.case === "error" && prepare.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    expect(await table(space, "SELECT state, refcount, size FROM chunks")).toEqual([{ state: 1, refcount: 0, size: 3 }]);
    expect(await table(space, "SELECT COUNT(*) AS n FROM versions")).toEqual([{ n: 0 }]);
  });

  it("keeps non-round nanosecond mtimes exactly through commits, listings and restores", async () => {
    const space = await setup();
    for (const mtimeNs of [1_700_000_000_000_000_001n, -123_456_789_012_345_679n, 9_223_372_036_854_775_807n]) {
      const parts = await content();
      parts.content.mtimeNs = mtimeNs;
      const id = uuidv7();
      const result = await space.a.commit([op.create(id, space.root, "t" + mtimeNs, parts.content)]);
      const post = result.changes[0]!.change;
      expect(post.case === "node" && post.value.current!.content!.mtimeNs).toBe(mtimeNs);
      expect((await space.a.tree()).get(id)!.current!.content!.mtimeNs).toBe(mtimeNs);
      const versions = await space.a.call({ case: "listVersions", value: { nodeId: id } });
      expect(versions.case === "versions" && versions.value.versions[0]!.content!.mtimeNs).toBe(mtimeNs);

      const later = await content();
      await space.a.commit([op.update(id, later.content)]);
      const firstVersion = versions.case === "versions" ? versions.value.versions[0]!.versionId : "";
      await space.a.commit([op.restore(id, { versionId: firstVersion })]);
      expect((await space.a.tree()).get(id)!.current!.content!.mtimeNs).toBe(mtimeNs);
    }
  });

  it("pages through every retained version, including several written by one commit", async () => {
    const space = await setup();
    const id = uuidv7();
    const v = await Promise.all(["1", "2", "3", "4", "5"].map((text) => content(text)));
    const receipts = await space.a.upload(...v.map((x) => x.content));
    await space.a.commit([op.create(id, space.root, "f", v[0]!.content), op.update(id, v[1]!.content), op.update(id, v[2]!.content)], { receipts });
    await space.a.commit([op.update(id, v[3]!.content)], { receipts });
    await space.a.commit([op.update(id, v[4]!.content)], { receipts });

    const seen: string[] = [];
    const seqs: bigint[] = [];
    let cursor = "";
    do {
      const page = await space.a.call({ case: "listVersions", value: { nodeId: id, limit: 2, cursor } });
      if (page.case !== "versions") throw new Error("listVersions failed");
      seen.push(...page.value.versions.map((x) => x.versionId));
      seqs.push(...page.value.versions.map((x) => x.seq));
      cursor = page.value.nextCursor;
    } while (cursor);
    expect(new Set(seen).size).toBe(5);
    expect(seqs).toEqual([3n, 2n, 1n, 1n, 1n]);
  });

  it("rejects moves and restores that would put a descendant deeper than 256, even when only a trashed one would", { timeout: 30_000 }, async () => {
    const space = await setup();
    let parent = space.root;
    const ops = [];
    for (let level = 1; level <= 254; level++) {
      const id = uuidv7();
      ops.push(op.mkdir(id, parent, "d" + level));
      parent = id;
    }
    const subtree = uuidv7();
    const child = uuidv7();
    const grandchild = uuidv7();
    ops.push(op.mkdir(subtree, space.root, "subtree"), op.mkdir(child, subtree, "child"), op.mkdir(grandchild, child, "grandchild"));
    await space.a.commit(ops);

    const deep = await space.a.commitRaw([op.move(subtree, parent, "subtree")]);
    expect(deep.case === "error" && deep.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    await space.a.commit([op.delete(grandchild)]);
    const withTrash = await space.a.commitRaw([op.move(subtree, parent, "subtree")]);
    expect(withTrash.case === "error" && withTrash.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    await space.a.commit([op.delete(subtree)]);
    const restore = await space.a.commitRaw([op.restore(subtree, { targetParentId: parent })]);
    expect(restore.case === "error" && restore.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    expect((await space.a.tree()).has(subtree)).toBe(false);

    const sideways = await space.a.commit([op.restore(subtree), op.move(subtree, ops[0]!.op.value!.nodeId as string, "subtree")]);
    expect(sideways.results.map((r) => r.code)).toEqual([OpResult_Code.APPLIED, OpResult_Code.APPLIED]);
  });

  it("applies identical concurrent retries once", async () => {
    const space = await setup();
    const parts = await content("shared", "shared");
    const receipts = await space.a.uploadParts(parts);
    const commitId = uuidv7();
    const ops = [op.create(uuidv7(), space.root, "one", parts.content)];
    const [x, y] = await Promise.all([space.a.commit(ops, { commitId, receipts }), space.a.commit(ops, { commitId, receipts })]);
    expect(x).toEqual(y);
    const racing = await Promise.all([space.a.commitRaw(ops, { commitId, receipts }), space.b.commitRaw(ops, { commitId, receipts })]);
    expect(racing.map((result) => (result.case === "error" ? result.value.code : result.case))).toEqual(["commit", Error_Code.INVALID_ARGUMENT]);
    expect(await table(space, "SELECT refcount FROM chunks")).toEqual([{ refcount: 1 }]);
    expect(await table(space, "SELECT COUNT(*) AS n FROM versions")).toEqual([{ n: 1 }]);
    const accepted = await table(space, "SELECT device_id, device_seq FROM device_commits");
    expect(accepted).toEqual([{ device_id: space.a.session.deviceId, device_seq: 1 }]);
    const next = await Promise.all([space.a.commit([op.mkdir(uuidv7(), space.root, "two")]), space.b.commit([op.mkdir(uuidv7(), space.root, "three")])]);
    expect(next.map((r) => r.seq).sort()).toEqual([2n, 3n]);
  });

  it("rolls back every table when a commit fails late, and replays a stored commit after eviction", { timeout: 30_000 }, async () => {
    const space = await setup();
    const kept = await createFile(space.a, space.root, "kept", "kept bytes");
    const parts = await content("rollback");
    const receipts = await space.a.uploadParts(parts);
    const snapshot = async () => ({
      nodes: await table(space, "SELECT * FROM nodes ORDER BY node_id"),
      versions: await table(space, "SELECT version_id, superseded_at_ms FROM versions ORDER BY version_id"),
      chunks: await table(space, "SELECT hash, state, refcount, zero_since_ms FROM chunks ORDER BY hash"),
      commits: await table(space, "SELECT seq FROM commits"),
      head: await table(space, "SELECT value FROM meta WHERE key = 'head_seq'"),
    });
    const before = await snapshot();

    const ops = [op.create(uuidv7(), space.root, "rollback", parts.content), op.update(kept.nodeId, parts.content)];
    let parent = space.root;
    for (let level = 1; level <= 257; level++) {
      const dir = uuidv7();
      ops.push(op.mkdir(dir, parent, "d" + level));
      parent = dir;
    }
    const late = await space.a.commitRaw(ops, { receipts });
    expect(late.case === "error" && late.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    expect(await snapshot()).toEqual(before);

    await runInDurableObject(space.stub, (instance: SpaceDO) => {
      (instance as unknown as { appendCommit: () => void }).appendCommit = () => {
        throw new Error("injected failure before the log append");
      };
    });
    const injected = await space.a.commitRaw([op.update(kept.nodeId, parts.content)], { receipts });
    expect(injected.case === "error" && injected.value.code).toBe(Error_Code.INTERNAL);
    expect(await snapshot()).toEqual(before);

    await evictDurableObject(space.stub);
    const commitId = uuidv7();
    const first = await space.a.commit([op.update(kept.nodeId, parts.content)], { commitId, receipts });
    expect(first.seq).toBe(2n);
    await evictDurableObject(space.stub);
    expect(await space.a.commit([op.update(kept.nodeId, parts.content)], { commitId })).toEqual(first);
    expect(await table(space, "SELECT MAX(seq) AS head FROM commits")).toEqual([{ head: 2 }]);
    await expectRefcountsMatchVersions(space);
  });

  it("matches independently computed manifest digests", async () => {
    const abc = fromHex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")!;
    const defg = fromHex("4c8a43980498636e9c1d1595fa5d115af7937c2422dfe68a2520a52b7a5fb4de")!;
    expect(toHex(await sha256(encoder.encode("abc")))).toBe(toHex(abc));
    const one = [create(ChunkRefSchema, { hash: abc, size: 3 })];
    const two = [...one, create(ChunkRefSchema, { hash: defg, size: 4 })];
    expect(toHex(await sha256(encodeChunkList(one)))).toBe("fea608ad6e33243d10f7a99453cf91bd068a08e001215e48e319d325325fccc8");
    expect(toHex(await sha256(encodeChunkList(two)))).toBe("c9903f857c7c593fdec297557ab9618bbe18fc8a95dc8a86cc8c58c75fac8f1a");

    const space = await setup();
    const handBuilt = create(ContentSchema, {
      size: 7n,
      chunks: two,
      chunkCount: 2,
      manifestDigest: fromHex("c9903f857c7c593fdec297557ab9618bbe18fc8a95dc8a86cc8c58c75fac8f1a")!,
    });
    const receipts = await space.a.uploadParts({ content: handBuilt, bytes: [encoder.encode("abc"), encoder.encode("defg")] });
    const result = await space.a.commit([op.create(uuidv7(), space.root, "golden", handBuilt)], { receipts });
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
  });
});

describe("GC under failures", () => {
  it("leaves chunks deleting after a failed R2 delete, refuses them meanwhile, and lets a later upload survive", async () => {
    const space = await setup();
    const parts = await content("abandoned upload");
    await space.a.uploadParts(parts);
    const at = Date.now() + DAY + HOUR;
    await interceptDeletes(space, async () => {
      throw new Error("R2 unavailable");
    });
    await expect(gc(space, at)).rejects.toThrow("R2 unavailable");
    expect(await table(space, "SELECT state FROM chunks")).toEqual([{ state: 3 }]);

    await setClock(space, at);
    space.a.nowMs = at;
    const plan = await space.a.call({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks[0]!.action).toBe(ChunkPlan_Action.RETRY_LATER);
    const refused = await space.a.commitRaw([op.create(uuidv7(), space.root, "f", parts.content)]);
    expect(refused.case === "error" && refused.value.code).toBe(Error_Code.MISSING_CHUNKS);

    await interceptDeletes(space, (keys, real) => real(keys));
    expect(await gc(space, at)).toMatchObject({ deletedChunks: 1 });
    const receipts = await space.a.uploadParts(parts);
    expect(receipts).toHaveLength(1);
    await space.a.commit([op.create(uuidv7(), space.root, "f", parts.content)], { receipts });
    expect(await gc(space, at + 40 * DAY)).toMatchObject({ deletedChunks: 0 });
    await expectRetainedBytes(space);
  });

  it("refuses a chunk while its deletion is paused between marking and R2, then accepts a fresh upload", async () => {
    const space = await setup();
    const parts = await content("paused deletion");
    await space.a.uploadParts(parts);
    const at = Date.now() + DAY + HOUR;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    await interceptDeletes(space, async (keys, real) => {
      await gate;
      await real(keys);
    });
    let running!: Promise<unknown>;
    await runInDurableObject(space.stub, (instance: SpaceDO) => {
      running = instance.collectGarbage(at);
    });
    for (let i = 0; i < 50 && (await table(space, "SELECT state FROM chunks"))[0]?.state !== 3; i++) await new Promise((r) => setTimeout(r, 10));
    expect(await table(space, "SELECT state FROM chunks")).toEqual([{ state: 3 }]);

    await setClock(space, at);
    space.a.nowMs = at;
    const plan = await space.a.call({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks[0]!.action).toBe(ChunkPlan_Action.RETRY_LATER);
    const refused = await space.a.commitRaw([op.create(uuidv7(), space.root, "f", parts.content)]);
    expect(refused.case === "error" && refused.value.code).toBe(Error_Code.MISSING_CHUNKS);

    release();
    await running;
    expect(await storedObjects(space.spaceId, parts.content.chunks[0]!.hash)).toEqual([]);
    const receipts = await space.a.uploadParts(parts);
    await space.a.commit([op.create(uuidv7(), space.root, "f", parts.content)], { receipts });
    await expectRetainedBytes(space);
  });

  it("finishes the deletion after an eviction between the R2 delete and dropping the rows", async () => {
    const space = await setup();
    const parts = await content("half deleted");
    await space.a.uploadParts(parts);
    const at = Date.now() + DAY + HOUR;
    await interceptDeletes(space, async (keys, real) => {
      await real(keys);
      throw new Error("crash after the R2 delete");
    });
    await expect(gc(space, at)).rejects.toThrow("crash after the R2 delete");
    expect(await table(space, "SELECT state FROM chunks")).toEqual([{ state: 3 }]);
    await evictDurableObject(space.stub);
    expect(await gc(space, at)).toMatchObject({ deletedChunks: 1 });
    expect(await table(space, "SELECT * FROM chunks")).toEqual([]);
  });
});

describe("GC and references", () => {
  it("keeps refcounts equal to a recount through shared chunks, retries, expiry, purge and re-reference", async () => {
    const space = await setup();
    const t0 = Date.now();
    const shared = await content("same", "same", "other");
    const receipts = await space.a.uploadParts(shared);
    const one = uuidv7();
    const two = uuidv7();
    const commitId = uuidv7();
    await space.a.commit([op.create(one, space.root, "one", shared.content), op.create(two, space.root, "two", shared.content)], { commitId, receipts });
    await space.a.commit([op.create(one, space.root, "one", shared.content), op.create(two, space.root, "two", shared.content)], { commitId });
    await expectRefcountsMatchVersions(space);
    expect(await table(space, "SELECT refcount FROM chunks ORDER BY refcount")).toEqual([{ refcount: 2 }, { refcount: 2 }]);

    const next = await newContent(space.a, "replacement");
    await space.a.commit([op.update(one, next.content), op.delete(two)], { receipts: next.receipts });
    await expectRefcountsMatchVersions(space);

    const t1 = t0 + 31 * DAY;
    expect(await gc(space, t1)).toMatchObject({ expiredVersions: 1, purgedNodes: 1, deletedChunks: 0 });
    await expectRefcountsMatchVersions(space);
    expect(await table(space, "SELECT refcount, zero_since_ms FROM chunks WHERE refcount = 0")).toEqual([
      { refcount: 0, zero_since_ms: t1 },
      { refcount: 0, zero_since_ms: t1 },
    ]);

    await setClock(space, t1 + 10 * DAY);
    const plan = await space.a.call({ case: "prepareUpload", value: { chunks: shared.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks.map((c) => c.action)).toEqual([ChunkPlan_Action.PRESENT, ChunkPlan_Action.PRESENT]);
    await space.a.commit([op.create(uuidv7(), space.root, "three", shared.content)]);
    expect(await table(space, "SELECT COUNT(*) AS n FROM chunks WHERE zero_since_ms IS NOT NULL")).toEqual([{ n: 0 }]);
    expect(await gc(space, t1 + 400 * DAY)).toMatchObject({ deletedChunks: 0 });
    await expectRefcountsMatchVersions(space);
    await expectRetainedBytes(space);
  });

  it("deletes nothing at the exact hold and version-retention boundaries, and lets a PRESENT hold outlast the grace", async () => {
    const space = await setup();
    const { nodeId, parts } = await createFile(space.a, space.root, "doc", "first bytes");
    const pendingParts = await content("pending only");
    await space.a.uploadParts(pendingParts);
    const holdUntil = Number((await table(space, "SELECT hold_until_ms FROM chunks WHERE refcount = 0"))[0]!.hold_until_ms);
    expect(await gc(space, holdUntil)).toMatchObject({ deletedChunks: 0 });
    expect(await gc(space, holdUntil + 1)).toMatchObject({ deletedChunks: 1 });

    const replaced = await newContent(space.a, "second bytes");
    await space.a.commit([op.update(nodeId, replaced.content)], { receipts: replaced.receipts });
    const superseded = Number((await table(space, "SELECT superseded_at_ms FROM versions WHERE superseded_at_ms IS NOT NULL"))[0]!.superseded_at_ms);
    expect(await gc(space, superseded + 30 * DAY)).toMatchObject({ expiredVersions: 0 });
    const zeroAt = superseded + 30 * DAY + 1;
    expect(await gc(space, zeroAt)).toMatchObject({ expiredVersions: 1, deletedChunks: 0 });

    await setClock(space, zeroAt + 29 * DAY + 12 * HOUR);
    const plan = await space.a.call({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks[0]!.action).toBe(ChunkPlan_Action.PRESENT);
    const held = zeroAt + 30 * DAY + 12 * HOUR;
    expect(await gc(space, zeroAt + 30 * DAY + 1)).toMatchObject({ deletedChunks: 0 });
    expect(await gc(space, held)).toMatchObject({ deletedChunks: 0 });
    expect(await gc(space, held + 1)).toMatchObject({ deletedChunks: 1 });
    expect(await storedObjects(space.spaceId, parts.content.chunks[0]!.hash)).toEqual([]);
    await expectRetainedBytes(space);
  });

  it("purges trash and deletes an unreferenced chunk only after the exact trash-retention and grace boundaries", async () => {
    const space = await setup();
    const { nodeId, parts } = await createFile(space.a, space.root, "doc", "trash me");
    await space.a.commit([op.delete(nodeId)]);
    const trashedAt = Number((await table(space, "SELECT trashed_at_ms FROM nodes WHERE status = 2"))[0]!.trashed_at_ms);
    expect(await gc(space, trashedAt + 30 * DAY)).toMatchObject({ purgedNodes: 0 });
    const purgedAt = trashedAt + 30 * DAY + 1;
    expect(await gc(space, purgedAt)).toMatchObject({ purgedNodes: 1, deletedChunks: 0 });

    const [chunk] = await table(space, "SELECT refcount, zero_since_ms, hold_until_ms FROM chunks");
    expect(chunk).toMatchObject({ refcount: 0, zero_since_ms: purgedAt });
    expect(Number(chunk!.hold_until_ms)).toBeLessThan(purgedAt);
    expect(await gc(space, purgedAt + 30 * DAY)).toMatchObject({ deletedChunks: 0 });
    expect(await gc(space, purgedAt + 30 * DAY + 1)).toMatchObject({ deletedChunks: 1 });
    expect(await storedObjects(space.spaceId, parts.content.chunks[0]!.hash)).toEqual([]);
  });

  it("rejects a commit whose change records do not fit in one log row and changes nothing", async () => {
    const space = await setup();
    const big = uuidv7();
    await space.a.commit([op.mkdir(big, space.root, "big")]);
    for (let batch = 0; batch < 14; batch++) {
      await space.a.commit(Array.from({ length: 1000 }, (_, i) => op.mkdir(uuidv7(), big, String(batch * 1000 + i).padStart(250, "x"))));
    }
    const before = await table(space, "SELECT MAX(seq) AS seq, COUNT(*) AS n FROM commits");
    const result = await space.a.commitRaw([op.delete(big)]);
    expect(result.case === "error" && result.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    expect(await table(space, "SELECT MAX(seq) AS seq, COUNT(*) AS n FROM commits")).toEqual(before);
    expect(await table(space, "SELECT COUNT(*) AS n FROM nodes WHERE status = 2")).toEqual([{ n: 0 }]);
  }, 120_000);

  it("runs GC from the alarm and re-arms it", async () => {
    const space = await setup();
    const parts = await content("left behind");
    await space.a.uploadParts(parts);
    const at = Date.now() + DAY + HOUR;
    await setClock(space, at);
    expect(await runDurableObjectAlarm(space.stub)).toBe(true);
    expect(await table(space, "SELECT * FROM chunks")).toEqual([]);
    expect(await runInDurableObject(space.stub, (_i, state) => state.storage.getAlarm())).toBe(at + DAY);
  });

  it("keeps the GC alarm armed when a run fails and finishes the deletion on the retry", async () => {
    const space = await setup();
    const parts = await content("left behind");
    await space.a.uploadParts(parts);
    await interceptDeletes(space, async () => {
      throw new Error("injected R2 delete failure");
    });
    const at = Date.now() + DAY + HOUR;
    await setClock(space, at);
    expect(await runDurableObjectAlarm(space.stub)).toBe(true);
    expect(await table(space, "SELECT state FROM chunks")).toEqual([{ state: 3 }]);
    const retryAt = await runInDurableObject(space.stub, (_i, state) => state.storage.getAlarm());
    expect(retryAt).toBe(at + 10 * 60_000);

    await interceptDeletes(space, (keys, real) => real(keys));
    await setClock(space, retryAt!);
    expect(await runDurableObjectAlarm(space.stub)).toBe(true);
    expect(await table(space, "SELECT * FROM chunks")).toEqual([]);
    expect(await storedObjects(space.spaceId, parts.content.chunks[0]!.hash)).toEqual([]);
  });
});

describe("more conflict rules", () => {
  it("R5: the later of two concurrent moves of one node wins", async () => {
    const space = await setup();
    const { a, b, root } = space;
    const [x, p1, p2] = [uuidv7(), uuidv7(), uuidv7()];
    await a.commit([op.mkdir(x, root, "X"), op.mkdir(p1, root, "P1"), op.mkdir(p2, root, "P2")]);
    b.baseSeq = 1n;
    await a.commit([op.move(x, p1, "X")]);
    const later = await b.commit([op.move(x, p2, "X moved by B")]);
    expect(later.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await a.tree()).get(x)).toMatchObject({ parentId: p2, name: "X moved by B" });
    await expectRetainedBytes(space);
  });

  it("R7: keeps a target renamed unseen and a newly created child, but not same-device changes", async () => {
    const space = await setup();
    const { a, b, root } = space;
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Folder")]);
    const { nodeId: old } = await createFile(a, dir, "old", "old");
    b.baseSeq = a.baseSeq;
    await a.commit([op.move(dir, root, "Renamed")]);
    const renamed = await b.commit([op.delete(dir)]);
    expect(renamed.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    let tree = await b.tree();
    expect(tree.get(dir)!.name).toBe("Renamed");
    expect(tree.has(old)).toBe(false);

    b.baseSeq = a.baseSeq;
    const { nodeId: fresh } = await createFile(a, dir, "fresh", "fresh");
    await b.commit([op.delete(dir)]);
    tree = await b.tree();
    expect(tree.has(dir) && tree.has(fresh)).toBe(true);

    const mine = await createFile(b, dir, "mine", "mine");
    const all = await b.commit([op.delete(dir)], { baseSeq: mine.result.seq - 1n });
    expect(all.results[0]!.code).toBe(OpResult_Code.APPLIED);
    tree = await b.tree();
    expect([dir, fresh, mine.nodeId].some((id) => tree.has(id))).toBe(false);
    await expectRetainedBytes(space);
  });

  it("R8 and R9: restores trash groups separately, revives a moved trashed subtree, and renames on a clash", async () => {
    const space = await setup();
    const { a, root } = space;
    const dir = uuidv7();
    const sub = uuidv7();
    await a.commit([op.mkdir(dir, root, "Parent"), op.mkdir(sub, dir, "Sub")]);
    const { nodeId: file } = await createFile(a, sub, "a.txt", "a");
    await a.commit([op.delete(file)]);
    await a.commit([op.delete(dir)]);
    await a.commit([op.restore(dir)]);
    let tree = await a.tree();
    expect(tree.has(dir) && tree.has(sub)).toBe(true);
    expect(tree.has(file)).toBe(false);

    await createFile(a, sub, "A.TXT", "replacement");
    const clash = await a.commit([op.restore(file)]);
    expect(clash.results[0]!.code).toBe(OpResult_Code.APPLIED_RENAMED);
    expect(clash.results[0]!.name).toMatch(/^a \(conflict from Laptop A \d{4}-\d{2}-\d{2}\)\.txt$/);

    await a.commit([op.delete(sub)]);
    const target = uuidv7();
    await a.commit([op.mkdir(target, root, "Elsewhere")]);
    const moved = await a.commit([op.move(sub, target, "Sub")]);
    expect(moved.results[0]!.code).toBe(OpResult_Code.APPLIED);
    tree = await a.tree();
    expect(tree.get(sub)).toMatchObject({ parentId: target, status: NodeStatus.LIVE });
    expect(tree.get(file)!.parentId).toBe(sub);
    await expectRetainedBytes(space);
  });

  it("R10: restoring an old version over an unseen edit makes a conflict copy", async () => {
    const space = await setup();
    const { a, b, root } = space;
    const { nodeId, result } = await createFile(a, root, "notes.md", "v1");
    const post = result.changes[0]!.change;
    const v1 = post.case === "node" ? post.value.current!.versionId : "";
    const v2 = await newContent(a, "v2");
    await a.commit([op.update(nodeId, v2.content)], { receipts: v2.receipts });
    b.baseSeq = 2n;
    const v3 = await newContent(b, "v3 from B");
    await b.commit([op.update(nodeId, v3.content)], { receipts: v3.receipts });
    const restored = await a.commit([op.restore(nodeId, { versionId: v1 })], { baseSeq: 2n });
    expect(restored.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await a.tree();
    expect(tree.get(nodeId)!.current!.content!.manifestDigest).toEqual(v3.content.manifestDigest);
    expect(tree.get(restored.results[0]!.nodeId)!.current!.content!.manifestDigest).toEqual((await content("v1")).content.manifestDigest);
    await expectRefcountsMatchVersions(space);
    await expectRetainedBytes(space);
  });
});
