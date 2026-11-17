// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { base64url, fromBase64url, hmac, hmacVerify } from "../crypto";
import { type Error_Cause, Error_Code } from "../gen/koidrive/v1/rpc_pb";
import { type ShareLink, type ShareLinkList, ShareLinkListSchema, ShareLinkSchema } from "../gen/koidrive/v1/share_pb";
import type { Sql } from "../host";
import { isUuidV7 } from "../ids";
import { ClientLimiter } from "./limiter";

const encoder = new TextEncoder();
const DAY_MS = 24 * 3_600_000;

/** Highest PBKDF2 iteration count workerd accepts; see cloudflare/workerd#1346. */
export const SHARE_PASSWORD_ITERATIONS = 100_000;
const SHARE_PASSWORD_KDF = "pbkdf2-sha256";
const SHARE_FAILURES_BEFORE_LOCK = 5;
export const SHARE_LOCK_MS = 15 * 60_000;
export const DEFAULT_EXPIRY_MS = 7 * DAY_MS;
export const MAX_EXPIRY_MS = 365 * DAY_MS;
const MIN_EXPIRY_MS = 60_000;
export const MAX_ACTIVE_LINKS = 1000;
export const MAX_REQUEST_ID_LENGTH = 64;
const LIST_PAGE = 100;
const PASSWORD_MIN = 8;
const PASSWORD_MAX = 256;
export const SESSION_MS = 3_600_000;

export const LINK_ID = /^[A-Za-z0-9_-]{21}[AQgw]$/;

export interface ShareLinkRow {
  link_id: string;
  space_id: string;
  node_id: string;
  created_by: string;
  created_at_ms: number;
  expires_at_ms: number | null;
  password_kdf: string | null;
  password_hash: ArrayBuffer | null;
  password_salt: ArrayBuffer | null;
  password_iterations: number | null;
  revoked_at_ms: number | null;
  failed_attempts: number;
  locked_until_ms: number | null;
}

export type Outcome<T> = { ok: true; value: T } | { ok: false; code: Error_Code; message: string; cause?: Error_Cause };
export type ShareOutcome<T> = { ok: true; value: T } | { ok: false; code: Error_Code; message: string; status: number; retryAfterMs?: number; cause?: Error_Cause };

function ok<T>(value: T): { ok: true; value: T } {
  return { ok: true, value };
}

function failure(code: Error_Code, message: string, status: number, retryAfterMs?: number): { ok: false; code: Error_Code; message: string; status: number; retryAfterMs?: number } {
  return { ok: false, code, message, status, ...(retryAfterMs === undefined ? {} : { retryAfterMs }) };
}

const notFound = () => failure(Error_Code.NOT_FOUND, "this link does not exist, has expired or was revoked", 404);
const notOwner = () => failure(Error_Code.FORBIDDEN, "only the space's owner manages its links", 403);
const invalid = (message: string) => failure(Error_Code.INVALID_ARGUMENT, message, 400);

async function passwordHash(password: string, salt: Uint8Array, iterations: number): Promise<ArrayBuffer> {
  const key = await crypto.subtle.importKey("raw", encoder.encode(password), "PBKDF2", false, ["deriveBits"]);
  return crypto.subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt, iterations }, key, 256);
}

function sameHash(a: ArrayBuffer, b: ArrayBuffer): boolean {
  return a.byteLength === b.byteLength && crypto.subtle.timingSafeEqual(a, b);
}

export interface ResolvedLink {
  spaceId: string;
  nodeId: string;
  expiresAtMs: number;
  generation: number;
}

export interface ShareLinkDeps {
  clock(): number;
  serverSecret(): Promise<Uint8Array>;
  spaceClosed(spaceId: string): boolean;
}

export class ShareLinks {
  readonly limiter = new ClientLimiter();

  constructor(
    private readonly sql: Sql,
    private readonly deps: ShareLinkDeps,
  ) {}

  private row(linkId: string): ShareLinkRow | undefined {
    return this.sql.exec("SELECT * FROM share_links WHERE link_id = ?", linkId).toArray()[0] as unknown as ShareLinkRow | undefined;
  }

  private active(linkId: string, nowMs: number): ShareLinkRow | undefined {
    const link = this.row(linkId);
    if (!link || link.revoked_at_ms !== null || (link.expires_at_ms !== null && link.expires_at_ms <= nowMs)) return undefined;
    return link;
  }

  private ownsSpace(userId: string, spaceId: string): boolean {
    return this.sql.exec("SELECT 1 FROM spaces WHERE space_id = ? AND owner_id = ?", spaceId, userId).toArray().length > 0;
  }

