// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccountDO } from "../src/account-do";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  RewindRequestSchema,
  RewindResultSchema,
  SpaceInfoSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { CreateShareLinkRequestSchema, type ShareLink, ShareLinkListSchema, ShareLinkSchema } from "../src/gen/koidrive/v1/share_pb";
import { fromBase64url, base64url } from "../src/crypto";
import { toHex, uuidv7 } from "../src/ids";
import { clientKey, MISSES_PER_HOUR, REQUESTS_PER_MINUTE } from "../src/share/limiter";
import { DEFAULT_EXPIRY_MS, LINK_ID, MAX_ACTIVE_LINKS, SESSION_MS, SHARE_LOCK_MS } from "../src/share/links";
import type { SpaceDO } from "../src/space-do";
import { chunkKey } from "../src/space/store";
import worker from "../src/worker";
import { content, op, TestClient } from "./helpers";
import { simulatedPitr } from "./pitr";

simulatedPitr.install();

const BASE = "https://koidrive.test";
const MINUTE = 60_000;
const decoder = new TextDecoder();

function account() {
  return env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
}

function space(spaceId: string) {
  return env.SPACE.get(env.SPACE.idFromName(spaceId));
}

afterEach(async () => {
  vi.restoreAllMocks();
  await runInDurableObject(account(), (instance: AccountDO) => {
    instance.clock = () => Date.now();
  });
});

function setAccountClock(atMs: number) {
  return runInDurableObject(account(), (instance: AccountDO) => {
    instance.clock = () => atMs;
  });
}

async function owned<Res extends DescMessage>(
  path: string,
  init: { method?: string; token?: string; body?: Uint8Array },
  schema: Res,
): Promise<{ status: number; message?: MessageShape<Res> }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  return { status: response.status, message: response.ok && bytes.length > 0 ? fromBinary(schema, bytes) : undefined };
}

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

let owner: DeviceCredentials | undefined;

async function getOwner(): Promise<DeviceCredentials> {
  if (owner) return owner;
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  const claimed = await owned(
    "/v1/claim",
    { body: body(ClaimRequestSchema, { code: line!.split(": ")[1]!, userName: "Owner", deviceName: "Laptop A", platform: "macos" }) },
    DeviceCredentialsSchema,
  );
  owner = claimed.message!;
  return owner;
}

async function newSpace() {
  const creds = await getOwner();
  const created = await owned("/v1/spaces", { token: creds.access!.token, body: body(CreateSpaceRequestSchema, { name: `Home ${crypto.randomUUID()}` }) }, SpaceInfoSchema);
  const spaceId = created.message!.spaceId;
  const device = new TestClient(space(spaceId), spaceId, {
    deviceId: creds.deviceId,
    userId: creds.userId,
    deviceName: "Laptop A",
    kind: "device",
    scope: null,
    role: "owner",
    maxFormat: 1,
    origin: BASE,
  });
  const ids = { docs: uuidv7(), sub: uuidv7(), other: uuidv7(), a: uuidv7(), b: uuidv7(), c: uuidv7(), top: uuidv7() };
  const files = {
    a: await content("alpha"),
    b: await content("bravo"),
    c: await content("charlie"),
    top: await content("top"),
  };
  const receipts = [];
  for (const parts of Object.values(files)) receipts.push(...(await device.uploadParts(parts)));
  await device.commit(
    [
      op.mkdir(ids.docs, spaceId, "Docs"),
      op.mkdir(ids.sub, ids.docs, "Sub"),
      op.mkdir(ids.other, spaceId, "Other"),
      op.create(ids.a, ids.docs, "a.txt", files.a.content),
      op.create(ids.b, ids.sub, "b.txt", files.b.content),
      op.create(ids.c, ids.other, "c.txt", files.c.content),
      op.create(ids.top, spaceId, "top.txt", files.top.content),
    ],
    { receipts },
  );
  return { spaceId, creds, device, ids };
}

async function addFile(device: TestClient, parentId: string, name: string, ...parts: string[]): Promise<string> {
  const nodeId = uuidv7();
  const made = await content(...parts);
  await device.commit([op.create(nodeId, parentId, name, made.content)], { receipts: await device.uploadParts(made) });
  return nodeId;
}

async function createLink(spaceId: string, init: MessageInitShape<typeof CreateShareLinkRequestSchema>) {
  const creds = await getOwner();
  return owned(`/v1/spaces/${spaceId}/links`, { token: creds.access!.token, body: body(CreateShareLinkRequestSchema, init) }, ShareLinkSchema);
}

async function link(spaceId: string, init: MessageInitShape<typeof CreateShareLinkRequestSchema>): Promise<ShareLink> {
  const created = await createLink(spaceId, init);
  expect(created.status).toBe(201);
  return created.message!;
}

let nextIp = 1;
function newIp(): string {
  const n = nextIp++;
  return `198.51.${Math.floor(n / 250)}.${(n % 250) + 1}`;
}

interface PublicReply {
  status: number;
  headers: Headers;
  json: Record<string, unknown> | null;
  text: string;
}

