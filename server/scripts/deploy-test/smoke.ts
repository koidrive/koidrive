// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, toJson } from "@bufbuild/protobuf";
import { afterAll, describe, expect, it } from "vitest";
import { RewindRequestSchema, RewindResultSchema, ServerInfo_DataPath, ServerInfoSchema, SpaceInfoSchema, SpaceListSchema } from "../../src/gen/koidrive/v1/auth_pb";
import { OpResult_Code } from "../../src/gen/koidrive/v1/commit_pb";
import { NodeKind, NodeStatus } from "../../src/gen/koidrive/v1/model_pb";
import { ChunkPlan_Action, Error_Code } from "../../src/gen/koidrive/v1/rpc_pb";
import { ShareInfoSchema, ShareLinkSchema, ShareListingSchema } from "../../src/gen/koidrive/v1/share_pb";
import { Device, type FileData, fileData, HttpError, op, randomBytes, Server, sleep, SpaceClient, send, sha256, stats, toHex, uuidv7, readBytes } from "./client";
import { type Account, getObject, listObjects, requireAccount } from "./cloudflare.ts";
import { addStats, readState } from "./state.ts";

const state = readState();
if (!state.url || !state.owner) throw new Error("run.ts deploy and run.ts claim must run first: the state file has no server or owner");
const account: Account = requireAccount();
const server = new Server(state.url);
const owner = new Device(server, state.owner);
const encoder = new TextEncoder();
const SEGMENT_WAIT_MS = 9 * 60_000;
const SNAPSHOT_WAIT_MS = 5 * 60_000;
const REWIND_WAIT_MS = 5 * 60_000;
const PITR_HISTORY_WAIT_MS = 90_000;

afterAll(() => addStats(stats));

const run = {
  second: undefined as Device | undefined,
  spaceId: "",
  space: undefined as SpaceClient | undefined,
  firstHistory: "",
  ids: { docs: uuidv7(), notes: uuidv7(), photo: uuidv7(), plan: uuidv7(), later: uuidv7() },
  files: {} as Record<"notes" | "photo" | "plan" | "notesV2" | "later", FileData>,
  seqA: 0n,
  seqB: 0n,
  spaceCreatedMs: 0,
  restorePointMs: 0,
  firstCommitMs: 0,
};

async function waitFor<T>(what: string, timeoutMs: number, check: () => Promise<T | undefined>): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value !== undefined) return value;
    if (Date.now() > deadline) throw new Error("timed out after " + timeoutMs / 1000 + " s waiting for " + what);
    await sleep(10_000);
  }
}

function seqName(seq: bigint): string {
  return String(seq).padStart(20, "0");
}