  private activeCount(spaceId: string, nowMs: number): number {
    return Number(
      this.sql
        .exec("SELECT COUNT(*) AS n FROM share_links WHERE space_id = ? AND revoked_at_ms IS NULL AND (expires_at_ms IS NULL OR expires_at_ms > ?)", spaceId, nowMs)
        .one().n,
    );
  }

  async create(args: { spaceId: string; nodeId: string; createdBy: string; expiresAtMs?: number | null; password?: string }): Promise<string> {
    const linkId = base64url(crypto.getRandomValues(new Uint8Array(16)));
    const salt = args.password ? crypto.getRandomValues(new Uint8Array(16)) : undefined;
    const hash = salt ? await passwordHash(args.password!, salt, SHARE_PASSWORD_ITERATIONS) : null;
    this.insert(linkId, args, this.deps.clock(), salt, hash);
    return linkId;
  }

  private insert(
    linkId: string,
    args: { spaceId: string; nodeId: string; createdBy: string; expiresAtMs?: number | null; requestId?: string },
    createdAtMs: number,
    salt: Uint8Array | undefined,
    hash: ArrayBuffer | null,
  ): void {
    this.sql.exec(
      `INSERT INTO share_links (link_id, space_id, node_id, created_by, created_at_ms, expires_at_ms, password_kdf,
         password_hash, password_salt, password_iterations, request_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      linkId,
      args.spaceId,
      args.nodeId,
      args.createdBy,
      createdAtMs,
      args.expiresAtMs ?? null,
      salt ? SHARE_PASSWORD_KDF : null,
      hash,
      salt ? (salt.buffer as ArrayBuffer) : null,
      salt ? SHARE_PASSWORD_ITERATIONS : null,
      args.requestId || null,
    );
  }

  revoke(linkId: string): void {
    this.sql.exec("UPDATE share_links SET revoked_at_ms = ? WHERE link_id = ? AND revoked_at_ms IS NULL", this.deps.clock(), linkId);
  }

  async open(linkId: string, password: string | undefined): Promise<Outcome<{ spaceId: string; nodeId: string }>> {
    const nowMs = this.deps.clock();
    const found = this.active(linkId, nowMs);
    if (!found) return { ok: false, code: Error_Code.FORBIDDEN, message: "this link does not exist or has expired" };
    if (found.password_hash) {
      const locked = () => ({ ok: false as const, code: Error_Code.RETRY_LATER, message: "too many wrong passwords" });
      if (found.locked_until_ms !== null && found.locked_until_ms > nowMs) return locked();
      if (found.password_kdf !== SHARE_PASSWORD_KDF) return { ok: false, code: Error_Code.INTERNAL, message: "unsupported password scheme" };
      const given = password === undefined ? undefined : await passwordHash(password, new Uint8Array(found.password_salt!), found.password_iterations!);
      const link = this.row(linkId)!;
      if (link.locked_until_ms !== null && link.locked_until_ms > nowMs) return locked();
      if (!given || !sameHash(given, link.password_hash!)) {
        const failed = link.failed_attempts + 1;
        const lock = failed >= SHARE_FAILURES_BEFORE_LOCK;
        this.sql.exec(
          "UPDATE share_links SET failed_attempts = ?, locked_until_ms = ? WHERE link_id = ?",
          lock ? 0 : failed,
          lock ? nowMs + SHARE_LOCK_MS : link.locked_until_ms,
          linkId,
        );
        return { ok: false, code: Error_Code.UNAUTHENTICATED, message: "wrong password" };
      }
      if (link.failed_attempts > 0) this.sql.exec("UPDATE share_links SET failed_attempts = 0 WHERE link_id = ?", linkId);
    }
    return ok({ spaceId: found.space_id, nodeId: found.node_id });
  }

  private message(link: ShareLinkRow, origin: string): ShareLink {
    return create(ShareLinkSchema, {
      linkId: link.link_id,
      url: `${origin}/s/${link.link_id}`,
      spaceId: link.space_id,
      nodeId: link.node_id,
      createdAtMs: BigInt(link.created_at_ms),
      expiresAtMs: BigInt(link.expires_at_ms ?? 0),
      hasPassword: link.password_hash !== null,
      revokedAtMs: BigInt(link.revoked_at_ms ?? 0),
    });
  }

  async ownerCreate(
    args: { userId: string; spaceId: string; nodeId: string; expiresAtMs: number; noExpiry: boolean; password: string; origin: string; requestId?: string },
    checkNode: () => Promise<ShareOutcome<null>>,
  ): Promise<ShareOutcome<ShareLink>> {
    const nowMs = this.deps.clock();
    const requestId = args.requestId ?? "";
    if (!this.ownsSpace(args.userId, args.spaceId)) return notOwner();
    if (requestId.length > MAX_REQUEST_ID_LENGTH) return invalid(`request_id is longer than ${MAX_REQUEST_ID_LENGTH} characters`);
    const made = () => (requestId ? this.requestedLink(args.spaceId, requestId) : undefined);
    const earlier = made();
    if (earlier) return this.resent(earlier, args);
    if (!isUuidV7(args.nodeId)) return invalid("node_id must be a UUIDv7");
    if (args.noExpiry && args.expiresAtMs !== 0) return invalid("no_expiry cannot be combined with expires_at_ms");
    if (args.expiresAtMs !== 0 && (args.expiresAtMs <= nowMs + MIN_EXPIRY_MS || args.expiresAtMs > nowMs + MAX_EXPIRY_MS)) {
      return invalid("expires_at_ms must lie more than one minute and at most 365 days ahead");
    }
    if (args.password !== "") {
      const length = [...args.password].length;
      if (!args.password.isWellFormed() || length < PASSWORD_MIN || length > PASSWORD_MAX) {
        return invalid(`a password has ${PASSWORD_MIN} to ${PASSWORD_MAX} Unicode scalar values`);
      }
    }
    if (this.deps.spaceClosed(args.spaceId)) return failure(Error_Code.RETRY_LATER, "the space is being rewound; retry later", 503);
    const node = await checkNode();
    if (!node.ok) return node;
    const salt = args.password ? crypto.getRandomValues(new Uint8Array(16)) : undefined;
    const hash = salt ? await passwordHash(args.password, salt, SHARE_PASSWORD_ITERATIONS) : null;
    const createdMs = this.deps.clock();
    const meanwhile = made();
    if (meanwhile) return this.resent(meanwhile, args);
    if (this.activeCount(args.spaceId, createdMs) >= MAX_ACTIVE_LINKS) {
      return failure(Error_Code.LIMIT_EXCEEDED, `a space has at most ${MAX_ACTIVE_LINKS} active links`, 413);
    }
    const linkId = base64url(crypto.getRandomValues(new Uint8Array(16)));
    const expiresAtMs = args.noExpiry ? null : args.expiresAtMs || createdMs + DEFAULT_EXPIRY_MS;
    this.insert(linkId, { spaceId: args.spaceId, nodeId: args.nodeId, createdBy: args.userId, expiresAtMs, requestId }, createdMs, salt, hash);
    return ok(this.message(this.row(linkId)!, args.origin));
  }

  private requestedLink(spaceId: string, requestId: string): ShareLinkRow | undefined {
    return this.sql.exec("SELECT * FROM share_links WHERE space_id = ? AND request_id = ?", spaceId, requestId).toArray()[0] as unknown as ShareLinkRow | undefined;
  }

  private async resent(
    link: ShareLinkRow,
    args: { nodeId: string; expiresAtMs: number; noExpiry: boolean; password: string; origin: string },
  ): Promise<ShareOutcome<ShareLink>> {
    const expiresAtMs = args.noExpiry ? null : args.expiresAtMs || link.created_at_ms + DEFAULT_EXPIRY_MS;
    const samePassword =
      args.password === ""
        ? link.password_hash === null
        : link.password_hash !== null &&
          sameHash(await passwordHash(args.password, new Uint8Array(link.password_salt!), link.password_iterations!), link.password_hash);
    if (link.node_id !== args.nodeId || link.expires_at_ms !== expiresAtMs || !samePassword) return invalid("this request_id was used with other fields");
    return ok(this.message(link, args.origin));
  }

  ownerList(args: { userId: string; spaceId: string; cursor: string; origin: string }): ShareOutcome<ShareLinkList> {
    if (!this.ownsSpace(args.userId, args.spaceId)) return notOwner();
    let found: ShareLinkRow[];
    if (args.cursor) {
      const match = /^(\d{1,16})\.([A-Za-z0-9_-]{22})$/.exec(args.cursor);
      if (!match) return invalid("malformed cursor");
      const createdMs = Number(match[1]);
      found = this.sql
        .exec(
          `SELECT * FROM share_links WHERE space_id = ? AND (created_at_ms < ? OR (created_at_ms = ? AND link_id < ?))
           ORDER BY created_at_ms DESC, link_id DESC LIMIT ?`,
          args.spaceId,
          createdMs,
          createdMs,
          match[2]!,
          LIST_PAGE + 1,
        )
        .toArray() as unknown as ShareLinkRow[];
    } else {
      found = this.sql
        .exec("SELECT * FROM share_links WHERE space_id = ? ORDER BY created_at_ms DESC, link_id DESC LIMIT ?", args.spaceId, LIST_PAGE + 1)
        .toArray() as unknown as ShareLinkRow[];
    }
    const page = found.slice(0, LIST_PAGE);
    const last = page.at(-1);
    return ok(
      create(ShareLinkListSchema, {
        links: page.map((link) => this.message(link, args.origin)),
        nextCursor: found.length > LIST_PAGE && last ? `${last.created_at_ms}.${last.link_id}` : "",
      }),
    );
  }

  ownerRevoke(args: { userId: string; spaceId: string; linkId: string }): ShareOutcome<null> {
    if (!this.ownsSpace(args.userId, args.spaceId)) return notOwner();
    const link = this.row(args.linkId);
    if (!link || link.space_id !== args.spaceId) return failure(Error_Code.NOT_FOUND, "no such link in this space", 404);
    this.revoke(args.linkId);
    return ok(null);
  }

  private async sessionKey(): Promise<Uint8Array> {
    return hmac(await this.deps.serverSecret(), "koidrive/share-session/v1");
  }

  private sessionMessage(link: ShareLinkRow, expiresAtMs: number): string {
    return `${link.link_id}|${expiresAtMs}|${base64url(new Uint8Array(link.password_salt!))}`;
  }

  private async validSession(link: ShareLinkRow, session: string | undefined, nowMs: number): Promise<boolean> {
    const [version, expires, macText, ...rest] = (session ?? "").split(".");
    if (version !== "v1" || rest.length > 0 || !expires || !/^\d{1,16}$/.test(expires) || macText === undefined) return false;
    const expiresAtMs = Number(expires);
    if (expiresAtMs <= nowMs) return false;
    const mac = fromBase64url(macText);
    return mac !== undefined && hmacVerify(await this.sessionKey(), this.sessionMessage(link, expiresAtMs), mac);
  }

  async resolve(args: { linkId: string; client: string; session?: string; counted: boolean }): Promise<ShareOutcome<ResolvedLink>> {
    const nowMs = this.deps.clock();
    if (args.counted) {
      const wait = this.limiter.request(args.client, nowMs);
      if (wait !== undefined) return failure(Error_Code.RETRY_LATER, "too many requests; retry later", 429, wait);
    }
    const link = this.active(args.linkId, nowMs);
    if (!link) {
      if (args.counted) this.limiter.miss(args.client, nowMs);
      return notFound();
    }
    if (link.password_hash && !(await this.validSession(link, args.session, nowMs))) {
      return failure(Error_Code.UNAUTHENTICATED, "this link needs its password", 401);
    }
    if (this.deps.spaceClosed(link.space_id)) return failure(Error_Code.RETRY_LATER, "this link is temporarily unavailable; retry later", 503);
    const rewinds = this.sql.exec("SELECT rewind_count FROM space_rewinds WHERE space_id = ?", link.space_id).toArray()[0];
    return ok({ spaceId: link.space_id, nodeId: link.node_id, expiresAtMs: link.expires_at_ms ?? 0, generation: Number(rewinds?.rewind_count ?? 0) });
  }

  async unlock(args: { linkId: string; client: string; password: string }): Promise<ShareOutcome<{ session?: string }>> {
    const nowMs = this.deps.clock();
    const wait = this.limiter.request(args.client, nowMs);
    if (wait !== undefined) return failure(Error_Code.RETRY_LATER, "too many requests; retry later", 429, wait);
    const opened = await this.open(args.linkId, args.password);
    if (!opened.ok) {
      if (opened.code === Error_Code.FORBIDDEN) {
        this.limiter.miss(args.client, nowMs);
        return notFound();
      }
      if (opened.code === Error_Code.RETRY_LATER) {
        const lockedUntil = this.row(args.linkId)?.locked_until_ms ?? nowMs;
        return failure(opened.code, opened.message, 429, Math.max(0, lockedUntil - nowMs));
      }
      return failure(opened.code, opened.message, opened.code === Error_Code.UNAUTHENTICATED ? 401 : 500);
    }
    const link = this.row(args.linkId)!;
    if (!link.password_hash) return ok({});
    const expiresAtMs = this.deps.clock() + SESSION_MS;
    const mac = await hmac(await this.sessionKey(), this.sessionMessage(link, expiresAtMs));
    return ok({ session: `v1.${expiresAtMs}.${base64url(mac)}` });
  }
}
