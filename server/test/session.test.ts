// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { TokenRequestSchema } from "../src/gen/koidrive/v1/auth_pb";
import { CommitResultSchema, OpResult_Code, UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import { Notice_Kind, ResyncRequired_Reason, RpcRequestSchema, RpcResponseSchema, ServerMessageSchema } from "../src/gen/koidrive/v1/messages_pb";
import { ChunkRefSchema, ContentSchema } from "../src/gen/koidrive/v1/model_pb";
import { Error_Cause, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { sha256 } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import { Close, type SpaceDO } from "../src/space-do";
import { CallFailure } from "../src/space/commit";
import { encodeChunkList } from "../src/space/store";
import { content, op } from "./helpers";
import { BASE, account, call, body, getOwner, pairDevice, createSpace, type CommitToken, tokenOf, Socket, put, prepared, nodeIds } from "./session";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("WebSocket session", () => {
  it("welcomes a device whose latest commit has an unknown digest version", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const first = await Socket.connect(spaceId, owner);
    await first.commit((await first.hello()).headSeq, [op.mkdir(uuidv7(), spaceId, "D")]);
    first.ws.close();
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => {
      for (const table of ["commits", "device_commits"]) state.storage.sql.exec(`UPDATE ${table} SET digest_version = 99`);
    });
    expect((await (await Socket.connect(spaceId, owner)).hello()).deviceSeq).toBe(1n);
  });

  async function twoDevices() {
    const a = await getOwner();
    const b = await pairDevice(a, "Desktop B");
    const spaceId = await createSpace(a);
    return { a, b, spaceId, stub: env.SPACE.get(env.SPACE.idFromName(spaceId)) };
  }

  it("rejects a commit whose change records do not fit in one 1 MiB message and keeps every frame under 1 MiB", { timeout: 30_000 }, async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const snapshot = () =>
      runInDurableObject(stub, (_instance, state) => {
        const all = (query: string) => state.storage.sql.exec(query).toArray();
        return {
          head: all("SELECT value FROM meta WHERE key = 'head_seq'"),
          commits: all("SELECT seq FROM commits"),
          nodes: all("SELECT node_id, name, status, meta_rev, content_rev FROM nodes ORDER BY node_id"),
          versions: all("SELECT version_id FROM versions"),
          chunks: all("SELECT hex(hash) AS hash, state, refcount FROM chunks ORDER BY hash"),
        };
      });
    const writer = await Socket.connect(spaceId, a);
    await writer.start();
    const reader = await Socket.connect(spaceId, b);
    await reader.start();
    const parts = await content(...Array.from({ length: 16_000 }, () => "x"));
    const plan = await writer.rpc({ case: "prepareUpload", value: { chunks: [parts.content.chunks[0]!] } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const uploaded = await put(plan.value.chunks[0]!.target!, parts.bytes[0]!);
    const receipts = [fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()))];
    const before = await snapshot();
    const both = await writer.rpc({
      case: "commit",
      value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "one", parts.content), op.create(uuidv7(), spaceId, "two", parts.content)], receipts, ...writer.slot() },
    });
    expect(both.case === "error" && both.value.code).toBe(Error_Code.LIMIT_EXCEEDED);
    expect(await snapshot()).toEqual(before);

    const frameSize = (msg: MessageInitShape<typeof ServerMessageSchema>["msg"]) => toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg })).length;
    for (const name of ["one", "two"]) {
      const result = await writer.commit(0n, [op.create(uuidv7(), spaceId, name, parts.content)], receipts);
      expect(frameSize({ case: "rpc", value: { reqId: 1n, result: { case: "commit", value: result } } })).toBeLessThanOrEqual(1024 * 1024);
      const live = await reader.next();
      expect(live.case).toBe("changes");
      expect(frameSize(live)).toBeLessThanOrEqual(1024 * 1024);
    }
    const late = await Socket.connect(spaceId, b);
    const welcome = await late.hello();
    late.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq: 0n } });
    const sizes: number[] = [];
    for (let msg = await late.next(); msg.case !== "caughtUp"; msg = await late.next()) sizes.push(frameSize(msg));
    expect(sizes).toHaveLength(2);
    expect(Math.max(...sizes)).toBeLessThanOrEqual(1024 * 1024);
  });

  it("answers a client's close at once with the client's code, so the client sees a clean close", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    for (const [code, reason] of [
      [1000, "bye"],
      [4000, "done"],
      [undefined, undefined],
    ] as const) {
      const socket = await Socket.connect(spaceId, owner);
      await socket.start();
      expect((await socket.rpc({ case: "listTree", value: {} })).case).toBe("tree");
      const startedMs = Date.now();
      const closed = new Promise<CloseEvent>((resolve) => socket.ws.addEventListener("close", resolve));
      socket.ws.close(code, reason);
      const event = await closed;
      expect({ code: event.code, wasClean: event.wasClean }).toEqual({ code: code ?? 1005, wasClean: true });
      expect(Date.now() - startedMs).toBeLessThan(2000);
    }
  });

  it("closes a connection on a message over 8 MiB before parsing it, and refuses an RPC body over 8 MiB", async () => {
    const { a, spaceId } = await twoDevices();
    const exact = await Socket.connect(spaceId, a);
    exact.ws.send(new Uint8Array(8 * 1024 * 1024));
    expect(await exact.waitClosed()).toMatchObject({ code: Close.PROTOCOL, reason: "malformed ClientMessage" });
    const over = await Socket.connect(spaceId, a);
    over.ws.send(new Uint8Array(8 * 1024 * 1024 + 1));
    expect(await over.waitClosed()).toMatchObject({ code: Close.PROTOCOL, reason: "message larger than 8 MiB" });
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: new Uint8Array(8 * 1024 * 1024 + 1) }, RpcResponseSchema);
    expect(rpc.status).toBe(413);
  });

  it("pages ListVersions, ListTree and ListTrash so that no reply exceeds 1 MiB over HTTP or WebSocket", { timeout: 30_000 }, async () => {
    const { a, spaceId } = await twoDevices();
    const writer = await Socket.connect(spaceId, a);
    await writer.start();
    const parts = await content(...Array.from({ length: 16_000 }, () => "x"));
    const plan = await writer.rpc({ case: "prepareUpload", value: { chunks: [parts.content.chunks[0]!] } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const uploaded = await put(plan.value.chunks[0]!.target!, parts.bytes[0]!);
    const receipts = [fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()))];
    const [one, two] = [uuidv7(), uuidv7()];
    await writer.commit(0n, [op.create(one, spaceId, "one", parts.content)], receipts);
    await writer.commit(1n, [op.update(one, parts.content)], receipts);
    await writer.commit(2n, [op.create(two, spaceId, "two", parts.content)], receipts);
    const frame = (result: MessageInitShape<typeof RpcResponseSchema>["result"]) =>
      toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 1n, result } } })).length;

    const pages = async (request: (cursor: string) => MessageInitShape<typeof RpcRequestSchema>["call"]) => {
      const seen: { http: number; socket: number; items: number }[] = [];
      for (let cursor = "", first = true; first || cursor !== ""; first = false) {
        const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", {
          method: "POST",
          headers: { Authorization: "Bearer " + a.access!.token },
          body: body(RpcRequestSchema, { reqId: 1n, call: request(cursor) }),
        });
        const httpBytes = new Uint8Array(await http.arrayBuffer());
        const result = await writer.rpc(request(cursor));
        expect(fromBinary(RpcResponseSchema, httpBytes).result.case).toBe(result.case);
        if (result.case !== "versions" && result.case !== "tree" && result.case !== "trash") throw new Error("unexpected " + result.case);
        const items = result.case === "versions" ? result.value.versions.length : result.value.nodes.length;
        seen.push({ http: httpBytes.length, socket: frame(result), items });
        cursor = result.value.nextCursor;
      }
      return seen;
    };
    const versions = await pages((cursor) => ({ case: "listVersions", value: { nodeId: one, cursor } }));
    expect(versions.map((p) => p.items)).toEqual([1, 1]);
    const tree = await pages((cursor) => ({ case: "listTree", value: { cursor } }));
    expect(tree.reduce((sum, p) => sum + p.items, 0)).toBe(3);
    expect(tree.length).toBeGreaterThan(1);
    await writer.commit(3n, [op.delete(one)]);
    await writer.commit(4n, [op.delete(two)]);
    const trash = await pages((cursor) => ({ case: "listTrash", value: { cursor } }));
    expect(trash.reduce((sum, p) => sum + p.items, 0)).toBe(2);
    for (const page of [...versions, ...tree, ...trash]) {
      expect(page.http).toBeLessThanOrEqual(1024 * 1024);
      expect(page.socket).toBeLessThanOrEqual(1024 * 1024);
    }
  });

  it("lists at most 1,000 missing chunks so the error fits in 1 MiB, and lists the rest once those are uploaded", async () => {
    const { a, spaceId, stub } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    const chunks = Array.from({ length: 32_768 }, (_, i) => {
      const hash = new Uint8Array(32);
      new DataView(hash.buffer).setUint32(0, i);
      return create(ChunkRefSchema, { hash, size: 1 });
    });
    const many = create(ContentSchema, { chunks, chunkCount: chunks.length, size: BigInt(chunks.length), manifestDigest: await sha256(encodeChunkList(chunks)) });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "missing", many)], ...socket.slot() } };
    const requestBytes = body(RpcRequestSchema, { reqId: 1n, call: request });
    expect(requestBytes.length).toBeLessThan(8 * 1024 * 1024);
    const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", { method: "POST", headers: { Authorization: "Bearer " + a.access!.token }, body: requestBytes });
    const httpBytes = new Uint8Array(await http.arrayBuffer());
    const viaHttp = fromBinary(RpcResponseSchema, httpBytes).result;
    const viaSocket = await socket.rpc(request);
    const wsBytes = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 1n, result: viaSocket } } })).length;
    for (const result of [viaHttp, viaSocket]) {
      expect(result.case === "error" && result.value.code).toBe(Error_Code.MISSING_CHUNKS);
      expect(result.case === "error" && result.value.missingChunks).toEqual(chunks.slice(0, 1000).map((chunk) => chunk.hash));
    }
    expect(httpBytes.length).toBeLessThanOrEqual(1024 * 1024);
    expect(wsBytes).toBeLessThanOrEqual(1024 * 1024);
    expect(await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT seq FROM commits").toArray())).toEqual([]);

    const parts = await content(...Array.from({ length: 1010 }, (_, i) => "chunk " + i));
    const commit = { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(uuidv7(), spaceId, "big", parts.content)], ...socket.slot() };
    const receipts: MessageInitShape<typeof UploadReceiptSchema>[] = [];
    const listed = async () => {
      const result = await socket.rpc({ case: "commit", value: { ...commit, receipts } });
      return result.case === "error" && result.value.code === Error_Code.MISSING_CHUNKS ? result.value.missingChunks : result.case;
    };
    const uploadListed = async (hashes: Uint8Array[]) => {
      const indexes = hashes.map((hash) => parts.content.chunks.findIndex((chunk) => toHex(chunk.hash) === toHex(hash)));
      const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: indexes.map((i) => parts.content.chunks[i]!) } });
      if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
      for (const [n, chunk] of plan.value.chunks.entries()) {
        const uploaded = await put(chunk.target!, parts.bytes[indexes[n]!]!);
        receipts.push(fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer())));
      }
    };
    const first = await listed();
    expect(Array.isArray(first) && first.length).toBe(1000);
    await uploadListed(first as Uint8Array[]);
    const rest = await listed();
    expect(Array.isArray(rest) && rest.map(toHex)).toEqual(parts.content.chunks.slice(1000).map((chunk) => toHex(chunk.hash)));
    await uploadListed(rest as Uint8Array[]);
    expect(await listed()).toBe("commit");
  }, 30_000);

  it("replaces an error reply that would exceed 1 MiB with a generic LIMIT_EXCEEDED, over HTTP and WebSocket", async () => {
    const { a, spaceId, stub } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    const hashes = Array.from({ length: 40_000 }, (_, i) => {
      const hash = new Uint8Array(32);
      new DataView(hash.buffer).setUint32(0, i);
      return hash;
    });
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new CallFailure(Error_Code.MISSING_CHUNKS, "an error with more missing chunks than one message holds", hashes);
      };
    });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "listTree", value: {} };
    const http = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/rpc", {
      method: "POST",
      headers: { Authorization: "Bearer " + a.access!.token },
      body: body(RpcRequestSchema, { reqId: 1n, call: request }),
    });
    const httpBytes = new Uint8Array(await http.arrayBuffer());
    const viaSocket = await socket.rpc(request);
    const wsBytes = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: 2n, result: viaSocket } } })).length;
    for (const result of [fromBinary(RpcResponseSchema, httpBytes).result, viaSocket]) {
      expect(result).toMatchObject({ case: "error", value: { code: Error_Code.LIMIT_EXCEEDED, message: "the error reply does not fit in one 1 MiB message", missingChunks: [] } });
    }
    expect(httpBytes.length).toBeLessThanOrEqual(1024 * 1024);
    expect(wsBytes).toBeLessThanOrEqual(1024 * 1024);
    expect(socket.closed).toBeUndefined();
  });

  it("closes a connection on any text message except ping, an oversized one included", async () => {
    const { a, spaceId } = await twoDevices();
    const big = await Socket.connect(spaceId, a);
    big.ws.send("ping");
    expect(await big.text()).toBe("pong");
    big.ws.send("x".repeat(8 * 1024 * 1024 + 1));
    big.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    expect(await big.next()).toMatchObject({ case: "goodbye", value: { code: Close.PROTOCOL } });
    expect((await big.waitClosed()).code).toBe(Close.PROTOCOL);
    const small = await Socket.connect(spaceId, a);
    small.ws.send("hello");
    expect((await small.waitClosed()).code).toBe(Close.PROTOCOL);
  });

  it("reports each device's highest accepted device_seq in Welcome", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    expect((await sa.start()).deviceSeq).toBe(0n);
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    expect((await (await Socket.connect(spaceId, a)).hello()).deviceSeq).toBe(2n);
    expect((await (await Socket.connect(spaceId, b)).hello()).deviceSeq).toBe(0n);
  });

  it("reports the commit token of the device's commit at device_seq in Welcome, also once the log no longer holds it", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    expect(await sa.start()).toMatchObject({ deviceSeq: 0n, deviceCommitId: "", deviceCommittedAtMs: 0n });
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    const second = await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const token = await runInDurableObject(stub, (_instance, state) => {
      const row = state.storage.sql.exec("SELECT commit_id, committed_at_ms FROM commits WHERE seq = 2").one();
      return { deviceCommitId: String(row.commit_id), deviceCommittedAtMs: BigInt(Number(row.committed_at_ms)) };
    });
    expect(token.deviceCommittedAtMs).toBe(second.committedAtMs);
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 2n, ...token });
    expect(await (await Socket.connect(spaceId, b)).hello()).toMatchObject({ deviceSeq: 0n, deviceCommitId: "", deviceCommittedAtMs: 0n });
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("DELETE FROM commits");
    });
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 2n, ...token });
  });

  it("reports no commit token in Welcome for a stored result from before commit tokens whose commit the log no longer holds", async () => {
    const { a, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    await sa.start();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => {
      const row = state.storage.sql.exec("SELECT results FROM device_commits").one();
      const result = fromBinary(CommitResultSchema, new Uint8Array(row.results as ArrayBuffer));
      result.committedAtMs = 0n;
      state.storage.sql.exec("UPDATE device_commits SET results = ?", toBinary(CommitResultSchema, result));
      state.storage.sql.exec("DELETE FROM commits");
    });
    expect(await (await Socket.connect(spaceId, a)).hello()).toMatchObject({ deviceSeq: 1n, deviceCommitId: "", deviceCommittedAtMs: 0n });
  });

  it("lets two devices commit and receive each other's changes", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    const welcome = await sa.start();
    expect(welcome).toMatchObject({ proto: 1, formatVersion: 1, headSeq: 0n, readOnly: false });
    await sb.start();

    const docs = uuidv7();
    const first = await sa.commit(0n, [op.mkdir(docs, spaceId, "Docs")]);
    expect(first.seq).toBe(1n);
    const atB = await sb.pushed();
    expect(atB.map((r) => r.seq)).toEqual([1n]);
    expect(atB[0]!.deviceId).toBe(a.deviceId);
    expect(nodeIds(atB)).toEqual([docs]);

    const notes = uuidv7();
    const second = await sb.commit(1n, [op.mkdir(notes, docs, "Notes")]);
    expect(second.seq).toBe(2n);
    expect(nodeIds(await sa.pushed())).toEqual([notes]);
  });

  it("resumes from the last seq after a dropped socket and across hibernation", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.start();
    let sb = await Socket.connect(spaceId, b);
    await sb.start();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    sb.ws.close(1000, "network drop");
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    await sa.commit(2n, [op.mkdir(uuidv7(), spaceId, "three")]);

    sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const caughtUp = await sb.resume(welcome.historyId, 1n);
    expect(caughtUp.records.map((r) => r.seq)).toEqual([2n, 3n]);
    expect(caughtUp.head).toBe(3n);

    await evictDurableObject(stub);
    await sa.commit(3n, [op.mkdir(uuidv7(), spaceId, "after hibernation")]);
    expect((await sb.pushed()).map((r) => r.seq)).toEqual([4n]);
  });

  it("turns a conflicting edit into a conflict copy and pushes it", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const sb = await Socket.connect(spaceId, b);
    await sa.start();
    await sb.start();
    const file = uuidv7();
    const v1 = await prepared(sa, "v1");
    await sa.commit(0n, [op.create(file, spaceId, "plan.txt", v1.content)], v1.receipts);
    await sb.pushed();

    const fromB = await prepared(sb, "B's edit");
    await sb.commit(1n, [op.update(file, fromB.content)], fromB.receipts);
    await sa.pushed();
    const fromA = await prepared(sa, "A's edit");
    const result = await sa.commit(1n, [op.update(file, fromA.content)], fromA.receipts);
    expect(result.results[0]!.code).toBe(OpResult_Code.CONFLICT_COPY);
    expect(result.results[0]!.name).toMatch(/^plan \(conflict from Laptop A \d{4}-\d{2}-\d{2}\)\.txt$/);

    const pushed = await sb.pushed();
    const copy = pushed[0]!.changes[0]!.change;
    expect(copy.case === "node" && copy.value).toMatchObject({ nodeId: result.results[0]!.nodeId, name: result.results[0]!.name, parentId: spaceId });
  });

  it("asks for a resync on another history or a seq ahead of the server", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    const welcome = await socket.hello();
    socket.send({ case: "resume", value: { historyId: uuidv7(), sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.HISTORY_CHANGED } });
    socket.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq: 9n } });
    expect(await socket.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.AHEAD_OF_SERVER } });
  });

  it("resumes from a token that names the commit at since_seq or from no token, and names the head's commit in CaughtUp (RFC 0013)", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.hello();
    expect((await sa.resume(welcome.historyId, 0n)).token).toEqual({ commitId: "", committedAtMs: 0n });
    for (const [i, name] of ["one", "two", "three"].entries()) await sa.commit(BigInt(i), [op.mkdir(uuidv7(), spaceId, name)]);

    const sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const all = await sb.resume(welcome.historyId, 0n);
    const tokens = all.records.map(tokenOf);
    expect(all.token).toEqual(tokens[2]);
    const fromOne = await sb.resume(welcome.historyId, 1n, tokens[0]);
    expect(fromOne.records.map((r) => r.seq)).toEqual([2n, 3n]);
    expect(fromOne.token).toEqual(tokens[2]);
    expect(await sb.resume(welcome.historyId, 3n, tokens[2])).toMatchObject({ records: [], head: 3n, token: tokens[2] });
    expect((await sb.resume(welcome.historyId, 1n)).records.map((r) => r.seq)).toEqual([2n, 3n]);
  });

  it("asks for a resync with AHEAD_OF_SERVER when the token at since_seq names another commit, at the head too (RFC 0013)", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    const welcome = await sa.hello();
    await sa.commit(0n, [op.mkdir(uuidv7(), spaceId, "one")]);
    await sa.commit(1n, [op.mkdir(uuidv7(), spaceId, "two")]);
    const [one, two] = (await sa.resume(welcome.historyId, 0n)).records.map(tokenOf);

    const sb = await Socket.connect(spaceId, b);
    await sb.hello();
    const forks: [bigint, CommitToken][] = [
      [1n, { ...one!, commitId: uuidv7() }],
      [1n, { ...one!, committedAtMs: one!.committedAtMs + 1n }],
      [2n, { ...two!, commitId: one!.commitId }],
      [2n, { ...two!, committedAtMs: two!.committedAtMs - 1n }],
    ];
    for (const [sinceSeq, since] of forks) {
      sb.send({ case: "resume", value: { historyId: welcome.historyId, sinceSeq, sinceCommitId: since.commitId, sinceCommittedAtMs: since.committedAtMs } });
      expect(await sb.next()).toMatchObject({ case: "resyncRequired", value: { reason: ResyncRequired_Reason.AHEAD_OF_SERVER } });
    }
  });

  it("closes a connection whose token expired, and one that reauthenticates as another device", async () => {
    const { a, b, spaceId, stub } = await twoDevices();
    const sa = await Socket.connect(spaceId, a);
    await sa.start();
    await runInDurableObject(stub, (instance: import("../src/space-do").SpaceDO) => instance.closeExpired(Date.now() + 16 * 60_000 + 1000));
    expect((await sa.waitClosed()).code).toBe(Close.TOKEN_EXPIRED);

    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    sb.send({ case: "reauth", value: { accessToken: b.access!.token } });
    sb.send({ case: "reauth", value: { accessToken: a.access!.token } });
    const goodbye = await sb.next();
    expect(goodbye).toMatchObject({ case: "goodbye", value: { code: Close.TOKEN_EXPIRED } });
  });

  it("requires Hello first, answers text pings, and refuses unknown users", async () => {
    const { a, spaceId } = await twoDevices();
    const socket = await Socket.connect(spaceId, a);
    socket.ws.send("ping");
    expect(await socket.text()).toBe("pong");
    socket.send({ case: "resume", value: { historyId: "", sinceSeq: 0n } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.PROTOCOL } });

    expect(await Socket.open(spaceId, undefined)).toBe(401);
    const stranger = uuidv7();
    await env.SPACE.get(env.SPACE.idFromName(stranger)).init({ spaceId: stranger, name: "x", ownerUserId: uuidv7(), spaceKey: new Uint8Array(32) });
    expect(await Socket.open(stranger, a.access!.token)).toBe(403);
  });

  it("serves the same RPCs over HTTP and pushes HTTP commits", async () => {
    const { a, b, spaceId } = await twoDevices();
    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    const commit = body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "via http")], historyId: sb.historyId, deviceSeq: 1n } } });
    const response = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: commit }, RpcResponseSchema);
    expect(response.message!.result.case).toBe("commit");
    expect((await sb.pushed())[0]!.seq).toBe(1n);

    const list = body(RpcRequestSchema, { reqId: 2n, call: { case: "listTree", value: {} } });
    const tree = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: list }, RpcResponseSchema);
    const page = tree.message!.result;
    expect(page.case === "tree" && page.value.nodes.map((n) => n.name).sort()).toEqual(["", "via http"]);
  });
});

