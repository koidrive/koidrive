// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import { BinaryReader } from "@bufbuild/protobuf/wire";
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { ExportHeaderSchema, NodeSnapshotSchema } from "../src/gen/koidrive/v1/export_pb";
import {
  type CommitRecord,
  CommitRecordSchema,
  ContentSchema,
  FieldIntentsSchema,
  type Intent,
  IntentSchema,
  NodeKind,
  type NodePurged,
  type NodeState,
  NodeStateSchema,
  NodeStatus,
  type VersionInfo,
  VersionInfoSchema,
} from "../src/gen/koidrive/v1/model_pb";
import { sha256 } from "../src/crypto";
import { toHex } from "../src/ids";
import { type Field, intentBytes, legacyIntentId, legacyPlaceKey } from "../src/space/intents";
import { boundMadeOver, nodeState, type NodeRow, rows, tombstoneRecord, type VersionRow, versionInfo } from "../src/space/store";

export interface RestoredNode {
  state: NodeState;
  previous: Map<string, { version: VersionInfo; supersededAtMs: bigint }>;
  trashedAtMs: bigint;
}

export interface Manifest {
  format: string;
  format_version: number;
  space_id: string;
  history_id: string;
  start_seq: number;
  end_seq: number;
  created_at: string;
  pages: { name: string; records: number; bytes: number; sha256: string }[];
}

export interface Restored {
  descriptor: Record<string, unknown>;
  manifest?: Manifest;
  headSeq: number;
  nodes: Map<string, RestoredNode>;
  tombstones: Map<string, NodePurged>;
}

function legacyIntent(nodeId: string, field: Field, key: string): Intent {
  return create(IntentSchema, { id: intentBytes(legacyIntentId(nodeId, field, key)) });
}

function withLegacyIntents(state: NodeState, liveSeq: bigint): NodeState {
  if (state.intents) return state;
  const status = state.status === NodeStatus.LIVE ? `live:${liveSeq}` : `trashed:${state.trashGroup}`;
  const intents = create(FieldIntentsSchema, {
    place: legacyIntent(state.nodeId, "place", legacyPlaceKey(Number(state.metaRev), state.parentId === "" ? null : state.parentId, state.name)),
    content: state.current ? legacyIntent(state.nodeId, "content", state.current.versionId) : undefined,
    status: legacyIntent(state.nodeId, "status", status),
  });
  return create(NodeStateSchema, { ...state, intents });
}

function carriedFlags(state: NodeState): boolean[] {
  return [state.intents?.place, state.intents?.content, state.intents?.status].map((intent) => intent?.replay ?? false);
}

