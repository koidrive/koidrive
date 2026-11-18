// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { type Op, OpResult_Code, OpSchema } from "../src/gen/koidrive/v1/commit_pb";
import { type ChangeRecord, type Content, FieldIntentsSchema, type Intent, IntentSchema, type NodeState, NodeStatus } from "../src/gen/koidrive/v1/model_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { fromHex, toHex, uuidv7 } from "../src/ids";
import type { SpaceDO } from "../src/space-do";
import { commitDigest } from "../src/space/digest";
import { type Field, legacyIntentId, legacyPlaceKey, type LostChangeRule } from "../src/space/intents";
import { MIGRATIONS } from "../src/space/schema";
import { getNode, nodeState } from "../src/space/store";
import legacyVectors from "../../spec/vectors/legacy-intents.json";
import { restoreFromR2, serverState } from "./export-reader";
import { client, content, freezeClock, newSpace, op, runSpaceAlarm, type TestClient } from "./helpers";

type Space = Awaited<ReturnType<typeof newSpace>>;
type IntentInit = { id?: Uint8Array; author?: string; replay?: boolean; over?: Uint8Array; appliedAtMs?: bigint };

const DAY_MS = 86_400_000;

function intentId(): Uint8Array {
  return fromHex(uuidv7().replaceAll("-", ""))!;
}

function intent(author: string, init: IntentInit = {}): Intent {
  return create(IntentSchema, { id: init.id ?? intentId(), authorDeviceId: author, replay: init.replay ?? false, over: init.over ?? new Uint8Array(), appliedAtMs: init.appliedAtMs ?? 0n });
}

function sent(base: Op, sentIntent: Intent, fields?: { place?: Intent; content?: Intent; status?: Intent }): Op {
  return create(OpSchema, { ...base, intent: sentIntent, fields: fields && create(FieldIntentsSchema, fields) });
}

function del(nodeId: string, view: { unseen?: Uint8Array[]; held?: Uint8Array[] } = {}): Op {
  return create(OpSchema, { op: { case: "delete", value: { nodeId, unseen: view.unseen ?? [], held: view.held ?? [] } } });
}

function update(nodeId: string, value: Content, ancestry: { madeOver?: string[]; sourceVersionId?: string; resend?: boolean } = {}): Op {
  return create(OpSchema, { op: { case: "updateContent", value: { nodeId, content: value, ...ancestry } } });
}

function changedId({ change }: ChangeRecord): string | undefined {
  return change.case === "node" ? change.value.nodeId : undefined;
}

function idOf(found: Intent | undefined): string {
  return toHex(found?.id ?? new Uint8Array());
}

async function stateOf(space: Space, nodeId: string): Promise<NodeState | undefined> {
  return runInDurableObject(space.stub, (_instance, state) => {
    const node = getNode(state.storage.sql, nodeId);
    return node && nodeState(state.storage.sql, node);
  });
}

async function setRule(space: Space, rule: LostChangeRule): Promise<void> {
  await runInDurableObject(space.stub, (instance: SpaceDO) => {
    instance.lostChangeRule = rule;
  });
}

async function setClock(space: Space, atMs: number): Promise<void> {
  await runInDurableObject(space.stub, (instance: SpaceDO) => {
    freezeClock(instance, atMs);
  });
}

async function setup() {
  const space = await newSpace();
  const a = await client(space, "Laptop");
  const b = await client(space, "Desk");
  const c = await client(space, "Tablet");
  const parts = await Promise.all(["one", "two", "three", "four"].map((text) => content(text + " " + uuidv7())));
  const values = parts.map((part) => part.content);
  const seed = uuidv7();
  const receipts = [];
  for (const part of parts) receipts.push(...(await a.uploadParts(part)));
  await a.commit([op.mkdir(seed, space.spaceId, "seed"), ...values.map((value, i) => op.create(uuidv7(), seed, `${i}.bin`, value))], { receipts });
  for (const device of [b, c]) device.baseSeq = a.baseSeq;
  return { space, a, b, c, values, root: space.spaceId };
}

async function file(owner: TestClient, parentId: string, name: string, value: Content, sentIntent?: Intent) {
  const nodeId = uuidv7();
  const base = op.create(nodeId, parentId, name, value);
  await owner.commit([sentIntent ? sent(base, sentIntent) : base]);
  return nodeId;
}

function sync(...devices: TestClient[]): void {
  const top = devices.reduce((max, device) => (device.baseSeq > max ? device.baseSeq : max), 0n);
  for (const device of devices) device.baseSeq = top;
}

