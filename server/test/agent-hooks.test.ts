// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type CommitResult, CommitSchema, OpResult_Code, OpSchema, PlaceSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ContentSchema } from "../src/gen/koidrive/v1/model_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { sha256 } from "../src/crypto";
import { errorReply } from "../src/http";
import { toHex, uuidv7 } from "../src/ids";
import { policy } from "../src/policy";
import { commitDigest } from "../src/space/digest";
import { client, content, newSpace, op, type TestClient } from "./helpers";

afterEach(() => {
  vi.restoreAllMocks();
});

async function setup() {
  const space = await newSpace();
  const a = await client(space, "Laptop A");
  const b = await client(space, "Desktop B");
  return { ...space, a, b, root: space.spaceId };
}

function code(result: Awaited<ReturnType<TestClient["commitRaw"]>>) {
  return result.case === "error" ? result.value.code : result.case;
}

async function createFile(c: TestClient, parentId: string, name: string): Promise<string> {
  const parts = await content(name);
  const receipts = await c.uploadParts(parts);
  const nodeId = uuidv7();
  await c.commit([op.create(nodeId, parentId, name, parts.content)], { receipts });
  return nodeId;
}

function sql(c: TestClient, query: string, ...bindings: unknown[]) {
  return runInDurableObject(c.stub, (_instance, state) => state.storage.sql.exec(query, ...bindings).toArray());
}

async function head(c: TestClient): Promise<number> {
  return Number((await sql(c, "SELECT value FROM meta WHERE key = 'head_seq'"))[0]!.value);
}

async function status(c: TestClient, nodeId: string): Promise<number | undefined> {
  const row = (await sql(c, "SELECT status FROM nodes WHERE node_id = ?", nodeId))[0];
  return row === undefined ? undefined : Number(row.status);
}

const LIVE = 1;

