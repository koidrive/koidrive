// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import type { AccountDO } from "../src/account-do";
import { IMPLEMENTED } from "../src/features";
import { type ServerInfo, ServerInfoSchema } from "../src/gen/koidrive/v1/auth_pb";
import worker from "../src/worker";
import { createSpace, getOwner, Socket } from "./session";

async function serverInfo(variable?: string): Promise<ServerInfo> {
  const request = new Request("https://koidrive.test/v1/server") as Parameters<typeof worker.fetch>[0];
  const response = await worker.fetch(request, { ...env, KOIDRIVE_FEATURES: variable } as unknown as Env, {} as ExecutionContext);
  return fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer()));
}

async function withOlderAccountCode<T>(run: () => Promise<T>): Promise<T> {
  const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName("account"));
  const prototype = await runInDurableObject(stub, (instance: AccountDO) => Object.getPrototypeOf(Object.getPrototypeOf(instance)) as Partial<AccountDO>);
  const original = prototype.implementedFeatures;
  delete prototype.implementedFeatures;
  try {
    return await run();
  } finally {
    prototype.implementedFeatures = original;
  }
}

describe("RFC 0022 section 3 rule 2: effective features", () => {
  it("lists in ServerInfo exactly the configured features the Account DO implements", async () => {
    expect((await serverInfo()).features.toSorted()).toEqual([...IMPLEMENTED].sort());
    expect((await serverInfo("-chunk_bundles,unknown")).features).toEqual(["git_tracked"]);
    expect((await serverInfo("-chunk_bundles, -git_tracked")).features).toEqual([]);
    const response = await exports.default.fetch("https://koidrive.test/v1/server");
    expect(fromBinary(ServerInfoSchema, new Uint8Array(await response.arrayBuffer())).features.toSorted()).toEqual([...IMPLEMENTED].sort());
  });

  it("lists none while the Account DO runs older code, while Welcome already lists what the Space DO implements", async () => {
    const owner = await getOwner();
    const spaceId = await createSpace(owner);
    await withOlderAccountCode(async () => {
      const info = await serverInfo();
      expect(info.features).toEqual([]);
      expect(info.setupAttempts).toBe(true);
      const socket = await Socket.connect(spaceId, owner);
      expect((await socket.start()).features.toSorted()).toEqual([...IMPLEMENTED].sort());
    });
    expect((await serverInfo()).features.toSorted()).toEqual([...IMPLEMENTED].sort());
  });
});