async function pub(path: string, init: { ip: string; method?: string; cookie?: string; json?: unknown }): Promise<PublicReply> {
  const headers: Record<string, string> = { Accept: "application/json", "CF-Connecting-IP": init.ip };
  if (init.cookie) headers.Cookie = init.cookie;
  if (init.json !== undefined) headers["Content-Type"] = "application/json";
  const response = await exports.default.fetch(BASE + path, {
    method: init.method ?? (init.json !== undefined ? "POST" : "GET"),
    headers,
    body: init.json !== undefined ? JSON.stringify(init.json) : undefined,
  });
  const text = await response.text();
  let json: Record<string, unknown> | null = null;
  try {
    json = text ? (JSON.parse(text) as Record<string, unknown>) : null;
  } catch {
    json = null;
  }
  return { status: response.status, headers: response.headers, json, text };
}

function cookieOf(reply: PublicReply): string {
  return reply.headers.get("Set-Cookie")!.split(";")[0]!;
}

async function readAll(response: Response): Promise<{ bytes: number[]; error?: unknown }> {
  const reader = response.body!.getReader();
  const bytes: number[] = [];
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return { bytes };
      bytes.push(...value);
    }
  } catch (error) {
    return { bytes, error };
  }
}

describe("share links: owner API (RFC 0005 section 2)", () => {
  it("creates links with the default expiry, explicit expiry or none, checks passwords and nodes, and lists them newest first", async () => {
    const { spaceId, ids } = await newSpace();
    const before = Date.now();
    const file = await link(spaceId, { nodeId: ids.top });
    expect(file.linkId).toMatch(LINK_ID);
    expect(file.url).toBe(`${BASE}/s/${file.linkId}`);
    expect(file.nodeId).toBe(ids.top);
    expect(file.hasPassword).toBe(false);
    expect(file.revokedAtMs).toBe(0n);
    expect(Number(file.expiresAtMs - file.createdAtMs)).toBe(DEFAULT_EXPIRY_MS);
    expect(Number(file.createdAtMs)).toBeGreaterThanOrEqual(before);

    const forever = await link(spaceId, { nodeId: ids.docs, noExpiry: true, password: "correct horse" });
    expect(forever.expiresAtMs).toBe(0n);
    expect(forever.hasPassword).toBe(true);
    const later = Date.now() + 30 * 24 * 3_600_000;
    expect((await link(spaceId, { nodeId: spaceId, expiresAtMs: BigInt(later) })).expiresAtMs).toBe(BigInt(later));

    const refused = async (init: MessageInitShape<typeof CreateShareLinkRequestSchema>) => (await createLink(spaceId, init)).status;
    expect(await refused({ nodeId: ids.top, noExpiry: true, expiresAtMs: BigInt(later) })).toBe(400);
    expect(await refused({ nodeId: ids.top, expiresAtMs: BigInt(Date.now() + 30_000) })).toBe(400);
    expect(await refused({ nodeId: ids.top, expiresAtMs: BigInt(Date.now() + 366 * 24 * 3_600_000) })).toBe(400);
    expect(await refused({ nodeId: ids.top, password: "seven77" })).toBe(400);
    expect(await refused({ nodeId: ids.top, password: "x".repeat(257) })).toBe(400);
    expect(await refused({ nodeId: "not-a-uuid" })).toBe(400);
    expect(await refused({ nodeId: uuidv7() })).toBe(404);

    const creds = await getOwner();
    const listed = await owned(`/v1/spaces/${spaceId}/links`, { method: "GET", token: creds.access!.token }, ShareLinkListSchema);
    expect(listed.status).toBe(200);
    expect(listed.message!.links.map((l) => l.nodeId)).toEqual([spaceId, ids.docs, ids.top]);
    expect(listed.message!.nextCursor).toBe("");

    const stored = await runInDurableObject(account(), (_instance, state) =>
      state.storage.sql.exec("SELECT password_kdf, password_iterations FROM share_links WHERE link_id = ?", forever.linkId).one(),
    );
    expect(stored).toEqual({ password_kdf: "pbkdf2-sha256", password_iterations: 100_000 });
  });

  it("refuses callers without a token, callers who do not own the space, and trashed nodes; revokes idempotently", async () => {
    const { spaceId, ids, device } = await newSpace();
    const creds = await getOwner();
    expect((await owned(`/v1/spaces/${spaceId}/links`, { body: body(CreateShareLinkRequestSchema, { nodeId: ids.top }) }, ShareLinkSchema)).status).toBe(401);

    const foreign = uuidv7();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version) VALUES (?, 'x', ?, 0, 1)", foreign, uuidv7());
    });
    expect((await owned(`/v1/spaces/${foreign}/links`, { token: creds.access!.token, body: body(CreateShareLinkRequestSchema, { nodeId: foreign }) }, ShareLinkSchema)).status).toBe(403);
    expect((await owned(`/v1/spaces/${foreign}/links`, { method: "GET", token: creds.access!.token }, ShareLinkListSchema)).status).toBe(403);

    await device.commit([op.delete(ids.other)]);
    expect((await createLink(spaceId, { nodeId: ids.other })).status).toBe(404);
    expect((await createLink(spaceId, { nodeId: ids.c })).status).toBe(404);

    const shared = await link(spaceId, { nodeId: ids.top });
    const revoke = (id: string, onSpace = spaceId) => owned(`/v1/spaces/${onSpace}/links/${id}`, { method: "DELETE", token: creds.access!.token }, ShareLinkSchema);
    expect((await revoke(shared.linkId)).status).toBe(204);
    expect((await revoke(shared.linkId)).status).toBe(204);
    expect((await revoke(base64url(crypto.getRandomValues(new Uint8Array(16))))).status).toBe(404);
    const elsewhere = await newSpace();
    expect((await revoke(shared.linkId, elsewhere.spaceId)).status).toBe(404);
    const listed = await owned(`/v1/spaces/${spaceId}/links`, { method: "GET", token: creds.access!.token }, ShareLinkListSchema);
    expect(listed.message!.links.find((l) => l.linkId === shared.linkId)!.revokedAtMs).toBeGreaterThan(0n);
  });

  it("caps a space at 1,000 active links, not counting revoked or expired ones", async () => {
    const { spaceId, ids, creds } = await newSpace();
    await runInDurableObject(account(), (_instance, state) => {
      for (let i = 0; i < MAX_ACTIVE_LINKS - 1; i++) {
        state.storage.sql.exec(
          "INSERT INTO share_links (link_id, space_id, node_id, created_by, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, ?, NULL)",
          base64url(crypto.getRandomValues(new Uint8Array(16))),
          spaceId,
          ids.top,
          creds.userId,
          i,
        );
      }
      state.storage.sql.exec(
        "INSERT INTO share_links (link_id, space_id, node_id, created_by, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, 0, 1)",
        base64url(crypto.getRandomValues(new Uint8Array(16))),
        spaceId,
        ids.top,
        creds.userId,
      );
    });
    const last = await link(spaceId, { nodeId: ids.top });
    expect((await createLink(spaceId, { nodeId: ids.top })).status).toBe(413);
    await owned(`/v1/spaces/${spaceId}/links/${last.linkId}`, { method: "DELETE", token: creds.access!.token }, ShareLinkSchema);
    expect((await createLink(spaceId, { nodeId: ids.top })).status).toBe(201);
    let pages = 0;
    let cursor = "";
    const seen = new Set<string>();
    do {
      const page = await owned(`/v1/spaces/${spaceId}/links?cursor=${cursor}`, { method: "GET", token: creds.access!.token }, ShareLinkListSchema);
      for (const l of page.message!.links) seen.add(l.linkId);
      cursor = page.message!.nextCursor;
      pages++;
    } while (cursor);
    expect(seen.size).toBe(MAX_ACTIVE_LINKS + 2);
    expect(pages).toBe(11);
  });
});

