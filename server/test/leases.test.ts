// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type DeviceCredentials } from "../src/gen/koidrive/v1/auth_pb";
import { RpcRequestSchema, RpcResponseSchema } from "../src/gen/koidrive/v1/messages_pb";
import { Error_Code } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import { Close, type SpaceDO } from "../src/space-do";
import { type AccountDO } from "../src/account-do";
import { SESSION_LEASE_MS } from "../src/account/account";
import { freezeClock, op } from "./helpers";
import { BASE, account, body, send, getOwner, pairDevice, createSpace, passLeases, Socket } from "./session";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("RFC 0014 session leases", () => {
  async function withLease<T>(leaseMs: number, body: () => Promise<T>): Promise<T> {
    const set = (value: number) => runInDurableObject(account(), (instance: AccountDO) => {
      instance.sessionLeaseMs = value;
    });
    await set(leaseMs);
    try {
      return await body();
    } finally {
      await set(SESSION_LEASE_MS);
    }
  }

  async function loseNotices(spaceId: string): Promise<() => void> {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => Object.getPrototypeOf(instance) as SpaceDO);
    const original = prototype.closeDevice;
    prototype.closeDevice = async () => {
      throw new Error("injected lost revocation notice");
    };
    return () => {
      prototype.closeDevice = original;
    };
  }

  function revokeRowOnly(deviceId: string) {
    return runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ?", Date.now(), deviceId);
    });
  }

  function listTree(socket: Socket) {
    socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "listTree", value: {} } } });
    return socket.next();
  }

  function ageLeases(spaceId: string, byMs: number) {
    return runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      freezeClock(instance, instance.clock() + byMs);
    });
  }

  async function holdAccount(method: "checkSession" | "checkSessions", held_calls = Infinity) {
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype[method] as (this: AccountDO, ...args: unknown[]) => Promise<unknown>;
    const asked: unknown[] = [];
    const { promise: held, resolve: release } = Promise.withResolvers<void>();
    (prototype as unknown as Record<string, unknown>)[method] = async function (this: AccountDO, ...args: unknown[]) {
      const call = asked.push(args[0]);
      const answer = await original.apply(this, args);
      if (call <= held_calls) await held;
      return answer;
    };
    return {
      asked,
      release,
      restore: () => {
        release();
        (prototype as unknown as Record<string, unknown>)[method] = original;
      },
    };
  }

  function renewalSettled(spaceId: string) {
    return vi.waitFor(() =>
      runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
        expect((instance as unknown as { renewing?: unknown }).renewing).toBeUndefined();
      }),
    );
  }

  async function connected(name: string) {
    const owner = await getOwner();
    const device = await pairDevice(owner, name);
    const spaceId = await createSpace(owner);
    await ageLeases(spaceId, 0);
    const socket = await Socket.connect(spaceId, device);
    await socket.start();
    return { owner, device, spaceId, socket };
  }



  async function slowAccount(delayMs: number) {
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const originals = { checkSession: prototype.checkSession, checkSessions: prototype.checkSessions };
    const calls = { checkSession: 0, checkSessions: 0 };
    const delay = () => new Promise((resolve) => setTimeout(resolve, delayMs));
    prototype.checkSession = async function (this: AccountDO, deviceId: string, spaceId?: string) {
      calls.checkSession++;
      await delay();
      return originals.checkSession.call(this, deviceId, spaceId);
    };
    prototype.checkSessions = async function (this: AccountDO, deviceIds: string[], spaceId: string) {
      calls.checkSessions++;
      await delay();
      return Object.fromEntries(await Promise.all(deviceIds.map(async (deviceId) => [deviceId, await originals.checkSession.call(this, deviceId, spaceId)])));
    };
    return {
      calls,
      restore: () => Object.assign(prototype, originals),
    };
  }

  let pool: DeviceCredentials[] | undefined;
  async function devicePool(): Promise<DeviceCredentials[]> {
    if (pool) return pool;
    const owner = await getOwner();
    const paired: DeviceCredentials[] = [];
    for (let i = 0; i < 24; i++) paired.push(await pairDevice(owner, "Pooled " + i));
    return (pool = paired);
  }

  async function steadyLoad(leaseMs: number, delayMs: number, forMs: number) {
    const spaceId = await createSpace(await getOwner());
    const creds = await devicePool();
    return withLease(leaseMs, async () => {
      const slow = await slowAccount(delayMs);
      try {
        const sockets = await Promise.all(creds.map((cred) => Socket.connect(spaceId, cred)));
        await Promise.all(sockets.map((socket) => socket.start()));
        const before = { ...slow.calls };
        const until = Date.now() + forMs;
        let replies = 0;
        await Promise.all(
          sockets.map(async (socket) => {
            while (Date.now() < until) if ((await listTree(socket)).case === "rpc") replies++;
          }),
        );
        for (const socket of sockets) socket.ws.close();
        return { replies, checks: slow.calls.checkSession - before.checkSession + slow.calls.checkSessions - before.checkSessions };
      } finally {
        slow.restore();
      }
    });
  }

  it("answers 24 devices from leases when checking them one at a time would outlast a lease", { timeout: 60_000 }, async () => {
    const { replies, checks } = await steadyLoad(1000, 50, 3000);
    expect(replies).toBeGreaterThan(1000);
    expect(checks * 100).toBeLessThan(replies);
  });

  it("checks 24 devices that connect at once without leases in a few Account DO calls", { timeout: 60_000 }, async () => {
    const spaceId = await createSpace(await getOwner());
    const creds = await devicePool();
    const slow = await slowAccount(50);
    try {
      const sockets = await Promise.all(creds.map((cred) => Socket.connect(spaceId, cred)));
      expect(slow.calls.checkSessions).toBeLessThanOrEqual(3);
      for (const socket of sockets) socket.ws.close();
    } finally {
      slow.restore();
    }
  });

  it("renews with a lapsed device's check every connected lease past half, and no lease short of half", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const [lapsed, pastHalf, fresh] = [await pairDevice(owner, "Lapsed"), await pairDevice(owner, "Past half"), await pairDevice(owner, "Fresh")];
    const sockets = await Promise.all([lapsed, pastHalf, fresh].map((device) => Socket.connect(spaceId, device)));
    for (const socket of sockets) await socket.start();
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      const leases = (instance as unknown as { leases: Map<string, { untilMs: number }> }).leases;
      leases.delete(lapsed.deviceId);
      leases.get(pastHalf.deviceId)!.untilMs = instance.clock() + SESSION_LEASE_MS * 0.4;
      leases.get(fresh.deviceId)!.untilMs = instance.clock() + SESSION_LEASE_MS * 0.9;
    });
    const check = await holdAccount("checkSessions");
    try {
      const reply = listTree(sockets[0]!);
      await vi.waitFor(() => expect(check.asked).toHaveLength(1));
      expect([...(check.asked[0] as string[])].sort()).toEqual([lapsed.deviceId, pastHalf.deviceId].sort());
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      check.restore();
    }
  });

  it("answers a Resume sent right after Hello by a socket without a lease in the order they were sent", async () => {
    const owner = await getOwner();
    const device = await pairDevice(owner, "Pipelined");
    const spaceId = await createSpace(owner);
    const socket = await Socket.connect(spaceId, device);
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (instance: SpaceDO) => {
      (instance as unknown as { leases: Map<string, unknown> }).leases.clear();
    });
    socket.send({ case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } });
    socket.send({ case: "resume", value: { historyId: "none", sinceSeq: 0n, sinceCommitId: "", sinceCommittedAtMs: 0n } });
    expect((await socket.next()).case).toBe("welcome");
    expect((await socket.next()).case).not.toBe("goodbye");
  });

  it("renews in one blocking check every connected device whose lease has run out", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const devices = [await pairDevice(owner, "Lapsed A"), await pairDevice(owner, "Lapsed B"), await pairDevice(owner, "Lapsed C")];
    const sockets = await Promise.all(devices.map((device) => Socket.connect(spaceId, device)));
    for (const socket of sockets) await socket.start();
    await passLeases(spaceId);
    const check = await holdAccount("checkSessions");
    try {
      const reply = listTree(sockets[0]!);
      await vi.waitFor(() => expect(check.asked).toHaveLength(1));
      expect([...(check.asked[0] as string[])].sort()).toEqual(devices.map((device) => device.deviceId).sort());
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sockets[1]!)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sockets[2]!)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(check.asked).toHaveLength(1);
    } finally {
      check.restore();
    }
  });

  it("renews a lease past half its life in the background, so a commit does not wait for the Account DO", async () => {
    const { device, spaceId, socket } = await connected("Renewing writer");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      socket.send({ case: "rpc", value: { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), ...socket.slot(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "While renewing")] } } } });
      expect(await socket.next()).toMatchObject({ case: "rpc", value: { result: { case: "commit" } } });
      socket.acceptedSeq += 1n;
      await vi.waitFor(() => expect(renewal.asked).toEqual([[device.deviceId]]));
    } finally {
      renewal.restore();
    }
    await renewalSettled(spaceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype.checkSession;
    prototype.checkSession = async () => {
      throw new Error("injected Account DO outage");
    };
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      prototype.checkSession = original;
    }
  });

  it("renews only leases that an event used past half their life, so an idle device takes the cold path again", async () => {
    const { owner, device: writer, spaceId, socket } = await connected("Busy writer");
    const idle = await pairDevice(owner, "Idle reader");
    const idleSocket = await Socket.connect(spaceId, idle);
    await idleSocket.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toEqual([[writer.deviceId]]));
    } finally {
      renewal.restore();
    }
    await revokeRowOnly(idle.deviceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
    expect(await listTree(idleSocket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  });

  it("keeps one renewal in flight, and starts the next once it has answered", async () => {
    const { spaceId, socket } = await connected("One in flight");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(renewal.asked).toHaveLength(1);
      renewal.release();
      await renewalSettled(spaceId);
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
    } finally {
      renewal.restore();
    }
  });

  it("renews in one check every lease that was used past half while a renewal was in flight", async () => {
    const { owner, spaceId, socket } = await connected("Batch A");
    const b = await pairDevice(owner, "Batch B");
    const c = await pairDevice(owner, "Batch C");
    const sb = await Socket.connect(spaceId, b);
    await sb.start();
    const sc = await Socket.connect(spaceId, c);
    await sc.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect(await listTree(sb)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(await listTree(sc)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      renewal.release();
      await renewalSettled(spaceId);
      expect(await listTree(sb)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
      expect([...(renewal.asked[1] as string[])].sort()).toEqual([b.deviceId, c.deviceId].sort());
    } finally {
      renewal.restore();
    }
  });

  it("does not renew a lease that was used past half but ended before the next renewal", async () => {
    const { owner, spaceId, socket } = await connected("Renewed writer");
    const x = await pairDevice(owner, "Ended before renewal");
    const sx = await Socket.connect(spaceId, x);
    await sx.start();
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect(await listTree(sx)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      renewal.release();
      await renewalSettled(spaceId);
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(2));
      expect(renewal.asked[1]).not.toContain(x.deviceId);
    } finally {
      renewal.restore();
    }
  });

  it("records a revocation that a renewal reports, so it outlasts the lease", async () => {
    const { device, spaceId, socket } = await connected("Revoked in renewal");
    await revokeRowOnly(device.deviceId);
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    const revoked = () =>
      runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => state.storage.sql.exec("SELECT 1 FROM revoked_devices WHERE device_id = ?", device.deviceId).toArray().length);
    await vi.waitFor(async () => expect(await revoked()).toBe(1));
  });

  it("stores nothing from a failed renewal, so the lease runs out as before", async () => {
    const { device, spaceId, socket } = await connected("Failed renewal");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    vi.spyOn(console, "error").mockImplementation(() => {});
    const prototype = await runInDurableObject(account(), (instance: AccountDO) => Object.getPrototypeOf(instance) as AccountDO);
    const original = prototype.checkSessions;
    let calls = 0;
    prototype.checkSessions = async () => {
      calls++;
      throw new Error("injected renewal failure");
    };
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(calls).toBe(1));
    } finally {
      prototype.checkSessions = original;
    }
    await revokeRowOnly(device.deviceId);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
    expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  });

  it("sends a new renewal once a renewal that never answers has been out for a lease", async () => {
    const { spaceId, socket } = await connected("Hung renewal");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions", 1);
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      expect(renewal.asked).toHaveLength(2);
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(3));
    } finally {
      renewal.restore();
    }
  });

  it("still waits for the Account DO once a lease has run out", async () => {
    const { spaceId, socket } = await connected("Expired lease");
    await passLeases(spaceId);
    const check = await holdAccount("checkSession");
    try {
      const reply = listTree(socket);
      const early = await Promise.race([reply.then(() => "answered"), new Promise((resolve) => setTimeout(() => resolve("waiting"), 300))]);
      expect(early).toBe("waiting");
      expect(check.asked).toHaveLength(1);
      check.release();
      expect(await reply).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    } finally {
      check.restore();
    }
  });

  it("refuses a device whose revocation notice arrived while a renewal of its lease was in flight", async () => {
    const { owner, device, spaceId, socket } = await connected("Revoked while renewing");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions");
    const identity = { userId: device.userId, deviceId: device.deviceId, deviceName: "Revoked while renewing", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const ask = async () =>
      fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }))).response).result;
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
      expect((await socket.waitClosed()).code).toBe(Close.DEVICE_REVOKED);
      renewal.release();
      await renewalSettled(spaceId);
      expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
    } finally {
      renewal.restore();
    }
  });

  it("keeps the answer to the check sent last when a renewal from before a cold check arrives after it", async () => {
    const { device, spaceId, socket } = await connected("Renewal overtaken");
    await ageLeases(spaceId, SESSION_LEASE_MS * 0.6);
    const renewal = await holdAccount("checkSessions", 1);
    const identity = { userId: device.userId, deviceId: device.deviceId, deviceName: "Renewal overtaken", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const ask = async () =>
      fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } }))).response).result;
    try {
      expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
      await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
      await runInDurableObject(account(), (_instance, state) => {
        state.storage.sql.exec("UPDATE devices SET expires_at_ms = ? WHERE device_id = ?", Date.now() - 1000, device.deviceId);
      });
      await ageLeases(spaceId, SESSION_LEASE_MS * 0.5);
      expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      renewal.release();
      await renewalSettled(spaceId);
      expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
    } finally {
      renewal.restore();
    }
  });

  it("ends a lease renewed from an answer that predates a revocation by the 204, when the space did not take the notice", async () => {
    await withLease(1000, async () => {
      const { owner, device, spaceId, socket } = await connected("Renewed before revocation");
      await ageLeases(spaceId, 600);
      const renewal = await holdAccount("checkSessions");
      const restore = await loseNotices(spaceId);
      try {
        expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
        await vi.waitFor(() => expect(renewal.asked).toHaveLength(1));
        const sentAtMs = Date.now();
        expect((await send("/v1/devices/" + device.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
        await ageLeases(spaceId, Date.now() - sentAtMs);
        renewal.release();
        await renewalSettled(spaceId);
        expect(socket.closed).toBeUndefined();
        expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      } finally {
        restore();
        renewal.restore();
      }
    });
  }, 30_000);

  it("lets no commit through that a device sends after the 204 of its revocation, during a burst of its commits", async () => {
    await withLease(1000, async () => {
      for (const notice of ["delivered", "lost"]) {
        const owner = await getOwner();
        const writer = await pairDevice(owner, "Burst " + notice);
        const spaceId = await createSpace(owner);
        const socket = await Socket.connect(spaceId, writer);
        await socket.start();
        const restore = notice === "lost" ? await loseNotices(spaceId) : () => {};
        try {
          let answered = false;
          const revoked = send("/v1/devices/" + writer.deviceId, { method: "DELETE", token: owner.access!.token }).then((reply) => {
            answered = true;
            return reply;
          });
          const late: string[] = [];
          for (let i = 1; i < 10_000; i++) {
            const commitId = uuidv7();
            if (answered) late.push(commitId);
            socket.send({ case: "rpc", value: { reqId: BigInt(i), call: { case: "commit", value: { commitId, ...socket.slot(), baseSeq: 0n, ops: [op.mkdir(uuidv7(), spaceId, "Burst " + i)] } } } });
            const reply = await socket.next().catch(() => undefined);
            if (reply?.case !== "rpc" || reply.value.result.case !== "commit") break;
            socket.acceptedSeq += 1n;
          }
          expect((await revoked).status, notice).toBe(204);
          expect((await socket.waitClosed()).code, notice).toBe(Close.DEVICE_REVOKED);
          const stored = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) =>
            state.storage.sql.exec("SELECT commit_id FROM commits").toArray().map((row) => String(row.commit_id)),
          );
          expect(stored.filter((commitId) => late.includes(commitId)), notice).toEqual([]);
        } finally {
          restore();
        }
      }
    });
  }, 30_000);

  it("holds the 204 of a revocation whose notice a space did not take until the space's lease has run out", async () => {
    await withLease(1000, async () => {
      const owner = await getOwner();
      const laptop = await pairDevice(owner, "Unreachable space");
      const spaceId = await createSpace(owner);
      const socket = await Socket.connect(spaceId, laptop);
      await socket.start();
      const restore = await loseNotices(spaceId);
      try {
        const startedAtMs = Date.now();
        expect((await send("/v1/devices/" + laptop.deviceId, { method: "DELETE", token: owner.access!.token })).status).toBe(204);
        expect(Date.now() - startedAtMs).toBeGreaterThanOrEqual(1000);
        expect(socket.closed).toBeUndefined();
        expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
      } finally {
        restore();
      }
    });
  }, 30_000);

  it("asks the Account DO again once a lease has run out, and ends a lease when the device's credential expires", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    const revoked = await pairDevice(owner, "Revoked unseen");
    const socket = await Socket.connect(spaceId, revoked);
    await socket.start();
    await revokeRowOnly(revoked.deviceId);
    expect(await listTree(socket)).toMatchObject({ case: "rpc", value: { result: { case: "tree" } } });
    await passLeases(spaceId);
    expect(await listTree(socket)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });

    const expiring = await pairDevice(owner, "Expiring");
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET expires_at_ms = ? WHERE device_id = ?", Date.now() + 2000, expiring.deviceId);
    });
    const fresh = await createSpace(owner);
    const agent = await Socket.connect(fresh, expiring);
    await agent.start();
    await runInDurableObject(account(), (_instance, state) => {
      state.storage.sql.exec("UPDATE devices SET expires_at_ms = ? WHERE device_id = ?", Date.now() - 1, expiring.deviceId);
    });
    await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(fresh)), (instance: SpaceDO) => {
      instance.clock = () => Date.now() + 2500;
    });
    expect(await listTree(agent)).toMatchObject({ case: "goodbye", value: { code: Close.DEVICE_REVOKED } });
  }, 30_000);

  it("starts a restarted Space DO with no leases", async () => {
    const owner = await getOwner();
    const tablet = await pairDevice(owner, "Restarted space");
    const spaceId = await createSpace(owner);
    const stub = env.SPACE.get(env.SPACE.idFromName(spaceId));
    const identity = { userId: tablet.userId, deviceId: tablet.deviceId, deviceName: "Tablet", expiresAtMs: Date.now() + 900_000, kind: "device", scope: null };
    const request = body(RpcRequestSchema, { reqId: 1n, call: { case: "listTree", value: {} } });
    const ask = async () => fromBinary(RpcResponseSchema, (await env.SPACE.get(env.SPACE.idFromName(spaceId)).httpRpc({ identity, origin: BASE }, request)).response).result;
    expect((await ask()).case).toBe("tree");
    await revokeRowOnly(tablet.deviceId);
    expect((await ask()).case).toBe("tree");
    await evictDurableObject(stub);
    expect(await ask()).toMatchObject({ case: "error", value: { code: Error_Code.UNAUTHENTICATED } });
  });
});
