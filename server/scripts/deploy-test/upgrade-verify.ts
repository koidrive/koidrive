// SPDX-License-Identifier: AGPL-3.0-or-later

import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { ServerInfoSchema, SpaceListSchema } from "../../src/gen/koidrive/v1/auth_pb";
import { ShareInfoSchema, ShareLinkSchema } from "../../src/gen/koidrive/v1/share_pb";
import { Device, fileData, op, randomBytes, readBytes, send, Server, sha256, SpaceClient, stats, toHex, uuidv7 } from "./client";
import { addStats, readState, writeState } from "./state.ts";

const state = readState();
const written = state.written;
if (!state.url || !state.owner || !written || state.deployments.length < 2) throw new Error("upgrade-write.ts and the second run.ts deploy must run first");
const [before, after] = [state.deployments.at(-2)!, state.deployments.at(-1)!];
const server = new Server(state.url);
const owner = new Device(server, state.owner);

afterAll(() => addStats(stats));

async function steps(dir: string, file: string): Promise<number> {
  const schema = (await import(join(dir, "src", file))) as { MIGRATIONS: readonly unknown[] };
  return schema.MIGRATIONS.length;
}

describe("an upgrade from " + before.sha.slice(0, 12) + " to " + after.sha.slice(0, 12), () => {
  it("never removes a schema step, and reports the steps each class runs", async () => {
    for (const [name, file] of [["AccountDO", "account/schema.ts"], ["SpaceDO", "space/schema.ts"]] as const) {
      const [from, to] = [await steps(before.dir, file), await steps(after.dir, file)];
      expect(to).toBeGreaterThanOrEqual(from);
      console.log(name + ": " + from + " schema steps before, " + to + " after; " + (to > from ? "objects run steps " + (from + 1) + " to " + to + " when they start" : "no schema step runs on this upgrade"));
    }
  });

  it("keeps the owner and the paired device, whose credentials still refresh", async () => {
    const info = await server.json("GET", "/v1/server", ServerInfoSchema);
    expect(info.claimed).toBe(true);
    const second = new Device(server, written.second);
    await second.token();
    const listed = await server.json("GET", "/v1/spaces", SpaceListSchema, { token: await second.token() });
    expect(listed.spaces.map((space) => space.spaceId)).toContain(written.spaceId);
    const claim = await server.request("POST", "/v1/claim", { body: { code: "AAAA-BBBB-CCCC", userName: "Intruder", deviceName: "x", platform: "linux" } });
    expect(claim.status).toBe(403);
    await readBytes(claim);
  });

  it("keeps the space's head, tree and file contents", async () => {
    const client = new SpaceClient(owner, written.spaceId);
    const welcome = await client.welcome();
    expect(String(welcome.headSeq)).toBe(written.headSeq);
    const tree = await client.tree();
    for (const file of written.files) {
      const node = tree.get(file.nodeId);
      expect(node?.name).toBe(file.name);
      expect(await client.download(node!.current!.content!)).toBe(file.sha256);
    }
  });

  it("keeps serving the share link created before the upgrade", async () => {
    const file = written.files.find((f) => f.name === "archive.bin")!;
    const info = await server.json("GET", "/v1/share/" + written.linkId, ShareInfoSchema);
    expect(info.size).toBe(BigInt(file.size));
    const download = await send(server.base + "/v1/share/" + written.linkId + "/file/" + file.nodeId);
    expect(download.status).toBe(200);
    expect(toHex(await sha256(await readBytes(download)))).toBe(file.sha256);
  });

  it("accepts new commits and a new password-protected link in the space written before the upgrade", async () => {
    const client = new SpaceClient(owner, written.spaceId);
    await client.welcome();
    const added = await fileData([new TextEncoder().encode("Written after the upgrade\n")]);
    const { receipts } = await client.upload(added);
    const nodeId = uuidv7();
    const result = await client.commit([op.create(nodeId, written.spaceId, "after-upgrade.txt", added.content)], receipts);
    expect(String(result.seq)).toBe(String(BigInt(written.headSeq) + 1n));
    const link = await server.json("POST", "/v1/spaces/" + written.spaceId + "/links", ShareLinkSchema, {
      body: { nodeId, password: "upgrade password " + uuidv7().slice(-6) },
      token: await owner.token(),
      status: 201,
    });
    expect(link.hasPassword).toBe(true);
  });

  it("writes a bundle as one pack while chunk_packs is on, and reads it back", async ({ skip }) => {
    const client = new SpaceClient(owner, written.spaceId);
    if (!(await client.welcome()).features.includes("chunk_packs")) skip("this deployment has chunk_packs off");
    const packed = await fileData([randomBytes(48 * 1024), randomBytes(16 * 1024), randomBytes(2 * 1024)]);
    const receipts = await client.uploadBundle(packed);
    expect(receipts.map((receipt) => receipt.token.split(".")[0])).toEqual(["v2", "v2", "v2"]);
    expect(new Set(receipts.map((receipt) => receipt.token.split(".")[2])).size).toBe(1);
    const nodeId = uuidv7();
    await client.commit([op.create(nodeId, written.spaceId, "packed.bin", packed.content)], receipts);
    expect(await client.download(packed.content)).toBe(packed.sha256);
    const updated = readState();
    updated.written = { ...updated.written!, packed: { nodeId, sha256: packed.sha256 } };
    writeState(updated);
  });
});