describe("share links: guessing resistance (RFC 0005 sections 1 and 5)", () => {
  it("issues 128-bit ids in their one canonical spelling, all different", async () => {
    const ids = new Set<string>();
    for (let i = 0; i < 500; i++) {
      const id = await account().createShareLink({ spaceId: uuidv7(), nodeId: uuidv7(), createdBy: uuidv7() });
      expect(id).toMatch(LINK_ID);
      const bytes = fromBase64url(id)!;
      expect(bytes.length).toBe(16);
      expect(base64url(bytes)).toBe(id);
      ids.add(id);
    }
    expect(ids.size).toBe(500);
  }, 30_000);

  it("answers malformed ids without any Durable Object call, and unknown, revoked and expired links identically", async () => {
    const { spaceId, ids, creds } = await newSpace();
    const live = await link(spaceId, { nodeId: ids.top });
    const unreachable = { ...env, ACCOUNT: { idFromName: () => { throw new Error("a malformed id reached the Account DO"); } } } as unknown as Env;
    const malformed = [live.linkId.slice(0, 21) + "B", live.linkId + "A", live.linkId.slice(1), "", "..%2F..", live.linkId.replace(/.$/, "=")];
    for (const id of malformed) {
      const response = await worker.fetch(new Request(`${BASE}/v1/share/${id}`, { headers: { Accept: "application/json" } }) as Parameters<typeof worker.fetch>[0], unreachable, { waitUntil() {} } as unknown as ExecutionContext);
      expect(response.status).toBe(404);
    }

    const revoked = await link(spaceId, { nodeId: ids.top });
    await owned(`/v1/spaces/${spaceId}/links/${revoked.linkId}`, { method: "DELETE", token: creds.access!.token }, ShareLinkSchema);
    const expired = await link(spaceId, { nodeId: ids.top, expiresAtMs: BigInt(Date.now() + 2 * MINUTE) });
    await setAccountClock(Date.now() + 3 * MINUTE);
    const ip = newIp();
    const replies = [];
    for (const id of [base64url(crypto.getRandomValues(new Uint8Array(16))), revoked.linkId, expired.linkId, live.linkId.slice(0, 21) + "B"]) {
      const reply = await pub(`/v1/share/${id}`, { ip });
      replies.push({ status: reply.status, text: reply.text });
    }
    expect(new Set(replies.map((r) => JSON.stringify(r))).size).toBe(1);
    expect(replies[0]!.status).toBe(404);
    expect(JSON.parse(replies[0]!.text)).toMatchObject({ code: "CODE_NOT_FOUND" });
  });

  it("blocks a client after 30 failed lookups in an hour, and only that client", async () => {
    const { spaceId, ids } = await newSpace();
    const live = await link(spaceId, { nodeId: ids.top });
    const guesser = newIp();
    const hourStart = Math.ceil(Date.now() / 3_600_000) * 3_600_000;
    await setAccountClock(hourStart + 1000);
    for (let i = 0; i < MISSES_PER_HOUR; i++) {
      expect((await pub(`/v1/share/${base64url(crypto.getRandomValues(new Uint8Array(16)))}`, { ip: guesser })).status).toBe(404);
    }
    const blocked = await pub(`/v1/share/${live.linkId}`, { ip: guesser });
    expect(blocked.status).toBe(429);
    expect(blocked.json).toMatchObject({ code: "CODE_RETRY_LATER" });
    expect(Number(blocked.json!.retryAfterMs)).toBe(3_600_000 - 1000);
    expect(blocked.headers.get("Retry-After")).toBe(String(3600 - 1));
    expect((await pub(`/v1/share/${live.linkId}`, { ip: newIp() })).status).toBe(200);
    await setAccountClock(hourStart + 3_600_000);
    expect((await pub(`/v1/share/${live.linkId}`, { ip: guesser })).status).toBe(200);
  });

  it("limits a client to 300 requests a minute", async () => {
    const { spaceId, ids } = await newSpace();
    const live = await link(spaceId, { nodeId: ids.docs });
    const ip = newIp();
    const minuteStart = Math.ceil(Date.now() / MINUTE) * MINUTE;
    await setAccountClock(minuteStart);
    for (let i = 0; i < REQUESTS_PER_MINUTE; i++) expect((await pub(`/v1/share/${live.linkId}/list`, { ip })).status).toBe(200);
    expect((await pub(`/v1/share/${live.linkId}/list`, { ip })).status).toBe(429);
    await setAccountClock(minuteStart + MINUTE);
    expect((await pub(`/v1/share/${live.linkId}/list`, { ip })).status).toBe(200);
  });

  it("keys IPv6 clients by their /64 prefix", () => {
    const key = (ip: string) => clientKey(new Request(BASE, { headers: { "CF-Connecting-IP": ip } }));
    expect(key("2001:db8:1:2:3:4:5:6")).toBe("2001:db8:1:2::/64");
    expect(key("2001:0db8:0001:0002::9")).toBe("2001:db8:1:2::/64");
    expect(key("2001:db8::1")).toBe("2001:db8:0:0::/64");
    expect(key("::ffff:192.0.2.1")).toBe("0:0:0:0::/64");
    expect(key("192.0.2.7")).toBe("192.0.2.7");
  });
});

