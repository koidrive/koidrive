// SPDX-License-Identifier: AGPL-3.0-or-later

import { Error_Cause } from "./gen/koidrive/v1/rpc_pb";

/**
 * The runtime reports most platform failures only in the error message. R2 appends its numeric error code in
 * parentheses (https://developers.cloudflare.com/r2/api/error-codes/), and Durable Object errors carry the `overloaded`
 * and `retryable` properties (https://developers.cloudflare.com/durable-objects/best-practices/error-handling/).
 * SQLITE_FULL is the text the Durable Objects limits FAQ documents. The free-tier text is not documented, and the four
 * kinds come from user reports.
 */
const MESSAGES: [RegExp, Error_Cause][] = [
  [/\bExceeded allowed (?:rows written|rows read|duration|volume of requests) in Durable Objects free tier\b/, Error_Cause.DAILY_LIMIT],
  [/\bdatabase or disk is full: SQLITE_FULL\b/, Error_Cause.STORAGE_FULL],
  [/\bToo many subrequests\b/i, Error_Cause.SUBREQUEST_LIMIT],
  [/\bexceeded (?:its )?CPU time limit\b/i, Error_Cause.CPU_EXCEEDED],
  [/\bexceeded (?:its )?memory limit\b|\bMemory limit would be exceeded\b/i, Error_Cause.MEMORY_EXCEEDED],
];

const R2_CODE = /\((1\d{4,5})\)\s*$/;

function r2Cause(code: number): Error_Cause {
  if (code === 10058) return Error_Cause.R2_RATE_LIMITED;
  if (code === 10001 || code === 10043) return Error_Cause.R2_UNAVAILABLE;
  return Error_Cause.R2_ERROR;
}

export function causeOf(error: unknown): Error_Cause {
  const text = error instanceof Error ? error.message : String(error);
  for (const [pattern, cause] of MESSAGES) if (pattern.test(text)) return cause;
  const r2 = R2_CODE.exec(text);
  if (r2) return r2Cause(Number(r2[1]));
  const flags = typeof error === "object" && error !== null ? (error as { overloaded?: unknown; retryable?: unknown }) : {};
  if (flags.overloaded === true) return Error_Cause.DO_OVERLOADED;
  if (flags.retryable === true) return Error_Cause.DO_UNAVAILABLE;
  return Error_Cause.EXCEPTION;
}
