// SPDX-License-Identifier: AGPL-3.0-or-later

import { expect, it } from "vitest";
import { causeOf } from "../src/cause";
import { Error_Cause } from "../src/gen/koidrive/v1/rpc_pb";

const flagged = (message: string, flags: Record<string, boolean>) => Object.assign(new Error(message), flags);

it.each<[unknown, Error_Cause]>([
  [new Error("Exceeded allowed rows written in Durable Objects free tier."), Error_Cause.DAILY_LIMIT],
  [new Error("database or disk is full: SQLITE_FULL"), Error_Cause.STORAGE_FULL],
  [new Error("Too many subrequests."), Error_Cause.SUBREQUEST_LIMIT],
  [new Error("Too many subrequests by single Worker invocation. To configure this limit, refer to https://developers.cloudflare.com/workers/wrangler/configuration/#limits"), Error_Cause.SUBREQUEST_LIMIT],
  [flagged("Durable Object exceeded its CPU time limit and was reset.", { remote: true, retryable: true }), Error_Cause.CPU_EXCEEDED],
  [flagged("Durable Object exceeded its memory limit and was reset.", { remote: true, retryable: true }), Error_Cause.MEMORY_EXCEEDED],
  [new Error("Memory limit would be exceeded before EOF."), Error_Cause.MEMORY_EXCEEDED],
  [new Error("put: Reduce your concurrent request rate for the same object. (10058)"), Error_Cause.R2_RATE_LIMITED],
  [new Error("put: We encountered an internal error. Please try again. (10001)"), Error_Cause.R2_UNAVAILABLE],
  [new Error("get: Service is temporarily unavailable. (10043)"), Error_Cause.R2_UNAVAILABLE],
  [new Error("put: Your metadata headers exceed the maximum allowed metadata size. (10012)"), Error_Cause.R2_ERROR],
  [flagged("Durable Object is overloaded. Too many requests queued.", { overloaded: true }), Error_Cause.DO_OVERLOADED],
  [flagged("Network connection lost.", { retryable: true }), Error_Cause.DO_UNAVAILABLE],
  [new TypeError("Cannot read properties of undefined (reading 'nodeId')"), Error_Cause.EXCEPTION],
  ["a thrown string", Error_Cause.EXCEPTION],
  [undefined, Error_Cause.EXCEPTION],
])("reads %s as cause %i", (error, cause) => {
  expect(causeOf(error)).toBe(cause);
});
