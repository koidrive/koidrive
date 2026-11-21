// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, type MessageInitShape, type MessageShape, toBinary } from "@bufbuild/protobuf";
import { exports } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ClaimRequestSchema,
  CreateSpaceRequestSchema,
  DeviceCredentialsSchema,
  SpaceInfoSchema,
} from "../src/gen/koidrive/v1/auth_pb";
import { UploadReceiptSchema } from "../src/gen/koidrive/v1/commit_pb";
import { ClientMessageSchema, RpcRequestSchema, type RpcResponse, RpcResponseSchema, ServerMessageSchema } from "../src/gen/koidrive/v1/messages_pb";
import { ChunkPlan_Action } from "../src/gen/koidrive/v1/rpc_pb";
import { uuidv7 } from "../src/ids";
import { content, op } from "./helpers";

const BASE = "https://koidrive.test";

afterEach(() => {
  vi.restoreAllMocks();
});

async function post<S extends DescMessage>(path: string, body: Uint8Array, schema: S, token?: string): Promise<MessageShape<S>> {
  const headers: Record<string, string> = { "Content-Type": "application/x-protobuf" };
  if (token) headers.Authorization = "Bearer " + token;
  const response = await exports.default.fetch(BASE + path, { method: "POST", headers, body });
  expect(response.ok, path).toBe(true);
  return fromBinary(schema, new Uint8Array(await response.arrayBuffer()));
}

function recordLogs(): string[] {
  const lines: string[] = [];
  for (const method of ["log", "info", "warn", "error", "debug"] as const) {
    vi.spyOn(console, method).mockImplementation((...args: unknown[]) => {
      lines.push(args.map((arg) => (arg instanceof Error ? `${arg.message}\n${arg.stack ?? ""}` : typeof arg === "string" ? arg : JSON.stringify(arg) ?? String(arg))).join(" "));
    });
  }
  return lines;
}

const sources = import.meta.glob<string>("../src/**/*.ts", { query: "?raw", import: "default", eager: true });

