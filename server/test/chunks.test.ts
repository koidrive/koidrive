// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ChunkBundleResultSchema, ChunkBundleSchema, ChunkPlan_Action, Error_Cause, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { type ServerKey } from "../src/auth";
import { sha256 } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import { HOLD_MS, type SpaceDO } from "../src/space-do";
import { chunkKey } from "../src/space/store";
import worker from "../src/worker";
import { countBundledChunks, decodeChunkBundle } from "../src/chunk-bundle";
import { BadRequest } from "../src/http";
import { content, op, storedObjects, targetIncarnation } from "./helpers";
import { BASE, account, body, getOwner, pairDevice, createSpace, Socket, put, prepared } from "./session";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("chunk relay", () => {
  async function connected() {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const socket = await Socket.connect(spaceId, a);
    await socket.start();
    return { a, spaceId, socket };
  }

  it("uploads through the Worker, commits with the receipt, and downloads the same bytes", async () => {
    const { a, spaceId, socket } = await connected();
    const b = await pairDevice(a, "Reader");
    const reader = await Socket.connect(spaceId, b);
    await reader.start();

    const file = await prepared(socket, "the bytes of a small file");
    expect(file.plan.chunks[0]!.target!.url.startsWith(BASE + "/v1/spaces/" + spaceId + "/chunks/")).toBe(true);
    expect(file.receipts).toHaveLength(1);
    const head = await env.BUCKET.head(chunkKey(spaceId, toHex(file.content.chunks[0]!.hash), targetIncarnation(file.plan.chunks[0]!.target!.url)));
    expect(head?.size).toBe(file.bytes[0]!.length);
    const nodeId = uuidv7();
    await socket.commit(0n, [op.create(nodeId, spaceId, "small.txt", file.content)], file.receipts);

    const pushed = await reader.pushed();
    const state = pushed[0]!.changes[0]!.change;
    const hash = state.case === "node" ? state.value.current!.content!.chunks[0]!.hash : new Uint8Array();
    const targets = await reader.rpc({ case: "getDownloadTargets", value: { hashes: [hash] } });
    if (targets.case !== "downloadTargets") throw new Error("getDownloadTargets failed");
    const response = await exports.default.fetch(targets.value.targets[0]!.target!.url);
    expect(response.status).toBe(200);
    const downloaded = new Uint8Array(await response.arrayBuffer());
    expect(toHex(await sha256(downloaded))).toBe(toHex(hash));

    const again = await socket.rpc({ case: "prepareUpload", value: { chunks: file.content.chunks } });
    expect(again.case === "prepareUpload" && again.value.chunks[0]!.action).toBe(ChunkPlan_Action.PRESENT);
  });

  it("rejects uploads with a bad ticket, a wrong length or bytes that do not match the hash", async () => {
    const { spaceId, socket } = await connected();
    const parts = await content("expected bytes");
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const target = plan.value.chunks[0]!.target!;
    const bytes = parts.bytes[0]!;
    const key = chunkKey(spaceId, toHex(parts.content.chunks[0]!.hash), targetIncarnation(target.url));

    const mac = new URL(target.url).searchParams.get("ticket")!.split(".")[2]!;
    const middle = target.url.lastIndexOf(mac) + Math.floor(mac.length / 2);
    const swapped = target.url[middle] === "A" ? "B" : "A";
    const tampered = { ...target, url: target.url.slice(0, middle) + swapped + target.url.slice(middle + 1) };
    expect(tampered.url).not.toBe(target.url);
    expect((await put(tampered, bytes)).status).toBe(403);
    expect((await put({ ...target, url: target.url.split("?")[0]! }, bytes)).status).toBe(403);
    const otherSpace = { ...target, url: target.url.replace(spaceId, uuidv7()) };
    expect((await put(otherSpace, bytes)).status).toBe(403);
    expect((await put(target, new Uint8Array(bytes.length + 1))).status).toBe(403);
    expect((await put(target, new Uint8Array(bytes.length))).status).toBe(400);
    expect(await env.BUCKET.head(key)).toBeNull();

    const download = await exports.default.fetch(target.url);
    expect(download.status).toBe(403);
    expect((await put(target, bytes)).status).toBe(200);
    expect((await put(target, bytes)).status).toBe(200);
  });

  async function preparedTicket() {
    const { spaceId, socket } = await connected();
    const parts = await content("review chunk " + uuidv7());
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: parts.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const bytes = parts.bytes[0]!;
    const target = plan.value.chunks[0]!.target!;
    const url = target.url;
    const request = () => new Request(url, { method: "PUT", body: bytes, headers: { "Content-Length": String(bytes.length) } });
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    return { spaceId, socket, parts, target, request, stub, key: chunkKey(spaceId, toHex(parts.content.chunks[0]!.hash), targetIncarnation(url)) };
  }

  function relay(request: Request, put: (key: string, body: ReadableStream | null, options: R2PutOptions) => Promise<R2Object | null>) {
    const bucket = { put };
    return worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET: bucket } as unknown as Env, { waitUntil() {} } as unknown as ExecutionContext);
  }

  it("removes an object that a late PUT recreated after GC deleted its row", async () => {
    const { request, stub, key } = await preparedTicket();
    let entered!: () => void;
    let resume!: () => void;
    const started = new Promise<void>((resolve) => (entered = resolve));
    const released = new Promise<void>((resolve) => (resume = resolve));
    const pending = relay(request(), async (k, body, options) => {
      entered();
      await released;
      return env.BUCKET.put(k, body, options);
    });
    await started;
    const at = Date.now() + HOLD_MS + 60_000;
    const first = await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at));
    expect(first).toMatchObject({ deletedChunks: 1, sweptChunks: 0 });
    resume();
    expect((await pending).status).toBe(200);
    expect(await env.BUCKET.head(key)).not.toBeNull();
    const rows = () => runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT * FROM chunks").toArray());
    expect(await rows()).toHaveLength(0);

    const day = 24 * 3_600_000;
    expect(await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at + day))).toMatchObject({ sweptChunks: 0 });
    const sweep = await runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(at + 31 * day));
    expect(sweep).toMatchObject({ sweptChunks: 1, deletedChunks: 1 });
    expect(await env.BUCKET.head(key)).toBeNull();
    expect(await rows()).toHaveLength(0);
  });

  it("sweeps only objects whose incarnation no chunk row holds, once the grace has passed, and retires each before deleting it", async () => {
    const { socket, parts, target, stub, spaceId, key } = await preparedTicket();
    const uploaded = await put(target, parts.bytes[0]!);
    expect(uploaded.status).toBe(200);
    const receipt = fromBinary(UploadReceiptSchema, new Uint8Array(await uploaded.arrayBuffer()));
    await socket.commit(0n, [op.create(uuidv7(), spaceId, "kept", parts.content)], [receipt]);
    const hex = toHex(parts.content.chunks[0]!.hash);
    const otherIncarnation = chunkKey(spaceId, hex, "0.999999");
    await env.BUCKET.put(otherIncarnation, parts.bytes[0]!);
    const orphan = await content("orphan " + uuidv7());
    const orphanHex = toHex(orphan.content.chunks[0]!.hash);
    const orphanKey = chunkKey(spaceId, orphanHex, "0.1");
    await env.BUCKET.put(orphanKey, orphan.bytes[0]!);

    const gc = (atMs: number) => runInDurableObject(stub, (instance: SpaceDO) => instance.collectGarbage(atMs));
    expect(await gc(Date.now())).toMatchObject({ sweptChunks: 0, deletedChunks: 0 });
    expect(await gc(Date.now() + 31 * 24 * 3_600_000)).toMatchObject({ sweptChunks: 2, deletedChunks: 2 });
    expect(await storedObjects(spaceId, hex)).toEqual([key]);
    expect(await storedObjects(spaceId, orphanHex)).toEqual([]);
    const retired = await runInDurableObject(account(), (_instance, state) =>
      state.storage.sql.exec("SELECT hash_hex, incarnation FROM chunk_retirements WHERE space_id = ? ORDER BY incarnation DESC", spaceId).toArray(),
    );
    expect(retired).toEqual([
      { hash_hex: hex, incarnation: "0.999999" },
      { hash_hex: orphanHex, incarnation: "0.1" },
    ]);
  });

  it("answers a checksum mismatch with 400 and storage failures with a retryable error", async () => {
    const { request, key } = await preparedTicket();
    const fail = (message: string) => relay(request(), async () => {
      throw new Error(message);
    });
    const decode = async (response: Response) => fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()));

    const mismatch = await fail("put: The SHA-256 checksum you specified did not match what we received.");
    expect(mismatch.status).toBe(400);
    expect((await decode(mismatch)).code).toBe(Error_Code.INVALID_ARGUMENT);
    const throttled = await fail("put: Reduce your concurrent request rate for the same object. (10058)");
    expect(throttled.status).toBe(429);
    expect(await decode(throttled)).toMatchObject({ code: Error_Code.RETRY_LATER, cause: Error_Cause.R2_RATE_LIMITED });
    const outage = await fail("put: We encountered an internal error. Please try again. (10001)");
    expect(outage.status).toBe(503);
    expect(await decode(outage)).toMatchObject({ code: Error_Code.RETRY_LATER, cause: Error_Cause.R2_UNAVAILABLE });
    expect(await env.BUCKET.head(key)).toBeNull();
  });

  function accountBinding(serverKey: () => Promise<ServerKey>) {
    return { idFromName: (name: string) => env.ACCOUNT.idFromName(name), get: () => ({ metered: async () => ({ value: await serverKey() }) }) };
  }

  it("reads the server key again on the next request after a failed Account DO call", async () => {
    const { request, key } = await preparedTicket();
    let calls = 0;
    const ACCOUNT = accountBinding(async () => {
      calls++;
      if (calls === 1) throw new Error("injected Account DO failure");
      return account().serverKey();
    });
    const send = () => worker.fetch(request() as Parameters<typeof worker.fetch>[0], { ...env, ACCOUNT } as unknown as Env, { waitUntil() {} } as unknown as ExecutionContext);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const failed = await send();
    expect(failed.status).toBe(500);
    expect(fromBinary(ErrorSchema, new Uint8Array(await failed.arrayBuffer()))).toMatchObject({ code: Error_Code.INTERNAL, cause: Error_Cause.EXCEPTION });
    expect((await send()).status).toBe(200);
    expect(calls).toBe(2);
    expect(await env.BUCKET.head(key)).not.toBeNull();
  });

  it("keeps no per-space state for unknown space ids and touches no storage before the ticket check", async () => {
    let calls = 0;
    const ACCOUNT = accountBinding(() => {
      calls++;
      return account().serverKey();
    });
    const BUCKET = new Proxy(
      {},
      {
        get: () => {
          throw new Error("R2 was used before the ticket check");
        },
      },
    );
    const relayEnv = { ...env, ACCOUNT, BUCKET } as unknown as Env;
    const send = (url: string, init?: RequestInit) => worker.fetch(new Request(url, init) as Parameters<typeof worker.fetch>[0], relayEnv, { waitUntil() {} } as unknown as ExecutionContext);
    for (let i = 0; i < 20; i++) {
      const url = `${BASE}/v1/spaces/${uuidv7()}/chunks/${"ab".repeat(32)}?inc=0.1&ticket=v1.9999999999999.AAAA`;
      expect((await send(url, { method: "PUT", body: new Uint8Array(4), headers: { "Content-Length": "4" } })).status).toBe(403);
      expect((await send(url)).status).toBe(403);
    }
    expect(calls).toBe(1);
  });
});

