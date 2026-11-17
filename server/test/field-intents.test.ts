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
import { client, content, newSpace, op, runSpaceAlarm, type TestClient } from "./helpers";

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
    instance.clock = () => atMs;
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

describe("RFC 0028 section 3.1: recording intents", () => {
  it("mints one intent for an op without one and reports all three fields with the commit's time", async () => {
    const { space, a, values, root } = await setup();
    const nodeId = uuidv7();
    const done = await a.commit([op.create(nodeId, root, "report.docx", values[0]!)]);
    const { intents } = (await stateOf(space, nodeId))!;
    const ids = [intents!.place, intents!.content, intents!.status].map(idOf);
    expect(new Set(ids).size).toBe(1);
    expect(ids[0]).toMatch(/^[0-9a-f]{12}7/);
    for (const field of [intents!.place!, intents!.content!, intents!.status!]) {
      expect(field.authorDeviceId).toBe(a.session.deviceId);
      expect(field.replay).toBe(false);
      expect(field.appliedAtMs).toBe(done.committedAtMs);
    }
    expect(done.changes[0]!.change.value).toMatchObject({ intents: { place: { appliedAtMs: done.committedAtMs } } });
  });

  it("gives an old client's resend, from-move and copy a carried intent with an empty author", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "deck.key", values[0]!);
    sync(a, b);
    await b.commit([update(nodeId, values[1]!, { resend: true })]);
    expect((await stateOf(space, nodeId))!.intents!.content).toMatchObject({ authorDeviceId: "", replay: true });
    const moved = await file(a, root, "notes.txt", values[2]!);
    sync(a, b);
    await b.commit([create(OpSchema, { op: { case: "move", value: { nodeId: moved, newParentId: root, newName: "notes 2.txt", from: { parentId: root, name: "notes.txt" } } } })]);
    expect((await stateOf(space, moved))!.intents!.place).toMatchObject({ authorDeviceId: "", replay: true });
  });

  it("records a separate intent per field for a create with fields", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = uuidv7();
    const [place, contentIntent] = [intent(b.session.deviceId), intent(a.session.deviceId)];
    await a.commit([sent(op.create(nodeId, root, "brief.pdf", values[0]!), place, { content: contentIntent })]);
    const { intents } = (await stateOf(space, nodeId))!;
    expect([idOf(intents!.place), idOf(intents!.content), idOf(intents!.status)]).toEqual([idOf(place), idOf(contentIntent), idOf(place)]);
    expect(intents!.place).toMatchObject({ authorDeviceId: b.session.deviceId, replay: true });
    expect(intents!.content).toMatchObject({ authorDeviceId: a.session.deviceId, replay: false });
  });

  it("keeps a replay's applied time, clamped to the commit's, and stamps every other op with the commit's time", async () => {
    const { space, a, values, root } = await setup();
    const [early, late, plain] = [uuidv7(), uuidv7(), uuidv7()];
    const done = await a.commit([
      sent(op.create(early, root, "early.txt", values[0]!), intent(a.session.deviceId, { replay: true, appliedAtMs: 1234n })),
      sent(op.create(late, root, "late.txt", values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: 9_999_999_999_999n })),
      sent(op.create(plain, root, "plain.txt", values[2]!), intent(a.session.deviceId, { appliedAtMs: 1234n })),
    ]);
    expect((await stateOf(space, early))!.intents!.content!.appliedAtMs).toBe(1234n);
    expect((await stateOf(space, late))!.intents!.content!.appliedAtMs).toBe(done.committedAtMs);
    expect((await stateOf(space, plain))!.intents!.content!.appliedAtMs).toBe(done.committedAtMs);
  });

  it("waits for its clock to pass the head's stamp before it commits, and stamps one past the head after 50 ms behind it", async () => {
    const { space, a, root } = await setup();
    const waits: number[] = [];
    let atMs = 5_000_000_000_000;
    await runInDurableObject(space.stub, (instance: SpaceDO) => {
      instance.clock = () => atMs;
      instance.sleep = async (ms) => {
        waits.push(ms);
        atMs += ms;
      };
    });
    const first = await a.commit([op.mkdir(uuidv7(), root, "one")]);
    const second = await a.commit([op.mkdir(uuidv7(), root, "two")]);
    expect(waits).toEqual([1]);
    expect(second.committedAtMs).toBe(BigInt(atMs));
    atMs = 1_000;
    const third = await a.commit([op.mkdir(uuidv7(), root, "three")]);
    expect(waits).toEqual([1, 50]);
    expect([second.committedAtMs - first.committedAtMs, third.committedAtMs - second.committedAtMs]).toEqual([1n, 1n]);
  });

  it("answers an op whose intent and value the field holds APPLIED and changes nothing", async () => {
    const { space, a, b, values, root } = await setup();
    const sentIntent = intent(a.session.deviceId);
    const nodeId = uuidv7();
    await a.commit([sent(op.create(nodeId, root, "same.txt", values[0]!), sentIntent)]);
    const before = await stateOf(space, nodeId);
    sync(a, b);
    const again = await a.commit([sent(op.create(nodeId, root, "same.txt", values[0]!), create(IntentSchema, { ...sentIntent, replay: true }))]);
    expect(again.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(again.changes).toEqual([]);
    const edit = await a.commit([sent(update(nodeId, values[0]!), sentIntent)]);
    expect(edit.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(edit.changes).toEqual([]);
    expect(await stateOf(space, nodeId)).toEqual(before);
  });

  it("answers an op without intents on an existing node as before: the same create is ID_EXISTS", async () => {
    const { a, values, root } = await setup();
    const nodeId = await file(a, root, "x.txt", values[0]!);
    const again = await a.commit([op.create(nodeId, root, "x.txt", values[0]!)]);
    expect(again.results[0]).toMatchObject({ code: OpResult_Code.REJECTED, rejectReason: "ID_EXISTS" });
  });

  for (const rule of ["A", "B"] as const) {
    it(`under rule ${rule}, the author's send of a carried value ${rule === "B" ? "claims it and moves only the edit revision" : "leaves it carried"}`, async () => {
      const { space, a, b, values, root } = await setup();
      await setRule(space, rule);
      const lost = intent(a.session.deviceId);
      const nodeId = await file(b, root, "lost.txt", values[0]!, create(IntentSchema, { ...lost, replay: true }));
      const carried = (await stateOf(space, nodeId))!;
      expect(carried.intents!.content!.replay).toBe(true);
      const claim = await a.commit([sent(update(nodeId, values[0]!), lost)]);
      expect(claim.results[0]!.code).toBe(OpResult_Code.APPLIED);
      const after = (await stateOf(space, nodeId))!;
      expect(after.intents!.content!.replay).toBe(rule === "A");
      expect([after.contentRev, after.metaRev]).toEqual([carried.contentRev, carried.metaRev]);
      expect(after.editRev > carried.editRev).toBe(rule === "B");
      expect(claim.changes.length).toBe(rule === "B" ? 1 : 0);
    });
  }

  it("supersedes a Move whose over the node's place does not hold and a Delete whose over its status does not hold", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "plan.txt", values[0]!);
    const held = (await stateOf(space, nodeId))!.intents!;
    sync(a, b);
    const stale = await b.commit([sent(op.move(nodeId, root, "plan 2.txt"), intent(b.session.deviceId, { over: intentId() }))]);
    expect(stale.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const moved = await b.commit([sent(op.move(nodeId, root, "plan 2.txt"), intent(b.session.deviceId, { over: held.place!.id }))]);
    expect(moved.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(idOf((await stateOf(space, nodeId))!.intents!.status)).toBe(idOf(held.status));
    const overStatus = await b.commit([sent(op.move(nodeId, root, "plan 3.txt"), intent(b.session.deviceId, { over: held.status!.id }))]);
    expect(overStatus.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const deleted = await b.commit([sent(del(nodeId), intent(b.session.deviceId, { over: intentId() }))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.LIVE);
    const trashed = await b.commit([sent(del(nodeId), intent(b.session.deviceId, { over: held.status!.id }))]);
    expect(trashed.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.TRASHED);
  });

  it("merges a Delete's held into a trash whose replaced status is the Delete's over, and answers APPLIED", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "budget.xlsx", values[0]!);
    const before = (await stateOf(space, nodeId))!.intents!.status!;
    sync(a, b);
    await a.commit([sent(del(nodeId), intent(a.session.deviceId))]);
    const lostRename = intentId();
    const merged = await b.commit([sent(del(nodeId, { held: [lostRename] }), intent(b.session.deviceId, { over: before.id }))]);
    expect(merged.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const trashed = (await stateOf(space, nodeId))!;
    expect(trashed.trashHeld.map(toHex)).toEqual([toHex(lostRename)]);
    expect(merged.changes.map(changedId)).toEqual([nodeId]);
  });

  it("reports on a trashed node the status intent its trash replaced, as the status intent's over, and clears it on restore", async () => {
    const { space, a, values, root } = await setup();
    const nodeId = await file(a, root, "slides.pptx", values[0]!);
    const created = (await stateOf(space, nodeId))!.intents!.status!;
    const removal = intent(a.session.deviceId);
    await a.commit([sent(del(nodeId), removal)]);
    const trashed = (await stateOf(space, nodeId))!.intents!.status!;
    expect([idOf(trashed), toHex(trashed.over)]).toEqual([idOf(removal), idOf(created)]);
    for (const field of ["place", "content"] as const) expect((await stateOf(space, nodeId))!.intents![field]!.over).toHaveLength(0);
    await a.commit([sent(op.restore(nodeId), intent(a.session.deviceId))]);
    expect((await stateOf(space, nodeId))!.intents!.status!.over).toHaveLength(0);
  });

  it("keeps the field's intent when RFC 0016 makes an edit a no-op, and reports the node", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "same.bin", values[0]!);
    sync(a, b);
    await a.commit([update(nodeId, values[1]!)]);
    const held = (await stateOf(space, nodeId))!.intents!.content!;
    const equal = await b.commit([sent(update(nodeId, values[1]!), intent(b.session.deviceId))]);
    expect(equal.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(equal.changes.map(changedId)).toEqual([nodeId]);
    expect(idOf((await stateOf(space, nodeId))!.intents!.content)).toBe(idOf(held));
  });

  it("gives a conflict copy the op's content intent and fresh place and status intents, carried when the op is", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "essay.doc", values[0]!);
    sync(a, b);
    await a.commit([update(nodeId, values[1]!)]);
    const own = intent(b.session.deviceId);
    const copied = await b.commit([sent(update(nodeId, values[2]!), own)]);
    expect(copied.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const copy = (await stateOf(space, copied.results[0]!.nodeId))!.intents!;
    expect(idOf(copy.content)).toBe(idOf(own));
    expect(copy.place).toMatchObject({ authorDeviceId: b.session.deviceId, replay: false });
    expect(idOf(copy.place)).not.toBe(idOf(own));
    const other = intent(a.session.deviceId, { replay: true });
    const carried = await b.commit([sent(update(nodeId, values[3]!), other)], { baseSeq: 1n });
    expect(carried.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const carriedCopy = (await stateOf(space, carried.results[0]!.nodeId))!.intents!;
    expect(carriedCopy.place).toMatchObject({ authorDeviceId: a.session.deviceId, replay: true });
    expect(carriedCopy.status).toMatchObject({ authorDeviceId: a.session.deviceId, replay: true });
  });

  it("keeps the target's intents when a create merges into an existing folder", async () => {
    const { space, a, b, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Photos")]);
    const before = (await stateOf(space, dir))!.intents;
    const merged = await b.commit([sent(op.mkdir(uuidv7(), root, "Photos"), intent(b.session.deviceId))]);
    expect(merged.results[0]!.code).toBe(OpResult_Code.MERGED_INTO);
    expect((await stateOf(space, dir))!.intents).toEqual(before);
  });

  it("refuses a commit with a malformed intent or fields on an op other than a create", async () => {
    const { a, values, root } = await setup();
    const nodeId = await file(a, root, "x.txt", values[0]!);
    const short = await a.commitRaw([sent(op.move(nodeId, root, "y.txt"), create(IntentSchema, { id: new Uint8Array(15) }))]);
    expect(short.case === "error" && short.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    const fields = await a.commitRaw([sent(op.move(nodeId, root, "y.txt"), intent(a.session.deviceId), { place: intent(a.session.deviceId) })]);
    expect(fields.case === "error" && fields.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    const view = await a.commitRaw([sent(del(nodeId, { unseen: [new Uint8Array(3)] }), intent(a.session.deviceId))]);
    expect(view.case === "error" && view.value.code).toBe(Error_Code.INVALID_ARGUMENT);
  });
});

describe("RFC 0028 section 3.2: carried values", () => {
  for (const [label, author, replay, rule, carried] of [
    ["another device's value", "other", false, "A", true],
    ["an empty author", "", false, "A", true],
    ["the author's own replay under rule A", "self", true, "A", true],
    ["the author's own replay under rule B", "self", true, "B", false],
    ["the author's own change", "self", false, "A", false],
  ] as const) {
    it(`counts ${label} as ${carried ? "carried" : "not carried"}`, async () => {
      const { space, a, b, values, root } = await setup();
      await setRule(space, rule);
      const nodeId = uuidv7();
      const by = author === "self" ? a.session.deviceId : author === "other" ? b.session.deviceId : "";
      await a.commit([sent(op.create(nodeId, root, "f.txt", values[0]!), intent(by, { replay }))]);
      expect((await stateOf(space, nodeId))!.intents!.content!.replay).toBe(carried);
    });
  }
});

describe("RFC 0028 section 3.3: deletes with a view", () => {
  it("keeps a node whose intent the delete lists as unseen, with the folders above it", async () => {
    const { space, a, b, values, root } = await setup();
    const [dir, sub] = [uuidv7(), uuidv7()];
    await a.commit([op.mkdir(dir, root, "Project"), op.mkdir(sub, dir, "Drafts")]);
    const kept = await file(a, sub, "draft.doc", values[0]!);
    const gone = await file(a, dir, "old.doc", values[1]!);
    sync(a, b);
    const unseen = (await stateOf(space, kept))!.intents!.content!.id;
    const deleted = await b.commit([sent(del(dir, { unseen: [unseen] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    for (const nodeId of [dir, sub, kept]) expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, gone))!.status).toBe(NodeStatus.TRASHED);
  });

  it("keeps another device's change after base_seq and trashes what the view holds", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Shared")]);
    const edited = await file(a, dir, "edited.txt", values[0]!);
    const untouched = await file(a, dir, "untouched.txt", values[1]!);
    sync(a, b);
    await a.commit([sent(update(edited, values[2]!), intent(a.session.deviceId))]);
    const deleted = await b.commit([sent(del(dir), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, edited))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, untouched))!.status).toBe(NodeStatus.TRASHED);
  });

  it("trashes a carried replay the delete did not see and keeps its content as a conflict copy beside it", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "carried.txt", values[0]!);
    sync(a, b, c);
    const replayed = await c.commit([sent(update(nodeId, values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: 5n }))]);
    expect(replayed.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const deleted = await b.commit([sent(del(nodeId), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.TRASHED);
    const copy = deleted.changes.map(({ change }) => change.value as NodeState).find((state) => state.nodeId !== nodeId)!;
    expect(copy).toMatchObject({ parentId: root, status: NodeStatus.LIVE, intents: { place: { replay: true, authorDeviceId: a.session.deviceId } } });
    expect(copy.name).toMatch(/^carried \(conflict from Desk /);
  });

  it("trashes a carried replay whose intent the delete lists as held without a copy", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "held.txt", values[0]!);
    sync(a, b, c);
    const replay = intent(a.session.deviceId, { replay: true });
    await c.commit([sent(update(nodeId, values[1]!), replay)]);
    const deleted = await b.commit([sent(del(nodeId, { held: [replay.id] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(deleted.changes.map(changedId)).toEqual([nodeId]);
    expect((await stateOf(space, nodeId))!.trashHeld.map(toHex)).toEqual([toHex(replay.id)]);
  });

  it("moves a node with a carried place out of a deleted folder when the view lacks its content, and trashes it when the view holds it", async () => {
    const { space, a, b, c, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Inbox")]);
    sync(a, b, c);
    const unseenCopy = uuidv7();
    const seenCopy = uuidv7();
    const seen = intent(a.session.deviceId, { replay: true });
    await c.commit([
      sent(op.create(unseenCopy, dir, "copy.txt", values[0]!), intent(a.session.deviceId, { replay: true })),
      sent(op.create(seenCopy, dir, "seen.txt", values[1]!), seen),
    ]);
    const deleted = await b.commit([sent(del(dir, { held: [seen.id] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(await stateOf(space, unseenCopy)).toMatchObject({ parentId: root, status: NodeStatus.LIVE, intents: { place: { replay: true } } });
    expect((await stateOf(space, seenCopy))!.status).toBe(NodeStatus.TRASHED);
    expect((await stateOf(space, dir))!.status).toBe(NodeStatus.TRASHED);
  });

  it("leaves a node with a carried place in a folder that R7 keeps, and moves it out of a trashed one into the nearest live folder", async () => {
    const { space, a, b, c, values, root } = await setup();
    const [top, kept, gone] = [uuidv7(), uuidv7(), uuidv7()];
    await a.commit([op.mkdir(top, root, "Top"), op.mkdir(kept, top, "Kept"), op.mkdir(gone, top, "Gone")]);
    const edited = await file(a, kept, "edited.txt", values[0]!);
    sync(a, b, c);
    const [besideEdit, inGone] = [uuidv7(), uuidv7()];
    await c.commit([sent(op.create(besideEdit, kept, "carried.txt", values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: 1n }))]);
    await c.commit([sent(op.create(inGone, gone, "carried.txt", values[1]!), intent(a.session.deviceId, { replay: true, appliedAtMs: 1n }))]);
    await a.commit([sent(update(edited, values[2]!), intent(a.session.deviceId))]);
    const placeBefore = (await stateOf(space, besideEdit))!.intents!.place;
    const deleted = await b.commit([sent(del(top), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, kept))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, gone))!.status).toBe(NodeStatus.TRASHED);
    const beside = (await stateOf(space, besideEdit))!;
    expect(beside).toMatchObject({ parentId: kept, status: NodeStatus.LIVE });
    expect(idOf(beside.intents!.place)).toBe(idOf(placeBefore));
    expect((await stateOf(space, inGone))!).toMatchObject({ parentId: top, status: NodeStatus.LIVE });
  });

  it("keeps everything inside a folder another device moved into the deleted folder", async () => {
    const { space, a, b, values, root } = await setup();
    const [target, moved] = [uuidv7(), uuidv7()];
    await a.commit([op.mkdir(target, root, "Target"), op.mkdir(moved, root, "Moved")]);
    const inside = await file(a, moved, "inside.txt", values[0]!);
    sync(a, b);
    await a.commit([sent(op.move(moved, target, "Moved"), intent(a.session.deviceId))]);
    const deleted = await b.commit([sent(del(target), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    for (const nodeId of [target, moved, inside]) expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.LIVE);
  });

  it("trashes a carried conflict copy whose content the delete's view holds, in the delete's trash group", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "deck.key", values[0]!);
    sync(a, b, c);
    await a.commit([update(nodeId, values[1]!)]);
    const replay = intent(b.session.deviceId, { replay: true });
    const copied = await c.commit([sent(update(nodeId, values[2]!), replay)]);
    expect(copied.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const copyId = copied.results[0]!.nodeId;
    sync(a, b, c);
    const deleted = await b.commit([sent(del(nodeId, { held: [replay.id] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const [original, copy] = [(await stateOf(space, nodeId))!, (await stateOf(space, copyId))!];
    expect(copy.status).toBe(NodeStatus.TRASHED);
    expect(copy.trashGroup).toBe(original.trashGroup);
  });

  it("leaves a carried copy whose content a delete lists live when R7 keeps the whole subtree", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "deck.key", values[0]!);
    sync(a, b, c);
    await a.commit([update(nodeId, values[1]!)]);
    const replay = intent(b.session.deviceId, { replay: true });
    const copied = await c.commit([sent(update(nodeId, values[2]!), replay)]);
    const copyId = copied.results[0]!.nodeId;
    sync(a, b, c);
    await a.commit([sent(op.move(nodeId, root, "deck 2.key"), intent(a.session.deviceId))]);
    const deleted = await b.commit([sent(del(nodeId, { held: [replay.id] }), intent(b.session.deviceId))]);
    expect(deleted.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.LIVE);
    expect((await stateOf(space, copyId))!.status).toBe(NodeStatus.LIVE);
  });

  it("trashes such a copy when a later delete only adds its held to an existing trash", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "deck.key", values[0]!);
    sync(a, b, c);
    await a.commit([update(nodeId, values[1]!)]);
    const replay = intent(b.session.deviceId, { replay: true });
    const copyId = (await c.commit([sent(update(nodeId, values[2]!), replay)])).results[0]!.nodeId;
    const before = (await stateOf(space, nodeId))!.intents!.status!;
    await a.commit([sent(del(nodeId), intent(a.session.deviceId))]);
    expect((await stateOf(space, copyId))!.status).toBe(NodeStatus.LIVE);
    const merged = await b.commit([sent(del(nodeId, { held: [replay.id] }), intent(b.session.deviceId, { over: before.id }))]);
    expect(merged.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect(await stateOf(space, copyId)).toMatchObject({ status: NodeStatus.TRASHED, trashGroup: (await stateOf(space, nodeId))!.trashGroup });
  });

  it("adds a second delete's held to the first one's, and supersedes a carried replay that either lists", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "both.txt", values[0]!);
    const [first, second] = [intent(c.session.deviceId, { replay: true }), intent(c.session.deviceId, { replay: true })];
    await a.commit([sent(del(nodeId, { held: [first.id] }), intent(a.session.deviceId))]);
    await b.commit([sent(del(nodeId, { held: [second.id] }), intent(b.session.deviceId))]);
    expect(new Set((await stateOf(space, nodeId))!.trashHeld.map(toHex))).toEqual(new Set([toHex(first.id), toHex(second.id)]));
    for (const replay of [first, second]) {
      const answer = await c.commit([sent(update(nodeId, values[1]!), replay)]);
      expect(answer.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    }
  });

  it("keeps a tombstone for a purged node, supersedes a replay's, carried op's or create's place and lands unknown content as a copy", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Keep")]);
    const nodeId = await file(a, dir, "gone.txt", values[0]!);
    const held = (await stateOf(space, nodeId))!.intents!.content!;
    await a.commit([sent(del(nodeId), intent(a.session.deviceId))]);
    const purged = await runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(Date.now() + 31 * DAY_MS));
    expect(purged.purgedNodes).toBe(1);
    expect(await stateOf(space, nodeId)).toBeUndefined();
    const moved = await b.commit([sent(op.move(nodeId, root, "back.txt"), intent(a.session.deviceId, { replay: true }))]);
    expect(moved.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const known = await b.commit([sent(update(nodeId, values[0]!), create(IntentSchema, { ...held, replay: true }))]);
    expect(known.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const lost = await b.commit([sent(update(nodeId, values[1]!), intent(a.session.deviceId, { replay: true }))]);
    expect(lost.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect((await stateOf(space, lost.results[0]!.nodeId))!.parentId).toBe(dir);
    const carried = await b.commit([sent(update(nodeId, values[2]!), intent(a.session.deviceId))]);
    expect(carried.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const recreated = await b.commit([op.mkdir(nodeId, root, "again")]);
    expect(recreated.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const own = await b.commit([sent(op.move(nodeId, root, "mine.txt"), intent(b.session.deviceId))]);
    expect(own.results[0]).toMatchObject({ code: OpResult_Code.REJECTED, rejectReason: "NOT_FOUND" });
    expect(await stateOf(space, nodeId)).toBeUndefined();
  });

  it("places a create or move into a purged folder under the nearest live ancestor of its last parent", async () => {
    const { space, a, b, values, root } = await setup();
    const [kept, gone, inner] = [uuidv7(), uuidv7(), uuidv7()];
    await a.commit([op.mkdir(kept, root, "Kept"), op.mkdir(gone, kept, "Gone"), op.mkdir(inner, gone, "Inner")]);
    const loose = await file(a, root, "loose.txt", values[0]!);
    await a.commit([sent(del(gone), intent(a.session.deviceId))]);
    const purged = await runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(Date.now() + 31 * DAY_MS));
    expect(purged.purgedNodes).toBe(2);
    const created = await file(b, inner, "new.txt", values[1]!);
    expect((await stateOf(space, created))!.parentId).toBe(kept);
    const moved = await b.commit([sent(op.move(loose, gone, "loose.txt"), intent(b.session.deviceId))]);
    expect(moved.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await stateOf(space, loose))!.parentId).toBe(kept);
  });
});

describe("RFC 0028 section 3.4 and 3.5: replays reaching the trash", () => {
  it("lands a carried edit of a trashed file as a conflict copy under the nearest live folder, unless the trash lists it", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Old")]);
    const nodeId = await file(a, dir, "memo.txt", values[0]!);
    const listed = intent(b.session.deviceId, { replay: true });
    await a.commit([sent(del(dir, { held: [listed.id] }), intent(a.session.deviceId))]);
    const superseded = await b.commit([sent(update(nodeId, values[1]!), listed)]);
    expect(superseded.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    const copied = await b.commit([sent(update(nodeId, values[2]!), intent(a.session.deviceId, { replay: true }))]);
    expect(copied.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(await stateOf(space, copied.results[0]!.nodeId)).toMatchObject({ parentId: root, status: NodeStatus.LIVE });
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.TRASHED);
  });

  it("makes a conflict copy of a carried create into a trashed folder whose place the trash lists but whose content it does not", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Gone")]);
    const [place, body] = [intent(b.session.deviceId, { replay: true }), intent(b.session.deviceId, { replay: true })];
    await a.commit([sent(del(dir, { held: [place.id] }), intent(a.session.deviceId))]);
    const nodeId = uuidv7();
    const created = await a.commit([sent(op.create(nodeId, dir, "new.txt", values[0]!), place, { content: body })]);
    expect(created.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(await stateOf(space, nodeId)).toBeUndefined();
    expect(await stateOf(space, created.results[0]!.nodeId)).toMatchObject({ parentId: root, intents: { content: { authorDeviceId: b.session.deviceId } } });
    expect((await stateOf(space, dir))!.status).toBe(NodeStatus.TRASHED);
    const listed = await a.commit([sent(op.create(uuidv7(), dir, "other.txt", values[1]!), intent(b.session.deviceId, { replay: true }), { content: place })]);
    expect(listed.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
  });

  it("revives a folder for a create whose place is not carried (R8)", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Back")]);
    await a.commit([sent(del(dir), intent(a.session.deviceId))]);
    const created = await b.commit([sent(op.create(uuidv7(), dir, "new.txt", values[0]!), intent(b.session.deviceId))]);
    expect(created.results[0]!.code).toBe(OpResult_Code.APPLIED);
    expect((await stateOf(space, dir))!.status).toBe(NodeStatus.LIVE);
  });

  for (const rule of ["A", "B"] as const) {
    it(`under rule ${rule}, the author's replayed Move of a node another device deleted is ${rule === "A" ? "superseded" : "applied and revives it"}`, async () => {
      const { space, a, b, values, root } = await setup();
      await setRule(space, rule);
      const nodeId = await file(a, root, "renamed.txt", values[0]!);
      sync(a, b);
      await b.commit([sent(del(nodeId), intent(b.session.deviceId))]);
      const replay = await a.commit([sent(op.move(nodeId, root, "lost rename.txt"), intent(a.session.deviceId, { replay: true }))]);
      expect(replay.results[0]!.code).toBe(rule === "A" ? OpResult_Code.SUPERSEDED : OpResult_Code.APPLIED);
      expect((await stateOf(space, nodeId))!.status).toBe(rule === "A" ? NodeStatus.TRASHED : NodeStatus.LIVE);
    });
  }

  for (const rule of ["A", "B"] as const) {
    it(`under rule ${rule}, answers a retried Move or Delete whose intent and value a trashed node holds APPLIED and changes nothing`, async () => {
      const { space, a, b, values, root } = await setup();
      await setRule(space, rule);
      const nodeId = await file(a, root, "kept.txt", values[0]!);
      const rename = intent(a.session.deviceId);
      await a.commit([sent(op.move(nodeId, root, "renamed.txt"), rename)]);
      sync(a, b);
      const removal = intent(b.session.deviceId);
      await b.commit([sent(del(nodeId), removal)]);
      const before = (await stateOf(space, nodeId))!;
      const moved = await a.commit([sent(op.move(nodeId, root, "renamed.txt"), rename)]);
      const deleted = await b.commit([sent(del(nodeId, { held: [intentId()] }), removal)]);
      for (const retried of [moved, deleted]) {
        expect(retried.results[0]!.code).toBe(OpResult_Code.APPLIED);
        expect(retried.changes).toEqual([]);
      }
      expect(await stateOf(space, nodeId)).toEqual(before);
    });
  }

  it("under rule B, revives a trashed file for the author's replayed edit that the trash does not list, and supersedes one it lists", async () => {
    const { space, a, b, values, root } = await setup();
    await setRule(space, "B");
    const nodeId = await file(a, root, "draft.txt", values[0]!);
    sync(a, b);
    const listed = intent(a.session.deviceId, { replay: true });
    await b.commit([sent(del(nodeId, { held: [listed.id] }), intent(b.session.deviceId))]);
    const known = await a.commit([sent(update(nodeId, values[1]!), listed)]);
    expect(known.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.TRASHED);
    const lostEdit = intent(a.session.deviceId, { replay: true });
    const lost = await a.commit([sent(update(nodeId, values[2]!), lostEdit)]);
    expect(lost.results[0]!.code).toBe(OpResult_Code.APPLIED);
    const revived = (await stateOf(space, nodeId))!;
    expect(revived.status).toBe(NodeStatus.LIVE);
    expect(idOf(revived.intents!.content)).toBe(idOf(lostEdit));
  });

  it("under rule B, supersedes the author's replayed Restore whose intent is the status the trash replaced", async () => {
    const { space, a, b, values, root } = await setup();
    await setRule(space, "B");
    const nodeId = await file(a, root, "back.txt", values[0]!);
    await a.commit([sent(del(nodeId), intent(a.session.deviceId))]);
    const restore = intent(a.session.deviceId);
    await a.commit([sent(op.restore(nodeId), restore)]);
    sync(a, b);
    await b.commit([sent(del(nodeId), intent(b.session.deviceId))]);
    const replay = await a.commit([sent(op.restore(nodeId), create(IntentSchema, { ...restore, replay: true }))]);
    expect(replay.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect((await stateOf(space, nodeId))!.status).toBe(NodeStatus.TRASHED);
  });

  it("under rule B, supersedes the author's replay whose intent the trash lists", async () => {
    const { space, a, b, values, root } = await setup();
    await setRule(space, "B");
    const nodeId = await file(a, root, "seen.txt", values[0]!);
    sync(a, b);
    const lost = intent(a.session.deviceId, { replay: true });
    await b.commit([sent(del(nodeId, { held: [lost.id] }), intent(b.session.deviceId))]);
    const replay = await a.commit([sent(op.move(nodeId, root, "seen 2.txt"), lost)]);
    expect(replay.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
  });

  it("keeps an old client's resend on the trash SUPERSEDED, as RFC 0018 says", async () => {
    const { a, b, values, root } = await setup();
    const nodeId = await file(a, root, "old.txt", values[0]!);
    await a.commit([op.delete(nodeId)]);
    const resend = await b.commit([update(nodeId, values[1]!, { resend: true })]);
    expect(resend.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
  });
});

describe("RFC 0028 section 3.6: replays the server already holds", () => {
  it("supersedes a replay whose intent another node holds, as a conflict copy does", async () => {
    const { space, a, b, c, values, root } = await setup();
    const nodeId = await file(a, root, "doc.txt", values[0]!);
    sync(a, b, c);
    await a.commit([update(nodeId, values[1]!)]);
    const lost = intent(b.session.deviceId);
    const copied = await b.commit([sent(update(nodeId, values[2]!), lost)]);
    expect(copied.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const replay = await c.commit([sent(update(nodeId, values[2]!), create(IntentSchema, { ...lost, replay: true }))], { baseSeq: a.baseSeq });
    expect(replay.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
    expect(replay.changes.map(changedId)).toEqual([copied.results[0]!.nodeId]);
    expect(idOf((await stateOf(space, nodeId))!.intents!.content)).not.toBe(idOf(lost));
  });
});

describe("RFC 0028 section 3.7: digest v5", () => {
  it("refuses a reused commit_id that differs only in an intent, and keeps v4's digest for a request without intents", async () => {
    const { a, root } = await setup();
    const commitId = uuidv7();
    const dir = uuidv7();
    const first = intent(a.session.deviceId);
    await a.commit([sent(op.mkdir(dir, root, "x"), first)], { commitId });
    const reused = await a.commitRaw([sent(op.mkdir(dir, root, "x"), intent(a.session.deviceId))], { commitId });
    expect(reused.case === "error" && reused.value.code).toBe(Error_Code.INVALID_ARGUMENT);
    const plain = create(OpSchema, op.mkdir(dir, root, "x"));
    const commit = { $typeName: "koidrive.v1.Commit", ops: [plain] } as never;
    expect(toHex(await commitDigest(commit, 5))).toBe(toHex(await commitDigest(commit, 4)));
  });
});

describe("RFC 0028 section 3.8: content conflicts by ancestry", () => {
  it("makes a conflict copy of an edit whose made_over lacks the current version, whatever its base_seq", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "chain.txt", values[0]!);
    const first = (await stateOf(space, nodeId))!.current!.versionId;
    await a.commit([update(nodeId, values[1]!)]);
    sync(a, b);
    const stale = await b.commit([sent(update(nodeId, values[2]!, { madeOver: [first] }), intent(b.session.deviceId))]);
    expect(stale.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    const current = (await stateOf(space, nodeId))!.current!.versionId;
    const over = await b.commit([sent(update(nodeId, values[3]!, { madeOver: [current] }), intent(b.session.deviceId))]);
    expect(over.results[0]!.code).toBe(OpResult_Code.APPLIED);
  });

  it("supersedes a replay whose source_version_id is in the current chain and whose intent the field does not hold", async () => {
    const { space, a, b, values, root } = await setup();
    const nodeId = await file(a, root, "src.txt", values[0]!);
    const source = (await stateOf(space, nodeId))!.current!.versionId;
    await a.commit([update(nodeId, values[1]!, { madeOver: [source] })]);
    sync(a, b);
    const replay = await b.commit([sent(update(nodeId, values[0]!, { sourceVersionId: source }), intent(a.session.deviceId, { replay: true }))]);
    expect(replay.results[0]!.code).toBe(OpResult_Code.SUPERSEDED);
  });
});

describe("RFC 0028 section 7.2: legacy intents", () => {
  it.each(legacyVectors.vectors)("computes the published vector for $note", (vector) => {
    expect(legacyIntentId(vector.node_id, vector.field as Field, vector.key)).toBe(vector.id.replaceAll("-", ""));
  });

  it("encodes a place key as the published vectors do", () => {
    const [, child, root, folder] = legacyVectors.vectors;
    expect(legacyPlaceKey(7, "0190f5c2-0000-7000-8000-0000000000ff", "a.txt")).toBe(child!.key);
    expect(legacyPlaceKey(1, null, "")).toBe(root!.key);
    expect(legacyPlaceKey(12, "0190f5c2-0000-7000-8000-000000000002", "Caf\u00e9.docx")).toBe(folder!.key);
  });

  it("snapshots the backfilled intents, so an R2 restore holds them before the next commit", async () => {
    const { space, a, root } = await setup();
    const now = Date.now();
    await setClock(space, now);
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Docs")]);
    await a.commit([op.move(dir, root, "Papers")]);
    const exportAt = now + DAY_MS + 10 * 60_000;
    await runSpaceAlarm(space.stub, exportAt);
    await runSpaceAlarm(space.stub, exportAt + 60_000);
    const headSeq = (await serverState(space.stub)).headSeq;
    expect((await restoreFromR2(space.spaceId)).manifest!.start_seq).toBe(headSeq);
    await runInDurableObject(space.stub, (_instance, state) => {
      const sql = state.storage.sql;
      for (const table of ["field_intents", "trash_held", "tombstones"]) sql.exec(`DROP TABLE ${table}`);
      sql.exec("ALTER TABLE nodes DROP COLUMN status_before");
      sql.exec("ALTER TABLE nodes DROP COLUMN copy_mark");
      sql.exec("UPDATE meta SET value = ? WHERE key = 'schema_version'", MIGRATIONS.length - 1);
    });
    await evictDurableObject(space.stub);
    const server = await serverState(space.stub);
    expect(idOf((await stateOf(space, dir))!.intents!.status)).toBe(legacyIntentId(dir, "status", `live:${headSeq - 1}`));
    await runSpaceAlarm(space.stub, exportAt + 2 * 60_000);
    await runSpaceAlarm(space.stub, exportAt + 3 * 60_000);
    const restored = await restoreFromR2(space.spaceId);
    expect(restored.headSeq).toBe(headSeq);
    expect(restored.nodes).toEqual(server.nodes);
  });

  it("gives every field of a store from before the migration its legacy intent, with the time of the commit at its revision", async () => {
    const { space, a, b, values, root } = await setup();
    const dir = uuidv7();
    await a.commit([op.mkdir(dir, root, "Docs")]);
    const live = await file(a, dir, "live.txt", values[0]!);
    const gone = await file(a, dir, "gone.txt", values[1]!);
    sync(a, b);
    await b.commit([op.delete(gone)]);
    await a.commit([op.delete(live)]);
    await a.commit([op.restore(live)]);
    const step = MIGRATIONS.length - 1;
    const rows = await runInDurableObject(space.stub, (_instance, state) => {
      const sql = state.storage.sql;
      for (const table of ["field_intents", "trash_held", "tombstones"]) sql.exec(`DROP TABLE ${table}`);
      sql.exec("ALTER TABLE nodes DROP COLUMN status_before");
      sql.exec("ALTER TABLE nodes DROP COLUMN copy_mark");
      sql.exec("UPDATE meta SET value = ? WHERE key = 'schema_version'", step);
      return {
        nodes: sql.exec("SELECT node_id, parent_id, name, status, content_rev, meta_rev, cur_version_id, trash_group FROM nodes").toArray(),
        commits: new Map(sql.exec("SELECT seq, committed_at_ms FROM commits").toArray().map((row) => [Number(row.seq), BigInt(Number(row.committed_at_ms))])),
      };
    });
    await evictDurableObject(space.stub);
    const restoredSeq = Number((await stateOf(space, live))!.metaRev);
    for (const row of rows.nodes) {
      const nodeId = String(row.node_id);
      const { intents } = (await stateOf(space, nodeId))!;
      const metaRev = Number(row.meta_rev);
      expect(idOf(intents!.place)).toBe(legacyIntentId(nodeId, "place", legacyPlaceKey(metaRev, row.parent_id === null ? null : String(row.parent_id), String(row.name))));
      expect(intents!.place).toMatchObject({ authorDeviceId: "", replay: false, appliedAtMs: rows.commits.get(metaRev) ?? 0n });
      if (row.cur_version_id) {
        expect(idOf(intents!.content)).toBe(legacyIntentId(nodeId, "content", String(row.cur_version_id)));
        expect(intents!.content!.appliedAtMs).toBe(rows.commits.get(Number(row.content_rev)) ?? 0n);
      } else expect(intents!.content).toBeUndefined();
      if (Number(row.status) === 2) expect(idOf(intents!.status)).toBe(legacyIntentId(nodeId, "status", `trashed:${row.trash_group}`));
    }
    expect(idOf((await stateOf(space, live))!.intents!.status)).toBe(legacyIntentId(live, "status", `live:${restoredSeq}`));
    expect(idOf((await stateOf(space, root))!.intents!.status)).toBe(legacyIntentId(root, "status", "live:0"));
    const moved = await a.commit([sent(op.move(live, root, "live.txt"), intent(a.session.deviceId, { over: (await stateOf(space, live))!.intents!.place!.id }))]);
    expect(moved.results[0]!.code).toBe(OpResult_Code.APPLIED);
  });
});

describe("RFC 0028 and RFC 0004 section 5: the export", () => {
  it("rebuilds intents, the trash's held list and tombstones from the R2 export", async () => {
    const { space, a, b, values, root } = await setup();
    const now = Date.now();
    await setClock(space, now);
    const kept = await file(a, root, "kept.txt", values[0]!, intent(b.session.deviceId, { replay: true, appliedAtMs: 7n }));
    const trashed = await file(a, root, "trashed.txt", values[1]!);
    const purged = await file(a, root, "purged.txt", values[2]!);
    const purgedContent = idOf((await stateOf(space, purged))!.intents!.content);
    await a.commit([sent(del(purged), intent(a.session.deviceId))]);
    await setClock(space, now + 10 * DAY_MS);
    const lost = intentId();
    await a.commit([sent(del(trashed, { held: [lost] }), intent(a.session.deviceId))]);
    const snapshotAt = now + 30 * DAY_MS - DAY_MS / 2;
    await setClock(space, snapshotAt);
    await runSpaceAlarm(space.stub, snapshotAt);
    await runSpaceAlarm(space.stub, snapshotAt + 60_000);
    const gc = await runInDurableObject(space.stub, (instance: SpaceDO) => instance.collectGarbage(snapshotAt + DAY_MS / 2 + 60_000));
    expect(gc.purgedNodes).toBe(1);
    const purgedAt = (await serverState(space.stub)).headSeq;
    await runSpaceAlarm(space.stub, snapshotAt + DAY_MS / 2 + 10 * 60_000);
    const replayed = await restoreFromR2(space.spaceId);
    expect(replayed.manifest!.start_seq).toBeLessThan(purgedAt);
    expect(replayed.headSeq).toBe(purgedAt);
    expect(replayed.tombstones).toEqual((await serverState(space.stub)).tombstones);
    await runSpaceAlarm(space.stub, now + 32 * DAY_MS, { pageNodes: 1 });
    const restored = await restoreFromR2(space.spaceId);
    expect(restored.manifest!.start_seq).toBeGreaterThanOrEqual(purgedAt);
    const server = await serverState(space.stub);
    expect(restored.nodes).toEqual(server.nodes);
    expect(restored.tombstones).toEqual(server.tombstones);
    expect(restored.nodes.get(kept)!.state.intents!.content).toMatchObject({ replay: true, appliedAtMs: 7n });
    expect(restored.nodes.get(trashed)!.state.trashHeld.map(toHex)).toEqual([toHex(lost)]);
    const tombstone = restored.tombstones.get(purged)!;
    expect(tombstone.lastParentId).toBe(root);
    expect(toHex(tombstone.intents!.content!.id)).toBe(purgedContent);
  });
});
