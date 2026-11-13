// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import type { SpaceUsage } from "../gen/koidrive/v1/auth_pb";
import { type Commit, type CommitResult, CommitResultSchema, type Op, type UploadReceipt } from "../gen/koidrive/v1/commit_pb";
import {
  CaughtUpSchema,
  ChangesSchema,
  ClientMessageSchema,
  GoodbyeSchema,
  type Hello,
  Notice_Kind,
  NoticeSchema,
  type Resume,
  ResyncRequired_Reason,
  ResyncRequiredSchema,
  type RpcRequest,
  type RpcResponse,
  RpcRequestSchema,
  RpcResponseSchema,
  type ServerMessage,
  ServerMessageSchema,
  WelcomeSchema,
} from "../gen/koidrive/v1/messages_pb";
import {
  type ChangeRecord,
  ChangeRecordSchema,
  type ChunkRef,
  type CommitRecord,
  CommitRecordSchema,
  type Content,
  NodePurgedSchema,
  NodeStateSchema,
  VersionInfoSchema,
} from "../gen/koidrive/v1/model_pb";
import {
  ChunkPlan_Action,
  ChunkPlanSchema,
  ChunkTargetSchema,
  DownloadTargetsSchema,
  Error_Code,
  ErrorSchema,
  type GetDownloadTargets,
  type GetVersionChunks,
  type ListTrash,
  type ListTree,
  type ListVersions,
  type PrepareUpload,
  PrepareUploadResultSchema,
  TargetSchema,
  TrashedSchema,
  TrashPageSchema,
  TreePageSchema,
  VersionChunksSchema,
  VersionListSchema,
} from "../gen/koidrive/v1/rpc_pb";
import type { SessionCheck } from "../account/account";
import type { AccountDO } from "../account-do";
import { type Identity, verifyAccessToken } from "../auth";
import { type ChunkObject, sha256, signChunkGrant, verifyChunkGrant } from "../crypto";
import { errorText } from "../errors";
import type { Database, ObjectState, ObjectStore, Socket, Sql } from "../host";
import { fromHex, isUuidV7, toHex, uuidv7 } from "../ids";
import { policy } from "../policy";
import { quotaRefusal } from "../quota";
import { type ApplyContext, CallFailure, CommitApplier } from "./commit";
import { type DigestVersion, type Digests, commitDigests } from "./digest";
import { descriptorDurable, Exporter, noteCommitted, startExport } from "./export";
import type { ShareInfo, ShareListing } from "../gen/koidrive/v1/share_pb";
import type { ShareOutcome } from "../share/links";
import { checkFence, checkLinkRoot, type DownloadFence, type ShareFile, shareFile, shareInfo, shareListing } from "../share/space";
import type { Pitr } from "./pitr";
import { ChunkState } from "./schema";
import { readUsage } from "./usage";
import {
  chunkKey,
  encodeChunkList,
  parseChunkKey,
  getMeta,
  getNode,
  getVersion,
  type NodeRow,
  nodeState,
  rows,
  setMeta,
  type VersionRow,
  versionChunkPage,
  versionChunks,
  versionInfo,
  versionPageCount,
} from "./store";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "../version";

export const FORMAT_VERSION = 1;
const HOUR_MS = 3_600_000;
const DAY_MS = 24 * HOUR_MS;
export const HOLD_MS = DAY_MS;
export const GRANT_MS = HOUR_MS;
const MAX_OPS = 1000;
const MAX_CHUNKS_PER_CALL = 1000;
/** A call stays well inside a Worker invocation's subrequest limit. */
export const MAX_HEAD_CHECKS = 50;
/** RFC 0003 section 5, rule 4. */
const MIN_CHUNK_GRACE_DAYS = 30;
export const MAX_MISSING_LISTED = MAX_CHUNKS_PER_CALL;
export const MAX_CHUNK_BYTES = 16 * 1024 * 1024;
const MAX_SERVER_FRAME = 1024 * 1024;
export const MAX_CLIENT_FRAME = 8 * 1024 * 1024;
const MAX_REQ_ID = 2n ** 64n - 1n;
const RETRY_LATER_MS = 60_000;
const PAGE_ENVELOPE_BYTES = 1024;
const GC_BATCH = 1000;
const GC_RETRY_MS = 10 * 60_000;
const R2_DELETE_BATCH = 1000;
const ORPHAN_SWEEP_MS = 7 * DAY_MS;
const CLOSED_RECHECK_MS = 10 * 60_000;
const EXPORT_RETRY_MS = 60_000;

export type Role = "owner" | "editor" | "viewer";

const SHARE_CLOSED = { ok: false, code: Error_Code.RETRY_LATER, message: "this link is temporarily unavailable; retry later", status: 503 } as const;

export const CONNECT_HEADER = "X-Koidrive-Connect";
export const TOKEN_GRACE_MS = 60_000;

export const Close = {
  TRY_AGAIN_LATER: 1013,
  PROTOCOL: 4000,
  TOKEN_EXPIRED: 4001,
  UNSUPPORTED_VERSION: 4002,
  DEVICE_REVOKED: 4003,
  SPACE_REWOUND: 4004,
} as const;

export interface Connect {
  identity: Identity;
  origin: string;
}

export interface Attachment {
  connId: string;
  identity: Identity;
  origin: string;
  role: Role;
  maxFormat: number;
  hello: boolean;
  live: boolean;
}

export interface Session {
  deviceId: string;
  userId: string;
  deviceName: string;
  kind: string;
  scope: string | null;
  role: Role;
  maxFormat: number;
  origin: string;
}

export interface SpaceSettings {
  version_retention_days: number;
  trash_retention_days: number;
  chunk_grace_days: number;
}

const DEFAULT_SETTINGS: SpaceSettings = { version_retention_days: 30, trash_retention_days: 30, chunk_grace_days: 30 };

export interface RewindCompletion {
  rewindId: string;
  attempt: number;
  historyId: string;
  rewindNumber: number;
  restorePointMs: number;
  completedAtMs: number;
  revokedDevices: string[];
  retired: { hashHex: string; incarnation: string }[];
}

export interface RewindReceipt {
  rewindId: string;
  historyId: string;
  restoreSeq: number;
  completedAtMs: number;
}

export interface GcReport {
  expiredVersions: number;
  purgedNodes: number;
  deletedChunks: number;
  sweptChunks: number;
  more: boolean;
}

function fail(code: Error_Code, message: string): never {
  throw new CallFailure(code, message);
}

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i]);
}

function checkChunkRef(chunk: ChunkRef): void {
  if (chunk.hash.length !== 32) fail(Error_Code.INVALID_ARGUMENT, "chunk hash must be 32 bytes");
  if (chunk.size < 1 || chunk.size > MAX_CHUNK_BYTES) fail(Error_Code.INVALID_ARGUMENT, "chunk size out of range");
  if (chunk.encoding !== 0) fail(Error_Code.INVALID_ARGUMENT, "unsupported chunk encoding");
}

async function checkContent(content: Content | undefined): Promise<Content> {
  if (!content) fail(Error_Code.INVALID_ARGUMENT, "content is required");
  if (content.chunks.length !== content.chunkCount) fail(Error_Code.INVALID_ARGUMENT, "commits must list every chunk inline");
  content.chunks.forEach(checkChunkRef);
  const total = content.chunks.reduce((sum, chunk) => sum + BigInt(chunk.size), 0n);
  if (total !== content.size) fail(Error_Code.INVALID_ARGUMENT, "content size does not match its chunks");
  if (!sameBytes(await sha256(encodeChunkList(content.chunks)), content.manifestDigest)) {
    fail(Error_Code.INVALID_ARGUMENT, "manifest_digest does not match the chunk list");
  }
  return content;
}

function checkIds(op: Op, index: number): void {
  const required: string[] = [];
  const optional: string[] = [];
  switch (op.op.case) {
    case "createDir":
    case "createFile":
      required.push(op.op.value.nodeId, op.op.value.parentId);
      break;
    case "updateContent":
    case "delete":
      required.push(op.op.value.nodeId);
      break;
    case "move":
      required.push(op.op.value.nodeId, op.op.value.newParentId);
      break;
    case "restore":
      required.push(op.op.value.nodeId);
      optional.push(op.op.value.versionId, op.op.value.targetParentId);
      break;
    default:
      fail(Error_Code.INVALID_ARGUMENT, `op ${index} is empty`);
  }
  if (!required.every(isUuidV7) || !optional.every((id) => id === "" || isUuidV7(id))) {
    fail(Error_Code.INVALID_ARGUMENT, `op ${index} has a malformed id`);
  }
}