describe("share links: passwords, sessions and lockout (RFC 0005 section 4)", () => {
  it("reveals nothing before the password, locks for 15 minutes after five failures, and opens a one-hour session", async () => {
    const { spaceId, ids } = await newSpace();
    const shared = await link(spaceId, { nodeId: ids.docs, password: "correct horse" });
    const ip = newIp();
    const path = `/v1/share/${shared.linkId}`;

    const info = await pub(path, { ip });
    expect(info.status).toBe(401);
    expect(info.json).toEqual({ passwordRequired: true });
    expect((await pub(path + "/list", { ip })).status).toBe(401);
    expect((await pub(`${path}/file/${ids.a}`, { ip })).status).toBe(401);

    const t0 = Date.now();
    await setAccountClock(t0);
    for (let i = 0; i < 5; i++) expect((await pub(path + "/unlock", { ip, json: { password: "wrong" } })).status).toBe(401);
    const locked = await pub(path + "/unlock", { ip, json: { password: "correct horse" } });
    expect(locked.status).toBe(429);
    expect(Number(locked.json!.retryAfterMs)).toBe(SHARE_LOCK_MS);
    await setAccountClock(t0 + SHARE_LOCK_MS - 1);
    expect((await pub(path + "/unlock", { ip, json: { password: "correct horse" } })).status).toBe(429);
    await setAccountClock(t0 + SHARE_LOCK_MS);
    const opened = await pub(path + "/unlock", { ip, json: { password: "correct horse" } });
    expect(opened.status).toBe(204);
    const setCookie = opened.headers.get("Set-Cookie")!;
    expect(setCookie).toMatch(new RegExp(`^fb_share=v1\\.\\d+\\.[A-Za-z0-9_-]+; Path=/v1/share/${shared.linkId}; Max-Age=3600; HttpOnly; Secure; SameSite=Strict$`));
    const cookie = cookieOf(opened);

    const unlocked = await pub(path, { ip, cookie });
    expect(unlocked.status).toBe(200);
    expect(unlocked.json).toMatchObject({ kind: "NODE_KIND_DIR", name: "Docs", rootId: ids.docs });
    expect((await pub(`${path}/file/${ids.a}`, { ip, cookie })).text).toBe("alpha");

    const [prefix, mac] = [cookie.slice(0, cookie.lastIndexOf(".")), cookie.slice(cookie.lastIndexOf(".") + 1)];
    expect((await pub(path, { ip, cookie: prefix + "." + (mac.startsWith("A") ? "B" : "A") + mac.slice(1) })).status).toBe(401);
    expect((await pub(path, { ip, cookie: cookie.replace(/v1\.(\d+)/, (_m, ms) => `v1.${Number(ms) + 1}`) })).status).toBe(401);

    await setAccountClock(t0 + SHARE_LOCK_MS + SESSION_MS);
    expect((await pub(path, { ip, cookie })).status).toBe(401);
  });

  it("binds a session to one link", async () => {
    const { spaceId, ids } = await newSpace();
    const first = await link(spaceId, { nodeId: ids.docs, password: "same password" });
    const second = await link(spaceId, { nodeId: ids.docs, password: "same password" });
    const ip = newIp();
    const opened = await pub(`/v1/share/${first.linkId}/unlock`, { ip, json: { password: "same password" } });
    expect(opened.status).toBe(204);
    expect((await pub(`/v1/share/${first.linkId}`, { ip, cookie: cookieOf(opened) })).status).toBe(200);
    expect((await pub(`/v1/share/${second.linkId}`, { ip, cookie: cookieOf(opened) })).status).toBe(401);
    const plain = await link(spaceId, { nodeId: ids.top });
    const unlockPlain = await pub(`/v1/share/${plain.linkId}/unlock`, { ip, json: { password: "" } });
    expect(unlockPlain.status).toBe(204);
    expect(unlockPlain.headers.get("Set-Cookie")).toBeNull();
  });
});

