// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, toBinary } from "@bufbuild/protobuf";
import { BinaryWriter } from "@bufbuild/protobuf/wire";
import { ExportHeaderSchema, NodeSnapshotSchema, RetainedVersionSchema } from "../gen/koidrive/v1/export_pb";
import type { CommitRecord } from "../gen/koidrive/v1/model_pb";
import { sha256 } from "../crypto";
import type { Database, ObjectStore, Sql } from "../host";
import { toHex, uuidv7Ms } from "../ids";
import { getMeta, getNode, INLINE_CHUNKS, type NodeRow, nodeState, rows, setMeta, tombstoneRecord, type VersionRow, versionInfo } from "./store";

const DAY_MS = 24 * 3_600_000;
const SEGMENT_COMMITS = 1000;
const SEGMENT_DELAY_MS = 5 * 60_000;
const SEGMENT_BYTES = 16 * 1024 * 1024;
const SNAPSHOT_COMMITS = 50_000;
const PAGE_BYTES = 8 * 1024 * 1024;
const KEPT_SNAPSHOTS = 2;
const SNAPSHOT_FORMAT_VERSION = 2;
const STEPS_PER_RUN = 25;
const LIST_LIMIT = 1000;

interface PageEntry {
  name: string;
  records: number;
  bytes: number;
  sha256: string;
}

interface Snapshot {
  startSeq: number;
  endSeq: number;
  createdAtMs: number;
}

interface RunningSnapshot {
  startSeq: number;
  createdAtMs: number;
  cursor: string;
  nextPage: number;
  pages: PageEntry[];
  endSeq?: number;
  manifest?: string;
}

interface ExportState {
  baseline: boolean;
  descriptor: boolean;
  dueSeq: number | null;
  since: { seq: number; atMs: number };
  segment?: { first: number; last: number };
  running?: RunningSnapshot;
  done: Snapshot[];
  pruning?: number;
}

type Step = "segment" | "prune" | "cut" | "page" | "manifest" | "descriptor" | "start";

function seqName(seq: number): string {
  return String(seq).padStart(20, "0");
}

function readState(sql: Sql): ExportState {
  const stored = getMeta(sql, "export_state");
  if (stored === undefined) return { baseline: false, descriptor: false, dueSeq: null, since: { seq: 0, atMs: 0 }, done: [] };
  return JSON.parse(String(stored)) as ExportState;
}

function exportedSeq(sql: Sql): number {
  return Number(getMeta(sql, "exported_seq") ?? 0);
}

function scheduleExport(sql: Sql, dueMs: number, lowerOnly: boolean): void {
  sql.exec(
    `INSERT INTO schedule (task, due_ms) VALUES ('export', ?)
     ON CONFLICT (task) DO UPDATE SET due_ms = ${lowerOnly ? "MIN(due_ms, excluded.due_ms)" : "excluded.due_ms"}`,
    dueMs,
  );
}

export function startExport(sql: Sql, fromSeq: number, atMs: number, baseline: boolean): void {
  setMeta(sql, "exported_seq", fromSeq);
  const state: ExportState = { baseline, descriptor: false, dueSeq: baseline ? fromSeq : null, since: { seq: fromSeq, atMs }, done: [] };
  setMeta(sql, "export_state", JSON.stringify(state));
  scheduleExport(sql, atMs, false);
}

export function bootstrapExport(sql: Sql, atMs: number): void {
  if (getMeta(sql, "space_id") === undefined || getMeta(sql, "export_state") !== undefined) return;
  const restoreSeq = getMeta(sql, "restore_seq");
  const fromSeq = Number(restoreSeq ?? 0);
  startExport(sql, fromSeq, atMs, restoreSeq !== undefined);
  const state = readState(sql);
  state.dueSeq ??= Number(getMeta(sql, "head_seq"));
  setMeta(sql, "export_state", JSON.stringify(state));
}