async function gunzip(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function frames(bytes: Uint8Array, format: string, formatVersion: number, spaceId: string, historyId: string): Promise<Uint8Array[]> {
  const reader = new BinaryReader(await gunzip(bytes));
  const out: Uint8Array[] = [];
  while (reader.pos < reader.len) out.push(reader.bytes());
  const header = fromBinary(ExportHeaderSchema, out.shift()!);
  if (header.format !== format || header.formatVersion !== formatVersion || header.spaceId !== spaceId || header.historyId !== historyId) {
    throw new Error(`unexpected header ${JSON.stringify(header)}`);
  }
  return out;
}

async function json<T>(key: string): Promise<T | undefined> {
  const object = await env.BUCKET.get(key);
  return object ? ((await object.json()) as T) : undefined;
}

async function keys(prefix: string): Promise<string[]> {
  const out: string[] = [];
  let cursor: string | undefined;
  do {
    const listed = await env.BUCKET.list({ prefix, cursor });
    out.push(...listed.objects.map((object) => object.key));
    cursor = listed.truncated ? listed.cursor : undefined;
  } while (cursor);
  return out;
}

function replay(nodes: Map<string, RestoredNode>, tombstones: Map<string, NodePurged>, liveSince: Map<string, bigint>, record: CommitRecord): void {
  for (const { change } of record.changes) {
    if (change.case === "purged") {
      nodes.delete(change.value.nodeId);
      tombstones.set(change.value.nodeId, change.value);
      continue;
    }
    if (change.case !== "node") throw new Error(`unexpected change ${change.case}`);
    tombstones.delete(change.value.nodeId);
    const known = nodes.get(change.value.nodeId);
    const revived = change.value.status === NodeStatus.LIVE && known?.state.status !== NodeStatus.LIVE;
    if (revived) liveSince.set(change.value.nodeId, record.seq);
    const incoming = withLegacyIntents(change.value, liveSince.get(change.value.nodeId) ?? 0n);
    const version = incoming.current;
    const held = version && (known?.state.current?.versionId === version.versionId ? known.state.current : known?.previous.get(version.versionId)?.version);
    if (held && version) {
      held.madeOver = boundMadeOver(version.madeOver);
    }
    const rev = (state: NodeState) => [state.contentRev, state.metaRev, state.editRev, state.metaEditRev].reduce((a, b) => (a > b ? a : b));
    if (known && rev(incoming) <= rev(known.state) && carriedFlags(incoming).join() === carriedFlags(known.state).join()) continue;
    const previous = new Map(known?.previous);
    let current = incoming.current;
    if (current) {
      const held = known?.state.current?.versionId === current.versionId ? known.state.current : previous.get(current.versionId)?.version;
      const content = current.content!;
      if (content.chunks.length < content.chunkCount && held && held.content!.chunks.length === held.content!.chunkCount) {
        current = create(VersionInfoSchema, { ...current, content: create(ContentSchema, { ...content, chunks: held.content!.chunks }) });
      }
    }
    const replaced = known?.state.current;
    if (replaced && replaced.versionId !== current?.versionId) previous.set(replaced.versionId, { version: replaced, supersededAtMs: record.committedAtMs });
    const trashed = incoming.status === NodeStatus.TRASHED;
    const trashedAtMs = !trashed ? 0n : known?.state.status === NodeStatus.TRASHED ? known.trashedAtMs : record.committedAtMs;
    nodes.set(incoming.nodeId, { state: create(NodeStateSchema, { ...incoming, current }), previous, trashedAtMs });
  }
}

export async function restoreFromR2(spaceId: string): Promise<Restored> {
  const meta = `spaces/${spaceId}/meta/`;
  const descriptors: Record<string, unknown>[] = [];
  for (const key of await keys(meta)) {
    if (!key.endsWith("/space.json")) continue;
    const descriptor = (await json<Record<string, unknown>>(key))!;
    if (descriptor.format !== "koidrive.space" || descriptor.format_version !== 1) throw new Error(`unexpected descriptor ${key}`);
    descriptors.push(descriptor);
  }
  const descriptor = descriptors.sort((a, b) => Number(b.rewind_number) - Number(a.rewind_number))[0];
  if (!descriptor) throw new Error("no descriptor");
  const historyId = String(descriptor.history_id);
  const prefix = `${meta}${historyId}/`;

  for (;;) {
    const manifests: Manifest[] = [];
    for (const key of await keys(prefix + "snap/")) {
      const manifest = key.endsWith("/manifest.json") ? await json<Manifest>(key) : undefined;
      if (manifest) manifests.push(manifest);
    }
    const manifest = manifests.sort((a, b) => b.start_seq - a.start_seq)[0];
    const nodes = new Map<string, RestoredNode>();
    const tombstones = new Map<string, NodePurged>();
    const liveSince = new Map<string, bigint>();
    if (manifest) {
      if (manifest.format !== "koidrive.snapshot" || ![1, 2].includes(manifest.format_version) || manifest.history_id !== historyId) throw new Error("unexpected manifest");
      let missing = false;
      for (const page of manifest.pages) {
        const object = await env.BUCKET.get(`${prefix}snap/${String(manifest.start_seq).padStart(20, "0")}/pages/${page.name}`);
        if (!object) {
          missing = true;
          break;
        }
        const bytes = new Uint8Array(await object.arrayBuffer());
        if (bytes.length !== page.bytes || toHex(await sha256(bytes)) !== page.sha256) throw new Error(`page ${page.name} does not match the manifest`);
        const records = await frames(bytes, "koidrive.snapshot", manifest.format_version, spaceId, historyId);
        if (records.length !== page.records) throw new Error(`page ${page.name} holds ${records.length} records`);
        for (const record of records) {
          const snapshot = fromBinary(NodeSnapshotSchema, record);
          if (snapshot.tombstone) {
            if (manifest.format_version < 2) throw new Error(`page ${page.name} holds a tombstone at format_version 1`);
            tombstones.set(snapshot.tombstone.nodeId, snapshot.tombstone);
            continue;
          }
          const previous = new Map(snapshot.previous.map((retained) => [retained.version!.versionId, { version: retained.version!, supersededAtMs: retained.supersededAtMs }]));
          nodes.set(snapshot.node!.nodeId, { state: snapshot.node!, previous, trashedAtMs: snapshot.trashedAtMs });
        }
      }
      if (missing) {
        const newer = (await keys(prefix + "snap/")).some((key) => key.endsWith("/manifest.json") && !key.includes(String(manifest.start_seq).padStart(20, "0")));
        if (newer) continue;
        throw new Error("the export is damaged: a page of the newest manifest is missing");
      }
    } else {
      nodes.set(spaceId, {
        state: withLegacyIntents(create(NodeStateSchema, { nodeId: spaceId, kind: NodeKind.DIR, status: NodeStatus.LIVE }), 0n),
        previous: new Map(),
        trashedAtMs: 0n,
      });
    }

    const startSeq = manifest?.start_seq ?? 0;
    const segments = (await keys(prefix + "log/"))
      .map((key) => {
        const [first, last] = key.slice(key.lastIndexOf("/") + 1, -".fblog".length).split("-").map(Number);
        return { key, first: first!, last: last! };
      })
      .sort((a, b) => a.first - b.first)
      .filter((segment) => segment.last > startSeq);
    let next = startSeq + 1;
    for (const [i, segment] of segments.entries()) {
      if (i === 0 ? segment.first > next : segment.first !== next) throw new Error(`the log does not chain at ${segment.key}`);
      const object = (await env.BUCKET.get(segment.key))!;
      let seq = segment.first;
      for (const frame of await frames(new Uint8Array(await object.arrayBuffer()), "koidrive.log", 1, spaceId, historyId)) {
        const record = fromBinary(CommitRecordSchema, frame);
        if (Number(record.seq) !== seq++) throw new Error(`${segment.key} skips a seq`);
        if (record.seq > BigInt(startSeq)) replay(nodes, tombstones, liveSince, record);
      }
      if (seq !== segment.last + 1) throw new Error(`${segment.key} ends early`);
      next = seq;
    }
    if (manifest && next - 1 < manifest.end_seq) throw new Error("the log stops before the manifest's end_seq");
    return { descriptor, manifest, headSeq: next - 1, nodes, tombstones };
  }
}

export function serverState(stub: DurableObjectStub): Promise<{ headSeq: number; nodes: Map<string, RestoredNode>; tombstones: Map<string, NodePurged> }> {
  return runInDurableObject(stub, (_instance, state) => {
    const sql = state.storage.sql;
    const nodes = new Map<string, RestoredNode>();
    for (const node of rows<NodeRow>(sql.exec("SELECT * FROM nodes"))) {
      const previous = new Map(
        rows<VersionRow>(sql.exec("SELECT * FROM versions WHERE node_id = ? AND version_id IS NOT ?", node.node_id, node.cur_version_id)).map((version) => [
          version.version_id,
          { version: versionInfo(sql, version, true), supersededAtMs: BigInt(version.superseded_at_ms ?? 0) },
        ]),
      );
      nodes.set(node.node_id, { state: nodeState(sql, node, true), previous, trashedAtMs: BigInt(node.trashed_at_ms ?? 0) });
    }
    const tombstones = new Map(
      rows<{ node_id: string }>(sql.exec("SELECT node_id FROM tombstones")).map(({ node_id }) => [node_id, tombstoneRecord(sql, node_id)!] as const),
    );
    return { headSeq: Number(sql.exec("SELECT value FROM meta WHERE key = 'head_seq'").one().value), nodes, tombstones };
  });
}