describe("share links: expiry and revocation (RFC 0005 section 9)", () => {
  it("stops working at its expiry time", async () => {
    const { spaceId, ids } = await newSpace();
    const expiresAtMs = Date.now() + 10 * MINUTE;
    const shared = await link(spaceId, { nodeId: ids.top, expiresAtMs: BigInt(expiresAtMs) });
    const ip = newIp();
    await setAccountClock(expiresAtMs - 1);
    const info = await pub(`/v1/share/${shared.linkId}`, { ip });
    expect(info.status).toBe(200);
    expect(info.json).toMatchObject({ kind: "NODE_KIND_FILE", name: "top.txt", size: "3", expiresAtMs: String(expiresAtMs) });
    await setAccountClock(expiresAtMs);
    expect((await pub(`/v1/share/${shared.linkId}`, { ip })).status).toBe(404);
    expect((await pub(`/v1/share/${shared.linkId}/file/${ids.top}`, { ip })).status).toBe(404);
  });

  it("applies a revocation to the next request, and stops a download in progress before its next chunk", async () => {
    const { spaceId, ids, device, creds } = await newSpace();
    const big = await addFile(device, ids.docs, "big.bin", "one-", "two-", "three-", "four-", "five");
    const shared = await link(spaceId, { nodeId: ids.docs });
    const ip = newIp();
    expect((await pub(`/v1/share/${shared.linkId}/list`, { ip })).status).toBe(200);

    const response = await exports.default.fetch(`${BASE}/v1/share/${shared.linkId}/file/${big}`, { headers: { "CF-Connecting-IP": ip } });
    expect(response.status).toBe(200);
    expect(response.headers.get("Content-Length")).toBe("23");
    const reader = response.body!.getReader();
    const first = await reader.read();
    expect(decoder.decode(first.value)).toBe("one-");

    expect((await owned(`/v1/spaces/${spaceId}/links/${shared.linkId}`, { method: "DELETE", token: creds.access!.token }, ShareLinkSchema)).status).toBe(204);
    expect((await pub(`/v1/share/${shared.linkId}/list`, { ip })).status).toBe(404);

    const received = [...first.value!];
    let failed = false;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        received.push(...value);
      }
    } catch {
      failed = true;
    }
    expect(failed).toBe(true);
    expect(received.length).toBeLessThan(23);
    expect("one-two-three-four-five".startsWith(decoder.decode(new Uint8Array(received)))).toBe(true);
  });
});

