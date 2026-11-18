// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import {
  type CreateDir,
  type CreateFile,
  type Delete,
  type Move,
  type Op,
  type OpResult,
  type Place,
  OpResult_Code,
  OpResultSchema,
  type Restore,
  type UpdateContent,
} from "../gen/koidrive/v1/commit_pb";
import { type ChangeRecord, ChangeRecordSchema, type ChunkRef, type Content } from "../gen/koidrive/v1/model_pb";
import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import type { Sql } from "../host";
import { toHex, uuidv7 } from "../ids";
import type { OpEffect } from "../policy";
import {
  FIELDS,
  type FieldIntent,
  holdersOf,
  type Incoming,
  type LostChangeRule,
  mintIntentId,
  type OpIntents,
  readIntents,
  resolveIntents,
  type Field,
  writeIntent,
} from "./intents";
import { conflictName, isReservedAtRoot, nameKey, normalizeName } from "./names";
import { NodeKindValue, NodeStatusValue } from "./schema";
import {
  boundMadeOver,
  encodeChunkList,
  expandMadeOver,
  getNode,
  getVersion,
  metaEditDev,
  metaEditRev,
  type NodeRow,
  nodeState,
  PAGE_CHUNKS,
  rows,
  tombstoneOf,
  versionChunks,
  versionContent,
  versionMadeOver,
} from "./store";

export const MAX_DEPTH = 256;

export class CallFailure extends Error {
  constructor(
    readonly code: Error_Code,
    message: string,
    readonly missingChunks: Uint8Array[] = [],
    readonly retryAfterMs = 0,
  ) {
    super(message);
  }
}

export interface Committer {
  deviceId: string;
  userId: string;
  deviceName: string;
}

export interface ApplyContext {
  seq: number;
  commitId: string;
  baseSeq: number;
  nowMs: number;
  committer: Committer;
  rule: LostChangeRule;
  authorize(op: Op, effect: OpEffect): boolean;
}

type Reject = "CYCLE" | "NOT_FOUND" | "INVALID_NAME" | "PARENT_NOT_DIR" | "ID_EXISTS" | "ROOT" | "NOT_FILE";

interface DeleteView {
  baseSeq: number;
  unseen: Set<string>;
  held: Set<string>;
  seenContent: Set<string>;
}

interface LostContent {
  node: NodeRow;
  content: FieldIntent;
  placeAuthor: string | undefined;
}

function result(code: OpResult_Code, nodeId = "", name = ""): OpResult {
  return create(OpResultSchema, { code, nodeId, name });
}

function rejected(reason: Reject): OpResult {
  return create(OpResultSchema, { code: OpResult_Code.REJECTED, rejectReason: reason });
}

function superseded(): OpResult {
  return result(OpResult_Code.SUPERSEDED);
}

export class CommitApplier {
  private readonly remap = new Map<string, string>();
  private readonly touched = new Set<string>();
  private changed = new Set<string>();
  private created = new Set<string>();
  private parentBefore = new Map<string, string | null>();
  private readonly trashed = new Set<string>();
  private intents!: OpIntents;

  constructor(
    private readonly sql: Sql,
    private readonly ctx: ApplyContext,
  ) {}

  apply(ops: readonly Op[]): { results: OpResult[]; changes: ChangeRecord[] } {
    const results = ops.map((op, index) => {
      this.changed = new Set();
      this.created = new Set();
      this.parentBefore = new Map();
      this.intents = resolveIntents(op, { deviceId: this.dev, seq: this.ctx.seq, nowMs: this.ctx.nowMs, rule: this.ctx.rule });
      const outcome = this.applyOp(op, index);
      if (!this.ctx.authorize(op, this.effect(op))) throw new CallFailure(Error_Code.FORBIDDEN, `op ${index} is not allowed for this device`);
      return outcome;
    });
    const changes = [...this.touched].map((nodeId) =>
      create(ChangeRecordSchema, { change: { case: "node", value: nodeState(this.sql, getNode(this.sql, nodeId)!) } }),
    );
    return { results, changes };
  }

  private effect(op: Op): OpEffect {
    const { changed, created, parentBefore } = this;
    const { op: named } = op;
    const ids =
      named.case === "createDir" || named.case === "createFile"
        ? [named.value.nodeId, named.value.parentId]
        : named.case === "move"
          ? [named.value.nodeId, named.value.newParentId]
          : named.case === "restore"
            ? [named.value.nodeId, named.value.targetParentId]
            : named.case
              ? [named.value.nodeId]
              : [];
    const parentNow = (nodeId: string) => getNode(this.sql, nodeId)?.parent_id ?? null;
    const ancestry = (nodeId: string, parentOf: (id: string) => string | null): string[] => {
      const chain: string[] = [];
      for (let parent = parentOf(nodeId); parent && chain.length <= MAX_DEPTH; parent = parentOf(parent)) chain.unshift(parent);
      return chain;
    };
    return {
      nodes: new Set(ids.filter((id) => id !== "").map((id) => this.id(id))),
      changed,
      created,
      ancestryBefore: (nodeId) =>
        created.has(this.id(nodeId))
          ? undefined
          : ancestry(this.id(nodeId), (id) => (parentBefore.has(id) ? parentBefore.get(id)! : parentNow(id))),
      ancestryAfter: (nodeId) => ancestry(this.id(nodeId), parentNow),
      subtree: (nodeId) =>
        rows<{ node_id: string }>(
          this.sql.exec(
            `WITH RECURSIVE sub(node_id) AS (
               SELECT node_id FROM nodes WHERE parent_id = ? UNION ALL SELECT n.node_id FROM nodes n JOIN sub ON n.parent_id = sub.node_id)
             SELECT node_id FROM sub`,
            this.id(nodeId),
          ),
        ).map((row) => row.node_id),
    };
  }

