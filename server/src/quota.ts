// SPDX-License-Identifier: AGPL-3.0-or-later

import { causeOf } from "./cause";
import { Error_Cause } from "./gen/koidrive/v1/rpc_pb";

const DAY_MS = 86_400_000;
const RESET_LAG_MS = 30 * 60_000;
const RESET_LAG_RETRY_MS = 5 * 60_000;

export interface QuotaRefusal {
  message: string;
  retryAfterMs: number;
  cause: Error_Cause;
}

/**
 * The wait runs until the next 00:00 UTC, when free-plan daily limits reset, except in the first 30 minutes after it,
 * because the errors can outlast the reset.
 */
export function quotaRefusal(error: unknown, nowMs: number): QuotaRefusal | undefined {
  const cause = causeOf(error);
  const sinceMidnightMs = nowMs % DAY_MS;
  const retryAfterMs = sinceMidnightMs < RESET_LAG_MS ? RESET_LAG_RETRY_MS : DAY_MS - sinceMidnightMs;
  if (cause === Error_Cause.DAILY_LIMIT) return { message: "the server reached a Workers Free daily limit; it resets at 00:00 UTC", retryAfterMs, cause };
  if (cause === Error_Cause.STORAGE_FULL) return { message: "the server's storage is full", retryAfterMs, cause };
  return undefined;
}
