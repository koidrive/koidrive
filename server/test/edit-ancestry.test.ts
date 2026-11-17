// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { CommitSchema, type CreateFile, OpResult_Code, OpSchema, type UpdateContent } from "../src/gen/koidrive/v1/commit_pb";
import { CommitRecordSchema, type Content } from "../src/gen/koidrive/v1/model_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import { commitDigest, type DigestVersion } from "../src/space/digest";
import { restoreFromR2 } from "./export-reader";
import { client, content, newSpace, op, runSpaceAlarm } from "./helpers";

function update(nodeId: string, value: Content, ancestry: Partial<Pick<UpdateContent, "madeOver" | "sourceVersionId" | "resend">> = {}) {
  return create(OpSchema, { op: { case: "updateContent", value: { nodeId, content: value, ...ancestry } } });
}

function createFile(nodeId: string, parentId: string, name: string, value: Content, ancestry: Partial<Pick<CreateFile, "madeOver" | "sourceVersionId">> = {}) {
  return create(OpSchema, { op: { case: "createFile", value: { nodeId, parentId, name, content: value, ...ancestry } } });
}

async function setup() {
  const space = await newSpace();
  const a = await client(space, "Author A");
  const b = await client(space, "Editor B");
  const values = await Promise.all([content("W1"), content("W2"), content("W3"), content("W4")]);
  const receipts = [];
  for (const value of values) receipts.push(...await a.uploadParts(value));
  const file = uuidv7();
  await a.commit([op.create(file, space.spaceId, "f.txt", values[0]!.content)], { receipts });
  const first = (await a.tree()).get(file)!.current!;
  return { ...space, a, b, file, first, values: values.map((value) => value.content) };
}

