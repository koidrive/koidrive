// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { type CommitResult, CommitSchema, type Op, OpResult_Code, OpSchema } from "../src/gen/koidrive/v1/commit_pb";
import { type ChangeRecord, type NodeState } from "../src/gen/koidrive/v1/model_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { configuredFeatures } from "../src/features";
import { toHex, uuidv7 } from "../src/ids";
import { SpaceDO } from "../src/space-do";
import { Close } from "../src/space/session";
import { commitDigest } from "../src/space/digest";
import { restoreFromR2, serverState } from "./export-reader";
import { client, content, freezeClock, newSpace, op, runSpaceAlarm, type TestClient } from "./helpers";
import { createSpace, getOwner, pairDevice, prepared, Socket } from "./session";

type Stub = DurableObjectStub<SpaceDO>;

const mark = (nodeId: string, tracked = true): Op => create(OpSchema, { op: { case: "markGitTracked", value: { nodeId, tracked } } });

function stateOf(changes: readonly ChangeRecord[], nodeId: string): NodeState | undefined {
  for (const { change } of changes) if (change.case === "node" && change.value.nodeId === nodeId) return change.value;
  return undefined;
}

function revs({ contentRev, metaRev, editRev, metaEditRev }: NodeState) {
  return { contentRev, metaRev, editRev, metaEditRev };
}

async function createFile(device: TestClient, parentId: string, name: string): Promise<{ nodeId: string; result: CommitResult }> {
  const parts = await content(name + " " + uuidv7());
  const nodeId = uuidv7();
  const result = await device.commit([op.create(nodeId, parentId, name, parts.content)], { receipts: await device.uploadParts(parts) });
  return { nodeId, result };
}

async function withFeatures<T>(stub: Stub, variable: string, run: () => Promise<T>): Promise<T> {
  const prototype = await runInDurableObject(stub, (instance: SpaceDO) => Object.getPrototypeOf(instance) as { features(): ReadonlySet<string> });
  const original = prototype.features;
  prototype.features = () => configuredFeatures(variable);
  try {
    return await run();
  } finally {
    prototype.features = original;
  }
}

function start(stub: Stub) {
  return runInDurableObject(stub, (_instance, state) => {
    new SpaceDO(state, env);
  });
}

