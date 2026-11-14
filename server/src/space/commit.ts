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
import { conflictName, isReservedAtRoot, nameKey, normalizeName } from "./names";
import { NodeKindValue, NodeStatusValue } from "./schema";
import {
  encodeChunkList,
  getNode,
  getVersion,
  metaEditDev,
  metaEditRev,
  type NodeRow,
  nodeState,
  PAGE_CHUNKS,
  rows,
  versionChunks,
  versionContent,
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
  authorize(op: Op, effect: OpEffect): boolean;
}

type Reject = "CYCLE" | "NOT_FOUND" | "INVALID_NAME" | "PARENT_NOT_DIR" | "ID_EXISTS" | "ROOT" | "NOT_FILE";

function result(code: OpResult_Code, nodeId = "", name = ""): OpResult {
  return create(OpResultSchema, { code, nodeId, name });
}

function rejected(reason: Reject): OpResult {
  return create(OpResultSchema, { code: OpResult_Code.REJECTED, rejectReason: reason });
}

export class CommitApplier {
  private readonly remap = new Map<string, string>();
  private readonly touched = new Set<string>();
  private changed = new Set<string>();
  private created = new Set<string>();
  private parentBefore = new Map<string, string | null>();
  private readonly trashed = new Set<string>();

  constructor(
    private readonly sql: Sql,
    private readonly ctx: ApplyContext,
  ) {}

