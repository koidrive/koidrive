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

it("answers a claim the server saved before an exception when the client sends it again with the same request id", async () => {
  const code = await claimCode();
  const account = env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
  vi.spyOn(console, "error").mockImplementation(() => {});
  await runInDurableObject(account, (instance: object) => {
    vi.spyOn(instance as { userName: () => string }, "userName").mockImplementation(() => {
      throw new Error("disk I/O error: SQLITE_IOERR");
    });
  });
  for (let n = 0; n < 4; n++) expect(await claim(code, "first-id")).toEqual({ status: 500, code: Error_Code.INTERNAL, cause: Error_Cause.EXCEPTION });
  await runInDurableObject(account, (instance: object) => {
    (instance as { userName: { mockRestore: () => void } }).userName.mockRestore();
  });
  expect(await claim(code, "first-id")).toMatchObject({ status: 200 });
  expect(await claim(code, "second-id")).toMatchObject({ status: 403 });
});
