// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from "@bufbuild/protobuf";
import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import { NodeKind } from "../gen/koidrive/v1/model_pb";
import { type ShareEntry, ShareEntrySchema, type ShareInfo, ShareInfoSchema, type ShareListing, ShareListingSchema } from "../gen/koidrive/v1/share_pb";
import { base64url, fromBase64url } from "../crypto";
import type { Sql } from "../host";
import { isUuidV7, toHex } from "../ids";
import type { ResolvedLink, ShareOutcome } from "./links";
import { NodeKindValue, NodeStatusValue, ChunkState } from "../space/schema";
import { chunkKey, packKey, getMeta, getNode, getVersion, type NodeRow, PAGE_CHUNKS, rows, versionChunks } from "../space/store";

export const LISTING_PAGE = 500;
export const MAX_DOWNLOAD_CHUNKS = PAGE_CHUNKS;
const MAX_DEPTH = 256;

const decoder = new TextDecoder();
const encoder = new TextEncoder();

export interface ShareFile {
  name: string;
  size: number;
  chunks: { key: string; hashHex: string; size: number; offset?: number }[];
  historyId: string;
}

export interface DownloadFence {
  historyId: string;
  generation: number;
}

const notReachable = (): { ok: false; code: Error_Code; message: string; status: number } => ({
  ok: false,
  code: Error_Code.NOT_FOUND,
  message: "this item is not available through this link",
  status: 404,
});

function refuse(code: Error_Code, message: string, status: number): { ok: false; code: Error_Code; message: string; status: number } {
  return { ok: false, code, message, status };
}

export function reachable(sql: Sql, rootId: string, nodeId: string): NodeRow | undefined {
  if (!isUuidV7(nodeId)) return undefined;
  const node = getNode(sql, nodeId);
  let cur = node;
  for (let depth = 0; cur && depth <= MAX_DEPTH; depth++) {
    if (cur.status !== NodeStatusValue.LIVE) return undefined;
    if (cur.node_id === rootId) return node;
    if (cur.parent_id === null) return undefined;
    cur = getNode(sql, cur.parent_id);
  }
  return undefined;
}

function displayName(sql: Sql, node: NodeRow): string {
  return node.parent_id === null ? String(getMeta(sql, "name") ?? "") : node.name;
}

function entry(sql: Sql, node: NodeRow): ShareEntry {
  const version = node.kind === NodeKindValue.FILE && node.cur_version_id ? getVersion(sql, node.cur_version_id) : undefined;
  return create(ShareEntrySchema, {
    nodeId: node.node_id,
    name: displayName(sql, node),
    kind: node.kind === NodeKindValue.DIR ? NodeKind.DIR : NodeKind.FILE,
    size: BigInt(version?.size ?? 0),
    mtimeMs: version ? BigInt(version.mtime_ns) / 1_000_000n : 0n,
  });
}

export function checkLinkRoot(sql: Sql, nodeId: string): ShareOutcome<null> {
  const node = isUuidV7(nodeId) ? getNode(sql, nodeId) : undefined;
  if (!node || node.status !== NodeStatusValue.LIVE) return refuse(Error_Code.NOT_FOUND, "node_id is not a live node of this space", 404);
  return { ok: true, value: null };
}

export function shareInfo(sql: Sql, rootId: string, expiresAtMs: number): ShareOutcome<ShareInfo> {
  const root = reachable(sql, rootId, rootId);
  if (!root) return notReachable();
  const { kind, name, size, mtimeMs } = entry(sql, root);
  return { ok: true, value: create(ShareInfoSchema, { kind, name, rootId, size, mtimeMs, expiresAtMs: BigInt(expiresAtMs) }) };
}