describe("share links", () => {
  it("checks the password, locks after repeated failures, and honours expiry and revocation", async () => {
    const links = account();
    const spaceId = uuidv7();
    const nodeId = uuidv7();
    const open = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7() });
    expect(await links.openShareLink(open, undefined)).toEqual({ ok: true, value: { spaceId, nodeId } });
    expect(open).toMatch(/^[A-Za-z0-9_-]{22}$/);

    const locked = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), password: "correct horse" });
    const started = Date.now();
    expect((await links.openShareLink(locked, "correct horse")).ok).toBe(true);
    console.log("PBKDF2-SHA256 100000 iterations, one share-link verification through the Account DO: " + (Date.now() - started) + " ms");
    for (let i = 0; i < 5; i++) expect((await links.openShareLink(locked, "wrong")).ok).toBe(false);
    expect(await links.openShareLink(locked, "correct horse")).toMatchObject({ ok: false, message: "too many wrong passwords" });

    const row = await runInDurableObject(links, (_instance, state) =>
      state.storage.sql.exec("SELECT password_kdf, password_salt, password_hash, password_iterations FROM share_links WHERE link_id = ?", locked).one(),
    );
    expect(row.password_kdf).toBe("pbkdf2-sha256");
    expect(row.password_iterations).toBe(100_000);
    expect((row.password_salt as ArrayBuffer).byteLength).toBe(16);
    expect((row.password_hash as ArrayBuffer).byteLength).toBe(32);

    const expired = await links.createShareLink({ spaceId, nodeId, createdBy: uuidv7(), expiresAtMs: Date.now() - 1 });
    expect((await links.openShareLink(expired, undefined)).ok).toBe(false);
    await links.revokeShareLink(open);
    expect((await links.openShareLink(open, undefined)).ok).toBe(false);
  });

  it("counts consecutive failures per link, including a missing password, and starts over after success or a lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const codes = async (attempts: (string | undefined)[]) => {
      const out = [];
      for (const attempt of attempts) {
        const result = await links.openShareLink(link, attempt);
        out.push(result.ok ? "ok" : Error_Code[result.code]);
      }
      return out;
    };
    expect(await codes(["x", undefined, "x", "x", "pw", "x", "x", "x", "x"])).toEqual([
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok",
      "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED",
    ]);
    expect(await codes(["x", "pw", "x"])).toEqual(["UNAUTHENTICATED", "RETRY_LATER", "RETRY_LATER"]);
    const state = await runInDurableObject(links, (_instance, s) =>
      s.storage.sql.exec("SELECT failed_attempts FROM share_links WHERE link_id = ?", link).one(),
    );
    expect(state.failed_attempts).toBe(0);
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET locked_until_ms = ? WHERE link_id = ?", Date.now() - 1, link);
    });
    expect(await codes(["x", "x", "x", "x", "pw"])).toEqual(["UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "UNAUTHENTICATED", "ok"]);
  });

  it("counts concurrent wrong passwords one by one, so they cannot get past the lock", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    const results = await Promise.all(Array.from({ length: 12 }, () => links.openShareLink(link, "x")));
    const codes = results.map((r) => (r.ok ? "ok" : Error_Code[r.code]));
    expect(codes.filter((c) => c === "UNAUTHENTICATED")).toHaveLength(5);
    expect(codes.filter((c) => c === "RETRY_LATER")).toHaveLength(7);
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.RETRY_LATER });
  });

  it("refuses to open a link whose password scheme the server does not know", async () => {
    const links = account();
    const link = await links.createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7(), password: "pw" });
    await runInDurableObject(links, (_instance, s) => {
      s.storage.sql.exec("UPDATE share_links SET password_kdf = 'argon2id' WHERE link_id = ?", link);
    });
    expect(await links.openShareLink(link, "pw")).toMatchObject({ ok: false, code: Error_Code.INTERNAL });
  });
});