describe("chunk bundles", () => {
  async function planned(...parts: string[]) {
    const a = await getOwner();
    const spaceId = await createSpace(a);
    const socket = await Socket.connect(spaceId, a);
    const welcome = await socket.start();
    const file = await content(...parts);
    const plan = await socket.rpc({ case: "prepareUpload", value: { chunks: file.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const targets = plan.value.chunks.map((chunk) => chunk.target!);
    const keys = file.content.chunks.map((chunk, i) => chunkKey(spaceId, toHex(chunk.hash), targetIncarnation(targets[i]!.url)));
    return { spaceId, socket, welcome, file, targets, keys };
  }

  const bundleBody = (chunks: { target: string; data: Uint8Array }[]) => body(ChunkBundleSchema, { chunks });
  const bundlePath = (spaceId: string) => `${BASE}/v1/spaces/${spaceId}/chunks`;

  async function sendBundle(spaceId: string, chunks: { target: string; data: Uint8Array }[], fetcher: (request: Request) => Promise<Response> = (request) => exports.default.fetch(request)) {
    const sent = bundleBody(chunks);
    const response = await fetcher(new Request(bundlePath(spaceId), { method: "POST", body: sent, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(sent.length) } }));
    const bytes = new Uint8Array(await response.arrayBuffer());
    return { status: response.status, results: response.ok ? fromBinary(ChunkBundleResultSchema, bytes).results : [], code: response.ok ? undefined : fromBinary(ErrorSchema, bytes).code };
  }

  it("is offered in Welcome, stores each chunk as its own upload, and a commit accepts the receipts", async () => {
    const { spaceId, socket, welcome, file, targets, keys } = await planned("first small file", "second small file", "third small file");
    expect(welcome.features).toContain("chunk_bundles");
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })));
    expect(sent.status).toBe(200);
    expect(sent.results.map((result) => [toHex(result.hash), result.status])).toEqual(file.content.chunks.map((chunk) => [toHex(chunk.hash), 200]));
    for (const [i, key] of keys.entries()) expect((await env.BUCKET.head(key))?.size).toBe(file.bytes[i]!.length);
    await socket.commit(0n, [op.create(uuidv7(), spaceId, "bundled.txt", file.content)], sent.results.map((result) => result.receipt!));

    const again = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })));
    expect(again.results.map((result) => result.status)).toEqual([200, 200, 200]);
    expect((await put(targets[0]!, file.bytes[0]!)).status).toBe(200);
  });

  it("answers each chunk with the status a PUT of it alone gets, and stores only the chunks that pass", async () => {
    const { spaceId, file, targets, keys } = await planned("bad ticket", "bad bytes", "longer than its ticket", "fine chunk");
    const mac = new URL(targets[0]!.url).searchParams.get("ticket")!.split(".")[2]!;
    const middle = targets[0]!.url.lastIndexOf(mac) + Math.floor(mac.length / 2);
    const tampered = targets[0]!.url.slice(0, middle) + (targets[0]!.url[middle] === "A" ? "B" : "A") + targets[0]!.url.slice(middle + 1);
    const chunks = [
      { target: tampered, data: file.bytes[0]! },
      { target: targets[1]!.url, data: new Uint8Array(file.bytes[1]!.length) },
      { target: targets[2]!.url, data: new Uint8Array(file.bytes[2]!.length + 1) },
      { target: targets[3]!.url, data: file.bytes[3]! },
    ];
    const sent = await sendBundle(spaceId, chunks);
    expect(sent.status).toBe(200);
    expect(sent.results.map((result) => result.status)).toEqual([403, 400, 403, 200]);
    expect(sent.results.map((result) => result.error?.code ?? Error_Code.UNSPECIFIED)).toEqual([Error_Code.FORBIDDEN, Error_Code.INVALID_ARGUMENT, Error_Code.FORBIDDEN, Error_Code.UNSPECIFIED]);
    expect(sent.results[3]!.receipt?.size).toBe(file.bytes[3]!.length);
    for (const key of keys.slice(0, 3)) expect(await env.BUCKET.head(key)).toBeNull();
    expect(await env.BUCKET.head(keys[3]!)).not.toBeNull();

    const alone = [
      await put({ ...targets[0]!, url: tampered }, chunks[0]!.data),
      await put(targets[1]!, chunks[1]!.data),
      await put(targets[2]!, chunks[2]!.data),
    ];
    expect(alone.map((response) => response.status)).toEqual([403, 400, 403]);
  });

  it("answers a chunk whose storage write fails with the PUT's retry status and still stores the others", async () => {
    const { spaceId, file, targets, keys } = await planned("throttled chunk", "unavailable chunk", "stored chunk");
    const failing = (request: Request) =>
      worker.fetch(request as Parameters<typeof worker.fetch>[0], {
        ...env,
        BUCKET: {
          put: (key: string, value: unknown, options: R2PutOptions) => {
            if (key === keys[0]) throw new Error("put: 429 too many requests (10058)");
            if (key === keys[1]) throw new Error("put: internal error");
            return env.BUCKET.put(key, value as ArrayBuffer, options);
          },
        },
      } as unknown as Env, {} as ExecutionContext);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })), failing);
    expect(sent.results.map((result) => [result.status, result.error?.code, result.error?.cause])).toEqual([
      [429, Error_Code.RETRY_LATER, Error_Cause.R2_RATE_LIMITED],
      [503, Error_Code.RETRY_LATER, Error_Cause.EXCEPTION],
      [200, undefined, undefined],
    ]);
    expect(await env.BUCKET.head(keys[2]!)).not.toBeNull();
  });

  it("refuses a malformed or oversized bundle whole and writes nothing", async () => {
    const { spaceId, file, targets } = await planned("one", "two");
    const BUCKET = new Proxy({}, { get: () => { throw new Error("R2 was used for a refused bundle"); } });
    const guarded = (request: Request) => worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET } as unknown as Env, {} as ExecutionContext);
    const valid = { target: targets[0]!.url, data: file.bytes[0]! };
    const cases: [string, { target: string; data: Uint8Array }[], number, Error_Code][] = [
      ["empty", [], 413, Error_Code.LIMIT_EXCEEDED],
      ["33 chunks", Array.from({ length: 33 }, () => valid), 413, Error_Code.LIMIT_EXCEEDED],
      ["a chunk twice", [valid, valid], 400, Error_Code.INVALID_ARGUMENT],
      ["another space", [valid, { target: targets[1]!.url.replace(spaceId, uuidv7()), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["not a chunk target", [valid, { target: bundlePath(spaceId), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["not a URL", [valid, { target: "chunk", data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["no incarnation", [valid, { target: targets[1]!.url.replace(/inc=[^&]*/, "inc=x"), data: file.bytes[1]! }], 400, Error_Code.INVALID_ARGUMENT],
      ["an empty chunk", [valid, { target: targets[1]!.url, data: new Uint8Array(0) }], 413, Error_Code.LIMIT_EXCEEDED],
      ["a chunk over 1 MiB", [valid, { target: targets[1]!.url, data: new Uint8Array((1 << 20) + 1) }], 413, Error_Code.LIMIT_EXCEEDED],
    ];
    for (const [name, chunks, status, code] of cases) {
      const sent = await sendBundle(spaceId, chunks, guarded);
      expect([name, sent.status, sent.code]).toEqual([name, status, code]);
    }
    const raw = (init: RequestInit) => guarded(new Request(bundlePath(spaceId), { method: "POST", ...init }));
    expect((await raw({ body: new Uint8Array([0xff, 0xff, 0xff]), headers: { "Content-Length": "3" } })).status).toBe(400);
    expect((await raw({ body: new Uint8Array((4 << 20) + 1), headers: { "Content-Length": String((4 << 20) + 1) } })).status).toBe(413);
    expect((await raw({ body: new Uint8Array((4 << 20) + 1), headers: { "Content-Length": "10" } })).status).toBe(413);
    expect((await raw({ body: new ReadableStream({ start: (c) => { c.enqueue(new Uint8Array(4)); c.close(); } }), duplex: "half" } as RequestInit)).status).toBe(411);
    expect((await guarded(new Request(bundlePath(spaceId)))).status).toBe(405);
  });

  it("counts the chunks of a bundle before decoding it, so a body of millions of empty entries is refused cheaply", async () => {
    const { spaceId } = await planned("hostile");
    const BUCKET = new Proxy({}, { get: () => { throw new Error("R2 was used for a refused bundle"); } });
    const guarded = (request: Request) => worker.fetch(request as Parameters<typeof worker.fetch>[0], { ...env, BUCKET } as unknown as Env, {} as ExecutionContext);
    const post = async (bytes: Uint8Array) => {
      const response = await guarded(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Length": String(bytes.length) } }));
      return [response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code];
    };
    for (const n of [33, 1000, 100000, 2000000]) {
      const bytes = new Uint8Array(n * 2);
      for (let i = 0; i < n; i++) bytes[2 * i] = 0x0a;
      expect([n, ...(await post(bytes))]).toEqual([n, 413, Error_Code.LIMIT_EXCEEDED]);
    }
    const undecodable = new Uint8Array(33 * 3 + 3);
    for (let i = 0; i < 33; i++) undecodable.set([0x0a, 0x01, 0xff], 3 * i);
    undecodable.set([0xff, 0xff, 0xff], 99);
    expect(await post(undecodable)).toEqual([413, Error_Code.LIMIT_EXCEEDED]);
    expect(await post(new Uint8Array([0x0a, 0x05, 0x00]))).toEqual([400, Error_Code.INVALID_ARGUMENT]);
    expect(await post(new Uint8Array([0x0b]))).toEqual([400, Error_Code.INVALID_ARGUMENT]);
  });

  it("refuses a chunks field with the wrong wire type before decoding, so it can't hide entries from the count", async () => {
    const { spaceId } = await planned("wrong wire type");
    const blocks = 200000;
    const bytes = new Uint8Array(8 + blocks * 20 + 3);
    bytes.set([9, 6, 24, 0, 24, 0, 24, 0], 0);
    for (let i = 0; i < blocks; i++) bytes.set([10, 18, 18, 16], 8 + i * 20);
    bytes.set([16, 16, 0], 8 + blocks * 20);
    expect(bytes.length).toBeLessThanOrEqual(4 << 20);
    expect(() => countBundledChunks(bytes)).toThrow(BadRequest);
    expect(() => countBundledChunks(new Uint8Array([0x0d, 0, 0, 0, 0]))).toThrow(BadRequest);
    expect(countBundledChunks(new Uint8Array([0x10, 0x01, 0x0a, 0x00]))).toBe(1);
    const response = await exports.default.fetch(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(bytes.length) } }));
    expect([response.status, fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code]).toEqual([400, Error_Code.INVALID_ARGUMENT]);
  });

  it("keeps no unknown fields of a bundle, at the top level or inside a chunk, when it decodes one", async () => {
    const { spaceId } = await planned("unknown fields");
    const varint = (n: number) => {
      const out: number[] = [];
      for (; n >= 0x80; n = Math.floor(n / 0x80)) out.push((n & 0x7f) | 0x80);
      return [...out, n];
    };
    const n = 2097150;
    const topLevel = new Uint8Array(2 + n * 2);
    topLevel[0] = 0x0a;
    for (let i = 0; i < n; i++) topLevel[2 + 2 * i] = 0x12;
    const m = n - 3;
    const inner = new Uint8Array(m * 2 + 3);
    for (let i = 0; i < m; i++) inner[2 * i] = 0x1a;
    inner.set([0x12, 0x01, 0x41], m * 2);
    const inChunk = new Uint8Array([0x0a, ...varint(inner.length), ...inner]);
    for (const bytes of [topLevel, inChunk]) {
      expect(bytes.length).toBeLessThanOrEqual(4 << 20);
      const decoded = decodeChunkBundle(bytes);
      expect(decoded.chunks).toHaveLength(1);
      expect([decoded.$unknown, decoded.chunks[0]!.$unknown]).toEqual([undefined, undefined]);
      const response = await exports.default.fetch(new Request(bundlePath(spaceId), { method: "POST", body: bytes, headers: { "Content-Type": "application/x-protobuf", "Content-Length": String(bytes.length) } }));
      expect(response.status).not.toBe(200);
      expect(fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer())).code).not.toBe(Error_Code.INTERNAL);
    }
  });

  it("exports from the Worker module only entrypoints the runtime accepts", async () => {
    const exported = Object.entries(await import("../src/worker"));
    const entrypoint = (value: unknown) => typeof value === "function" || (typeof value === "object" && value !== null && "fetch" in value);
    expect(exported.filter(([, value]) => !entrypoint(value)).map(([name]) => name)).toEqual([]);
  });

  it("makes one storage write per chunk, at most six at once, for a full bundle", async () => {
    const parts = Array.from({ length: 32 }, (_, i) => `bundled chunk ${i} ${uuidv7()}`);
    const { spaceId, file, targets } = await planned(...parts);
    let open = 0;
    let peak = 0;
    let writes = 0;
    const counting = (request: Request) =>
      worker.fetch(request as Parameters<typeof worker.fetch>[0], {
        ...env,
        BUCKET: {
          put: async (key: string, value: unknown, options: R2PutOptions) => {
            writes++;
            peak = Math.max(peak, ++open);
            try {
              await new Promise((resolve) => setTimeout(resolve, 5));
              return await env.BUCKET.put(key, value as ArrayBuffer, options);
            } finally {
              open--;
            }
          },
        },
      } as unknown as Env, {} as ExecutionContext);
    const sent = await sendBundle(spaceId, targets.map((target, i) => ({ target: target.url, data: file.bytes[i]! })), counting);
    expect(sent.results.map((result) => result.status)).toEqual(Array(32).fill(200));
    expect(writes).toBe(32);
    expect(peak).toBe(6);
  });
});