export function shareListing(sql: Sql, rootId: string, dirId: string, cursor: string): ShareOutcome<ShareListing> {
  const dir = reachable(sql, rootId, dirId || rootId);
  if (!dir) return notReachable();
  if (dir.kind !== NodeKindValue.DIR) return refuse(Error_Code.INVALID_ARGUMENT, "only a folder has a listing", 400);
  let after: [string, string] = ["", ""];
  if (cursor) {
    const bytes = fromBase64url(cursor);
    let parsed: unknown;
    try {
      parsed = bytes ? JSON.parse(decoder.decode(bytes)) : undefined;
    } catch {
      parsed = undefined;
    }
    if (!Array.isArray(parsed) || parsed.length !== 2 || !parsed.every((part) => typeof part === "string")) {
      return refuse(Error_Code.INVALID_ARGUMENT, "malformed cursor", 400);
    }
    after = parsed as [string, string];
  }
  const found = rows<NodeRow>(
    sql.exec(
      `SELECT * FROM nodes WHERE parent_id = ? AND status = 1 AND (name_key > ? OR (name_key = ? AND node_id > ?))
       ORDER BY name_key, node_id LIMIT ?`,
      dir.node_id,
      after[0],
      after[0],
      after[1],
      LISTING_PAGE + 1,
    ),
  );
  const page = found.slice(0, LISTING_PAGE);
  const path: NodeRow[] = [];
  for (let cur: NodeRow | undefined = dir; cur; cur = cur.node_id === rootId ? undefined : getNode(sql, cur.parent_id!)) path.unshift(cur);
  const last = page.at(-1);
  return {
    ok: true,
    value: create(ShareListingSchema, {
      entries: page.map((node) => entry(sql, node)),
      path: path.map((node) => entry(sql, node)),
      nextCursor: found.length > LISTING_PAGE && last ? base64url(encoder.encode(JSON.stringify([last.name_key, last.node_id]))) : "",
    }),
  };
}

export function shareFile(sql: Sql, spaceId: string, rootId: string, nodeId: string): ShareOutcome<ShareFile> {
  const node = reachable(sql, rootId, nodeId);
  if (!node) return notReachable();
  if (node.kind !== NodeKindValue.FILE || !node.cur_version_id) return refuse(Error_Code.INVALID_ARGUMENT, "only a file can be downloaded", 400);
  const version = getVersion(sql, node.cur_version_id);
  if (!version) return notReachable();
  if (version.chunk_count > MAX_DOWNLOAD_CHUNKS) {
    return refuse(Error_Code.LIMIT_EXCEEDED, `this file has more than ${MAX_DOWNLOAD_CHUNKS} chunks, too many to download through a link`, 413);
  }
  const places = new Map<string, { key: string; offset?: number }>();
  const chunks: ShareFile["chunks"] = [];
  for (const chunk of versionChunks(sql, version)) {
    const hashHex = toHex(chunk.hash);
    let place = places.get(hashHex);
    if (place === undefined) {
      const row = sql.exec("SELECT state, incarnation, pack_id, pack_offset FROM chunks WHERE hash = ?", chunk.hash).toArray()[0];
      if (row?.state !== ChunkState.LIVE || row.incarnation === null) {
        return refuse(Error_Code.RETRY_LATER, "part of this file is not stored right now; retry later", 503);
      }
      place = row.pack_id === null ? { key: chunkKey(spaceId, hashHex, String(row.incarnation)) } : { key: packKey(spaceId, String(row.pack_id)), offset: Number(row.pack_offset) };
      places.set(hashHex, place);
    }
    chunks.push({ ...place, hashHex, size: chunk.size });
  }
  return { ok: true, value: { name: node.name, size: version.size, chunks, historyId: String(getMeta(sql, "history_id")) } };
}

export function checkFence(sql: Sql, spaceId: string, resolved: ShareOutcome<ResolvedLink>, fence: DownloadFence): ShareOutcome<null> {
  if (!resolved.ok) return resolved;
  if (resolved.value.spaceId !== spaceId || resolved.value.generation !== fence.generation || String(getMeta(sql, "history_id")) !== fence.historyId) {
    return refuse(Error_Code.RETRY_LATER, "the space was rewound during this download", 503);
  }
  return { ok: true, value: null };
}
