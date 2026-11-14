// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import {
  type ChunkRef,
  ChunkRefSchema,
  type Content,
  ContentSchema,
  NodeKind,
  type NodeState,
  NodeStateSchema,
  NodeStatus,
  type VersionInfo,
  VersionInfoSchema,
} from "../gen/koidrive/v1/model_pb";
import type { Sql, SqlCursor, SqlRow, SqlValue } from "../host";

export const PAGE_CHUNKS = 4096;
export const INLINE_CHUNKS = 16384;
const CHUNK_ENTRY_BYTES = 36;

export interface NodeRow {
  node_id: string;
  parent_id: string | null;
  name: string;
  name_key: string;
  kind: number;
  status: number;
  content_rev: number;
  content_dev: string;
  edit_rev: number;
  edit_dev: string;
  meta_rev: number;
  meta_dev: string;
  meta_edit_rev: number | null;
  meta_edit_dev: string | null;
  parent_rev: number;
  parent_dev: string;
  cur_version_id: string | null;
  trash_group: string | null;
  trashed_at_ms: number | null;
  pending_for: string | null;
  pending_by: string | null;
}

export interface VersionRow {
  version_id: string;
  node_id: string;
  seq: number;
  size: number;
  /** Decimal text, because SQLite bindings carry no 64-bit integers. */
  mtime_ns: string;
  executable: number;
  chunk_count: number;
  manifest_digest: ArrayBuffer;
  chunker: string;
  chunks: ArrayBuffer | null;
  device_id: string;
  user_id: string;
  created_at_ms: number;
  superseded_at_ms: number | null;
}

export const INCARNATION = /^\d{1,15}\.\d{1,15}$/;

export function chunkKey(spaceId: string, hashHex: string, incarnation: string): string {
  return `spaces/${spaceId}/chunks/${hashHex.slice(0, 2)}/${hashHex}/${incarnation}`;
}

export function parseChunkKey(spaceId: string, key: string): { hashHex: string; incarnation: string } | undefined {
  const match = /^([0-9a-f]{2})\/([0-9a-f]{64})\/([^/]+)$/.exec(key.slice(`spaces/${spaceId}/chunks/`.length));
  if (!key.startsWith(`spaces/${spaceId}/chunks/`) || !match || !match[2]!.startsWith(match[1]!) || !INCARNATION.test(match[3]!)) return undefined;
  return { hashHex: match[2]!, incarnation: match[3]! };
}

export function rows<T>(cursor: SqlCursor<SqlRow>): T[] {
  return cursor.toArray() as unknown as T[];
}

export function encodeChunkList(chunks: readonly ChunkRef[]): Uint8Array {
  const out = new Uint8Array(chunks.length * CHUNK_ENTRY_BYTES);
  const view = new DataView(out.buffer);
  chunks.forEach((chunk, i) => {
    out.set(chunk.hash, i * CHUNK_ENTRY_BYTES);
    view.setUint32(i * CHUNK_ENTRY_BYTES + 32, chunk.size, true);
  });
  return out;
}

export function decodeChunkList(bytes: Uint8Array): ChunkRef[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const out: ChunkRef[] = [];
  for (let offset = 0; offset < bytes.length; offset += CHUNK_ENTRY_BYTES) {
    out.push(create(ChunkRefSchema, { hash: bytes.slice(offset, offset + 32), size: view.getUint32(offset + 32, true) }));
  }
  return out;
}

export function getMeta(sql: Sql, key: string): SqlValue | undefined {
  return sql.exec("SELECT value FROM meta WHERE key = ?", key).toArray()[0]?.value;
}

export function setMeta(sql: Sql, key: string, value: SqlValue): void {
  sql.exec("INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", key, value);
}

export function getNode(sql: Sql, nodeId: string): NodeRow | undefined {
  return rows<NodeRow>(sql.exec("SELECT * FROM nodes WHERE node_id = ?", nodeId))[0];
}

export function getVersion(sql: Sql, versionId: string): VersionRow | undefined {
  return rows<VersionRow>(sql.exec("SELECT * FROM versions WHERE version_id = ?", versionId))[0];
}

export function versionPageCount(version: VersionRow): number {
  return Math.max(1, Math.ceil(version.chunk_count / PAGE_CHUNKS));
}

export function versionChunkPage(sql: Sql, version: VersionRow, page: number): ChunkRef[] {
  if (version.chunks) return page === 0 ? decodeChunkList(new Uint8Array(version.chunks)) : [];
  const row = sql
    .exec("SELECT chunks FROM version_chunk_pages WHERE version_id = ? AND page = ?", version.version_id, page)
    .toArray()[0];
  return row ? decodeChunkList(new Uint8Array(row.chunks as ArrayBuffer)) : [];
}

export function versionChunks(sql: Sql, version: VersionRow): ChunkRef[] {
  const out: ChunkRef[] = [];
  for (let page = 0; page < versionPageCount(version); page++) out.push(...versionChunkPage(sql, version, page));
  return out;
}

export function versionContent(sql: Sql, version: VersionRow, inline: boolean, complete = false): Content {
  return create(ContentSchema, {
    size: BigInt(version.size),
    mtimeNs: BigInt(version.mtime_ns),
    executable: version.executable === 1,
    chunks: inline && (complete || version.chunk_count <= INLINE_CHUNKS) ? versionChunks(sql, version) : [],
    chunkCount: version.chunk_count,
    manifestDigest: new Uint8Array(version.manifest_digest),
    chunker: version.chunker,
  });
}

export function versionInfo(sql: Sql, version: VersionRow, complete = false): VersionInfo {
  return create(VersionInfoSchema, {
    versionId: version.version_id,
    seq: BigInt(version.seq),
    content: versionContent(sql, version, true, complete),
    deviceId: version.device_id,
    userId: version.user_id,
    createdAtMs: BigInt(version.created_at_ms),
  });
}

export function metaEditRev(node: NodeRow): number {
  return node.meta_edit_rev ?? node.meta_rev;
}

export function metaEditDev(node: NodeRow): string {
  return node.meta_edit_dev ?? node.meta_dev;
}

export function nodeState(sql: Sql, node: NodeRow, complete = false): NodeState {
  const version = node.cur_version_id ? getVersion(sql, node.cur_version_id) : undefined;
  return create(NodeStateSchema, {
    nodeId: node.node_id,
    parentId: node.parent_id ?? "",
    name: node.name,
    kind: node.kind === 1 ? NodeKind.DIR : NodeKind.FILE,
    status: node.status === 1 ? NodeStatus.LIVE : NodeStatus.TRASHED,
    contentRev: BigInt(node.content_rev),
    editRev: BigInt(node.edit_rev),
    metaRev: BigInt(node.meta_rev),
    metaEditRev: BigInt(metaEditRev(node)),
    trashGroup: node.trash_group ?? "",
    current: version ? versionInfo(sql, version, complete) : undefined,
    pendingFor: node.pending_for ?? "",
    pendingBy: node.pending_by ?? "",
  });
}
