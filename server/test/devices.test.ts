// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary, type MessageInitShape, toBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AccessTokenSchema,
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  DeviceInfo_Kind,
  DeviceListSchema,
  PairCompleteRequestSchema,
  PairStartResponseSchema,
  RewindRequestSchema,
  ServerInfoSchema,
  SpaceInfoSchema,
  type SpaceList,
  SpaceListSchema,
  TokenRequestSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { RpcRequestSchema, RpcResponseSchema } from "../src/gen/koidrive/v1/messages_pb";
import { Error_Cause, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { type Identity, signAccessToken, verifyAccessToken } from "../src/auth";
import { fromBase64url } from "../src/crypto";
import { ShareLinkListSchema } from "../src/gen/koidrive/v1/share_pb";
import { uuidv7, uuidv7Ms } from "../src/ids";
import { type Principal, policy } from "../src/policy";
import { Close, type SpaceDO } from "../src/space-do";
import { MIGRATIONS as SPACE_MIGRATIONS } from "../src/space/schema";
import { SESSION_LEASE_MS } from "../src/account/account";
import { op } from "./helpers";
import { BASE, account, call, body, send, getOwner, pairDevice, createSpace, passLeases, Socket } from "./session";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("owner claim, pairing and tokens", () => {
  it("claims the server once with the logged code, pairs a device and refreshes tokens", async () => {
    const before = await call("/v1/server", { method: "GET" }, ServerInfoSchema);
    expect(before.message!.claimed).toBe(false);
    const wrong = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(wrong.status).toBe(403);

    const owner = await getOwner();
    expect((await call("/v1/server", { method: "GET" }, ServerInfoSchema)).message!.claimed).toBe(true);
    expect((await exports.default.fetch(BASE + "/setup").then((r) => r.text())).includes("already has an owner")).toBe(true);
    const again = await call("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", deviceName: "x" }) }, DeviceCredentialsSchema);
    expect(again.status).toBe(403);

    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    expect(started.message!.code).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}$/);
    const pairBody = body(PairCompleteRequestSchema, { code: started.message!.code.toLowerCase(), deviceName: "Desktop B", platform: "linux" });
    const paired = await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema);
    expect(paired.message!.userId).toBe(owner.userId);
    expect(paired.message!.deviceId).not.toBe(owner.deviceId);
    expect((await call("/v1/pair/complete", { body: pairBody }, DeviceCredentialsSchema)).status).toBe(403);

    const refreshed = await call(
      "/v1/token",
      { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: paired.message!.refreshCredential }) },
      AccessTokenSchema,
    );
    expect(refreshed.message!.expiresAtMs).toBeGreaterThan(BigInt(Date.now()));
    const spaces = await call("/v1/spaces", { method: "GET", token: refreshed.message!.token }, SpaceListSchema);
    expect(spaces.status).toBe(200);

    const forged = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: paired.message!.deviceId, refreshCredential: "nope" }) }, AccessTokenSchema);
    expect(forged.status).toBe(401);
    expect((await call("/v1/spaces", { method: "GET" }, SpaceListSchema)).status).toBe(401);
    const token = owner.access!.token;
    const at = token.length - 10;
    const tampered = token.slice(0, at) + (token[at] === "A" ? "B" : "A") + token.slice(at + 1);
    expect((await call("/v1/spaces", { method: "GET", token: tampered }, SpaceListSchema)).status).toBe(401);
  });

  it("stops accepting a revoked device", async () => {
    const owner = await getOwner();
    const laptop = await pairDevice(owner, "Lost laptop");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, laptop);
    await socket.start();
    await account().revokeDevice(laptop.deviceId);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const refresh = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: laptop.deviceId, refreshCredential: laptop.refreshCredential }) }, AccessTokenSchema);
    expect(refresh.status).toBe(401);
  });

  it("refuses a revoked device's old access token on every path, also after the Space DO was evicted", async () => {
    const owner = await getOwner();
    const laptop = await pairDevice(owner, "Stolen laptop");
    const spaceId = await createSpace(owner);
    await account().revokeDevice(laptop.deviceId);
    expect(await Socket.open(spaceId, laptop.access!.token)).toBe(401);
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: laptop.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    expect(rpc.status).toBe(401);
    await evictDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)));
    expect(await Socket.open(spaceId, laptop.access!.token)).toBe(401);
  });

  it("keeps a revocation in the Space DO, so a missed Account DO check cannot reopen the space", async () => {
    const owner = await getOwner();
    const tablet = await pairDevice(owner, "Tablet");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await stub.closeDevice(tablet.deviceId);
    expect(await account().isDeviceActive(tablet.deviceId)).toBe(true);
    expect(await Socket.open(spaceId, tablet.access!.token)).toBe(401);
    const rpc = await call("/v1/spaces/" + spaceId + "/rpc", { token: tablet.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    const result = rpc.message!.result;
    expect(result.case === "error" && result.value.code).toBe(Error_Code.UNAUTHENTICATED);
    const stored = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray());
    expect(stored).toEqual([{ device_id: tablet.deviceId }]);
  });

  it("closes a connection whose Reauth comes from a device revoked without a notice to the space", async () => {
    const owner = await getOwner();
    const phone = await pairDevice(owner, "Phone");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, phone);
    await socket.start();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), phone.deviceId);
    });
    await passLeases(spaceId);
    socket.send({ case: "reauth", value: { accessToken: phone.access!.token } });
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
  });

  it("refuses every socket message, a Commit included, once the Account DO revoked the device without a notice to the space", async () => {
    const owner = await getOwner();
    const watch = await pairDevice(owner, "Watch");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const live = await Socket.connect(spaceId, watch);
    await live.start();
    const fresh = await Socket.connect(spaceId, watch);
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), watch.deviceId);
    });
    expect(await account().isDeviceActive(watch.deviceId)).toBe(false);
    await passLeases(spaceId);

    fresh.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    expect(await fresh.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    live.send({ case: "rpc", value: { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "after revocation")] } } } });
    expect(await live.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    expect((await live.waitClosed()).code).toBe(Close.DEVICE_REVOKED);

    const stored = await runInDurableObject(stub, (_instance, state) => ({
      commits: state.storage.sql.exec("SELECT commit_id FROM commits").toArray(),
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(stored).toEqual({ commits: [], revoked: [{ device_id: watch.deviceId }] });
  });

  it("refuses a revoked device in the Space DO itself after eviction, without the Worker's check in front", async () => {
    const owner = await getOwner();
    const tv = await pairDevice(owner, "TV");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await stub.closeDevice(tv.deviceId);
    await evictDurableObject(stub);
    const identity = { userId: tv.userId, deviceId: tv.deviceId, deviceName: "TV", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const request = body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "x")] } } });
    const result = fromBinary(RpcResponseSchema, (await stub.httpRpc({ identity, origin: BASE }, request)).response).result;
    expect(result.case === "error" && result.value.code).toBe(Error_Code.UNAUTHENTICATED);
    const commits = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT seq FROM commits").toArray());
    expect(commits).toEqual([]);
  });

  it("closes only the asking socket with 1013 when the Account DO cannot answer, and keeps the space running", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Pad");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const asking = await Socket.connect(spaceId, pad);
    await asking.start();
    const other = await Socket.connect(spaceId, owner);
    await other.start();
    await passLeases(spaceId);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = prototype.checkSession;
    prototype.checkSession = async () => {
      throw new Error("injected Account DO failure");
    };
    try {
      asking.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      expect(await asking.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER } });
      expect((await asking.waitClosed()).code).toBe(Close.TRY_AGAIN_LATER);
    } finally {
      prototype.checkSession = original;
    }
    expect((await other.rpc({ case: "listTree", value: {} })).case).toBe("tree");
    expect(other.closed).toBeUndefined();
    const revoked = await runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray());
    expect(revoked).toEqual([]);
  });

  it("migrates a schema-one database that already holds revoked_devices", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE meta SET value = 1 WHERE key = 'schema_version'");
    });
    await evictDurableObject(stub);
    const device = uuidv7();
    await stub.closeDevice(device);
    const after = await runInDurableObject(stub, (_instance, state) => ({
      version: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'schema_version'").one().value,
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(after).toEqual({ version: SPACE_MIGRATIONS.length, revoked: [{ device_id: device }] });
  });

  it("adds revoked_devices to a version-one Space database and keeps its data", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const socket = await Socket.connect(spaceId, owner);
    await socket.start();
    const dir = uuidv7();
    await socket.commit(0n, [op.mkdir(dir, spaceId, "kept")]);
    socket.ws.close(1000);
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("DROP TABLE revoked_devices");
      state.storage.sql.exec("UPDATE meta SET value = 1 WHERE key = 'schema_version'");
    });
    await evictDurableObject(stub);
    const device = uuidv7();
    await stub.closeDevice(device);
    const after = await runInDurableObject(stub, (_instance, state) => ({
      version: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'schema_version'").one().value,
      head: state.storage.sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value,
      commits: state.storage.sql.exec("SELECT seq FROM commits").toArray(),
      node: state.storage.sql.exec("SELECT name FROM nodes WHERE node_id = ?", dir).toArray(),
      revoked: state.storage.sql.exec("SELECT device_id FROM revoked_devices").toArray(),
    }));
    expect(after).toEqual({ version: SPACE_MIGRATIONS.length, head: 1, commits: [{ seq: 1 }], node: [{ name: "kept" }], revoked: [{ device_id: device }] });
  });
});

