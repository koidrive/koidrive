// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  DeviceListSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  SpaceInfoSchema,
  SpaceListSchema,
  TokenRequestSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { CreateShareLinkRequestSchema, ShareLinkListSchema, ShareLinkSchema } from "../src/gen/koidrive/v1/share_pb";

const BASE = "https://koidrive.test";

async function call<S extends DescMessage>(
  path: string,
  init: { method?: string; token?: string; body?: Uint8Array },
  schema: S,
): Promise<{ status: number; message?: MessageShape<S>; code?: Error_Code }> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (init.token) headers.Authorization = "Bearer " + init.token;
  const response = await exports.default.fetch(BASE + path, { method: init.method ?? "POST", headers, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (response.ok) return { status: response.status, message: fromBinary(schema, bytes) };
  const error = response.headers.get("Content-Type") === "application/x-protobuf";
  return { status: response.status, code: error ? fromBinary(ErrorSchema, bytes).code : undefined };
}

function body<S extends DescMessage>(schema: S, init: MessageInitShape<S>): Uint8Array {
  return toBinary(schema, create(schema, init));
}

async function claimCode(): Promise<string> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  log.mockRestore();
  return line!.split(": ")[1]!;
}

function refresh(device: DeviceCredentials) {
  return call("/v1/token", { body: body(TokenRequestSchema, { deviceId: device.deviceId, refreshCredential: device.refreshCredential }) }, AccessTokenSchema);
}

async function deviceNames(owner: DeviceCredentials): Promise<string[]> {
  const list = await call("/v1/devices", { method: "GET", token: owner.access!.token }, DeviceListSchema);
  return list.message!.devices.map((device) => device.name);
}

let owner: DeviceCredentials | undefined;

async function getOwner(): Promise<DeviceCredentials> {
  owner ??= (await call("/v1/claim", { body: body(ClaimRequestSchema, { code: await claimCode(), userName: "Owner", deviceName: "Laptop", platform: "macos" }) }, DeviceCredentialsSchema)).message!;
  return owner;
}