describe("RFC 0026 edit ancestry", () => {
  it("creates a user's edit made over a lost W2 and supersedes a later resend of W2", async () => {
    const { a, b, values, spaceId } = await setup();
    const file = uuidv7();
    const lostW2 = uuidv7();
    const result = await a.commit([createFile(file, spaceId, "after-loss.txt", values[2]!, { madeOver: [lostW2] })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([lostW2]);
    const resend = await b.commit([update(file, values[1]!, { resend: true, sourceVersionId: lostW2 })]);
    expect(resend.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(resend.changes).toEqual([]);
  });

  it("carries a resend-create's source alias through an edit over the recreated version", async () => {
    const { a, b, values, spaceId } = await setup();
    const [file, source, base] = [uuidv7(), uuidv7(), uuidv7()];
    await a.commit([createFile(file, spaceId, "recreated.txt", values[1]!, { sourceVersionId: source, madeOver: [base, source] })]);
    const recreated = (await a.tree()).get(file)!.current!;
    expect(recreated.madeOver).toEqual([source, base]);
    await b.commit([update(file, values[2]!, { madeOver: [recreated.versionId] })]);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([recreated.versionId, source, base]);
    const resend = await a.commit([update(file, values[1]!, { resend: true, sourceVersionId: source })]);
    expect(resend.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
  });

  it("old client on new server: a create beside independent W3 keeps its renamed copy without ancestry", async () => {
    const { a: oldClient, b, file, first, values, spaceId } = await setup();
    await b.commit([update(file, values[2]!, { madeOver: [first.versionId] })]);
    const copy = uuidv7();
    const result = await oldClient.commit([op.create(copy, spaceId, "f.txt", values[1]!)]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED_RENAMED);
    const tree = await oldClient.tree();
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[2]!.manifestDigest);
    expect(tree.get(copy)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
    expect(tree.get(copy)!.current!.madeOver).toEqual([]);
  });

  it("bounds a create's source alias and bases to 64 first-occurrence IDs", async () => {
    const { a, values, spaceId } = await setup();
    const [file, source] = [uuidv7(), uuidv7()];
    const ids = Array.from({ length: 80 }, () => uuidv7());
    await a.commit([createFile(file, spaceId, "bounded.txt", values[1]!, { sourceVersionId: source, madeOver: ids.flatMap((id) => [id, id]) })]);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([source, ...ids.slice(0, 63)]);
  });

  it("applies an edit made over the unseen current version without a conflict copy", async () => {
    const { a, b, file, first, values } = await setup();
    const result = await b.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const tree = await a.tree();
    expect(tree.size).toBe(2);
    expect(tree.get(file)!.current!.madeOver).toEqual([first.versionId]);
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
    expect(result.changes[0]!.change).toMatchObject({ case: "node", value: { current: { madeOver: [first.versionId] } } });
  });

  it("supersedes a resend by its source value even when the resend has no made_over bases", async () => {
    const { a, b, file, first, values } = await setup();
    await b.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const before = (await a.tree()).get(file)!;
    const result = await a.commit([update(file, values[0]!, { resend: true, sourceVersionId: first.versionId })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(result.changes).toEqual([]);
    expect((await a.tree()).get(file)).toEqual(before);
    expect(await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("SELECT count(*) AS count FROM versions").one().count)).toBe(2);
  });

  it("carries resend source identity through a later edit made over the replayed version", async () => {
    const { a, b, file, first, values } = await setup();
    const lostSource = uuidv7();
    await a.commit([update(file, values[1]!, { resend: true, sourceVersionId: lostSource, madeOver: [first.versionId] })]);
    const replayed = (await a.tree()).get(file)!.current!;
    expect(replayed.madeOver).toEqual([lostSource, first.versionId]);
    await b.commit([update(file, values[2]!, { madeOver: [replayed.versionId] })]);
    const result = await a.commit([update(file, values[1]!, { resend: true, sourceVersionId: lostSource })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(result.changes).toEqual([]);
    expect((await a.tree()).size).toBe(2);
  });

  it("keeps a resend's source alias on its conflict copy without changing the target's ancestry", async () => {
    const { a, b, file, first, values } = await setup();
    await b.commit([update(file, values[2]!, { madeOver: [first.versionId] })]);
    const before = (await a.tree()).get(file)!;
    const source = uuidv7();
    const result = await a.commit([update(file, values[1]!, { resend: true, sourceVersionId: source })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await a.tree();
    expect(tree.get(file)).toEqual(before);
    expect(tree.get(result.results[0]!.nodeId)!.current!.madeOver).toEqual([source]);
  });

  it("caps a resend's source alias together with its bases and does not interpret the alias as an overwrite", async () => {
    const { a, b, file, first, values } = await setup();
    const ids = Array.from({ length: 90 }, () => uuidv7());
    const result = await b.commit([update(file, values[1]!, { resend: true, sourceVersionId: first.versionId, madeOver: ids })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect((await a.tree()).get(result.results[0]!.nodeId)!.current!.madeOver).toEqual([first.versionId, ...ids.slice(0, 63)]);
  });

  it("does not store a source alias on a non-resend edit", async () => {
    const { a, file, first, values } = await setup();
    await a.commit([update(file, values[1]!, { sourceVersionId: uuidv7(), madeOver: [first.versionId] })]);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([first.versionId]);
  });

  it("records a same-content resend's source alias without advancing content or edit revisions", async () => {
    const { a, b, file, first, values, spaceId } = await setup();
    const source = uuidv7();
    const result = await b.commit([update(file, values[0]!, { resend: true, sourceVersionId: source })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const after = (await a.tree()).get(file)!;
    expect(after.contentRev).toBe(1n);
    expect(after.editRev).toBe(1n);
    expect(after.current!.versionId).toBe(first.versionId);
    expect(after.current!.madeOver).toEqual([source]);
    expect(result.changes[0]!.change).toMatchObject({ case: "node", value: { current: { madeOver: [source] } } });
    expect(await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("SELECT count(*) AS count FROM versions").one().count)).toBe(1);
    await runSpaceAlarm(a.stub, Date.now() + 5 * 60 * 1000);
    expect((await restoreFromR2(spaceId)).nodes.get(file)!.state.current!.madeOver).toEqual([source]);
    await a.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const resend = await b.commit([update(file, values[0]!, { resend: true, sourceVersionId: source })]);
    expect(resend.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(resend.changes).toEqual([]);
    expect((await a.tree()).size).toBe(2);
  });

  it("bounds repeated same-content source aliases and leaves Superseded's current chain unchanged", async () => {
    const { a, b, file, values } = await setup();
    const sources = Array.from({ length: 70 }, () => uuidv7());
    for (const source of sources) {
      const result = await b.commit([update(file, values[0]!, { resend: true, sourceVersionId: source })], { baseSeq: 0n });
      expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    }
    const before = (await a.tree()).get(file)!;
    expect(before.current!.madeOver).toEqual([...sources].reverse().slice(0, 64));
    expect(before.contentRev).toBe(1n);
    expect(before.editRev).toBe(1n);
    const duplicate = await b.commit([update(file, values[0]!, { resend: true, sourceVersionId: sources[69]!, madeOver: [uuidv7()] })]);
    expect(duplicate.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(duplicate.changes).toEqual([]);
    expect((await a.tree()).get(file)).toEqual(before);
  });

  it("keeps the legacy copy for a resend without a source ID despite ancestry on the current version", async () => {
    const { a, b, file, first, values } = await setup();
    await b.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const result = await a.commit([update(file, values[0]!, { resend: true })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await a.tree();
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
    expect(tree.get(result.results[0]!.nodeId)!.current!.content!.manifestDigest).toEqual(values[0]!.manifestDigest);
  });

  it("old client on new server: an edit over W2 without ancestry keeps the legacy copy and loses no write", async () => {
    const { a: oldClient, b, file, first, values } = await setup();
    await b.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const result = await oldClient.commit([op.update(file, values[2]!)]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await oldClient.tree();
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
    expect(tree.get(result.results[0]!.nodeId)!.current!.content!.manifestDigest).toEqual(values[2]!.manifestDigest);
    expect(tree.get(result.results[0]!.nodeId)!.current!.madeOver).toEqual([]);
  });

  it("old client on new server: resending lost W2 beside an independent W3 keeps both writes as a legacy copy", async () => {
    const { a: oldClient, b, file, first, values } = await setup();
    await b.commit([update(file, values[2]!, { madeOver: [first.versionId] })]);
    const result = await oldClient.commit([update(file, values[1]!, { resend: true })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await oldClient.tree();
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[2]!.manifestDigest);
    expect(tree.get(result.results[0]!.nodeId)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
  });

  it("keeps an unrelated lost source value beside W3 when W3 was not made over it", async () => {
    const { a, b, file, first, values } = await setup();
    await b.commit([update(file, values[2]!, { madeOver: [first.versionId] })]);
    const result = await a.commit([update(file, values[1]!, { resend: true, sourceVersionId: uuidv7() })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await a.tree();
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[2]!.manifestDigest);
    expect(tree.get(result.results[0]!.nodeId)!.current!.content!.manifestDigest).toEqual(values[1]!.manifestDigest);
  });

  it("preserves the current version's ancestry when an unseen equal-content edit creates no version", async () => {
    const { a, b, file, first, values } = await setup();
    const result = await b.commit([update(file, values[0]!, { madeOver: [first.versionId, uuidv7()] })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(result.changes).toEqual([]);
    expect((await a.tree()).get(file)!.current).toEqual(first);
  });

  it("expands a retained same-file version's ancestry over the current version after a rollback", async () => {
    const { a, b, file, first, values } = await setup();
    await a.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const second = (await a.tree()).get(file)!.current!;
    await a.commit([update(file, values[2]!, { madeOver: [second.versionId] })]);
    const third = (await a.tree()).get(file)!.current!;
    await runInDurableObject(a.stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE nodes SET cur_version_id = ?, content_rev = ? WHERE node_id = ?", second.versionId, Number(second.seq), file);
    });
    const result = await b.commit([update(file, values[3]!, { madeOver: [third.versionId] })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([third.versionId, second.versionId, first.versionId]);
  });

  it("retains unknown IDs but does not expand the ancestry of another file", async () => {
    const { a, b, file, first, values, spaceId } = await setup();
    const other = uuidv7();
    await a.commit([op.create(other, spaceId, "other.txt", values[0]!)]);
    await a.commit([update(other, values[1]!, { madeOver: [first.versionId] })]);
    const otherVersion = (await a.tree()).get(other)!.current!;
    const unknown = uuidv7();
    const result = await b.commit([update(file, values[2]!, { madeOver: [unknown, otherVersion.versionId, unknown] })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect((await a.tree()).get(result.results[0]!.nodeId)!.current!.madeOver).toEqual([unknown, otherVersion.versionId]);
    expect((await a.tree()).get(file)!.current!.versionId).toBe(first.versionId);
  });

  it("deduplicates first occurrences and expands known ancestry depth-first in place", async () => {
    const { a, file, first, values } = await setup();
    await a.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const second = (await a.tree()).get(file)!.current!;
    const [newest, oldest] = [uuidv7(), uuidv7()];
    await a.commit([update(file, values[2]!, { madeOver: [newest, second.versionId, oldest, first.versionId, newest] })]);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual([newest, second.versionId, first.versionId, oldest]);
  });

  it("bounds every stored version, current NodeState, change record, and version listing across 80 edits", async () => {
    const { a, file, first, values, spaceId } = await setup();
    const versions = [first.versionId];
    for (let index = 0; index < 80; index++) {
      const result = await a.commit([update(file, values[index % values.length]!, { madeOver: [versions[versions.length - 1]!] })]);
      const change = result.changes[0]!.change;
      expect(change.case).toBe("node");
      if (change.case !== "node") throw new Error("missing node change");
      expect(change.value.current!.madeOver).toEqual([...versions].reverse().slice(0, 64));
      versions.push(change.value.current!.versionId);
    }
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual(versions.slice(0, -1).reverse().slice(0, 64));
    const stored = await runInDurableObject(a.stub, (_instance, state) => ({
      versions: state.storage.sql.exec("SELECT made_over FROM versions WHERE node_id = ?", file).toArray(),
      records: state.storage.sql.exec("SELECT record FROM commits ORDER BY seq").toArray(),
    }));
    expect(stored.versions).toHaveLength(81);
    for (const row of stored.versions) {
      const chain = String(row.made_over);
      const ids = chain === "" ? [] : chain.split(",");
      expect(chain.length).toBeLessThanOrEqual(64 * 37 - 1);
      expect(ids.length).toBeLessThanOrEqual(64);
      expect(new Set(ids).size).toBe(ids.length);
    }
    for (const row of stored.records) {
      if (!(row.record instanceof ArrayBuffer)) throw new Error("missing commit record bytes");
      const record = fromBinary(CommitRecordSchema, new Uint8Array(row.record));
      for (const { change } of record.changes) {
        if (change.case === "node") expect(change.value.current?.madeOver.length ?? 0).toBeLessThanOrEqual(64);
      }
    }
    const listed = await a.call({ case: "listVersions", value: { nodeId: file, limit: 100 } });
    expect(listed.case).toBe("versions");
    if (listed.case !== "versions") throw new Error("missing version listing");
    expect(listed.value.versions).toHaveLength(81);
    for (const version of listed.value.versions) expect(version.madeOver.length).toBeLessThanOrEqual(64);
    await runSpaceAlarm(a.stub, Date.now() + 24 * 60 * 60 * 1000 + 5 * 60 * 1000);
    const restored = (await restoreFromR2(spaceId)).nodes.get(file)!;
    expect(restored.state.current!.madeOver).toEqual(versions.slice(0, -1).reverse().slice(0, 64));
    expect(restored.previous.size).toBe(80);
    for (const { version } of restored.previous.values()) expect(version.madeOver.length).toBeLessThanOrEqual(64);
  });

  it("caps oversized input after first-occurrence dedup without growing the stored chain", async () => {
    const { a, file, values } = await setup();
    const ids = Array.from({ length: 90 }, () => uuidv7());
    await a.commit([update(file, values[1]!, { madeOver: ids.flatMap((id) => [id, id]) })]);
    expect((await a.tree()).get(file)!.current!.madeOver).toEqual(ids.slice(0, 64));
  });

  it("keeps both writes when ancestry expansion pushes the current version past the 64-ID bound", async () => {
    const { a, b, file, first, values } = await setup();
    await a.commit([update(file, values[1]!, { madeOver: [first.versionId] })]);
    const second = (await a.tree()).get(file)!.current!;
    await a.commit([update(file, values[2]!, { madeOver: [second.versionId] })]);
    const before = (await a.tree()).get(file)!;
    const current = before.current!;
    const unknown = Array.from({ length: 62 }, () => uuidv7());
    expect(new Set(unknown).size).toBe(62);
    const madeOver = [second.versionId, ...unknown, current.versionId];
    expect(madeOver).toHaveLength(64);
    const result = await b.commit([update(file, values[3]!, { madeOver })]);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const tree = await a.tree();
    expect(tree.size).toBe(3);
    expect(tree.get(file)).toEqual(before);
    expect(tree.get(file)!.current!.content!.manifestDigest).toEqual(values[2]!.manifestDigest);
    const copy = tree.get(result.results[0]!.nodeId)!.current!;
    expect(copy.content!.manifestDigest).toEqual(values[3]!.manifestDigest);
    expect(copy.content!.manifestDigest).not.toEqual(current.content!.manifestDigest);
    const expected = [second.versionId, first.versionId, ...unknown];
    expect(copy.madeOver).toEqual(expected);
    expect(copy.madeOver).toHaveLength(64);
    expect(copy.madeOver).not.toContain(current.versionId);
    const stored = await runInDurableObject(a.stub, (_instance, state) =>
      state.storage.sql.exec("SELECT made_over FROM versions WHERE version_id = ?", copy.versionId).one(),
    );
    const storedChain = String(stored.made_over).split(",");
    expect(storedChain).toEqual(expected);
    expect(storedChain).toHaveLength(64);
    expect(storedChain).not.toContain(current.versionId);
  });

  it("upgrades existing versions with empty ancestry and keeps their content across restarts", async () => {
    const { a, file, first } = await setup();
    await runInDurableObject(a.stub, (_instance, state) => {
      state.storage.sql.exec("ALTER TABLE versions DROP COLUMN made_over");
      state.storage.sql.exec("UPDATE meta SET value = 10 WHERE key = 'schema_version'");
    });
    for (let restart = 0; restart < 2; restart++) {
      await evictDurableObject(a.stub);
      expect((await a.tree()).get(file)!.current).toEqual(first);
      expect((await a.tree()).get(file)!.current!.madeOver).toEqual([]);
    }
  });

  it("rejects malformed ancestry IDs at the request boundary", async () => {
    const { a, file, values } = await setup();
    for (const ancestry of [{ madeOver: ["bad-id"] }, { sourceVersionId: "bad-id", resend: true }]) {
      const result = await a.commitRaw([update(file, values[1]!, ancestry)]);
      expect(result.case).toBe("error");
      if (result.case === "error") expect(result.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    }
    expect((await a.tree()).size).toBe(2);
  });

  it("adds ancestry to digest v4 while preserving earlier projections and the original request order", async () => {
    const value = (await content("W2")).content;
    const [nodeId, first, second] = [uuidv7(), uuidv7(), uuidv7()];
    const request = (ancestry: Parameters<typeof update>[2]) => create(CommitSchema, { ops: [update(nodeId, value, ancestry)] });
    const plain = request({});
    const original = request({ madeOver: [first, second], sourceVersionId: first });
    for (const version of [1, 2, 3] satisfies DigestVersion[]) {
      expect(await commitDigest(original, version)).toEqual(await commitDigest(plain, version));
    }
    expect(await commitDigest(plain, 4)).toEqual(await commitDigest(plain, 3));
    const digest = await commitDigest(original);
    for (const changed of [
      request({ madeOver: [second, first], sourceVersionId: first }),
      request({ madeOver: [first, second, first], sourceVersionId: first }),
      request({ madeOver: [first, second], sourceVersionId: second }),
      plain,
    ]) expect(await commitDigest(changed)).not.toEqual(digest);
  });

  it("includes both CreateFile ancestry fields in digest v4 without changing older projections", async () => {
    const value = (await content("W2")).content;
    const [file, parent, source, base] = [uuidv7(), uuidv7(), uuidv7(), uuidv7()];
    const request = (ancestry: Parameters<typeof createFile>[4]) => create(CommitSchema, { ops: [createFile(file, parent, "f.txt", value, ancestry)] });
    const plain = request({});
    const inherited = request({ madeOver: [base] });
    const replayed = request({ sourceVersionId: source });
    for (const version of [1, 2, 3] satisfies DigestVersion[]) {
      expect(await commitDigest(inherited, version)).toEqual(await commitDigest(plain, version));
      expect(await commitDigest(replayed, version)).toEqual(await commitDigest(plain, version));
    }
    expect(await commitDigest(plain, 4)).toEqual(await commitDigest(plain, 3));
    expect(await commitDigest(inherited)).not.toEqual(await commitDigest(plain));
    expect(await commitDigest(replayed)).not.toEqual(await commitDigest(plain));
  });

  for (const version of [1, 2, 3, 4] satisfies DigestVersion[]) {
    it(`checks ancestry-only commit reuse with stored digest v${version}, including after log compaction`, async () => {
      const { a, file, first, values } = await setup();
      const original = update(file, values[1]!, version === 4 ? { madeOver: [first.versionId], sourceVersionId: first.versionId } : {});
      const commitId = uuidv7();
      const result = await a.commit([original], { commitId });
      const digest = await commitDigest(create(CommitSchema, { baseSeq: 1n, deviceSeq: 2n, ops: [original] }), version);
      await runInDurableObject(a.stub, (_instance, state) => {
        for (const table of ["commits", "device_commits"]) {
          state.storage.sql.exec(`UPDATE ${table} SET request_digest = ?, digest_version = ? WHERE commit_id = ?`, digest, version, commitId);
        }
      });
      for (const compacted of [false, true]) {
        if (compacted) await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM commits WHERE commit_id = ?", commitId));
        for (const ancestry of [{ madeOver: [uuidv7()] }, { sourceVersionId: uuidv7() }]) {
          const retry = await a.commitRaw([update(file, values[1]!, ancestry)], { commitId });
          expect(retry.case).toBe("error");
          if (retry.case === "error") expect(retry.value.code).toBe(Error_Code.INVALID_ARGUMENT);
        }
        expect(await a.commit([original], { commitId })).toEqual(result);
      }
    });

    it(`rejects new CreateFile ancestry on a reused commit with stored digest v${version}`, async () => {
      const { a, values, spaceId } = await setup();
      const file = uuidv7();
      const original = createFile(file, spaceId, "created.txt", values[1]!);
      const commitId = uuidv7();
      const result = await a.commit([original], { commitId });
      const digest = await commitDigest(create(CommitSchema, { baseSeq: 1n, deviceSeq: 2n, ops: [original] }), version);
      await runInDurableObject(a.stub, (_instance, state) => {
        for (const table of ["commits", "device_commits"]) {
          state.storage.sql.exec(`UPDATE ${table} SET request_digest = ?, digest_version = ? WHERE commit_id = ?`, digest, version, commitId);
        }
      });
      for (const compacted of [false, true]) {
        if (compacted) await runInDurableObject(a.stub, (_instance, state) => state.storage.sql.exec("DELETE FROM commits WHERE commit_id = ?", commitId));
        for (const ancestry of [{ madeOver: [uuidv7()] }, { sourceVersionId: uuidv7() }]) {
          const retry = await a.commitRaw([createFile(file, spaceId, "created.txt", values[1]!, ancestry)], { commitId });
          expect(retry.case).toBe("error");
          if (retry.case === "error") expect(retry.value.code).toBe(Error_Code.INVALID_ARGUMENT);
        }
        expect(await a.commit([original], { commitId })).toEqual(result);
      }
    });
  }
});
