// SPDX-License-Identifier: AGPL-3.0-or-later

export const FORMAT_VERSION = 1;
export const HOUR_MS = 3_600_000;
export const DAY_MS = 24 * HOUR_MS;
export const PITR_WINDOW_MS = 30 * DAY_MS;
export const BOOKMARK_SAFETY_MS = HOUR_MS;
export const HOLD_MS = DAY_MS;
export const GRANT_MS = HOUR_MS;
export const MAX_OPS = 1000;
export const MAX_CHUNKS_PER_CALL = 1000;
/** A call stays well inside a Worker invocation's subrequest limit. */
export const MAX_HEAD_CHECKS = 50;
/** RFC 0003 section 5, rule 4. */
export const MIN_CHUNK_GRACE_DAYS = 30;
export const MAX_MISSING_LISTED = MAX_CHUNKS_PER_CALL;
export const MAX_CHUNK_BYTES = 16 * 1024 * 1024;
export const MAX_SERVER_FRAME = 1024 * 1024;
export const MAX_CLIENT_FRAME = 8 * 1024 * 1024;
export const MAX_REQ_ID = 2n ** 64n - 1n;
export const RETRY_LATER_MS = 60_000;
export const CLOCK_WAIT_MS = 50;
export const PAGE_ENVELOPE_BYTES = 1024;
export const GC_BATCH = 1000;
export const PURGE_BYTES = MAX_SERVER_FRAME - 64 * 1024;
export const GC_RETRY_MS = 10 * 60_000;
export const R2_DELETE_BATCH = 1000;
export const ORPHAN_SWEEP_MS = 7 * DAY_MS;
export const CLOSED_RECHECK_MS = 10 * 60_000;
export const EXPORT_RETRY_MS = 60_000;
export const MAX_PARKED_BYTES = 8192;
export const USAGE_GRACE_MS = 60_000;
