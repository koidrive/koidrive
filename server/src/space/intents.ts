// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary } from "@bufbuild/protobuf";
import type { Op } from "../gen/koidrive/v1/commit_pb";
import { type CommitRecord, CommitRecordSchema, type FieldIntents, FieldIntentsSchema, type Intent, IntentSchema, NodeStatus } from "../gen/koidrive/v1/model_pb";
import type { Sql } from "../host";
import { fromHex, toHex, uuidv7 } from "../ids";
import { LEGACY_INTENTS_DUE } from "./schema";

export const FIELDS = ["place", "content", "status"] as const;
export type Field = (typeof FIELDS)[number];

export type LostChangeRule = "A" | "B";
export const LOST_CHANGE_RULE: LostChangeRule = "A";

export interface FieldIntent {
  id: string;
  author: string;
  carried: boolean;
  appliedAtMs: number;
  seq: number;
}

export type NodeIntents = Partial<Record<Field, FieldIntent>>;

export interface Incoming extends FieldIntent {
  replay: boolean;
}

export interface OpIntents {
  client: boolean;
  fields: Record<Field, Incoming>;
  over: string | undefined;
}

interface IntentRow {
  field: Field;
  intent_id: string;
  author: string;
  carried: number;
  applied_at_ms: number;
  seq: number;
}

export function readIntents(sql: Sql, nodeId: string): NodeIntents {
  const found: NodeIntents = {};
  for (const row of sql.exec("SELECT * FROM field_intents WHERE node_id = ?", nodeId).toArray() as unknown as IntentRow[]) {
    found[row.field] = { id: row.intent_id, author: row.author, carried: row.carried === 1, appliedAtMs: Number(row.applied_at_ms), seq: Number(row.seq) };
  }
  return found;
}

export function writeIntent(sql: Sql, nodeId: string, field: Field, intent: FieldIntent): void {
  sql.exec(
    `INSERT INTO field_intents (node_id, field, intent_id, author, carried, applied_at_ms, seq) VALUES (?, ?, ?, ?, ?, ?, ?)
     ON CONFLICT (node_id, field) DO UPDATE SET intent_id = excluded.intent_id, author = excluded.author, carried = excluded.carried,
       applied_at_ms = excluded.applied_at_ms, seq = excluded.seq`,
    nodeId,
    field,
    intent.id,
    intent.author,
    intent.carried ? 1 : 0,
    intent.appliedAtMs,
    intent.seq,
  );
}

export function holdersOf(sql: Sql, intentId: string): { node_id: string; field: Field }[] {
  return sql.exec("SELECT node_id, field FROM field_intents WHERE intent_id = ?", intentId).toArray() as unknown as { node_id: string; field: Field }[];
}

export function trashHeld(sql: Sql, group: string | null): string[] {
  if (group === null) return [];
  return sql.exec("SELECT intent_id FROM trash_held WHERE trash_group = ? ORDER BY intent_id", group).toArray().map((row) => String(row.intent_id));
}

export function mintIntentId(nowMs: number): string {
  return uuidv7(nowMs).replaceAll("-", "");
}

export function intentBytes(id: string): Uint8Array {
  return fromHex(id)!;
}

export function wireIntent(intent: FieldIntent | undefined): Intent | undefined {
  if (!intent) return undefined;
  return create(IntentSchema, { id: intentBytes(intent.id), authorDeviceId: intent.author, replay: intent.carried, appliedAtMs: BigInt(intent.appliedAtMs) });
}

export function wireIntents(intents: NodeIntents): FieldIntents | undefined {
  if (!intents.place && !intents.content && !intents.status) return undefined;
  return create(FieldIntentsSchema, { place: wireIntent(intents.place), content: wireIntent(intents.content), status: wireIntent(intents.status) });
}

const INTENT_BYTES = 16;

function validId(bytes: Uint8Array, optional: boolean): boolean {
  return bytes.length === INTENT_BYTES || (optional && bytes.length === 0);
}