function opContent(op: Op): Content | undefined | null {
  if (op.op.case === "createFile" || op.op.case === "updateContent") return op.op.value.content;
  return null;
}

function frameBytes(response: RpcResponse): number {
  return toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: response } })).length;
}

function reply(reqId: bigint, result: RpcResponse["result"]): { response: RpcResponse } {
  const response = create(RpcResponseSchema, { reqId, result });
  if (frameBytes(response) > MAX_SERVER_FRAME) fail(Error_Code.LIMIT_EXCEEDED, "the reply does not fit in one 1 MiB message");
  return { response };
}

function fitPage<R, T>(rows: R[], build: (row: R) => T, encodedSize: (item: T) => number): T[] {
  const page: T[] = [];
  let bytes = PAGE_ENVELOPE_BYTES;
  for (const row of rows) {
    const item = build(row);
    bytes += encodedSize(item) + 6;
    if (page.length > 0 && bytes > MAX_SERVER_FRAME) break;
    page.push(item);
  }
  return page;
}

export type Directory = Pick<AccountDO, "checkSessions" | "isSpaceClosed" | "retireChunks" | "serverKey" | "shareResolve">;

export function spaceLogic<B extends abstract new (...args: any[]) => object>(Base: B) {
  abstract class Space extends Base {
    protected abstract readonly db: Database;
    protected abstract readonly sql: Sql;
    protected abstract readonly pitr: Pitr;
    protected abstract readonly state: ObjectState;
    protected abstract readonly objects: ObjectStore;
    protected abstract account(): Directory;
    private spaceKeyCache: Uint8Array | undefined;
    clock: () => number = () => Date.now();
    snapshotPageNodes = 1000;
    private exporting: Promise<void> | undefined;
    private alarmStale = false;
    private readonly leases = new Map<string, { check: SessionCheck; askedAtMs: number; untilMs: number }>();
    private leaseGeneration = 0;
    private readonly renewalsDue = new Set<string>();
    private renewing: { atMs: number; forMs: number } | undefined;
    private gathering: { deviceIds: Set<string>; answers: Promise<Record<string, SessionCheck> | undefined> } | undefined;

    async init(args: { spaceId: string; name: string; ownerUserId: string; spaceKey: Uint8Array }): Promise<void> {
      const existing = getMeta(this.sql, "space_id");
      if (existing !== undefined) {
        if (existing !== args.spaceId) throw new Error("this Space DO already holds another space");
        return;
      }
      this.db.transactionSync(() => {
        setMeta(this.sql, "space_id", args.spaceId);
        setMeta(this.sql, "name", args.name);
        setMeta(this.sql, "format_version", FORMAT_VERSION);
        setMeta(this.sql, "history_id", uuidv7());
        setMeta(this.sql, "head_seq", 0);
        setMeta(this.sql, "log_floor_seq", 0);
        setMeta(this.sql, "space_key", args.spaceKey.slice().buffer);
        setMeta(this.sql, "settings", JSON.stringify(DEFAULT_SETTINGS));
        this.sql.exec("INSERT INTO members (user_id, role, added_seq) VALUES (?, 'owner', 0)", args.ownerUserId);
        this.sql.exec(
          `INSERT INTO nodes (node_id, parent_id, name, name_key, kind, status, content_rev, content_dev, meta_rev, meta_dev)
           VALUES (?, NULL, '', '', 1, 1, 0, '', 0, '')`,
          args.spaceId,
        );
        startExport(this.sql, 0, this.clock(), false);
      });
      try {
        await this.runExport();
      } catch (error) {
        console.error("space export failed", errorText(error));
      }
      await this.schedule("gc", this.clock() + DAY_MS);
    }

    async rpc(session: Session, request: Uint8Array): Promise<Uint8Array> {
      const { response } = await this.call(session, fromBinary(RpcRequestSchema, request));
      return toBinary(RpcResponseSchema, response);
    }

    async httpRpc(connect: Connect, request: Uint8Array): Promise<Uint8Array> {
      const req = fromBinary(RpcRequestSchema, request);
      const role = this.memberRole(connect.identity.userId);
      const refuse = (code: Error_Code, message: string) =>
        toBinary(RpcResponseSchema, create(RpcResponseSchema, { reqId: req.reqId, result: { case: "error", value: create(ErrorSchema, { code, message }) } }));
      if (!role) return refuse(Error_Code.FORBIDDEN, "not a member of this space");
      const check = await this.checkDevice(connect.identity.deviceId);
      if (check === undefined) return refuse(Error_Code.RETRY_LATER, "could not check the device; retry later");
      if (!check.active) return refuse(Error_Code.UNAUTHENTICATED, "device revoked");
      if (check.closed) return refuse(Error_Code.RETRY_LATER, "the space is being rewound; retry later");
      const device = this.sql.exec("SELECT max_format FROM devices WHERE device_id = ?", connect.identity.deviceId).toArray()[0];
      const session = this.sessionOf(connect, role, device ? Number(device.max_format) : FORMAT_VERSION, check);
      const { response, record } = await this.call(session, req);
      if (record) await this.broadcast(record);
      return toBinary(RpcResponseSchema, response);
    }

    async webSocketMessage(ws: Socket, message: string | ArrayBuffer): Promise<void> {
      try {
        await this.receive(ws, message);
      } catch (error) {
        const quota = quotaRefusal(error, this.clock());
        if (!quota) throw error;
        console.error("space session stopped at a platform limit", errorText(error));
        this.send(ws, { case: "notice", value: create(NoticeSchema, { kind: Notice_Kind.QUOTA_EXCEEDED, message: quota.message, retryAfterMs: BigInt(quota.retryAfterMs) }) });
        this.goodbye(ws, Close.TRY_AGAIN_LATER, quota.message);
      }
    }

    private async receive(ws: Socket, message: string | ArrayBuffer): Promise<void> {
      if (ws.readyState !== WebSocket.OPEN) return;
      if (typeof message === "string") return this.goodbye(ws, Close.PROTOCOL, "text messages other than ping are not supported");
      if (message.byteLength > MAX_CLIENT_FRAME) return this.goodbye(ws, Close.PROTOCOL, "message larger than 8 MiB");
      const attachment = ws.deserializeAttachment() as Attachment;
      if (this.clock() > attachment.identity.expiresAtMs + TOKEN_GRACE_MS) return this.goodbye(ws, Close.TOKEN_EXPIRED, "access token expired");
      const check = await this.checkDevice(attachment.identity.deviceId);
      if (check === undefined) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
      if (!check.active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
      if (check.closed) return this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
      if (ws.readyState !== WebSocket.OPEN) return;
      let msg;
      try {
        msg = fromBinary(ClientMessageSchema, new Uint8Array(message)).msg;
      } catch {
        return this.goodbye(ws, Close.PROTOCOL, "malformed ClientMessage");
      }
      if (msg.case === "hello") return this.hello(ws, attachment, msg.value);
      if (msg.case === "reauth") return this.reauth(ws, attachment, msg.value.accessToken);
      if (!attachment.hello) return this.goodbye(ws, Close.PROTOCOL, "Hello must come first");
      if (msg.case === "resume") return this.resume(ws, attachment, msg.value);
      if (msg.case === "rpc") {
        const { response, record } = await this.call(this.sessionOf(attachment, attachment.role, attachment.maxFormat, check), msg.value);
        this.send(ws, { case: "rpc", value: response });
        if (record) await this.broadcast(record, attachment.connId);
        return;
      }
      return this.goodbye(ws, Close.PROTOCOL, "empty ClientMessage");
    }

    async closeDevice(deviceId: string): Promise<void> {
      this.recordRevoked(deviceId);
      for (const ws of this.state.getWebSockets(deviceId)) this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
    }

    private recordRevoked(deviceId: string): void {
      this.sql.exec("INSERT INTO revoked_devices (device_id, revoked_at_ms) VALUES (?, ?) ON CONFLICT DO NOTHING", deviceId, Date.now());
    }

    private revoked(deviceId: string): boolean {
      return this.sql.exec("SELECT 1 FROM revoked_devices WHERE device_id = ?", deviceId).toArray().length > 0;
    }

    private knownCheck(deviceId: string): SessionCheck | undefined {
      if (this.revoked(deviceId)) return { active: false, revoked: true, closed: false, kind: "", scope: null, leaseMs: 0 };
      const lease = this.leases.get(deviceId);
      if (!lease) return undefined;
      const leftMs = lease.untilMs - this.clock();
      if (leftMs <= 0) return undefined;
      if (leftMs < lease.check.leaseMs / 2) {
        this.renewalsDue.add(deviceId);
        this.renewLeases();
      }
      return lease.check;
    }

    private dropLeases(): void {
      this.leases.clear();
      this.leaseGeneration++;
    }

    private storeLease(deviceId: string, check: SessionCheck, askedAtMs: number): void {
      if ((this.leases.get(deviceId)?.askedAtMs ?? -Infinity) > askedAtMs) return;
      this.leases.set(deviceId, { check, askedAtMs, untilMs: askedAtMs + check.leaseMs });
    }

    private renewLeases(): void {
      const askedAtMs = this.clock();
      if (this.renewing && askedAtMs - this.renewing.atMs < this.renewing.forMs) return;
      const due = [...this.renewalsDue].flatMap((deviceId) => {
        const lease = this.leases.get(deviceId);
        const leftMs = lease ? lease.untilMs - askedAtMs : 0;
        return lease && leftMs > 0 && leftMs < lease.check.leaseMs / 2 ? [{ deviceId, leaseMs: lease.check.leaseMs }] : [];
      });
      this.renewalsDue.clear();
      if (due.length === 0) return;
      const deviceIds = due.map(({ deviceId }) => deviceId);
      const generation = this.leaseGeneration;
      const renewal = { atMs: askedAtMs, forMs: Math.max(...due.map(({ leaseMs }) => leaseMs)) };
      this.renewing = renewal;
      this.account()
        .checkSessions(deviceIds, String(getMeta(this.sql, "space_id")))
        .then((answers) => {
          if (generation !== this.leaseGeneration) return;
          for (const [deviceId, check] of Object.entries(answers)) {
            if (check.revoked) this.recordRevoked(deviceId);
            this.storeLease(deviceId, check, askedAtMs);
          }
        })
        .catch((error) => console.error("account lease renewal failed", errorText(error)))
        .finally(() => {
          if (this.renewing === renewal) this.renewing = undefined;
        });
    }

    protected async checkDevice(deviceId: string): Promise<SessionCheck | undefined> {
      return this.knownCheck(deviceId) ?? (await this.coldCheck([deviceId]))?.[deviceId];
    }

    protected async admitDevice(deviceId: string): Promise<SessionCheck | undefined> {
      const known = this.knownCheck(deviceId);
      if (known) return known;
      if (!this.gathering) {
        const gathered = new Set<string>();
        const answers = new Promise((resolve) => setTimeout(resolve, 0)).then(() => {
          this.gathering = undefined;
          return this.coldCheck(gathered);
        });
        this.gathering = { deviceIds: gathered, answers };
      }
      this.gathering.deviceIds.add(deviceId);
      return (await this.gathering.answers)?.[deviceId];
    }

    private async checkDevices(deviceIds: string[]): Promise<Map<string, SessionCheck> | undefined> {
      const checks = new Map<string, SessionCheck>();
      const unknown = deviceIds.filter((deviceId) => {
        const known = this.knownCheck(deviceId);
        if (known) checks.set(deviceId, known);
        return !known;
      });
      if (unknown.length === 0) return checks;
      const answered = await this.coldCheck(unknown);
      if (!answered) return undefined;
      for (const deviceId of unknown) if (answered[deviceId]) checks.set(deviceId, answered[deviceId]);
      return checks;
    }

    /**
     * The error is caught inside the callback, because an exception there would reset this object and drop every
     * connection.
     */
    private async coldCheck(deviceIds: Iterable<string>): Promise<Record<string, SessionCheck> | undefined> {
      const account = this.account();
      const spaceId = String(getMeta(this.sql, "space_id"));
      const answers = await this.state.blockConcurrencyWhile(async () => {
        const askedAtMs = this.clock();
        const asked = new Set(deviceIds);
        for (const ws of this.state.getWebSockets()) {
          const deviceId = (ws.deserializeAttachment() as Attachment).identity.deviceId;
          const lease = this.leases.get(deviceId);
          if ((!lease || lease.untilMs - askedAtMs < lease.check.leaseMs / 2) && !this.revoked(deviceId)) asked.add(deviceId);
        }
        try {
          const answers = await account.checkSessions([...asked], spaceId);
          for (const [deviceId, check] of Object.entries(answers)) this.storeLease(deviceId, check, askedAtMs);
          return answers;
        } catch (error) {
          console.error("account device check failed", errorText(error));
          return undefined;
        }
      });
      for (const [deviceId, check] of Object.entries(answers ?? {})) if (check.revoked) this.recordRevoked(deviceId);
      return answers;
    }

    private checkOpen(): void {
      if (this.sql.exec("SELECT 1 FROM rewind_markers LIMIT 1").toArray().length > 0) fail(Error_Code.RETRY_LATER, "the space is being rewound");
    }

    closeExpired(nowMs: number): number | undefined {
      let next: number | undefined;
      for (const ws of this.state.getWebSockets()) {
        const deadline = (ws.deserializeAttachment() as Attachment).identity.expiresAtMs + TOKEN_GRACE_MS;
        if (deadline < nowMs) this.goodbye(ws, Close.TOKEN_EXPIRED, "access token expired");
        else next = Math.min(next ?? deadline, deadline);
      }
      return next;
    }

    protected memberRole(userId: string): Role | undefined {
      const row = this.sql.exec("SELECT role FROM members WHERE user_id = ?", userId).toArray()[0];
      return row ? (String(row.role) as Role) : undefined;
    }

    private sessionOf(connect: Connect, role: Role, maxFormat: number, { kind, scope }: SessionCheck): Session {
      const { identity, origin } = connect;
      return { deviceId: identity.deviceId, userId: identity.userId, deviceName: identity.deviceName, kind, scope, role, maxFormat, origin };
    }

    private send(ws: Socket, msg: ServerMessage["msg"]): void {
      if (ws.readyState !== WebSocket.OPEN) return;
      ws.send(toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg })));
    }

    private goodbye(ws: Socket, code: number, reason: string): void {
      try {
        this.send(ws, { case: "goodbye", value: create(GoodbyeSchema, { code, reason }) });
        ws.close(code, reason);
      } catch {
      }
    }

    private async broadcast(record: CommitRecord, exceptConnId?: string): Promise<void> {
      const sockets = this.state.getWebSockets().filter((ws) => {
        const attachment = ws.deserializeAttachment() as Attachment;
        return attachment.live && attachment.connId !== exceptConnId;
      });
      if (sockets.length === 0) return;
      const deviceIds = [...new Set(sockets.map((ws) => (ws.deserializeAttachment() as Attachment).identity.deviceId))];
      const checks = await this.checkDevices(deviceIds);
      const frame = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "changes", value: { commits: [record] } } }));
      for (const ws of sockets) {
        if (ws.readyState !== WebSocket.OPEN) continue;
        const check = checks?.get((ws.deserializeAttachment() as Attachment).identity.deviceId);
        if (!check) this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
        else if (!check.active) this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
        else if (check.closed) this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
        else {
          try {
            ws.send(frame);
          } catch {
          }
        }
      }
    }

    private hello(ws: Socket, attachment: Attachment, hello: Hello): void {
      const proto = Math.min(PROTO_MAX, hello.protoMax);
      if (proto < Math.max(PROTO_MIN, hello.protoMin)) {
        return this.goodbye(ws, Close.UNSUPPORTED_VERSION, "this server speaks protocol 1." + PROTO_MIN + " to 1." + PROTO_MAX);
      }
      attachment.hello = true;
      attachment.maxFormat = hello.maxFormat;
      ws.serializeAttachment(attachment);
      const { identity } = attachment;
      this.sql.exec(
        `INSERT INTO devices (device_id, user_id, name, max_format, last_seen_ms) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (device_id) DO UPDATE SET name = excluded.name, max_format = excluded.max_format, last_seen_ms = excluded.last_seen_ms`,
        identity.deviceId,
        identity.userId,
        identity.deviceName,
        hello.maxFormat,
        this.clock(),
      );
      const formatVersion = Number(getMeta(this.sql, "format_version"));
      const welcome = create(WelcomeSchema, {
        proto,
        serverVersion: SERVER_VERSION,
        formatVersion,
        historyId: String(getMeta(this.sql, "history_id")),
        headSeq: BigInt(this.headSeq),
        logFloorSeq: BigInt(Number(getMeta(this.sql, "log_floor_seq"))),
        deviceSeq: this.acceptedDeviceSeq(identity.deviceId),
        restoreSeq: BigInt(Number(getMeta(this.sql, "restore_seq") ?? 0)),
        restorePointMs: BigInt(Number(getMeta(this.sql, "restore_point_ms") ?? 0)),
        readOnly: attachment.role === "viewer" || hello.maxFormat < formatVersion,
      });
      this.send(ws, { case: "welcome", value: welcome });
    }

    private resume(ws: Socket, attachment: Attachment, resume: Resume): void {
      const head = this.headSeq;
      let reason: ResyncRequired_Reason | undefined;
      if (resume.historyId !== getMeta(this.sql, "history_id")) reason = ResyncRequired_Reason.HISTORY_CHANGED;
      else if (resume.sinceSeq > BigInt(head)) reason = ResyncRequired_Reason.AHEAD_OF_SERVER;
      else if (resume.sinceSeq < BigInt(Number(getMeta(this.sql, "log_floor_seq")))) reason = ResyncRequired_Reason.LOG_TRUNCATED;
      else if (this.tokenDiffers(resume.sinceSeq, resume.sinceCommitId, resume.sinceCommittedAtMs)) reason = ResyncRequired_Reason.AHEAD_OF_SERVER;
      if (reason !== undefined) {
        const resync = create(ResyncRequiredSchema, { reason });
        if (reason === ResyncRequired_Reason.HISTORY_CHANGED) {
          resync.restoreSeq = BigInt(Number(getMeta(this.sql, "restore_seq") ?? 0));
          resync.restorePointMs = BigInt(Number(getMeta(this.sql, "restore_point_ms") ?? 0));
        }
        return this.send(ws, { case: "resyncRequired", value: resync });
      }

      let batch: CommitRecord[] = [];
      let bytes = 0;
      const flush = () => {
        if (batch.length > 0) this.send(ws, { case: "changes", value: create(ChangesSchema, { commits: batch }) });
        batch = [];
        bytes = 0;
      };
      for (const row of this.sql.exec("SELECT record FROM commits WHERE seq > ? ORDER BY seq", Number(resume.sinceSeq))) {
        const record = new Uint8Array(row.record as ArrayBuffer);
        const framed = record.length + 4;
        if (bytes + framed > MAX_SERVER_FRAME - 8) flush();
        batch.push(fromBinary(CommitRecordSchema, record));
        bytes += framed;
      }
      flush();
      const token = this.tokenAt(head);
      this.send(ws, {
        case: "caughtUp",
        value: create(CaughtUpSchema, { headSeq: BigInt(head), headCommitId: token.commitId, headCommittedAtMs: token.committedAtMs }),
      });
      attachment.live = true;
      ws.serializeAttachment(attachment);
    }

    private async reauth(ws: Socket, attachment: Attachment, token: string): Promise<void> {
      const account = this.account();
      const identity = await verifyAccessToken(await account.serverKey(), token, this.clock());
      if (!identity || identity.deviceId !== attachment.identity.deviceId || identity.userId !== attachment.identity.userId) {
        return this.goodbye(ws, Close.TOKEN_EXPIRED, "invalid access token");
      }
      const check = await this.checkDevice(identity.deviceId);
      if (check === undefined) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
      if (!check.active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
      if (check.closed) return this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
      const current = ws.deserializeAttachment() as Attachment;
      current.identity = identity;
      ws.serializeAttachment(current);
      await this.scheduleTokenSweep(identity.expiresAtMs + TOKEN_GRACE_MS);
    }

    protected async scheduleTokenSweep(dueMs: number): Promise<void> {
      const current = this.sql.exec("SELECT due_ms FROM schedule WHERE task = 'tokens'").toArray()[0];
      if (!current || Number(current.due_ms) > dueMs) await this.schedule("tokens", dueMs);
    }
    async call(session: Session, request: RpcRequest): Promise<{ response: RpcResponse; record?: CommitRecord }> {
      const reqId = request.reqId;
      try {
        this.checkOpen();
        const call = request.call;
        switch (call.case) {
          case "commit": {
            const { result, record } = await this.commit(session, call.value);
            return { response: create(RpcResponseSchema, { reqId, result: { case: "commit", value: result } }), record };
          }
          case "prepareUpload":
            return reply(reqId, { case: "prepareUpload", value: await this.prepareUpload(session, call.value) });
          case "getDownloadTargets":
            return reply(reqId, { case: "downloadTargets", value: await this.downloadTargets(session, call.value) });
          case "getVersionChunks":
            return reply(reqId, { case: "versionChunks", value: this.versionChunks(call.value) });
          case "listVersions":
            return reply(reqId, { case: "versions", value: this.listVersions(call.value) });
          case "listTree":
            return reply(reqId, { case: "tree", value: this.listTree(call.value) });
          case "listTrash":
            return reply(reqId, { case: "trash", value: this.listTrash(call.value) });
          default:
            return fail(Error_Code.INVALID_ARGUMENT, "empty request");
        }
      } catch (error) {
        if (!(error instanceof CallFailure)) {
          console.error("space rpc failed", errorText(error));
          const quota = quotaRefusal(error, this.clock());
          error = quota ? new CallFailure(Error_Code.QUOTA_EXCEEDED, quota.message, [], quota.retryAfterMs) : new CallFailure(Error_Code.INTERNAL, "internal error");
        }
        const failure = error as CallFailure;
        const value = create(ErrorSchema, {
          code: failure.code,
          message: failure.message,
          missingChunks: failure.missingChunks,
          retryAfterMs: BigInt(failure.retryAfterMs),
        });
        const response = create(RpcResponseSchema, { reqId, result: { case: "error", value } });
        if (frameBytes(response) <= MAX_SERVER_FRAME) return { response };
        const limit = create(ErrorSchema, { code: Error_Code.LIMIT_EXCEEDED, message: "the error reply does not fit in one 1 MiB message" });
        return { response: create(RpcResponseSchema, { reqId, result: { case: "error", value: limit } }) };
      }
    }

    private get spaceId(): string {
      const id = getMeta(this.sql, "space_id");
      if (typeof id !== "string") fail(Error_Code.INVALID_ARGUMENT, "space does not exist");
      return id;
    }

    private get spaceKey(): Uint8Array {
      this.spaceKeyCache ??= new Uint8Array(getMeta(this.sql, "space_key") as ArrayBuffer);
      return this.spaceKeyCache;
    }

    private get headSeq(): number {
      return Number(getMeta(this.sql, "head_seq"));
    }

    private settings(): SpaceSettings {
      const settings: SpaceSettings = { ...DEFAULT_SETTINGS, ...JSON.parse(String(getMeta(this.sql, "settings"))) };
      settings.chunk_grace_days = Math.max(MIN_CHUNK_GRACE_DAYS, settings.chunk_grace_days);
      return settings;
    }

    private checkCanWrite(session: Session): void {
      if (session.role === "viewer") fail(Error_Code.FORBIDDEN, "viewers cannot change this space");
      if (session.maxFormat < Number(getMeta(this.sql, "format_version"))) {
        fail(Error_Code.READ_ONLY, "this client is older than the space format");
      }
    }

    private storedResult(
      commitId: string,
    ): { result: CommitResult; deviceId: string; digest: Uint8Array | null; digestVersion: DigestVersion } | undefined {
      const row =
        this.sql.exec("SELECT results, device_id, request_digest, digest_version FROM commits WHERE commit_id = ?", commitId).toArray()[0] ??
        this.sql
          .exec("SELECT results, device_id, request_digest, digest_version FROM device_commits WHERE commit_id = ?", commitId)
          .toArray()[0];
      if (!row) return undefined;
      const result = fromBinary(CommitResultSchema, new Uint8Array(row.results as ArrayBuffer));
      if (result.committedAtMs === 0n) {
        const logged = this.sql.exec("SELECT committed_at_ms FROM commits WHERE commit_id = ?", commitId).toArray()[0];
        result.committedAtMs = BigInt(Number(logged?.committed_at_ms ?? 0));
      }
      const version = Number(row.digest_version);
      return {
        result,
        deviceId: String(row.device_id ?? ""),
        digest: row.request_digest === null ? null : new Uint8Array(row.request_digest as ArrayBuffer),
        digestVersion: version === 1 || version === 2 ? version : 3,
      };
    }

    private tokenAt(seq: number): { commitId: string; committedAtMs: bigint } {
      const row = this.sql.exec("SELECT commit_id, committed_at_ms FROM commits WHERE seq = ?", seq).toArray()[0];
      return { commitId: String(row?.commit_id ?? ""), committedAtMs: BigInt(Number(row?.committed_at_ms ?? 0)) };
    }

    private tokenDiffers(seq: bigint, commitId: string, committedAtMs: bigint): boolean {
      if (commitId === "" || seq === 0n || seq < BigInt(Number(getMeta(this.sql, "log_floor_seq")))) return false;
      const held = this.tokenAt(Number(seq));
      return held.commitId !== commitId || held.committedAtMs !== committedAtMs;
    }

    private acceptedDeviceSeq(deviceId: string): bigint {
      const row = this.sql.exec("SELECT device_seq FROM device_commits WHERE device_id = ?", deviceId).toArray()[0];
      return BigInt(Number(row?.device_seq ?? 0));
    }

    private checkCommitOrder(deviceId: string, commit: Commit, digests: Digests): CommitResult | undefined {
      if (commit.historyId !== getMeta(this.sql, "history_id")) fail(Error_Code.HISTORY_MISMATCH, "the commit was built in another history of this space");
      const stored = this.storedResult(commit.commitId);
      if (stored) {
        if (stored.deviceId !== deviceId) fail(Error_Code.INVALID_ARGUMENT, "commit_id belongs to a commit of another device");
        if (stored.digest && !sameBytes(stored.digest, digests[stored.digestVersion])) fail(Error_Code.INVALID_ARGUMENT, "commit_id belongs to a different commit");
        return stored.result;
      }
      const accepted = this.acceptedDeviceSeq(deviceId);
      if (commit.deviceSeq === 0n) fail(Error_Code.INVALID_ARGUMENT, "device_seq is required");
      if (commit.deviceSeq <= accepted) {
        fail(Error_Code.STALE_DEVICE_STATE, `device_seq ${commit.deviceSeq} is at or below this device's highest accepted one, ${accepted}`);
      }
      if (commit.deviceSeq > accepted + 1n) fail(Error_Code.INVALID_ARGUMENT, `device_seq must be ${accepted + 1n}`);
      const head = BigInt(this.headSeq);
      if (commit.baseSeq > head) fail(Error_Code.AHEAD_OF_SERVER, "base_seq is ahead of the server");
      if (this.tokenDiffers(commit.baseSeq, commit.baseCommitId, commit.baseCommittedAtMs)) {
        fail(Error_Code.AHEAD_OF_SERVER, "the server does not hold the commit this commit's base names");
      }
      if (commit.requireHead && commit.baseSeq < head) fail(Error_Code.HEAD_MOVED, "the space changed after base_seq");
      return undefined;
    }

    private appendCommit(record: CommitRecord, result: CommitResult, digest: Uint8Array | null): void {
      const recordBytes = toBinary(CommitRecordSchema, record);
      const resultBytes = toBinary(CommitResultSchema, result);
      const pushed = toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "changes", value: { commits: [record] } } }));
      const answered = toBinary(
        ServerMessageSchema,
        create(ServerMessageSchema, { msg: { case: "rpc", value: { reqId: MAX_REQ_ID, result: { case: "commit", value: result } } } }),
      );
      if (Math.max(pushed.length, answered.length) > MAX_SERVER_FRAME) {
        fail(Error_Code.LIMIT_EXCEEDED, "the commit's change records do not fit in one 1 MiB message; split the commit");
      }
      this.sql.exec(
        "INSERT INTO commits (seq, commit_id, device_id, user_id, committed_at_ms, record, results, request_digest, digest_version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 3)",
        Number(record.seq),
        record.commitId,
        record.deviceId,
        record.userId,
        Number(record.committedAtMs),
        recordBytes,
        resultBytes,
        digest,
      );
      setMeta(this.sql, "head_seq", Number(record.seq));
    }

    private async commit(session: Session, commit: Commit): Promise<{ result: CommitResult; record?: CommitRecord }> {
      const digests = await commitDigests(commit);
      const stored = this.checkCommitOrder(session.deviceId, commit, digests);
      if (stored) {
        await this.armAfterCommit();
        return { result: stored };
      }
      this.checkCanWrite(session);
      if (!isUuidV7(commit.commitId)) fail(Error_Code.INVALID_ARGUMENT, "commit_id must be a UUIDv7");
      if (commit.ops.length === 0) fail(Error_Code.INVALID_ARGUMENT, "a commit needs at least one op");
      if (commit.ops.length > MAX_OPS) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_OPS} ops per commit`);

      commit.ops.forEach(checkIds);
      const referenced = new Map<string, ChunkRef>();
      for (const op of commit.ops) {
        const content = opContent(op);
        if (content === null) continue;
        for (const chunk of (await checkContent(content)).chunks) {
          const hex = toHex(chunk.hash);
          if ((referenced.get(hex)?.size ?? chunk.size) !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "one chunk hash is listed with two sizes");
          referenced.set(hex, chunk);
        }
      }
      const nowMs = this.clock();
      const confirmed = await this.confirmUploads(referenced, commit.receipts, nowMs);

      const done = this.db.transactionSync((): { result: CommitResult; record?: CommitRecord; rearm?: boolean } => {
        if (this.revoked(session.deviceId)) fail(Error_Code.UNAUTHENTICATED, "device revoked");
        this.checkOpen();
        const again = this.checkCommitOrder(session.deviceId, commit, digests);
        if (again) return { result: again };
        const head = this.headSeq;

        const missing: Uint8Array[] = [];
        for (const [hex, chunk] of referenced) {
          const row = this.sql.exec("SELECT size, state, incarnation FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
          const ready =
            row &&
            row.size === chunk.size &&
            (row.state === ChunkState.LIVE || (row.state === ChunkState.PENDING && row.incarnation !== null && confirmed.get(hex) === row.incarnation));
          if (!ready) missing.push(chunk.hash);
        }
        if (missing.length > 0) {
          const listed = missing.slice(0, MAX_MISSING_LISTED);
          throw new CallFailure(Error_Code.MISSING_CHUNKS, `${missing.length} chunks are not uploaded; ${listed.length} are listed`, listed);
        }

        const ctx: ApplyContext = {
          seq: head + 1,
          commitId: commit.commitId,
          baseSeq: Number(commit.baseSeq),
          nowMs,
          committer: { deviceId: session.deviceId, userId: session.userId, deviceName: session.deviceName },
          authorize: (op, effect) => policy.authorize(session, op, effect),
        };
        const applier = new CommitApplier(this.sql, ctx);
        const { results, changes } = applier.apply(commit.ops);
        if (!policy.deletes(session, this.spaceId, applier.trashedFiles())) {
          fail(Error_Code.CONFIRMATION_REQUIRED, "this commit deletes more files than the owner allows without confirming");
        }
        const record = create(CommitRecordSchema, {
          seq: BigInt(ctx.seq),
          commitId: commit.commitId,
          deviceId: session.deviceId,
          userId: session.userId,
          committedAtMs: BigInt(nowMs),
          changes,
        });
        const result = create(CommitResultSchema, { seq: BigInt(ctx.seq), results, changes, committedAtMs: BigInt(nowMs) });
        this.appendCommit(record, result, digests[3]);
        this.sql.exec(
          `INSERT INTO device_commits (device_id, device_seq, commit_id, results, request_digest, digest_version) VALUES (?, ?, ?, ?, ?, 3)
           ON CONFLICT (device_id) DO UPDATE SET device_seq = excluded.device_seq, commit_id = excluded.commit_id, results = excluded.results,
             request_digest = excluded.request_digest, digest_version = excluded.digest_version`,
          session.deviceId,
          Number(commit.deviceSeq),
          commit.commitId,
          toBinary(CommitResultSchema, result),
          digests[3],
        );
        return { result, record, rearm: noteCommitted(this.sql, record) };
      });
      if (done.rearm || !done.record || this.alarmStale) await this.armAfterCommit();
      return { result: done.result, record: done.record };
    }

    private async armAfterCommit(): Promise<void> {
      try {
        await this.armAlarm();
        this.alarmStale = false;
      } catch (error) {
        this.alarmStale = true;
        console.error("space alarm write failed", errorText(error));
      }
    }

    private async confirmUploads(referenced: Map<string, ChunkRef>, receipts: readonly UploadReceipt[], nowMs: number): Promise<Map<string, string>> {
      const spaceId = this.spaceId;
      const confirmed = new Map<string, string>();
      const unconfirmed: { object: ChunkObject; size: number }[] = [];
      const receiptsByHash = Map.groupBy(receipts, (receipt) => toHex(receipt.hash));
      for (const [hex, chunk] of referenced) {
        const row = this.sql.exec("SELECT size, state, incarnation, head_checked FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
        if (!row || row.size !== chunk.size || row.state !== ChunkState.PENDING || row.incarnation === null) continue;
        const object = { spaceId, hashHex: hex, incarnation: String(row.incarnation) };
        let valid = row.head_checked === 1;
        for (const receipt of receiptsByHash.get(hex) ?? []) {
          if (receipt.size !== chunk.size) continue;
          if (await verifyChunkGrant(this.spaceKey, "receipt", object, receipt.size, receipt.token, nowMs)) valid = true;
        }
        if (valid) confirmed.set(hex, object.incarnation);
        else unconfirmed.push({ object, size: chunk.size });
      }
      const checked: ChunkObject[] = [];
      for (const { object, size } of unconfirmed.slice(0, MAX_HEAD_CHECKS)) {
        const head = await this.objects.head(chunkKey(spaceId, object.hashHex, object.incarnation));
        if (head?.size !== size) continue;
        confirmed.set(object.hashHex, object.incarnation);
        checked.push(object);
      }
      if (checked.length > 0) {
        this.db.transactionSync(() => {
          for (const object of checked) {
            this.sql.exec(
              "UPDATE chunks SET hold_until_ms = MAX(hold_until_ms, ?), head_checked = 1 WHERE hash = ? AND state = ? AND incarnation = ?",
              nowMs + HOLD_MS,
              fromHex(object.hashHex)!,
              ChunkState.PENDING,
              object.incarnation,
            );
          }
        });
      }
      return confirmed;
    }

    private chunkUrl(session: Session, object: ChunkObject, ticket: string): string {
      return `${session.origin}/v1/spaces/${object.spaceId}/chunks/${object.hashHex}?inc=${object.incarnation}&ticket=${ticket}`;
    }

    private allocateIncarnation(): string {
      const next = Number(getMeta(this.sql, "next_incarnation") ?? 1);
      setMeta(this.sql, "next_incarnation", next + 1);
      return `${Number(getMeta(this.sql, "rewind_number") ?? 0)}.${next}`;
    }

    private async prepareUpload(session: Session, request: PrepareUpload) {
      this.checkCanWrite(session);
      if (request.chunks.length > MAX_CHUNKS_PER_CALL) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_CHUNKS_PER_CALL} chunks per call`);
      request.chunks.forEach(checkChunkRef);
      const byHash = new Map<string, ChunkRef>();
      for (const chunk of request.chunks) {
        const hex = toHex(chunk.hash);
        if ((byHash.get(hex)?.size ?? chunk.size) !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "one chunk hash is listed with two sizes");
        byHash.set(hex, chunk);
      }
      const chunks = [...byHash.values()];
      const nowMs = this.clock();
      const holdUntil = nowMs + HOLD_MS;
      const actions = this.db.transactionSync(() => {
        this.checkOpen();
        return chunks.map((chunk): { action: ChunkPlan_Action; incarnation?: string } => {
          const row = this.sql.exec("SELECT size, state, incarnation FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
          if (!row) {
            const incarnation = this.allocateIncarnation();
            this.sql.exec(
              "INSERT INTO chunks (hash, size, state, refcount, hold_until_ms, incarnation) VALUES (?, ?, ?, 0, ?, ?)",
              chunk.hash,
              chunk.size,
              ChunkState.PENDING,
              holdUntil,
              incarnation,
            );
            return { action: ChunkPlan_Action.UPLOAD, incarnation };
          }
          if (row.size !== chunk.size) fail(Error_Code.INVALID_ARGUMENT, "chunk size differs from the stored chunk");
          if (row.state === ChunkState.DELETING) return { action: ChunkPlan_Action.RETRY_LATER };
          this.sql.exec("UPDATE chunks SET hold_until_ms = MAX(hold_until_ms, ?) WHERE hash = ?", holdUntil, chunk.hash);
          if (row.state === ChunkState.LIVE) return { action: ChunkPlan_Action.PRESENT };
          if (row.incarnation !== null) return { action: ChunkPlan_Action.UPLOAD, incarnation: String(row.incarnation) };
          const incarnation = this.allocateIncarnation();
          this.sql.exec("UPDATE chunks SET incarnation = ? WHERE hash = ?", incarnation, chunk.hash);
          return { action: ChunkPlan_Action.UPLOAD, incarnation };
        });
      });
      const expiresAtMs = nowMs + GRANT_MS;
      const spaceId = this.spaceId;
      const plans = [];
      for (const [i, chunk] of chunks.entries()) {
        const { action, incarnation } = actions[i]!;
        const plan = create(ChunkPlanSchema, { hash: chunk.hash, action });
        if (action === ChunkPlan_Action.RETRY_LATER) plan.retryAfterMs = BigInt(RETRY_LATER_MS);
        if (incarnation !== undefined) {
          const object = { spaceId, hashHex: toHex(chunk.hash), incarnation };
          const ticket = await signChunkGrant(this.spaceKey, "put", object, chunk.size, expiresAtMs);
          plan.target = create(TargetSchema, { url: this.chunkUrl(session, object, ticket), method: "PUT", expiresAtMs: BigInt(expiresAtMs) });
        }
        plans.push(plan);
      }
      return create(PrepareUploadResultSchema, { chunks: plans });
    }

    private async downloadTargets(session: Session, request: GetDownloadTargets) {
      if (request.hashes.length > MAX_CHUNKS_PER_CALL) fail(Error_Code.LIMIT_EXCEEDED, `at most ${MAX_CHUNKS_PER_CALL} chunks per call`);
      const spaceId = this.spaceId;
      const found = request.hashes.map((hash) => {
        const row = this.sql.exec("SELECT state, incarnation FROM chunks WHERE hash = ?", hash).toArray()[0];
        return row?.state === ChunkState.LIVE && row.incarnation !== null ? { spaceId, hashHex: toHex(hash), incarnation: String(row.incarnation) } : undefined;
      });
      const missing = request.hashes.filter((_hash, i) => !found[i]);
      if (missing.length > 0) throw new CallFailure(Error_Code.MISSING_CHUNKS, "some chunks are not stored", missing);
      const expiresAtMs = this.clock() + GRANT_MS;
      const targets = [];
      for (const [i, hash] of request.hashes.entries()) {
        const object = found[i]!;
        const ticket = await signChunkGrant(this.spaceKey, "get", object, 0, expiresAtMs);
        const target = create(TargetSchema, { url: this.chunkUrl(session, object, ticket), method: "GET", expiresAtMs: BigInt(expiresAtMs) });
        targets.push(create(ChunkTargetSchema, { hash, target }));
      }
      return create(DownloadTargetsSchema, { targets });
    }

    private versionChunks(request: GetVersionChunks) {
      const version = getVersion(this.sql, request.versionId);
      if (!version) fail(Error_Code.INVALID_ARGUMENT, "unknown version");
      const pageCount = versionPageCount(version);
      if (request.page >= pageCount) fail(Error_Code.INVALID_ARGUMENT, "page out of range");
      return create(VersionChunksSchema, { chunks: versionChunkPage(this.sql, version, request.page), page: request.page, pageCount });
    }

    private listVersions(request: ListVersions) {
      const limit = Math.min(request.limit || 100, 1000);
      const [seqText, versionId = ""] = request.cursor ? request.cursor.split("/") : [String(Number.MAX_SAFE_INTEGER)];
      const seq = Number(seqText);
      if (!Number.isSafeInteger(seq)) fail(Error_Code.INVALID_ARGUMENT, "malformed cursor");
      const found = rows<VersionRow>(
        this.sql.exec(
          `SELECT * FROM versions WHERE node_id = ? AND (seq < ? OR (seq = ? AND version_id < ?))
           ORDER BY seq DESC, version_id DESC LIMIT ?`,
          request.nodeId,
          seq,
          seq,
          versionId,
          limit + 1,
        ),
      );
      const page = fitPage(found.slice(0, limit), (version) => versionInfo(this.sql, version), (info) => toBinary(VersionInfoSchema, info).length);
      const last = found[page.length - 1];
      return create(VersionListSchema, {
        versions: page,
        nextCursor: found.length > page.length && last ? `${last.seq}/${last.version_id}` : "",
      });
    }

    private nodePage(status: number, cursor: string, requested: number) {
      const limit = Math.min(requested || 1000, 5000);
      const found = rows<NodeRow>(
        this.sql.exec("SELECT * FROM nodes WHERE status = ? AND node_id > ? ORDER BY node_id LIMIT ?", status, cursor, limit + 1),
      );
      const entries = fitPage(
        found.slice(0, limit),
        (node) => ({
          state: nodeState(this.sql, node),
          trashed:
            node.trashed_at_ms === null
              ? undefined
              : create(TrashedSchema, { nodeId: node.node_id, trashedAtMs: BigInt(node.trashed_at_ms), deviceId: node.meta_dev }),
        }),
        ({ state, trashed }) => toBinary(NodeStateSchema, state).length + (trashed ? toBinary(TrashedSchema, trashed).length + 6 : 0),
      );
      const more = entries.length < found.length;
      return { entries, nextCursor: more ? entries.at(-1)!.state.nodeId : "" };
    }

    private listTree(request: ListTree) {
      const head = this.headSeq;
      const token = this.tokenAt(head);
      const { entries, nextCursor } = this.nodePage(1, request.cursor, request.limit);
      return create(TreePageSchema, {
        nodes: entries.map((entry) => entry.state),
        nextCursor,
        headSeq: BigInt(head),
        headCommitId: token.commitId,
        headCommittedAtMs: token.committedAtMs,
      });
    }

    private listTrash(request: ListTrash) {
      const { entries, nextCursor } = this.nodePage(2, request.cursor, request.limit);
      return create(TrashPageSchema, {
        nodes: entries.map((entry) => entry.state),
        trashed: entries.flatMap((entry) => entry.trashed ?? []),
        nextCursor,
      });
    }

    private dropVersion(version: VersionRow, nowMs: number): void {
      const distinct = new Map(versionChunks(this.sql, version).map((chunk) => [toHex(chunk.hash), chunk.hash]));
      for (const hash of distinct.values()) {
        this.sql.exec(
          "UPDATE chunks SET refcount = refcount - 1, zero_since_ms = CASE WHEN refcount = 1 THEN ? ELSE zero_since_ms END WHERE hash = ?",
          nowMs,
          hash,
        );
      }
      this.sql.exec("DELETE FROM version_chunk_pages WHERE version_id = ?", version.version_id);
      this.sql.exec("DELETE FROM versions WHERE version_id = ?", version.version_id);
    }

    async collectGarbage(nowMs: number): Promise<GcReport> {
      const settings = this.settings();
      const spaceId = this.spaceId;
      const purge = this.db.transactionSync(() => {
        const expired = rows<VersionRow>(
          this.sql.exec(
            "SELECT * FROM versions WHERE superseded_at_ms IS NOT NULL AND superseded_at_ms < ? LIMIT ?",
            nowMs - settings.version_retention_days * DAY_MS,
            GC_BATCH,
          ),
        );
        for (const version of expired) this.dropVersion(version, nowMs);

        const changes: ChangeRecord[] = [];
        for (let budget = GC_BATCH; budget > 0; ) {
          const due = rows<NodeRow>(
            this.sql.exec(
              `SELECT * FROM nodes n WHERE status = 2 AND trashed_at_ms < ?
                 AND NOT EXISTS (SELECT 1 FROM nodes c WHERE c.parent_id = n.node_id) LIMIT ?`,
              nowMs - settings.trash_retention_days * DAY_MS,
              budget,
            ),
          );
          if (due.length === 0) break;
          budget -= due.length;
          for (const node of due) {
            for (const version of rows<VersionRow>(this.sql.exec("SELECT * FROM versions WHERE node_id = ?", node.node_id))) {
              this.dropVersion(version, nowMs);
            }
            this.sql.exec("DELETE FROM nodes WHERE node_id = ?", node.node_id);
            changes.push(create(ChangeRecordSchema, { change: { case: "purged", value: create(NodePurgedSchema, { nodeId: node.node_id }) } }));
          }
        }
        let record: CommitRecord | undefined;
        if (changes.length > 0) {
          const seq = BigInt(this.headSeq + 1);
          record = create(CommitRecordSchema, { seq, commitId: uuidv7(nowMs), committedAtMs: BigInt(nowMs), changes });
          this.appendCommit(record, create(CommitResultSchema, { seq, changes, committedAtMs: BigInt(nowMs) }), null);
          noteCommitted(this.sql, record);
        }
        return { expiredVersions: expired.length, purgedNodes: changes.length, record };
      });
      if (purge.record) await this.broadcast(purge.record);

      const marked = this.db.transactionSync(() => {
        const hashes = this.sql
          .exec(
            `SELECT hash FROM chunks WHERE refcount = 0 AND hold_until_ms < ?
               AND (state = 1 OR (state = 2 AND zero_since_ms < ?)) LIMIT ?`,
            nowMs,
            nowMs - settings.chunk_grace_days * DAY_MS,
            GC_BATCH,
          )
          .toArray();
        for (const { hash } of hashes) this.sql.exec("UPDATE chunks SET state = 3 WHERE hash = ?", hash);
        return hashes.length;
      });

      let sweptChunks = 0;
      let sweeping = false;
      if (nowMs >= Number(getMeta(this.sql, "orphan_sweep_due_ms") ?? 0)) {
        const listed = await this.objects.list({
          prefix: `spaces/${spaceId}/chunks/`,
          cursor: String(getMeta(this.sql, "orphan_sweep_cursor") ?? "") || undefined,
          limit: GC_BATCH,
        });
        const graceStart = nowMs - settings.chunk_grace_days * DAY_MS;
        const rewound = Number(getMeta(this.sql, "rewind_completed_ms") ?? 0) > graceStart;
        this.db.transactionSync(() => {
          for (const object of listed.objects) {
            const parsed = parseChunkKey(spaceId, object.key);
            if (!parsed || rewound || object.uploaded.getTime() > graceStart) continue;
            const hash = fromHex(parsed.hashHex)!;
            if (this.sql.exec("SELECT 1 FROM chunks WHERE hash = ? AND incarnation = ?", hash, parsed.incarnation).toArray().length > 0) continue;
            this.sql.exec("INSERT INTO orphans (key, hash, incarnation) VALUES (?, ?, ?) ON CONFLICT DO NOTHING", object.key, hash, parsed.incarnation);
            sweptChunks++;
          }
          setMeta(this.sql, "orphan_sweep_cursor", listed.truncated ? listed.cursor : "");
          if (!listed.truncated) setMeta(this.sql, "orphan_sweep_due_ms", nowMs + ORPHAN_SWEEP_MS);
        });
        sweeping = listed.truncated;
      }

      let deletedChunks = 0;
      for (;;) {
        const rows = this.sql
          .exec("SELECT hash, incarnation FROM chunks WHERE state = 3 LIMIT ?", R2_DELETE_BATCH)
          .toArray()
          .map((row) => ({ hash: new Uint8Array(row.hash as ArrayBuffer), incarnation: row.incarnation as string | null }));
        const orphans = this.sql
          .exec("SELECT key, hash, incarnation FROM orphans LIMIT ?", R2_DELETE_BATCH - rows.length)
          .toArray()
          .map((row) => ({ key: String(row.key), hash: new Uint8Array(row.hash as ArrayBuffer), incarnation: String(row.incarnation) }));
        if (rows.length + orphans.length === 0) break;
        const objects = [
          ...rows.flatMap(({ hash, incarnation }) => (incarnation === null ? [] : [{ hashHex: toHex(hash), incarnation }])),
          ...orphans.map(({ hash, incarnation }) => ({ hashHex: toHex(hash), incarnation })),
        ];
        if (objects.length > 0) {
          const retired = await this.account().retireChunks(spaceId, objects);
          if (!retired.ok) throw new Error(`chunk retirement refused: ${retired.message}`);
          await this.objects.delete(objects.map((object) => chunkKey(spaceId, object.hashHex, object.incarnation)));
        }
        this.db.transactionSync(() => {
          for (const { hash } of rows) this.sql.exec("DELETE FROM chunks WHERE hash = ? AND state = 3", hash);
          for (const { key } of orphans) this.sql.exec("DELETE FROM orphans WHERE key = ?", key);
        });
        deletedChunks += rows.length + orphans.length;
      }
      const { expiredVersions, purgedNodes } = purge;
      return {
        expiredVersions,
        purgedNodes,
        deletedChunks,
        sweptChunks,
        more: expiredVersions === GC_BATCH || purgedNodes === GC_BATCH || marked === GC_BATCH || sweeping,
      };
    }

    async rewindInfo(restorePointMs: number): Promise<{ historyId: string; headSeq: number; descriptorDurable: boolean; bookmark: string | undefined }> {
      this.dropLeases();
      const bookmark = await this.pitr.bookmarkForTime(restorePointMs);
      return { historyId: String(getMeta(this.sql, "history_id")), headSeq: this.headSeq, descriptorDurable: descriptorDurable(this.sql), bookmark };
    }

    async rewindPrepare(args: { rewindId: string; attempt: number; bookmark: string }): Promise<{ historyId: string; headSeq: number; earlier: boolean }> {
      for (const ws of this.state.getWebSockets()) this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
      this.sql.exec("INSERT INTO rewind_markers (rewind_id, attempt) VALUES (?, ?) ON CONFLICT DO NOTHING", args.rewindId, args.attempt);
      await this.state.storage.sync();
      const marked = await this.pitr.currentBookmark();
      return { historyId: String(getMeta(this.sql, "history_id")), headSeq: this.headSeq, earlier: args.bookmark < marked };
    }

    async rewindArm(bookmark: string): Promise<void> {
      await this.pitr.restoreOnNextStart(bookmark);
      this.state.abort("restarting to restore the space");
    }

    async restart(): Promise<void> {
      this.state.abort("restarting");
    }

    async rewindAbort(rewindId: string): Promise<void> {
      this.sql.exec("DELETE FROM rewind_markers WHERE rewind_id = ?", rewindId);
    }

    async rewindComplete(args: RewindCompletion): Promise<{ state: "marker" } | { state: "completed"; receipt: RewindReceipt }> {
      if (this.sql.exec("SELECT 1 FROM rewind_markers WHERE rewind_id = ? AND attempt = ?", args.rewindId, args.attempt).toArray().length > 0) {
        return { state: "marker" };
      }
      const receipt = this.db.transactionSync((): RewindReceipt => {
        const stored = getMeta(this.sql, "rewind_receipt");
        const previous = stored === undefined ? undefined : (JSON.parse(String(stored)) as RewindReceipt);
        if (previous?.rewindId === args.rewindId) return previous;
        const completed: RewindReceipt = { rewindId: args.rewindId, historyId: args.historyId, restoreSeq: this.headSeq, completedAtMs: args.completedAtMs };
        setMeta(this.sql, "history_id", args.historyId);
        setMeta(this.sql, "rewind_number", args.rewindNumber);
        setMeta(this.sql, "restore_seq", completed.restoreSeq);
        setMeta(this.sql, "restore_point_ms", args.restorePointMs);
        setMeta(this.sql, "rewind_completed_ms", args.completedAtMs);
        for (const deviceId of args.revokedDevices) {
          this.sql.exec("INSERT INTO revoked_devices (device_id, revoked_at_ms) VALUES (?, ?) ON CONFLICT DO NOTHING", deviceId, args.completedAtMs);
        }
        for (const { hashHex, incarnation } of args.retired) {
          this.sql.exec(
            "UPDATE chunks SET state = ?, incarnation = NULL, head_checked = 0 WHERE hash = ? AND incarnation = ?",
            ChunkState.PENDING,
            fromHex(hashHex)!,
            incarnation,
          );
        }
        this.sql.exec("UPDATE chunks SET state = ?, incarnation = NULL, head_checked = 0 WHERE state = ?", ChunkState.PENDING, ChunkState.DELETING);
        const holdUntil = args.completedAtMs + this.settings().chunk_grace_days * DAY_MS;
        this.sql.exec("UPDATE chunks SET state = ?, hold_until_ms = ? WHERE refcount = 0", ChunkState.PENDING, holdUntil);
        this.sql.exec("DELETE FROM rewind_markers");
        startExport(this.sql, completed.restoreSeq, args.completedAtMs, true);
        setMeta(this.sql, "rewind_receipt", JSON.stringify(completed));
        return completed;
      });
      await this.armAlarm();
      return { state: "completed", receipt };
    }

    async usage(userId: string): Promise<{ ok: true; value: SpaceUsage } | { ok: false; code: Error_Code; message: string }> {
      if (this.memberRole(userId) !== "owner") return { ok: false, code: Error_Code.FORBIDDEN, message: "only the space's owner can read its usage" };
      return { ok: true, value: readUsage(this.sql) };
    }

    async shareCheckNode(nodeId: string): Promise<ShareOutcome<null>> {
      return checkLinkRoot(this.sql, nodeId);
    }

    async shareInfo(rootId: string, expiresAtMs: number): Promise<ShareOutcome<ShareInfo>> {
      return (await this.closedForRewind()) ? SHARE_CLOSED : shareInfo(this.sql, rootId, expiresAtMs);
    }

    async shareList(rootId: string, dirId: string, cursor: string): Promise<ShareOutcome<ShareListing>> {
      return (await this.closedForRewind()) ? SHARE_CLOSED : shareListing(this.sql, rootId, dirId, cursor);
    }

    async shareFile(rootId: string, nodeId: string): Promise<ShareOutcome<ShareFile>> {
      return (await this.closedForRewind()) ? SHARE_CLOSED : shareFile(this.sql, this.spaceId, rootId, nodeId);
    }

    async shareContinue(link: { linkId: string; client: string; session?: string }, fence: DownloadFence): Promise<ShareOutcome<null>> {
      let resolved;
      try {
        resolved = await this.account().shareResolve({ ...link, counted: false });
      } catch (error) {
        console.error("account link check failed", errorText(error));
        return SHARE_CLOSED;
      }
      return checkFence(this.sql, this.spaceId, resolved, fence);
    }

    private async closedForRewind(): Promise<boolean> {
      const spaceId = getMeta(this.sql, "space_id");
      if (spaceId === undefined) return false;
      try {
        return await this.account().isSpaceClosed(String(spaceId));
      } catch (error) {
        console.error("account rewind check failed", errorText(error));
        return true;
      }
    }

    private runExport(): Promise<void> {
      this.exporting ??= new Exporter(this.db, this.objects, this.snapshotPageNodes).run(this.clock()).finally(() => {
        this.exporting = undefined;
      });
      return this.exporting;
    }

    private async schedule(task: string, dueMs: number): Promise<void> {
      this.sql.exec("INSERT INTO schedule (task, due_ms) VALUES (?, ?) ON CONFLICT (task) DO UPDATE SET due_ms = excluded.due_ms", task, dueMs);
      await this.armAlarm();
    }

    protected async armAlarm(): Promise<void> {
      const next = this.sql.exec("SELECT MIN(due_ms) AS due FROM schedule").one().due;
      if (next === null) await this.state.storage.deleteAlarm();
      else await this.state.storage.setAlarm(Number(next));
    }

    async alarm(): Promise<void> {
      const nowMs = this.clock();
      if (await this.closedForRewind()) {
        await this.state.storage.setAlarm(nowMs + CLOSED_RECHECK_MS);
        return;
      }
      const due = this.sql.exec("SELECT task FROM schedule WHERE due_ms <= ?", nowMs).toArray().map((row) => String(row.task));
      for (const task of due) {
        if (task === "gc") {
          let nextMs = nowMs + GC_RETRY_MS;
          try {
            const report = await this.collectGarbage(nowMs);
            nextMs = nowMs + (report.more ? 60_000 : DAY_MS);
          } catch (error) {
            console.error("space gc failed", errorText(error));
          }
          this.sql.exec("UPDATE schedule SET due_ms = ? WHERE task = 'gc'", nextMs);
        }
        if (task === "export") {
          try {
            await this.runExport();
          } catch (error) {
            console.error("space export failed", errorText(error));
            this.sql.exec("UPDATE schedule SET due_ms = ? WHERE task = 'export'", nowMs + EXPORT_RETRY_MS);
          }
        }
        if (task === "tokens") {
          const next = this.closeExpired(nowMs);
          if (next === undefined) this.sql.exec("DELETE FROM schedule WHERE task = 'tokens'");
          else this.sql.exec("UPDATE schedule SET due_ms = ? WHERE task = 'tokens'", next);
        }
      }
      await this.armAlarm();
    }
  }
  return Space;
}
