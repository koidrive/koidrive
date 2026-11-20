// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { type Op, OpSchema } from "../src/gen/koidrive/v1/commit_pb";
import { type ChangeRecord, CommitRecordSchema, IntentSchema, type NodeState, NodeStatus } from "../src/gen/koidrive/v1/model_pb";
import { fromHex, toHex, uuidv7 } from "../src/ids";
import type { SpaceDO } from "../src/space-do";
import { getNode, nodeState } from "../src/space/store";
import { client, content, freezeClock, newSpace, op, type TestClient } from "./helpers";

type Space = Awaited<ReturnType<typeof newSpace>>;

function intentId(): Uint8Array {
  return fromHex(uuidv7().replaceAll("-", ""))!;
}

function deleteHolding(nodeId: string, author: string, held: Uint8Array[]): Op {
  return create(OpSchema, {
    op: { case: "delete", value: { nodeId, unseen: [], held, trashGroup: "" } },
    intent: create(IntentSchema, { id: intentId(), authorDeviceId: author }),
  });
}

function storedRow(space: Space, nodeId: string) {
  return runInDurableObject(space.stub, (_instance, state) => {
    const sql = state.storage.sql;
    const node = getNode(sql, nodeId)!;
    return {
      node,
      intents: sql.exec("SELECT * FROM field_intents WHERE node_id = ? ORDER BY field", nodeId).toArray(),
      held: sql.exec("SELECT intent_id FROM trash_held WHERE trash_group = ? ORDER BY intent_id", node.trash_group).toArray(),
      state: nodeState(sql, node),
    };
  });
}

function loggedChanges(space: Space, afterSeq: bigint): Promise<{ seq: bigint; changes: ChangeRecord[] }[]> {
  return runInDurableObject(space.stub, (_instance, state) =>
    state.storage.sql
      .exec("SELECT record FROM commits WHERE seq > ? ORDER BY seq", Number(afterSeq))
      .toArray()
      .map((row) => fromBinary(CommitRecordSchema, new Uint8Array(row.record as ArrayBuffer))),
  );
}

function statesOf(changes: ChangeRecord[], nodeId: string): NodeState[] {
  return changes.flatMap(({ change }) => (change.case === "node" && change.value.nodeId === nodeId ? [change.value] : []));
}

function setClock(space: Space, atMs: number) {
  return runInDurableObject(space.stub, (instance: SpaceDO) => freezeClock(instance, atMs));
}

async function trashEntry(c: TestClient, nodeId: string) {
  const page = await c.call({ case: "listTrash", value: {} });
  if (page.case !== "trash") throw new Error("listTrash failed");
  return page.value.trashed.find((entry) => entry.nodeId === nodeId);
}

