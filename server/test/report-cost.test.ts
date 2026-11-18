// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { type BillingWindow, estimateMonthlyCost, type Line, PRICING } from "../src/report/cost";
import type { Counter, Stored } from "../src/report/usage";

const GB = 1e9;
const NOTHING_STORED: Stored = { do_stored_bytes: 0, r2_stored_bytes: 0, r2_objects: 0 };

function window(hours: number, billing: Partial<Record<Counter, number>>, stored: Partial<Stored> = {}): BillingWindow {
  return { hours, billing, stored: { ...NOTHING_STORED, ...stored } };
}

function charged(estimate: ReturnType<typeof estimateMonthlyCost>): Partial<Record<Line, number>> {
  return Object.fromEntries(Object.entries(estimate.lines).filter(([, line]) => line.usd > 0).map(([name, line]) => [name, line.usd]));
}

describe("monthly cost estimate", () => {
  it("keeps every price and allowance in one table that names its page", () => {
    for (const row of [PRICING.subscription, PRICING.objectMemory, PRICING.webSocket, ...Object.values(PRICING.lines)]) {
      expect(row.source).toMatch(/^https:\/\/developers\.cloudflare\.com\/(workers|durable-objects|r2)\/(platform\/)?pricing\/$/);
    }
  });

  it("charges only the subscription for a day that stays inside every allowance, and shows each Free plan limit it uses", () => {
    const day = window(
      24,
      {
        worker_requests: 100_000,
        do_account_requests: 10_000,
        do_space_requests: 10_000,
        do_ws_messages: 200_000,
        do_space_active_ms: 101_562_500,
        do_rows_read: 5_000_000,
        do_rows_written: 100_000,
        r2_class_a: 33_000,
        r2_class_b: 330_000,
      },
      { do_stored_bytes: 5 * GB, r2_stored_bytes: 10 * GB },
    );
    const estimate = estimateMonthlyCost(day);
    expect(estimate.usd).toBe(5);
    expect(charged(estimate)).toEqual({});
    expect(estimate.lines.do_requests.monthly).toBe(900_000);
    expect(estimate.lines.do_gb_s.monthly).toBe(390_000);
    expect(estimate.freeShare).toEqual({
      worker_requests: 1,
      do_requests: 0.3,
      do_gb_s: 1,
      do_rows_read: 1,
      do_rows_written: 1,
      do_stored_gb: 1,
      r2_class_a: 0.99,
      r2_class_b: 0.99,
      r2_stored_gb: 1,
    });
  });

  it.each<[string, BillingWindow, Line, number]>([
    ["Worker requests", window(24, { worker_requests: 500_000 }), "worker_requests", 1.5],
    ["Durable Object requests", window(1, { do_space_requests: 2_000 }), "do_requests", 0.15],
    ["Durable Object duration", window(1, { do_account_active_ms: 5_000_000 }), "do_gb_s", 12.5],
    ["rows read", window(1, { do_rows_read: 40_000_000 }), "do_rows_read", 3.8],
    ["rows written", window(24, { do_rows_written: 2_000_000 }), "do_rows_written", 10],
    ["SQL storage", window(1, {}, { do_stored_bytes: 6 * GB }), "do_stored_gb", 0.2],
    ["R2 Class A", window(24, { r2_class_a: 50_000 }), "r2_class_a", 4.5],
    ["R2 Class B", window(24, { r2_class_b: 500_000 }), "r2_class_b", 1.8],
    ["R2 storage", window(24, {}, { r2_stored_bytes: 12 * GB }), "r2_stored_gb", 0.03],
  ])("charges %s past its allowance", (_name, used, line, usd) => {
    const estimate = estimateMonthlyCost(used);
    expect(Object.keys(charged(estimate))).toEqual([line]);
    expect(estimate.lines[line].usd).toBeCloseTo(usd, 10);
    expect(estimate.usd).toBeCloseTo(5 + usd, 10);
  });

  it("rounds billable Durable Object and R2 usage up to the next unit, and Workers usage not at all", () => {
    const month = (billing: Partial<Record<Counter, number>>, stored: Partial<Stored> = {}) => estimateMonthlyCost(window(30 * 24, billing, stored));
    expect(month({ do_space_active_ms: 7_031_250_000 }).lines.do_gb_s).toEqual({ monthly: 900_000, usd: 12.5 });
    expect(month({ do_rows_read: 25e9 + 1 }).lines.do_rows_read.usd).toBeCloseTo(0.001, 10);
    expect(month({ do_rows_written: 50e6 + 1 }).lines.do_rows_written.usd).toBe(1);
    expect(month({}, { do_stored_bytes: 5.5 * GB }).lines.do_stored_gb.usd).toBeCloseTo(0.2, 10);
    expect(month({ r2_class_a: 2_000_001 }).lines.r2_class_a.usd).toBe(9);
    expect(month({}, { r2_stored_bytes: 11.1 * GB }).lines.r2_stored_gb.usd).toBeCloseTo(0.03, 10);
    expect(month({ worker_requests: 10_000_001 }).lines.worker_requests.usd).toBeCloseTo(0.0000003, 12);
  });

  it("keeps the report's own hand-overs, alarms, gauges and flag reads for one busy isolate and one busy space under 1% of each Free daily limit", () => {
    const handOvers = { worker: 144, space: 144, alarm: 1 };
    const usageAlarms = 24;
    const accountAlarms = 24;
    const wakes = { space: 86_400 / 10, account: 86_400 / 10 };
    const accountCalls = handOvers.worker + handOvers.space + handOvers.alarm;
    const overhead = window(24, {
      do_account_requests: accountCalls + accountAlarms,
      do_space_requests: usageAlarms,
      do_rows_read: wakes.space * 2 + wakes.account + accountCalls * 2,
      do_rows_written: accountCalls + 24 * 2 + 24 * 2 + usageAlarms * 4 + 1 + accountAlarms,
      do_account_active_ms: accountCalls * 5,
      do_space_active_ms: handOvers.space * 20 + (usageAlarms + handOvers.alarm) * 10,
    });
    const shares = estimateMonthlyCost(overhead).freeShare;
    expect(shares.do_requests).toBeCloseTo(0.00337, 10);
    expect(shares.do_rows_read).toBeCloseTo(0.0052996, 10);
    expect(shares.do_rows_written).toBeCloseTo(0.00506, 10);
    expect(shares.do_gb_s).toBeLessThan(0.0001);
  });

  it("measures stored data in GB of 10^9 bytes and duration at 128 MB as 0.128 GB, as Cloudflare's pricing examples do", () => {
    const month = (billing: Partial<Record<Counter, number>>, stored: Partial<Stored> = {}) => estimateMonthlyCost(window(30 * 24, billing, stored));
    expect(month({}, { r2_stored_bytes: 10_500_000_000 }).lines.r2_stored_gb.usd).toBeCloseTo(0.015, 10);
    expect(month({}, { do_stored_bytes: 5_500_000_000 }).lines.do_stored_gb.usd).toBeCloseTo(0.2, 10);
    expect(month({ do_space_active_ms: 1_000_000_000 }).lines.do_gb_s.monthly).toBeCloseTo(128_000, 6);
    expect(month({ do_space_active_ms: 86_400_000_000 }).lines.do_gb_s.usd).toBe(137.5);
  });

  it("bills incoming WebSocket messages as one request per 20", () => {
    const estimate = estimateMonthlyCost(window(30 * 24, { do_ws_messages: 40_000_000 }));
    expect(estimate.lines.do_requests).toEqual({ monthly: 2_000_000, usd: 0.15 });
  });
});
