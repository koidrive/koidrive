// SPDX-License-Identifier: AGPL-3.0-or-later

import { afterAll, expect, it } from "vitest";
import { Device, Server, SpaceClient, stats } from "./client";
import { addStats, readState } from "./state.ts";

const state = readState();
const packed = state.written?.packed;
if (!state.url || !state.owner || !packed) throw new Error("upgrade-verify.ts must write a pack with chunk_packs on first");

afterAll(() => addStats(stats));

it("keeps a file written as a pack readable after chunk_packs is turned off", async () => {
  const client = new SpaceClient(new Device(new Server(state.url!), state.owner!), state.written!.spaceId);
  expect((await client.welcome()).features).not.toContain("chunk_packs");
  const node = (await client.tree()).get(packed.nodeId);
  expect(await client.download(node!.current!.content!)).toBe(packed.sha256);
});
