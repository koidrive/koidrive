// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { expect, it, vi } from "vitest";
import { ClaimRequestSchema, CreateSpaceRequestSchema, DeviceCredentialsSchema, SpaceInfoSchema } from "../src/gen/koidrive/v1/auth_pb";
import { RpcRequestSchema } from "../src/gen/koidrive/v1/messages_pb";
import { Error_Cause, Error_Code, ErrorSchema } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import { getMeta } from "../src/space/store";
import { op } from "./helpers";

const BASE = "https://koidrive.test";
const DAY_MS = 86_400_000;

async function post(path: string, body: Uint8Array, token?: string): Promise<Response> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (token) headers.Authorization = "Bearer " + token;
  return exports.default.fetch(BASE + path, { method: "POST", headers, body });
}

it("answers a commit that fills the space's storage with 429 QUOTA_EXCEEDED and the RFC 0001 section 9 wait", async () => {
  const log = vi.spyOn(console, "log");
  await exports.default.fetch(BASE + "/setup");
  const code = log.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"))!.split(": ")[1]!;
  log.mockRestore();
  const claim = toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, userName: "Owner", deviceName: "Laptop", platform: "macos" }));
  const token = fromBinary(DeviceCredentialsSchema, new Uint8Array(await (await post("/v1/claim", claim)).arrayBuffer())).access!.token;
  const created = await post("/v1/spaces", toBinary(CreateSpaceRequestSchema, create(CreateSpaceRequestSchema, { name: "Home" })), token);
  const spaceId = fromBinary(SpaceInfoSchema, new Uint8Array(await created.arrayBuffer())).spaceId;

  const historyId = await runInDurableObject(env.SPACE.get(env.SPACE.idFromName(spaceId)), (_instance, state) => {
    const sql = state.storage.sql as SqlStorage & { setMaxPageCountForTest(count: number): void };
    sql.setMaxPageCountForTest(Math.ceil(sql.databaseSize / Number(sql.exec("PRAGMA page_size").one().page_size)));
    return String(getMeta(sql, "history_id"));
  });
  const ops = Array.from({ length: 200 }, (_, i) => op.mkdir(uuidv7(), spaceId, `folder ${i} ${"x".repeat(150)}`));
  const commit = create(RpcRequestSchema, { reqId: 1n, call: { case: "commit", value: { commitId: uuidv7(), baseSeq: 0n, ops, historyId, deviceSeq: 1n } } });
  const logged = vi.spyOn(console, "error").mockImplementation(() => {});
  const before = Date.now();
  const response = await post(`/v1/spaces/${spaceId}/rpc`, toBinary(RpcRequestSchema, commit), token);
  const after = Date.now();

  expect(response.status).toBe(429);
  const error = fromBinary(ErrorSchema, new Uint8Array(await response.arrayBuffer()));
  expect(error).toMatchObject({ code: Error_Code.QUOTA_EXCEEDED, message: "the server's storage is full", cause: Error_Cause.STORAGE_FULL });
  const wait = (nowMs: number) => (nowMs % DAY_MS < 30 * 60_000 ? 300_000 : DAY_MS - (nowMs % DAY_MS));
  const bounds = [wait(before), wait(after)].sort((x, y) => x - y);
  expect(error.retryAfterMs).toBeGreaterThanOrEqual(BigInt(bounds[0]!));
  expect(error.retryAfterMs).toBeLessThanOrEqual(BigInt(bounds[1]!));
  const original = logged.mock.calls.find(([what]) => what === "request stopped at a platform limit")?.[1];
  expect(String(original)).toMatch(/^Error: database or disk is full: SQLITE_FULL/);
});