export function requestSnapshot(sql: Sql, atMs: number): void {
  if (getMeta(sql, "export_state") === undefined) return;
  const state = readState(sql);
  const head = Number(getMeta(sql, "head_seq"));
  state.running = undefined;
  state.dueSeq = state.done.some((done) => done.startSeq === head) ? head + 1 : head;
  setMeta(sql, "export_state", JSON.stringify(state));
  scheduleExport(sql, atMs, true);
}

export function descriptorDurable(sql: Sql): boolean {
  return readState(sql).descriptor;
}

export function noteCommitted(sql: Sql, record: CommitRecord): boolean {
  const seq = Number(record.seq);
  const atMs = Number(record.committedAtMs);
  const waiting = seq - exportedSeq(sql);
  let dueMs = waiting >= SEGMENT_COMMITS ? atMs : waiting === 1 ? atMs + SEGMENT_DELAY_MS : undefined;
  const large = record.changes.some(
    ({ change }) => change.case === "node" && change.value.current?.seq === record.seq && (change.value.current.content?.chunkCount ?? 0) > INLINE_CHUNKS,
  );
  if (large) {
    const state = readState(sql);
    state.dueSeq = Math.max(state.dueSeq ?? seq, seq);
    setMeta(sql, "export_state", JSON.stringify(state));
    dueMs = atMs;
  }
  if (dueMs === undefined) return false;
  scheduleExport(sql, dueMs, true);
  return true;
}