describe("share links: what a link reaches (RFC 0005 section 6)", () => {
  it("never lets a folder link reach outside its subtree, and follows moves and the trash", async () => {
    const { spaceId, ids, device } = await newSpace();
    const shared = await link(spaceId, { nodeId: ids.docs });
    const ip = newIp();
    const path = `/v1/share/${shared.linkId}`;
    const list = (dir = "") => pub(`${path}/list${dir ? "?dir=" + encodeURIComponent(dir) : ""}`, { ip });
    const file = (nodeId: string) => pub(`${path}/file/${nodeId}`, { ip });
    const names = (reply: PublicReply) => (reply.json!.entries as { name: string }[]).map((e) => e.name);

    const root = await list();
    expect(root.status).toBe(200);
    expect(names(root)).toEqual(["a.txt", "Sub"]);
    expect((root.json!.path as { name: string }[]).map((e) => e.name)).toEqual(["Docs"]);
    const sub = await list(ids.sub);
    expect(names(sub)).toEqual(["b.txt"]);
    expect((sub.json!.path as { name: string }[]).map((e) => e.name)).toEqual(["Docs", "Sub"]);
    expect((await file(ids.b)).text).toBe("bravo");

    for (const outside of [spaceId, ids.other, uuidv7(), "../", "..%2F..%2F"]) expect((await list(outside)).status).toBe(404);
    for (const outside of [ids.c, ids.top, spaceId, ids.other, uuidv7()]) expect((await file(outside)).status).toBe(404);
    for (const traversal of ["..", "..%2F" + ids.top, encodeURIComponent(`spaces/${spaceId}/chunks`)]) {
      expect((await pub(`${path}/file/${traversal}`, { ip })).status).toBe(404);
    }
    expect((await list(ids.a)).status).toBe(400);

    await device.commit([op.move(ids.b, ids.other, "b.txt"), op.move(ids.c, ids.sub, "c.txt")]);
    expect((await file(ids.b)).status).toBe(404);
    expect((await file(ids.c)).text).toBe("charlie");

    await device.commit([op.delete(ids.sub)]);
    expect((await list(ids.sub)).status).toBe(404);
    expect((await file(ids.c)).status).toBe(404);
    await device.commit([op.restore(ids.sub)]);
    expect((await file(ids.c)).text).toBe("charlie");

    await device.commit([op.move(ids.docs, ids.other, "Renamed docs")]);
    expect((await pub(path, { ip })).json).toMatchObject({ name: "Renamed docs", rootId: ids.docs });
    expect((await file(ids.a)).text).toBe("alpha");

    await device.commit([op.delete(ids.docs)]);
    expect((await pub(path, { ip })).status).toBe(404);
    expect((await file(ids.a)).status).toBe(404);
  });

  it("gives a file link its own file and nothing else", async () => {
    const { spaceId, ids } = await newSpace();
    const shared = await link(spaceId, { nodeId: ids.a });
    const ip = newIp();
    expect((await pub(`/v1/share/${shared.linkId}/file/${ids.a}`, { ip })).text).toBe("alpha");
    expect((await pub(`/v1/share/${shared.linkId}/file/${ids.b}`, { ip })).status).toBe(404);
    expect((await pub(`/v1/share/${shared.linkId}/list`, { ip })).status).toBe(400);
  });

  it("pages a large folder by name", async () => {
    const { spaceId, device } = await newSpace();
    const dirId = uuidv7();
    const ops = [op.mkdir(dirId, spaceId, "Many")];
    for (let i = 0; i < 600; i++) ops.push(op.mkdir(uuidv7(), dirId, `folder ${String(i).padStart(3, "0")}`));
    await device.commit(ops);
    const shared = await link(spaceId, { nodeId: dirId });
    const ip = newIp();
    const first = await pub(`/v1/share/${shared.linkId}/list`, { ip });
    expect((first.json!.entries as unknown[]).length).toBe(500);
    const second = await pub(`/v1/share/${shared.linkId}/list?cursor=${first.json!.nextCursor as string}`, { ip });
    const names = [...(first.json!.entries as { name: string }[]), ...(second.json!.entries as { name: string }[])].map((e) => e.name);
    expect(names).toEqual(Array.from({ length: 600 }, (_v, i) => `folder ${String(i).padStart(3, "0")}`));
    expect(second.json!.nextCursor).toBeUndefined();
    expect((await pub(`/v1/share/${shared.linkId}/list?cursor=bad!`, { ip })).status).toBe(400);
  });
});

async function keyOf(spaceId: string, bytes: string): Promise<string> {
  const hash = (await content(bytes)).content.chunks[0]!.hash;
  const incarnation = await runInDurableObject(space(spaceId), (_instance, state) =>
    String(state.storage.sql.exec("SELECT incarnation FROM chunks WHERE hash = ?", hash).one().incarnation),
  );
  return chunkKey(spaceId, toHex(hash), incarnation);
}

function withoutR2(path: string): Promise<Response> {
  const BUCKET = { get: () => Promise.reject(new Error("this request must not read R2")) };
  return worker.fetch(
    new Request(BASE + path, { headers: { "CF-Connecting-IP": newIp() } }) as Parameters<typeof worker.fetch>[0],
    { ...env, BUCKET } as unknown as Env,
    { waitUntil() {} } as unknown as ExecutionContext,
  );
}

