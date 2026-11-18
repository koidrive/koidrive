// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, type MessageShape } from "@bufbuild/protobuf";
import type { Identity } from "../auth";
import { Error_Code, ErrorSchema } from "../gen/koidrive/v1/rpc_pb";
import { causeOf } from "../cause";
import { errorText } from "../errors";
import { CreateShareLinkRequestSchema, ShareInfoSchema, ShareLinkListSchema, ShareLinkSchema, ShareListingSchema, ShareUnlockRequestSchema } from "../gen/koidrive/v1/share_pb";
import { BadRequest, errorReply, readMessage, reply } from "../http";
import type { ObjectStore } from "../host";
import { isUuidV7, toHex } from "../ids";
import { account, space } from "../stubs";
import { clientKey } from "./limiter";
import { LINK_ID, SESSION_MS, type ShareOutcome } from "./links";
import type { ShareFile } from "./space";

export const PUBLIC_HEADERS: Record<string, string> = {
  "Cache-Control": "no-store",
  "X-Robots-Tag": "noindex, nofollow",
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
};

export const VIEWER_HEADERS: Record<string, string> = {
  "Content-Security-Policy":
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Resource-Policy": "same-origin",
  "Permissions-Policy": "camera=(), microphone=(), geolocation=()",
  "X-Robots-Tag": "noindex, nofollow",
};

const SESSION_COOKIE = "fb_share";

type Failure = Extract<ShareOutcome<unknown>, { ok: false }>;

export function withHeaders(response: Response, headers: Record<string, string>): Response {
  for (const [name, value] of Object.entries(headers)) response.headers.set(name, value);
  return response;
}

function refusal(request: Request, failure: Failure): Response {
  const error = create(ErrorSchema, { code: failure.code, message: failure.message, retryAfterMs: BigInt(failure.retryAfterMs ?? 0), cause: failure.cause });
  const response = reply(request, ErrorSchema, error, failure.status);
  if (failure.retryAfterMs !== undefined) response.headers.set("Retry-After", String(Math.ceil(failure.retryAfterMs / 1000)));
  return response;
}

function textRefusal(failure: Failure): Response {
  const response = new Response(failure.message + "\n", { status: failure.status, headers: { "Content-Type": "text/plain; charset=utf-8" } });
  if (failure.retryAfterMs !== undefined) response.headers.set("Retry-After", String(Math.ceil(failure.retryAfterMs / 1000)));
  return response;
}

function answer<S extends DescMessage>(request: Request, schema: S, outcome: ShareOutcome<MessageShape<S>>, status = 200): Response {
  return outcome.ok ? reply(request, schema, outcome.value, status) : refusal(request, outcome);
}

function sessionCookie(request: Request): string | undefined {
  for (const part of (request.headers.get("Cookie") ?? "").split(";")) {
    const [name, ...value] = part.trim().split("=");
    if (name === SESSION_COOKIE) return value.join("=");
  }
  return undefined;
}

