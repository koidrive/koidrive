// SPDX-License-Identifier: AGPL-3.0-or-later

import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import type { Sql } from "../host";

export const CLAIM_FAILURES_PER_WINDOW = 10;
export const CLAIM_FAILURE_WINDOW_MS = 10 * 60_000;
export const PAIRING_FAILURES_PER_WINDOW = 20;
export const MAX_FAILURE_ROWS = 10_000;
export const NO_ADDRESS = "(no address)";
export const MIN_RECOVERY_KEY_LENGTH = 32;
export const MAX_SETUP_ATTEMPT_LENGTH = 64;
export const REDEEMED_CODE_REPLAY_MS = 25 * 3_600_000;
const UNUSED_OF_ATTEMPT = "setup_attempt = ? AND user_id = ? AND last_seen_ms IS NULL AND revoked_at_ms IS NULL";

export type ClaimOutcome = { ok: true; revoke: string[] } | { ok: true; earlierDevice: string } | { ok: false; code: Error_Code; message: string };

export interface ClaimAttempt {
  codeHash: ArrayBuffer;
  rawHash: ArrayBuffer;
  recoveryKeyHash: ArrayBuffer | undefined;
  client: string;
  nowMs: number;
  ownerBefore: string | undefined;
  userId: string;
  userName: string;
  deviceId: string;
  setupAttempt: string;
  requestId: string;
  storeDevice(): void;
}

function refused(message: string): ClaimOutcome {
  return { ok: false, code: Error_Code.FORBIDDEN, message };
}

function sameDigest(a: ArrayBuffer, b: ArrayBuffer): boolean {
  return a.byteLength === b.byteLength && crypto.subtle.timingSafeEqual(a, b);
}

function bucket(client: string): string {
  return client || NO_ADDRESS;
}

const FAILURES_PER_WINDOW = { claim: CLAIM_FAILURES_PER_WINDOW, recovery: CLAIM_FAILURES_PER_WINDOW, pair: PAIRING_FAILURES_PER_WINDOW };

type FailureScope = keyof typeof FAILURES_PER_WINDOW;

function throttled(sql: Sql, scope: FailureScope, client: string, nowMs: number): boolean {
  const row = sql
    .exec("SELECT failures FROM claim_failures WHERE scope = ? AND client = ? AND window_start_ms > ?", scope, bucket(client), nowMs - CLAIM_FAILURE_WINDOW_MS)
    .toArray()[0];
  return Number(row?.failures ?? 0) >= FAILURES_PER_WINDOW[scope];
}

function recordFailure(sql: Sql, scope: FailureScope, client: string, nowMs: number): void {
  sql.exec("DELETE FROM claim_failures WHERE window_start_ms <= ?", nowMs - CLAIM_FAILURE_WINDOW_MS);
  sql.exec(
    "INSERT INTO claim_failures (scope, client, failures, window_start_ms) VALUES (?, ?, 1, ?) ON CONFLICT (scope, client) DO UPDATE SET failures = failures + 1",
    scope,
    bucket(client),
    nowMs,
  );
  sql.exec(
    "DELETE FROM claim_failures WHERE rowid IN (SELECT rowid FROM claim_failures ORDER BY window_start_ms LIMIT max(0, (SELECT COUNT(*) FROM claim_failures) - ?))",
    MAX_FAILURE_ROWS,
  );
}

export function wrongCode(sql: Sql, scope: FailureScope, client: string, nowMs: number, message: string): { ok: false; code: Error_Code; message: string } {
  if (throttled(sql, scope, client, nowMs)) {
    return { ok: false, code: Error_Code.RETRY_LATER, message: "too many wrong codes from this address; try again in a few minutes" };
  }
  recordFailure(sql, scope, client, nowMs);
  return { ok: false, code: Error_Code.FORBIDDEN, message };
}

export function storeClaimCode(sql: Sql, codeHash: ArrayBuffer, nowMs: number, expiresAtMs: number): boolean {
  if (sql.exec("SELECT 1 FROM users LIMIT 1").toArray().length > 0) return false;
  sql.exec("DELETE FROM claim_codes WHERE expires_at_ms <= ?", nowMs);
  if (sql.exec("SELECT 1 FROM claim_codes LIMIT 1").toArray().length > 0) return false;
  sql.exec("INSERT INTO claim_codes (code_hash, expires_at_ms) VALUES (?, ?)", codeHash, expiresAtMs);
  return true;
}