describe("RFC 0029 section 3.4: a folder a commit revived only as an ancestor of changes it moved away", () => {
  it("goes back to the trash with its pre-commit row, and the result, the log and a retry report it trashed", async () => {
    const space = await newSpace();
    const [laptop, desk] = [await client(space, "Laptop"), await client(space, "Desk")];
    const [a, c, b, e, x, d] = Array.from({ length: 6 }, () => uuidv7());
    const parts = await content("child " + uuidv7());
    await setClock(space, 1_000_000);
    await laptop.commit(
      [op.mkdir(a, space.spaceId, "old"), op.mkdir(c, a, "c"), op.create(b, c, "child", parts.content), op.mkdir(e, c, "e"), op.mkdir(x, space.spaceId, "x"), op.mkdir(d, x, "d")],
      { receipts: await laptop.uploadParts(parts) },
    );
    desk.baseSeq = laptop.baseSeq;
    await desk.commit([op.delete(a)]);
    const built = desk.baseSeq;
    await setClock(space, 2_000_000);
    const trashedX = await desk.commit([deleteHolding(x, desk.session.deviceId, [intentId()])]);
    const before = await storedRow(space, x);
    expect(before.held).toHaveLength(1);

    await setClock(space, 9_000_000);
    const commitId = uuidv7();
    const ops = [op.move(a, space.spaceId, "new"), op.move(b, d, "child"), op.move(e, space.spaceId, "e"), op.move(c, space.spaceId, "c"), op.delete(c), op.move(d, e, "d")];
    const result = await laptop.commit(ops, { commitId, baseSeq: built });

    const after = await storedRow(space, x);
    expect(after.node).toEqual(before.node);
    expect(after.intents).toEqual(before.intents);
    expect(after.held).toEqual(before.held);
    expect(await trashEntry(laptop, x)).toMatchObject({ trashedAtMs: trashedX.committedAtMs, deviceId: desk.session.deviceId });
    const tree = await laptop.tree();
    expect([tree.get(d)?.parentId, tree.get(b)?.parentId, tree.has(x)]).toEqual([e, d, false]);

    expect(statesOf(result.changes, x)).toEqual([before.state]);
    expect(before.state.status).toBe(NodeStatus.TRASHED);
    const logged = await loggedChanges(space, trashedX.seq);
    expect(logged.map((record) => record.seq)).toEqual([result.seq]);
    expect(logged[0]!.changes).toEqual(result.changes);

    await desk.commit([op.mkdir(uuidv7(), space.spaceId, "later")]);
    const retried = await laptop.commit(ops, { commitId, baseSeq: built });
    expect(retried).toEqual(result);
  });

  it("goes back to the row a Delete of the same commit gave it, with the held the Delete merged and that Delete's trash group", async () => {
    const space = await newSpace();
    const [laptop, desk] = [await client(space, "Laptop"), await client(space, "Desk")];
    const [x, d, f] = [uuidv7(), uuidv7(), uuidv7()];
    await laptop.commit([op.mkdir(x, space.spaceId, "x"), op.mkdir(d, x, "d"), op.mkdir(f, space.spaceId, "f")]);
    desk.baseSeq = laptop.baseSeq;
    await desk.commit([deleteHolding(x, desk.session.deviceId, [intentId()])]);
    const { node: deleted } = await storedRow(space, x);
    const held = intentId();
    const result = await laptop.commit([deleteHolding(x, laptop.session.deviceId, [held]), op.move(f, d, "f"), op.move(d, space.spaceId, "d")]);
    const after = await storedRow(space, x);
    expect(after.node).toEqual({ ...deleted, meta_rev: Number(result.seq), meta_dev: laptop.session.deviceId });
    expect(after.held).toHaveLength(2);
    expect(statesOf(result.changes, x)).toEqual([after.state]);
    expect((await laptop.tree()).get(d)?.parentId).toBe(space.spaceId);
  });

  it("keeps the held another op of the commit merged into its group and takes the meta revision the merge gave the group, keeping its deletion device", async () => {
    const space = await newSpace();
    const [laptop, desk] = [await client(space, "Laptop"), await client(space, "Desk")];
    const [x, d, f] = [uuidv7(), uuidv7(), uuidv7()];
    await laptop.commit([op.mkdir(x, space.spaceId, "x"), op.mkdir(d, x, "d"), op.mkdir(f, space.spaceId, "f")]);
    desk.baseSeq = laptop.baseSeq;
    const first = intentId();
    const trashedX = await desk.commit([deleteHolding(x, desk.session.deviceId, [first])]);
    const before = await storedRow(space, x);
    const second = intentId();
    const result = await laptop.commit([op.move(f, x, "f"), deleteHolding(d, laptop.session.deviceId, [second]), op.move(f, space.spaceId, "f")]);
    const after = await storedRow(space, x);
    expect(after.node).toEqual({ ...before.node, meta_rev: Number(result.seq) });
    expect(await trashEntry(laptop, x)).toMatchObject({ trashedAtMs: trashedX.committedAtMs, deviceId: desk.session.deviceId });
    expect(after.intents).toEqual(before.intents);
    expect(after.held.map((row) => row.intent_id)).toEqual([first, second].map(toHex).sort());
    expect((await storedRow(space, d)).node).toMatchObject({ trash_group: before.node.trash_group, meta_rev: Number(result.seq) });
    expect(statesOf(result.changes, x)).toEqual([after.state]);
  });
});
