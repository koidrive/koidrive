// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { type Identity, type ServerKey, bearerToken, verifyAccessToken } from "./auth";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  DeviceCredentialsSchema,
  DeviceListSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  RewindRequestSchema,
  RewindResultSchema,
  ServerInfo_DataPath,
  ServerInfoSchema,
  SpaceInfoSchema,
  SpaceListSchema,
  SpaceUsageSchema,
  TokenRequestSchema,
} from "./gen/koidrive/v1/auth_pb";
import { UploadReceiptSchema } from "./gen/koidrive/v1/commit_pb";
import {
  type BundledChunkResult,
  BundledChunkResultSchema,
  ChunkBundleResultSchema,
  Error_Code,
  ErrorSchema,
} from "./gen/koidrive/v1/rpc_pb";
import { countBundledChunks, decodeChunkBundle, MAX_BUNDLE_CHUNKS } from "./chunk-bundle";
import { errorText } from "./errors";
import type { ObjectStore } from "./host";
import { BadRequest, errorReply, readMessage, reply } from "./http";
import { type ChunkObject, deriveSpaceKey, signChunkGrant, verifyChunkGrant } from "./crypto";
import { fromHex, isUuidV7 } from "./ids";
import { type Capability, policy } from "./policy";
import { quotaRefusal } from "./quota";
import { shareRoute } from "./share/routes";
import { CONNECT_HEADER, type Connect, GRANT_MS, MAX_CHUNK_BYTES, MAX_CLIENT_FRAME } from "./space/space";
import { chunkKey, INCARNATION } from "./space/store";
import { STORAGE } from "./storage";
import { PROTO_MAX, PROTO_MIN, SERVER_COMMIT, SERVER_VERSION } from "./version";

export { AccountDO } from "./account-do";
export { SpaceDO } from "./space-do";

const serverKeys = new WeakMap<object, Promise<ServerKey>>();