/** The ASCII fallback and the RFC 6266 UTF-8 form of a download's file name. */
function contentDisposition(name: string): string {
  const fallback = name.replace(/[^\x20-\x7e]|["\\%;]/g, "_");
  const encoded = encodeURIComponent(name).replace(/['()*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());
  return `attachment; filename="${fallback}"; filename*=UTF-8''${encoded}`;
}

class ChunkCheckFailed extends Error {}

async function openChunk(objects: ObjectStore, chunk: ShareFile["chunks"][number]): Promise<ReadableStream<Uint8Array>> {
  const object = await objects.get(chunk.key);
  if (!object) throw new ChunkCheckFailed(`chunk object ${chunk.key} is missing`);
  const stored = object.checksums.sha256;
  const problem =
    object.size !== chunk.size
      ? `chunk object ${chunk.key} has ${object.size} bytes, expected ${chunk.size}`
      : stored && toHex(new Uint8Array(stored)) !== chunk.hashHex
        ? `chunk object ${chunk.key} has a stored SHA-256 other than its hash`
        : undefined;
  if (problem) {
    await object.body.cancel();
    throw new ChunkCheckFailed(problem);
  }
  return object.body;
}

function attachmentHeaders(name: string): Record<string, string> {
  return {
    "Content-Type": "application/octet-stream",
    "Content-Disposition": contentDisposition(name),
    "Content-Security-Policy": "default-src 'none'; sandbox",
  };
}

async function download(request: Request, env: Env, file: ShareFile, fenceCheck: () => Promise<Failure | undefined>): Promise<Response> {
  if (file.chunks.length === 0) return new Response(new Uint8Array(0), { headers: { ...attachmentHeaders(file.name), "Content-Length": "0" } });
  let current: ReadableStreamDefaultReader<Uint8Array>;
  try {
    current = (await openChunk(env.BUCKET, file.chunks[0]!)).getReader();
  } catch (error) {
    if (!(error instanceof ChunkCheckFailed)) throw error;
    console.error("share download refused", errorText(error));
    return textRefusal({ ok: false, code: Error_Code.INTERNAL, message: "This file failed its integrity check and cannot be downloaded.", status: 502 });
  }
  let next = 1;
  const chunks = new ReadableStream<Uint8Array>(
    {
      async pull(controller) {
        for (;;) {
          const { done, value } = await current.read();
          if (!done) {
            controller.enqueue(value);
            return;
          }
          if (next >= file.chunks.length) {
            controller.close();
            return;
          }
          const failed = await fenceCheck();
          if (failed) {
            console.error("share download stopped", failed.message);
            controller.error(new Error(failed.message));
            return;
          }
          try {
            current = (await openChunk(env.BUCKET, file.chunks[next++]!)).getReader();
          } catch (error) {
            console.error("share download aborted", errorText(error));
            controller.error(error);
            return;
          }
        }
      },
      cancel(reason) {
        return current.cancel(reason);
      },
    },
    { highWaterMark: 0 },
  );
  const { readable, writable } = new FixedLengthStream(file.size);
  chunks.pipeTo(writable).catch(() => {});
  return new Response(readable, { headers: { ...attachmentHeaders(file.name), "Content-Length": String(file.size) } });
}

async function publicApi(request: Request, env: Env, linkId: string, rest: string): Promise<Response> {
  if (!LINK_ID.test(linkId)) {
    return refusal(request, { ok: false, code: Error_Code.NOT_FOUND, message: "this link does not exist, has expired or was revoked", status: 404 });
  }
  const client = clientKey(request);
  const method = request.method;
  const url = new URL(request.url);

  if (rest === "/unlock" && method === "POST") {
    const body = await readMessage(request, ShareUnlockRequestSchema);
    const outcome = await account(env).shareUnlock({ linkId, client, password: body.password });
    if (!outcome.ok) return refusal(request, outcome);
    const response = new Response(null, { status: 204 });
    if (outcome.value.session) {
      response.headers.set(
        "Set-Cookie",
        `${SESSION_COOKIE}=${outcome.value.session}; Path=/v1/share/${linkId}; Max-Age=${SESSION_MS / 1000}; HttpOnly; Secure; SameSite=Strict`,
      );
    }
    return response;
  }

  if (method !== "GET") return new Response("Method Not Allowed", { status: 405 });
  const fileRoute = /^\/file\/([^/]+)$/.exec(rest);
  if (rest !== "" && rest !== "/list" && !fileRoute) return new Response("Not Found", { status: 404 });

  const session = sessionCookie(request);
  const resolved = await account(env).shareResolve({ linkId, client, session, counted: true });
  if (!resolved.ok) {
    if (rest === "" && resolved.code === Error_Code.UNAUTHENTICATED) return reply(request, ShareInfoSchema, create(ShareInfoSchema, { passwordRequired: true }), 401);
    return fileRoute ? textRefusal(resolved) : refusal(request, resolved);
  }
  const { spaceId, nodeId: rootId, expiresAtMs } = resolved.value;
  const stub = space(env, spaceId);

  if (rest === "") return answer(request, ShareInfoSchema, await stub.shareInfo(rootId, expiresAtMs));
  if (rest === "/list") {
    const dir = url.searchParams.get("dir") ?? "";
    return answer(request, ShareListingSchema, await stub.shareList(rootId, dir, url.searchParams.get("cursor") ?? ""));
  }
  const nodeId = fileRoute![1]!;
  if (!isUuidV7(nodeId)) return textRefusal({ ok: false, code: Error_Code.NOT_FOUND, message: "this item is not available through this link", status: 404 });
  const file = await stub.shareFile(rootId, nodeId);
  if (!file.ok) return textRefusal(file);
  const fence = { historyId: file.value.historyId, generation: resolved.value.generation };
  return download(request, env, file.value, async () => {
    try {
      const checked = await space(env, spaceId).shareContinue({ linkId, client, session }, fence);
      return checked.ok ? undefined : checked;
    } catch (error) {
      console.error("share fence check failed", errorText(error));
      return { ok: false, code: Error_Code.RETRY_LATER, message: "this link is temporarily unavailable; retry later", status: 503, cause: causeOf(error) };
    }
  });
}

async function ownerApi(request: Request, env: Env, spaceId: string, linkId: string | undefined, identity: Identity): Promise<Response> {
  const url = new URL(request.url);
  const method = request.method;
  if (linkId === undefined && method === "POST") {
    const body = await readMessage(request, CreateShareLinkRequestSchema);
    const outcome = await account(env).shareCreate({
      userId: identity.userId,
      spaceId,
      nodeId: body.nodeId,
      expiresAtMs: Number(body.expiresAtMs),
      noExpiry: body.noExpiry,
      password: body.password,
      origin: url.origin,
      requestId: body.requestId,
    });
    return answer(request, ShareLinkSchema, outcome, 201);
  }
  if (linkId === undefined && method === "GET") {
    const outcome = await account(env).shareList({ userId: identity.userId, spaceId, cursor: url.searchParams.get("cursor") ?? "", origin: url.origin });
    return answer(request, ShareLinkListSchema, outcome);
  }
  if (linkId !== undefined && method === "DELETE") {
    const outcome = await account(env).shareRevoke({ userId: identity.userId, spaceId, linkId });
    return outcome.ok ? new Response(null, { status: 204 }) : refusal(request, outcome);
  }
  return new Response("Method Not Allowed", { status: 405 });
}

async function viewer(request: Request, env: Env, linkId: string): Promise<Response> {
  const headers = { ...VIEWER_HEADERS, "Cache-Control": "no-store" };
  if (request.method !== "GET" && request.method !== "HEAD") return withHeaders(new Response("Method Not Allowed", { status: 405 }), headers);
  if (!LINK_ID.test(linkId)) return withHeaders(new Response("Not Found", { status: 404 }), headers);
  const page = await env.ASSETS.fetch(new URL("/viewer/", request.url));
  return withHeaders(new Response(request.method === "HEAD" ? null : page.body, { status: page.status, headers: { "Content-Type": "text/html; charset=utf-8" } }), headers);
}

export async function shareRoute(request: Request, env: Env, authenticate: () => Promise<Identity | Response>): Promise<Response | undefined> {
  const path = new URL(request.url).pathname;

  const page = /^\/s\/([^/]*)$/.exec(path);
  if (page) return viewer(request, env, page[1]!);

  const shared = /^\/v1\/share\/([^/]*)(\/.*)?$/.exec(path);
  if (shared) {
    try {
      return withHeaders(await publicApi(request, env, shared[1]!, shared[2] ?? ""), PUBLIC_HEADERS);
    } catch (error) {
      if (error instanceof BadRequest) return withHeaders(errorReply(request, Error_Code.INVALID_ARGUMENT, error.message), PUBLIC_HEADERS);
      throw error;
    }
  }

  const owned = /^\/v1\/spaces\/([0-9a-f-]{36})\/links(?:\/([^/]+))?$/.exec(path);
  if (owned && isUuidV7(owned[1]!)) {
    const identity = await authenticate();
    if (identity instanceof Response) return identity;
    return ownerApi(request, env, owned[1]!, owned[2], identity);
  }
  return undefined;
}
