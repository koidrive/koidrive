// SPDX-License-Identifier: AGPL-3.0-or-later

import { fromBinary } from "@bufbuild/protobuf";
import { type ChunkBundle, ChunkBundleSchema } from "./gen/koidrive/v1/rpc_pb";
import { BadRequest } from "./http";

export const MAX_BUNDLE_CHUNKS = 32;

export function countBundledChunks(bytes: Uint8Array): number {
  const malformed = () => new BadRequest("body is not a valid koidrive.v1.ChunkBundle");
  let at = 0;
  const varint = () => {
    let value = 0;
    for (let shift = 0; ; shift += 7) {
      if (at >= bytes.length || shift > 49) throw malformed();
      const byte = bytes[at++]!;
      value += (byte & 0x7f) * 2 ** shift;
      if (byte < 0x80) return value;
    }
  };
  const skip = (length: number) => {
    if (length > bytes.length - at) throw malformed();
    at += length;
  };
  let count = 0;
  while (at < bytes.length) {
    const tag = varint();
    const field = Math.floor(tag / 8);
    const wire = tag % 8;
    if (field === 0 || (field === 1 && wire !== 2)) throw malformed();
    if (wire === 0) varint();
    else if (wire === 1) skip(8);
    else if (wire === 2) skip(varint());
    else if (wire === 5) skip(4);
    else throw malformed();
    if (field === 1 && ++count > MAX_BUNDLE_CHUNKS) return count;
  }
  return count;
}

export function decodeChunkBundle(bytes: Uint8Array): ChunkBundle {
  try {
    return fromBinary(ChunkBundleSchema, bytes, { readUnknownFields: false });
  } catch {
    throw new BadRequest("body is not a valid koidrive.v1.ChunkBundle");
  }
}
