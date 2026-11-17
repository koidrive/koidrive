// SPDX-License-Identifier: AGPL-3.0-or-later

export const LATENCY_BUCKETS = 16;
export const FLUSH_MS = 60_000;
const HOUR_MS = 3_600_000;

export interface Usage {
  counts: Record<string, number>;
  latency: Record<string, number[]>;
}

export type UsageHours = Record<number, Usage>;

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

export class UsageTally {
  private hours: UsageHours = {};
  private empty = true;
  private flushedAtMs = 0;
  private reportingNow: boolean | undefined;

  get reporting(): boolean | undefined {
    return this.reportingNow;
  }

  set reporting(on: boolean | undefined) {
    this.reportingNow = on;
    if (on !== false) return;
    this.hours = {};
    this.empty = true;
  }

  count(route: string, outcome: string, latencyMs: number, nowMs: number): void {
    if (this.reporting === false) return;
    const usage = (this.hours[Math.floor(nowMs / HOUR_MS)] ??= emptyUsage());
    const key = route + "." + outcome;
    usage.counts[key] = (usage.counts[key] ?? 0) + 1;
    (usage.latency[route] ??= new Array<number>(LATENCY_BUCKETS).fill(0))[latencyBucket(latencyMs)]! += 1;
    this.empty = false;
  }

  take(nowMs: number): UsageHours | undefined {
    if (this.empty || this.reporting === false || nowMs - this.flushedAtMs < FLUSH_MS) return undefined;
    const hours = this.hours;
    this.hours = {};
    this.empty = true;
    this.flushedAtMs = nowMs;
    return hours;
  }
}
