// SPDX-License-Identifier: AGPL-3.0-or-later

import { clone, create, fromBinary, toBinary } from "@bufbuild/protobuf";
import type { SpaceUsage } from "../gen/koidrive/v1/auth_pb";
import { type Commit, type CommitResult, CommitResultSchema, type UploadReceipt } from "../gen/koidrive/v1/commit_pb";
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
  type NodeState,
  NodeStateSchema,
  VersionInfoSchema,
} from "../gen/koidrive/v1/model_pb";
import {
  ChunkPlan_Action,
  ChunkPlanSchema,
  ChunkTargetSchema,
  DownloadTargetsSchema,
  Error_Cause,
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
import { verifyAccessToken } from "../auth";
import { type ChunkObject, signChunkGrant, verifyChunkGrant } from "../crypto";
import { errorText } from "../errors";
import type { Database, ObjectState, ObjectStore, Socket, Sql, SqlRow } from "../host";
import { fromHex, isUuidV7, toHex, uuidv7 } from "../ids";
import { GIT_TRACKED } from "../features";
import { policy } from "../policy";
import { causeOf } from "../cause";
import { platformFailure, quotaRefusal } from "../quota";
import { refusalOutcome } from "../http";
import type { MeteredCall } from "../report/meter";
import { batchDue, type Carried, type Counted, mergeBatch, nextHourMs, type UsageBatch, UsageTally } from "../report/usage";
import { type ApplyContext, CallFailure, CommitApplier } from "./commit";
import { DIGEST_VERSION, type Digests, commitDigests, digestVersion } from "./digest";
import { descriptorDurable, Exporter, noteCommitted, startExport } from "./export";
import type { ShareInfo, ShareListing } from "../gen/koidrive/v1/share_pb";
import type { ShareOutcome } from "../share/links";
import { checkFence, checkLinkRoot, type DownloadFence, type ShareFile, shareFile, shareInfo, shareListing } from "../share/space";
import type { Pitr } from "./pitr";
import { ChunkState } from "./schema";
import { backfillLegacyIntents, readIntents, trashHeld } from "./intents";
import { readUsage } from "./usage";
import {
  chunkKey,
  parseChunkKey,
  getMeta,
  getVersion,
  type NodeRow,
  nodeState,
  purgedRecord,
  rows,
  setMeta,
  type VersionRow,
  versionChunkPage,
  versionChunks,
  versionInfo,
  versionPageCount,
} from "./store";
import { PROTO_MAX, PROTO_MIN, SERVER_VERSION } from "../version";
import {
  FORMAT_VERSION,
  HOUR_MS,
  DAY_MS,
  PITR_WINDOW_MS,
  BOOKMARK_SAFETY_MS,
  HOLD_MS,
  GRANT_MS,
  MAX_OPS,
  MAX_CHUNKS_PER_CALL,
  MAX_HEAD_CHECKS,
  MIN_CHUNK_GRACE_DAYS,
  MAX_MISSING_LISTED,
  MAX_SERVER_FRAME,
  MAX_CLIENT_FRAME,
  MAX_REQ_ID,
  RETRY_LATER_MS,
  CLOCK_WAIT_MS,
  GC_BATCH,
  PURGE_BYTES,
  GC_RETRY_MS,
  R2_DELETE_BATCH,
  ORPHAN_SWEEP_MS,
  CLOSED_RECHECK_MS,
  EXPORT_RETRY_MS,
  MAX_PARKED_BYTES,
  USAGE_GRACE_MS,
} from "./limits";
import { type Role, closeReason, RETIRED_HISTORY, CheckFailed, TOKEN_GRACE_MS, Close, type Connect, type Attachment, type Session } from "./session";
import {
  fail,
  sameBytes,
  checkChunkRef,
  checkContent,
  checkIds,
  opContent,
  rpcRoute,
  frameBytes,
  reply,
  fitPage,
} from "./requests";

function welcomed(ws: Socket): ReadonlySet<string> {
  return new Set((ws.deserializeAttachment() as Attachment).features ?? []);
}

function unmark(state: NodeState): void {
  state.gitTracked = false;
  state.gitTrackedRev = 0n;
}

function unmarkRecords(records: readonly ChangeRecord[]): void {
  for (const { change } of records) if (change.case === "node") unmark(change.value);
}

function unmarkReply(response: RpcResponse): RpcResponse {
  const { result } = response;
  if (result.case === "commit") unmarkRecords(result.value.changes);
  if (result.case === "tree" || result.case === "trash") result.value.nodes.forEach(unmark);
  return response;
}

function withoutGitTracked(message: ServerMessage): ServerMessage {
  const { msg } = message;
  if (msg.case === "changes") for (const commit of msg.value.commits) unmarkRecords(commit.changes);
  if (msg.case === "rpc") unmarkReply(msg.value);
  return message;
}

function confirmedPending(row: SqlRow): boolean {
  return row.state === ChunkState.PENDING && row.incarnation !== null && row.head_checked === 1;
}

function oldestUsableBookmarkMs(nowMs: number): number {
  return nowMs - PITR_WINDOW_MS + BOOKMARK_SAFETY_MS;
}

class ClockBehind extends Error {
  constructor(readonly waitMs: number) {
    super("the clock reads no later than the head's stamp");
  }
}

const SHARE_CLOSED = { ok: false, code: Error_Code.RETRY_LATER, message: "this link is temporarily unavailable; retry later", status: 503 } as const;

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

export type Directory = Pick<AccountDO, "checkSessions" | "isSpaceClosed" | "recordUsage" | "retireChunks" | "serverKey" | "shareResolve">;

export function spaceLogic<B extends abstract new (...args: any[]) => object>(Base: B) {
  abstract class Space extends Base {
    protected abstract readonly db: Database;
    protected abstract readonly sql: Sql;
    protected abstract readonly pitr: Pitr;
    protected abstract readonly state: ObjectState;
    protected abstract readonly objects: ObjectStore;
    protected abstract account(call?: MeteredCall): Directory;
    protected abstract features(): ReadonlySet<string>;
    private spaceKeyCache: Uint8Array | undefined;
    clock: () => number = () => Date.now();
    sleep: (ms: number) => Promise<void> = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    snapshotPageNodes = 1000;
    protected readonly traffic = new UsageTally(() => this.clock(), "do_space_active_ms");
    private usageArmed = false;
    private storedHour: number | undefined;
    private exporting: Promise<void> | undefined;
    private alarmStale = false;
    private bookmarking: Promise<void> = Promise.resolve();
    private stampTurn: Promise<unknown> = Promise.resolve();
    private readonly leases = new Map<string, { check: SessionCheck; askedAtMs: number; untilMs: number }>();
    private leaseGeneration = 0;
    private readonly renewalsDue = new Set<string>();
    private renewing: { atMs: number; forMs: number } | undefined;
    private gathering: { deviceIds: Set<string>; answers: Promise<Record<string, SessionCheck> | CheckFailed> } | undefined;

    async init(args: { spaceId: string; name: string; ownerUserId: string; spaceKey: Uint8Array; reporting?: boolean; startedMs?: number }): Promise<void> {
      if (args.reporting !== undefined) this.learn(args.reporting, args.startedMs);
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
        backfillLegacyIntents(this.sql);
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
      const { response, counted } = await this.call(session, fromBinary(RpcRequestSchema, request));
      this.traffic.count(counted.route, counted.outcome, counted.latencyMs, this.clock());
      return toBinary(RpcResponseSchema, this.unwelcomed(response));
    }

    async httpRpc(connect: Connect, request: Uint8Array): Promise<{ response: Uint8Array; counted: Counted }> {
      const startedMs = this.clock();
      const req = fromBinary(RpcRequestSchema, request);
      const role = this.memberRole(connect.identity.userId);
      const refuse = (code: Error_Code, message: string, cause = Error_Cause.UNSPECIFIED) => ({
        response: toBinary(RpcResponseSchema, create(RpcResponseSchema, { reqId: req.reqId, result: { case: "error", value: create(ErrorSchema, { code, message, cause }) } })),
        counted: { route: rpcRoute(req), outcome: refusalOutcome(code, cause), latencyMs: this.clock() - startedMs },
      });
      if (!role) return refuse(Error_Code.FORBIDDEN, "not a member of this space");
      const check = await this.checkDevice(connect.identity.deviceId);
      if (check instanceof CheckFailed) return refuse(Error_Code.RETRY_LATER, "could not check the device; retry later", check.cause);
      if (!check.active) return refuse(Error_Code.UNAUTHENTICATED, "device revoked");
      if (check.closed) return refuse(Error_Code.RETRY_LATER, "the space is being rewound; retry later");
      if (this.historyRetired(check)) return refuse(Error_Code.HISTORY_MISMATCH, RETIRED_HISTORY);
      const device = this.sql.exec("SELECT max_format FROM devices WHERE device_id = ?", connect.identity.deviceId).toArray()[0];
      const session = this.sessionOf(connect, role, device ? Number(device.max_format) : FORMAT_VERSION, check);
      const { response, record, counted } = await this.call(session, req);
      if (record) await this.broadcast(record);
      return { response: toBinary(RpcResponseSchema, this.unwelcomed(response)), counted };
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
      if (check instanceof CheckFailed) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
      if (!check.active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
      if (check.closed) return this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
      if (this.historyRetired(check)) return this.goodbye(ws, Close.TRY_AGAIN_LATER, RETIRED_HISTORY);
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
        const { response, record, counted } = await this.call(this.sessionOf(attachment, attachment.role, attachment.maxFormat, check), msg.value);
        if (response.result.case === "error" && this.revoked(attachment.identity.deviceId)) {
          this.traffic.count(counted.route, counted.outcome, counted.latencyMs, this.clock());
          return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
        }
        this.send(ws, { case: "rpc", value: response });
        if (record) await this.broadcast(record, attachment.connId);
        return this.traffic.count(counted.route, counted.outcome, counted.latencyMs, this.clock());
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
      if (this.revoked(deviceId)) return { active: false, revoked: true, closed: false, historyId: null, kind: "", scope: null, leaseMs: 0, reporting: false };
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
      if (check.historyId === undefined) return;
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
      void this.account({ keep: !this.state.getWebSockets().some((ws) => ws.readyState === WebSocket.OPEN) })
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

    protected historyRetired(check: SessionCheck): boolean {
      return typeof check.historyId === "string" && check.historyId !== String(getMeta(this.sql, "history_id"));
    }

    protected async checkDevice(deviceId: string): Promise<SessionCheck | CheckFailed> {
      const known = this.knownCheck(deviceId);
      if (known) return known;
      const answers = await this.coldCheck([deviceId]);
      return answers instanceof CheckFailed ? answers : (answers[deviceId] ?? new CheckFailed(Error_Cause.UNSPECIFIED));
    }

    protected async admitDevice(deviceId: string): Promise<SessionCheck | CheckFailed> {
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
      const answers = await this.gathering.answers;
      return answers instanceof CheckFailed ? answers : (answers[deviceId] ?? new CheckFailed(Error_Cause.UNSPECIFIED));
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
      if (answered instanceof CheckFailed) return undefined;
      for (const deviceId of unknown) if (answered[deviceId]) checks.set(deviceId, answered[deviceId]);
      return checks;
    }

    /**
     * The error is caught inside the callback, because an exception there would reset this object and drop every
     * connection.
     */
    private async coldCheck(deviceIds: Iterable<string>): Promise<Record<string, SessionCheck> | CheckFailed> {
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
          return new CheckFailed(causeOf(error));
        }
      });
      if (answers instanceof CheckFailed) return answers;
      for (const [deviceId, check] of Object.entries(answers)) if (check.revoked) this.recordRevoked(deviceId);
      return Object.fromEntries(Object.entries(answers).filter(([, check]) => check.historyId !== undefined));
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
      const message = create(ServerMessageSchema, { msg });
      ws.send(toBinary(ServerMessageSchema, welcomed(ws).has(GIT_TRACKED) ? message : withoutGitTracked(clone(ServerMessageSchema, message))));
    }

    private unwelcomed(response: RpcResponse): RpcResponse {
      return this.features().has(GIT_TRACKED) ? response : unmarkReply(clone(RpcResponseSchema, response));
    }

    protected closeStaleWelcomes(): void {
      const features = this.features();
      for (const ws of this.state.getWebSockets()) {
        const sent = welcomed(ws);
        if (!(ws.deserializeAttachment() as Attachment).hello || (sent.size === features.size && [...sent].every((name) => features.has(name)))) continue;
        this.goodbye(ws, Close.TRY_AGAIN_LATER, "the server's features changed; connect again");
      }
    }

    private goodbye(ws: Socket, code: number, reason: string): void {
      void this.releaseUsage(ws);
      try {
        this.send(ws, { case: "goodbye", value: create(GoodbyeSchema, { code, reason }) });
        ws.close(code, closeReason(reason));
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
      const message = create(ServerMessageSchema, { msg: { case: "changes", value: { commits: [record] } } });
      const marked = toBinary(ServerMessageSchema, message);
      const unmarked = sockets.every((ws) => welcomed(ws).has(GIT_TRACKED)) ? marked : toBinary(ServerMessageSchema, withoutGitTracked(clone(ServerMessageSchema, message)));
      for (const ws of sockets) {
        if (ws.readyState !== WebSocket.OPEN) continue;
        const check = checks?.get((ws.deserializeAttachment() as Attachment).identity.deviceId);
        if (!check) this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
        else if (!check.active) this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
        else if (check.closed) this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
        else if (this.historyRetired(check)) this.goodbye(ws, Close.TRY_AGAIN_LATER, RETIRED_HISTORY);
        else {
          try {
            ws.send(welcomed(ws).has(GIT_TRACKED) ? marked : unmarked);
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
      const features = [...this.features()];
      ws.serializeAttachment({ ...(ws.deserializeAttachment() as Attachment), hello: true, maxFormat: hello.maxFormat, features });
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
        ...this.acceptedDeviceCommit(identity.deviceId),
        restoreSeq: BigInt(Number(getMeta(this.sql, "restore_seq") ?? 0)),
        restorePointMs: BigInt(Number(getMeta(this.sql, "restore_point_ms") ?? 0)),
        readOnly: attachment.role === "viewer" || hello.maxFormat < formatVersion,
        features,
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
      ws.serializeAttachment({ ...(ws.deserializeAttachment() as Attachment), live: true });
    }

    private async reauth(ws: Socket, attachment: Attachment, token: string): Promise<void> {
      const account = this.account();
      const identity = await verifyAccessToken(await account.serverKey(), token, this.clock());
      if (!identity || identity.deviceId !== attachment.identity.deviceId || identity.userId !== attachment.identity.userId) {
        return this.goodbye(ws, Close.TOKEN_EXPIRED, "invalid access token");
      }
      const check = await this.checkDevice(identity.deviceId);
      if (check instanceof CheckFailed) return this.goodbye(ws, Close.TRY_AGAIN_LATER, "could not check the device; reconnect later");
      if (!check.active) return this.goodbye(ws, Close.DEVICE_REVOKED, "device revoked");
      if (check.closed) return this.goodbye(ws, Close.SPACE_REWOUND, "the space is being rewound");
      if (this.historyRetired(check)) return this.goodbye(ws, Close.TRY_AGAIN_LATER, RETIRED_HISTORY);
      const current = ws.deserializeAttachment() as Attachment;
      current.identity = identity;
      ws.serializeAttachment(current);
      await this.scheduleTokenSweep(identity.expiresAtMs + TOKEN_GRACE_MS);
    }

    protected async scheduleTokenSweep(dueMs: number): Promise<void> {
      const current = this.sql.exec("SELECT due_ms FROM schedule WHERE task = 'tokens'").toArray()[0];
      if (!current || Number(current.due_ms) > dueMs) await this.schedule("tokens", dueMs);
    }
    async call(session: Session, request: RpcRequest): Promise<{ response: RpcResponse; record?: CommitRecord; counted: Counted }> {
      const startedMs = this.clock();
      const answered = await this.answer(session, request);
      const result = answered.response.result;
      return {
        ...answered,
        counted: {
          route: rpcRoute(request),
          outcome: result.case === "error" ? refusalOutcome(result.value.code, result.value.cause) : "ok",
          latencyMs: this.clock() - startedMs,
        },
      };
    }

    async webSocketClose(ws: Socket, code: number, reason: string): Promise<void> {
      if (ws.readyState === WebSocket.CLOSING) {
        if (code === 1000 || (code >= 3000 && code <= 4999)) ws.close(code, reason);
        else ws.close();
      }
      this.releaseUsage(ws);
    }

    async webSocketError(ws: Socket): Promise<void> {
      this.releaseUsage(ws);
    }

    private releaseUsage(ws: Socket): void {
      const usage = this.unpark(ws);
      if (usage) this.traffic.put(usage);
    }

    protected received(carried: Carried): void {
      if (carried.reporting !== undefined) this.learn(carried.reporting, carried.startedMs);
      if (!carried.usage) return;
      this.traffic.put(carried.usage);
      if (this.traffic.idle) this.rest();
    }

    protected loadFlag(): void {
      const flag = getMeta(this.sql, "reporting");
      const { on, startedMs } = flag === undefined ? {} : (JSON.parse(String(flag)) as { on?: boolean; startedMs?: number });
      this.traffic.reporting = on === true;
      this.traffic.restart(startedMs);
      if (on === true) this.storedHour = (JSON.parse(String(getMeta(this.sql, "r2_stored") ?? "{}")) as { hour?: number }).hour;
    }

    private learn(on: boolean, startedMs: number | undefined): void {
      const changed = this.traffic.reporting !== on;
      this.traffic.reporting = on;
      const restarted = this.traffic.restart(startedMs);
      if (!changed && !restarted) return;
      this.db.transactionSync(() => {
        setMeta(this.sql, "reporting", JSON.stringify({ on, startedMs: this.traffic.startedMs }));
        this.sql.exec("DELETE FROM meta WHERE key = 'r2_stored'");
      });
      this.storedHour = undefined;
      if (!on && changed) this.dropParked();
      else if (on && restarted) this.dropParked(this.traffic.startedMs);
    }

    protected async ended(event: string, args: unknown[], replying: boolean): Promise<void> {
      if (this.traffic.reporting !== true) return;
      this.traffic.lapse();
      if (replying) return;
      this.noteStored();
      const nowMs = this.clock();
      const open = this.state.getWebSockets().filter((ws) => ws.readyState === WebSocket.OPEN);
      const carrier = event.startsWith("webSocket") ? (args[0] as Socket) : undefined;
      const target = carrier?.readyState === WebSocket.OPEN ? carrier : open[0];
      const due = this.traffic.due(nowMs) || open.some((ws) => batchDue((ws.deserializeAttachment() as Attachment).usage ?? { openedMs: nowMs, hours: {} }, nowMs));
      if (target && !due && (await this.parkOn(target))) return;
      await this.handOverUsage();
    }

    private async parkOn(target: Socket): Promise<boolean> {
      await this.armUsage();
      this.traffic.lapse();
      const batch = this.traffic.take();
      if (!batch || this.park(target, batch)) return true;
      this.traffic.put(batch);
      return false;
    }

    private park(ws: Socket, batch: UsageBatch): boolean {
      const attachment = ws.deserializeAttachment() as Attachment;
      attachment.usage = mergeBatch(attachment.usage, batch);
      if (JSON.stringify(attachment).length > MAX_PARKED_BYTES) return false;
      ws.serializeAttachment(attachment);
      return true;
    }

    private unpark(ws: Socket): UsageBatch | undefined {
      const attachment = ws.deserializeAttachment() as Attachment;
      const { usage } = attachment;
      if (!usage) return undefined;
      delete attachment.usage;
      ws.serializeAttachment(attachment);
      return usage;
    }

    private dropParked(openedBeforeMs = Infinity): void {
      for (const ws of this.state.getWebSockets()) {
        if (((ws.deserializeAttachment() as Attachment).usage?.openedMs ?? Infinity) < openedBeforeMs) this.unpark(ws);
      }
    }

    private async armUsage(): Promise<void> {
      if (this.usageArmed) return;
      this.usageArmed = true;
      if (this.sql.exec("SELECT 1 FROM schedule WHERE task = 'usage'").toArray().length > 0) return;
      await this.schedule("usage", nextHourMs(this.clock()) + USAGE_GRACE_MS);
    }

    private async handOverUsage(): Promise<void> {
      for (const ws of this.state.getWebSockets()) {
        const usage = this.unpark(ws);
        if (usage) this.traffic.put(usage);
      }
      await this.traffic.handOver((batch) => this.account().recordUsage(batch));
      this.rest();
    }

    private rest(): void {
      const target = this.traffic.reporting === true ? this.state.getWebSockets().find((ws) => ws.readyState === WebSocket.OPEN) : undefined;
      this.traffic.lapse();
      const batch = this.traffic.take();
      if (batch && target && !this.park(target, batch)) this.traffic.put(batch);
    }

    private noteStored(): void {
      const nowMs = this.clock();
      const hour = Math.floor(nowMs / HOUR_MS);
      if (this.storedHour === hour) return;
      this.storedHour = hour;
      const day = Math.floor(nowMs / DAY_MS);
      const cached = getMeta(this.sql, "r2_stored");
      let counted = cached === undefined ? undefined : (JSON.parse(String(cached)) as { day: number; bytes: number; objects: number });
      if (counted?.day !== day) {
        const row = this.sql.exec("SELECT COUNT(*) AS objects, COALESCE(SUM(size), 0) AS bytes FROM chunks WHERE state != ?", ChunkState.PENDING).one();
        counted = { day, bytes: Number(row.bytes), objects: Number(row.objects) };
      }
      setMeta(this.sql, "r2_stored", JSON.stringify({ day: counted.day, bytes: counted.bytes, objects: counted.objects, hour }));
      this.traffic.note({ object: this.state.id.toString(), sqlBytes: this.sql.databaseSize, r2Bytes: counted.bytes, r2Objects: counted.objects });
    }

        private async answer(session: Session, request: RpcRequest): Promise<{ response: RpcResponse; record?: CommitRecord }> {
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
        let cause = Error_Cause.UNSPECIFIED;
        if (!(error instanceof CallFailure)) {
          console.error("space rpc failed", errorText(error));
          const platform = platformFailure(error, this.clock());
          cause = platform.cause;
          error = new CallFailure(platform.code, platform.message, [], platform.retryAfterMs);
        }
        const failure = error as CallFailure;
        const value = create(ErrorSchema, {
          code: failure.code,
          message: failure.message,
          missingChunks: failure.missingChunks,
          retryAfterMs: BigInt(failure.retryAfterMs),
          cause,
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
    ): { result: CommitResult; deviceId: string; digest: Uint8Array | null; digestVersion: number } | undefined {
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
      return {
        result,
        deviceId: String(row.device_id ?? ""),
        digest: row.request_digest === null ? null : new Uint8Array(row.request_digest as ArrayBuffer),
        digestVersion: Number(row.digest_version),
      };
    }

    private headStampMs(): number {
      return Number(this.tokenAt(this.headSeq).committedAtMs);
    }

    private stampAfterHead(): number {
      const nowMs = this.clock();
      const headMs = this.headStampMs();
      if (nowMs <= headMs) throw new ClockBehind(headMs + 1 - nowMs);
      return nowMs;
    }

    private inStampOrder<T>(append: () => T): Promise<T> {
      const turn = this.stampTurn.then(async () => {
        for (let waitedMs = 0; ; ) {
          try {
            return append();
          } catch (error) {
            if (!(error instanceof ClockBehind)) throw error;
            if (waitedMs + error.waitMs > CLOCK_WAIT_MS) {
              throw new CallFailure(Error_Code.RETRY_LATER, "the server clock is behind its newest commit; retry later", [], Math.min(error.waitMs, RETRY_LATER_MS));
            }
            await this.sleep(error.waitMs);
            waitedMs += error.waitMs;
          }
        }
      });
      this.stampTurn = turn.catch(() => undefined);
      return turn;
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

    private acceptedDeviceCommit(deviceId: string): { deviceSeq: bigint; deviceCommitId: string; deviceCommittedAtMs: bigint } {
      const row = this.sql.exec("SELECT device_seq, commit_id FROM device_commits WHERE device_id = ?", deviceId).toArray()[0];
      const deviceSeq = BigInt(Number(row?.device_seq ?? 0));
      const commitId = String(row?.commit_id ?? "");
      const committedAtMs = commitId === "" ? 0n : (this.storedResult(commitId)?.result.committedAtMs ?? 0n);
      if (committedAtMs === 0n) return { deviceSeq, deviceCommitId: "", deviceCommittedAtMs: 0n };
      return { deviceSeq, deviceCommitId: commitId, deviceCommittedAtMs: committedAtMs };
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
        if (stored.digest && !sameBytes(stored.digest, digests[digestVersion(stored.digestVersion)])) fail(Error_Code.INVALID_ARGUMENT, "commit_id belongs to a different commit");
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
        "INSERT INTO commits (seq, commit_id, device_id, user_id, committed_at_ms, record, results, request_digest, digest_version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        Number(record.seq),
        record.commitId,
        record.deviceId,
        record.userId,
        Number(record.committedAtMs),
        recordBytes,
        resultBytes,
        digest,
        DIGEST_VERSION,
      );
      setMeta(this.sql, "head_seq", Number(record.seq));
      this.bookmarkHead(Number(record.seq), Number(record.committedAtMs));
    }

    private bookmarkHead(seq: number, fromMs: number): void {
      this.bookmarking = this.bookmarking.then(async () => {
        try {
          const bookmark = await this.pitr.currentBookmark();
          if (this.headSeq !== seq) return;
          this.sql.exec("INSERT INTO commit_bookmarks (seq, bookmark, from_ms) VALUES (?, ?, ?) ON CONFLICT DO NOTHING", seq, bookmark, fromMs);
        } catch (error) {
          console.error("commit bookmark read failed", errorText(error));
        }
      });
    }

    private async commit(session: Session, commit: Commit): Promise<{ result: CommitResult; record?: CommitRecord }> {
      const digests = await commitDigests(commit);
      const stored = this.checkCommitOrder(session.deviceId, commit, digests);
      if (stored) {
        await this.armAfterCommit();
        return { result: stored };
      }
      const marking = commit.ops.findIndex((each) => each.op.case === "markGitTracked");
      if (marking >= 0 && !this.features().has(GIT_TRACKED)) fail(Error_Code.INVALID_ARGUMENT, `op ${marking} needs the feature ${GIT_TRACKED}, which this server has off`);
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
      const confirmed = await this.confirmUploads(referenced, commit.receipts, this.clock());

      const done = await this.inStampOrder(() => this.db.transactionSync((): { result: CommitResult; record?: CommitRecord; rearm?: boolean } => {
        if (this.revoked(session.deviceId)) fail(Error_Code.UNAUTHENTICATED, "device revoked");
        this.checkOpen();
        const again = this.checkCommitOrder(session.deviceId, commit, digests);
        if (again) return { result: again };
        const head = this.headSeq;
        const committedAtMs = this.stampAfterHead();

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
          nowMs: committedAtMs,
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
          committedAtMs: BigInt(committedAtMs),
          changes,
        });
        const result = create(CommitResultSchema, { seq: BigInt(ctx.seq), results, changes, committedAtMs: BigInt(committedAtMs) });
        this.appendCommit(record, result, digests[DIGEST_VERSION]);
        this.sql.exec(
          `INSERT INTO device_commits (device_id, device_seq, commit_id, results, request_digest, digest_version) VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT (device_id) DO UPDATE SET device_seq = excluded.device_seq, commit_id = excluded.commit_id, results = excluded.results,
             request_digest = excluded.request_digest, digest_version = excluded.digest_version`,
          session.deviceId,
          Number(commit.deviceSeq),
          commit.commitId,
          toBinary(CommitResultSchema, result),
          digests[DIGEST_VERSION],
          DIGEST_VERSION,
        );
        return { result, record, rearm: noteCommitted(this.sql, record) };
      }));
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
      const receipted: ChunkObject[] = [];
      let live = 0;
      const receiptsByHash = Map.groupBy(receipts, (receipt) => toHex(receipt.hash));
      for (const [hex, chunk] of referenced) {
        const row = this.sql.exec("SELECT size, state, incarnation, head_checked FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
        if (row?.state === ChunkState.LIVE && row.size === chunk.size) live++;
        if (!row || row.size !== chunk.size || row.state !== ChunkState.PENDING || row.incarnation === null) continue;
        const object = { spaceId, hashHex: hex, incarnation: String(row.incarnation) };
        if (confirmedPending(row)) {
          confirmed.set(hex, object.incarnation);
          continue;
        }
        let valid = false;
        for (const receipt of receiptsByHash.get(hex) ?? []) {
          if (receipt.size !== chunk.size) continue;
          if (await verifyChunkGrant(this.spaceKey, "receipt", object, receipt.size, receipt.token, nowMs)) valid = true;
        }
        if (valid) {
          confirmed.set(hex, object.incarnation);
          receipted.push(object);
        } else unconfirmed.push({ object, size: chunk.size });
      }
      const checked: ChunkObject[] = [];
      for (const { object, size } of unconfirmed.slice(0, MAX_HEAD_CHECKS)) {
        const head = await this.objects.head(chunkKey(spaceId, object.hashHex, object.incarnation));
        if (head?.size !== size) continue;
        confirmed.set(object.hashHex, object.incarnation);
        checked.push(object);
      }
      const recorded = live + confirmed.size < referenced.size ? [...checked, ...receipted] : checked;
      if (recorded.length > 0) {
        this.db.transactionSync(() => {
          for (const object of recorded) {
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
          const row = this.sql.exec("SELECT size, state, incarnation, head_checked FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
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
          if (row.state === ChunkState.LIVE || confirmedPending(row)) return { action: ChunkPlan_Action.PRESENT };
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
      const purge = await this.inStampOrder(() => this.db.transactionSync(() => {
        this.sql.exec("DELETE FROM commit_bookmarks WHERE from_ms < ?", oldestUsableBookmarkMs(nowMs));
        const expired = rows<VersionRow>(
          this.sql.exec(
            "SELECT * FROM versions WHERE superseded_at_ms IS NOT NULL AND superseded_at_ms < ? LIMIT ?",
            nowMs - settings.version_retention_days * DAY_MS,
            GC_BATCH,
          ),
        );
        for (const version of expired) this.dropVersion(version, nowMs);

        const changes: ChangeRecord[] = [];
        let bytes = 0;
        let full = false;
        batch: for (let budget = GC_BATCH; budget > 0; ) {
          const due = rows<NodeRow>(
            this.sql.exec(
              `SELECT * FROM nodes n WHERE status = 2 AND trashed_at_ms < ?
                 AND NOT EXISTS (SELECT 1 FROM nodes c WHERE c.parent_id = n.node_id) LIMIT ?`,
              nowMs - settings.trash_retention_days * DAY_MS,
              budget,
            ),
          );
          if (due.length === 0) break;
          for (const node of due) {
            const intents = readIntents(this.sql, node.node_id);
            const held = trashHeld(this.sql, node.trash_group);
            const change = create(ChangeRecordSchema, { change: { case: "purged", value: purgedRecord(node.node_id, node.parent_id, intents, held) } });
            bytes += toBinary(ChangeRecordSchema, change).length;
            if (changes.length > 0 && bytes > PURGE_BYTES) {
              full = true;
              break batch;
            }
            budget--;
            for (const version of rows<VersionRow>(this.sql.exec("SELECT * FROM versions WHERE node_id = ?", node.node_id))) {
              this.dropVersion(version, nowMs);
            }
            this.sql.exec(
              "INSERT OR REPLACE INTO tombstones (node_id, parent_id, name, intents, held) VALUES (?, ?, ?, ?, ?)",
              node.node_id,
              node.parent_id,
              node.name,
              JSON.stringify(intents),
              JSON.stringify(held),
            );
            this.sql.exec("DELETE FROM field_intents WHERE node_id = ?", node.node_id);
            this.sql.exec("DELETE FROM nodes WHERE node_id = ?", node.node_id);
            changes.push(change);
          }
        }
        this.sql.exec("DELETE FROM trash_held WHERE trash_group NOT IN (SELECT trash_group FROM nodes WHERE trash_group IS NOT NULL)");
        let record: CommitRecord | undefined;
        if (changes.length > 0) {
          const seq = BigInt(this.headSeq + 1);
          const committedAtMs = this.stampAfterHead();
          record = create(CommitRecordSchema, { seq, commitId: uuidv7(committedAtMs), committedAtMs: BigInt(committedAtMs), changes });
          this.appendCommit(record, create(CommitResultSchema, { seq, changes, committedAtMs: BigInt(committedAtMs) }), null);
          noteCommitted(this.sql, record);
        }
        return { expiredVersions: expired.length, purgedNodes: changes.length, full, record };
      }));
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
      const { expiredVersions, purgedNodes, full } = purge;
      return {
        expiredVersions,
        purgedNodes,
        deletedChunks,
        sweptChunks,
        more: expiredVersions === GC_BATCH || purgedNodes === GC_BATCH || full || marked === GC_BATCH || sweeping,
      };
    }

    async rewindInfo(restorePointMs: number): Promise<{ historyId: string; headSeq: number; descriptorDurable: boolean; bookmark: string | undefined }> {
      this.dropLeases();
      await this.bookmarking;
      const approximate = await this.pitr.bookmarkForTime(restorePointMs);
      const exact = this.sql
        .exec(
          `SELECT bookmark FROM commit_bookmarks WHERE from_ms <= ? AND from_ms >= ? AND seq =
             (SELECT COALESCE(MAX(seq), 0) FROM commits WHERE seq < (SELECT COALESCE(MIN(seq), ?) FROM commits WHERE committed_at_ms > ?))`,
          restorePointMs,
          oldestUsableBookmarkMs(this.clock()),
          Number.MAX_SAFE_INTEGER,
          restorePointMs,
        )
        .toArray()[0]?.bookmark;
      const bookmark = approximate === undefined ? undefined : typeof exact === "string" ? exact : approximate;
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
        this.sql.exec("DELETE FROM commit_bookmarks");
        startExport(this.sql, completed.restoreSeq, args.completedAtMs, true);
        setMeta(this.sql, "rewind_receipt", JSON.stringify(completed));
        return completed;
      });
      this.bookmarkHead(receipt.restoreSeq, receipt.completedAtMs);
      await this.armAlarm();
      await this.bookmarking;
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
        return { ...SHARE_CLOSED, cause: causeOf(error) };
      }
      return checkFence(this.sql, this.spaceId, resolved, fence);
    }

    private async closedForRewind(): Promise<boolean> {
      const spaceId = getMeta(this.sql, "space_id");
      if (spaceId === undefined) return false;
      try {
        return await this.account().isSpaceClosed(String(spaceId), String(getMeta(this.sql, "history_id")));
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
        if (task === "usage") {
          this.usageArmed = false;
          this.sql.exec("DELETE FROM schedule WHERE task = 'usage'");
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
