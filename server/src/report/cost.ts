// SPDX-License-Identifier: AGPL-3.0-or-later

import type { Counter, Stored } from "./usage";

const WORKERS = "https://developers.cloudflare.com/workers/platform/pricing/";
const DURABLE_OBJECTS = "https://developers.cloudflare.com/durable-objects/platform/pricing/";
const R2 = "https://developers.cloudflare.com/r2/pricing/";

export const MONTH_HOURS = 30 * 24;
const GB = 1e9;

type Free = { perDay: number } | { perMonth: number } | { total: number };

interface Price {
  source: string;
  unit: number;
  usdPerUnit: number;
  included: number;
  roundsUp: boolean;
  free?: Free;
}

export const PRICING = {
  subscription: { source: WORKERS, usdPerMonth: 5 },
  objectMemory: { source: DURABLE_OBJECTS, gb: 0.128 },
  webSocket: { source: DURABLE_OBJECTS, messagesPerRequest: 20 },
  lines: {
    worker_requests: { source: WORKERS, unit: 1e6, usdPerUnit: 0.3, included: 10e6, roundsUp: false, free: { perDay: 100_000 } },
    do_requests: { source: DURABLE_OBJECTS, unit: 1e6, usdPerUnit: 0.15, included: 1e6, roundsUp: true, free: { perDay: 100_000 } },
    do_gb_s: { source: DURABLE_OBJECTS, unit: 1e6, usdPerUnit: 12.5, included: 400_000, roundsUp: true, free: { perDay: 13_000 } },
    do_rows_read: { source: DURABLE_OBJECTS, unit: 1e6, usdPerUnit: 0.001, included: 25e9, roundsUp: true, free: { perDay: 5e6 } },
    do_rows_written: { source: DURABLE_OBJECTS, unit: 1e6, usdPerUnit: 1, included: 50e6, roundsUp: true, free: { perDay: 100_000 } },
    do_stored_gb: { source: DURABLE_OBJECTS, unit: 1, usdPerUnit: 0.2, included: 5, roundsUp: true, free: { total: 5 } },
    r2_class_a: { source: R2, unit: 1e6, usdPerUnit: 4.5, included: 1e6, roundsUp: true, free: { perMonth: 1e6 } },
    r2_class_b: { source: R2, unit: 1e6, usdPerUnit: 0.36, included: 10e6, roundsUp: true, free: { perMonth: 10e6 } },
    r2_stored_gb: { source: R2, unit: 1, usdPerUnit: 0.015, included: 10, roundsUp: true, free: { perMonth: 10 } },
  } satisfies Record<string, Price>,
} as const;

export type Line = keyof typeof PRICING.lines;

export interface BillingWindow {
  hours: number;
  billing: Partial<Record<Counter, number>>;
  stored: Stored;
}

export interface CostEstimate {
  usd: number;
  lines: Record<Line, { monthly: number; usd: number }>;
  freeShare: Partial<Record<Line, number>>;
}

function monthly(window: BillingWindow): Record<Line, number> {
  const count = (counter: Counter) => (window.billing[counter] ?? 0) * (MONTH_HOURS / window.hours);
  return {
    worker_requests: count("worker_requests"),
    do_requests: count("do_account_requests") + count("do_space_requests") + count("do_ws_messages") / PRICING.webSocket.messagesPerRequest,
    do_gb_s: ((count("do_account_active_ms") + count("do_space_active_ms")) / 1000) * PRICING.objectMemory.gb,
    do_rows_read: count("do_rows_read"),
    do_rows_written: count("do_rows_written"),
    do_stored_gb: window.stored.do_stored_bytes / GB,
    r2_class_a: count("r2_class_a"),
    r2_class_b: count("r2_class_b"),
    r2_stored_gb: window.stored.r2_stored_bytes / GB,
  };
}

function freeShare(free: Free, month: number): number {
  if ("perDay" in free) return (month * 24) / MONTH_HOURS / free.perDay;
  if ("perMonth" in free) return month / free.perMonth;
  return month / free.total;
}

export function estimateMonthlyCost(window: BillingWindow): CostEstimate {
  const month = monthly(window);
  const lines = {} as CostEstimate["lines"];
  const shares: CostEstimate["freeShare"] = {};
  let usd = PRICING.subscription.usdPerMonth;
  for (const line of Object.keys(PRICING.lines) as Line[]) {
    const price: Price = PRICING.lines[line];
    const over = Math.max(0, month[line] - price.included) / price.unit;
    const lineUsd = (price.roundsUp ? Math.ceil(over) : over) * price.usdPerUnit;
    lines[line] = { monthly: month[line], usd: lineUsd };
    usd += lineUsd;
    if (price.free) shares[line] = freeShare(price.free, month[line]);
  }
  return { usd, lines, freeShare: shares };
}