function consoleCalls(source: string): { call: string; args: string }[] {
  return [...source.matchAll(/console\.\w+\(/g)].map((match) => {
    const start = match.index + match[0].length;
    let end = start;
    let depth = 1;
    let quote: string | undefined;
    for (; depth > 0 && end < source.length; end++) {
      const char = source[end]!;
      if (quote) {
        if (char === "\\") end++;
        else if (char === quote) quote = undefined;
      } else if (char === '"' || char === "'" || char === "`") quote = char;
      else if (char === "(") depth++;
      else if (char === ")") depth--;
    }
    return { call: source.slice(match.index, end), args: source.slice(start, end - 1) };
  });
}

function rawCaughtErrors(source: string): string[] {
  const caught = [...source.matchAll(/catch\s*\(\s*\(?\s*(\w+)/g)].map((match) => match[1]!);
  return consoleCalls(source).flatMap(({ call, args }) =>
    caught.some((name) => new RegExp("\\b" + name + "\\b").test(args.replaceAll("errorText(" + name + ")", ""))) ? [call] : [],
  );
}

describe("server logs", () => {
  it("carry no ticket, receipt, token or credential through an upload, a failed upload and a download", async () => {
    const claimLog = vi.spyOn(console, "log");
    await exports.default.fetch(BASE + "/setup");
    const code = claimLog.mock.calls.map((args) => String(args[0])).find((text) => text.startsWith("Koidrive claim code"))!.split(": ")[1]!;
    claimLog.mockRestore();
    const owner = await post("/v1/claim", toBinary(ClaimRequestSchema, create(ClaimRequestSchema, { code, deviceName: "Laptop", platform: "linux" })), DeviceCredentialsSchema);
    const token = owner.access!.token;
    const space = await post("/v1/spaces", toBinary(CreateSpaceRequestSchema, create(CreateSpaceRequestSchema, { name: "Home" })), SpaceInfoSchema, token);

    const lines = recordLogs();
    const secrets = [token, owner.refreshCredential];
    let reqId = 1n;
    const rpc = async (call: MessageInitShape<typeof RpcRequestSchema>["call"]): Promise<RpcResponse["result"]> => {
      const body = toBinary(RpcRequestSchema, create(RpcRequestSchema, { reqId: reqId++, call }));
      return (await post(`/v1/spaces/${space.spaceId}/rpc`, body, RpcResponseSchema, token)).result;
    };

    const response = await exports.default.fetch(`${BASE}/v1/spaces/${space.spaceId}/ws`, { headers: { Upgrade: "websocket", Authorization: "Bearer " + token } });
    const ws = response.webSocket!;
    ws.binaryType = "arraybuffer";
    ws.accept();
    const welcome = new Promise<string>((resolve) =>
      ws.addEventListener("message", (event) => {
        const msg = fromBinary(ServerMessageSchema, new Uint8Array(event.data as ArrayBuffer)).msg;
        if (msg.case === "welcome") resolve(msg.value.historyId);
      }),
    );
    ws.send(toBinary(ClientMessageSchema, create(ClientMessageSchema, { msg: { case: "hello", value: { clientName: "test", clientVersion: "0", protoMin: 1, protoMax: 1, maxFormat: 1 } } })));
    const historyId = await welcome;
    ws.close(1000, "done");

    const file = await content("a file whose upload and download are logged");
    const plan = await rpc({ case: "prepareUpload", value: { chunks: file.content.chunks } });
    if (plan.case !== "prepareUpload") throw new Error("prepareUpload failed");
    const target = plan.value.chunks.find((chunk) => chunk.action === ChunkPlan_Action.UPLOAD)!.target!;
    secrets.push(new URL(target.url).searchParams.get("ticket")!);
    const corrupt = new Uint8Array(file.bytes[0]!.length).fill(42);
    expect((await exports.default.fetch(target.url, { method: "PUT", body: corrupt })).status).toBe(400);
    const stored = await exports.default.fetch(target.url, { method: "PUT", body: file.bytes[0]! });
    const receipt = fromBinary(UploadReceiptSchema, new Uint8Array(await stored.arrayBuffer()));
    secrets.push(receipt.token);

    const nodeId = uuidv7();
    const commit = await rpc({
      case: "commit",
      value: { commitId: uuidv7(), baseSeq: 0n, ops: [op.create(nodeId, space.spaceId, "a.txt", file.content)], receipts: [receipt], historyId, deviceSeq: 1n },
    });
    expect(commit.case).toBe("commit");
    const download = await rpc({ case: "getDownloadTargets", value: { hashes: [file.content.chunks[0]!.hash] } });
    if (download.case !== "downloadTargets") throw new Error("getDownloadTargets failed");
    const url = download.value.targets[0]!.target!.url;
    secrets.push(new URL(url).searchParams.get("ticket")!);
    expect(new Uint8Array(await (await exports.default.fetch(url)).arrayBuffer())).toEqual(file.bytes[0]);
    expect((await exports.default.fetch(url.replace(/ticket=[^&]+/, "ticket=forged"))).status).toBe(403);

    expect(lines.some((line) => line.startsWith("chunk upload failed")), "the failed upload was logged, so the recording works").toBe(true);
    for (const secret of secrets) {
      expect(secret.length).toBeGreaterThan(16);
      expect(lines.filter((line) => line.includes(secret))).toEqual([]);
    }
    expect(lines.filter((line) => /ticket=|Bearer /.test(line))).toEqual([]);
    expect(lines.filter((line) => line.includes("koidrive_rpc")), "each Space RPC logs its kind and item count").toEqual([
      JSON.stringify({ koidrive_rpc: "prepareUpload", koidrive_items: 1 }),
      JSON.stringify({ koidrive_rpc: "commit", koidrive_items: 1 }),
      JSON.stringify({ koidrive_rpc: "getDownloadTargets", koidrive_items: 1 }),
    ]);
  });

  it("format every caught error with errorText, so its name and message reach Workers Logs", () => {
    expect(Object.keys(sources).length).toBeGreaterThan(10);
    expect(rawCaughtErrors('try {} catch (error) { console.error("x (y) failed", error); }'), "the check finds a raw caught error").toHaveLength(1);
    expect(rawCaughtErrors('.catch((error) => {\n  console.error(\n    "x failed",\n    error,\n  );\n});'), "the check finds a call wrapped across lines").toHaveLength(1);
    expect(rawCaughtErrors('try {} catch (error) { console.error("x failed:", errorText(error)); }'), "the check accepts errorText").toEqual([]);
    expect(Object.entries(sources).flatMap(([path, source]) => rawCaughtErrors(source).map((call) => path + ": " + call))).toEqual([]);
  });
});