describe("RFC 0006 principals, capabilities and device routes", () => {
  function refresh(creds: DeviceCredentials) {
    return call("/v1/token", { body: body(TokenRequestSchema, { deviceId: creds.deviceId, refreshCredential: creds.refreshCredential }) }, AccessTokenSchema);
  }

  function listDevices(token: string, cursor = "") {
    return call("/v1/devices" + (cursor ? "?cursor=" + cursor : ""), { method: "GET", token }, DeviceListSchema);
  }

  function updateDevice(deviceId: string, assignment: string, value: unknown) {
    return runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec(`UPDATE devices SET ${assignment} = ? WHERE device_id = ?`, value, deviceId);
    });
  }

  it("lists the user's principals in creation order with their issuer, and pages them by cursor", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Lister");
    expect(pad.credentialExpiresAtMs).toBe(0n);
    const listed = await listDevices(pad.access!.token);
    expect(listed.status).toBe(200);
    const byId = new Map(listed.message!.devices.map((device) => [device.deviceId, device]));
    expect(byId.get(owner.deviceId)).toMatchObject({ name: "Laptop A", platform: "macos", kind: DeviceInfo_Kind.DEVICE, issuedBy: "", current: false });
    expect(byId.get(pad.deviceId)).toMatchObject({
      name: "Lister",
      platform: "linux",
      kind: DeviceInfo_Kind.DEVICE,
      issuedBy: owner.deviceId,
      current: true,
      revokedAtMs: 0n,
      credentialExpiresAtMs: 0n,
    });
    expect(byId.get(pad.deviceId)!.createdAtMs).toBeGreaterThan(0n);
    const ids = listed.message!.devices.map((device) => device.deviceId);
    expect(ids.indexOf(owner.deviceId)).toBeLessThan(ids.indexOf(pad.deviceId));
    expect(ids.at(-1)).toBe(pad.deviceId);
    expect(listed.message!.nextCursor).toBe("");

    const extra = Array.from({ length: 150 }, () => uuidv7());
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, list_seq) VALUES (?, ?, 'Extra', 'linux', ?, ?, (SELECT MAX(list_seq) + 1 FROM devices))",
          id,
          owner.userId,
          Date.now(),
          new Uint8Array(32),
        );
      }
    });
    try {
      const pages: string[][] = [];
      let cursor = "";
      do {
        const page = await listDevices(pad.access!.token, cursor);
        pages.push(page.message!.devices.map((device) => device.deviceId));
        cursor = page.message!.nextCursor;
      } while (cursor);
      expect(pages[0]).toHaveLength(100);
      expect(pages.flat()).toEqual([...ids, ...extra]);
      expect((await send("/v1/devices?cursor=" + extra[0], { method: "GET", token: pad.access!.token })).status).toBe(400);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("finishes a listing that an AccountDO from before list_seq started, in device_id order", async () => {
    const owner = await getOwner();
    const extra = Array.from({ length: 150 }, () => uuidv7()).reverse();
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, list_seq) VALUES (?, ?, 'Extra', 'linux', ?, ?, (SELECT MAX(list_seq) + 1 FROM devices))",
          id,
          owner.userId,
          Date.now(),
          new Uint8Array(32),
        );
      }
    });
    try {
      const byDeviceId = await runInDurableObject(account(), (_instance, state) =>
        state.storage.sql.exec("SELECT device_id FROM devices WHERE user_id = ? ORDER BY device_id", owner.userId).toArray().map((row) => String(row.device_id)),
      );
      const olderFirstPage = byDeviceId.slice(0, 100);
      const continued: string[] = [];
      let cursor = olderFirstPage.at(-1)!;
      while (cursor) {
        const page = await account().listDevices({ userId: owner.userId, callerId: owner.deviceId, cursor });
        continued.push(...page.devices.map((device) => device.deviceId));
        cursor = page.nextCursor;
      }
      expect([...olderFirstPage, ...continued]).toEqual(byDeviceId);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("lists a device whose pairing was still storing it when a listing passed newer devices", async () => {
    const owner = await getOwner();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    let held = false;
    await runInDurableObject(account(), (instance) => {
      const target = instance as unknown as { accessToken: (...args: unknown[]) => Promise<unknown> };
      const accessToken = target.accessToken.bind(instance);
      vi.spyOn(target, "accessToken").mockImplementationOnce(async (...args) => {
        held = true;
        await gate;
        return accessToken(...args);
      });
    });
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const pending = call(
      "/v1/pair/complete",
      { body: body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Slow", platform: "linux" }) },
      DeviceCredentialsSchema,
    );
    await vi.waitFor(() => expect(held).toBe(true));
    const newer: string[] = [];
    try {
      for (let i = 0; i < 101; i++) newer.push((await pairDevice(owner, "Newer")).deviceId);
      const first = await listDevices(owner.access!.token);
      expect(first.message!.nextCursor).not.toBe("");
      release();
      const paired = await pending;
      expect(paired.status).toBe(200);
      newer.push(paired.message!.deviceId);
      const listed = first.message!.devices.map((device) => device.deviceId);
      let cursor = first.message!.nextCursor;
      while (cursor) {
        const page = await listDevices(owner.access!.token, cursor);
        listed.push(...page.message!.devices.map((device) => device.deviceId));
        cursor = page.message!.nextCursor;
      }
      expect(listed).toEqual(expect.arrayContaining(newer));
      expect(new Set(listed).size).toBe(listed.length);
    } finally {
      release();
      await pending;
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) state.storage.sql.exec("DELETE FROM devices WHERE device_id = ?", id);
      });
    }
  });

  it("revokes a principal of the caller's user: its sockets close with 4003, its credential stops working, and a repeat changes nothing", async () => {
    const owner = await getOwner();
    const tv = await pairDevice(owner, "Revoked TV");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, tv);
    await socket.start();
    expect(await send("/v1/devices/" + tv.deviceId, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 204, code: undefined });
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    expect((await refresh(tv)).status).toBe(401);
    const listed = await listDevices(owner.access!.token);
    expect(listed.message!.devices.find((device) => device.deviceId === tv.deviceId)!.revokedAtMs).toBeGreaterThan(0n);
    expect((await send("/v1/devices/" + tv.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);

    const stranger = uuidv7();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec(
        "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash) VALUES (?, ?, 'Stranger', 'linux', ?, ?)",
        stranger,
        uuidv7(),
        Date.now(),
        new Uint8Array(32),
      );
    });
    for (const id of [uuidv7(), stranger]) {
      expect(await send("/v1/devices/" + id, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 404, code: Error_Code.NOT_FOUND });
    }
    const untouched = await runInDurableObject(account(), (_instance, state) => state.storage.sql.exec("SELECT revoked_at_ms FROM devices WHERE device_id = ?", stranger).one());
    expect(untouched.revoked_at_ms).toBeNull();

    expect((await send("/v1/devices", { method: "POST", token: owner.access!.token })).status).toBe(405);
    expect((await send("/v1/devices", { method: "GET" })).status).toBe(401);
  });

  it("refuses a lost device's refresh credential, old access token, socket and pairing code once another device revoked it (RFC 0009 section 2)", async () => {
    const owner = await getOwner();
    const lost = await pairDevice(owner, "Lost laptop");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, lost);
    await socket.start();
    const pairing = await call("/v1/pair/start", { token: lost.access!.token }, PairStartResponseSchema);
    expect((await send("/v1/spaces", { method: "GET", token: lost.access!.token })).status).toBe(200);

    expect(await send("/v1/devices/" + lost.deviceId, { method: "DELETE", token: owner.access!.token })).toEqual({ status: 204, code: undefined });
    const unauthenticated = { status: 401, code: Error_Code.UNAUTHENTICATED };
    expect(await send("/v1/spaces", { method: "GET", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/devices", { method: "GET", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/pair/start", { token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/devices/" + owner.deviceId, { method: "DELETE", token: lost.access!.token })).toEqual(unauthenticated);
    expect(await send("/v1/token", { body: body(TokenRequestSchema, { deviceId: lost.deviceId, refreshCredential: lost.refreshCredential }) })).toEqual(unauthenticated);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const invitee = body(PairCompleteRequestSchema, { code: pairing.message!.code, deviceName: "Thief's laptop", platform: "linux" });
    expect(await send("/v1/pair/complete", { body: invitee })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
    expect((await refresh(owner)).status).toBe(200);
  });

  it("lets a device revoke itself when it signs out, and refuses the last active device until it confirms (RFC 0009 section 5)", async () => {
    const owner = await getOwner();
    const spare = await pairDevice(owner, "Spare");
    expect(await send("/v1/devices/" + spare.deviceId, { method: "DELETE", token: spare.access!.token })).toEqual({ status: 204, code: undefined });
    expect((await refresh(spare)).status).toBe(401);

    const active = (await listDevices(owner.access!.token)).message!.devices.filter((device) => device.revokedAtMs === 0n && !device.current);
    for (const device of active) {
      expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
    }
    const listed = (await listDevices(owner.access!.token)).message!.devices;
    expect(listed.filter((device) => device.revokedAtMs === 0n).map((device) => device.deviceId)).toEqual([owner.deviceId]);

    for (const query of ["", "?confirm=yes"]) {
      const refused = await send("/v1/devices/" + owner.deviceId + query, { method: "DELETE", token: owner.access!.token });
      expect(refused).toEqual({ status: 409, code: Error_Code.CONFIRMATION_REQUIRED });
    }
    const after = (await listDevices(owner.access!.token)).message!.devices.find((device) => device.deviceId === owner.deviceId)!;
    expect(after.revokedAtMs).toBe(0n);
    expect((await refresh(owner)).status).toBe(200);
  });

  it("revokes the user's last active device once it confirms, and of two last devices signing out at once the second must confirm (RFC 0009 section 5)", async () => {
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("last-device"));
    await runInDurableObject(stub, async (instance: import("../src/account-do").AccountDO) => {
      const code = (await instance.issueClaimCode())!;
      const claimed = await instance.claim({ code, userName: "Owner", deviceName: "Only", platform: "macos" });
      if (!claimed.ok) throw new Error(claimed.message);
      const only = claimed.value;
      const pairing = await instance.startPairing(only.deviceId);
      if (!pairing.ok) throw new Error(pairing.message);
      const paired = await instance.completePairing({ code: pairing.value.code, deviceName: "Second", platform: "linux" }, "test-client");
      if (!paired.ok) throw new Error(paired.message);
      const second = paired.value;
      const revoke = (device: DeviceCredentials, confirmLastDevice: boolean) =>
        instance.revokeUserDevice({ userId: only.userId, callerId: device.deviceId, deviceId: device.deviceId, confirmLastDevice });

      const both = await Promise.all([revoke(only, false), revoke(second, false)]);
      expect(both.map((outcome) => (outcome.ok ? "revoked" : Error_Code[outcome.code])).sort()).toEqual(["CONFIRMATION_REQUIRED", "revoked"]);
      const last = (await instance.isDeviceActive(only.deviceId)) ? only : second;
      expect(await instance.isDeviceActive(last.deviceId)).toBe(true);

      expect(await revoke(last, true)).toMatchObject({ ok: true });
      expect(await instance.isDeviceActive(last.deviceId)).toBe(false);
      expect(await instance.refreshAccess({ deviceId: last.deviceId, refreshCredential: last.refreshCredential })).toMatchObject({
        ok: false,
        code: Error_Code.UNAUTHENTICATED,
      });
    });
  });

  it("lets only one of two devices that revoke each other at the same time succeed, so one stays active (RFC 0009 section 5)", async () => {
    const owner = await getOwner();
    const [first, second] = [await pairDevice(owner, "First"), await pairDevice(owner, "Second")];
    const replies = await Promise.all([
      send("/v1/devices/" + second.deviceId, { method: "DELETE", token: first.access!.token }),
      send("/v1/devices/" + first.deviceId, { method: "DELETE", token: second.access!.token }),
    ]);
    expect(replies.map((reply) => reply.status).sort()).toEqual([204, 401]);
    const listed = (await listDevices(owner.access!.token)).message!.devices;
    const active = listed.filter((device) => [first.deviceId, second.deviceId].includes(device.deviceId) && device.revokedAtMs === 0n);
    expect(active).toHaveLength(1);
  });

  it("denies everything to a principal whose kind or scope this server does not know", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    for (const [assignment, value] of [["kind", "agent"], ["scope", "photos"]] as const) {
      const pad = await pairDevice(owner, "Unknown " + assignment);
      const socket = await Socket.connect(spaceId, pad);
      await socket.start();
      await updateDevice(pad.deviceId, assignment, value);
      await passLeases(spaceId);
      expect((await refresh(pad)).status, assignment).toBe(401);
      expect(await send("/v1/spaces", { method: "GET", token: pad.access!.token })).toEqual({ status: 401, code: Error_Code.UNAUTHENTICATED });
      expect((await send("/v1/pair/start", { token: pad.access!.token })).status).toBe(401);
      expect(await Socket.open(spaceId, pad.access!.token)).toBe(401);
      socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
      expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    }
    const inviter = await pairDevice(owner, "Inviter");
    const started = await call("/v1/pair/start", { token: inviter.access!.token }, PairStartResponseSchema);
    await updateDevice(inviter.deviceId, "kind", "agent");
    const pairBody = body(PairCompleteRequestSchema, { code: started.message!.code, deviceName: "Invitee", platform: "linux" });
    expect(await send("/v1/pair/complete", { body: pairBody })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
  });

  async function commitAsOwner(owner: DeviceCredentials, spaceId: string, historyId: string) {
    const deviceSeq = BigInt(Number((await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
      state.storage.sql.exec("SELECT COALESCE(MAX(device_seq), 0) AS seq FROM device_commits WHERE device_id = ?", owner.deviceId).one().seq)))) + 1n;
    const commit = { commitId: uuidv7(), historyId, deviceSeq, baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "Pushed " + deviceSeq)] };
    const result = await call(`/v1/spaces/${spaceId}/rpc`, { token: owner.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: commit } }) }, RpcResponseSchema);
    expect(result.message!.result.case).toBe("commit");
  }

  it("closes a passive connection with 4003 instead of pushing to it once its principal is denied", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const watcher = await Socket.connect(spaceId, owner);
    const { historyId } = await watcher.start();
    for (const [column, value] of [["kind", "agent"], ["scope", "photos"], ["expires_at_ms", 1]] as const) {
      const pad = await pairDevice(owner, "Passive " + column);
      const socket = await Socket.connect(spaceId, pad);
      await socket.start();
      await updateDevice(pad.deviceId, column, value);
      await passLeases(spaceId);
      await commitAsOwner(owner, spaceId, historyId);
      expect(await socket.next(), column).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      expect((await watcher.pushed()).length).toBe(1);
    }
  });

  it("closes passive connections with 1013 when the Account DO cannot check them before a push", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Unchecked");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    const { historyId } = await socket.start();
    const listed = await call(`/v1/spaces/${spaceId}/rpc`, { token: owner.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
    expect(listed.message!.result.case).toBe("tree");
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = prototype.checkSessions;
    prototype.checkSessions = async () => {
      throw new Error("injected Account DO failure");
    };
    try {
      await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
        (instance as unknown as { leases: Map<string, unknown> }).leases.delete(pad.deviceId);
      });
      await commitAsOwner(owner, spaceId, historyId);
      expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.TRY_AGAIN_LATER } });
    } finally {
      prototype.checkSessions = original;
    }
  });

  it("answers a device check the Account DO could not make with RETRY_LATER and the failure's cause, over HTTP RPC, the WebSocket upgrade and a share link", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Overloaded check");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: import("../src/account-do").AccountDO) => Object.getPrototypeOf(instance) as import("../src/account-do").AccountDO);
    const original = { checkSessions: prototype.checkSessions, shareResolve: prototype.shareResolve };
    const overloaded = async () => {
      throw Object.assign(new Error("Durable Object is overloaded. Too many requests queued."), { overloaded: true });
    };
    prototype.checkSessions = overloaded;
    prototype.shareResolve = overloaded;
    try {
      await runInDurableObject(stub, (instance: SpaceDO) => {
        (instance as unknown as { leases: Map<string, unknown> }).leases.clear();
      });
      const rpc = await call(`/v1/spaces/${spaceId}/rpc`, { token: pad.access!.token, body: body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }) }, RpcResponseSchema);
      expect(rpc.message!.result).toMatchObject({ case: "error", value: { code: Error_Code.RETRY_LATER, cause: Error_Cause.DO_OVERLOADED } });
      const upgrade = await exports.default.fetch(BASE + "/v1/spaces/" + spaceId + "/ws", { headers: { Upgrade: "websocket", Authorization: "Bearer " + pad.access!.token } });
      const refused = fromBinary(ErrorSchema, new Uint8Array(await upgrade.arrayBuffer()));
      expect([upgrade.status, refused.code, refused.cause]).toEqual([503, Error_Code.RETRY_LATER, Error_Cause.DO_OVERLOADED]);
      expect(await stub.shareContinue({ linkId: "link", client: "client" }, { historyId: "history", generation: 1 })).toMatchObject({
        ok: false,
        code: Error_Code.RETRY_LATER,
        status: 503,
        cause: Error_Cause.DO_OVERLOADED,
      });
    } finally {
      Object.assign(prototype, original);
    }
  });

  it("lets a principal back in once its row is supported again, because only a revocation is remembered", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Temporarily unknown");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    await socket.start();
    await updateDevice(pad.deviceId, "kind", "agent");
    await passLeases(spaceId);
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    expect(await socket.next()).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
    await updateDevice(pad.deviceId, "kind", "device");
    await passLeases(spaceId);
    const reopened = await Socket.connect(spaceId, pad);
    await reopened.start();
    expect((await reopened.rpc({ case: "listTree", value: {} })).case).toBe("tree");
    const remembered = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
      state.storage.sql.exec("SELECT device_id FROM revoked_devices WHERE device_id = ?", pad.deviceId).toArray(),
    );
    expect(remembered).toEqual([]);
  });

  it("keeps a revocation notice that a space did not take, and delivers it on a repeat or from the alarm", async () => {
    const owner = await getOwner();
    const [repeated, alarmed] = [await pairDevice(owner, "Repeat"), await pairDevice(owner, "Alarm")];
    await createSpace(owner);
    vi.spyOn(console, "error").mockImplementation(() => {});
    await runInDurableObject(account(), async (instance: import("../src/account-do").AccountDO, state) => {
      const namespace = (instance as unknown as { env: Env }).env.SPACE;
      const delivered: string[] = [];
      let fail = true;
      const spy = vi.spyOn(namespace, "get").mockImplementation((() => ({
        metered: async (_method: string, [deviceId]: [string]) => {
          if (fail) throw new Error("injected close failure");
          delivered.push(deviceId);
          return { value: undefined };
        },
      })) as never);
      const pending = (deviceId: string) => state.storage.sql.exec("SELECT COUNT(*) AS n FROM revocation_notices WHERE device_id = ?", deviceId).one().n;
      instance.sessionLeaseMs = 0;
      try {
        for (const device of [repeated, alarmed]) {
          expect(await instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: device.deviceId, confirmLastDevice: false })).toMatchObject({ ok: true });
          expect(pending(device.deviceId)).toBeGreaterThan(0);
          expect((await instance.checkSession(device.deviceId)).revoked).toBe(true);
        }
        expect(await state.storage.getAlarm()).not.toBeNull();
        fail = false;
        await instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: repeated.deviceId, confirmLastDevice: false });
        expect(pending(repeated.deviceId)).toBe(0);
        expect(delivered).toContain(repeated.deviceId);
        state.storage.sql.exec("UPDATE revocation_notices SET next_try_ms = 0");
        await instance.alarm();
        expect(pending(alarmed.deviceId)).toBe(0);
        expect(delivered).toContain(alarmed.deviceId);
      } finally {
        spy.mockRestore();
        instance.sessionLeaseMs = SESSION_LEASE_MS;
      }
    });
  });

  it("arms the alarm when it stores a revocation, so a notice is delivered even if the object is evicted before delivering it", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Evicted before delivery");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, pad);
    await socket.start();
    const stub = account();
    await runInDurableObject(stub, async (instance: import("../src/account-do").AccountDO, state) => {
      await state.storage.deleteAlarm();
      vi.spyOn(instance as unknown as { deliverNotices: () => Promise<void> }, "deliverNotices").mockRejectedValue(new Error("evicted before delivery"));
      await expect(instance.revokeUserDevice({ userId: owner.userId, callerId: owner.deviceId, deviceId: pad.deviceId, confirmLastDevice: false })).rejects.toThrow("evicted before delivery");
      expect(await state.storage.getAlarm()).not.toBeNull();
      state.storage.sql.exec("UPDATE revocation_notices SET next_try_ms = 0 WHERE device_id = ?", pad.deviceId);
    });
    await evictDurableObject(stub);
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
    const pending = await runInDurableObject(stub, (_instance, state) =>
      state.storage.sql.exec("SELECT COUNT(*) AS n FROM revocation_notices WHERE device_id = ?", pad.deviceId).one().n,
    );
    expect(pending).toBe(0);
  });

  it("never lets an access token outlive its credential, and refuses an expired credential", async () => {
    const owner = await getOwner();
    const pad = await pairDevice(owner, "Expiring");
    const expiresAtMs = Date.now() + 60_000;
    await updateDevice(pad.deviceId, "expires_at_ms", expiresAtMs);
    const token = await refresh(pad);
    expect(token.message!.expiresAtMs).toBe(BigInt(expiresAtMs));
    const listed = await listDevices(owner.access!.token);
    expect(listed.message!.devices.find((device) => device.deviceId === pad.deviceId)!.credentialExpiresAtMs).toBe(BigInt(expiresAtMs));
    await updateDevice(pad.deviceId, "expires_at_ms", Date.now() - 1);
    expect(await send("/v1/token", { body: body(TokenRequestSchema, { deviceId: pad.deviceId, refreshCredential: pad.refreshCredential }) })).toEqual({
      status: 401,
      code: Error_Code.UNAUTHENTICATED,
    });
    expect((await send("/v1/spaces", { method: "GET", token: token.message!.token })).status).toBe(401);
  });

  it("carries the principal's kind and scope in the token, reads a token without them as a full-scope device, and decides from the row", async () => {
    const owner = await getOwner();
    const claims = JSON.parse(new TextDecoder().decode(fromBase64url(owner.access!.token.split(".")[2]!)!));
    expect(claims).toMatchObject({ k: "device", s: null });
    const key = await account().serverKey();
    const nowMs = Date.now();
    const identity = { userId: owner.userId, deviceId: owner.deviceId, deviceName: "Laptop A", expiresAtMs: nowMs + 60_000 };
    const legacy = await signAccessToken(key, identity as Identity);
    expect(JSON.parse(new TextDecoder().decode(fromBase64url(legacy.split(".")[2]!)!))).not.toHaveProperty("k");
    expect(await verifyAccessToken(key, legacy, nowMs)).toEqual({ ...identity, kind: "device", scope: null });

    const claimed = await signAccessToken(key, { ...identity, kind: "agent", scope: "photos" });
    const asked: Principal[] = [];
    vi.spyOn(policy, "can").mockImplementation((principal) => {
      asked.push(principal);
      return true;
    });
    expect((await send("/v1/spaces", { token: claimed, body: body(CreateSpaceRequestSchema, { name: "Row decides" }) })).status).toBe(201);
    expect(asked).toEqual([expect.objectContaining({ deviceId: owner.deviceId, kind: "device", scope: null })]);
  });

  it("checks each capability before its action and refuses it with FORBIDDEN", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const spaceId = await createSpace(owner);
    const cases = [
      ["space.create", () => send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Refused" }) })],
      ["device.pair", () => send("/v1/pair/start", { token })],
      ["device.manage", () => send("/v1/devices", { method: "GET", token })],
      ["device.manage", () => send("/v1/devices/" + owner.deviceId, { method: "DELETE", token })],
      ["share.manage", () => send(`/v1/spaces/${spaceId}/links`, { method: "GET", token })],
      ["space.rewind", () => send(`/v1/spaces/${spaceId}/rewind`, { token, body: body(RewindRequestSchema, { restorePointMs: 1n, rewindId: uuidv7() }) })],
    ] as const;
    for (const [capability, request] of cases) {
      const can = vi.spyOn(policy, "can").mockImplementation((_principal, asked) => asked !== capability);
      expect(await request(), capability).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
      can.mockRestore();
    }
    expect((await refresh(owner)).status).toBe(200);
    expect((await call(`/v1/spaces/${spaceId}/links`, { method: "GET", token }, ShareLinkListSchema)).status).toBe(200);
  });
});

