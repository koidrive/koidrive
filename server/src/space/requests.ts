// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, toBinary } from "@bufbuild/protobuf";
import { type Op } from "../gen/koidrive/v1/commit_pb";
import { type RpcRequest, type RpcResponse, RpcResponseSchema, ServerMessageSchema } from "../gen/koidrive/v1/messages_pb";
import { type ChunkRef, type Content } from "../gen/koidrive/v1/model_pb";
import { Error_Code } from "../gen/koidrive/v1/rpc_pb";
import { sha256 } from "../crypto";
import { isUuidV7 } from "../ids";
import { CallFailure } from "./commit";
import { intentsValid } from "./intents";
import { encodeChunkList } from "./store";
import { MAX_CHUNK_BYTES, MAX_SERVER_FRAME, PAGE_ENVELOPE_BYTES } from "./limits";

export function fail(code: Error_Code, message: string): never {
  throw new CallFailure(code, message);
}

export function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i]);
}

export function checkChunkRef(chunk: ChunkRef): void {
  if (chunk.hash.length !== 32) fail(Error_Code.INVALID_ARGUMENT, "chunk hash must be 32 bytes");
  if (chunk.size < 1 || chunk.size > MAX_CHUNK_BYTES) fail(Error_Code.INVALID_ARGUMENT, "chunk size out of range");
  if (chunk.encoding !== 0) fail(Error_Code.INVALID_ARGUMENT, "unsupported chunk encoding");
}

export async function checkContent(content: Content | undefined): Promise<Content> {
  if (!content) fail(Error_Code.INVALID_ARGUMENT, "content is required");
  if (content.chunks.length !== content.chunkCount) fail(Error_Code.INVALID_ARGUMENT, "commits must list every chunk inline");
  content.chunks.forEach(checkChunkRef);
  const total = content.chunks.reduce((sum, chunk) => sum + BigInt(chunk.size), 0n);
  if (total !== content.size) fail(Error_Code.INVALID_ARGUMENT, "content size does not match its chunks");
  if (!sameBytes(await sha256(encodeChunkList(content.chunks)), content.manifestDigest)) {
    fail(Error_Code.INVALID_ARGUMENT, "manifest_digest does not match the chunk list");
  }
  return content;
}

export function checkIds(op: Op, index: number): void {
  const required: string[] = [];
  const optional: string[] = [];
  switch (op.op.case) {
    case "createDir":
      required.push(op.op.value.nodeId, op.op.value.parentId);
      break;
    case "createFile":
      required.push(op.op.value.nodeId, op.op.value.parentId, ...op.op.value.madeOver);
      optional.push(op.op.value.sourceVersionId);
      break;
    case "updateContent":
      required.push(op.op.value.nodeId, ...op.op.value.madeOver);
      optional.push(op.op.value.sourceVersionId);
      break;
    case "delete":
      required.push(op.op.value.nodeId);
      break;
    case "move":
      required.push(op.op.value.nodeId, op.op.value.newParentId);
      break;
    case "restore":
      required.push(op.op.value.nodeId);
      optional.push(op.op.value.versionId, op.op.value.targetParentId);
      break;
    case "markGitTracked":
      required.push(op.op.value.nodeId);
      break;
    default:
      fail(Error_Code.INVALID_ARGUMENT, `op ${index} is empty`);
  }
  if (op.op.case !== "markGitTracked" && !intentsValid(op)) fail(Error_Code.INVALID_ARGUMENT, `op ${index} has a malformed intent`);
  if (!required.every(isUuidV7) || !optional.every((id) => id === "" || isUuidV7(id))) {
    fail(Error_Code.INVALID_ARGUMENT, `op ${index} has a malformed id`);
  }
}

export function opContent(op: Op): Content | undefined | null {
  if (op.op.case === "createFile" || op.op.case === "updateContent") return op.op.value.content;
  return null;
}

export function rpcRoute(request: RpcRequest): string {
  return "rpc_" + (request.call.case ?? "empty").replace(/[A-Z]/g, (letter) => "_" + letter.toLowerCase());
}

export function frameBytes(response: RpcResponse): number {
  return toBinary(ServerMessageSchema, create(ServerMessageSchema, { msg: { case: "rpc", value: response } })).length;
}

export function reply(reqId: bigint, result: RpcResponse["result"]): { response: RpcResponse } {
  const response = create(RpcResponseSchema, { reqId, result });
  if (frameBytes(response) > MAX_SERVER_FRAME) fail(Error_Code.LIMIT_EXCEEDED, "the reply does not fit in one 1 MiB message");
  return { response };
}

export function fitPage<R, T>(rows: R[], build: (row: R) => T, encodedSize: (item: T) => number): T[] {
  const page: T[] = [];
  let bytes = PAGE_ENVELOPE_BYTES;
  for (const row of rows) {
    const item = build(row);
    bytes += encodedSize(item) + 6;
    if (page.length > 0 && bytes > MAX_SERVER_FRAME) break;
    page.push(item);
  }
  return page;
}
