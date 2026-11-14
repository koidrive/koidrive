// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { type ClaimAttempt, MAX_SETUP_ATTEMPT_LENGTH, MIN_RECOVERY_KEY_LENGTH, ownerOf, redeemClaim, stillUnused, storeClaimCode, wrongCode } from "./claim";
import { ACCESS_TOKEN_MS, type ServerKey, signAccessToken } from "../auth";
import { base64url, deriveSpaceKey, sha256 } from "../crypto";
import { errorText } from "../errors";
import {
  type AccessToken,
  AccessTokenSchema,
  type DeviceCredentials,
  DeviceCredentialsSchema,
  DeviceInfo_Kind,
  DeviceInfoSchema,
  type DeviceList,
  DeviceListSchema,
  type RewindResult,
  RewindResultSchema,
  type SpaceInfo,
  SpaceInfoSchema,
  type SpaceList,
  SpaceListSchema,
} from "../gen/koidrive/v1/auth_pb";
import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import type { ShareLink, ShareLinkList } from "../gen/koidrive/v1/share_pb";
import type { Database, ObjectState, Sql } from "../host";
import type { SpaceDO } from "../space-do";
import { isUuidV7, uuidv7 } from "../ids";
import { knownPrincipal } from "../policy";
import { FORMAT_VERSION } from "../space/space";
import { MAX_REQUEST_ID_LENGTH, type Outcome, type ResolvedLink, type ShareOutcome, ShareLinks } from "../share/links";

const CLAIM_CODE_MS = 3_600_000;
const PAIRING_CODE_MS = 10 * 60_000;
const DEVICE_PAGE = 100;
const SPACE_PAGE = 100;
const BASE32 = "ABCDEFGHJKMNPQRSTVWXYZ0123456789";
const DAY_MS = 24 * 3_600_000;
const REFRESH_CREDENTIAL = /^[A-Za-z0-9_-]{43}$/;
const RETIREMENT_KEEP_MS = 30 * DAY_MS;
/** PITR restores any point in the past 30 days. */
const PITR_WINDOW_MS = 30 * DAY_MS;
const REWIND_MIN_AGE_MS = 60_000;
const REWIND_MAX_AGE_MS = 29 * DAY_MS;
const REWIND_RETRY_MIN_MS = 1000;
const NOTICE_RETRY_MS = 30_000;
export const SESSION_LEASE_MS = 5000;
const REWIND_RETRY_MAX_MS = 10 * 60_000;

type RewindPhase = "admitted" | "prepared" | "arming" | "completed" | "failed";

interface RewindRow {
  rewind_id: string;
  space_id: string;
  restore_point_ms: number;
  expected_history_id: string;
  bookmark: string;
  history_id: string;
  rewind_number: number;
  previous_history_id: string | null;
  previous_head_seq: number | null;
  attempt: number;
  phase: RewindPhase;
  result: ArrayBuffer | null;
  error_code: number | null;
  error: string | null;
  admitted_at_ms: number;
  ended_at_ms: number | null;
  tries: number;
}

export interface RewindArgs {
  userId: string;
  spaceId: string;
  rewindId: string;
  restorePointMs: number;
  expectedHistoryId: string;
}
const encoder = new TextEncoder();

function ok<T>(value: T): Outcome<T> {
  return { ok: true, value };
}

function failure<T>(code: Error_Code, message: string): Outcome<T> {
  return { ok: false, code, message };
}

function randomCode(length: number): string {
  const bytes = crypto.getRandomValues(new Uint8Array(length));
  return Array.from(bytes, (byte) => BASE32[byte % BASE32.length]).join("");
}

function normalizeCode(code: string): string {
  return code.toUpperCase().replace(/[^A-Z0-9]/g, "");
}

async function codeHash(code: string): Promise<ArrayBuffer> {
  return (await sha256(encoder.encode(normalizeCode(code)))).slice().buffer;
}

function sameBytes(a: ArrayBuffer, b: ArrayBuffer): boolean {
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  return x.length === y.length && x.every((byte, i) => byte === y[i]);
}

interface DeviceRow {
  device_id: string;
  user_id: string;
  name: string;
  platform: string;
  created_at_ms: number;
  last_seen_ms: number | null;
  refresh_hash: ArrayBuffer;
  revoked_at_ms: number | null;
  kind: string;
  scope: string | null;
  expires_at_ms: number | null;
  issued_by: string | null;
  rotation_clock_ms: number;
}

export interface SessionCheck {
  active: boolean;
  revoked: boolean;
  closed: boolean;
  historyId?: string | null;
  kind: string;
  scope: string | null;
  leaseMs: number;
}

const KINDS: Record<string, DeviceInfo_Kind> = { device: DeviceInfo_Kind.DEVICE, agent: DeviceInfo_Kind.AGENT };

interface SpaceRow {
  space_id: string;
  name: string;
  owner_id: string;
  created_at_ms: number;
  format_version: number;
}

function spaceInfo(row: SpaceRow): SpaceInfo {
  return create(SpaceInfoSchema, {
    spaceId: row.space_id,
    name: row.name,
    formatVersion: row.format_version,
    createdAtMs: BigInt(row.created_at_ms),
  });
}