describe("RFC 0033 section 2: MarkGitTracked", () => {
  it("sets the mark of a live file and changes only git_tracked and git_tracked_rev, with no record for an equal value", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const { nodeId, result: created } = await createFile(a, space.spaceId, "dist.js");
    const before = stateOf(created.changes, nodeId)!;
    expect(before).toMatchObject({ gitTracked: false, gitTrackedRev: 0n });

    const marked = await a.commit([mark(nodeId)]);
    expect(marked.results[0]).toMatchObject({ code: OpResult_Code.APPLIED, name: "dist.js" });
    const after = stateOf(marked.changes, nodeId)!;
    expect(after).toMatchObject({ gitTracked: true, gitTrackedRev: marked.seq, ...revs(before) });
    expect(after.intents).toEqual(before.intents);

    const again = await a.commit([mark(nodeId)]);
    expect(again.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(again.changes).toEqual([]);

    const cleared = await a.commit([mark(nodeId, false)]);
    expect(stateOf(cleared.changes, nodeId)).toMatchObject({ gitTracked: false, gitTrackedRev: cleared.seq });
  });

  it("answers REJECTED for a directory and SUPERSEDED for a trashed or purged file", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, space.spaceId, "build")]);
    const trashed = (await createFile(a, space.spaceId, "trashed.js")).nodeId;
    const purged = (await createFile(a, space.spaceId, "purged.js")).nodeId;
    await a.commit([op.delete(trashed), op.delete(purged)]);
    await runInDurableObject(space.stub, async (instance: SpaceDO, state) => {
      state.storage.sql.exec("UPDATE nodes SET trashed_at_ms = 0 WHERE node_id = ?", purged);
      await instance.collectGarbage(Date.now());
    });

    const result = await a.commit([mark(dir), mark(space.spaceId), mark(trashed), mark(purged)]);
    expect(result.results.map(({ code, rejectReason }) => [code, rejectReason])).toEqual([
      [OpResult_Code.REJECTED, "NOT_FILE"],
      [OpResult_Code.REJECTED, "NOT_FILE"],
      [OpResult_Code.SUPERSEDED, ""],
      [OpResult_Code.SUPERSEDED, ""],
    ]);
    expect(result.changes).toEqual([]);
  });

  it("keeps a later no-op Move a non-move and a Delete whose requires names the node applying, and other ops keep the mark", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const b = await client(space, "Desktop");
    const { nodeId, result: created } = await createFile(a, space.spaceId, "app.js");
    const before = stateOf(created.changes, nodeId)!;
    const marked = await b.commit([mark(nodeId)], { baseSeq: created.seq });

    const moved = await a.commit([op.move(nodeId, space.spaceId, "app.js")], { baseSeq: created.seq });
    expect(moved.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(moved.changes).toEqual([]);

    const requires = { nodeId, contentRev: before.contentRev, metaRev: before.metaRev };
    const deleted = await a.commit([create(OpSchema, { op: { case: "delete", value: { nodeId, requires } } })], { baseSeq: created.seq });
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(stateOf(deleted.changes, nodeId)).toMatchObject({ gitTracked: true, gitTrackedRev: marked.seq });

    const restored = await a.commit([op.restore(nodeId)]);
    const renamed = await a.commit([op.move(nodeId, space.spaceId, "main.js")]);
    const parts = await content("edited " + uuidv7());
    const edited = await a.commit([op.update(nodeId, parts.content)], { receipts: await a.uploadParts(parts) });
    for (const result of [restored, renamed, edited]) expect(stateOf(result.changes, nodeId)).toMatchObject({ gitTracked: true, gitTrackedRev: marked.seq });
  });

  it("starts a conflict copy of a marked file with the mark at the commit's seq, and a new file without it", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const b = await client(space, "Desktop");
    const { nodeId, result: created } = await createFile(a, space.spaceId, "app.js");
    const marked = await b.commit([mark(nodeId)], { baseSeq: created.seq });
    const theirs = await content("theirs " + uuidv7());
    await b.commit([op.update(nodeId, theirs.content)], { receipts: await b.uploadParts(theirs) });

    const ours = await content("ours " + uuidv7());
    const conflicted = await a.commit([op.update(nodeId, ours.content)], { baseSeq: marked.seq, receipts: await a.uploadParts(ours) });
    const copy = conflicted.results[0]!;
    expect(copy.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(stateOf(conflicted.changes, copy.nodeId)).toMatchObject({ gitTracked: true, gitTrackedRev: conflicted.seq });

    const fresh = await createFile(a, space.spaceId, "new.js");
    expect(stateOf(fresh.result.changes, fresh.nodeId)).toMatchObject({ gitTracked: false, gitTrackedRev: 0n });
  });

  it("keeps the mark through an export round trip, from the log and from a snapshot", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const now = Date.now();
    await runInDurableObject(space.stub, (instance: SpaceDO) => freezeClock(instance, now));
    const { nodeId } = await createFile(a, space.spaceId, "app.js");
    await a.commit([mark(nodeId)]);
    await runSpaceAlarm(space.stub, now + 60_000);
    await runSpaceAlarm(space.stub, now + 5 * 60_000);
    const fromLog = await restoreFromR2(space.spaceId);
    expect(fromLog.manifest).toBeUndefined();
    expect(fromLog.nodes.get(nodeId)!.state).toMatchObject({ gitTracked: true });
    expect(fromLog.nodes).toEqual((await serverState(space.stub)).nodes);

    await a.commit([op.mkdir(uuidv7(), space.spaceId, "later")]);
    await runSpaceAlarm(space.stub, now + 2 * 24 * 3_600_000);
    const fromSnapshot = await restoreFromR2(space.spaceId);
    expect(fromSnapshot.manifest).toBeDefined();
    expect(fromSnapshot.nodes.get(nodeId)!.state).toMatchObject({ gitTracked: true });
    expect(fromSnapshot.nodes).toEqual((await serverState(space.stub)).nodes);
  });
});

describe("RFC 0033 section 2 rule 6: digest v7", () => {
  it("equals digest v6 for a request without the op and covers the op", async () => {
    const parts = await content("one");
    const commit = (ops: Op[]) => create(CommitSchema, { baseSeq: 1n, deviceSeq: 2n, ops });
    const plain = commit([op.mkdir("d", "r", "D"), op.create("f", "d", "f", parts.content), op.move("m", "r", "n"), op.delete("x", "c:0"), op.restore("y")]);
    expect(toHex(await commitDigest(plain, 7))).toBe(toHex(await commitDigest(plain, 6)));
    const [on, off] = [commit([mark("f")]), commit([mark("f", false)])];
    expect(toHex(await commitDigest(on, 6))).toBe(toHex(await commitDigest(commit([create(OpSchema)]), 6)));
    expect(toHex(await commitDigest(on, 7))).not.toBe(toHex(await commitDigest(on, 6)));
    expect(toHex(await commitDigest(on, 7))).not.toBe(toHex(await commitDigest(off, 7)));
  });

  it("returns the stored result to a resend of a commit stored with digest v7, and refuses a changed body", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const { nodeId } = await createFile(a, space.spaceId, "app.js");
    const marking = uuidv7();
    const storedV7 = await a.commit([mark(nodeId)], { commitId: marking });
    expect(await a.commit([mark(nodeId)], { commitId: marking })).toEqual(storedV7);
    const changed = await a.commitRaw([mark(nodeId, false)], { commitId: marking });
    expect(changed.case === "error" && changed.value.code).toBe(Error_Code.INVALID_ARGUMENT);
  });
});