describe("adversarial review of #659", () => {
  it("under rule B, the author's replayed edit of a file another device deleted without seeing it revives the file with the edit (R8)", async () => {
    const { space, a, b, values, root } = await setup();
    await setRule(space, "B");
    const nodeId = await file(a, root, "report.docx", values[0]!);
    sync(a, b);
    await b.commit([sent(del(nodeId), intent(b.session.deviceId))]);
    const replay = await a.commit([sent(update(nodeId, values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: BigInt(Date.now()) }))]);
    expect(replay.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const after = (await stateOf(space, nodeId))!;
    expect(after.status).toBe(NodeStatus.LIVE);
    expect(after.current!.content!.size).toBe(values[1]!.size);
  });

  it("a delete answered SUPERSEDED because it kept the target trashes no conflict copy elsewhere", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "deck.key", values[0]!);
    sync(a, b, c);
    await a.commit([update(nodeId, values[1]!)]);
    const lost = intent(a.session.deviceId, { replay: true });
    const copied = await c.commit([sent(update(nodeId, values[2]!), lost)]);
    expect(copied.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const copyId = copied.results[0]!.nodeId;
    sync(a, b, c);
    await a.commit([sent(op.move(nodeId, root, "deck renamed.key"), intent(a.session.deviceId))]);
    const deleted = await b.commit([sent(del(nodeId, { held: [lost.id] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, copyId))!.status).toBe(NodeStatus.LIVE);
  });

  it("after the migration, a rebuild from the R2 export reproduces the server's backfilled intents", async () => {
    const { space, a, values, root } = await setup();
    const now = Date.now();
    await setClock(space, now);
    const nodeId = await file(a, root, "plan.docx", values[0]!);
    const createdSeq = Number((await stateOf(space, nodeId))!.metaRev);
    await a.commit([op.move(nodeId, root, "plan 2.docx")]);
    await runInDurableObject(space.stub, (_instance, state) => {
      state.storage.sql.exec("DELETE FROM field_intents");
    });
    const snapshotAt = now + 32 * DAY_MS;
    await runSpaceAlarm(space.stub, snapshotAt);
    await runSpaceAlarm(space.stub, snapshotAt + 60_000);
    await runSpaceAlarm(space.stub, snapshotAt + 10 * 60_000);
    const step = MIGRATIONS.length - 1;
    await runInDurableObject(space.stub, (_instance, state) => {
      const sql = state.storage.sql;
      for (const table of ["field_intents", "trash_held", "tombstones"]) sql.exec(`DROP TABLE ${table}`);
      sql.exec("ALTER TABLE nodes DROP COLUMN status_before");
      sql.exec("ALTER TABLE nodes DROP COLUMN copy_mark");
      sql.exec("UPDATE meta SET value = ? WHERE key = 'schema_version'", step);
    });
    await evictDurableObject(space.stub);
    const server = (await serverState(space.stub)).nodes.get(nodeId)!.state;
    expect(idOf(server.intents!.status)).toBe(legacyIntentId(nodeId, "status", `live:${createdSeq}`));
    for (let i = 1; i <= 4; i++) await runSpaceAlarm(space.stub, snapshotAt + DAY_MS + i * 10 * 60_000);
    const restored = await restoreFromR2(space.spaceId);
    expect(restored.manifest!.start_seq).toBeGreaterThan(createdSeq);
    const now2 = await serverState(space.stub);
    expect(restored.headSeq).toBe(now2.headSeq);
    for (const [id, node] of now2.nodes) expect(restored.nodes.get(id)?.state.intents).toEqual(node.state.intents);
  });
it("a delete answered SUPERSEDED with nothing trashed moves no node out of the folder it kept", async () => {
    const { space, a, b, c, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Shared")]);
    const edited = await file(a, dir, "edited.txt", values[0]!);
    sync(a, b, c);
    const carried = uuidv7();
    await c.commit([sent(op.create(carried, dir, "carried.txt", values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: BigInt(Date.now()) }))]);
    expect((await stateOf(space, carried))!.intents!.place!.replay).toBe(true);
    await a.commit([sent(update(edited, values[2]!), intent(a.session.deviceId))]);
    const deleted = await b.commit([sent(del(dir), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, dir))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, carried))!).toMatchObject({ parentId: dir, status: NodeStatus.LIVE });
  });

  it("schedules the next GC run soon when a purge stops at the message size limit with nodes still due", async () => {
    const { space, a, b, root } = await setup();
    const dirs = [uuidv7(), uuidv7()];
    for (const [n, dir] of dirs.entries()) {
      await a.commit([op.mkdir(dir, root, `Big ${n}`), ...Array.from({ length: 20 }, (_, i) => op.mkdir(uuidv7(), dir, `d${i}`))]);
      await a.commit([sent(del(dir, { held: Array.from({ length: 2000 }, () => intent(b.session.deviceId).id) }), intent(a.session.deviceId))]);
    }
    const report = await runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(Date.now() + 31 * DAY_MS));
    expect(report.purgedNodes).toBeLessThan(42);
    expect(report.more).toBe(true);
  });
});
