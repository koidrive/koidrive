// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { expect, it, vi } from "vitest";
import { ClaimRequestSchema, DeviceCredentialsSchema } from "../src/gen/koidrive/v1/auth_pb";
import { Error_Cause, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";

const BASE = "https://koidrive.test";

async function claimCode(): Promise<string> {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const line = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"));
  log.mockRestore();
  return line!.split(": ")[1]!;
}

async function claim(code: string, requestId: string) {
  const body = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop", platform: "macos", requestId }));
  const response = await exports.default.fetch(BASE + "/v1/claim", { method: "POST", headers: { "Content-Type": "application/x-protobuf" }, body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (response.ok) return { status: response.status, deviceId: fromBinary(DeviceCredentialsSchema, bytes).deviceId };
  const error = fromBinary(ErrorSchema, bytes);
  return { status: response.status, code: error.code, cause: error.cause };
}

const HOUR_MS = 3_600_000;

it("answers a claim the server saved before a daily limit when the client sends it again with the same request id, for 25 hours after the claim", async () => {
  const code = await claimCode();
  const account = env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
  vi.spyOn(console, "error").mockImplementation(() => {});
  await runInDurableObject(account, (instance: object) => {
    vi.spyOn(instance as { userName: () => string }, "userName").mockImplementation(() => {
      throw new Error("Exceeded allowed rows written in Durable Objects free tier.");
    });
  });
  expect(await claim(code, "first-id")).toEqual({ status: 429, code: Error_Code.QUOTA_EXCEEDED, cause: Error_Cause.DAILY_LIMIT });
  await runInDurableObject(account, (instance: object) => {
    (instance as { userName: { mockRestore: () => void } }).userName.mockRestore();
  });
  const passed = (ms: number) => runInDurableObject(account, (_instance, state) => state.storage.sql.exec("UPDATE claim_codes SET expires_at_ms = expires_at_ms - ?", ms));
  await passed(24 * HOUR_MS);
  expect(await claim(code, "first-id")).toMatchObject({ status: 200 });
  expect(await claim(code, "second-id")).toMatchObject({ status: 403 });
  await passed(2 * HOUR_MS);
  expect(await claim(code, "first-id")).toMatchObject({ status: 403 });
});
