// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, type DescMessage, fromBinary, fromJson, type MessageShape, toBinary, toJson } from "@bufbuild/protobuf";
import { Error_Cause, Error_Code, ErrorSchema } from "./gen/koidrive/v1/rpc_pb";

function wantsJson(request: Request): boolean {
  const type = request.headers.get("Content-Type") ?? request.headers.get("Accept") ?? "";
  return type.includes("application/json");
}

export class BadRequest extends Error {}

export async function readMessage<S extends DescMessage>(request: Request, schema: S): Promise<MessageShape<S>> {
  try {
    if (wantsJson(request)) return fromJson(schema, await request.json());
    return fromBinary(schema, new Uint8Array(await request.arrayBuffer()));
  } catch {
    throw new BadRequest(`body is not a valid ${schema.typeName}`);
  }
}

export function reply<S extends DescMessage>(request: Request, schema: S, message: MessageShape<S>, status = 200): Response {
  if (wantsJson(request)) return Response.json(toJson(schema, message), { status });
  return new Response(toBinary(schema, message), { status, headers: { "Content-Type": "application/x-protobuf" } });
}

const STATUS: Partial<Record<Error_Code, number>> = {
  [Error_Code.UNAUTHENTICATED]: 401,
  [Error_Code.FORBIDDEN]: 403,
  [Error_Code.NOT_FOUND]: 404,
  [Error_Code.INVALID_ARGUMENT]: 400,
  [Error_Code.LIMIT_EXCEEDED]: 413,
  [Error_Code.RETRY_LATER]: 429,
  [Error_Code.QUOTA_EXCEEDED]: 429,
  [Error_Code.CONFIRMATION_REQUIRED]: 409,
};

export function errorReply(request: Request, code: Error_Code, message: string, status = STATUS[code] ?? 500, cause = Error_Cause.UNSPECIFIED): Response {
  return reply(request, ErrorSchema, create(ErrorSchema, { code, message, cause }), status);
}