describe("share links: downloads (RFC 0005 section 7)", () => {
  it("serves the current version as an attachment that a browser cannot render", async () => {
    const { spaceId, ids, device } = await newSpace();
    const named = await addFile(device, ids.docs, 'Résumé "final" 2026.html', "<script>", "alert(1)</script>");
    const shared = await link(spaceId, { nodeId: ids.docs });
    const response = await exports.default.fetch(`${BASE}/v1/share/${shared.linkId}/file/${named}`, { headers: { "CF-Connecting-IP": newIp() } });
    expect(response.status).toBe(200);
    expect(response.headers.get("Content-Type")).toBe("application/octet-stream");
    expect(response.headers.get("Content-Length")).toBe("25");
    expect(response.headers.get("Content-Disposition")).toBe(`attachment; filename="R_sum_ _final_ 2026.html"; filename*=UTF-8''R%C3%A9sum%C3%A9%20%22final%22%202026.html`);
    expect(response.headers.get("Content-Security-Policy")).toBe("default-src 'none'; sandbox");
    expect(response.headers.get("X-Content-Type-Options")).toBe("nosniff");
    expect(response.headers.get("Cache-Control")).toBe("no-store");
    expect(await response.text()).toBe("<script>alert(1)</script>");
  });

  it("serves an empty file without reading R2, through a file link and through a folder link", async () => {
    const { spaceId, ids, device } = await newSpace();
    const empty = await addFile(device, ids.docs, "empty.txt");
    const direct = await link(spaceId, { nodeId: empty });
    const folder = await link(spaceId, { nodeId: ids.docs });
    for (const shared of [direct, folder]) {
      const response = await withoutR2(`/v1/share/${shared.linkId}/file/${empty}`);
      expect(response.status).toBe(200);
      expect(response.headers.get("Content-Length")).toBe("0");
      expect(response.headers.get("Content-Disposition")).toBe(`attachment; filename="empty.txt"; filename*=UTF-8''empty.txt`);
      expect(await response.text()).toBe("");
    }
    expect((await pub(`/v1/share/${direct.linkId}`, { ip: newIp() })).json).toMatchObject({ kind: "NODE_KIND_FILE", name: "empty.txt" });
    expect((await withoutR2(`/v1/share/${direct.linkId}/file/${ids.a}`)).status).toBe(404);
  });

  it("refuses a file whose first chunk is missing, has the wrong size or another stored checksum, before sending any of its bytes", async () => {
    const { spaceId, ids } = await newSpace();
    const shared = await link(spaceId, { nodeId: ids.docs });
    const key = await keyOf(spaceId, "alpha");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const get = () => pub(`/v1/share/${shared.linkId}/file/${ids.a}`, { ip: newIp() });

    await env.BUCKET.delete(key);
    const missing = await get();
    expect(missing.status).toBe(502);
    expect(missing.headers.get("Content-Type")).toBe("text/plain; charset=utf-8");
    await env.BUCKET.put(key, "alpha!");
    expect((await get()).status).toBe(502);
    const other = new TextEncoder().encode("alphA");
    await env.BUCKET.put(key, other, { sha256: toHex(new Uint8Array(await crypto.subtle.digest("SHA-256", other))) });
    const swapped = await get();
    expect(swapped.status).toBe(502);
    expect(swapped.text).not.toContain("alph");
    expect(error.mock.calls.map((args) => String(args[1]))).toEqual([
      expect.stringContaining("is missing"),
      expect.stringContaining("has 6 bytes, expected 5"),
      expect.stringContaining("has a stored SHA-256 other than its hash"),
    ]);

    const alpha = new TextEncoder().encode("alpha");
    await env.BUCKET.put(key, alpha, { sha256: toHex(new Uint8Array(await crypto.subtle.digest("SHA-256", alpha))) });
    expect((await get()).text).toBe("alpha");
  });

  it("aborts a download at a bad later chunk without sending any byte of it", async () => {
    const { spaceId, ids, device } = await newSpace();
    const nodeId = await addFile(device, ids.docs, "parts.bin", "first-", "second-", "third");
    const shared = await link(spaceId, { nodeId: ids.docs });
    await env.BUCKET.put(await keyOf(spaceId, "second-"), "SECOND-LONGER");
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const response = await exports.default.fetch(`${BASE}/v1/share/${shared.linkId}/file/${nodeId}`, { headers: { "CF-Connecting-IP": newIp() } });
    expect(response.status).toBe(200);
    const { bytes, error: failure } = await readAll(response);
    expect(failure).toBeDefined();
    expect(decoder.decode(new Uint8Array(bytes))).toBe("first-");
    expect(error.mock.calls.map((args) => String(args[1]))).toEqual([expect.stringContaining("has 13 bytes, expected 7")]);
  });

  it("refuses a file with more chunks than one download covers", async () => {
    const { spaceId, ids } = await newSpace();
    const shared = await link(spaceId, { nodeId: ids.docs });
    await runInDurableObject(space(spaceId), (_instance, state) => {
      const node = state.storage.sql.exec("SELECT cur_version_id FROM nodes WHERE node_id = ?", ids.a).one();
      state.storage.sql.exec("UPDATE versions SET chunk_count = 4097 WHERE version_id = ?", node.cur_version_id);
    });
    const reply = await pub(`/v1/share/${shared.linkId}/file/${ids.a}`, { ip: newIp() });
    expect(reply.status).toBe(413);
    expect(reply.headers.get("Content-Type")).toBe("text/plain; charset=utf-8");
  });
});

async function holdClosed(spaceId: string): Promise<() => Promise<void>> {
  const rewindId = uuidv7();
  await runInDurableObject(account(), (_instance, state) => {
    state.storage.sql.exec(
      `INSERT INTO rewinds (rewind_id, space_id, restore_point_ms, expected_history_id, bookmark, history_id, rewind_number, attempt, phase, admitted_at_ms)
       VALUES (?, ?, 0, '', '', ?, 1, 1, 'arming', 0)`,
      rewindId,
      spaceId,
      uuidv7(),
    );
  });
  return () =>
    runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE rewinds SET phase = 'failed' WHERE rewind_id = ?", rewindId);
    });
}

