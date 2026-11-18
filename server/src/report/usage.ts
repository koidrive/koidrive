// SPDX-License-Identifier: AGPL-3.0-or-later

import { errorText } from "../errors";

export const LATENCY_BUCKETS = 16;
export const QUIET_MS = 25_000;
export const BATCH_MS = 600_000;
const HOUR_MS = 3_600_000;

export const COUNTERS = [
  "worker_requests",
  "egress_bytes",
  "do_account_requests",
  "do_account_active_ms",
  "do_space_requests",
  "do_space_active_ms",
  "do_ws_messages",
  "do_rows_read",
  "do_rows_written",
  "r2_class_a",
  "r2_class_b",
] as const;
export type Counter = (typeof COUNTERS)[number];

export const GAUGES = ["do_stored_bytes", "r2_stored_bytes", "r2_objects"] as const;
export type Gauge = (typeof GAUGES)[number];
export type Stored = Record<Gauge, number>;

export interface ObjectStored {
  object: string;
  sqlBytes: number;
  r2Bytes: number;
  r2Objects: number;
}

export interface Usage {
  counts: Record<string, number>;
  latency: Record<string, number[]>;
  billing: Partial<Record<Counter, number>>;
}

export type UsageHours = Record<number, Usage>;

export interface UsageBatch {
  openedMs: number;
  hours: UsageHours;
  stored?: ObjectStored[];
}

export interface Carried {
  usage?: UsageBatch;
  reporting?: boolean;
  startedMs?: number;
}

export interface Counted {
  route: string;
  outcome: string;
  latencyMs: number;
}

export function latencyBucket(ms: number): number {
  if (!(ms >= 1)) return 0;
  return Math.min(LATENCY_BUCKETS - 1, Math.floor(Math.log2(ms)) + 1);
}

export function emptyUsage(): Usage {
  return { counts: {}, latency: {}, billing: {} };
}

export function storedUsage(text: string): Usage {
  return { ...emptyUsage(), ...(JSON.parse(text) as Partial<Usage>) };
}

export function mergeUsage(into: Usage, from: Usage): Usage {
  for (const [key, count] of Object.entries(from.counts)) into.counts[key] = (into.counts[key] ?? 0) + count;
  for (const [route, buckets] of Object.entries(from.latency)) {
    const sum = (into.latency[route] ??= new Array<number>(LATENCY_BUCKETS).fill(0));
    buckets.forEach((count, index) => (sum[index]! += count));
  }
  const billing = (into.billing ??= {});
  for (const counter of COUNTERS) {
    const amount = from.billing?.[counter];
    if (amount !== undefined) billing[counter] = (billing[counter] ?? 0) + amount;
  }
  return into;
}

export function mergeBatch(into: UsageBatch | undefined, from: UsageBatch): UsageBatch {
  const merged = into ?? { openedMs: from.openedMs, hours: {} };
  merged.openedMs = Math.min(merged.openedMs, from.openedMs);
  for (const [hour, usage] of Object.entries(from.hours)) mergeUsage((merged.hours[Number(hour)] ??= emptyUsage()), usage);
  if (from.stored) merged.stored = [...(merged.stored ?? []).filter((one) => !from.stored!.some((other) => other.object === one.object)), ...from.stored];
  return merged;
}

export function batchDue(batch: UsageBatch, nowMs: number): boolean {
  return nowMs - batch.openedMs >= UsageTally.batchMs || Math.floor(nowMs / HOUR_MS) > Math.floor(batch.openedMs / HOUR_MS);
}

export function nextHourMs(nowMs: number): number {
  return (Math.floor(nowMs / HOUR_MS) + 1) * HOUR_MS;
}

export class UsageTally {
  static quietMs = QUIET_MS;
  static batchMs = BATCH_MS;
  private batch: UsageBatch | undefined;
  private kept: UsageBatch | undefined;
  private lastMs = 0;
  private reportingNow: boolean | undefined;
  private stops = 0;
  private startedAtMs: number | undefined;
  private busy = 0;
  private sinceMs = 0;

  constructor(
    private readonly now: () => number = () => Date.now(),
    private readonly active?: Counter,
  ) {}