describe("platform limits", () => {
  const DAY_MS = 86_400_000;
  const DAILY = "Exceeded allowed rows written in Durable Objects free tier.";
  const DAILY_MESSAGE = "the server reached a Workers Free daily limit; it resets at 00:00 UTC";
  const HOUR_MS = 3_600_000;
  const wait = (nowMs: number) => BigInt(nowMs % DAY_MS < HOUR_MS / 2 ? 300_000 : DAY_MS - (nowMs % DAY_MS));

  async function connected(sinceMidnightMs = HOUR_MS) {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    await runInDurableObject(stub, (instance: SpaceDO) => {
      const now = Date.now();
      instance.clock = () => now - (now % DAY_MS) - DAY_MS + sinceMidnightMs;
    });
    return { a, spaceId, stub, socket, retryAfterMs: wait(sinceMidnightMs) };
  }

  function failNextSpaceQuery(stub: DurableObjectStub<SpaceDO>, error: Error) {
    return runInDurableObject(stub, (_instance, state) => {
      vi.spyOn(state.storage.sql, "exec").mockImplementationOnce(() => {
        throw error;
      });
    });
  }

  it("answers an RPC that a daily limit stops with QUOTA_EXCEEDED and a wait until 00:00 UTC, over HTTP and WebSocket", async () => {
    const { a, spaceId, stub, socket, retryAfterMs } = await connected();
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new Error(DAILY);
      };
    });
    const request: MessageInitShape<typeof RpcRequestSchema>["call"] = { case: "listTree", value: {} };
    const http = await call("/v1/spaces/" + spaceId + "/rpc", { token: a.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: request }) }, RpcResponseSchema);
    const quota = { case: "error", value: { code: Error_Code.QUOTA_EXCEEDED, message: DAILY_MESSAGE, retryAfterMs } };
    expect(http.message!.result).toMatchObject(quota);
    expect(await socket.rpc(request)).toMatchObject(quota);
    expect(socket.closed).toBeUndefined();
    expect(logged.mock.calls.filter(([what]) => what === "space rpc failed").map(([, error]) => error)).toEqual(["Error: " + DAILY, "Error: " + DAILY]);
  });

  it.each([
    [10 * 60_000, 300_000n],
    [HOUR_MS / 2 - 1, 300_000n],
    [HOUR_MS / 2, BigInt(DAY_MS - HOUR_MS / 2)],
  ])("waits 5 minutes instead of a day in the first 30 minutes after 00:00 UTC: %i ms after it, %i ms", async (sinceMidnightMs, retryAfterMs) => {
    const { stub, socket } = await connected(sinceMidnightMs);
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(stub, (instance: SpaceDO) => {
      (instance as unknown as { listTree: () => never }).listTree = () => {
        throw new Error(DAILY);
      };
    });
    expect(await socket.rpc({ case: "listTree", value: {} })).toMatchObject({ case: "error", value: { code: Error_Code.QUOTA_EXCEEDED, retryAfterMs } });
  });

  it("answers a transient platform failure of an RPC with RETRY_LATER and any other error with INTERNAL, each with its cause", async () => {
    const { spaceId, stub, socket } = await connected();
    vi.spyOn(console, "error").mockImplementation(() => {});
    const retry = { code: Error_Code.RETRY_LATER, message: "the server is briefly unavailable; retry" };
    const internal = { code: Error_Code.INTERNAL, message: "internal error" };
    const others: [Error, Error_Cause, { code: Error_Code; message: string }][] = [
      [new Error("disk I/O error: SQLITE_IOERR"), Error_Cause.EXCEPTION, internal],
      [Object.assign(new Error("Durable Object reset because its code was updated."), { retryable: true }), Error_Cause.DO_UNAVAILABLE, retry],
      [Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true }), Error_Cause.DO_OVERLOADED, retry],
      [new Error("get: We encountered an internal error. Please try again. (10001)"), Error_Cause.R2_UNAVAILABLE, retry],
      [new Error("put: The specified bucket does not exist. (10006)"), Error_Cause.R2_ERROR, internal],
      [new Error("Too many subrequests."), Error_Cause.SUBREQUEST_LIMIT, internal],
      [new TypeError("Cannot read properties of undefined (reading 'nodeId')"), Error_Cause.EXCEPTION, internal],
      [new Error("the request failed (10001)"), Error_Cause.EXCEPTION, internal],
    ];
    for (const [error, cause, answer] of others) {
      await runInDurableObject(stub, (instance: SpaceDO) => {
        (instance as unknown as { listTree: () => never }).listTree = () => {
          throw error;
        };
      });
      expect(await socket.rpc({ case: "listTree", value: {} }), error.message).toMatchObject({
        case: "error",
        value: { ...answer, retryAfterMs: 0n, cause },
      });
    }
    expect(spaceId).toBeTruthy();
  });

  it("sends a Notice and closes the session when a daily limit stops a message before its RPC is read", async () => {
    const { stub, socket, retryAfterMs } = await connected();
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    await failNextSpaceQuery(stub, new Error(DAILY));
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    expect(await socket.next()).toMatchObject({ case: "notice", value: { kind: Notice_Kind.QUOTA_EXCEEDED, message: DAILY_MESSAGE, retryAfterMs } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER, reason: DAILY_MESSAGE } });
    expect((await socket.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
    expect(logged.mock.calls.find(([what]) => what === "space session stopped at a platform limit")?.[1]).toBe("Error: " + DAILY);
  });

  it("sends no Notice for any other error that stops a message before its RPC is read", async () => {
    const { stub, socket } = await connected();
    await failNextSpaceQuery(stub, new Error("disk I/O error: SQLITE_IOERR"));
    socket.send({ case: "rpc", value: { reqId: 99n, call: { case: "listTree", value: {} } } });
    expect(await socket.rpc({ case: "listTree", value: {} })).toMatchObject({ case: "tree" });
    expect(socket.closed).toBeUndefined();
  });

  it.each([
    "Exceeded allowed rows written in Durable Objects free tier.",
    "Exceeded allowed rows read in Durable Objects free tier.",
    "Exceeded allowed duration in Durable Objects free tier.",
    "Exceeded allowed volume of requests in Durable Objects free tier.",
    "database or disk is full: SQLITE_FULL",
  ])("answers a request the Account DO fails with %j with 429 QUOTA_EXCEEDED and a wait until 00:00 UTC", async (text) => {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    const exec = await runInDurableObject(account(), (_instance, state) =>
      vi.spyOn(state.storage.sql, "exec").mockImplementation(() => {
        throw new Error(text);
      }),
    );
    const refusal = async (path: string, init: RequestInit) => {
      const before = Date.now();
      const response = await exports.default.fetch(BASE + path, init);
      const after = Date.now();
      const error = fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()));
      const bounds = [wait(before), wait(after)].sort((x, y) => (x < y ? -1 : 1));
      expect(error.retryAfterMs).toBeGreaterThanOrEqual(bounds[0]!);
      expect(error.retryAfterMs).toBeLessThanOrEqual(bounds[1]!);
      return { status: response.status, code: error.code, message: error.message };
    };
    try {
      const upgrade = await refusal("/v1/spaces/" + spaceId + "/ws", { headers: { Upgrade: "websocket", Authorization: "Bearer " + a.access!.token } });
      const token = await refusal("/v1/token", {
        method: "POST",
        headers: { "Content-Type": "application/x-protobuf" },
        body: body(TokenRequestSchema, { deviceId: a.deviceId, refreshCredential: a.refreshCredential }),
      });
      const message = text.includes("SQLITE_FULL") ? "the server's storage is full" : DAILY_MESSAGE;
      expect([upgrade, token]).toEqual([0, 1].map(() => ({ status: 429, code: Error_Code.QUOTA_EXCEEDED, message })));
    } finally {
      exec.mockRestore();
    }
    expect(logged.mock.calls.filter(([what]) => what === "request stopped at a platform limit").map(([, error]) => error)).toEqual(["Error: " + text, "Error: " + text]);
  });

  it("answers a transient failure of the Account DO with 503 RETRY_LATER and any other error with 500 INTERNAL, each with its cause", async () => {
    const a = await getOwner();
    const failures: [Error, number, Error_Code, Error_Cause][] = [
      [new Error("disk I/O error: SQLITE_IOERR"), 500, Error_Code.INTERNAL, Error_Cause.EXCEPTION],
      [Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true }), 503, Error_Code.RETRY_LATER, Error_Cause.DO_OVERLOADED],
    ];
    for (const [thrown, status, code, cause] of failures) {
      const exec = await runInDurableObject(account(), (_instance, state) =>
        vi.spyOn(state.storage.sql, "exec").mockImplementation(() => {
          throw thrown;
        }),
      );
      try {
        const request = exports.default.fetch(BASE + "/v1/token", {
          method: "POST",
          headers: { "Content-Type": "application/x-protobuf" },
          body: body(TokenRequestSchema, { deviceId: a.deviceId, refreshCredential: a.refreshCredential }),
        });
        vi.spyOn(console, "error").mockImplementation(() => {});
        const response = await request;
        expect([response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()))], thrown.message).toMatchObject([status, { code, cause }]);
      } finally {
        exec.mockRestore();
      }
    }
  });
});