async function startDownload(path: string) {
  const response = await exports.default.fetch(BASE + path, { headers: { "CF-Connecting-IP": newIp() } });
  expect(response.status).toBe(200);
  const reader = response.body!.getReader();
  const first = await reader.read();
  const received = [...first.value!];
  return {
    async rest(): Promise<{ text: string; failed: boolean }> {
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) return { text: decoder.decode(new Uint8Array(received)), failed: false };
          received.push(...value);
        }
      } catch {
        return { text: decoder.decode(new Uint8Array(received)), failed: true };
      }
    },
  };
}

describe("share links: rewinds (RFC 0005 sections 7 and 10)", () => {
  it("answers RETRY_LATER while a rewind holds the space closed, and stops a download that was running", async () => {
    const { spaceId, ids, device, creds } = await newSpace();
    const parts = await addFile(device, ids.docs, "parts.bin", "1111", "2222", "3333", "4444", "5555", "6666");
    const shared = await link(spaceId, { nodeId: ids.docs });
    const ip = newIp();
    const running = await startDownload(`/v1/share/${shared.linkId}/file/${parts}`);
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const reopen = await holdClosed(spaceId);
    try {
      for (const path of ["", "/list", `/file/${ids.a}`]) expect((await pub(`/v1/share/${shared.linkId}${path}`, { ip })).status).toBe(503);
      expect((await createLink(spaceId, { nodeId: ids.top })).status).toBe(503);
      const rest = await running.rest();
      expect(rest.failed).toBe(true);
      expect(rest.text.length).toBeLessThan(24);
      expect("111122223333444455556666".startsWith(rest.text)).toBe(true);
      expect(error.mock.calls.some((args) => String(args[1]).includes("temporarily unavailable"))).toBe(true);
      expect((await owned(`/v1/spaces/${spaceId}/links/${shared.linkId}`, { method: "DELETE", token: creds.access!.token }, ShareLinkSchema)).status).toBe(204);
    } finally {
      await reopen();
    }
    const after = await link(spaceId, { nodeId: ids.docs });
    expect((await pub(`/v1/share/${after.linkId}/list`, { ip })).status).toBe(200);
    expect((await pub(`/v1/share/${shared.linkId}/list`, { ip })).status).toBe(404);
  });

  it("stops a download when a rewind runs to completion between reads, and serves the restored state to new requests", async () => {
    const { spaceId, ids, device, creds } = await newSpace();
    const token = creds.access!.token;
    const createdAtMs = Number(
      await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT created_at_ms FROM spaces WHERE space_id = ?", spaceId).one().created_at_ms),
    );
    const parts = await addFile(device, ids.docs, "parts.bin", "1111", "2222", "3333", "4444", "5555", "6666");
    const restorePointMs = createdAtMs + MINUTE;
    await runInDurableObject(space(spaceId), (instance: SpaceDO, state) => {
      instance.clock = () => restorePointMs + 1;
      return simulatedPitr.snapshot(state, restorePointMs);
    });

    const updated = await content("AAAA", "BBBB", "CCCC", "DDDD", "EEEE", "FFFF");
    await device.commit([op.update(parts, updated.content)], { receipts: await device.uploadParts(updated) });
    const later = uuidv7();
    await device.commit([op.mkdir(later, spaceId, "Later")]);
    const fresh = await addFile(device, ids.docs, "fresh.txt", "new!");
    const docs = await link(spaceId, { nodeId: ids.docs });
    const laterLink = await link(spaceId, { nodeId: later });
    const stripped = await keyOf(spaceId, "3333");
    const [hashHex, incarnation] = stripped.split("/").slice(-2) as [string, string];
    expect((await account().retireChunks(spaceId, [{ hashHex, incarnation }])).ok).toBe(true);

    const running = await startDownload(`/v1/share/${docs.linkId}/file/${parts}`);
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await setAccountClock(createdAtMs + 5 * MINUTE);
    const request = body(RewindRequestSchema, { restorePointMs: BigInt(restorePointMs), expectedHistoryId: await device.history(), rewindId: uuidv7() });
    const rewound = await owned(`/v1/spaces/${spaceId}/rewind`, { token, body: request }, RewindResultSchema);
    expect(rewound.status).toBe(200);

    const rest = await running.rest();
    expect(rest.failed).toBe(true);
    expect(rest.text.length).toBeLessThan(24);
    expect("AAAABBBBCCCCDDDDEEEEFFFF".startsWith(rest.text)).toBe(true);
    expect(error.mock.calls.some((args) => String(args[1]).includes("the space was rewound during this download"))).toBe(true);

    const ip = newIp();
    expect((await pub(`/v1/share/${laterLink.linkId}`, { ip })).status).toBe(404);
    expect((await pub(`/v1/share/${docs.linkId}/file/${fresh}`, { ip })).status).toBe(404);
    expect((await pub(`/v1/share/${docs.linkId}/list`, { ip })).status).toBe(200);
    const unavailable = await pub(`/v1/share/${docs.linkId}/file/${parts}`, { ip });
    expect(unavailable.status).toBe(503);
    expect(unavailable.text).toContain("not stored right now");
    expect((await pub(`/v1/share/${docs.linkId}/file/${ids.a}`, { ip })).text).toBe("alpha");
    expect(Error_Code.NOT_FOUND).toBe(15);
  });
});
