// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { OpResult_Code } from "../src/gen/koidrive/v1/commit_pb";
import { CommitRecordSchema, NodeStatus } from "../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { toHex, uuidv7 } from "../src/ids";
import type { SpaceDO } from "../src/space-do";
import { conflictName, nameKey } from "../src/space/names";
import { chunkKey } from "../src/space/store";
import { client, content, freezeClock, newSpace, op, storedObjects, type TestClient } from "./helpers";

const DAY = 24 * 3_600_000;

async function setup() {
  const space = await newSpace();
  const a = await client(space, "Laptop A");
  const b = await client(space, "Desktop B");
  return { ...space, a, b, root: space.spaceId };
}

async function createFile(c: TestClient, parentId: string, name: string, text: string) {
  const parts = await content(text);
  const nodeId = uuidv7();
  const result = await c.commit([op.create(nodeId, parentId, name, parts.content)], { receipts: await c.uploadParts(parts) });
  return { nodeId, result };
}

async function newContent(c: TestClient, text: string) {
  const parts = await content(text);
  return { content: parts.content, receipts: await c.uploadParts(parts) };
}

describe("Space DO commits", () => {
  it("lets two devices commit and see each other's changes", async () => {
    const { a, b, root } = await setup();
    const docs = uuidv7();
    const notes = await content("hello from A");
    const first = await a.commit([op.mkdir(docs, root, "Docs"), op.create(uuidv7(), docs, "a.txt", notes.content)], {
      receipts: await a.uploadParts(notes),
    });
    expect(first.seq).toBe(1n);
    expect(first.results.map((r) => r.code)).toEqual([OpResult_Code.APPLIED, OpResult_Code.APPLIED]);
    expect(first.changes).toHaveLength(2);

    b.baseSeq = 1n;
    const { result: second } = await createFile(b, docs, "b.txt", "hello from B");
    expect(second.seq).toBe(2n);

    for (const device of [a, b]) {
      const names = [...(await device.tree()).values()].map((node) => node.name).sort();
      expect(names).toEqual(["", "Docs", "a.txt", "b.txt"]);
    }
  });

  it("returns the stored result when a commit_id is retried", async () => {
    const { a, root } = await setup();
    const commitId = uuidv7();
    const ops = [op.mkdir(uuidv7(), root, "Once")];
    const first = await a.commit(ops, { commitId });
    expect(await a.commit(ops, { commitId })).toEqual(first);
    const other = await a.commitRaw([op.mkdir(uuidv7(), root, "Twice")], { commitId });
    expect(other.case === "error" && other.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    expect([...(await a.tree()).values()].map((node) => node.name)).not.toContain("Twice");
  });

  it("refuses a base_seq ahead of the server with AHEAD_OF_SERVER, and commits from viewers or old clients", async () => {
    const space = await setup();
    const ahead = await space.a.commitRaw([op.mkdir(uuidv7(), space.root, "X")], { baseSeq: 5n });
    expect(ahead.case === "error" && ahead.value.code).toBe(Error_Code.AHEAD_OF_SERVER);
    const viewer = await client(space, "Viewer", { role: "viewer" });
    const denied = await viewer.commitRaw([op.mkdir(uuidv7(), space.root, "X")]);
    expect(denied.case === "error" && denied.value.code).toBe(Error_Code.FORBIDDEN);
    const old = await client(space, "Old", { maxFormat: 0 });
    const readOnly = await old.commitRaw([op.mkdir(uuidv7(), space.root, "X")]);
    expect(readOnly.case === "error" && readOnly.value.code).toBe(Error_Code.READ_ONLY);
  });

  it("accepts a pending chunk only with a valid receipt or an object R2 holds under the row's incarnation", async () => {
    const { a, b, root } = await setup();
    const parts = await content("chunk bytes");
    const receipts = await a.uploadParts(parts);
    const [key] = await storedObjects(a.spaceId, parts.content.chunks[0]!.hash);
    await env.BUCKET.delete(key!);
    const missing = await a.commitRaw([op.create(uuidv7(), root, "f.bin", parts.content)]);
    expect(missing.case).toBe("error");
    if (missing.case === "error") {
      expect(missing.value.code).toBe(Error_Code.MISSING_CHUNKS);
      expect(missing.value.missingChunks.map(toHex)).toEqual([toHex(parts.content.chunks[0]!.hash)]);
    }
    const forged = await a.commitRaw([op.create(uuidv7(), root, "f.bin", parts.content)], {
      receipts: [{ ...receipts[0]!, token: receipts[0]!.token.slice(0, -2) + "AA" }],
    });
    expect(forged.case === "error" && forged.value.code).toBe(Error_Code.MISSING_CHUNKS);
    await env.BUCKET.put(key!, parts.bytes[0]!);
    await a.commit([op.create(uuidv7(), root, "f.bin", parts.content)]);

    const plan = await b.call({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks[0]!.action).toBe(ChunkPlan_Action.PRESENT);
    b.baseSeq = 1n;
    const reuse = await b.commit([op.create(uuidv7(), root, "copy.bin", parts.content)]);
    expect(reuse.results[0]!.code).toBe(OpResult_Code.APPLIED);
  });

  it("rejects malformed content", async () => {
    const { a, root } = await setup();
    const parts = await content("abc");
    parts.content.size = 4n;
    const result = await a.commitRaw([op.create(uuidv7(), root, "bad", parts.content)], { receipts: await a.uploadParts(parts) });
    expect(result.case === "error" && result.value.code).toBe(Error_Code.INVALID_ARGUMENT);
  });
});

describe("RFC 0001 name and move rules", () => {
  it("rejects .koidrive in any case directly under the root and allows it deeper", async () => {
    const { a, root } = await setup();
    const dir = uuidv7();
    const nested = uuidv7();
    const trashed = uuidv7();
    const parts = await content("x");
    const result = await a.commit(
      [
        op.mkdir(uuidv7(), root, ".koidrive"),
        op.create(uuidv7(), root, ".KoiDrive", parts.content),
        op.mkdir(dir, root, "dir"),
        op.mkdir(nested, dir, ".koidrive"),
        op.mkdir(trashed, root, "old"),
      ],
      { receipts: await a.uploadParts(parts) },
    );
    expect(result.results.map((r) => [r.code, r.rejectReason])).toEqual([
      [OpResult_Code.REJECTED, "INVALID_NAME"],
      [OpResult_Code.REJECTED, "INVALID_NAME"],
      [OpResult_Code.APPLIED, ""],
      [OpResult_Code.APPLIED, ""],
      [OpResult_Code.APPLIED, ""],
    ]);
    await a.commit([op.delete(trashed)]);
    const moved = await a.commit([op.move(nested, root, ".koidrive"), op.move(dir, root, ".KOIDRIVE"), op.restore(trashed, { name: ".koidrive" })]);
    expect(moved.results.map((r) => [r.code, r.rejectReason])).toEqual([
      [OpResult_Code.REJECTED, "INVALID_NAME"],
      [OpResult_Code.REJECTED, "INVALID_NAME"],
      [OpResult_Code.REJECTED, "INVALID_NAME"],
    ]);
    const tree = await a.tree();
    expect(tree.get(nested)).toMatchObject({ parentId: dir, name: ".koidrive" });
    expect(tree.get(dir)).toMatchObject({ parentId: root, name: "dir" });
    expect(tree.has(trashed)).toBe(false);
  });

  it("rejects a Kelvin-sign alias of .koidrive at the root", async () => {
    const { a, root } = await setup();
    const result = await a.commit([op.mkdir(uuidv7(), root, ".\u212Aoidrive")]);
    expect(result.results[0]).toMatchObject({ code: OpResult_Code.REJECTED, rejectReason: "INVALID_NAME" });
  });

  it("renames a stored reserved root name when R8 revives it", async () => {
    const space = await setup();
    const { a, root } = space;
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "legacy dir")]);
    const updated = await createFile(a, root, "legacy file", "v1");
    const restored = await createFile(a, root, "legacy restore", "v1");
    const firstVersion = restored.result.changes[0]!.change;
    const versionId = firstVersion.case === "node" ? firstVersion.value.current!.versionId : "";
    await a.commit([op.delete(dir), op.delete(updated.nodeId), op.delete(restored.nodeId)]);
    await runInDurableObject(space.stub, (_instance, state) => {
      for (const [id, name] of [[dir, ".KoiDrive"], [updated.nodeId, ".koidrive"], [restored.nodeId, ".\u212AOIDRIVE"]]) {
        state.storage.sql.exec("UPDATE nodes SET name = ?, name_key = ? WHERE node_id = ?", name, nameKey(name!), id);
      }
    });

    const parts = await content("v2");
    const result = await a.commit(
      [op.mkdir(uuidv7(), dir, "child"), op.update(updated.nodeId, parts.content), op.restore(restored.nodeId, { versionId })],
      { receipts: await a.uploadParts(parts) },
    );
    expect(result.results.map((r) => r.code)).toEqual([OpResult_Code.APPLIED, OpResult_Code.APPLIED_RENAMED, OpResult_Code.APPLIED_RENAMED]);
    const tree = await a.tree();
    expect(tree.get(dir)!.name).toMatch(/^\.KoiDrive \(conflict from Laptop A \d{4}-\d{2}-\d{2}\)$/);
    expect(tree.get(updated.nodeId)!.name).toMatch(/^\.koidrive \(conflict from Laptop A \d{4}-\d{2}-\d{2} 2\)$/);
    expect(tree.get(restored.nodeId)!.name).toMatch(/^\.KOIDRIVE \(conflict from Laptop A \d{4}-\d{2}-\d{2} 3\)$/);
    expect([...tree.values()].filter((n) => n.parentId === root && nameKey(n.name) === ".koidrive")).toEqual([]);
  });

  it("applies a no-op Move without a change record, so a concurrent delete still trashes the node", async () => {
    const { a, b, root } = await setup();
    const { nodeId } = await createFile(a, root, "notes.txt", "a");
    const metaRev = (await b.tree()).get(nodeId)!.metaRev;
    const noop = await b.commit([op.move(nodeId, root, "notes.txt")], { baseSeq: metaRev });
    expect(noop.results[0]).toMatchObject({ code: OpResult_Code.APPLIED, name: "notes.txt" });
    expect(noop.changes).toEqual([]);
    expect((await b.tree()).get(nodeId)!.metaRev).toBe(metaRev);

    const deleted = await a.commit([op.delete(nodeId)], { baseSeq: metaRev });
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const state = deleted.changes[0]!.change;
    expect(state.case === "node" && state.value.status).toBe(NodeStatus.TRASHED);
  });

  it("treats a case-only rename as a real move", async () => {
    const { a, root } = await setup();
    const { nodeId } = await createFile(a, root, "notes.txt", "a");
    const before = (await a.tree()).get(nodeId)!.metaRev;
    const renamed = await a.commit([op.move(nodeId, root, "Notes.txt")]);
    expect(renamed.changes).toHaveLength(1);
    expect((await a.tree()).get(nodeId)).toMatchObject({ name: "Notes.txt", metaRev: renamed.seq });
    expect(renamed.seq).toBeGreaterThan(before);
  });
});