describe("RFC 0006 section 8: digest v1", () => {
  it("hashes the proto3 encoding of base_seq, ops, device_seq and require_head, and nothing else", async () => {
    const encoded = [0x10, 0x01, 0x1a, 0x05, 0x2a, 0x03, 0x0a, 0x01, 0x6e, 0x30, 0x02];
    const expected = toHex(await sha256(new Uint8Array(encoded)));
    const commit = create(CommitSchema, {
      commitId: uuidv7(),
      baseSeq: 1n,
      ops: [op.delete("n")],
      receipts: [{ hash: new Uint8Array(32), size: 1, token: "receipt" }],
      historyId: uuidv7(),
      deviceSeq: 2n,
      runId: "run",
    });
    expect(toHex(await commitDigest(commit))).toBe(expected);
    const withUnknown = [0x10, 0x01, 0x1a, 0x07, 0x2a, 0x05, 0x0a, 0x01, 0x6e, 0x70, 0x05, 0x30, 0x02, 0x78, 0x01];
    expect(toHex(await commitDigest(fromBinary(CommitSchema, new Uint8Array(withUnknown))))).toBe(expected);
    const requires = { nodeId: "o", contentRev: 2n, metaRev: 1n };
    const withRequires = create(CommitSchema, { baseSeq: 1n, deviceSeq: 2n, ops: [create(OpSchema, { op: { case: "delete", value: { nodeId: "n", requires } } })] });
    expect(toHex(await commitDigest(withRequires))).toBe(expected);

    const empty = create(CommitSchema, { ops: [create(OpSchema, { op: { case: "delete", value: {} } })], requireHead: true });
    expect(toHex(await commitDigest(empty))).toBe(toHex(await sha256(new Uint8Array([0x1a, 0x02, 0x2a, 0x00, 0x38, 0x01]))));
  });

  it("covers resend from digest v2 and leaves digest v1 as it was", async () => {
    const parts = await content("one");
    const update = (resend: boolean) =>
      create(CommitSchema, { baseSeq: 1n, deviceSeq: 2n, ops: [create(OpSchema, { op: { case: "updateContent", value: { nodeId: "n", content: parts.content, resend } } })] });
    const [plain, resent] = [update(false), update(true)];
    expect(toHex(await commitDigest(plain, 1))).toBe(toHex(await commitDigest(plain, 2)));
    expect(toHex(await commitDigest(resent, 1))).toBe(toHex(await commitDigest(plain, 1)));
    expect(toHex(await commitDigest(resent, 2))).not.toBe(toHex(await commitDigest(plain, 2)));
  });

  it("covers pending marks from digest v3 and leaves digest v2 as it was", async () => {
    const marked = (pendingFor: string) =>
      create(CommitSchema, {
        baseSeq: 1n,
        deviceSeq: 2n,
        ops: [
          create(OpSchema, { op: { case: "createDir", value: { nodeId: "d", parentId: "r", name: "a (pending)", pendingFor } } }),
          create(OpSchema, { op: { case: "move", value: { nodeId: "m", newParentId: "r", newName: "b (pending)", pendingFor } } }),
        ],
      });
    const [plain, pending] = [marked(""), marked("a")];
    expect(toHex(await commitDigest(plain, 2))).toBe(toHex(await commitDigest(plain, 3)));
    expect(toHex(await commitDigest(pending, 2))).toBe(toHex(await commitDigest(plain, 2)));
    expect(toHex(await commitDigest(pending, 3))).not.toBe(toHex(await commitDigest(plain, 3)));
    for (const at of [0, 1]) {
      const one = create(CommitSchema, { ...plain, ops: plain.ops.map((each, index) => (index === at ? pending.ops[index]! : each)) });
      expect(toHex(await commitDigest(one, 3))).not.toBe(toHex(await commitDigest(plain, 3)));
    }
  });

  it("covers a move's from from digest v3 and leaves digest v2 as it was", async () => {
    const moved = (from?: { parentId: string; name: string }) =>
      create(CommitSchema, {
        baseSeq: 1n,
        deviceSeq: 2n,
        ops: [create(OpSchema, { op: { case: "move", value: { nodeId: "m", newParentId: "r", newName: "b", from: from && create(PlaceSchema, from) } } })],
      });
    const [plain, guarded, elsewhere] = [moved(), moved({ parentId: "r", name: "b (pending)" }), moved({ parentId: "r", name: "c" })];
    expect(toHex(await commitDigest(guarded, 2))).toBe(toHex(await commitDigest(plain, 2)));
    expect(toHex(await commitDigest(guarded, 3))).not.toBe(toHex(await commitDigest(plain, 3)));
    expect(toHex(await commitDigest(guarded, 3))).not.toBe(toHex(await commitDigest(elsewhere, 3)));
  });

  it("changes with every covered field and keeps the request's order", async () => {
    const parts = await content("one", "two");
    const file = (init: Partial<typeof parts.content>) => op.create("f", "d", "f.txt", create(ContentSchema, { ...parts.content, ...init }));
    const base = { baseSeq: 3n, deviceSeq: 4n, ops: [op.mkdir("d", "r", "D"), file({})] };
    const digest = async (init: Parameters<typeof create<typeof CommitSchema>>[1]) => toHex(await commitDigest(create(CommitSchema, init)));
    const reference = await digest(base);
    expect(await digest({ ...base, commitId: uuidv7(), historyId: uuidv7(), runId: "run", receipts: [{ size: 3 }] })).toBe(reference);
    const variants = [
      { ...base, baseSeq: 2n },
      { ...base, deviceSeq: 5n },
      { ...base, requireHead: true },
      { ...base, ops: [...base.ops].reverse() },
      { ...base, ops: [base.ops[0]!, file({ chunks: [...parts.content.chunks].reverse() })] },
      { ...base, ops: [base.ops[0]!, file({ mtimeNs: 1n })] },
      { ...base, ops: [base.ops[0]!, file({ executable: true })] },
    ];
    for (const variant of variants) expect(await digest(variant)).not.toBe(reference);
  });
});