  get reporting(): boolean | undefined {
    return this.reportingNow;
  }

  set reporting(on: boolean | undefined) {
    this.reportingNow = on;
    if (on !== false) return;
    this.batch = undefined;
    this.kept = undefined;
    this.stops++;
  }

  get startedMs(): number | undefined {
    return this.startedAtMs;
  }

  restart(startedMs: number | undefined): boolean {
    if (startedMs === undefined || startedMs === this.startedAtMs) return false;
    this.startedAtMs = startedMs;
    if (this.batch && this.batch.openedMs < startedMs) this.batch = undefined;
    if (this.kept && this.kept.openedMs < startedMs) this.kept = undefined;
    return true;
  }

  get idle(): boolean {
    return this.busy === 0;
  }

  private usage(nowMs: number): Usage | undefined {
    if (this.reporting === false) return undefined;
    this.batch ??= { openedMs: nowMs, hours: {} };
    return (this.batch.hours[Math.floor(nowMs / HOUR_MS)] ??= emptyUsage());
  }

  count(route: string, outcome: string, latencyMs: number, nowMs: number): void {
    const usage = this.usage(nowMs);
    if (!usage) return;
    const key = route + "." + outcome;
    usage.counts[key] = (usage.counts[key] ?? 0) + 1;
    (usage.latency[route] ??= new Array<number>(LATENCY_BUCKETS).fill(0))[latencyBucket(latencyMs)]! += 1;
    this.lastMs = Math.max(this.lastMs, nowMs);
  }

  add(counter: Counter, amount: number): void {
    if (amount <= 0) return;
    const usage = this.usage(this.now());
    if (usage) usage.billing[counter] = (usage.billing[counter] ?? 0) + amount;
  }

  note(stored: ObjectStored): void {
    if (this.reporting === false) return;
    this.put({ openedMs: this.now(), hours: {}, stored: [stored] });
  }

  enter(): void {
    if (this.busy++ === 0) this.sinceMs = Date.now();
  }

  lapse(): void {
    if (this.busy === 0 || !this.active) return;
    const nowMs = Date.now();
    this.add(this.active, nowMs - this.sinceMs);
    this.sinceMs = nowMs;
  }

  leave(): void {
    this.busy--;
  }

  receive(carried: Carried, authoritative: boolean): void {
    if (carried.reporting !== undefined && (authoritative || this.reporting === undefined)) this.reporting = carried.reporting;
    if (authoritative || this.startedAtMs === undefined) this.restart(carried.startedMs);
    if (carried.usage) this.put(carried.usage);
  }

  async settle(record: (batch: UsageBatch) => Promise<boolean>, now: () => number, wait: (ms: number) => Promise<unknown>): Promise<void> {
    if (this.reporting !== true) return;
    if (!this.due(now())) {
      await wait(UsageTally.quietMs);
      const nowMs = now();
      if (!this.due(nowMs) && nowMs - this.lastMs < UsageTally.quietMs) return;
    }
    await this.handOver(record);
  }

  due(nowMs: number): boolean {
    return this.batch !== undefined && batchDue(this.batch, nowMs);
  }

  take(): UsageBatch | undefined {
    const batch = this.batch;
    this.batch = undefined;
    return batch;
  }

  keep(): void {
    const batch = this.take();
    if (batch) this.kept = mergeBatch(this.kept, batch);
  }

  takeAll(): UsageBatch | undefined {
    const kept = this.kept;
    this.kept = undefined;
    const batch = this.take();
    return kept && batch ? mergeBatch(kept, batch) : (kept ?? batch);
  }

  put(batch: UsageBatch): void {
    if (this.reporting === false || batch.openedMs < (this.startedAtMs ?? -Infinity)) return;
    this.batch = mergeBatch(this.batch, batch);
  }

  async handOver(record: (batch: UsageBatch) => Promise<boolean>): Promise<void> {
    const batch = this.take();
    if (!batch) return;
    const stops = this.stops;
    try {
      const on = await record(batch);
      if (!on || this.stops === stops) this.reporting = on;
    } catch (error) {
      console.error("usage flush failed", errorText(error));
      if (this.stops === stops) this.put(batch);
    }
  }
}