describe("conflict rules", () => {
  it("R1: renames an incoming node whose name clashes case-insensitively", async () => {
    const { a, b, root, stub } = await setup();
    await runInDurableObject(stub, (instance: SpaceDO) => freezeClock(instance, Date.now()));
    await createFile(a, root, "Notes.txt", "a");
    const { nodeId, result } = await createFile(b, root, "notes.TXT", "b");
    const committedAt = Number((await b.tree()).get(nodeId)!.current!.createdAtMs);
    expect(result.results[0]).toMatchObject({
      code: OpResult_Code.APPLIED_RENAMED,
      name: conflictName("notes.TXT", "Desktop B", committedAt, 1),
    });
    expect(result.results[0]!.name).toMatch(/^notes \(conflict from Desktop B \d{4}-\d{2}-\d{2}\)\.TXT$/);

    const again = await createFile(b, root, "NOTES.txt", "c");
    expect(again.result.results[0]!.name).toMatch(/^NOTES \(conflict from Desktop B \d{4}-\d{2}-\d{2} 2\)\.txt$/);
  });

  it("keeps conflict names within 255 bytes after NFC, even when NFC lengthens the device name", async () => {
    const name = conflictName("x." + "e".repeat(200), "\u0A5E".repeat(40), Date.UTC(2026, 10, 10), 2);
    expect(name).toBe(name.normalize("NFC"));
    expect(new TextEncoder().encode(name).length).toBeLessThanOrEqual(255);
    expect(name.startsWith(" (conflict from " + "\u0A2B\u0A3C".repeat(21) + " 2026-11-10 2).")).toBe(true);
  });

  it("gives canonical caseless pairs one key and keeps compatibility and accent distinctions", () => {
    for (const [x, y] of [["\u0390", "\u03AA\u0301"], ["\u212A", "k"], ["\u00E9", "E\u0301"], ["Straße", "STRASSE"], ["a\u0315\u0300", "A\u0300\u0315"]]) {
      expect(nameKey(x!)).toBe(nameKey(y!));
    }
    for (const [x, y] of [["\u2460", "1"], ["\uFF21", "A"], ["\u00E9", "e"], ["\u0130", "I"], ["\u0131", "i"]]) {
      expect(nameKey(x!)).not.toBe(nameKey(y!));
    }
  });

  it("keeps the date and attempt in conflict names when NFC expands the device name past the budget", () => {
    const at = Date.UTC(2026, 10, 10);
    const device = "\u{1D160}".repeat(32);
    const names = [1, 2, 3].map((attempt) => conflictName("x.txt", device, at, attempt));
    expect(new Set(names).size).toBe(3);
    for (const name of names) {
      expect(name).toBe(name.normalize("NFC"));
      expect(new TextEncoder().encode(name).length).toBeLessThanOrEqual(255);
      expect(new TextEncoder().encode(name.slice(name.indexOf(" (conflict from ") + 16, name.lastIndexOf(" 2026-11-10"))).length).toBeLessThanOrEqual(128);
    }
    expect(names[2]).toMatch(/ 2026-11-10 3\)\.txt$/);
  });

  it("commits repeated renames and conflict copies from a device whose name NFC expands past the budget", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop A");
    const odd = await client(space, "\u{1D160}".repeat(32));
    const root = space.spaceId;
    const { nodeId } = await createFile(a, root, "x.txt", "original");
    const renamed = [];
    for (let i = 0; i < 3; i++) renamed.push((await createFile(odd, root, "x.txt", "copy " + i)).result.results[0]!);
    expect(renamed.map((r) => r.code)).toEqual([OpResult_Code.APPLIED_RENAMED, OpResult_Code.APPLIED_RENAMED, OpResult_Code.APPLIED_RENAMED]);
    expect(new Set(renamed.map((r) => r.name)).size).toBe(3);

    const base = odd.baseSeq;
    const update = await newContent(a, "newer");
    await a.commit([op.update(nodeId, update.content)], { receipts: update.receipts });
    const copies = [];
    for (let i = 0; i < 3; i++) {
      const stale = await newContent(odd, "stale " + i);
      copies.push((await odd.commit([op.update(nodeId, stale.content)], { baseSeq: base, receipts: stale.receipts })).results[0]!);
    }
    expect(copies.map((r) => r.code)).toEqual([OpResult_Code.CONFLICT_COPY, OpResult_Code.CONFLICT_COPY, OpResult_Code.CONFLICT_COPY]);
    expect(new Set([...renamed, ...copies].map((r) => r.name)).size).toBe(6);
  });

  it("R2: treats canonically equivalent names that differ only by case as one name", async () => {
    const { a, b, root } = await setup();
    const first = uuidv7();
    await a.commit([op.mkdir(first, root, "\u0390")]);
    const result = await b.commit([op.mkdir(uuidv7(), root, "\u03AA\u0301")]);
    expect(result.results[0]).toMatchObject({ code: OpResult_Code.MERGED_INTO, nodeId: first });
  });

  it("R2: merges a new directory into an existing one with the same name", async () => {
    const { a, b, root } = await setup();
    const photosA = uuidv7();
    await a.commit([op.mkdir(photosA, root, "Photos")]);
    const photosB = uuidv7();
    const fileId = uuidv7();
    const parts = await content("img");
    const result = await b.commit([op.mkdir(photosB, root, "photos"), op.create(fileId, photosB, "1.jpg", parts.content)], {
      receipts: await b.uploadParts(parts),
    });
    expect(result.results[0]).toMatchObject({ code: OpResult_Code.MERGED_INTO, nodeId: photosA, name: "Photos" });
    const tree = await b.tree();
    expect(tree.get(fileId)!.parentId).toBe(photosA);
    expect(tree.has(photosB)).toBe(false);
  });

  it("R3: keeps the first edit and turns a concurrent edit into a conflict copy", async () => {
    const { a, b, root } = await setup();
    const { nodeId } = await createFile(a, root, "report.docx", "v1");
    b.baseSeq = 1n;
    const fromB = await newContent(b, "v2 from B");
    await b.commit([op.update(nodeId, fromB.content)], { receipts: fromB.receipts });

    const fromA = await newContent(a, "v2 from A");
    const result = await a.commit([op.update(nodeId, fromA.content)], { baseSeq: 1n, receipts: fromA.receipts });
    const copy = result.results[0]!;
    expect(copy.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(copy.nodeId).not.toBe(nodeId);
    const tree = await a.tree();
    expect(copy.name).toBe(conflictName("report.docx", "Laptop A", Number(tree.get(copy.nodeId)!.current!.createdAtMs), 1));
    expect(tree.get(nodeId)!.current!.content!.manifestDigest).toEqual(fromB.content.manifestDigest);
    expect(tree.get(copy.nodeId)!.current!.content!.manifestDigest).toEqual(fromA.content.manifestDigest);
    expect(tree.get(copy.nodeId)!.parentId).toBe(root);
  });

  it("R3: does not treat a device's own earlier commit as unseen", async () => {
    const { a, root } = await setup();
    const { nodeId } = await createFile(a, root, "draft.md", "1");
    const second = await newContent(a, "2");
    await a.commit([op.update(nodeId, second.content)], { baseSeq: 1n, receipts: second.receipts });
    const third = await newContent(a, "3");
    const result = await a.commit([op.update(nodeId, third.content)], { baseSeq: 1n, receipts: third.receipts });
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
  });

  it("R4: applies an edit over an unseen rename", async () => {
    const { a, b, root } = await setup();
    const { nodeId } = await createFile(a, root, "old.txt", "1");
    b.baseSeq = 1n;
    await b.commit([op.move(nodeId, root, "new.txt")]);
    const edit = await newContent(a, "2");
    const result = await a.commit([op.update(nodeId, edit.content)], { baseSeq: 1n, receipts: edit.receipts });
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const node = (await a.tree()).get(nodeId)!;
    expect(node.name).toBe("new.txt");
    expect(node.current!.content!.manifestDigest).toEqual(edit.content.manifestDigest);
  });

  it("R5 and R6: applies moves in commit order and rejects the one that would make a cycle", async () => {
    const { a, b, root } = await setup();
    const x = uuidv7();
    const y = uuidv7();
    await a.commit([op.mkdir(x, root, "X"), op.mkdir(y, root, "Y")]);
    b.baseSeq = 1n;
    await a.commit([op.move(x, y, "X")]);
    const result = await b.commit([op.move(y, x, "Y")]);
    expect(result.results[0]).toMatchObject({ code: OpResult_Code.REJECTED, rejectReason: "CYCLE" });
    const tree = await a.tree();
    expect(tree.get(x)!.parentId).toBe(y);
    expect(tree.get(y)!.parentId).toBe(root);
  });

  it("R7: keeps nodes with unseen changes and their ancestors when deleting", async () => {
    const { a, b, root } = await setup();
    const dir = uuidv7();
    const sub = uuidv7();
    await a.commit([op.mkdir(dir, root, "Project"), op.mkdir(sub, dir, "src")]);
    const { nodeId: edited } = await createFile(a, sub, "main.rs", "fn main() {}");
    const { nodeId: untouched } = await createFile(a, dir, "README", "readme");
    b.baseSeq = a.baseSeq;
    const edit = await newContent(a, "fn main() { edited }");
    await a.commit([op.update(edited, edit.content)], { receipts: edit.receipts });

    const result = await b.commit([op.delete(dir)]);
    expect(result.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const tree = await b.tree();
    expect([dir, sub, edited].every((id) => tree.has(id))).toBe(true);
    expect(tree.has(untouched)).toBe(false);

    const trash = await b.call({ case: "listTrash", value: {} });
    expect(trash.case === "trash" && trash.value.nodes.map((n) => n.nodeId)).toEqual([untouched]);
  });

  it("R7: trashes a whole subtree under one trash group", async () => {
    const { a, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Old")]);
    const { nodeId: file } = await createFile(a, dir, "x", "x");
    const commitId = uuidv7();
    const result = await a.commit([op.delete(dir)], { commitId });
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const states = result.changes.map((c) => (c.change.case === "node" ? c.change.value : undefined)!);
    expect(states.map((s) => s.nodeId).sort()).toEqual([dir, file].sort());
    expect(new Set(states.map((s) => s.trashGroup))).toEqual(new Set([`${commitId}:0`]));
    expect(states.every((s) => s.status === NodeStatus.TRASHED)).toBe(true);
  });

  it("RFC 0012: each delete of a commit takes the trash group <commit_id>:<op index>", async () => {
    const { a, root } = await setup();
    const [first, second] = [uuidv7(), uuidv7()];
    await a.commit([op.mkdir(first, root, "First"), op.mkdir(second, root, "Second")]);
    const commitId = uuidv7();
    const result = await a.commit([op.delete(first), op.delete(second)], { commitId });
    const groups = new Map(result.changes.flatMap((c) => (c.change.case === "node" ? [[c.change.value.nodeId, c.change.value.trashGroup] as const] : [])));
    expect(groups).toEqual(new Map([[first, `${commitId}:0`], [second, `${commitId}:1`]]));
  });

  it("RFC 0031: a deletion that takes several commits trashes into one group, and its restore brings back every node", async () => {
    const { a, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Big")]);
    const files = [];
    for (const name of ["a", "b", "c"]) files.push((await createFile(a, dir, name, name)).nodeId);
    const first = uuidv7();
    await a.commit([op.delete(files[0]!), op.delete(files[1]!, `${first}:0`)], { commitId: first });
    const result = await a.commit([op.delete(dir, `${first}:0`)]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const groups = new Set(result.changes.flatMap((c) => (c.change.case === "node" ? [c.change.value.trashGroup] : [])));
    expect(groups).toEqual(new Set([`${first}:0`]));

    await a.commit([op.restore(dir)]);
    const tree = await a.tree();
    expect([dir, ...files].every((id) => tree.has(id))).toBe(true);
  });

  it("RFC 0031: a delete keeps its own group when the group it names is not one this device opened", async () => {
    const { a, b, root } = await setup();
    const opened = uuidv7();
    const first = uuidv7();
    await a.commit([op.mkdir(first, root, "First")]);
    await a.commit([op.delete(first)], { commitId: opened });
    const cases: [TestClient, (sent: string) => string][] = [
      [b, () => `${opened}:0`],
      [a, (sent) => `${sent}:1`],
      [a, (sent) => `${sent}:00`],
      [a, () => `${uuidv7()}:0`],
      [a, () => "forged"],
    ];
    for (const [device, named] of cases) {
      const dir = uuidv7();
      await a.commit([op.mkdir(dir, root, named("x"))]);
      device.baseSeq = a.baseSeq;
      const sent = uuidv7();
      const result = await device.commit([op.delete(dir, named(sent))], { commitId: sent });
      expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
      const groups = result.changes.flatMap((c) => (c.change.case === "node" ? [c.change.value.trashGroup] : []));
      expect(groups).toEqual([`${sent}:0`]);
      a.baseSeq = device.baseSeq;
    }
  });

  it("R8: an unseen edit revives a deleted file, and creating inside a trashed folder revives the folder", async () => {
    const { a, b, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Shared")]);
    const { nodeId: file } = await createFile(a, dir, "notes.txt", "1");
    b.baseSeq = a.baseSeq;
    await a.commit([op.delete(dir)]);

    const edit = await newContent(b, "edited on B");
    const revived = await b.commit([op.update(file, edit.content)], { receipts: edit.receipts });
    expect(revived.results[0]!.code).toBe(OpResult_Code.APPLIED);
    let tree = await b.tree();
    expect(tree.get(file)!.status).toBe(NodeStatus.LIVE);
    expect(tree.get(dir)!.status).toBe(NodeStatus.LIVE);

    await a.commit([op.delete(dir)], { baseSeq: b.baseSeq });
    const created = uuidv7();
    await b.commit([op.mkdir(created, dir, "new")], { baseSeq: b.baseSeq + 1n });
    tree = await b.tree();
    expect(tree.has(dir)).toBe(true);
    expect(tree.has(created)).toBe(true);
    expect(tree.has(file)).toBe(false);
  });

  it("R8: a commit that moves a folder out of the trash and deletes a member again leaves the member trashed", async () => {
    const { a, b, root } = await setup();
    const [outer, inner] = [uuidv7(), uuidv7()];
    await a.commit([op.mkdir(outer, root, "Outer"), op.mkdir(inner, outer, "Inner")]);
    const { nodeId: file } = await createFile(a, inner, "notes.txt", "1");
    b.baseSeq = a.baseSeq;
    await b.commit([op.delete(outer)]);
    a.baseSeq = b.baseSeq;
    const sent = await a.commit([
      op.move(outer, root, "Outer 2"),
      op.move(inner, outer, "Inner"),
      op.move(file, inner, "notes.txt"),
      op.delete(inner),
    ]);
    expect(sent.results.map((r) => r.code)).toEqual([OpResult_Code.APPLIED, OpResult_Code.APPLIED, OpResult_Code.APPLIED, OpResult_Code.APPLIED]);
    for (const device of [a, b]) {
      const tree = await device.tree();
      expect(tree.get(outer)?.status).toBe(NodeStatus.LIVE);
      expect(tree.get(outer)?.name).toBe("Outer 2");
      expect(tree.get(inner)?.status ?? NodeStatus.TRASHED).toBe(NodeStatus.TRASHED);
      expect(tree.get(file)?.status ?? NodeStatus.TRASHED).toBe(NodeStatus.TRASHED);
    }
  });

  it("R8: a commit that trashes a member again under require_head is refused when another device restored the member after its base", async () => {
    const { a, b, root } = await setup();
    const [outer, inner, keep] = [uuidv7(), uuidv7(), uuidv7()];
    await a.commit([op.mkdir(outer, root, "Outer"), op.mkdir(inner, outer, "Inner"), op.mkdir(keep, root, "Keep")]);
    const { nodeId: file } = await createFile(a, inner, "notes.txt", "1");
    b.baseSeq = a.baseSeq;
    await b.commit([op.delete(outer)]);
    const planned = b.baseSeq;
    await b.commit([op.restore(file, { targetParentId: keep })]);
    a.baseSeq = planned;
    const refused = await a.commitRaw(
      [op.move(outer, root, "Outer 2"), op.move(inner, outer, "Inner"), op.move(file, inner, "notes.txt"), op.delete(inner)],
      { requireHead: true },
    );
    expect(refused.case === "error" && refused.value.code).toBe(Error_Code.HEAD_MOVED);
    const tree = await b.tree();
    expect(tree.get(file)?.status).toBe(NodeStatus.LIVE);
    expect(tree.get(file)?.parentId).toBe(keep);
    expect(tree.get(outer)?.status ?? NodeStatus.TRASHED).toBe(NodeStatus.TRASHED);
  });

  it("R9 and R10: restores from the trash and restores an old version", async () => {
    const { a, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Album")]);
    const { nodeId: file, result: created } = await createFile(a, dir, "pic.png", "first");
    const firstVersion = created.changes.find((c) => c.change.case === "node" && c.change.value.nodeId === file)!;
    const firstVersionId = firstVersion.change.case === "node" ? firstVersion.change.value.current!.versionId : "";
    const second = await newContent(a, "second");
    await a.commit([op.update(file, second.content)], { receipts: second.receipts });

    await a.commit([op.delete(dir)]);
    const restored = await a.commit([op.restore(dir)]);
    expect(restored.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect([...(await a.tree()).keys()]).toEqual(expect.arrayContaining([dir, file]));

    const back = await a.commit([op.restore(file, { versionId: firstVersionId })]);
    expect(back.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const node = (await a.tree()).get(file)!;
    expect(node.current!.versionId).not.toBe(firstVersionId);
    expect(node.current!.content!.manifestDigest).toEqual((await content("first")).content.manifestDigest);

    const versions = await a.call({ case: "listVersions", value: { nodeId: file, limit: 2 } });
    expect(versions.case === "versions" && versions.value.versions.length).toBe(2);
    expect(versions.case === "versions" && versions.value.nextCursor).not.toBe("");
  });

  it("R11: rejects invalid ops without failing the commit", async () => {
    const { a, root } = await setup();
    const { nodeId: file } = await createFile(a, root, "file", "x");
    const dir = uuidv7();
    const result = await a.commit([
      op.move(root, root, "root"),
      op.delete(root),
      op.mkdir(uuidv7(), file, "under a file"),
      op.delete(uuidv7()),
      op.mkdir(file, root, "duplicate id"),
      op.mkdir(uuidv7(), root, ".."),
      op.mkdir(dir, root, "fine"),
    ]);
    expect(result.results.map((r) => r.rejectReason)).toEqual(["ROOT", "ROOT", "PARENT_NOT_DIR", "NOT_FOUND", "ID_EXISTS", "INVALID_NAME", ""]);
    expect(result.results.at(-1)!.code).toBe(OpResult_Code.APPLIED);
  });
});

describe("RFC 0023 deletion time and device in ListTrash", () => {
  function setClock(space: { stub: Awaited<ReturnType<typeof newSpace>>["stub"] }, atMs: number) {
    return runInDurableObject(space.stub, (instance: SpaceDO) => {
      freezeClock(instance, atMs);
    });
  }

  async function trash(c: TestClient) {
    const entries = new Map<string, { trashedAtMs: bigint; deviceId: string }>();
    let cursor = "";
    do {
      const page = await c.call({ case: "listTrash", value: { cursor, limit: 1 } });
      if (page.case !== "trash") throw new Error("listTrash failed");
      expect(page.value.trashed.map((t) => t.nodeId)).toEqual(page.value.nodes.map((n) => n.nodeId));
      for (const { nodeId, trashedAtMs, deviceId } of page.value.trashed) entries.set(nodeId, { trashedAtMs, deviceId });
      cursor = page.value.nextCursor;
    } while (cursor);
    return entries;
  }

  it("lists the commit time and the device that deleted a file, not the device that created it", async () => {
    const space = await setup();
    const { nodeId: file } = await createFile(space.b, space.root, "report.docx", "draft");
    space.a.baseSeq = space.b.baseSeq;
    const deleted = await space.a.commit([op.delete(file)]);
    expect(await trash(space.a)).toEqual(new Map([[file, { trashedAtMs: deleted.committedAtMs, deviceId: space.a.session.deviceId }]]));
  });

  it("lists every node of a deleted folder, and keeps the earlier deletion of a child trashed before it", async () => {
    const space = await setup();
    const { a, b, root } = space;
    const [dir, sub] = [uuidv7(), uuidv7()];
    await setClock(space, 500_000);
    await a.commit([op.mkdir(dir, root, "Project"), op.mkdir(sub, dir, "slides")]);
    const { nodeId: deck } = await createFile(a, sub, "pitch.key", "deck");
    const { nodeId: old } = await createFile(a, dir, "old.xlsx", "old");
    await setClock(space, 1_000_000);
    await a.commit([op.delete(old)]);
    b.baseSeq = a.baseSeq;
    await setClock(space, 2_000_000);
    await b.commit([op.delete(dir)]);
    const byB = { trashedAtMs: 2_000_000n, deviceId: b.session.deviceId };
    expect(await trash(a)).toEqual(
      new Map([
        [dir, byB],
        [sub, byB],
        [deck, byB],
        [old, { trashedAtMs: 1_000_000n, deviceId: a.session.deviceId }],
      ]),
    );
  });

  it("reports the second deletion of a node that was restored and deleted again", async () => {
    const space = await setup();
    const { a, b, root } = space;
    await setClock(space, 500_000);
    const { nodeId: file } = await createFile(a, root, "notes.pages", "notes");
    await setClock(space, 1_000_000);
    await a.commit([op.delete(file)]);
    await setClock(space, 2_000_000);
    await a.commit([op.restore(file)]);
    expect(await trash(a)).toEqual(new Map());
    b.baseSeq = a.baseSeq;
    await setClock(space, 3_000_000);
    await b.commit([op.delete(file)]);
    expect(await trash(a)).toEqual(new Map([[file, { trashedAtMs: 3_000_000n, deviceId: b.session.deviceId }]]));
  });
});

describe("names", () => {
  it("formats conflict names", () => {
    const at = Date.UTC(2026, 10, 9, 23, 59);
    expect(conflictName("a.tar.gz", "Mac/Book", at, 1)).toBe("a.tar (conflict from MacBook 2026-11-09).gz");
    expect(conflictName(".bashrc", "pc", at, 2)).toBe(".bashrc (conflict from pc 2026-11-09 2)");
    expect(conflictName("x", "d".repeat(40), at, 1)).toBe(`x (conflict from ${"d".repeat(32)} 2026-11-09)`);
    expect(new TextEncoder().encode(conflictName("가".repeat(85), "pc", at, 1)).length).toBeLessThanOrEqual(255);
  });

  it("treats names that differ only by Unicode case folding as the same", async () => {
    const { a, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "Straße")]);
    const result = await a.commit([op.mkdir(uuidv7(), root, "STRASSE")]);
    expect(result.results[0]!.code).toBe(OpResult_Code.MERGED_INTO);
    const nfd = await a.commit([op.mkdir(uuidv7(), root, "Cafe\u0301")]);
    expect(nfd.results[0]!.name).toBe("Café");
  });
});

describe("garbage collection", () => {
  async function gc(space: { stub: DurableObjectStub<SpaceDO> }, atMs: number) {
    return runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
  }

  async function stored(spaceId: string, hash: Uint8Array): Promise<boolean> {
    return (await storedObjects(spaceId, hash)).length > 0;
  }

  it("never deletes a pending chunk inside its hold, or a referenced chunk", async () => {
    const space = await setup();
    const kept = await content("referenced");
    await space.a.commit([op.create(uuidv7(), space.root, "kept", kept.content)], { receipts: await space.a.uploadParts(kept) });
    const pending = await content("uploaded, never committed");
    await space.a.uploadParts(pending);
    const keptHash = kept.content.chunks[0]!.hash;
    const pendingHash = pending.content.chunks[0]!.hash;

    const now = Date.now();
    expect(await gc(space, now + 23 * 3_600_000)).toMatchObject({ deletedChunks: 0 });
    expect(await stored(space.spaceId, pendingHash)).toBe(true);

    expect(await gc(space, now + DAY + 60_000)).toMatchObject({ deletedChunks: 1 });
    expect(await stored(space.spaceId, pendingHash)).toBe(false);
    expect(await stored(space.spaceId, keptHash)).toBe(true);

    expect(await gc(space, now + 3650 * DAY)).toMatchObject({ deletedChunks: 0 });
    expect(await stored(space.spaceId, keptHash)).toBe(true);
  });

  it("deletes a chunk only after version retention and the 30-day grace", async () => {
    const space = await setup();
    const { nodeId, result } = await createFile(space.a, space.root, "doc", "old bytes");
    const oldHash = (await content("old bytes")).content.chunks[0]!.hash;
    const next = await newContent(space.a, "new bytes");
    await space.a.commit([op.update(nodeId, next.content)], { receipts: next.receipts });
    expect(result.seq).toBe(1n);

    const now = Date.now();
    expect(await gc(space, now + 29 * DAY)).toMatchObject({ expiredVersions: 0, deletedChunks: 0 });
    const expiredAt = now + 31 * DAY;
    expect(await gc(space, expiredAt)).toMatchObject({ expiredVersions: 1, deletedChunks: 0 });
    expect(await gc(space, expiredAt + 29 * DAY)).toMatchObject({ deletedChunks: 0 });
    expect(await stored(space.spaceId, oldHash)).toBe(true);

    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE chunks SET hold_until_ms = ? WHERE hash = ?", expiredAt + 31 * DAY, oldHash);
    });
    expect(await gc(space, expiredAt + 30 * DAY + 1)).toMatchObject({ deletedChunks: 0 });
    expect(await gc(space, expiredAt + 31 * DAY + 1)).toMatchObject({ deletedChunks: 1 });
    expect(await stored(space.spaceId, oldHash)).toBe(false);
    expect(await stored(space.spaceId, next.content.chunks[0]!.hash)).toBe(true);
  });

  it("purges expired trash with a NodePurged commit", async () => {
    const space = await setup();
    const dir = uuidv7();
    await space.a.commit([op.mkdir(dir, space.root, "tmp")]);
    const { nodeId } = await createFile(space.a, dir, "scratch", "temporary");
    await space.a.commit([op.delete(dir)]);
    const report = await gc(space, Date.now() + 31 * DAY);
    expect(report.purgedNodes).toBe(2);
    const head = await runInDurableObject(space.stub, (_i, state) =>
      state.storage.sql.exec("SELECT seq, device_id, record FROM commits ORDER BY seq DESC LIMIT 1").one(),
    );
    expect({ seq: head.seq, device: head.device_id }).toEqual({ seq: 4, device: "" });
    const record = fromBinary(CommitRecordSchema, new Uint8Array(head.record as ArrayBuffer));
    expect(record.changes.map((c) => c.change.case === "purged" && c.change.value.nodeId)).toEqual([nodeId, dir]);
    const trash = await space.a.call({ case: "listTrash", value: {} });
    expect(trash.case === "trash" && trash.value.nodes).toEqual([]);
  });

  it("answers RETRY_LATER and MISSING_CHUNKS for a chunk being deleted", async () => {
    const space = await setup();
    const parts = await content("doomed");
    const receipts = await space.a.uploadParts(parts);
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE chunks SET state = 3");
    });
    const plan = await space.a.call({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    expect(plan.case === "prepareUpload" && plan.value.chunks[0]!.action).toBe(ChunkPlan_Action.RETRY_LATER);
    const commit = await space.a.commitRaw([op.create(uuidv7(), space.root, "f", parts.content)], { receipts });
    expect(commit.case === "error" && commit.value.code).toBe(Error_Code.MISSING_CHUNKS);
  });

  it("arms the GC alarm when the space is created", async () => {
    const space = await setup();
    const alarm = await runInDurableObject(space.stub, (_instance, state) => state.storage.getAlarm());
    expect(alarm).toBeGreaterThan(Date.now() + DAY - 60_000);
  });
});