  apply(ops: readonly Op[]): { results: OpResult[]; changes: ChangeRecord[] } {
    const results = ops.map((op, index) => {
      this.changed = new Set();
      this.created = new Set();
      this.parentBefore = new Map();
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
        return this.updateContent(op.op.value.nodeId, op.op.value.content!, op.op.value.content!.chunks, op.op.value.resend);
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
         trashed_at_ms = NULL, meta_rev = ?, meta_dev = ?, meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?`,
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

  private parentFor(parentId: string): NodeRow | Reject {
    const parent = this.node(parentId);
    if (!parent) return "NOT_FOUND";
    if (parent.kind !== NodeKindValue.DIR) return "PARENT_NOT_DIR";
    this.checkDepth(parent);
    if (parent.status === NodeStatusValue.TRASHED) this.reviveChain(parent.node_id);
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
    this.touch(nodeId);
    this.created.add(nodeId);
  }

  private insertVersion(nodeId: string, content: Content, chunks: readonly ChunkRef[]): string {
    const versionId = uuidv7(this.ctx.nowMs);
    const paged = chunks.length > PAGE_CHUNKS;
    this.sql.exec(
      `INSERT INTO versions (version_id, node_id, seq, size, mtime_ns, executable, chunk_count, manifest_digest, chunker,
         chunks, device_id, user_id, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
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

  private createDir(op: CreateDir): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    const pendingFor = pendingName(op.pendingFor);
    if (pendingFor === undefined) return rejected("INVALID_NAME");
    if (getNode(this.sql, op.nodeId)) return rejected("ID_EXISTS");
    const parent = this.parentFor(op.parentId);
    if (typeof parent === "string") return rejected(parent);
    if (!parent.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const existing = this.liveWithName(parent.node_id, nameKey(name), "");
    if (existing?.kind === NodeKindValue.DIR) {
      this.remap.set(op.nodeId, existing.node_id);
      return result(OpResult_Code.MERGED_INTO, existing.node_id, existing.name);
    }
    const picked = this.pickName(parent.node_id, name, "");
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.DIR, null, this.dev);
    this.mark(op.nodeId, pendingFor);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private createFile(op: CreateFile): OpResult {
    const name = normalizeName(op.name);
    if (!name) return rejected("INVALID_NAME");
    if (getNode(this.sql, op.nodeId)) return rejected("ID_EXISTS");
    const parent = this.parentFor(op.parentId);
    if (typeof parent === "string") return rejected(parent);
    if (!parent.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    const picked = this.pickName(parent.node_id, name, "");
    const versionId = this.insertVersion(op.nodeId, op.content!, op.content!.chunks);
    this.insertNode(op.nodeId, parent.node_id, picked.name, NodeKindValue.FILE, versionId, this.dev);
    return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
  }

  private updateContent(nodeId: string, content: Content, chunks: readonly ChunkRef[], resend = false): OpResult {
    const found = this.node(nodeId);
    if (!found) return rejected("NOT_FOUND");
    if (found.kind !== NodeKindValue.FILE) return rejected("NOT_FILE");
    if (resend && found.status === NodeStatusValue.TRASHED) return result(OpResult_Code.SUPERSEDED);
    if (this.unseenContent(found) && this.holds(found, content)) {
      if (resend || found.edit_rev >= found.content_rev) return result(OpResult_Code.APPLIED, "", found.name);
      const { node, renamed } = this.reviveTarget(found);
      this.sql.exec("UPDATE nodes SET edit_rev = ?, edit_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, node.node_id);
      this.touch(node.node_id);
      return result(renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", node.name);
    }
    const { node, renamed } = this.reviveTarget(found);
    if (this.unseenContent(node)) {
      const copyId = uuidv7(this.ctx.nowMs);
      const name = this.pickConflictCopyName(node.parent_id!, node.name);
      const versionId = this.insertVersion(copyId, content, chunks);
      this.insertNode(copyId, node.parent_id!, name, NodeKindValue.FILE, versionId, "");
      return result(OpResult_Code.CONFLICT_COPY, copyId, name);
    }
    this.sql.exec("UPDATE versions SET superseded_at_ms = ? WHERE version_id = ?", this.ctx.nowMs, node.cur_version_id);
    const versionId = this.insertVersion(node.node_id, content, chunks);
    const [editRev, editDev] = resend ? [node.edit_rev, node.edit_dev] : [this.ctx.seq, this.dev];
    this.sql.exec(
      "UPDATE nodes SET cur_version_id = ?, content_rev = ?, content_dev = ?, edit_rev = ?, edit_dev = ? WHERE node_id = ?",
      versionId,
      this.ctx.seq,
      this.dev,
      editRev,
      editDev,
      node.node_id,
    );
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
    if (!found) return rejected("NOT_FOUND");
    if (!found.parent_id) return rejected("ROOT");
    const name = normalizeName(op.newName);
    if (!name) return rejected("INVALID_NAME");
    const pendingFor = pendingName(op.pendingFor);
    if (pendingFor === undefined) return rejected("INVALID_NAME");
    const target = this.node(op.newParentId);
    if (!target) return rejected("NOT_FOUND");
    if (target.kind !== NodeKindValue.DIR) return rejected("PARENT_NOT_DIR");
    if (this.isSelfOrDescendant(target, found.node_id)) return rejected("CYCLE");
    if (!target.parent_id && isReservedAtRoot(name)) return rejected("INVALID_NAME");
    if (op.from && !this.isAt(found, op.from, target)) return result(OpResult_Code.SUPERSEDED);
    if (found.status === NodeStatusValue.LIVE && found.parent_id === target.node_id && found.name === name) {
      if (op.from) return result(OpResult_Code.APPLIED, "", name);
      if (metaEditRev(found) < found.meta_rev) {
        this.sql.exec(
          "UPDATE nodes SET parent_rev = ?, parent_dev = ?, meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?",
          this.ctx.seq,
          this.dev,
          this.ctx.seq,
          this.dev,
          found.node_id,
        );
        this.touch(found.node_id);
      } else if (this.unseenParent(found)) {
        this.sql.exec("UPDATE nodes SET parent_rev = ?, parent_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, found.node_id);
      }
      return result(OpResult_Code.APPLIED, "", name);
    }
    const parent = this.parentFor(target.node_id) as NodeRow;
    this.checkSubtreeDepth(found, parent);
    if (found.status === NodeStatusValue.TRASHED) {
      const group = found.trash_group!;
      const picked = this.reviveOne(found, parent.node_id, name, this.dev);
      this.reviveGroup(found.node_id, group);
      this.mark(found.node_id, pendingFor);
      return result(picked.renamed ? OpResult_Code.APPLIED_RENAMED : OpResult_Code.APPLIED, "", picked.name);
    }
    const picked = this.pickName(parent.node_id, name, found.node_id);
    const placed = found.parent_id !== parent.node_id || (!op.from && this.unseenParent(found));
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
      op.from ? metaEditRev(found) : this.ctx.seq,
      op.from ? metaEditDev(found) : this.dev,
      found.node_id,
    );
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

  private trashSubtree(node: NodeRow, group: string): boolean {
    if (this.unseenParent(node)) return true;
    let keep = this.unseenEdit(node) || this.unseenMeta(node);
    const children = rows<NodeRow>(this.sql.exec("SELECT * FROM nodes WHERE parent_id = ? AND status = 1", node.node_id));
    for (const child of children) keep = this.trashSubtree(child, group) || keep;
    if (keep) return true;
    this.sql.exec(
      "UPDATE nodes SET status = 2, trash_group = ?, trashed_at_ms = ?, meta_rev = ?, meta_dev = ?, meta_edit_rev = ?, meta_edit_dev = ? WHERE node_id = ?",
      group,
      this.ctx.nowMs,
      this.ctx.seq,
      this.dev,
      this.ctx.seq,
      this.dev,
      node.node_id,
    );
    this.touch(node.node_id);
    if (node.kind === NodeKindValue.FILE) this.trashed.add(node.node_id);
    return false;
  }

  private delete(op: Delete, index: number): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return rejected("NOT_FOUND");
    if (!node.parent_id) return rejected("ROOT");
    if (node.status === NodeStatusValue.TRASHED) return result(OpResult_Code.APPLIED);
    const required = op.requires && this.node(op.requires.nodeId);
    if (
      op.requires &&
      !(
        required?.status === NodeStatusValue.LIVE &&
        BigInt(required.content_rev) === op.requires.contentRev &&
        BigInt(required.meta_rev) === op.requires.metaRev
      )
    ) {
      return result(OpResult_Code.SUPERSEDED);
    }
    if (this.trashSubtree(node, `${this.ctx.commitId}:${index}`)) return result(OpResult_Code.SUPERSEDED);
    if (required) {
      this.sql.exec("UPDATE nodes SET parent_rev = ?, parent_dev = ? WHERE node_id = ?", this.ctx.seq, this.dev, required.node_id);
    }
    return result(OpResult_Code.APPLIED);
  }

  private restore(op: Restore): OpResult {
    const node = this.node(op.nodeId);
    if (!node) return rejected("NOT_FOUND");
    if (op.versionId) {
      const version = getVersion(this.sql, op.versionId);
      if (!version || version.node_id !== node.node_id) return rejected("NOT_FOUND");
      return this.updateContent(node.node_id, versionContent(this.sql, version, false), versionChunks(this.sql, version));
    }
    if (!node.parent_id) return rejected("ROOT");
    if (node.status !== NodeStatusValue.TRASHED) return result(OpResult_Code.APPLIED, "", node.name);
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