function validIntent(intent: Intent | undefined): boolean {
  return intent === undefined || (validId(intent.id, false) && validId(intent.over, true));
}

export function intentsValid(op: Op): boolean {
  if (!validIntent(op.intent)) return false;
  if (op.fields) {
    if (op.op.case !== "createDir" && op.op.case !== "createFile") return false;
    if (!op.intent) return false;
    if (![op.fields.place, op.fields.content, op.fields.status].every((intent) => intent === undefined || intent.id.length === 0 || validIntent(intent))) return false;
  }
  if (op.op.case === "delete") return [...op.op.value.unseen, ...op.op.value.held].every((id) => validId(id, false));
  return true;
}

function mintedCarried(op: Op): boolean {
  const { op: named } = op;
  return (
    (named.case === "updateContent" && (named.value.resend || named.value.sourceVersionId !== "")) ||
    (named.case === "createFile" && named.value.sourceVersionId !== "") ||
    (named.case === "move" && named.value.from !== undefined)
  );
}

export function resolveIntents(op: Op, ctx: { deviceId: string; seq: number; nowMs: number; rule: LostChangeRule }): OpIntents {
  const carried = (author: string, replay: boolean) => author !== ctx.deviceId || (ctx.rule === "A" && replay);
  if (!op.intent) {
    const author = mintedCarried(op) ? "" : ctx.deviceId;
    const minted: Incoming = { id: mintIntentId(ctx.nowMs), author, carried: carried(author, false), appliedAtMs: ctx.nowMs, seq: ctx.seq, replay: false };
    return { client: false, fields: { place: minted, content: minted, status: minted }, over: undefined };
  }
  const incoming = (intent: Intent): Incoming => ({
    id: toHex(intent.id),
    author: intent.authorDeviceId,
    carried: carried(intent.authorDeviceId, intent.replay),
    appliedAtMs: intent.replay ? Math.min(Math.max(Number(intent.appliedAtMs), 0), ctx.nowMs) : ctx.nowMs,
    seq: ctx.seq,
    replay: intent.replay,
  });
  const main = incoming(op.intent);
  const field = (intent: Intent | undefined) => (intent && intent.id.length > 0 ? incoming(intent) : main);
  return {
    client: true,
    fields: { place: field(op.fields?.place), content: field(op.fields?.content), status: field(op.fields?.status) },
    over: op.intent.over.length > 0 ? toHex(op.intent.over) : undefined,
  };
}

const LEGACY_NAMESPACE = fromHex("701471d0f5ea47eb936a485a91d0891e")!;
const encoder = new TextEncoder();

function sha1(message: Uint8Array): Uint8Array {
  const words = new Uint32Array(80);
  const padded = new Uint8Array(Math.ceil((message.length + 9) / 64) * 64);
  padded.set(message);
  padded[message.length] = 0x80;
  const view = new DataView(padded.buffer);
  view.setUint32(padded.length - 8, Math.floor((message.length * 8) / 2 ** 32));
  view.setUint32(padded.length - 4, (message.length * 8) >>> 0);
  const h = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
  const rotl = (x: number, n: number) => ((x << n) | (x >>> (32 - n))) >>> 0;
  for (let block = 0; block < padded.length; block += 64) {
    for (let i = 0; i < 16; i++) words[i] = view.getUint32(block + i * 4);
    for (let i = 16; i < 80; i++) words[i] = rotl(words[i - 3]! ^ words[i - 8]! ^ words[i - 14]! ^ words[i - 16]!, 1);
    let [a, b, c, d, e] = h as [number, number, number, number, number];
    for (let i = 0; i < 80; i++) {
      const [f, k] =
        i < 20 ? [(b & c) | (~b & d), 0x5a827999] : i < 40 ? [b ^ c ^ d, 0x6ed9eba1] : i < 60 ? [(b & c) | (b & d) | (c & d), 0x8f1bbcdc] : [b ^ c ^ d, 0xca62c1d6];
      const next = (rotl(a, 5) + (f >>> 0) + e + k + words[i]!) >>> 0;
      [e, d, c, b, a] = [d, c, rotl(b, 30), a, next];
    }
    [a, b, c, d, e].forEach((value, i) => (h[i] = (h[i]! + value) >>> 0));
  }
  const out = new Uint8Array(20);
  const outView = new DataView(out.buffer);
  h.forEach((value, i) => outView.setUint32(i * 4, value));
  return out;
}

