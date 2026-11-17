// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";
import { OpSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ChunkRefSchema, CommitRecordSchema, ContentSchema } from "../src/gen/koidrive/v1/model_pb";
import { sha256 } from "../src/crypto";
import { uuidv7, uuidv7Ms } from "../src/ids";
import { SpaceDO } from "../src/space-do";
import { descriptorDurable } from "../src/space/export";
import { encodeChunkList, setMeta } from "../src/space/store";
import { type Manifest, restoreFromR2, serverState } from "./export-reader";
import { client, content, newSpace, op, runSpaceAlarm, type TestClient, unknownOutcome } from "./helpers";

const MINUTE = 60_000;
const DAY = 24 * 60 * MINUTE;

type Space = Awaited<ReturnType<typeof newSpace>>;

function setClock(space: Space, device: TestClient, atMs: number) {
  device.nowMs = atMs;
  return runInDurableObject(space.stub, (instance: SpaceDO) => {
    instance.clock = () => atMs;
  });
}

async function listKeys(prefix: string): Promise<string[]> {
  return (await env.BUCKET.list({ prefix })).objects.map((object) => object.key);
}

async function historyPrefix(space: Space, device: TestClient): Promise<string> {
  return `spaces/${space.spaceId}/meta/${await device.history()}/`;
}

async function manifests(prefix: string): Promise<Manifest[]> {
  const out: Manifest[] = [];
  for (const key of await listKeys(prefix + "snap/")) if (key.endsWith("/manifest.json")) out.push((await (await env.BUCKET.get(key))!.json()) as Manifest);
  return out;
}

async function expectRestorable(space: Space) {
  const restored = await restoreFromR2(space.spaceId);
  const server = await serverState(space.stub);
  expect(restored.headSeq).toBe(server.headSeq);
  expect(restored.nodes).toEqual(server.nodes);
  return restored;
}

async function forgetExport(space: Space, meta: Record<string, number> = {}) {
  await runInDurableObject(space.stub, (_instance, state) => {
    const { sql } = state.storage;
    sql.exec("DELETE FROM meta WHERE key IN ('export_state', 'exported_seq')");
    sql.exec("DELETE FROM schedule WHERE task = 'export'");
    for (const [key, value] of Object.entries(meta)) setMeta(sql, key, value);
  });
  await env.BUCKET.delete(await listKeys(`spaces/${space.spaceId}/meta/`));
  await reopen(space);
}

/**
 * Opens a minute ahead, so that a deadline it sets is not already past, which setAlarm would move to the moment of the
 * call.
 */
function reopen(space: Space) {
  return runInDurableObject(space.stub, (_instance, state) => {
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + MINUTE);
    new SpaceDO(state, env);
    vi.restoreAllMocks();
  });
}

function alarmState(space: Space) {
  return runInDurableObject(space.stub, async (_instance, state) => ({
    alarm: await state.storage.getAlarm(),
    due: Number(state.storage.sql.exec("SELECT MIN(due_ms) AS due FROM schedule").one().due),
    exportDue: Number(state.storage.sql.exec("SELECT due_ms FROM schedule WHERE task = 'export'").toArray()[0]?.due_ms),
    descriptor: descriptorDurable(state.storage.sql),
  }));
}

function failNextAlarm(space: Space) {
  return runInDurableObject(space.stub, (_instance, state) => {
    vi.spyOn(state.storage, "setAlarm").mockRejectedValueOnce(new Error("the alarm write failed"));
  });
}

async function recordPushes(space: Space): Promise<bigint[]> {
  const seqs: bigint[] = [];
  await runInDurableObject(space.stub, (instance: SpaceDO) => {
    const call = instance.call.bind(instance);
    vi.spyOn(instance, "call").mockImplementation(async (...args) => {
      const answer = await call(...args);
      if (answer.record) seqs.push(answer.record.seq);
      return answer;
    });
  });
  return seqs;
}

const seqName = (seq: number) => String(seq).padStart(20, "0");
const segmentKey = (prefix: string, first: number, last: number) => `${prefix}log/${seqName(first)}-${seqName(last)}.fblog`;

