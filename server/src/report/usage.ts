// SPDX-License-Identifier: AGPL-3.0-or-later

import { errorText } from "../errors";

export const LATENCY_BUCKETS = 16;
export const QUIET_MS = 25_000;
export const BATCH_MS = 600_000;
const HOUR_MS = 3_600_000;

export interface Usage {
  counts: Record<string, number>;
  latency: Record<string, number[]>;
}

export type UsageHours = Record<number, Usage>;

export interface UsageBatch {
  openedMs: number;
  hours: UsageHours;
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
  return { counts: {}, latency: {} };
}

export function mergeUsage(into: Usage, from: Usage): Usage {
  for (const [key, count] of Object.entries(from.counts)) into.counts[key] = (into.counts[key] ?? 0) + count;
  for (const [route, buckets] of Object.entries(from.latency)) {
    const sum = (into.latency[route] ??= new Array<number>(LATENCY_BUCKETS).fill(0));
    buckets.forEach((count, index) => (sum[index]! += count));
  }
  return into;
}

export function mergeBatch(into: UsageBatch | undefined, from: UsageBatch): UsageBatch {
  const merged = into ?? { openedMs: from.openedMs, hours: {} };
  merged.openedMs = Math.min(merged.openedMs, from.openedMs);
  for (const [hour, usage] of Object.entries(from.hours)) mergeUsage((merged.hours[Number(hour)] ??= emptyUsage()), usage);
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
  private lastMs = 0;
  private reportingNow: boolean | undefined;

  get reporting(): boolean | undefined {
    return this.reportingNow;
  }

  set reporting(on: boolean | undefined) {
    this.reportingNow = on;
    if (on === false) this.batch = undefined;
  }

  count(route: string, outcome: string, latencyMs: number, nowMs: number): void {
    if (this.reporting !== true) return;
    this.batch ??= { openedMs: nowMs, hours: {} };
    const usage = (this.batch.hours[Math.floor(nowMs / HOUR_MS)] ??= emptyUsage());
    const key = route + "." + outcome;
    usage.counts[key] = (usage.counts[key] ?? 0) + 1;
    (usage.latency[route] ??= new Array<number>(LATENCY_BUCKETS).fill(0))[latencyBucket(latencyMs)]! += 1;
    this.lastMs = Math.max(this.lastMs, nowMs);
  }

  async settle(record: (hours: UsageHours) => Promise<boolean>, now: () => number, wait: (ms: number) => Promise<unknown>): Promise<void> {
    if (this.reporting !== true) return;
    if (!this.due(now())) {
      await wait(UsageTally.quietMs);
      const nowMs = now();
      if (!this.due(nowMs) && nowMs - this.lastMs < UsageTally.quietMs) return;
    }
    await this.handOver(record);
  }

  private due(nowMs: number): boolean {
    return this.batch !== undefined && batchDue(this.batch, nowMs);
  }

  take(): UsageBatch | undefined {
    const batch = this.batch;
    this.batch = undefined;
    return batch;
  }

  put(batch: UsageBatch): void {
    if (this.reporting === false) return;
    this.batch = mergeBatch(this.batch, batch);
  }

  async handOver(record: (hours: UsageHours) => Promise<boolean>): Promise<void> {
    const batch = this.take();
    if (!batch) return;
    try {
      this.reporting = await record(batch.hours);
    } catch (error) {
      console.error("usage flush failed", errorText(error));
      this.put(batch);
    }
  }
}