  private touch(nodeId: string, previousParent?: string | null): void {
    this.touched.add(nodeId);
    this.changed.add(nodeId);
    if (previousParent !== undefined && !this.parentBefore.has(nodeId)) this.parentBefore.set(nodeId, previousParent);
  }

  trashedFiles(): number {
    let count = 0;
    for (const nodeId of this.trashed) if (getNode(this.sql, nodeId)?.status === NodeStatusValue.TRASHED) count++;
    return count;
  }

  private applyOp(op: Op, index: number): OpResult {
    switch (op.op.case) {
      case "createDir":
        return this.createDir(op.op.value);
      case "createFile":
        return this.createFile(op.op.value);
      case "updateContent":
        return this.updateContent(op.op.value.nodeId, op.op.value.content!, op.op.value.content!.chunks, op.op.value);
      case "move":
        return this.move(op.op.value);
      case "delete":
        return this.delete(op.op.value, index);
      case "restore":
        return this.restore(op.op.value);
      default:
        throw new CallFailure(Error_Code.INVALID_ARGUMENT, `op ${index} is empty`);
    }
  }

  private id(nodeId: string): string {
    return this.remap.get(nodeId) ?? nodeId;
  }

  private node(nodeId: string): NodeRow | undefined {
    return getNode(this.sql, this.id(nodeId));
  }

  private get dev(): string {
    return this.ctx.committer.deviceId;
  }

  private get client(): boolean {
    return this.intents.client;
  }

  private incoming(field: Field): Incoming {
    return this.intents.fields[field];
  }

  private setIntent(nodeId: string, field: Field, intent: FieldIntent = this.incoming(field)): void {
    writeIntent(this.sql, nodeId, field, { id: intent.id, author: intent.author, carried: intent.carried, appliedAtMs: intent.appliedAtMs, seq: this.ctx.seq });
  }

  private minted(author: string, carried: boolean): FieldIntent {
    return { id: mintIntentId(this.ctx.nowMs), author, carried, appliedAtMs: this.ctx.nowMs, seq: this.ctx.seq };
  }

  private authorsReplayUnderB(intent: Incoming): boolean {
    return this.client && this.ctx.rule === "B" && intent.replay && !intent.carried;
  }

  private holdsIntents(node: NodeRow, fields: readonly Field[]): boolean {
    const held = readIntents(this.sql, node.node_id);
    return fields.every((field) => held[field]?.id === this.incoming(field).id);
  }

  private same(node: NodeRow, fields: readonly Field[]): OpResult {
    if (this.ctx.rule === "B") this.claim(node, fields);
    return result(OpResult_Code.APPLIED, "", node.name);
  }