export function ownerOf(sql: Sql): string | undefined {
  const row = sql.exec("SELECT user_id FROM users ORDER BY created_at_ms, user_id LIMIT 1").toArray()[0];
  return row ? String(row.user_id) : undefined;
}

export function redeemClaim(sql: Sql, attempt: ClaimAttempt): ClaimOutcome {
  const owner = ownerOf(sql);
  if (owner !== attempt.ownerBefore) return { ok: false, code: Error_Code.RETRY_LATER, message: "the deployment changed during the claim; retry" };
  if (owner) return recover(sql, attempt);

  const key = presentedKey(attempt);
  if (key) {
    const spent = spendKey(sql, key, attempt.nowMs);
    if (spent) return spent;
    sql.exec("DELETE FROM claim_codes");
  } else {
    const live = sql.exec("SELECT code_hash FROM claim_codes WHERE expires_at_ms > ?", attempt.nowMs).toArray();
    if (live.length === 0) return refused("no valid claim code; open /setup to issue one");
    const redeemed = live.find((row) => sameDigest(row.code_hash as ArrayBuffer, attempt.codeHash))?.code_hash;
    if (!redeemed) return wrongCode(sql, "claim", attempt.client, attempt.nowMs, "wrong claim code");
    sql.exec("DELETE FROM claim_codes WHERE code_hash != ?", redeemed);
    sql.exec(
      "UPDATE claim_codes SET request_id = ?, claimed_device_id = ?, expires_at_ms = ? WHERE code_hash = ?",
      attempt.requestId || null,
      attempt.deviceId,
      attempt.nowMs + REDEEMED_CODE_REPLAY_MS,
      redeemed,
    );
  }
  sql.exec("DELETE FROM claim_failures WHERE scope = 'claim'");
  sql.exec("INSERT INTO users (user_id, name, created_at_ms) VALUES (?, ?, ?)", attempt.userId, attempt.userName || "Owner", attempt.nowMs);
  attempt.storeDevice();
  return { ok: true, revoke: key ? joinAttempt(sql, attempt) : [] };
}

function recover(sql: Sql, attempt: ClaimAttempt): ClaimOutcome {
  const key = presentedKey(attempt);
  if (!key) return repeatedClaim(sql, attempt) ?? wrongCode(sql, "recovery", attempt.client, attempt.nowMs, "this deployment already has an owner");
  const spent = spendKey(sql, key, attempt.nowMs);
  if (spent) return spent;
  attempt.storeDevice();
  return { ok: true, revoke: joinAttempt(sql, attempt) };
}

function repeatedClaim(sql: Sql, attempt: ClaimAttempt): ClaimOutcome | undefined {
  if (!attempt.requestId) return undefined;
  const row = sql
    .exec("SELECT code_hash, claimed_device_id FROM claim_codes WHERE request_id = ? AND expires_at_ms > ?", attempt.requestId, attempt.nowMs)
    .toArray()[0];
  if (!row || !sameDigest(row.code_hash as ArrayBuffer, attempt.codeHash)) return undefined;
  return { ok: true, earlierDevice: String(row.claimed_device_id) };
}

function joinAttempt(sql: Sql, attempt: ClaimAttempt): string[] {
  if (!attempt.setupAttempt) return [];
  sql.exec("UPDATE devices SET setup_attempt = ? WHERE device_id = ?", attempt.setupAttempt, attempt.deviceId);
  return sql
    .exec(`SELECT device_id FROM devices WHERE ${UNUSED_OF_ATTEMPT} AND device_id != ?`, attempt.setupAttempt, attempt.userId, attempt.deviceId)
    .toArray()
    .map((row) => String(row.device_id));
}

export function stillUnused(sql: Sql, deviceId: string, setupAttempt: string, userId: string): boolean {
  return sql.exec(`SELECT 1 FROM devices WHERE ${UNUSED_OF_ATTEMPT} AND device_id = ?`, setupAttempt, userId, deviceId).toArray().length > 0;
}

function presentedKey(attempt: ClaimAttempt): ArrayBuffer | undefined {
  const key = attempt.recoveryKeyHash;
  return key && sameDigest(key, attempt.rawHash) ? key : undefined;
}

function spendKey(sql: Sql, key: ArrayBuffer, nowMs: number): ClaimOutcome | undefined {
  if (sql.exec("SELECT 1 FROM used_recovery_keys WHERE key_hash = ?", key).toArray().length > 0) {
    return refused("this recovery key was already used; set a new one");
  }
  sql.exec("INSERT INTO used_recovery_keys (key_hash, used_at_ms) VALUES (?, ?)", key, nowMs);
  return undefined;
}