export function legacyIntentId(nodeId: string, field: Field, key: string): string {
  const name = encoder.encode(`${nodeId}\0${field}\0${key}`);
  const input = new Uint8Array(LEGACY_NAMESPACE.length + name.length);
  input.set(LEGACY_NAMESPACE);
  input.set(name, LEGACY_NAMESPACE.length);
  const bytes = sha1(input).slice(0, 16);
  bytes[6] = (bytes[6]! & 0x0f) | 0x50;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  return toHex(bytes);
}

export function legacyPlaceKey(metaRev: number, parentId: string | null, name: string): string {
  return `${metaRev}\0${parentId ?? ""}\0${name}`;
}

interface LegacyNode {
  node_id: string;
  parent_id: string | null;
  name: string;
  status: number;
  content_rev: number;
  meta_rev: number;
  cur_version_id: string | null;
  trash_group: string | null;
}

function decodedChanges(record: ArrayBuffer): CommitRecord["changes"] {
  try {
    return fromBinary(CommitRecordSchema, new Uint8Array(record)).changes;
  } catch {
    return [];
  }
}

export function backfillMigratedIntents(sql: Sql): void {
  if (sql.exec("SELECT 1 FROM meta WHERE key = ?", LEGACY_INTENTS_DUE).toArray().length === 0) return;
  backfillLegacyIntents(sql);
  sql.exec("DELETE FROM meta WHERE key = ?", LEGACY_INTENTS_DUE);
}

export function backfillLegacyIntents(sql: Sql): void {
  const missing = sql
    .exec(
      `SELECT node_id, parent_id, name, status, content_rev, meta_rev, cur_version_id, trash_group FROM nodes n
       WHERE NOT EXISTS (SELECT 1 FROM field_intents i WHERE i.node_id = n.node_id)`,
    )
    .toArray() as unknown as LegacyNode[];
  if (missing.length === 0) return;
  const committedAt = new Map<number, number>();
  const liveSince = new Map<string, number>();
  const live = new Set<string>();
  for (const row of sql.exec("SELECT seq, committed_at_ms, record FROM commits ORDER BY seq").toArray()) {
    const seq = Number(row.seq);
    committedAt.set(seq, Number(row.committed_at_ms));
    for (const { change } of decodedChanges(row.record as ArrayBuffer)) {
      if (change.case === "purged") live.delete(change.value.nodeId);
      if (change.case !== "node") continue;
      const nodeId = change.value.nodeId;
      if (change.value.status !== NodeStatus.LIVE) live.delete(nodeId);
      else if (!live.has(nodeId)) {
        live.add(nodeId);
        liveSince.set(nodeId, seq);
      }
    }
  }
  const legacy = (nodeId: string, field: Field, key: string, seq: number): FieldIntent => ({
    id: legacyIntentId(nodeId, field, key),
    author: "",
    carried: false,
    appliedAtMs: committedAt.get(seq) ?? 0,
    seq,
  });
  for (const node of missing) {
    const metaRev = Number(node.meta_rev);
    writeIntent(sql, node.node_id, "place", legacy(node.node_id, "place", legacyPlaceKey(metaRev, node.parent_id, node.name), metaRev));
    if (node.cur_version_id) writeIntent(sql, node.node_id, "content", legacy(node.node_id, "content", node.cur_version_id, Number(node.content_rev)));
    const liveSeq = liveSince.get(node.node_id) ?? 0;
    const status =
      node.status === 1
        ? legacy(node.node_id, "status", `live:${liveSeq}`, liveSeq)
        : legacy(node.node_id, "status", `trashed:${node.trash_group ?? ""}`, metaRev);
    writeIntent(sql, node.node_id, "status", status);
  }
}