  private claim(node: NodeRow, fields: readonly Field[]): void {
    const held = readIntents(this.sql, node.node_id);
    const claimed = fields.filter((field) => held[field]?.carried && this.incoming(field).author === this.dev);
    if (claimed.length === 0) return;
    for (const field of claimed) writeIntent(this.sql, node.node_id, field, { ...held[field]!, carried: false });
    if (claimed.includes("content")) this.sql.exec("UPDATE nodes SET edit_rev = ?, edit_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, node.node_id);
    if (claimed.some((field) => field !== "content")) {
      this.sql.exec("UPDATE nodes SET meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, node.node_id);
    }
    this.touch(node.node_id);
  }

  private heldElsewhere(nodeId: string, fields: readonly Field[]): boolean {
    if (!this.client) return false;
    for (const field of fields) {
      const intent = this.incoming(field);
      if (!intent.replay) continue;
      const holder = holdersOf(this.sql, intent.id).find((found) => found.node_id !== nodeId);
      if (!holder) continue;
      this.touch(holder.node_id);
      return true;
    }
    return false;
  }

  private overHeld(node: NodeRow, field: "place" | "status"): boolean {
    const over = this.intents.over;
    return over === undefined || readIntents(this.sql, node.node_id)[field]?.id === over;
  }

  private trashKnows(node: NodeRow, intentId: string): boolean {
    const held = readIntents(this.sql, node.node_id);
    if (FIELDS.some((field) => held[field]?.id === intentId) || node.status_before === intentId) return true;
    return this.sql.exec("SELECT 1 FROM trash_held WHERE trash_group = ? AND intent_id = ?", node.trash_group, intentId).toArray().length > 0;
  }

  private purgedReplay(nodeId: string, create = false): ReturnType<typeof tombstoneOf> {
    if (!create && !FIELDS.some((field) => this.incoming(field).replay || this.incoming(field).carried)) return undefined;
    return tombstoneOf(this.sql, nodeId);
  }

  private root(): NodeRow {
    return rows<NodeRow>(this.sql.exec("SELECT * FROM nodes WHERE parent_id IS NULL LIMIT 1"))[0]!;
  }

  private nearestLive(nodeId: string | null): NodeRow {
    for (let cur = nodeId, steps = 0; cur && steps <= MAX_DEPTH; steps++) {
      const node = getNode(this.sql, cur);
      if (node?.status === NodeStatusValue.LIVE) return node;
      cur = node ? node.parent_id : (tombstoneOf(this.sql, cur)?.parent_id ?? null);
    }
    return this.root();
  }

  private holds(node: NodeRow, content: Content): boolean {
    const current = node.cur_version_id ? getVersion(this.sql, node.cur_version_id) : undefined;
    return (
      current !== undefined &&
      BigInt(current.size) === content.size &&
      (current.executable === 1) === content.executable &&
      toHex(new Uint8Array(current.manifest_digest)) === toHex(content.manifestDigest)
    );
  }

  private unseenContent(node: NodeRow): boolean {
    return node.content_rev > this.ctx.baseSeq && node.content_dev !== this.dev;
  }

  private unseenEdit(node: NodeRow): boolean {
    return node.edit_rev > this.ctx.baseSeq && node.edit_dev !== this.dev;
  }

  private unseenMeta(node: NodeRow): boolean {
    return metaEditRev(node) > this.ctx.baseSeq && metaEditDev(node) !== this.dev;
  }

  private unseenParent(node: NodeRow): boolean {
    return node.parent_rev > this.ctx.baseSeq && node.parent_dev !== this.dev;
  }

  private depth(node: NodeRow): number {
    let depth = 0;
    for (let cur: NodeRow | undefined = node; cur?.parent_id; cur = getNode(this.sql, cur.parent_id)) {
      if (++depth > MAX_DEPTH) break;
    }
    return depth;
  }

  private checkDepth(parent: NodeRow): void {
    if (this.depth(parent) + 1 > MAX_DEPTH) {
      throw new CallFailure(Error_Code.LIMIT_EXCEEDED, `path depth would exceed ${MAX_DEPTH}`);
    }
  }

  private checkSubtreeDepth(node: NodeRow, parent: NodeRow): void {
    const newDepth = this.depth(parent) + 1;
    if (newDepth <= this.depth(node)) return;
    const height = this.sql
      .exec(
        `WITH RECURSIVE sub(id, d) AS (
           SELECT ?, 0 UNION ALL SELECT n.node_id, sub.d + 1 FROM nodes n JOIN sub ON n.parent_id = sub.id WHERE sub.d <= ?)
         SELECT MAX(d) AS height FROM sub`,
        node.node_id,
        MAX_DEPTH - newDepth,
      )
      .one().height;
    if (newDepth + Number(height) > MAX_DEPTH) {
      throw new CallFailure(Error_Code.LIMIT_EXCEEDED, `path depth would exceed ${MAX_DEPTH}`);
    }
  }

  private liveWithName(parentId: string, key: string, excludeId: string): NodeRow | undefined {
    return rows<NodeRow>(
      this.sql.exec(
        "SELECT * FROM nodes WHERE parent_id = ? AND name_key = ? AND status = 1 AND node_id != ? LIMIT 1",
        parentId,
        key,
        excludeId,
      ),
    )[0];
  }

  private pickName(parentId: string, desired: string, excludeId: string): { name: string; renamed: boolean } {
    const atRoot = !getNode(this.sql, parentId)?.parent_id;
    let name = desired;
    for (let attempt = 1; this.liveWithName(parentId, nameKey(name), excludeId) || (atRoot && isReservedAtRoot(name)); attempt++) {
      name = conflictName(desired, this.ctx.committer.deviceName, this.ctx.nowMs, attempt);
    }
    return { name, renamed: name !== desired };
  }

  private pickConflictCopyName(parentId: string, original: string): string {
    for (let attempt = 1; ; attempt++) {
      const name = conflictName(original, this.ctx.committer.deviceName, this.ctx.nowMs, attempt);
      if (!this.liveWithName(parentId, nameKey(name), "")) return name;
    }
  }

  private reviveOne(node: NodeRow, parentId: string, desiredName: string, parentDev: string): { name: string; renamed: boolean } {
    const picked = this.pickName(parentId, desiredName, node.node_id);
    this.sql.exec(
      `UPDATE nodes SET status = 1, parent_id = ?, parent_rev = ?, parent_dev = ?, name = ?, name_key = ?, trash_group = NULL,
         trashed_at_ms = NULL, status_before = NULL, meta_rev = ?, meta_dev = ?, meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?`,
      parentId,
      this.ctx.seq,
      parentDev,
      picked.name,
      nameKey(picked.name),
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      node.node_id,
    );
    this.setIntent(node.node_id, "status");
    if (parentId !== node.parent_id || picked.name !== node.name) this.setIntent(node.node_id, "place");
    this.touch(node.node_id, node.parent_id);
    return picked;
  }

  private reviveChain(nodeId: string): void {
    const chain: NodeRow[] = [];
    for (let cur = getNode(this.sql, nodeId); cur && cur.status === NodeStatusValue.TRASHED; cur = getNode(this.sql, cur.parent_id ?? "")) {
      chain.push(cur);
    }
    for (const node of chain.reverse()) this.reviveOne(node, node.parent_id!, node.name, "");
  }

  private reviveGroup(parentId: string, group: string): void {
    const children = rows<NodeRow>(
      this.sql.exec("SELECT * FROM nodes WHERE parent_id = ? AND status = 2 AND trash_group = ?", parentId, group),
    );
    for (const child of children) {
      this.reviveOne(child, parentId, child.name, "");
      this.reviveGroup(child.node_id, group);
    }
  }

  private reviveTarget(node: NodeRow): { node: NodeRow; renamed: boolean } {
    if (node.status !== NodeStatusValue.TRASHED) return { node, renamed: false };
    const group = node.trash_group!;
    this.reviveChain(node.parent_id!);
    const { renamed } = this.reviveOne(node, node.parent_id!, node.name, "");
    this.reviveGroup(node.node_id, group);
    return { node: getNode(this.sql, node.node_id)!, renamed };
  }

  private placeIn(parentId: string): NodeRow | undefined {
    const parent = this.node(parentId);
    if (parent) return parent;
    const tombstone = tombstoneOf(this.sql, this.id(parentId));
    return tombstone && this.nearestLive(tombstone.parent_id);
  }

  private parentRow(parentId: string): NodeRow | Reject {
    const parent = this.placeIn(parentId);
    if (!parent) return "NOT_FOUND";
    if (parent.kind !== NodeKindValue.DIR) return "PARENT_NOT_DIR";
    this.checkDepth(parent);
    return parent;
  }

  private parentFor(parentId: string): NodeRow | Reject {
    const parent = this.parentRow(parentId);
    if (typeof parent !== "string" && parent.status === NodeStatusValue.TRASHED) this.reviveChain(parent.node_id);
    return parent;
  }

  private insertNode(nodeId: string, parentId: string, name: string, kind: number, versionId: string | null, parentDev: string): void {
    this.sql.exec(
      `INSERT INTO nodes (node_id, parent_id, parent_rev, parent_dev, name, name_key, kind, status, content_rev, content_dev, edit_rev,
         edit_dev, meta_rev, meta_dev, meta_edit_rev, meta_edit_dev, cur_version_id) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      nodeId,
      parentId,
      this.ctx.seq,
      parentDev,
      name,
      nameKey(name),
      kind,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      versionId,
    );
    this.sql.exec("DELETE FROM tombstones WHERE node_id = ?", nodeId);
    this.touch(nodeId);
    this.created.add(nodeId);
  }

  private insertVersion(nodeId: string, content: Content, chunks: readonly ChunkRef[], madeOver: readonly string[] = []): string {
    const versionId = uuidv7(this.ctx.nowMs);
    const paged = chunks.length > PAGE_CHUNKS;
    this.sql.exec(
      `INSERT INTO versions (version_id, node_id, seq, size, mtime_ns, executable, chunk_count, manifest_digest, chunker,
         chunks, device_id, user_id, created_at_ms, made_over) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      versionId,
      nodeId,
      this.ctx.seq,
      Number(content.size),
      String(content.mtimeNs),
      content.executable ? 1 : 0,
      chunks.length,
      content.manifestDigest,
      content.chunker,
      paged ? null : encodeChunkList(chunks),
      this.dev,
      this.ctx.committer.userId,
      this.ctx.nowMs,
      boundMadeOver(madeOver).join(","),
    );
    for (let page = 0; paged && page * PAGE_CHUNKS < chunks.length; page++) {
      this.sql.exec(
        "INSERT INTO version_chunk_pages (version_id, page, chunks) VALUES (?, ?, ?)",
        versionId,
        page,
        encodeChunkList(chunks.slice(page * PAGE_CHUNKS, (page + 1) * PAGE_CHUNKS)),
      );
    }
    const distinct = new Map(chunks.map((chunk) => [chunk.hash.join(","), chunk.hash]));
    for (const hash of distinct.values()) {
      this.sql.exec("UPDATE chunks SET refcount = refcount + 1, state = 2, zero_since_ms = NULL WHERE hash = ?", hash);
    }
    return versionId;
  }

  private conflictCopy(parent: NodeRow, original: string, content: Content, chunks: readonly ChunkRef[], madeOver: readonly string[], intent: FieldIntent): OpResult {
    const home = this.depth(parent) < MAX_DEPTH ? parent : getNode(this.sql, parent.parent_id!)!;
    const copyId = uuidv7(this.ctx.nowMs);
    const name = this.pickConflictCopyName(home.node_id, original);
    const versionId = this.insertVersion(copyId, content, chunks, madeOver);
    this.insertNode(copyId, home.node_id, name, NodeKindValue.FILE, versionId, "");
    const [place, status] = intent.carried ? [this.minted(intent.author, true), this.minted(intent.author, true)] : [this.minted(this.dev, false), this.minted(this.dev, false)];
    this.setIntent(copyId, "content", intent);
    this.setIntent(copyId, "place", place);
    this.setIntent(copyId, "status", status);
    if (intent.carried) this.sql.exec("UPDATE nodes SET copy_mark = ? WHERE node_id = ?", place.id + status.id, copyId);
    return result(OpResult_Code.CONFLICT_COPY, copyId, name);
  }

  private copyOf(node: NodeRow, parent: NodeRow, intent: FieldIntent): void {
    const version = getVersion(this.sql, node.cur_version_id!)!;
    this.conflictCopy(parent, node.name, versionContent(this.sql, version, false), versionChunks(this.sql, version), versionMadeOver(version), intent);
  }

  private mark(nodeId: string, pendingFor: string | null): void {
    const key = pendingFor === null ? null : nameKey(pendingFor);
    this.sql.exec(
      "UPDATE nodes SET pending_for = CASE WHEN name_key = ? THEN NULL ELSE ? END, pending_by = CASE WHEN name_key = ? THEN NULL ELSE ? END WHERE node_id = ?",
      key,
      pendingFor,
      key,
      pendingFor === null ? null : this.ctx.committer.deviceId,
      nodeId,
    );
  }

  private sameCreate(existing: NodeRow, parentId: string, name: string, content: Content | undefined): boolean {
    const fields: Field[] = content ? ["place", "content", "status"] : ["place", "status"];
    return (
      this.client &&
      existing.status === NodeStatusValue.LIVE &&
      existing.parent_id === this.id(parentId) &&
      existing.name === name &&
      (content === undefined || this.holds(existing, content)) &&
      this.holdsIntents(existing, fields)
    );
  }

  private createIntoTrash(parent: NodeRow, name: string, content: { value: Content; madeOver: string[] } | undefined): OpResult | undefined {
    if (!this.client || parent.status !== NodeStatusValue.TRASHED) return undefined;
    const place = this.incoming("place");
    const fields: Field[] = content ? ["place", "content", "status"] : ["place", "status"];
    if (this.authorsReplayUnderB(place) && fields.some((field) => this.trashKnows(parent, this.incoming(field).id))) return superseded();
    if (!place.carried) return undefined;
    const intent = this.incoming("content");
    if (!content || this.trashKnows(parent, intent.id)) return superseded();
    return this.conflictCopy(this.nearestLive(parent.node_id), name, content.value, content.value.chunks, content.madeOver, intent);
  }

  private createDir(op: CreateDir): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    const pendingFor = pendingName(op.pendingFor);
    if (pendingFor === undefined) return rejected("INVALID_NAME");
    const existing = getNode(this.sql, op.nodeId);
    if (existing) return this.sameCreate(existing, op.parentId, name, undefined) ? this.same(existing, ["place", "status"]) : rejected("ID_EXISTS");
    if (this.purgedReplay(op.nodeId, true)) return superseded();
    if (this.heldElsewhere(op.nodeId, ["place", "status"])) return superseded();
    const found = this.parentRow(op.parentId);
    if (typeof found === "string") return rejected(found);
    if (!found.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const trashed = this.createIntoTrash(found, name, undefined);
    if (trashed) return trashed;
    const parent = this.parentFor(op.parentId) as NodeRow;
    const existingDir = this.liveWithName(parent.node_id, nameKey(name), "");
    if (existingDir?.kind === NodeKindValue.DIR) {
      this.remap.set(op.nodeId, existingDir.node_id);
      return result(OpResult_Code.MERGED_INTO, existingDir.node_id, existingDir.name);
    }
    const picked = this.pickName(parent.node_id, name, "");
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.DIR, null, this.dev);
    this.setIntent(op.nodeId, "place");
    this.setIntent(op.nodeId, "status");
    this.mark(op.nodeId, pendingFor);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private createFile(op: CreateFile): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    const existing = getNode(this.sql, op.nodeId);
    if (existing) return this.sameCreate(existing, op.parentId, name, op.content!) ? this.same(existing, ["place", "content", "status"]) : rejected("ID_EXISTS");
    const bases = expandMadeOver(this.sql, op.nodeId, op.madeOver);
    const madeOver = boundMadeOver(op.sourceVersionId ? [op.sourceVersionId, ...bases] : bases);
    const tombstone = this.purgedReplay(op.nodeId, true);
    if (tombstone) {
      const intent = this.incoming("content");
      if (tombstone.knows.has(intent.id) || this.heldElsewhere(op.nodeId, ["content"])) return superseded();
      return this.conflictCopy(this.nearestLive(tombstone.parent_id), name, op.content!, op.content!.chunks, madeOver, intent);
    }
    if (this.heldElsewhere(op.nodeId, ["place", "content", "status"])) return superseded();
    const found = this.parentRow(op.parentId);
    if (typeof found === "string") return rejected(found);
    if (!found.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const trashed = this.createIntoTrash(found, name, { value: op.content!, madeOver });
    if (trashed) return trashed;
    const parent = this.parentFor(op.parentId) as NodeRow;
    const picked = this.pickName(parent.node_id, name, "");
    const versionId = this.insertVersion(op.nodeId, op.content!, op.content!.chunks, madeOver);
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.FILE, versionId, this.dev);
    for (const field of FIELDS) this.setIntent(op.nodeId, field);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private updateContent(nodeId: string, content: Content, chunks: readonly ChunkRef[], ancestry?: UpdateContent): OpResult {
    const found = this.node(nodeId);
    const intent = this.incoming("content");
    const sourceVersionId = ancestry?.sourceVersionId ?? "";
    if (!found) {
      const tombstone = this.purgedReplay(this.id(nodeId));
      if (!tombstone) return rejected("NOT_FOUND");
      if (tombstone.knows.has(intent.id) || this.heldElsewhere(this.id(nodeId), ["content"])) return superseded();
      const parent = this.nearestLive(tombstone.parent_id);
      return this.conflictCopy(parent, tombstone.name, content, chunks, boundMadeOver(ancestry?.madeOver ?? []), intent);
    }
    if (found.kind !== NodeKindValue.FILE) return rejected("NOT_FILE");
    if (this.client && this.holdsIntents(found, ["content"]) && this.holds(found, content)) return this.same(found, ["content"]);
    if (this.heldElsewhere(found.node_id, ["content"])) return superseded();
    const resend = ancestry?.resend ?? false;
    const replay = resend || (this.client && intent.replay);
    const yields = resend || (this.client && intent.carried);
    if (found.status === NodeStatusValue.TRASHED) {
      if (this.client && (intent.carried || this.authorsReplayUnderB(intent))) {
        if (this.trashKnows(found, intent.id)) return superseded();
        if (intent.carried) return this.conflictCopy(this.nearestLive(found.parent_id), found.name, content, chunks, boundMadeOver(ancestry?.madeOver ?? []), intent);
      }
      if (resend) return superseded();
    }
    const current = found.cur_version_id ? getVersion(this.sql, found.cur_version_id) : undefined;
    if (replay && sourceVersionId && current && versionMadeOver(current).includes(sourceVersionId)) {
      return superseded();
    }
    const bases = expandMadeOver(this.sql, found.node_id, ancestry?.madeOver ?? []);
    const overCurrent = current !== undefined && bases.includes(current.version_id);
    const byAncestry = this.client && bases.length > 0;
    const stale = (node: NodeRow) => (byAncestry ? !overCurrent : this.unseenContent(node) && !overCurrent);
    if ((byAncestry ? !overCurrent : this.unseenContent(found)) && this.holds(found, content)) {
      if (replay && sourceVersionId && current) {
        const madeOver = boundMadeOver([sourceVersionId, ...versionMadeOver(current)]);
        this.sql.exec("UPDATE versions SET made_over = ? WHERE version_id = ?", madeOver.join(","), current.version_id);
        this.touch(found.node_id);
      }
      if (yields || found.edit_rev >= found.content_rev) {
        if (this.client) this.touch(found.node_id);
        return result(OpResult_Code.APPLIED, "", found.name);
      }
      const { node, renamed } = this.reviveTarget(found);
      this.sql.exec("UPDATE nodes SET edit_rev = ?, edit_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, node.node_id);
      this.touch(node.node_id);
      return result(renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", node.name);
    }
    const madeOver = boundMadeOver(replay && sourceVersionId ? [sourceVersionId, ...bases] : bases);
    const { node, renamed } = this.reviveTarget(found);
    if (stale(node)) return this.conflictCopy(getNode(this.sql, node.parent_id!)!, node.name, content, chunks, madeOver, intent);
    this.sql.exec("UPDATE versions SET superseded_at_ms = ? WHERE version_id = ?", this.ctx.nowMs, node.cur_version_id);
    const versionId = this.insertVersion(node.node_id, content, chunks, madeOver);
    const [editRev, editDev] = yields ? [node.edit_rev, node.edit_dev] : [this.ctx.seq, this.dev];
    this.sql.exec(
      "UPDATE nodes SET cur_version_id = ?, content_rev = ?, content_dev = ?, edit_rev = ?, edit_dev = ? WHERE node_id = ?",
      versionId,
      this.ctx.seq,
      this.dev,
      editRev,
      editDev,
      node.node_id,
    );
    this.setIntent(node.node_id, "content");
    this.touch(node.node_id);
    return result(renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", node.name);
  }

  private isSelfOrDescendant(candidate: NodeRow, ancestorId: string): boolean {
    for (let cur: NodeRow | undefined = candidate; cur; cur = cur.parent_id ? getNode(this.sql, cur.parent_id) : undefined) {
      if (cur.node_id === ancestorId) return true;
    }
    return false;
  }

  private move(op: Move): OpResult {
    const found = this.node(op.nodeId);
    if (!found) return this.purgedReplay(this.id(op.nodeId)) ? superseded() : rejected("NOT_FOUND");
    if (!found.parent_id) return rejected("ROOT");
    const name = normalizeName(op.newName);
    if (!name) return rejected("INVALID_NAME");
    const pendingFor = pendingName(op.pendingFor);
    if (pendingFor === undefined) return rejected("INVALID_NAME");
    const target = this.placeIn(op.newParentId);
    if (!target) return rejected("NOT_FOUND");
    if (target.kind !== NodeKindValue.DIR) return rejected("PARENT_NOT_DIR");
    if (this.isSelfOrDescendant(target, found.node_id)) return rejected("CYCLE");
    if (!target.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const place = this.incoming("place");
    const live = found.status === NodeStatusValue.LIVE;
    if (this.client) {
      if (found.parent_id === target.node_id && found.name === name && this.holdsIntents(found, ["place"])) return this.same(found, ["place"]);
      if (this.heldElsewhere(found.node_id, ["place"])) return superseded();
      if (!this.overHeld(found, "place")) return superseded();
      const trash = !live ? found : target.status === NodeStatusValue.TRASHED ? target : undefined;
      if (trash && (place.carried || (this.authorsReplayUnderB(place) && this.trashKnows(trash, place.id)))) return superseded();
    }
    if (op.from && !this.isAt(found, op.from, target)) return superseded();
    const yields = op.from !== undefined || (this.client && place.carried);
    if (live && found.parent_id === target.node_id && found.name === name) {
      if (yields) return result(OpResult_Code.APPLIED, "", name);
      if (metaEditRev(found) < found.meta_rev) {
        this.sql.exec(
          "UPDATE nodes SET parent_rev = ?, parent_dev = ?, meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?",
          this.ctx.seq,
          this.dev,
          this.ctx.seq,
          this.dev,
          found.node_id,
        );
        this.setIntent(found.node_id, "place");
        this.touch(found.node_id);
      } else if (this.unseenParent(found)) {
        this.sql.exec("UPDATE nodes SET parent_rev = ?, parent_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, found.node_id);
      }
      return result(OpResult_Code.APPLIED, "", name);
    }
    const parent = this.parentFor(target.node_id) as NodeRow;
    this.checkSubtreeDepth(found, parent);
    if (!live) {
      const group = found.trash_group!;
      const picked = this.reviveOne(found, parent.node_id, name, this.dev);
      this.setIntent(found.node_id, "place");
      this.reviveGroup(found.node_id, group);
      this.mark(found.node_id, pendingFor);
      return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
    }
    const picked = this.pickName(parent.node_id, name, found.node_id);
    const placed = found.parent_id !== parent.node_id || (!yields && this.unseenParent(found));
    this.sql.exec(
      `UPDATE nodes SET parent_id = ?, parent_rev = ?, parent_dev = ?, name = ?, name_key = ?, meta_rev = ?, meta_dev = ?, meta_edit_rev = ?,
         meta_edit_dev = ? WHERE node_id = ?`,
      parent.node_id,
      placed ? this.ctx.seq : found.parent_rev,
      placed ? this.dev : found.parent_dev,
      picked.name,
      nameKey(picked.name),
      this.ctx.seq,
      this.dev,
      yields ? metaEditRev(found) : this.ctx.seq,
      yields ? metaEditDev(found) : this.dev,
      found.node_id,
    );
    this.setIntent(found.node_id, "place");
    this.mark(found.node_id, pendingFor);
    this.touch(found.node_id, found.parent_id);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private isAt(node: NodeRow, from: Place, target: NodeRow): boolean {
    return (
      node.status === NodeStatusValue.LIVE &&
      target.status === NodeStatusValue.LIVE &&
      node.parent_id === this.id(from.parentId) &&
      node.name === normalizeName(from.name)
    );
  }

  private liveChildren(node: NodeRow): NodeRow[] {
    return rows<NodeRow>(this.sql.exec("SELECT * FROM nodes WHERE parent_id = ? AND status = 1", node.node_id));
  }

  private trashNode(node: NodeRow, group: string): void {
    this.sql.exec(
      `UPDATE nodes SET status = 2, trash_group = ?, trashed_at_ms = ?, meta_rev = ?, meta_dev = ?, meta_edit_rev = ?, meta_edit_dev = ?,
         status_before = ? WHERE node_id = ?`,
      group,
      this.ctx.nowMs,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      readIntents(this.sql, node.node_id).status?.id ?? null,
      node.node_id,
    );
    this.setIntent(node.node_id, "status");
    this.touch(node.node_id);
    if (node.kind === NodeKindValue.FILE) this.trashed.add(node.node_id);
  }

  private trashSubtree(node: NodeRow, group: string): boolean {
    if (this.unseenParent(node)) return true;
    let keep = this.unseenEdit(node) || this.unseenMeta(node);
    for (const child of this.liveChildren(node)) keep = this.trashSubtree(child, group) || keep;
    if (keep) return true;
    this.trashNode(node, group);
    return false;
  }

  private inView(view: DeleteView, intent: FieldIntent): boolean {
    return view.held.has(intent.id) || (intent.seq <= view.baseSeq && !view.unseen.has(intent.id));
  }

  private unseenBy(view: DeleteView, intent: FieldIntent | undefined): boolean {
    return (
      intent !== undefined &&
      !intent.carried &&
      !this.inView(view, intent) &&
      (view.unseen.has(intent.id) || (intent.author !== this.dev && intent.seq > view.baseSeq))
    );
  }

  private trashByView(target: NodeRow, group: string, view: DeleteView): boolean {
    const lost: LostContent[] = [];
    const walk = (node: NodeRow): boolean => {
      const held = readIntents(this.sql, node.node_id);
      if (this.unseenBy(view, held.place) && node.parent_rev >= held.place!.seq) return true;
      let keep = FIELDS.some((field) => this.unseenBy(view, held[field]));
      for (const child of this.liveChildren(node)) keep = walk(child) || keep;
      if (keep) return true;
      const content = held.content;
      if (content && this.inView(view, content)) view.seenContent.add(content.id);
      if (node.kind === NodeKindValue.FILE && content && !this.inView(view, content) && (held.place?.carried || content.carried)) {
        lost.push({ node, content, placeAuthor: held.place?.carried ? held.place.author : undefined });
        if (held.place?.carried) return node.node_id === target.node_id;
      }
      this.trashNode(node, group);
      return false;
    };
    const kept = walk(target);
    for (const { node, content, placeAuthor } of lost) {
      if (placeAuthor === undefined) {
        this.copyOf(node, this.nearestLive(node.parent_id), content);
        continue;
      }
      if (node.node_id === target.node_id || getNode(this.sql, node.parent_id!)?.status === NodeStatusValue.LIVE) continue;
      const parent = this.nearestLive(node.parent_id);
      const picked = this.pickName(parent.node_id, node.name, node.node_id);
      this.sql.exec(
        "UPDATE nodes SET parent_id = ?, parent_rev = ?, parent_dev = '', name = ?, name_key = ?, meta_rev = ?, meta_dev = ? WHERE node_id = ?",
        parent.node_id,
        this.ctx.seq,
        picked.name,
        nameKey(picked.name),
        this.ctx.seq,
        this.dev,
        node.node_id,
      );
      this.setIntent(node.node_id, "place", this.minted(placeAuthor, true));
      this.touch(node.node_id, node.parent_id);
    }
    return kept;
  }

  private trashCopies(intentIds: Iterable<string>, group: string): void {
    for (const intentId of intentIds) {
      for (const holder of holdersOf(this.sql, intentId)) {
        if (holder.field !== "content") continue;
        const copy = getNode(this.sql, holder.node_id);
        if (copy?.status !== NodeStatusValue.LIVE || !copy.copy_mark) continue;
        const held = readIntents(this.sql, copy.node_id);
        if (copy.copy_mark === `${held.place?.id}${held.status?.id}`) this.trashNode(copy, group);
      }
    }
  }

  private mergeHeld(node: NodeRow, held: readonly string[]): void {
    const group = node.trash_group!;
    let added = false;
    for (const intentId of held) {
      added = this.sql.exec("INSERT INTO trash_held (trash_group, intent_id) VALUES (?, ?) ON CONFLICT DO NOTHING RETURNING intent_id", group, intentId).toArray().length > 0 || added;
    }
    if (added) {
      for (const member of rows<{ node_id: string }>(this.sql.exec("SELECT node_id FROM nodes WHERE trash_group = ?", group))) {
        this.sql.exec("UPDATE nodes SET meta_rev = ?, meta_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, member.node_id);
        this.touch(member.node_id);
      }
    }
    this.trashCopies(held, group);
  }

  private delete(op: Delete, index: number): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return this.purgedReplay(this.id(op.nodeId)) ? superseded() : rejected("NOT_FOUND");
    if (!node.parent_id) return rejected("ROOT");
    const held = op.held.map(toHex);
    const trashed = node.status === NodeStatusValue.TRASHED;
    if (this.client) {
      if (trashed && this.holdsIntents(node, ["status"])) {
        this.mergeHeld(node, held);
        return this.same(node, ["status"]);
      }
      if (trashed && this.intents.over !== undefined && node.status_before === this.intents.over) {
        this.mergeHeld(node, held);
        return result(OpResult_Code.APPLIED);
      }
      if (!this.overHeld(node, "status")) return superseded();
    }
    if (trashed) {
      if (this.client) this.mergeHeld(node, held);
      return result(OpResult_Code.APPLIED);
    }
    const required = op.requires && this.node(op.requires.nodeId);
    if (
      op.requires &&
      !(
        required?.status === NodeStatusValue.LIVE &&
        BigInt(required.content_rev) === op.requires.contentRev &&
        BigInt(required.meta_rev) === op.requires.metaRev
      )
    ) {
      return superseded();
    }
    const group = `${this.ctx.commitId}:${index}`;
    if (this.client) {
      const view: DeleteView = { baseSeq: this.ctx.baseSeq, unseen: new Set(op.unseen.map(toHex)), held: new Set(held), seenContent: new Set() };
      const kept = this.trashByView(node, group, view);
      if (this.sql.exec("SELECT 1 FROM nodes WHERE trash_group = ? LIMIT 1", group).toArray().length === 0) return superseded();
      for (const intentId of held) this.sql.exec("INSERT INTO trash_held (trash_group, intent_id) VALUES (?, ?) ON CONFLICT DO NOTHING", group, intentId);
      this.trashCopies([...view.held, ...view.seenContent], group);
      if (kept) return superseded();
    } else if (this.trashSubtree(node, group)) return superseded();
    if (required) {
      this.sql.exec("UPDATE nodes SET parent_rev = ?, parent_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, required.node_id);
    }
    return result(OpResult_Code.APPLIED);
  }

  private restore(op: Restore): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return this.purgedReplay(this.id(op.nodeId)) ? superseded() : rejected("NOT_FOUND");
    if (op.versionId) {
      const version = getVersion(this.sql, op.versionId);
      if (!version || version.node_id !== node.node_id) return rejected("NOT_FOUND");
      return this.updateContent(node.node_id, versionContent(this.sql, version, false), versionChunks(this.sql, version));
    }
    if (!node.parent_id) return rejected("ROOT");
    if (node.status !== NodeStatusValue.TRASHED) return result(OpResult_Code.APPLIED, "", node.name);
    const status = this.incoming("status");
    if (this.client && (status.carried || (this.authorsReplayUnderB(status) && this.trashKnows(node, status.id)))) return superseded();
    const name = op.name ? normalizeName(op.name) : node.name;
    if (!name) return rejected("INVALID_NAME");
    const target = this.node(op.targetParentId || node.parent_id);
    if (!target) return rejected("NOT_FOUND");
    if (target.kind !== NodeKindValue.DIR) return rejected("PARENT_NOT_DIR");
    if (this.isSelfOrDescendant(target, node.node_id)) return rejected("CYCLE");
    if (!target.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const parent = this.parentFor(target.node_id) as NodeRow;
    this.checkSubtreeDepth(node, parent);
    const group = node.trash_group!;
    const picked = this.reviveOne(node, parent.node_id, name, this.dev);
    this.reviveGroup(node.node_id, group);
    if (op.name || op.targetParentId) this.mark(node.node_id, null);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }
}

function pendingName(name: string): string | null | undefined {
  return name === "" ? null : normalizeName(name);
}