function account(env: Env) {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

function key(env: Env): Promise<ServerKey> {
  let cached = serverKeys.get(env.ACCOUNT);
  if (!cached) {
    cached = account(env)
      .serverKey()
      .catch((error: unknown) => {
        serverKeys.delete(env.ACCOUNT);
        throw error;
      });
    serverKeys.set(env.ACCOUNT, cached);
  }
  return cached;
}

async function authenticate(request: Request, env: Env, spaceId?: string): Promise<Identity | Response> {
  const token = bearerToken(request);
  const identity = token ? await verifyAccessToken(await key(env), token, Date.now()) : undefined;
  if (!identity) return errorReply(request, Error_Code.UNAUTHENTICATED, "missing or invalid access token");
  const check = await account(env).checkSession(identity.deviceId, spaceId);
  if (!check.active) return errorReply(request, Error_Code.UNAUTHENTICATED, "this device was revoked");
  if (check.closed) return errorReply(request, Error_Code.RETRY_LATER, "the space is being rewound; retry later", 503);
  return { ...identity, kind: check.kind, scope: check.scope };
}

function refuseWithout(request: Request, identity: Identity, capability: Capability): Response | undefined {
  if (policy.can(identity, capability)) return undefined;
  return errorReply(request, Error_Code.FORBIDDEN, `this device may not use ${capability}`);
}

function listCursor(url: URL): number {
  const cursor = url.searchParams.get("cursor") ?? "";
  if (!/^\d{0,15}$/.test(cursor)) throw new BadRequest("malformed cursor");
  return Number(cursor);
}

async function devices(request: Request, env: Env, deviceId: string | undefined): Promise<Response> {
  const identity = await authenticate(request, env);
  if (identity instanceof Response) return identity;
  const refused = refuseWithout(request, identity, "device.manage");
  if (refused) return refused;
  if (deviceId === undefined && request.method === "GET") {
    const after = listCursor(new URL(request.url));
    return reply(request, DeviceListSchema, await account(env).listDevices({ userId: identity.userId, callerId: identity.deviceId, cursor: after ? String(after) : "" }));
  }
  if (deviceId !== undefined && request.method === "DELETE") {
    const confirmLastDevice = new URL(request.url).searchParams.get("confirm") === "last-device";
    const outcome = await account(env).revokeUserDevice({ userId: identity.userId, callerId: identity.deviceId, deviceId, confirmLastDevice });
    return outcome.ok ? new Response(null, { status: 204 }) : errorReply(request, outcome.code, outcome.message);
  }
  return new Response("Method Not Allowed", { status: 405 });
}

const encoder = new TextEncoder();
const MAX_NAME_BYTES = 255;

function checkNames(fields: Record<string, string>): void {
  for (const [field, value] of Object.entries(fields)) {
    if (encoder.encode(value).length > MAX_NAME_BYTES) throw new BadRequest(`${field} is longer than ${MAX_NAME_BYTES} bytes`);
  }
}

const REWIND_STATUS: Partial<Record<Error_Code, number>> = { [Error_Code.HISTORY_MISMATCH]: 409, [Error_Code.RETRY_LATER]: 503 };

async function rewind(request: Request, env: Env, spaceId: string): Promise<Response> {
  const identity = await authenticate(request, env);
  if (identity instanceof Response) return identity;
  const refused = refuseWithout(request, identity, "space.rewind");
  if (refused) return refused;
  const body = await readMessage(request, RewindRequestSchema);
  const outcome = await account(env).rewind({
    userId: identity.userId,
    spaceId,
    rewindId: body.rewindId,
    restorePointMs: Number(body.restorePointMs),
    expectedHistoryId: body.expectedHistoryId,
  });
  if (!outcome.ok) return errorReply(request, outcome.code, outcome.message, REWIND_STATUS[outcome.code]);
  return reply(request, RewindResultSchema, create(RewindResultSchema, outcome.value));
}

async function usage(request: Request, env: Env, spaceId: string): Promise<Response> {
  const identity = await authenticate(request, env, spaceId);
  if (identity instanceof Response) return identity;
  const refused = refuseWithout(request, identity, "space.usage");
  if (refused) return refused;
  const outcome = await space(env, spaceId).usage(identity.userId);
  if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
  return reply(request, SpaceUsageSchema, outcome.value);
}

async function spaceKey(env: Env, spaceId: string): Promise<Uint8Array> {
  return deriveSpaceKey((await key(env)).secret, spaceId);
}

async function chunk(request: Request, env: Env, objects: ObjectStore, spaceId: string, hex: string): Promise<Response> {
  const params = new URL(request.url).searchParams;
  const ticket = params.get("ticket") ?? "";
  const incarnation = params.get("inc") ?? "";
  if (!INCARNATION.test(incarnation)) return errorReply(request, Error_Code.FORBIDDEN, "invalid or expired ticket");
  const object: ChunkObject = { spaceId, hashHex: hex, incarnation };
  const nowMs = Date.now();

  if (request.method === "GET") {
    if (!(await verifyChunkGrant(await spaceKey(env, spaceId), "get", object, 0, ticket, nowMs))) {
      return errorReply(request, Error_Code.FORBIDDEN, "invalid or expired download ticket");
    }
    const stored = await objects.get(chunkKey(spaceId, hex, incarnation));
    if (!stored) return errorReply(request, Error_Code.INVALID_ARGUMENT, "chunk not found", 404);
    return new Response(stored.body, {
      headers: { "Content-Type": "application/octet-stream", "Content-Length": String(stored.size), "Cache-Control": "private, max-age=3600" },
    });
  }

  if (request.method !== "PUT") return new Response("Method Not Allowed", { status: 405, headers: { Allow: "GET, PUT" } });
  const length = request.headers.get("Content-Length");
  if (length === null || !/^\d+$/.test(length)) return errorReply(request, Error_Code.INVALID_ARGUMENT, "Content-Length is required", 411);
  const size = Number(length);
  if (size < 1 || size > MAX_CHUNK_BYTES) return errorReply(request, Error_Code.LIMIT_EXCEEDED, "chunk size out of range");
  const stored = await storeChunk(objects, await spaceKey(env, spaceId), object, size, ticket, request.body, nowMs);
  if (stored.receipt) return reply(request, UploadReceiptSchema, stored.receipt);
  return reply(request, ErrorSchema, stored.error!, stored.status);
}

async function storeChunk(
  objects: ObjectStore,
  secret: Uint8Array,
  object: ChunkObject,
  size: number,
  ticket: string,
  body: ReadableStream | Uint8Array | null,
  nowMs: number,
): Promise<BundledChunkResult> {
  const hash = fromHex(object.hashHex);
  const refuse = (status: number, code: Error_Code, message: string) => create(BundledChunkResultSchema, { hash, status, error: create(ErrorSchema, { code, message }) });
  if (!(await verifyChunkGrant(secret, "put", object, size, ticket, nowMs))) {
    return refuse(403, Error_Code.FORBIDDEN, "invalid or expired upload ticket, or a size the ticket does not cover");
  }
  try {
    await objects.put(chunkKey(object.spaceId, object.hashHex, object.incarnation), body, { sha256: object.hashHex, onlyIf: { etagDoesNotMatch: "*" } });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error("chunk upload failed", object.spaceId, object.hashHex, errorText(error));
    if (/checksum/i.test(message)) return refuse(400, Error_Code.INVALID_ARGUMENT, "the body does not match the chunk hash");
    const throttled = /\b429\b|10058|too many|rate/i.test(message);
    return refuse(throttled ? 429 : 503, Error_Code.RETRY_LATER, "chunk storage is unavailable; retry the upload");
  }
  const token = await signChunkGrant(secret, "receipt", object, size, nowMs + GRANT_MS);
  return create(BundledChunkResultSchema, { hash, status: 200, receipt: create(UploadReceiptSchema, { hash, size, token }) });
}

const MAX_BUNDLED_CHUNK_BYTES = 1 << 20;
const MAX_BUNDLE_BYTES = 4 << 20;
const BUNDLE_WRITES_AT_ONCE = 6;
const CHUNK_PATH = /^\/v1\/spaces\/([0-9a-f-]{36})\/chunks\/([0-9a-f]{64})$/;

async function bundle(request: Request, env: Env, objects: ObjectStore, spaceId: string): Promise<Response> {
  if (request.method !== "POST") return new Response("Method Not Allowed", { status: 405, headers: { Allow: "POST" } });
  const length = request.headers.get("Content-Length");
  if (length === null || !/^\d+$/.test(length)) return errorReply(request, Error_Code.INVALID_ARGUMENT, "Content-Length is required", 411);
  const tooLarge = () => errorReply(request, Error_Code.LIMIT_EXCEEDED, `a bundle is at most ${MAX_BUNDLE_BYTES} bytes`);
  if (Number(length) > MAX_BUNDLE_BYTES) return tooLarge();
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (bytes.length > MAX_BUNDLE_BYTES) return tooLarge();
  const entries = countBundledChunks(bytes);
  if (entries < 1 || entries > MAX_BUNDLE_CHUNKS) return errorReply(request, Error_Code.LIMIT_EXCEEDED, `a bundle holds 1 to ${MAX_BUNDLE_CHUNKS} chunks`);
  const { chunks } = decodeChunkBundle(bytes);
  const seen = new Set<string>();
  const uploads = chunks.map((chunk) => {
    let target: URL;
    try {
      target = new URL(chunk.target);
    } catch {
      throw new BadRequest("a bundled chunk's target is not a URL");
    }
    const path = CHUNK_PATH.exec(target.pathname);
    const incarnation = target.searchParams.get("inc") ?? "";
    if (!path || path[1] !== spaceId || !INCARNATION.test(incarnation)) throw new BadRequest("a bundled chunk's target is not an upload target of this space");
    if (seen.has(path[2]!)) throw new BadRequest("a bundle lists one chunk twice");
    seen.add(path[2]!);
    if (chunk.data.length < 1 || chunk.data.length > MAX_BUNDLED_CHUNK_BYTES) {
      throw new LimitExceeded(`a bundled chunk is 1 to ${MAX_BUNDLED_CHUNK_BYTES} bytes`);
    }
    return { object: { spaceId, hashHex: path[2]!, incarnation }, ticket: target.searchParams.get("ticket") ?? "", data: chunk.data };
  });
  const secret = await spaceKey(env, spaceId);
  const nowMs = Date.now();
  const results: BundledChunkResult[] = new Array(uploads.length);
  let next = 0;
  const writer = async () => {
    for (let i = next++; i < uploads.length; i = next++) {
      const { object, ticket, data } = uploads[i]!;
      results[i] = await storeChunk(objects, secret, object, data.length, ticket, data, nowMs);
    }
  };
  await Promise.all(Array.from({ length: Math.min(BUNDLE_WRITES_AT_ONCE, uploads.length) }, writer));
  return reply(request, ChunkBundleResultSchema, create(ChunkBundleResultSchema, { results }));
}

class LimitExceeded extends Error {}

function space(env: Env, spaceId: string) {
  return env.SPACE.get(env.SPACE.idFromName(spaceId));
}

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;
  const method = request.method;

  const shared = await shareRoute(request, env, async () => {
    const identity = await authenticate(request, env);
    return identity instanceof Response ? identity : (refuseWithout(request, identity, "share.manage") ?? identity);
  });
  if (shared) return shared;

  if (method === "GET" && path === "/v1/server") {
    const info = create(ServerInfoSchema, {
      serverVersion: SERVER_VERSION,
      protoMin: PROTO_MIN,
      protoMax: PROTO_MAX,
      dataPath: ServerInfo_DataPath.RELAY,
      claimed: await account(env).isClaimed(),
      setupAttempts: await account(env)
        .handlesSetupAttempts()
        .catch(() => false),
      storage: STORAGE,
      commit: SERVER_COMMIT,
    });
    return reply(request, ServerInfoSchema, info);
  }

  if (method === "GET" && path === "/setup") {
    const code = await account(env).issueClaimCode();
    if (code) console.log("Koidrive claim code (valid for 1 hour): " + code);
    const text = (await account(env).isClaimed())
      ? "This Koidrive server already has an owner.\n"
      : "A one-time claim code, valid for one hour, is in this Worker's logs. Find it under this Worker's Observability tab in the dashboard by searching for \"Koidrive claim code\", run `koi server claim`, or read it with `wrangler tail`.\n";
    return new Response(text, { headers: { "Content-Type": "text/plain; charset=utf-8" } });
  }

  if (method === "POST" && path === "/v1/claim") {
    const body = await readMessage(request, ClaimRequestSchema);
    checkNames({ user_name: body.userName, device_name: body.deviceName, platform: body.platform });
    const recoveryKey = (env as Env & { KOIDRIVE_RECOVERY_KEY?: string }).KOIDRIVE_RECOVERY_KEY;
    const outcome = await account(env).claim(body, request.headers.get("CF-Connecting-IP") ?? "", recoveryKey);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, DeviceCredentialsSchema, create(DeviceCredentialsSchema, outcome.value));
  }

  if (method === "POST" && path === "/v1/pair/complete") {
    const body = await readMessage(request, PairCompleteRequestSchema);
    checkNames({ device_name: body.deviceName, platform: body.platform });
    const outcome = await account(env).completePairing(body, request.headers.get("CF-Connecting-IP") ?? "");
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, DeviceCredentialsSchema, create(DeviceCredentialsSchema, outcome.value));
  }

  if (method === "POST" && path === "/v1/token") {
    const body = await readMessage(request, TokenRequestSchema);
    const outcome = await account(env).refreshAccess(body);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    return reply(request, AccessTokenSchema, create(AccessTokenSchema, outcome.value));
  }

  const bundleRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/chunks$/.exec(path);
  if (bundleRoute && isUuidV7(bundleRoute[1]!)) return bundle(request, env, env.BUCKET, bundleRoute[1]!);

  const chunkRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/chunks\/([0-9a-f]{64})$/.exec(path);
  if (chunkRoute && isUuidV7(chunkRoute[1]!)) return chunk(request, env, env.BUCKET, chunkRoute[1]!, chunkRoute[2]!);

  const rewindRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/rewind$/.exec(path);
  if (rewindRoute && method === "POST" && isUuidV7(rewindRoute[1]!)) return rewind(request, env, rewindRoute[1]!);

  const usageRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/usage$/.exec(path);
  if (usageRoute && method === "GET" && isUuidV7(usageRoute[1]!)) return usage(request, env, usageRoute[1]!);

  const devicesRoute = /^\/v1\/devices(?:\/([0-9a-f-]{36}))?$/.exec(path);
  if (devicesRoute) return devices(request, env, devicesRoute[1]);

  const spaceRoute = /^\/v1\/spaces\/([0-9a-f-]{36})\/(ws|rpc)$/.exec(path);
  if (path !== "/v1/pair/start" && path !== "/v1/spaces" && !spaceRoute) return new Response("Not Found", { status: 404 });
  const identity = await authenticate(request, env, spaceRoute && isUuidV7(spaceRoute[1]!) ? spaceRoute[1]! : undefined);
  if (identity instanceof Response) return identity;

  if (method === "POST" && path === "/v1/pair/start") {
    const refused = refuseWithout(request, identity, "device.pair");
    if (refused) return refused;
    const outcome = await account(env).startPairing(identity.deviceId);
    if (!outcome.ok) return errorReply(request, outcome.code, outcome.message);
    const started = create(PairStartResponseSchema, { code: outcome.value.code, expiresAtMs: BigInt(outcome.value.expiresAtMs) });
    return reply(request, PairStartResponseSchema, started);
  }

  if (path === "/v1/spaces") {
    if (method === "GET") {
      return reply(request, SpaceListSchema, await account(env).listSpaces({ userId: identity.userId, after: listCursor(url) }));
    }
    if (method === "POST") {
      const refused = refuseWithout(request, identity, "space.create");
      if (refused) return refused;
      const body = await readMessage(request, CreateSpaceRequestSchema);
      checkNames({ name: body.name });
      const created = await account(env).createSpace({ userId: identity.userId, name: body.name, requestId: body.requestId });
      if (!created.ok) return errorReply(request, created.code, created.message, created.code === Error_Code.INVALID_ARGUMENT ? 409 : undefined);
      return reply(request, SpaceInfoSchema, create(SpaceInfoSchema, created.value), 201);
    }
  }

  if (spaceRoute && isUuidV7(spaceRoute[1]!)) {
    const connect: Connect = { identity, origin: url.origin };
    const stub = space(env, spaceRoute[1]!);
    if (spaceRoute[2] === "ws" && method === "GET") {
      const headers = new Headers(request.headers);
      headers.set(CONNECT_HEADER, JSON.stringify(connect));
      return stub.fetch(new Request(request.url, { headers }));
    }
    if (spaceRoute[2] === "rpc" && method === "POST") {
      const tooLarge = () => errorReply(request, Error_Code.LIMIT_EXCEEDED, "request body larger than 8 MiB", 413);
      if (Number(request.headers.get("Content-Length") ?? 0) > MAX_CLIENT_FRAME) return tooLarge();
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.length > MAX_CLIENT_FRAME) return tooLarge();
      return new Response(await stub.httpRpc(connect, body), { headers: { "Content-Type": "application/x-protobuf" } });
    }
  }

  return new Response("Not Found", { status: 404 });
}

export default {
  async fetch(request, env, _ctx): Promise<Response> {
    try {
      return await route(request, env);
    } catch (error) {
      if (error instanceof BadRequest) return errorReply(request, Error_Code.INVALID_ARGUMENT, error.message);
      if (error instanceof LimitExceeded) return errorReply(request, Error_Code.LIMIT_EXCEEDED, error.message);
      const quota = quotaRefusal(error, Date.now());
      if (!quota) throw error;
      console.error("request stopped at a platform limit", errorText(error));
      return reply(request, ErrorSchema, create(ErrorSchema, { code: Error_Code.QUOTA_EXCEEDED, message: quota.message, retryAfterMs: BigInt(quota.retryAfterMs) }), 429);
    }
  },
} satisfies ExportedHandler<Env>;