describe("RFC 0033 section 2 rule 9: the git_tracked feature", () => {
  it("is on by default and follows KOIDRIVE_FEATURES", () => {
    expect([...configuredFeatures(undefined)]).toEqual(["git_tracked"]);
    expect([...configuredFeatures("-git_tracked")]).toEqual([]);
    expect([...configuredFeatures("-git_tracked, git_tracked")]).toEqual(["git_tracked"]);
    expect([...configuredFeatures("unknown,-other")]).toEqual(["git_tracked"]);
  });

  it("refuses a commit that holds the op with INVALID_ARGUMENT while off and applies nothing", async () => {
    const space = await newSpace();
    const a = await client(space, "Laptop");
    const { nodeId, result } = await createFile(a, space.spaceId, "app.js");
    const refused = await withFeatures(space.stub, "-git_tracked", () => a.commitRaw([op.mkdir(uuidv7(), space.spaceId, "x"), mark(nodeId)]));
    expect(refused.case === "error" && refused.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    const stored = await runInDurableObject(space.stub, (_instance, state) => ({
      head: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value,
      marked: state.storage.sql.exec("SELECT git_tracked FROM nodes WHERE node_id = ?", nodeId).one().git_tracked,
    }));
    expect(stored).toEqual({ head: Number(result.seq), marked: 0 });
  });

  it("leaves the fields out of every record and page on a connection whose Welcome lacks the feature", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const without = await Socket.connect(spaceId, owner);
    expect((await withFeatures(stub, "-git_tracked", () => without.start())).features).not.toContain("git_tracked");
    const marking = await Socket.connect(spaceId, owner);
    expect((await marking.start()).features).toContain("git_tracked");

    const file = uuidv7();
    const parts = await prepared(marking, "dist " + uuidv7());
    const created = await marking.commit(0n, [op.create(file, spaceId, "dist.js", parts.content)], parts.receipts);
    const marked = await marking.commit(created.seq, [mark(file)]);
    expect(stateOf(marked.changes, file)).toMatchObject({ gitTracked: true, gitTrackedRev: marked.seq });
    await without.pushed();
    expect(stateOf((await without.pushed())[0]!.changes, file)).toMatchObject({ gitTracked: false, gitTrackedRev: 0n });

    const a = await client({ spaceId, stub }, "Laptop");
    const movedWhileOff = await withFeatures(stub, "-git_tracked", async () => {
      expect((await a.tree()).get(file)).toMatchObject({ gitTracked: false, gitTrackedRev: 0n });
      return marking.commit(marked.seq, [op.move(file, spaceId, "app.js")]);
    });
    expect(stateOf(movedWhileOff.changes, file)).toMatchObject({ gitTracked: true, gitTrackedRev: marked.seq });
    expect(stateOf((await without.pushed())[0]!.changes, file)).toMatchObject({ gitTracked: false });

    const page = await without.rpc({ case: "listTree", value: {} });
    expect(page.case === "tree" && page.value.nodes.find((node) => node.nodeId === file)).toMatchObject({ gitTracked: false, gitTrackedRev: 0n });
    const replayed = await without.resume(without.historyId, 0n);
    expect(replayed.records.map((record) => stateOf(record.changes, file)?.gitTracked)).toEqual([false, false, false]);

    const later = await Socket.connect(spaceId, owner);
    await later.hello();
    const after = await later.resume(later.historyId, 0n);
    expect(after.records.map((record) => stateOf(record.changes, file)?.gitTracked)).toEqual([false, true, true]);
  }, 30_000);

  it("closes on start each connection whose Welcome disagrees with the feature's state, both ways", async () => {
    const owner = await getOwner();
    const phone = await pairDevice(owner, "Phone");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const connect = async (creds = owner) => {
      const socket = await Socket.connect(spaceId, creds);
      await socket.start();
      return socket;
    };
    const lacking = await withFeatures(stub, "-git_tracked", () => connect());
    const listing = await connect();
    const unrecorded = await connect(phone);
    await runInDurableObject(stub, (_instance, state) => {
      for (const ws of state.getWebSockets(phone.deviceId)) {
        const { features: _features, ...older } = ws.deserializeAttachment() as { features?: string[] };
        ws.serializeAttachment(older);
      }
    });

    await start(stub);
    for (const socket of [lacking, unrecorded]) {
      expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER } });
      expect((await socket.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
    }
    expect((await listing.rpc({ case: "listTree", value: {} })).case).toBe("tree");

    await withFeatures(stub, "-git_tracked", () => start(stub));
    expect((await listing.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
  });
});