describe("RFC 0007 name limits and space list pages", () => {
  const refused = { status: 400, code: Error_Code.INVALID_ARGUMENT };
  const longest = "가".repeat(85);
  const tooLong = longest + "a";

  function listSpaces(token: string, cursor = "") {
    return call("/v1/spaces" + (cursor ? "?cursor=" + cursor : ""), { method: "GET", token }, SpaceListSchema);
  }

  async function spacePages(token: string, from = ""): Promise<SpaceList[]> {
    const pages: SpaceList[] = [];
    let cursor = from;
    do {
      const page = await listSpaces(token, cursor);
      expect(page.status).toBe(200);
      pages.push(page.message!);
      cursor = page.message!.nextCursor;
    } while (cursor);
    return pages;
  }

  it("refuses a claim whose user name, device name or platform is longer than 255 bytes before it checks the code", async () => {
    await getOwner();
    const claim = (init: MessageInitShape<typeof ClaimRequestSchema>) => send("/v1/claim", { body: body(ClaimRequestSchema, { code: "AAAA-BBBB-CCCC", ...init }) });
    expect(await claim({ userName: tooLong })).toEqual(refused);
    expect(await claim({ deviceName: tooLong })).toEqual(refused);
    expect(await claim({ platform: tooLong })).toEqual(refused);
    expect(await claim({ userName: longest, deviceName: longest, platform: longest })).toEqual({ status: 403, code: Error_Code.FORBIDDEN });
  });

  it("refuses a pairing whose device name or platform is longer than 255 bytes, and keeps the pairing code valid", async () => {
    const owner = await getOwner();
    const started = await call("/v1/pair/start", { token: owner.access!.token }, PairStartResponseSchema);
    const complete = (deviceName: string, platform: string) =>
      body(PairCompleteRequestSchema, { code: started.message!.code, deviceName, platform });
    expect(await send("/v1/pair/complete", { body: complete(tooLong, "linux") })).toEqual(refused);
    expect(await send("/v1/pair/complete", { body: complete("Desk", tooLong) })).toEqual(refused);
    const paired = await call("/v1/pair/complete", { body: complete(longest, longest) }, DeviceCredentialsSchema);
    expect(paired.status).toBe(200);
    const listed = await call("/v1/devices", { method: "GET", token: paired.message!.access!.token }, DeviceListSchema);
    expect(listed.message!.devices.find((device) => device.current)).toMatchObject({ name: longest, platform: longest });
  });

  it("gives a paired device its user's name, and keeps its credentials within 4 KiB with the longest user and device names", async () => {
    const owner = await getOwner();
    expect(owner.userName).toBe("Owner");
    expect((await pairDevice(owner, "Phone")).userName).toBe("Owner");
    const setName = (name: string) =>
      runInDurableObject(account(), (_instance, state) => {
        state.storage.sql.exec("UPDATE users SET name = ? WHERE user_id = ?", name, owner.userId);
      });
    await setName("\u0001".repeat(255));
    try {
      const paired = await pairDevice(owner, "\u0001".repeat(255));
      expect(paired.userName).toBe("\u0001".repeat(255));
      expect(toBinary(DeviceCredentialsSchema, paired).length).toBeLessThanOrEqual(4096);
    } finally {
      await setName("Owner");
    }
  });

  it("keeps the credentials and the access token of a device with the longest name within 4 KiB", async () => {
    const owner = await getOwner();
    const paired = await pairDevice(owner, "\u0001".repeat(255));
    expect(toBinary(DeviceCredentialsSchema, paired).length).toBeLessThanOrEqual(4096);
    const refreshed = await call("/v1/token", { body: body(TokenRequestSchema, { deviceId: paired.deviceId, refreshCredential: paired.refreshCredential }) }, AccessTokenSchema);
    expect(refreshed.status).toBe(200);
    expect(toBinary(AccessTokenSchema, refreshed.message!).length).toBeLessThanOrEqual(4096);
  });

  it("refuses a space name longer than 255 bytes and creates one of exactly 255", async () => {
    const token = (await getOwner()).access!.token;
    expect(await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: tooLong }) })).toEqual(refused);
    const created = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: longest }) }, SpaceInfoSchema);
    expect(created.status).toBe(201);
    expect(created.message!.name).toBe(longest);
  });

  it("refuses a space name the owner already uses, compared exactly after trimming, when two creates race", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Shared ${uuidv7()}`;
    const createNamed = (text: string) => send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: text }) });
    const raced = await Promise.all([createNamed(name), createNamed(name)]);
    expect(raced.map((outcome) => outcome.status).sort()).toEqual([201, 409]);
    expect(await createNamed(` ${name}  `)).toEqual({ status: 409, code: Error_Code.INVALID_ARGUMENT });
    expect((await createNamed(name.toUpperCase())).status).toBe(201);
    const listed = (await spacePages(token)).flatMap((page) => page.spaces);
    expect(listed.filter((space) => space.name === name)).toHaveLength(1);
  });

  it("frees a space name again when the space behind it cannot be set up", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Broken ${uuidv7()}`;
    await runInDurableObject(account(), (instance) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      vi.spyOn(spaces, "get").mockImplementationOnce(
        () => ({ init: () => Promise.reject(new Error("the Space DO is unavailable")) }) as unknown as ReturnType<typeof spaces.get>,
      );
    });
    vi.spyOn(console, "error").mockImplementation(() => {});
    expect(await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) })).toEqual({ status: 500, code: Error_Code.INTERNAL });
    const listed = (await spacePages(token)).flatMap((page) => page.spaces);
    expect(listed.some((space) => space.name === name)).toBe(false);
    const created = await call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) }, SpaceInfoSchema);
    expect(created.status).toBe(201);
  });

  it("frees a space name when the Account DO aborts during the create, after its storage was flushed", async () => {
    const token = (await getOwner()).access!.token;
    const name = `Interrupted ${uuidv7()}`;
    await runInDurableObject(account(), (instance, state) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      vi.spyOn(spaces, "get").mockImplementationOnce(
        () =>
          ({
            init: async () => {
              await state.storage.sync();
              state.abort("interrupted while the Space DO was being set up");
            },
          }) as unknown as ReturnType<typeof spaces.get>,
      );
    });
    await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) }).catch(() => undefined);
    expect((await send("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name }) })).status).toBe(201);
  });

  it("lists a space whose create was still storing it when a listing passed newer spaces", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    const { promise: gate, resolve: release } = Promise.withResolvers<void>();
    const { promise: storing, resolve: entered } = Promise.withResolvers<string>();
    await runInDurableObject(account(), (instance) => {
      const spaces = (instance as unknown as { env: Env }).env.SPACE;
      const get = spaces.get.bind(spaces);
      vi.spyOn(spaces, "get").mockImplementationOnce((id) => {
        const stub = get(id);
        return {
          metered: (method: string, args: [Parameters<typeof stub.init>[0]]) => {
            entered(args[0].spaceId);
            return gate.then(() => stub.metered(method, args));
          },
        } as unknown as typeof stub;
      });
    });
    const pending = call("/v1/spaces", { token, body: body(CreateSpaceRequestSchema, { name: "Slow" }) }, SpaceInfoSchema);
    const newerMs = uuidv7Ms(await storing) + 1;
    const newer = Array.from({ length: 101 }, () => uuidv7(newerMs));
    try {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) {
          state.storage.sql.exec(
            "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version, list_seq) VALUES (?, ?, ?, ?, 1, (SELECT COALESCE(MAX(list_seq), 0) + 1 FROM spaces))",
            id,
            `Newer ${id}`,
            owner.userId,
            newerMs,
          );
        }
      });
      const first = await listSpaces(token);
      expect(first.message!.nextCursor).not.toBe("");
      expect(first.message!.spaces.some((space) => newer.includes(space.spaceId))).toBe(true);
      release();
      const created = await pending;
      expect(created.status).toBe(201);
      const later = await spacePages(token, first.message!.nextCursor);
      const listed = [...first.message!.spaces, ...later.flatMap((page) => page.spaces)].map((space) => space.spaceId);
      expect(listed).toContain(created.message!.spaceId);
      expect(listed).toEqual(expect.arrayContaining(newer));
      expect(new Set(listed).size).toBe(listed.length);
    } finally {
      release();
      await pending;
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of newer) state.storage.sql.exec("DELETE FROM spaces WHERE space_id = ?", id);
      });
    }
  });

  it("lists the user's spaces in creation order and pages them by cursor, each page within 32 KiB", async () => {
    const owner = await getOwner();
    const token = owner.access!.token;
    expect(await send("/v1/spaces?cursor=" + uuidv7(), { method: "GET", token })).toEqual(refused);
    const before = (await spacePages(token)).flatMap((page) => page.spaces.map((space) => space.spaceId));
    const extra = Array.from({ length: 150 }, () => uuidv7());
    await runInDurableObject(account(), (_instance, state) => {
      for (const id of extra) {
        state.storage.sql.exec(
          "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version, list_seq) VALUES (?, ?, ?, ?, 1, (SELECT MAX(list_seq) + 1 FROM spaces))",
          id,
          "\u0001".repeat(255),
          owner.userId,
          Date.now(),
        );
      }
    });
    try {
      const pages = await spacePages(token);
      expect(pages[0]!.spaces).toHaveLength(100);
      expect(pages.flatMap((page) => page.spaces.map((space) => space.spaceId))).toEqual([...before, ...extra]);
      for (const page of pages) expect(toBinary(SpaceListSchema, page).length).toBeLessThanOrEqual(32 * 1024);
    } finally {
      await runInDurableObject(account(), (_instance, state) => {
        for (const id of extra) state.storage.sql.exec("DELETE FROM spaces WHERE space_id = ?", id);
      });
    }
  });
});