async function gzip(data: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([data]).stream().pipeThrough(new CompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

export class Exporter {
  constructor(
    private readonly storage: Database,
    private readonly bucket: ObjectStore,
    private readonly pageNodes: number,
  ) {}

  private get sql(): Sql {
    return this.storage.sql;
  }

  async run(nowMs: number): Promise<void> {
    for (let i = 0; i < STEPS_PER_RUN; i++) {
      const step = this.next(nowMs);
      if (!step) break;
      await this.take(step, nowMs);
    }
    const dueMs = this.next(nowMs) ? nowMs : this.nextDue();
    if (dueMs === undefined) this.sql.exec("DELETE FROM schedule WHERE task = 'export'");
    else scheduleExport(this.sql, dueMs, false);
  }

  private get headSeq(): number {
    return Number(getMeta(this.sql, "head_seq"));
  }

  private get spaceId(): string {
    return String(getMeta(this.sql, "space_id"));
  }

  private get historyId(): string {
    return String(getMeta(this.sql, "history_id"));
  }

  private key(path: string): string {
    return `spaces/${this.spaceId}/meta/${this.historyId}/${path}`;
  }

  private update(change: (state: ExportState) => void): void {
    this.storage.transactionSync(() => {
      const state = readState(this.sql);
      change(state);
      setMeta(this.sql, "export_state", JSON.stringify(state));
    });
  }

  private firstWaitingMs(): number {
    return Number(this.sql.exec("SELECT committed_at_ms FROM commits WHERE seq = ?", exportedSeq(this.sql) + 1).one().committed_at_ms);
  }

  private next(nowMs: number): Step | undefined {
    const state = readState(this.sql);
    const head = this.headSeq;
    const exported = exportedSeq(this.sql);
    const running = state.running;
    if (state.segment) return "segment";
    if (state.pruning !== undefined) return "prune";
    if (
      head > exported &&
      (head - exported >= SEGMENT_COMMITS ||
        this.firstWaitingMs() + SEGMENT_DELAY_MS <= nowMs ||
        (running?.endSeq !== undefined && exported < running.endSeq))
    ) {
      return "cut";
    }
    if (running) return running.endSeq === undefined ? "page" : "manifest";
    if (!state.descriptor && (!state.baseline || state.done.length > 0)) return "descriptor";
    const { since } = state;
    if ((state.dueSeq !== null && head >= state.dueSeq) || (head > since.seq && (nowMs - since.atMs >= DAY_MS || head - since.seq >= SNAPSHOT_COMMITS))) return "start";
    return undefined;
  }

  private nextDue(): number | undefined {
    const state = readState(this.sql);
    const head = this.headSeq;
    const due: number[] = [];
    if (head > exportedSeq(this.sql)) due.push(this.firstWaitingMs() + SEGMENT_DELAY_MS);
    if (head > state.since.seq) due.push(state.since.atMs + DAY_MS);
    return due.length > 0 ? Math.min(...due) : undefined;
  }

  private async take(step: Step, nowMs: number): Promise<void> {
    switch (step) {
      case "segment":
        return this.writeSegment();
      case "prune":
        return this.prune();
      case "cut":
        return this.cutSegment();
      case "page":
        return this.writePage();
      case "manifest":
        return this.writeManifest();
      case "descriptor":
        return this.writeDescriptor();
      case "start": {
        const head = this.headSeq;
        return this.update((state) => {
          state.running = { startSeq: head, createdAtMs: nowMs, cursor: "", nextPage: 0, pages: [] };
        });
      }
    }
  }

  private header(format: string, formatVersion: number): Uint8Array {
    return toBinary(ExportHeaderSchema, create(ExportHeaderSchema, { format, formatVersion, spaceId: this.spaceId, historyId: this.historyId }));
  }

  private cutSegment(): void {
    const first = exportedSeq(this.sql) + 1;
    const found = this.sql.exec("SELECT seq, length(record) AS bytes FROM commits WHERE seq >= ? ORDER BY seq LIMIT ?", first, SEGMENT_COMMITS).toArray();
    if (Number(found[0]?.seq) !== first) throw new Error(`commit ${first} is missing from the log`);
    let last = first;
    let bytes = 0;
    for (const row of found) {
      bytes += Number(row.bytes);
      if (Number(row.seq) > first && bytes > SEGMENT_BYTES) break;
      last = Number(row.seq);
    }
    this.update((state) => {
      state.segment = { first, last };
    });
  }

  private async writeSegment(): Promise<void> {
    const { first, last } = readState(this.sql).segment!;
    const writer = new BinaryWriter();
    writer.bytes(this.header("koidrive.log", 1));
    let expected = first;
    for (const row of this.sql.exec("SELECT seq, record FROM commits WHERE seq BETWEEN ? AND ? ORDER BY seq", first, last)) {
      if (Number(row.seq) !== expected++) throw new Error(`commit ${expected - 1} is missing from the log`);
      writer.bytes(new Uint8Array(row.record as ArrayBuffer));
    }
    if (expected !== last + 1) throw new Error(`commit ${expected} is missing from the log`);
    await this.bucket.put(this.key(`log/${seqName(first)}-${seqName(last)}.fblog`), await gzip(writer.finish()));
    this.update((state) => {
      state.segment = undefined;
      setMeta(this.sql, "exported_seq", last);
    });
  }

  private nodeSnapshot(node: NodeRow): Uint8Array {
    const previous = rows<VersionRow>(
      this.sql.exec("SELECT * FROM versions WHERE node_id = ? AND version_id IS NOT ? ORDER BY seq, version_id", node.node_id, node.cur_version_id),
    ).map((version) => create(RetainedVersionSchema, { version: versionInfo(this.sql, version, true), supersededAtMs: BigInt(version.superseded_at_ms ?? 0) }));
    return toBinary(
      NodeSnapshotSchema,
      create(NodeSnapshotSchema, { node: nodeState(this.sql, node, true), previous, trashedAtMs: BigInt(node.trashed_at_ms ?? 0) }),
    );
  }

  private async writePage(): Promise<void> {
    const running = readState(this.sql).running!;
    const head = this.headSeq;
    const found = rows<{ node_id: string; purged: number }>(
      this.sql.exec(
        `SELECT node_id, 0 AS purged FROM nodes WHERE node_id > ? UNION ALL SELECT node_id, 1 AS purged FROM tombstones WHERE node_id > ?
         ORDER BY node_id LIMIT ?`,
        running.cursor,
        running.cursor,
        this.pageNodes,
      ),
    );
    const writer = new BinaryWriter();
    writer.bytes(this.header("koidrive.snapshot", SNAPSHOT_FORMAT_VERSION));
    let records = 0;
    let bytes = 0;
    for (const entry of found) {
      const record = entry.purged
        ? toBinary(NodeSnapshotSchema, create(NodeSnapshotSchema, { tombstone: tombstoneRecord(this.sql, entry.node_id) }))
        : this.nodeSnapshot(getNode(this.sql, entry.node_id)!);
      if (records > 0 && bytes + record.length > PAGE_BYTES) break;
      writer.bytes(record);
      records++;
      bytes += record.length;
    }
    const last = records === found.length && found.length < this.pageNodes;
    if (records === 0) {
      return this.update((state) => {
        state.running!.endSeq = head;
      });
    }
    const page = running.nextPage;
    this.update((state) => {
      state.running!.nextPage = page + 1;
    });
    const file = await gzip(writer.finish());
    await this.bucket.put(this.key(`snap/${seqName(running.startSeq)}/pages/${page}.fbsnap`), file);
    const entry: PageEntry = { name: `${page}.fbsnap`, records, bytes: file.length, sha256: toHex(await sha256(file)) };
    this.update((state) => {
      const current = state.running!;
      current.pages.push(entry);
      current.cursor = found[records - 1]!.node_id;
      if (last) current.endSeq = head;
    });
  }

  private async writeManifest(): Promise<void> {
    let running = readState(this.sql).running!;
    if (running.manifest === undefined) {
      const manifest = JSON.stringify({
        format: "koidrive.snapshot",
        format_version: SNAPSHOT_FORMAT_VERSION,
        space_id: this.spaceId,
        history_id: this.historyId,
        start_seq: running.startSeq,
        end_seq: running.endSeq,
        created_at: new Date(running.createdAtMs).toISOString(),
        pages: running.pages,
      });
      this.update((state) => {
        state.running!.manifest = manifest;
      });
      running = readState(this.sql).running!;
    }
    await this.bucket.put(this.key(`snap/${seqName(running.startSeq)}/manifest.json`), running.manifest!);
    this.update((state) => {
      const { startSeq, endSeq, createdAtMs } = state.running!;
      state.running = undefined;
      state.done.push({ startSeq, endSeq: endSeq!, createdAtMs });
      state.since = { seq: endSeq!, atMs: createdAtMs };
      if (state.dueSeq !== null && startSeq >= state.dueSeq) state.dueSeq = null;
      if (state.done.length > KEPT_SNAPSHOTS) state.pruning = state.done.shift()!.startSeq;
    });
  }

  private async prune(): Promise<void> {
    const dir = this.key(`snap/${seqName(readState(this.sql).pruning!)}/`);
    await this.bucket.delete(dir + "manifest.json");
    const listed = await this.bucket.list({ prefix: dir + "pages/", limit: LIST_LIMIT });
    if (listed.objects.length > 0) await this.bucket.delete(listed.objects.map((object) => object.key));
    if (listed.truncated) return;
    this.update((state) => {
      state.pruning = undefined;
    });
  }

  private async writeDescriptor(): Promise<void> {
    const spaceId = this.spaceId;
    const descriptor = {
      format: "koidrive.space",
      format_version: 1,
      space_id: spaceId,
      name: String(getMeta(this.sql, "name")),
      created_at: new Date(uuidv7Ms(spaceId)).toISOString(),
      hash_alg: "sha256",
      chunk_key: "spaces/{space_id}/chunks/{hex2}/{hex}/{incarnation}",
      chunk_encoding: "raw",
      history_id: this.historyId,
      rewind_number: Number(getMeta(this.sql, "rewind_number") ?? 0),
    };
    await this.bucket.put(this.key("space.json"), JSON.stringify(descriptor));
    this.update((state) => {
      state.descriptor = true;
    });
  }
}