describe("R2 metadata export (spec v0 section 11, RFC 0004)", () => {
  it("writes the descriptor of a space's first history when the space is created", async () => {
    const space = await newSpace("Photos");
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    expect(await listKeys(`spaces/${space.spaceId}/meta/`)).toEqual([prefix + "space.json"]);
    expect(await (await env.BUCKET.get(prefix + "space.json"))!.json()).toEqual({
      format: "koidrive.space",
      format_version: 1,
      space_id: space.spaceId,
      name: "Photos",
      created_at: new Date(uuidv7Ms(space.spaceId)).toISOString(),
      hash_alg: "sha256",
      chunk_key: "spaces/{space_id}/chunks/{hex2}/{hex}/{incarnation}",
      chunk_encoding: "raw",
      history_id: await a.history(),
      rewind_number: 0,
    });
    await expectRestorable(space);
  });

  it("exports log segments and fuzzy snapshot pages that restore the tree, retained versions and trash", async () => {
    const space = await newSpace();
    const { spaceId } = space;
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    let now = Date.now();
    await setClock(space, a, now);
    const [dir, f1, f2, f3] = [uuidv7(), uuidv7(), uuidv7(), uuidv7()];
    const parts = await Promise.all(["one", "two", "three", "four", "five"].map((text) => content(text + " " + uuidv7())));
    const [c1, c2, c3, c4, c5] = parts.map((part) => part.content);
    await a.commit([op.mkdir(dir, spaceId, "docs"), op.create(f1, dir, "a.txt", c1!), op.create(f2, dir, "b.txt", c2!), op.create(f3, spaceId, "c.txt", c3!)], {
      receipts: await a.upload(c1!, c2!, c3!),
    });
    await setClock(space, a, (now += MINUTE));
    await a.commit([op.update(f1, c4!)], { receipts: await a.upload(c4!) });
    await a.commit([op.move(f2, spaceId, "b2.txt")]);
    await a.commit([op.delete(f3)]);

    await runSpaceAlarm(space.stub, now + MINUTE);
    expect(await listKeys(prefix + "log/")).toEqual([]);
    await runSpaceAlarm(space.stub, now + 5 * MINUTE);
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 1, 4)]);
    expect(await manifests(prefix)).toEqual([]);
    await expectRestorable(space);

    await setClock(space, a, (now += DAY));
    const [f4, f5] = [uuidv7(), uuidv7()];
    await a.commit([op.create(f4, dir, "d.txt", c5!), op.mkdir(f5, spaceId, "e")], { receipts: await a.upload(c5!) });
    // Each side polls a flag, because a promise resolved inside the Durable Object would carry its I/O context back
    // into the test.
    const receipts = await a.upload(c2!);
    const pause = { reached: false, released: false };
    const until = async (flag: () => boolean) => {
      while (!flag()) await new Promise((resolve) => setTimeout(resolve, 5));
    };
    const running = runSpaceAlarm(space.stub, now + 5 * MINUTE, {
      pageNodes: 2,
      intercept: async (key, put) => {
        if (key.endsWith("/pages/0.fbsnap")) {
          pause.reached = true;
          await until(() => pause.released);
        }
        return put();
      },
    });
    await until(() => pause.reached);
    const during = (await a.commit([op.update(f1, c2!), op.delete(f2), op.restore(f3), op.move(f4, f5, "d.txt"), op.delete(dir)], { receipts, historyId: prefix.split("/")[3] })).seq;
    pause.released = true;
    await running;
    const [manifest] = await manifests(prefix);
    expect(manifest).toMatchObject({ format: "koidrive.snapshot", format_version: 1, space_id: spaceId, start_seq: 5, end_seq: Number(during) });
    expect(manifest!.pages.map((page) => page.name)).toEqual(["0.fbsnap", "1.fbsnap", "2.fbsnap", "3.fbsnap"]);
    expect(manifest!.pages.reduce((sum, page) => sum + page.records, 0)).toBe(7);
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 1, 4), segmentKey(prefix, 5, 5), segmentKey(prefix, 6, 6)]);
    const restored = await expectRestorable(space);
    expect(restored.manifest!.start_seq).toBe(5);
    expect(restored.nodes.get(f1)!.previous.size).toBe(2);
    expect(restored.nodes.get(f2)!.trashedAtMs).toBe(BigInt(now + 5 * MINUTE));
  });

  it.each([1, 64])("restores newest-first alias chains across fuzzy snapshot pages with %i initial bases", async (baseCount) => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const b = await client(space, "Phone");
    const prefix = await historyPrefix(space, a);
    const now = Date.now();
    await setClock(space, a, now);
    const files = [uuidv7(), uuidv7()].sort();
    const early = files[0]!;
    const pageOrder = [space.spaceId, ...files].sort();
    const earlyPage = pageOrder.indexOf(early);
    const latePage = pageOrder.indexOf(files[1]!);
    const initial = Array.from({ length: baseCount }, () => uuidv7());
    const payload = await content("fuzzy aliases " + uuidv7());
    await a.commit(files.map((nodeId, index) => create(OpSchema, {
      op: { case: "createFile", value: { nodeId, parentId: space.spaceId, name: `file-${index}.txt`, content: payload.content, madeOver: initial } },
    })), { receipts: await a.uploadParts(payload) });
    const before = await a.tree();
    const aliases = [uuidv7(), uuidv7()];
    const pause = { reached: false, released: false };
    const events: string[] = [];
    const until = async (flag: () => boolean) => {
      while (!flag()) await new Promise((resolve) => setTimeout(resolve, 5));
    };
    const running = runSpaceAlarm(space.stub, now + DAY + 5 * MINUTE, {
      pageNodes: 1,
      intercept: async (key, put) => {
        if (key.endsWith(`/pages/${earlyPage}.fbsnap`)) {
          events.push("early page put");
          pause.reached = true;
          await until(() => pause.released);
        }
        if (key.endsWith(`/pages/${latePage}.fbsnap`)) events.push("late page put");
        return put();
      },
    });
    await until(() => pause.reached);
    try {
      for (const [index, sourceVersionId] of aliases.entries()) {
        const result = await b.commit(files.map((nodeId) => create(OpSchema, {
          op: { case: "updateContent", value: { nodeId, content: payload.content, resend: true, sourceVersionId } },
        })), { baseSeq: 0n });
        expect(result.seq).toBe(BigInt(index + 2));
        expect(result.changes).toHaveLength(2);
        events.push(`alias commit ${result.seq}`);
      }
    } finally {
      pause.released = true;
    }
    await running;
    expect(events).toEqual(["early page put", "alias commit 2", "alias commit 3", "late page put"]);
    const [manifest] = await manifests(prefix);
    expect(manifest).toMatchObject({ start_seq: 1, end_seq: 3 });
    const expected = [...aliases].reverse().concat(initial).slice(0, 64);
    const server = await a.tree();
    for (const file of files) {
      expect(server.get(file)!.current!.versionId).toBe(before.get(file)!.current!.versionId);
      expect(server.get(file)!.contentRev).toBe(1n);
      expect(server.get(file)!.editRev).toBe(1n);
      expect(server.get(file)!.current!.madeOver).toEqual(expected);
    }
    const restored = await restoreFromR2(space.spaceId);
    for (const file of files) {
      expect(restored.nodes.get(file)!.state.current!.versionId).toBe(before.get(file)!.current!.versionId);
      expect(restored.nodes.get(file)!.state.current!.madeOver).toEqual(expected);
      if (baseCount === 64) {
        expect(restored.nodes.get(file)!.state.current!.madeOver).not.toContain(initial[62]);
        expect(restored.nodes.get(file)!.state.current!.madeOver).not.toContain(initial[63]);
      }
    }
    await expectRestorable(space);
  });

  it("snapshots a version of more than 16,384 chunks at once and keeps its complete chunk list through later records", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    const now = Date.now();
    await setClock(space, a, now);
    const piece = new TextEncoder().encode("piece " + uuidv7());
    const chunk = create(ChunkRefSchema, { hash: await sha256(piece), size: piece.length });
    const chunks = Array.from({ length: 16_385 }, () => chunk);
    const big = create(ContentSchema, {
      size: BigInt(piece.length * chunks.length),
      chunks,
      chunkCount: chunks.length,
      manifestDigest: await sha256(encodeChunkList(chunks)),
      chunker: "fastcdc2020-1m-4m-16m",
    });
    const file = uuidv7();
    await a.commit([op.create(file, space.spaceId, "big.bin", big)], { receipts: await a.upload(create(ContentSchema, { chunks: [chunk] })) });
    const logged = await runInDurableObject(space.stub, (_instance, state) =>
      fromBinary(CommitRecordSchema, new Uint8Array(state.storage.sql.exec("SELECT record FROM commits WHERE seq = 1").one().record as ArrayBuffer)),
    );
    const loggedContent = logged.changes[0]!.change.value as { current: { content: { chunks: unknown[]; chunkCount: number } } };
    expect([loggedContent.current.content.chunks.length, loggedContent.current.content.chunkCount]).toEqual([0, 16_385]);

    await runSpaceAlarm(space.stub, now);
    expect((await manifests(prefix)).map((manifest) => [manifest.start_seq, manifest.end_seq])).toEqual([[1, 1]]);
    await setClock(space, a, now + 1);
    await a.commit([op.move(file, space.spaceId, "renamed.bin")]);
    await runSpaceAlarm(space.stub, now + 1 + 5 * MINUTE);
    const restored = await expectRestorable(space);
    expect(restored.nodes.get(file)!.state.current!.content!.chunks).toHaveLength(16_385);
  });

  it("keeps the two newest snapshots and deletes older ones, orphan pages included, but never a log segment", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    let now = Date.now();
    const starts: number[] = [];
    for (let i = 0; i < 4; i++) {
      await setClock(space, a, (now += DAY + MINUTE));
      const { seq } = await a.commit([op.mkdir(uuidv7(), space.spaceId, "day " + i)]);
      starts.push(Number(seq));
      await runSpaceAlarm(space.stub, now + 5 * MINUTE);
      if (i === 0) await env.BUCKET.put(`${prefix}snap/${seqName(starts[0]!)}/pages/7.fbsnap`, "an orphan page");
    }
    expect((await manifests(prefix)).map((manifest) => manifest.start_seq)).toEqual(starts.slice(2));
    expect(await listKeys(`${prefix}snap/${seqName(starts[0]!)}/`)).toEqual([]);
    expect(await listKeys(`${prefix}snap/${seqName(starts[1]!)}/`)).toEqual([]);
    expect(await listKeys(prefix + "log/")).toEqual(starts.map((seq) => segmentKey(prefix, seq, seq)));
    await expectRestorable(space);
  });

  it("repeats a segment's range, skips a page number and rewrites the same manifest bytes after writes with unknown outcomes", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    let now = Date.now();
    await setClock(space, a, now);
    for (let i = 0; i < 3; i++) await a.commit([op.mkdir(uuidv7(), space.spaceId, "folder " + i)]);
    const isSegment = (key: string) => key.includes("/log/");
    await runSpaceAlarm(space.stub, (now += 5 * MINUTE), { intercept: unknownOutcome(isSegment, false) });
    expect(await listKeys(prefix + "log/")).toEqual([]);
    await a.commit([op.mkdir(uuidv7(), space.spaceId, "after the failure")]);
    await runSpaceAlarm(space.stub, (now += MINUTE), { intercept: unknownOutcome(isSegment, true) });
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 1, 3)]);
    await runSpaceAlarm(space.stub, (now += 5 * MINUTE));
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 1, 3), segmentKey(prefix, 4, 4)]);

    now += DAY;
    await runSpaceAlarm(space.stub, now, { pageNodes: 2, intercept: unknownOutcome((key) => key.endsWith("/pages/1.fbsnap"), true) });
    const snap = `${prefix}snap/${seqName(4)}/`;
    expect(await listKeys(snap)).toEqual([snap + "pages/0.fbsnap", snap + "pages/1.fbsnap"]);
    await runSpaceAlarm(space.stub, (now += 2 * MINUTE), { pageNodes: 2, intercept: unknownOutcome((key) => key.endsWith("/manifest.json"), true) });
    const lostManifest = await (await env.BUCKET.get(snap + "manifest.json"))!.text();
    await runSpaceAlarm(space.stub, (now += 2 * MINUTE), { pageNodes: 2 });
    expect(await (await env.BUCKET.get(snap + "manifest.json"))!.text()).toBe(lostManifest);
    const [manifest] = await manifests(prefix);
    expect(manifest!.pages.map((page) => page.name)).toEqual(["0.fbsnap", "2.fbsnap", "3.fbsnap"]);
    expect(await listKeys(snap + "pages/")).toContain(snap + "pages/1.fbsnap");
    await expectRestorable(space);
  });

  it("starts the export of a space that an earlier server created when its object opens, from seq 0 in its first history", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    await a.commit([op.mkdir(uuidv7(), space.spaceId, "one")]);
    await forgetExport(space);
    const opened = await alarmState(space);
    expect(opened.alarm).toBe(opened.exportDue);
    const b = await client(space, "Phone");
    await b.commit([op.mkdir(uuidv7(), space.spaceId, "two")], { baseSeq: 1n });
    await runSpaceAlarm(space.stub, Date.now() + 2 * DAY);
    expect((await alarmState(space)).descriptor).toBe(true);
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 1, 2)]);
    expect((await manifests(prefix)).map((manifest) => manifest.start_seq)).toEqual([2]);
    await expectRestorable(space);
  });

  it("starts the export of a rewound history that an earlier server created from restore_seq, with the descriptor after a snapshot", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const prefix = await historyPrefix(space, a);
    await a.commit([op.mkdir(uuidv7(), space.spaceId, "one")]);
    await forgetExport(space, { restore_seq: 1, rewind_number: 1 });
    expect((await alarmState(space)).descriptor).toBe(false);
    const b = await client(space, "Phone");
    await b.commit([op.mkdir(uuidv7(), space.spaceId, "two")], { baseSeq: 1n });
    await runSpaceAlarm(space.stub, Date.now() + 2 * DAY);
    expect((await alarmState(space)).descriptor).toBe(true);
    expect(await listKeys(prefix + "log/")).toEqual([segmentKey(prefix, 2, 2)]);
    expect((await manifests(prefix)).map((manifest) => manifest.start_seq)).toEqual([2]);
    expect(await (await env.BUCKET.get(prefix + "space.json"))!.json()).toMatchObject({ rewind_number: 1 });
    await expectRestorable(space);
  });

  it("returns and pushes a stored commit whose alarm write failed, and sets the alarm on its retry or the next commit", async () => {
    for (const repair of ["retry", "next commit"] as const) {
      const space = await newSpace();
      const a = await client(space, "Laptop");
      const pushed = await recordPushes(space);
      await failNextAlarm(space);
      const commitId = uuidv7();
      const ops = [op.mkdir(uuidv7(), space.spaceId, "one")];
      expect((await a.commit(ops, { commitId })).seq).toBe(1n);
      expect(pushed).toEqual([1n]);
      const failed = await alarmState(space);
      expect(failed.alarm).toBeGreaterThan(failed.exportDue);
      if (repair === "retry") expect((await a.commit(ops, { commitId })).seq).toBe(1n);
      else expect((await a.commit([op.mkdir(uuidv7(), space.spaceId, "two")])).seq).toBe(2n);
      const repaired = await alarmState(space);
      expect(repaired.alarm).toBe(repaired.exportDue);
      vi.restoreAllMocks();
    }
  });

  it("sets the alarm for the earliest deadline when the object opens after an alarm write failed", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    await failNextAlarm(space);
    expect((await a.commit([op.mkdir(uuidv7(), space.spaceId, "one")])).seq).toBe(1n);
    vi.restoreAllMocks();
    const failed = await alarmState(space);
    expect(failed.alarm).toBeGreaterThan(failed.exportDue);
    await reopen(space);
    const opened = await alarmState(space);
    expect(opened.alarm).toBe(opened.exportDue);
    expect(opened.alarm).toBe(opened.due);
  });
});