describe("a server deployed to a real account", () => {
  it("reports its owner and refuses a second claim", async () => {
    const info = await server.json("GET", "/v1/server", ServerInfoSchema);
    expect(info.claimed).toBe(true);
    expect(info.dataPath).toBe(ServerInfo_DataPath.RELAY);
    expect(info.protoMin).toBeLessThanOrEqual(info.protoMax);
    const setup = await readBytes(await send(server.base + "/setup"));
    const text = new TextDecoder().decode(setup);
    expect(text).toContain("already has an owner");
    const second = await server.request("POST", "/v1/claim", { body: { code: "AAAA-BBBB-CCCC", userName: "Intruder", deviceName: "x", platform: "linux" } });
    expect(second.status).toBe(403);
    await readBytes(second);
    console.log("Server " + info.serverVersion + ", protocol " + info.protoMin + "-" + info.protoMax + ", claimed");
  });

  it("pairs a second device, which refreshes its own access token", async () => {
    run.second = await owner.pair("deploy-test second device");
    expect(run.second.credentials.userId).toBe(state.owner!.userId);
    expect(run.second.credentials.deviceId).not.toBe(state.owner!.deviceId);
    expect(await run.second.token()).toMatch(/^v1\./);
  });

  it("creates a space that both devices list", async () => {
    run.spaceCreatedMs = Date.now();
    const space = await server.json("POST", "/v1/spaces", SpaceInfoSchema, { body: { name: "Deploy test" }, token: await owner.token(), status: 201 });
    run.spaceId = space.spaceId;
    const listed = await server.json("GET", "/v1/spaces", SpaceListSchema, { token: await run.second!.token() });
    expect(listed.spaces.map((s) => s.spaceId)).toContain(space.spaceId);
    run.space = new SpaceClient(owner, space.spaceId);
    const welcome = await run.space.welcome();
    expect(welcome.headSeq).toBe(0n);
    run.firstHistory = welcome.historyId;
    console.log("Space " + space.spaceId + ", history " + welcome.historyId);
  });

  it("uploads chunks that R2 checks against their SHA-256, commits files and deduplicates committed chunks", async () => {
    const space = run.space!;
    run.files.notes = await fileData([encoder.encode("Notes written by the deploy test at " + new Date().toISOString() + "\n")]);
    run.files.photo = await fileData([randomBytes(300 * 1024), randomBytes(1024 * 1024), randomBytes(200 * 1024)]);
    run.files.plan = await fileData([encoder.encode("# Plan\n\nShip it.\n")]);
    const { receipts, uploaded } = await space.upload(run.files.notes, run.files.photo, run.files.plan);
    expect(uploaded).toBe(5);

    const wrong = await fileData([randomBytes(4096)]);
    const plan = await space.rpc({ case: "prepareUpload", value: { chunks: wrong.content.chunks } });
    if (plan.case !== "prepareUpload" || !plan.value.chunks[0]?.target) throw new Error("no upload target for a new chunk");
    const target = plan.value.chunks[0].target;
    const refused = await send(target.url, { method: "PUT", headers: { ...target.headers, "Content-Type": "application/octet-stream" }, body: randomBytes(4096) });
    expect(refused.status).toBe(400);
    await readBytes(refused);

    run.firstCommitMs = Date.now();
    const result = await space.commit(
      [
        op.mkdir(run.ids.docs, run.spaceId, "docs"),
        op.create(run.ids.notes, run.spaceId, "notes.txt", run.files.notes.content),
        op.create(run.ids.photo, run.spaceId, "photo.bin", run.files.photo.content),
        op.create(run.ids.plan, run.ids.docs, "plan.md", run.files.plan.content),
      ],
      receipts,
    );
    expect(result.results.map((r) => r.code)).toEqual([OpResult_Code.APPLIED, OpResult_Code.APPLIED, OpResult_Code.APPLIED, OpResult_Code.APPLIED]);
    run.seqA = result.seq;

    const again = await space.rpc({ case: "prepareUpload", value: { chunks: run.files.photo.content.chunks } });
    expect(again.case === "prepareUpload" && again.value.chunks.every((plan) => plan.action === ChunkPlan_Action.PRESENT)).toBe(true);
  });

  it("lets the second device read the tree and download every file", async () => {
    const reader = new SpaceClient(run.second!, run.spaceId);
    await reader.welcome();
    const tree = await reader.tree();
    for (const [key, name] of [["notes", "notes.txt"], ["photo", "photo.bin"], ["plan", "plan.md"]] as const) {
      const node = tree.get(run.ids[key]);
      expect(node?.name).toBe(name);
      expect(await reader.download(node!.current!.content!)).toBe(run.files[key].sha256);
    }
  });

  it("commits a second set of changes twenty seconds after a restore point", async () => {
    await sleep(Math.max(20_000, run.spaceCreatedMs + PITR_HISTORY_WAIT_MS - Date.now()));
    run.restorePointMs = Date.now();
    await sleep(20_000);
    run.files.notesV2 = await fileData([encoder.encode("Notes changed after the restore point\n")]);
    run.files.later = await fileData([encoder.encode("Written after the restore point\n")]);
    const { receipts } = await run.space!.upload(run.files.notesV2, run.files.later);
    const result = await run.space!.commit(
      [op.update(run.ids.notes, run.files.notesV2.content), op.delete(run.ids.plan), op.create(run.ids.later, run.spaceId, "later.txt", run.files.later.content)],
      receipts,
    );
    run.seqB = result.seq;
    expect(run.seqB).toBe(run.seqA + 1n);
  });

  it("serves a share link to a file without a password, and its viewer page", async () => {
    const token = await owner.token();
    const link = await server.json("POST", "/v1/spaces/" + run.spaceId + "/links", ShareLinkSchema, { body: { nodeId: run.ids.photo }, token, status: 201 });
    expect(link.hasPassword).toBe(false);
    expect(link.url).toBe(server.base + "/s/" + link.linkId);
    const page = await send(link.url);
    expect(page.status).toBe(200);
    expect(page.headers.get("content-security-policy")).toContain("default-src 'none'");
    expect(new TextDecoder().decode(await readBytes(page))).toContain('src="/viewer/viewer.js"');
    const info = await server.json("GET", "/v1/share/" + link.linkId, ShareInfoSchema);
    expect(info.kind).toBe(NodeKind.FILE);
    expect(info.name).toBe("photo.bin");
    expect(info.size).toBe(BigInt(run.files.photo.size));
    const file = await send(server.base + "/v1/share/" + link.linkId + "/file/" + run.ids.photo);
    expect(file.status).toBe(200);
    expect(file.headers.get("content-disposition")).toContain('filename="photo.bin"');
    expect(toHex(await sha256(await readBytes(file)))).toBe(run.files.photo.sha256);
  });

  it("serves a password-protected link to a folder only after unlocking, and stops when it is revoked", async () => {
    const token = await owner.token();
    const password = "correct horse battery " + uuidv7().slice(-6);
    const folder = uuidv7();
    const inside = await fileData([encoder.encode("Shared with a password\n")]);
    const { receipts } = await run.space!.upload(inside);
    await run.space!.commit([op.mkdir(folder, run.spaceId, "shared"), op.create(uuidv7(), folder, "inside.txt", inside.content)], receipts);
    run.seqB = run.space!.baseSeq;
    const link = await server.json("POST", "/v1/spaces/" + run.spaceId + "/links", ShareLinkSchema, { body: { nodeId: folder, password }, token, status: 201 });
    expect(link.hasPassword).toBe(true);
    const locked = await server.json("GET", "/v1/share/" + link.linkId, ShareInfoSchema, { status: 401 });
    expect(locked.passwordRequired).toBe(true);
    const wrong = await server.request("POST", "/v1/share/" + link.linkId + "/unlock", { body: { password: "not the password" } });
    expect(wrong.ok).toBe(false);
    await readBytes(wrong);
    const unlocked = await server.request("POST", "/v1/share/" + link.linkId + "/unlock", { body: { password } });
    expect(unlocked.status).toBe(204);
    const cookie = (unlocked.headers.get("set-cookie") ?? "").split(";")[0]!;
    expect(cookie).toMatch(/^fb_share=./);
    await readBytes(unlocked);
    const listing = await server.json("GET", "/v1/share/" + link.linkId + "/list", ShareListingSchema, { headers: { Cookie: cookie } });
    const entry = listing.entries.find((e) => e.name === "inside.txt");
    expect(entry).toBeDefined();
    const file = await send(server.base + "/v1/share/" + link.linkId + "/file/" + entry!.nodeId, { headers: { Cookie: cookie } });
    expect(file.status).toBe(200);
    expect(toHex(await sha256(await readBytes(file)))).toBe(inside.sha256);
    const revoked = await server.request("DELETE", "/v1/spaces/" + run.spaceId + "/links/" + link.linkId, { token });
    expect(revoked.status).toBe(204);
    await readBytes(revoked);
    const gone = await server.request("GET", "/v1/share/" + link.linkId, { headers: { Cookie: cookie } });
    expect(gone.status).toBe(404);
    await readBytes(gone);
  });

  it("stores the chunks in R2 and writes the space's metadata export there (RFC 0004)", async () => {
    const prefix = "spaces/" + run.spaceId + "/meta/" + run.firstHistory + "/";
    const chunks = (await listObjects(account, state.name, "spaces/" + run.spaceId + "/chunks/")) ?? [];
    expect(chunks.length).toBeGreaterThanOrEqual(8);
    console.log("Waiting for the first log segment, due five minutes after the first commit");
    const keys = await waitFor("space.json and a log segment under " + prefix, Math.max(60_000, run.firstCommitMs + SEGMENT_WAIT_MS - Date.now()), async () => {
      const listed = ((await listObjects(account, state.name, prefix)) ?? []).map((object) => object.key.slice(prefix.length));
      return listed.includes("space.json") && listed.some((key) => key.startsWith("log/")) ? listed : undefined;
    });
    expect(keys.find((key) => key.startsWith("log/"))).toMatch(new RegExp("^log/" + seqName(1n) + "-\\d{20}\\.fblog$"));
    const descriptor = JSON.parse(new TextDecoder().decode(await getObject(account, state.name, prefix + "space.json")));
    expect(descriptor).toMatchObject({ format: "koidrive.space", format_version: 1, space_id: run.spaceId, history_id: run.firstHistory, hash_alg: "sha256", rewind_number: 0 });
    console.log("R2 holds " + chunks.length + " chunk objects and the export " + keys.join(", "));
  });

  it("rewinds the space to the restore point on the real Durable Object (RFC 0003)", async () => {
    const request = toJson(
      RewindRequestSchema,
      create(RewindRequestSchema, { rewindId: uuidv7(), restorePointMs: BigInt(run.restorePointMs), expectedHistoryId: run.firstHistory }),
    );
    const result = await waitFor("the rewind to complete", REWIND_WAIT_MS, async () => {
      try {
        return await server.json("POST", "/v1/spaces/" + run.spaceId + "/rewind", RewindResultSchema, { body: request, token: await owner.token() });
      } catch (error) {
        if (error instanceof HttpError && error.status === 503) return undefined;
        throw error;
      }
    });
    expect(result.previousHistoryId).toBe(run.firstHistory);
    expect(result.historyId).not.toBe(run.firstHistory);
    expect(result.restoreSeq).toBe(run.seqA);
    expect(result.previousHeadSeq).toBe(run.seqB);

    const space = run.space!;
    const welcome = await space.welcome();
    expect(welcome.historyId).toBe(result.historyId);
    expect(welcome.headSeq).toBe(run.seqA);
    const tree = await space.tree();
    expect(tree.get(run.ids.notes)?.current?.content?.manifestDigest).toEqual(run.files.notes.content.manifestDigest);
    expect(tree.get(run.ids.plan)?.status).toBe(NodeStatus.LIVE);
    expect(tree.has(run.ids.later)).toBe(false);
    expect(await space.download(tree.get(run.ids.notes)!.current!.content!)).toBe(run.files.notes.sha256);

    const stale = new SpaceClient(run.second!, run.spaceId);
    stale.historyId = run.firstHistory;
    stale.baseSeq = run.seqB;
    await expect(stale.commit([op.mkdir(uuidv7(), run.spaceId, "stale")])).rejects.toMatchObject({ code: String(Error_Code.HISTORY_MISMATCH) });
    console.log("Rewound to seq " + result.restoreSeq + " of history " + run.firstHistory + "; new history " + result.historyId);

    const prefix = "spaces/" + run.spaceId + "/meta/" + result.historyId + "/";
    const manifestKey = "snap/" + seqName(result.restoreSeq) + "/manifest.json";
    const keys = await waitFor("the rewound history's snapshot and descriptor under " + prefix, SNAPSHOT_WAIT_MS, async () => {
      const listed = ((await listObjects(account, state.name, prefix)) ?? []).map((object) => object.key.slice(prefix.length));
      return listed.includes("space.json") && listed.includes(manifestKey) ? listed : undefined;
    });
    const manifest = JSON.parse(new TextDecoder().decode(await getObject(account, state.name, prefix + manifestKey)));
    expect(manifest).toMatchObject({ format: "koidrive.snapshot", format_version: 2, space_id: run.spaceId, history_id: result.historyId, start_seq: Number(result.restoreSeq) });
    expect(manifest.pages.length).toBeGreaterThan(0);
    const descriptor = JSON.parse(new TextDecoder().decode(await getObject(account, state.name, prefix + "space.json")));
    expect(descriptor).toMatchObject({ history_id: result.historyId, rewind_number: 1 });
    console.log("The rewound history exported " + keys.join(", "));

    const after = await fileData([encoder.encode("Written after the rewind\n")]);
    const { receipts } = await space.upload(after);
    const committed = await space.commit([op.create(uuidv7(), run.spaceId, "after-rewind.txt", after.content)], receipts);
    expect(committed.seq).toBe(run.seqA + 1n);
  });
});