export type SpaceDirectory = Pick<SpaceDO, "closeDevice" | "init" | "restart" | "rewindAbort" | "rewindArm" | "rewindComplete" | "rewindInfo" | "rewindPrepare" | "shareCheckNode">;

export function accountLogic<B extends abstract new (...args: any[]) => object>(Base: B) {
  abstract class Account extends Base {
    protected abstract readonly db: Database;
    protected abstract readonly sql: Sql;
    protected abstract readonly state: ObjectState;
    protected abstract spaceStub(spaceId: string): SpaceDirectory;
    private keyCache: ServerKey | undefined;
    private readonly driving = new Set<string>();
    private readonly creating = new Set<string>();
    private readonly creatingRequests = new Map<string, string>();
    private readonly admitting = new Set<string>();
    sessionLeaseMs = SESSION_LEASE_MS;
    clock: () => number = () => Date.now();
    protected abstract readonly shares: ShareLinks;

    async serverKey(): Promise<ServerKey> {
      if (this.keyCache) return this.keyCache;
      let row = this.sql.exec("SELECT kid, secret FROM server_keys WHERE status = 'active' LIMIT 1").toArray()[0];
      if (!row) {
        const kid = base64url(crypto.getRandomValues(new Uint8Array(6)));
        const secret = crypto.getRandomValues(new Uint8Array(32));
        this.sql.exec("INSERT INTO server_keys (kid, secret, created_at_ms, status) VALUES (?, ?, ?, 'active')", kid, secret.buffer, Date.now());
        row = { kid, secret: secret.buffer };
      }
      this.keyCache = { kid: String(row.kid), secret: new Uint8Array(row.secret as ArrayBuffer) };
      return this.keyCache;
    }

    async isClaimed(): Promise<boolean> {
      return this.sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0;
    }

    async handlesSetupAttempts(): Promise<boolean> {
      return true;
    }

    async issueClaimCode(): Promise<string | undefined> {
      const nowMs = Date.now();
      const code = randomCode(12);
      const hash = await codeHash(code);
      const stored = this.db.transactionSync(() => storeClaimCode(this.sql, hash, nowMs, nowMs + CLAIM_CODE_MS));
      return stored ? `${code.slice(0, 4)}-${code.slice(4, 8)}-${code.slice(8)}` : undefined;
    }

    async claim(
      args: { code: string; userName: string; deviceName: string; platform: string; setupAttempt?: string; requestId?: string },
      client = "",
      recoveryKey?: string,
    ): Promise<Outcome<DeviceCredentials>> {
      const setupAttempt = args.setupAttempt ?? "";
      if (setupAttempt.length > MAX_SETUP_ATTEMPT_LENGTH) return failure(Error_Code.INVALID_ARGUMENT, `setup_attempt is longer than ${MAX_SETUP_ATTEMPT_LENGTH} characters`);
      const requestId = args.requestId ?? "";
      if (requestId.length > MAX_REQUEST_ID_LENGTH) return failure(Error_Code.INVALID_ARGUMENT, `request_id is longer than ${MAX_REQUEST_ID_LENGTH} characters`);
      const nowMs = Date.now();
      const ownerBefore = ownerOf(this.sql);
      const userId = ownerBefore ?? uuidv7(nowMs);
      const device = await this.prepareDevice(userId, args.deviceName, args.platform, nowMs, null);
      const attempt: ClaimAttempt = {
        codeHash: await codeHash(args.code),
        rawHash: (await sha256(encoder.encode(args.code))).slice().buffer,
        recoveryKeyHash:
          recoveryKey && recoveryKey.length >= MIN_RECOVERY_KEY_LENGTH ? (await sha256(encoder.encode(recoveryKey))).slice().buffer : undefined,
        client,
        nowMs,
        ownerBefore,
        userId,
        userName: args.userName,
        deviceId: device.credentials.deviceId,
        setupAttempt,
        requestId,
        storeDevice: device.store,
      };
      const outcome = this.db.transactionSync(() => redeemClaim(this.sql, attempt));
      if (!outcome.ok) return outcome;
      if ("earlierDevice" in outcome) return (await this.reissue(outcome.earlierDevice, nowMs)) ?? failure(Error_Code.FORBIDDEN, "this deployment already has an owner");
      for (const deviceId of outcome.revoke) {
        if (!stillUnused(this.sql, deviceId, setupAttempt, userId)) continue;
        await this.revokeDevice(deviceId).catch((error: unknown) => console.error("setup attempt cleanup failed", deviceId, errorText(error)));
      }
      if (this.activeDevice(device.credentials.deviceId) === undefined) {
        return failure(Error_Code.FORBIDDEN, "a later claim of this setup attempt replaced this one; set a new recovery key and claim again");
      }
      return ok(create(DeviceCredentialsSchema, { ...device.credentials, userName: this.userName(userId) }));
    }

    private userName(userId: string): string {
      return String(this.sql.exec("SELECT name FROM users WHERE user_id = ?", userId).toArray()[0]?.name ?? "");
    }

    private async prepareDevice(userId: string, deviceName: string, platform: string, nowMs: number, issuedBy: string | null) {
      const deviceId = uuidv7(nowMs);
      const name = deviceName || "Unnamed device";
      const refresh = base64url(crypto.getRandomValues(new Uint8Array(32)));
      const refreshHash = (await sha256(encoder.encode(refresh))).slice().buffer;
      const credentials = create(DeviceCredentialsSchema, {
        userId,
        deviceId,
        refreshCredential: refresh,
        access: await this.accessToken({ user_id: userId, device_id: deviceId, name, kind: "device", scope: null, expires_at_ms: null }, nowMs),
        credentialExpiresAtMs: 0n,
      });
      const store = () => {
        this.sql.exec(
          "INSERT INTO devices (device_id, user_id, name, platform, created_at_ms, refresh_hash, issued_by, list_seq) VALUES (?, ?, ?, ?, ?, ?, ?, (SELECT COALESCE(MAX(list_seq), 0) + 1 FROM devices))",
          deviceId,
          userId,
          name,
          platform,
          nowMs,
          refreshHash,
          issuedBy,
        );
      };
      return { credentials, store };
    }

    private async accessToken(device: Pick<DeviceRow, "user_id" | "device_id" | "name" | "kind" | "scope" | "expires_at_ms">, nowMs: number) {
      const expiresAtMs = Math.min(nowMs + ACCESS_TOKEN_MS, device.expires_at_ms ?? Infinity);
      const token = await signAccessToken(await this.serverKey(), {
        userId: device.user_id,
        deviceId: device.device_id,
        deviceName: device.name,
        expiresAtMs,
        kind: device.kind,
        scope: device.scope,
      });
      return create(AccessTokenSchema, { token, expiresAtMs: BigInt(expiresAtMs) });
    }

    async isDeviceActive(deviceId: string): Promise<boolean> {
      return this.activeDevice(deviceId) !== undefined;
    }

    private activeDevice(deviceId: string): DeviceRow | undefined {
      const row = this.sql.exec("SELECT * FROM devices WHERE device_id = ? AND revoked_at_ms IS NULL", deviceId).toArray()[0] as unknown as
        | DeviceRow
        | undefined;
      if (!row || !knownPrincipal(row.kind, row.scope)) return undefined;
      if (row.expires_at_ms !== null && row.expires_at_ms <= Date.now()) return undefined;
      return row;
    }

    async startPairing(deviceId: string): Promise<Outcome<{ code: string; expiresAtMs: number }>> {
      const nowMs = Date.now();
      if (!this.activeDevice(deviceId)) return failure(Error_Code.UNAUTHENTICATED, "device is not active");
      const code = randomCode(8);
      const expiresAtMs = nowMs + PAIRING_CODE_MS;
      this.sql.exec("DELETE FROM pairing_codes WHERE expires_at_ms <= ?", nowMs);
      this.sql.exec("INSERT INTO pairing_codes (code_hash, device_id, expires_at_ms) VALUES (?, ?, ?)", await codeHash(code), deviceId, expiresAtMs);
      return ok({ code: `${code.slice(0, 4)}-${code.slice(4)}`, expiresAtMs });
    }

    async completePairing(
      args: { code: string; deviceName: string; platform: string; requestId?: string },
      client: string,
    ): Promise<Outcome<DeviceCredentials>> {
      const nowMs = Date.now();
      const requestId = args.requestId ?? "";
      if (requestId.length > MAX_REQUEST_ID_LENGTH) return failure(Error_Code.INVALID_ARGUMENT, `request_id is longer than ${MAX_REQUEST_ID_LENGTH} characters`);
      const hash = await codeHash(args.code);
      const refused = () => wrongCode(this.sql, "pair", client, nowMs, "invalid or expired pairing code");
      const row = this.sql.exec("SELECT * FROM pairing_codes WHERE code_hash = ? AND expires_at_ms > ?", hash, nowMs).toArray()[0];
      const inviter = row && this.activeDevice(String(row.device_id));
      if (!inviter) return refused();
      if (row.paired_device_id !== null) {
        const again = requestId && row.request_id === requestId ? await this.reissue(String(row.paired_device_id), nowMs) : undefined;
        return again ?? refused();
      }
      const device = await this.prepareDevice(inviter.user_id, args.deviceName, args.platform, nowMs, inviter.device_id);
      const paired = this.db.transactionSync(() => {
        const taken = this.sql
          .exec("UPDATE pairing_codes SET request_id = ?, paired_device_id = ? WHERE code_hash = ? AND paired_device_id IS NULL RETURNING 1", requestId || null, device.credentials.deviceId, hash)
          .toArray().length > 0;
        if (taken) device.store();
        return taken;
      });
      if (!paired) return failure(Error_Code.RETRY_LATER, "this pairing code is being used; retry");
      return ok(create(DeviceCredentialsSchema, { ...device.credentials, userName: this.userName(inviter.user_id) }));
    }

    private async reissue(deviceId: string, nowMs: number): Promise<Outcome<DeviceCredentials> | undefined> {
      const device = this.activeDevice(deviceId);
      if (!device || device.last_seen_ms !== null) return undefined;
      const refresh = base64url(crypto.getRandomValues(new Uint8Array(32)));
      const refreshHash = (await sha256(encoder.encode(refresh))).slice().buffer;
      const access = await this.accessToken(device, nowMs);
      const replaced = this.sql
        .exec(
          "UPDATE devices SET refresh_hash = ? WHERE device_id = ? AND refresh_hash = ? AND last_seen_ms IS NULL AND revoked_at_ms IS NULL RETURNING 1",
          refreshHash,
          deviceId,
          device.refresh_hash,
        )
        .toArray().length > 0;
      if (!replaced) return this.activeDevice(deviceId)?.last_seen_ms === null ? failure(Error_Code.RETRY_LATER, "another request is issuing this device's credential; retry") : undefined;
      return ok(
        create(DeviceCredentialsSchema, {
          userId: device.user_id,
          deviceId,
          refreshCredential: refresh,
          access,
          credentialExpiresAtMs: 0n,
          userName: this.userName(device.user_id),
        }),
      );
    }

    async refreshAccess(args: {
      deviceId: string;
      refreshCredential: string;
      rotation?: boolean;
      nextRefreshCredential?: string;
      rotateBeforeMs?: bigint;
    }): Promise<Outcome<AccessToken>> {
      const proposed = args.nextRefreshCredential ?? "";
      const rotateBeforeMs = Number(args.rotateBeforeMs ?? 0n);
      if (proposed !== "" && (!REFRESH_CREDENTIAL.test(proposed) || !args.rotation || rotateBeforeMs === 0)) {
        return failure(Error_Code.INVALID_ARGUMENT, "next_refresh_credential needs a well-formed credential, rotation and rotate_before_ms");
      }
      const presented = (await sha256(encoder.encode(args.refreshCredential))).slice().buffer;
      const proposal = proposed === "" ? undefined : (await sha256(encoder.encode(proposed))).slice().buffer;
      const nowMs = this.clock();
      const device = this.activeDevice(args.deviceId);
      if (!device || !sameBytes(device.refresh_hash, presented)) {
        return failure(Error_Code.UNAUTHENTICATED, "unknown device or refresh credential");
      }
      const serverTimeMs = Math.max(nowMs, device.rotation_clock_ms);
      const rotated = proposal !== undefined && serverTimeMs <= rotateBeforeMs;
      this.sql.exec(
        "UPDATE devices SET last_seen_ms = ?, rotation_clock_ms = ?, refresh_hash = ? WHERE device_id = ?",
        nowMs,
        args.rotation ? serverTimeMs : device.rotation_clock_ms,
        rotated ? proposal : device.refresh_hash,
        device.device_id,
      );
      const token = await this.accessToken(device, nowMs);
      if (args.rotation) {
        token.serverTimeMs = BigInt(serverTimeMs);
        token.rotated = rotated;
      }
      return ok(token);
    }

    async listDevices(args: { userId: string; callerId: string; cursor: string }): Promise<DeviceList> {
      const byDeviceId = !/^\d{0,15}$/.test(args.cursor);
      const found = (
        byDeviceId
          ? this.sql.exec("SELECT * FROM devices WHERE user_id = ? AND device_id > ? ORDER BY device_id LIMIT ?", args.userId, args.cursor, DEVICE_PAGE + 1)
          : this.sql.exec("SELECT * FROM devices WHERE user_id = ? AND list_seq > ? ORDER BY list_seq LIMIT ?", args.userId, Number(args.cursor), DEVICE_PAGE + 1)
      ).toArray() as unknown as (DeviceRow & { list_seq: number })[];
      const page = found.slice(0, DEVICE_PAGE);
      return create(DeviceListSchema, {
        devices: page.map((row) =>
          create(DeviceInfoSchema, {
            deviceId: row.device_id,
            name: row.name,
            platform: row.platform,
            kind: KINDS[row.kind] ?? DeviceInfo_Kind.UNSPECIFIED,
            createdAtMs: BigInt(row.created_at_ms),
            lastSeenMs: BigInt(row.last_seen_ms ?? 0),
            revokedAtMs: BigInt(row.revoked_at_ms ?? 0),
            credentialExpiresAtMs: BigInt(row.expires_at_ms ?? 0),
            issuedBy: row.issued_by ?? "",
            current: row.device_id === args.callerId,
          }),
        ),
        nextCursor: found.length <= DEVICE_PAGE ? "" : byDeviceId ? page[page.length - 1]!.device_id : String(page[page.length - 1]!.list_seq),
      });
    }

    async revokeUserDevice(args: { userId: string; callerId: string; deviceId: string; confirmLastDevice: boolean }): Promise<Outcome<null>> {
      if (!this.activeDevice(args.callerId)) return failure(Error_Code.UNAUTHENTICATED, "this device was revoked");
      const found = this.sql.exec("SELECT 1 FROM devices WHERE device_id = ? AND user_id = ?", args.deviceId, args.userId).toArray().length > 0;
      if (!found) return failure(Error_Code.NOT_FOUND, "no such device");
      if (args.deviceId === args.callerId && !args.confirmLastDevice && this.isLastActiveDevice(args.userId, args.callerId)) {
        return failure(Error_Code.CONFIRMATION_REQUIRED, "this is the user's last active device; confirm to revoke it");
      }
      await this.revokeDevice(args.deviceId);
      return ok(null);
    }

    private isLastActiveDevice(userId: string, deviceId: string): boolean {
      const others = this.sql.exec("SELECT device_id FROM devices WHERE user_id = ? AND kind = 'device' AND device_id != ? AND revoked_at_ms IS NULL", userId, deviceId).toArray();
      return !others.some((row) => this.activeDevice(String(row.device_id)) !== undefined);
    }

    async revokeDevice(deviceId: string): Promise<void> {
      const nowMs = Date.now();
      this.db.transactionSync(() => {
        const revoked = this.sql.exec("UPDATE devices SET revoked_at_ms = ? WHERE device_id = ? AND revoked_at_ms IS NULL", nowMs, deviceId).rowsWritten;
        if (revoked === 0) return;
        this.sql.exec(
          "INSERT INTO revocation_notices (device_id, space_id, next_try_ms) SELECT ?, space_id, ? FROM spaces WHERE true ON CONFLICT DO NOTHING",
          deviceId,
          nowMs + NOTICE_RETRY_MS,
        );
      });
      await this.armAlarm();
      await this.deliverNotices(deviceId);
      if (this.sql.exec("SELECT 1 FROM revocation_notices WHERE device_id = ? LIMIT 1", deviceId).toArray().length === 0) return;
      const revokedAtMs = Number(this.sql.exec("SELECT revoked_at_ms FROM devices WHERE device_id = ?", deviceId).one().revoked_at_ms);
      const leaseLeftMs = revokedAtMs + this.sessionLeaseMs - Date.now();
      if (leaseLeftMs > 0) await new Promise((resolve) => setTimeout(resolve, leaseLeftMs));
    }

    private async deliverNotices(deviceId?: string): Promise<void> {
      const pending = (
        deviceId === undefined
          ? this.sql.exec("SELECT device_id, space_id FROM revocation_notices WHERE next_try_ms <= ?", Date.now())
          : this.sql.exec("SELECT device_id, space_id FROM revocation_notices WHERE device_id = ?", deviceId)
      ).toArray();
      await Promise.all(
        pending.map(async ({ device_id, space_id }) => {
          try {
            await this.spaceStub(String(space_id)).closeDevice(String(device_id));
            this.sql.exec("DELETE FROM revocation_notices WHERE device_id = ? AND space_id = ?", device_id, space_id);
          } catch (error) {
            console.error("revocation notice failed", device_id, space_id, errorText(error));
            this.sql.exec("UPDATE revocation_notices SET next_try_ms = ? WHERE device_id = ? AND space_id = ?", Date.now() + NOTICE_RETRY_MS, device_id, space_id);
          }
        }),
      );
      await this.armAlarm();
    }

    async createSpace(args: { userId: string; name: string; requestId?: string }): Promise<Outcome<SpaceInfo>> {
      const nowMs = Date.now();
      const spaceId = uuidv7(nowMs);
      const name = args.name.trim() || "Koidrive";
      const requestId = args.requestId ?? "";
      if (requestId.length > MAX_REQUEST_ID_LENGTH) return failure(Error_Code.INVALID_ARGUMENT, `request_id is longer than ${MAX_REQUEST_ID_LENGTH} characters`);
      if (requestId) {
        const made = this.sql.exec("SELECT * FROM spaces WHERE owner_id = ? AND request_id = ?", args.userId, requestId).toArray()[0] as unknown as SpaceRow | undefined;
        if (made) return made.name === name ? ok(spaceInfo(made)) : failure(Error_Code.INVALID_ARGUMENT, "this request_id was used with another name");
      }
      const request = JSON.stringify([args.userId, requestId]);
      const pending = requestId ? this.creatingRequests.get(request) : undefined;
      if (pending !== undefined) {
        return pending === name ? failure(Error_Code.RETRY_LATER, "this space is still being created; retry") : failure(Error_Code.INVALID_ARGUMENT, "this request_id was used with another name");
      }
      const held = JSON.stringify([args.userId, name]);
      if (this.creating.has(held) || this.sql.exec("SELECT 1 FROM spaces WHERE owner_id = ? AND name = ?", args.userId, name).toArray().length > 0) {
        return failure(Error_Code.INVALID_ARGUMENT, "you already have a space with this name");
      }
      this.creating.add(held);
      if (requestId) this.creatingRequests.set(request, name);
      try {
        const spaceKey = await deriveSpaceKey((await this.serverKey()).secret, spaceId);
        await this.spaceStub(spaceId).init({ spaceId, name, ownerUserId: args.userId, spaceKey });
        this.sql.exec(
          "INSERT INTO spaces (space_id, name, owner_id, created_at_ms, format_version, list_seq, request_id) VALUES (?, ?, ?, ?, ?, (SELECT COALESCE(MAX(list_seq), 0) + 1 FROM spaces), ?)",
          spaceId,
          name,
          args.userId,
          nowMs,
          FORMAT_VERSION,
          requestId || null,
        );
      } finally {
        this.creating.delete(held);
        if (requestId) this.creatingRequests.delete(request);
      }
      return ok(spaceInfo({ space_id: spaceId, name, owner_id: args.userId, created_at_ms: nowMs, format_version: FORMAT_VERSION }));
    }

    async retireChunks(spaceId: string, objects: { hashHex: string; incarnation: string }[]): Promise<Outcome<null>> {
      const nowMs = Date.now();
      if (this.spaceClosed(spaceId)) return failure(Error_Code.RETRY_LATER, "the space is being rewound");
      this.sql.exec("DELETE FROM chunk_retirements WHERE space_id = ? AND retired_at_ms < ?", spaceId, nowMs - RETIREMENT_KEEP_MS);
      for (const { hashHex, incarnation } of objects) {
        this.sql.exec(
          "INSERT INTO chunk_retirements (space_id, hash_hex, incarnation, retired_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
          spaceId,
          hashHex,
          incarnation,
          nowMs,
        );
      }
      return ok(null);
    }

    async checkSessions(deviceIds: string[], spaceId: string): Promise<Record<string, SessionCheck>> {
      return Object.fromEntries(await Promise.all(deviceIds.map(async (deviceId) => [deviceId, await this.checkSession(deviceId, spaceId)])));
    }

    async checkSession(deviceId: string, spaceId?: string): Promise<SessionCheck> {
      const device = this.activeDevice(deviceId);
      const revoked = this.sql.exec("SELECT 1 FROM devices WHERE device_id = ? AND revoked_at_ms IS NOT NULL", deviceId).toArray().length > 0;
      const untilExpiryMs = device?.expires_at_ms == null ? Infinity : device.expires_at_ms - Date.now();
      return {
        active: device !== undefined,
        revoked,
        closed: spaceId !== undefined && this.heldClosed(spaceId),
        historyId: spaceId === undefined ? null : this.currentHistory(spaceId),
        kind: device?.kind ?? "",
        scope: device?.scope ?? null,
        leaseMs: Math.min(this.sessionLeaseMs, untilExpiryMs),
      };
    }

    async isSpaceClosed(spaceId: string, historyId?: string): Promise<boolean> {
      if (this.heldClosed(spaceId) || historyId === undefined) return true;
      const current = this.currentHistory(spaceId);
      return current !== null && current !== historyId;
    }

    private currentHistory(spaceId: string): string | null {
      const row = this.sql
        .exec("SELECT history_id FROM rewinds WHERE space_id = ? AND phase = 'completed' ORDER BY rewind_number DESC LIMIT 1", spaceId)
        .toArray()[0];
      return row === undefined ? null : String(row.history_id);
    }

    private heldClosed(spaceId: string): boolean {
      return this.spaceClosed(spaceId) || this.admitting.has(spaceId);
    }

    protected spaceClosed(spaceId: string): boolean {
      return this.sql.exec("SELECT 1 FROM rewinds WHERE space_id = ? AND phase NOT IN ('completed', 'failed')", spaceId).toArray().length > 0;
    }

    private rewindRow(rewindId: string): RewindRow | undefined {
      return this.sql.exec("SELECT * FROM rewinds WHERE rewind_id = ?", rewindId).toArray()[0] as unknown as RewindRow | undefined;
    }

    async rewind(args: RewindArgs): Promise<Outcome<RewindResult>> {
      if (!isUuidV7(args.rewindId)) return failure(Error_Code.INVALID_ARGUMENT, "rewind_id must be a UUIDv7");
      const known = this.rewindRow(args.rewindId);
      if (known) {
        if (known.space_id !== args.spaceId || known.restore_point_ms !== args.restorePointMs || known.expected_history_id !== args.expectedHistoryId) {
          return failure(Error_Code.INVALID_ARGUMENT, "this rewind_id was used with other fields");
        }
        await this.drive(args.rewindId);
        return this.rewindOutcome(args.rewindId);
      }

      const nowMs = this.clock();
      const space = this.sql.exec("SELECT owner_id, created_at_ms FROM spaces WHERE space_id = ?", args.spaceId).toArray()[0];
      if (!space || space.owner_id !== args.userId) return failure(Error_Code.FORBIDDEN, "only the space's owner can rewind it");
      const point = args.restorePointMs;
      if (point > nowMs - REWIND_MIN_AGE_MS || point < nowMs - REWIND_MAX_AGE_MS) {
        return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms must be one minute to 29 days in the past");
      }
      if (point < Number(space.created_at_ms)) return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms is before the space was created");
      const closedBefore = this.sql
        .exec("SELECT 1 FROM rewinds WHERE space_id = ? AND ended_at_ms IS NOT NULL AND admitted_at_ms <= ? AND ended_at_ms >= ?", args.spaceId, point, point)
        .toArray();
      if (closedBefore.length > 0) return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms lies inside an earlier rewind of this space");
      if (this.heldClosed(args.spaceId)) return failure(Error_Code.RETRY_LATER, "another rewind of this space has not ended");

      this.admitting.add(args.spaceId);
      try {
        let info;
        try {
          info = await this.spaceStub(args.spaceId).rewindInfo(point);
        } catch (error) {
          console.error("rewind admission failed", errorText(error));
          return failure(Error_Code.RETRY_LATER, "the space could not be read; retry the rewind");
        }
        if (info.bookmark === undefined) return failure(Error_Code.INVALID_ARGUMENT, "restore_point_ms is before the space's point-in-time history begins");
        if (info.historyId !== args.expectedHistoryId) return failure(Error_Code.HISTORY_MISMATCH, "expected_history_id is not the space's current history");
        if (!info.descriptorDurable) return failure(Error_Code.RETRY_LATER, "the space's current history is not exported to R2 yet");
        if (this.rewindRow(args.rewindId) || this.spaceClosed(args.spaceId)) return failure(Error_Code.RETRY_LATER, "another rewind of this space has not ended");

        const count = Number(this.sql.exec("SELECT rewind_count FROM space_rewinds WHERE space_id = ?", args.spaceId).toArray()[0]?.rewind_count ?? 0) + 1;
        this.sql.exec(
          "INSERT INTO space_rewinds (space_id, rewind_count) VALUES (?, ?) ON CONFLICT (space_id) DO UPDATE SET rewind_count = excluded.rewind_count",
          args.spaceId,
          count,
        );
        this.sql.exec(
          `INSERT INTO rewinds (rewind_id, space_id, restore_point_ms, expected_history_id, bookmark, history_id, rewind_number, attempt,
             phase, admitted_at_ms, next_try_ms) VALUES (?, ?, ?, ?, ?, ?, ?, 1, 'admitted', ?, ?)`,
          args.rewindId,
          args.spaceId,
          point,
          args.expectedHistoryId,
          info.bookmark,
          uuidv7(),
          count,
          nowMs,
          Date.now() + REWIND_RETRY_MIN_MS,
        );
      } finally {
        this.admitting.delete(args.spaceId);
      }
      await this.armAlarm();
      await this.drive(args.rewindId);
      return this.rewindOutcome(args.rewindId);
    }

    private rewindOutcome(rewindId: string): Outcome<RewindResult> {
      const row = this.rewindRow(rewindId)!;
      if (row.phase === "completed") return ok(fromBinary(RewindResultSchema, new Uint8Array(row.result!)));
      if (row.phase === "failed") return failure(row.error_code ?? Error_Code.INTERNAL, row.error ?? "the rewind failed");
      return failure(Error_Code.RETRY_LATER, "the rewind is running" + (row.error ? "; last error: " + row.error : ""));
    }

    private async drive(rewindId: string): Promise<void> {
      if (this.driving.has(rewindId)) return;
      this.driving.add(rewindId);
      try {
        for (;;) {
          const row = this.rewindRow(rewindId);
          if (!row || row.phase === "completed" || row.phase === "failed") return;
          if (row.phase === "admitted") {
            const prepared = await this.spaceStub(row.space_id).rewindPrepare({ rewindId, attempt: row.attempt, bookmark: row.bookmark });
            if (!prepared.earlier) return await this.failRewind(row, "the selected restore point is not earlier than the space's current state");
            this.sql.exec(
              `UPDATE rewinds SET phase = 'prepared', previous_history_id = COALESCE(previous_history_id, ?),
                 previous_head_seq = COALESCE(previous_head_seq, ?) WHERE rewind_id = ?`,
              prepared.historyId,
              prepared.headSeq,
              rewindId,
            );
          } else if (row.phase === "prepared") {
            this.sql.exec("UPDATE rewinds SET phase = 'arming' WHERE rewind_id = ?", rewindId);
            try {
              await this.spaceStub(row.space_id).rewindArm(row.bookmark);
            } catch {
              // Arming restarts the Space DO, so the call ends in an error either way; settling decides what happened.
            }
          } else {
            try {
              await this.spaceStub(row.space_id).restart();
            } catch {
            }
            const nowMs = this.clock();
            const completion = await this.spaceStub(row.space_id).rewindComplete({
              rewindId,
              attempt: row.attempt,
              historyId: row.history_id,
              rewindNumber: row.rewind_number,
              restorePointMs: row.restore_point_ms,
              completedAtMs: nowMs,
              revokedDevices: this.sql.exec("SELECT device_id FROM devices WHERE revoked_at_ms IS NOT NULL").toArray().map((r) => String(r.device_id)),
              retired: this.sql
                .exec("SELECT hash_hex, incarnation FROM chunk_retirements WHERE space_id = ?", row.space_id)
                .toArray()
                .map((r) => ({ hashHex: String(r.hash_hex), incarnation: String(r.incarnation) })),
            });
            if (completion.state === "marker") {
              if (nowMs > row.restore_point_ms + PITR_WINDOW_MS) {
                return await this.failRewind(row, "the restore point left the PITR window before the restore ran");
              }
              this.sql.exec("UPDATE rewinds SET phase = 'admitted', attempt = attempt + 1 WHERE rewind_id = ?", rewindId);
              throw new Error("the restore did not run; preparing another attempt");
            }
            const result = create(RewindResultSchema, {
              historyId: row.history_id,
              restoreSeq: BigInt(completion.receipt.restoreSeq),
              restorePointMs: BigInt(row.restore_point_ms),
              previousHistoryId: row.previous_history_id ?? "",
              previousHeadSeq: BigInt(row.previous_head_seq ?? 0),
            });
            this.sql.exec(
              "UPDATE rewinds SET phase = 'completed', result = ?, error = NULL, ended_at_ms = ?, next_try_ms = NULL WHERE rewind_id = ?",
              toBinary(RewindResultSchema, result),
              nowMs,
              rewindId,
            );
            this.sql.exec("UPDATE space_rewinds SET completed_at_ms = ? WHERE space_id = ?", completion.receipt.completedAtMs, row.space_id);
            return;
          }
        }
      } catch (error) {
        const row = this.rewindRow(rewindId)!;
        const delay = Math.min(REWIND_RETRY_MAX_MS, REWIND_RETRY_MIN_MS * 2 ** Math.min(row.tries, 20));
        console.error("rewind phase failed", rewindId, row.phase, errorText(error));
        this.sql.exec(
          "UPDATE rewinds SET tries = tries + 1, error = ?, next_try_ms = ? WHERE rewind_id = ?",
          error instanceof Error ? error.message : String(error),
          Date.now() + delay,
          rewindId,
        );
      } finally {
        this.driving.delete(rewindId);
        await this.armAlarm();
      }
    }

    private async failRewind(row: RewindRow, reason: string): Promise<void> {
      await this.spaceStub(row.space_id).rewindAbort(row.rewind_id);
      this.sql.exec(
        "UPDATE rewinds SET phase = 'failed', error_code = ?, error = ?, ended_at_ms = ?, next_try_ms = NULL WHERE rewind_id = ?",
        Error_Code.INVALID_ARGUMENT,
        reason,
        this.clock(),
        row.rewind_id,
      );
    }

    private async armAlarm(): Promise<void> {
      const rewind = this.sql.exec("SELECT MIN(next_try_ms) AS due FROM rewinds WHERE phase NOT IN ('completed', 'failed')").one().due;
      const notice = this.sql.exec("SELECT MIN(next_try_ms) AS due FROM revocation_notices").one().due;
      const due = [rewind, notice].filter((value) => value !== null).map(Number);
      if (due.length === 0) await this.state.storage.deleteAlarm();
      else await this.state.storage.setAlarm(Math.max(Math.min(...due), Date.now() + REWIND_RETRY_MIN_MS));
    }

    async alarm(): Promise<void> {
      const due = this.sql
        .exec("SELECT rewind_id FROM rewinds WHERE phase NOT IN ('completed', 'failed') AND (next_try_ms IS NULL OR next_try_ms <= ?)", Date.now())
        .toArray();
      for (const { rewind_id } of due) await this.drive(String(rewind_id));
      await this.deliverNotices();
    }

    async listSpaces(args: { userId: string; after: number }): Promise<SpaceList> {
      const found = this.sql
        .exec("SELECT * FROM spaces WHERE owner_id = ? AND list_seq > ? ORDER BY list_seq LIMIT ?", args.userId, args.after, SPACE_PAGE + 1)
        .toArray() as unknown as (SpaceRow & { list_seq: number })[];
      const page = found.slice(0, SPACE_PAGE);
      return create(SpaceListSchema, {
        spaces: page.map(spaceInfo),
        nextCursor: found.length > SPACE_PAGE ? String(page[page.length - 1]!.list_seq) : "",
      });
    }

    async createShareLink(args: { spaceId: string; nodeId: string; createdBy: string; expiresAtMs?: number; password?: string }): Promise<string> {
      return this.shares.create(args);
    }

    async revokeShareLink(linkId: string): Promise<void> {
      this.shares.revoke(linkId);
    }

    async openShareLink(linkId: string, password: string | undefined): Promise<Outcome<{ spaceId: string; nodeId: string }>> {
      return this.shares.open(linkId, password);
    }

    async shareCreate(args: Parameters<ShareLinks["ownerCreate"]>[0]): Promise<ShareOutcome<ShareLink>> {
      return this.shares.ownerCreate(args, () => this.spaceStub(args.spaceId).shareCheckNode(args.nodeId));
    }

    async shareList(args: Parameters<ShareLinks["ownerList"]>[0]): Promise<ShareOutcome<ShareLinkList>> {
      return this.shares.ownerList(args);
    }

    async shareRevoke(args: Parameters<ShareLinks["ownerRevoke"]>[0]): Promise<ShareOutcome<null>> {
      return this.shares.ownerRevoke(args);
    }

    async shareResolve(args: Parameters<ShareLinks["resolve"]>[0]): Promise<ShareOutcome<ResolvedLink>> {
      return this.shares.resolve(args);
    }

    async shareUnlock(args: Parameters<ShareLinks["unlock"]>[0]): Promise<ShareOutcome<{ session?: string }>> {
      return this.shares.unlock(args);
    }
  }
  return Account;
}