describe("RFC 0006 section 8: stored results", () => {
  it("stores the request's digest with each result, in the log and as the device's latest commit", async () => {
    const { a, root } = await setup();
    await a.commit([op.mkdir(uuidv7(), root, "Docs")]);
    expect(await sql(a, "SELECT length(request_digest) AS bytes FROM commits")).toEqual([{ bytes: 32 }]);
    expect(await sql(a, "SELECT length(request_digest) AS bytes FROM device_commits")).toEqual([{ bytes: 32 }]);
  });

  it("returns a stored result only to the device that made the commit, and only for the same request", async () => {
    const { a, b, root } = await setup();
    const parts = await content("receipted");
    const receipts = await a.uploadParts(parts);
    const commitId = uuidv7();
    const ops = [op.create(uuidv7(), root, "f.txt", parts.content)];
    const first = await a.commit(ops, { commitId, receipts });
    expect(await a.commit(ops, { commitId, receipts: [] })).toEqual(first);
    for (const init of [{ baseSeq: 1n }, { deviceSeq: 2n }, { requireHead: true }]) {
      expect(code(await a.commitRaw(ops, { commitId, ...init }))).toBe(Error_Code.INVALID_ARGUMENT);
    }
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "Other")], { commitId }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect(code(await b.commitRaw(ops, { commitId }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect(await head(a)).toBe(1);
    expect((await b.commit([op.mkdir(uuidv7(), root, "B")])).seq).toBe(2n);
  });

  it("refuses a reuse that differs only in a pending mark", async () => {
    const { a, root } = await setup();
    const [nodeId, commitId] = [uuidv7(), uuidv7()];
    const mkdir = (pendingFor: string) =>
      create(OpSchema, { op: { case: "createDir", value: { nodeId, parentId: root, name: "a (pending)", pendingFor } } });
    const first = await a.commit([mkdir("a")], { commitId });
    expect(await a.commit([mkdir("a")], { commitId })).toEqual(first);
    expect(code(await a.commitRaw([mkdir("")], { commitId }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect(code(await a.commitRaw([mkdir("b")], { commitId }))).toBe(Error_Code.INVALID_ARGUMENT);
  });

  it("checks the device's latest commit once the log no longer holds it, and never returns a commit without a device", async () => {
    const { a, root } = await setup();
    const kept = uuidv7();
    const keptOps = [op.mkdir(uuidv7(), root, "Kept")];
    const result = await a.commit(keptOps, { commitId: kept });
    await sql(a, "DELETE FROM commits WHERE commit_id = ?", kept);
    expect(code(await a.commitRaw([op.mkdir(uuidv7(), root, "Other")], { commitId: kept }))).toBe(Error_Code.INVALID_ARGUMENT);
    expect(await a.commit(keptOps, { commitId: kept })).toEqual(result);

    const server = uuidv7();
    const serverOps = [op.mkdir(uuidv7(), root, "Server")];
    await a.commit(serverOps, { commitId: server });
    await sql(a, "UPDATE commits SET device_id = '' WHERE commit_id = ?", server);
    expect(code(await a.commitRaw(serverOps, { commitId: server }))).toBe(Error_Code.INVALID_ARGUMENT);
  });

  it("returns a result stored before RFC 0006 to its own device without the body check", async () => {
    const { a, b, root } = await setup();
    const commitId = uuidv7();
    const result: CommitResult = await a.commit([op.mkdir(uuidv7(), root, "Old")], { commitId });
    await sql(a, "UPDATE commits SET request_digest = NULL");
    await sql(a, "UPDATE device_commits SET request_digest = NULL");
    expect(await a.commit([op.mkdir(uuidv7(), root, "Body unknown")], { commitId })).toEqual(result);
    expect(code(await b.commitRaw([op.mkdir(uuidv7(), root, "Old")], { commitId }))).toBe(Error_Code.INVALID_ARGUMENT);
  });
});

describe("RFC 0006 section 3: authorize", () => {
  it("passes each op's transition, read from the tree just before and after that op, in op order", async () => {
    const { a, root } = await setup();
    const [docs, archive, sub, added] = [uuidv7(), uuidv7(), uuidv7(), uuidv7()];
    await a.commit([op.mkdir(docs, root, "Docs"), op.mkdir(archive, root, "Archive"), op.mkdir(sub, docs, "Sub")]);
    const file = await createFile(a, sub, "f.txt");
    const sorted = (ids: Iterable<string>) => [...ids].sort();
    const seen: unknown[] = [];
    const authorize = vi.spyOn(policy, "authorize").mockImplementation((principal, applied, effect) => {
      const changed = sorted(effect.changed);
      seen.push({
        principal: [principal.deviceId, principal.kind, principal.scope],
        op: applied.op.case,
        nodes: sorted(effect.nodes),
        changed,
        created: sorted(effect.created),
        before: Object.fromEntries(changed.map((id) => [id, effect.ancestryBefore(id)])),
        after: Object.fromEntries(changed.map((id) => [id, effect.ancestryAfter(id)])),
        subtree: sorted(effect.subtree(docs)),
        file: [effect.ancestryBefore(file), effect.ancestryAfter(file)],
      });
      return true;
    });
    await a.commit([op.move(docs, archive, "Docs"), op.mkdir(added, sub, "Added"), op.delete(docs), op.restore(docs)]);
    expect(authorize).toHaveBeenCalledTimes(4);
    const principal = [a.session.deviceId, "device", null];
    const below = sorted([sub, file, added]);
    const everything = sorted([docs, sub, file, added]);
    const moved = [root, archive, docs, sub];
    expect(seen).toEqual([
      {
        principal,
        op: "move",
        nodes: sorted([docs, archive]),
        changed: [docs],
        created: [],
        before: { [docs]: [root] },
        after: { [docs]: [root, archive] },
        subtree: sorted([sub, file]),
        file: [[root, docs, sub], moved],
      },
      {
        principal,
        op: "createDir",
        nodes: sorted([added, sub]),
        changed: [added],
        created: [added],
        before: { [added]: undefined },
        after: { [added]: moved },
        subtree: below,
        file: [moved, moved],
      },
      {
        principal,
        op: "delete",
        nodes: [docs],
        changed: everything,
        created: [],
        before: { [docs]: [root, archive], [sub]: [root, archive, docs], [file]: moved, [added]: moved },
        after: { [docs]: [root, archive], [sub]: [root, archive, docs], [file]: moved, [added]: moved },
        subtree: below,
        file: [moved, moved],
      },
      {
        principal,
        op: "restore",
        nodes: [docs],
        changed: everything,
        created: [],
        before: { [docs]: [root, archive], [sub]: [root, archive, docs], [file]: moved, [added]: moved },
        after: { [docs]: [root, archive], [sub]: [root, archive, docs], [file]: moved, [added]: moved },
        subtree: below,
        file: [moved, moved],
      },
    ]);
  });

  it("names the folder an R2 merge resolved to, for the merging op and for later ops that use the merged ID", async () => {
    const { a, root } = await setup();
    const existing = uuidv7();
    await a.commit([op.mkdir(existing, root, "Docs")]);
    const [alias, child] = [uuidv7(), uuidv7()];
    const seen: unknown[] = [];
    vi.spyOn(policy, "authorize").mockImplementation((_principal, _applied, effect) => {
      seen.push({
        nodes: [...effect.nodes].sort(),
        changed: [...effect.changed],
        created: [...effect.created],
        alias: [effect.ancestryBefore(alias), effect.ancestryAfter(alias), effect.subtree(alias)],
      });
      return true;
    });
    const merged = await a.commit([op.mkdir(alias, root, "DOCS"), op.mkdir(child, alias, "Inside")]);
    expect(merged.results[0]).toMatchObject({ code: OpResult_Code.MERGED_INTO, nodeId: existing });
    expect(seen).toEqual([
      { nodes: [existing, root].sort(), changed: [], created: [], alias: [[root], [root], []] },
      { nodes: [child, existing].sort(), changed: [child], created: [child], alias: [[root], [root], [child]] },
    ]);
  });

  it("refuses the whole commit with FORBIDDEN when one op is refused, storing nothing and leaving the slot free", async () => {
    const { a, root } = await setup();
    const file = await createFile(a, root, "kept.txt");
    const before = await head(a);
    const dir = uuidv7();
    vi.spyOn(policy, "authorize").mockImplementation((_principal, applied) => applied.op.case !== "delete");
    expect(code(await a.commitRaw([op.mkdir(dir, root, "New"), op.delete(file)]))).toBe(Error_Code.FORBIDDEN);
    expect(await head(a)).toBe(before);
    expect(await status(a, dir)).toBeUndefined();
    expect(await status(a, file)).toBe(LIVE);
    vi.restoreAllMocks();
    expect((await a.commit([op.mkdir(dir, root, "New")])).seq).toBe(BigInt(before + 1));
    expect(await sql(a, "SELECT device_seq FROM device_commits")).toEqual([{ device_seq: 2 }]);
  });
});

describe("RFC 0006 section 10: delete count", () => {
  it("counts the live files a commit leaves in the trash, without the ones R7 keeps or a later op restores", async () => {
    const { a, b, root, spaceId } = await setup();
    const [docs, sub] = [uuidv7(), uuidv7()];
    await a.commit([op.mkdir(docs, root, "Docs"), op.mkdir(sub, docs, "Sub")]);
    const edited = await createFile(a, docs, "edited.txt");
    await createFile(a, sub, "one.txt");
    await createFile(a, sub, "two.txt");
    const loose = await createFile(a, root, "loose.txt");
    const update = await content("edited elsewhere");
    b.baseSeq = a.baseSeq;
    await b.commit([op.update(edited, update.content)], { receipts: await b.uploadParts(update) });

    const counts: unknown[] = [];
    vi.spyOn(policy, "deletes").mockImplementation((principal, space, trashedFiles) => {
      counts.push([principal.deviceId, space, trashedFiles]);
      return true;
    });
    await a.commit([op.delete(docs)]);
    expect(await status(a, edited)).toBe(LIVE);
    await a.commit([op.delete(loose), op.restore(loose)]);
    await a.commit([op.mkdir(uuidv7(), root, "Nothing deleted")]);
    expect(counts).toEqual([
      [a.session.deviceId, spaceId, 2],
      [a.session.deviceId, spaceId, 0],
      [a.session.deviceId, spaceId, 0],
    ]);
  });

  it("refuses with CONFIRMATION_REQUIRED when the policy asks for confirmation, storing nothing and leaving the slot free", async () => {
    const { a, root } = await setup();
    const file = await createFile(a, root, "precious.txt");
    const before = await head(a);
    vi.spyOn(policy, "deletes").mockReturnValue(false);
    expect(code(await a.commitRaw([op.delete(file)]))).toBe(Error_Code.CONFIRMATION_REQUIRED);
    expect(await head(a)).toBe(before);
    expect(await status(a, file)).toBe(LIVE);
    vi.restoreAllMocks();
    expect((await a.commit([op.delete(file)])).seq).toBe(BigInt(before + 1));
  });

  it("maps CONFIRMATION_REQUIRED to 409 over HTTP", () => {
    expect(errorReply(new Request("https://koidrive.test/v1/spaces"), Error_Code.CONFIRMATION_REQUIRED, "confirm").status).toBe(409);
  });
});