describe("RFC 0024 create retries", () => {
  it("answers a retried claim with the device the first claim made", async () => {
    const code = await claimCode();
    const claim = (requestId: string) =>
      call("/v1/claim", { body: body(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop", platform: "macos", requestId }) }, DeviceCredentialsSchema);
    const first = await claim("claim-1");
    const again = await claim("claim-1");
    expect(again.status).toBe(200);
    expect(again.message!.deviceId).toBe(first.message!.deviceId);
    expect(again.message!.userName).toBe("Owner");
    expect((await claim("claim-2")).status).toBe(403);
    expect((await refresh(first.message!)).status).toBe(401);
    expect((await refresh(again.message!)).status).toBe(200);
    expect((await claim("claim-1")).status).toBe(403);
    owner = again.message!;
    expect(await deviceNames(owner)).toEqual(["Laptop"]);
  });

  it("answers a retried pairing with the device the first pairing made", async () => {
    const owner = await getOwner();
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const pair = (requestId: string) =>
      call("/v1/pair/complete", { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Desk", platform: "linux", requestId }) }, DeviceCredentialsSchema);
    const first = await pair("pair-1");
    const again = await pair("pair-1");
    expect(again.status).toBe(200);
    expect(again.message!.deviceId).toBe(first.message!.deviceId);
    expect(again.message!.userId).toBe(owner.userId);
    expect((await pair("pair-2")).status).toBe(403);
    expect((await refresh(first.message!)).status).toBe(401);
    expect((await refresh(again.message!)).status).toBe(200);
    expect((await pair("pair-1")).status).toBe(403);
    expect(await deviceNames(owner)).toEqual(["Laptop", "Desk"]);
  });

  it("answers a retried pairing for 25 hours after the pairing, long after its code would have expired", async () => {
    const owner = await getOwner();
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const pair = (requestId: string) =>
      call("/v1/pair/complete", { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Tablet", platform: "linux", requestId }) }, DeviceCredentialsSchema);
    const passed = (ms: number) =>
      runInDurableObject(env.ACCOUNT.get(env.ACCOUNT.idFromName("account")), (_instance, state) => state.storage.sql.exec("UPDATE pairing_codes SET expires_at_ms = expires_at_ms - ?", ms));
    const first = await pair("late-1");
    await passed(24 * 3_600_000);
    const again = await pair("late-1");
    expect(again.status).toBe(200);
    expect(again.message!.deviceId).toBe(first.message!.deviceId);
    await passed(2 * 3_600_000);
    expect((await pair("late-1")).status).toBe(403);
  });

  it("answers a retried space create with the space the first request made", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const createSpace = (name: string, requestId: string) => call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name, requestId }) }, SpaceInfoSchema);
    const first = await createSpace("Photos", "space-1");
    const again = await createSpace("Photos", "space-1");
    expect(first.status).toBe(201);
    expect(again.status).toBe(201);
    expect(again.message!.spaceId).toBe(first.message!.spaceId);
    expect(await createSpace("Photos", "space-2")).toMatchObject({ status: 409, code: Error_Code.INVALID_ARGUMENT });
    expect(await createSpace("Pictures", "space-1")).toMatchObject({ status: 409, code: Error_Code.INVALID_ARGUMENT });

    const racing = await Promise.all([createSpace("Music", "space-3"), createSpace("Music", "space-3")]);
    expect(racing.map((answer) => answer.status)).toContain(201);
    const settled = await createSpace("Music", "space-3");
    expect(settled.status).toBe(201);
    expect(racing.filter((answer) => answer.status === 201).map((answer) => answer.message!.spaceId)).toEqual(
      racing.filter((answer) => answer.status === 201).map(() => settled.message!.spaceId),
    );

    const clashing = await Promise.all([createSpace("Video", "space-5"), createSpace("Films", "space-5")]);
    expect(clashing.map((answer) => answer.status)).toEqual([201, 409]);
    expect(clashing[1]!.code).toBe(Error_Code.INVALID_ARGUMENT);

    const names = (await call("/v1/spaces", { method: "GET", token }, SpaceListSchema)).message!.spaces.map((space) => space.name);
    expect(names).toEqual(["Photos", "Music", "Video"]);
  });

  it("answers a retried share link create with the link the first request made", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const space = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Shared", requestId: "space-4" }) }, SpaceInfoSchema);
    const spaceId = space.message!.spaceId;
    const share = (nodeId: string, requestId: string, init: { password?: string; noExpiry?: boolean; expiresAtMs?: bigint } = {}) =>
      call(`/v1/spaces/${spaceId}/links`, { token, body: body(CreateShareLinkRequestSchema, { nodeId, requestId, ...init }) }, ShareLinkSchema);
    const first = await share(spaceId, "link-1");
    const again = await share(spaceId, "link-1");
    expect(first.status).toBe(201);
    expect(again.status).toBe(201);
    expect(again.message!.url).toBe(first.message!.url);
    expect(await share(owner.deviceId, "link-1")).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });
    expect(await share(spaceId, "link-1", { password: "correct horse" })).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });
    expect(await share(spaceId, "link-1", { noExpiry: true })).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });

    const locked = await share(spaceId, "link-2", { password: "correct horse" });
    expect((await share(spaceId, "link-2", { password: "correct horse" })).message!.url).toBe(locked.message!.url);
    expect(await share(spaceId, "link-2", { password: "wrong horse!" })).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });
    expect(await share(spaceId, "link-2")).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });

    const expiresAtMs = BigInt(Date.now() + 86_400_000);
    const dated = await share(spaceId, "link-3", { expiresAtMs });
    expect((await share(spaceId, "link-3", { expiresAtMs })).message!.url).toBe(dated.message!.url);
    expect(await share(spaceId, "link-3", { expiresAtMs: expiresAtMs + 1000n })).toMatchObject({ status: 400, code: Error_Code.INVALID_ARGUMENT });
    const links = await call(`/v1/spaces/${spaceId}/links`, { method: "GET", token }, ShareLinkListSchema);
    expect(links.message!.links).toHaveLength(3);
  });

  it("answers a retried share link create with the link the first request made after it expired or was revoked", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const space = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Ended", requestId: "space-ended" }) }, SpaceInfoSchema);
    const spaceId = space.message!.spaceId;
    const share = (requestId: string) => call(`/v1/spaces/${spaceId}/links`, { token, body: body(CreateShareLinkRequestSchema, { nodeId: spaceId, requestId }) }, ShareLinkSchema);
    const account = env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
    const eightDaysMs = 8 * 86_400_000;

    const old = await share("link-old");
    await runInDurableObject(account, (_instance, state) =>
      state.storage.sql.exec("UPDATE share_links SET created_at_ms = created_at_ms - ?, expires_at_ms = expires_at_ms - ? WHERE request_id = ?", eightDaysMs, eightDaysMs, "link-old"),
    );
    const expired = await share("link-old");
    expect(expired.status).toBe(201);
    expect(expired.message!.url).toBe(old.message!.url);
    expect(Number(expired.message!.expiresAtMs)).toBeLessThan(Date.now());

    const first = await share("link-revoked");
    const revoke = await exports.default.fetch(`${BASE}/v1/spaces/${spaceId}/links/${first.message!.linkId}`, { method: "DELETE", headers: { Authorization: "Bearer " + token } });
    expect(revoke.status).toBe(204);
    const revoked = await share("link-revoked");
    expect(revoked.status).toBe(201);
    expect(revoked.message!.url).toBe(first.message!.url);
    expect(Number(revoked.message!.revokedAtMs)).toBeGreaterThan(0);
  });

  it("refuses a request_id longer than 64 characters", async () => {
    const owner = await getOwner();
    const answer = await call("/v1/spaces", { token: owner.access!.token, body: body(CreateSpaceRequestSchema, { name: "Long", requestId: "r".repeat(65) }) }, SpaceInfoSchema);
    expect(answer.code).toBe(Error_Code.INVALID_ARGUMENT);
  });
});
