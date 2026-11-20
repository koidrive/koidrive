// SPDX-License-Identifier: AGPL-3.0-or-later

import type { ObjectStore } from "./host";

const MAGIC = new TextEncoder().encode("KDPACK01");
const FOOTER_BYTES = 16;
const ENTRY_BYTES = 36;
export const SUFFIX_ENTRIES = 32;
export const SUFFIX_BYTES = FOOTER_BYTES + SUFFIX_ENTRIES * ENTRY_BYTES;

export interface PackedChunk {
  hash: Uint8Array;
  data: Uint8Array;
}

export interface PackEntry {
  hash: Uint8Array;
  offset: number;
  size: number;
}

export function encodePack(chunks: readonly PackedChunk[]): Uint8Array {
  const bodyBytes = chunks.reduce((sum, chunk) => sum + chunk.data.length, 0);
  const out = new Uint8Array(bodyBytes + chunks.length * ENTRY_BYTES + FOOTER_BYTES);
  const view = new DataView(out.buffer);
  let offset = 0;
  chunks.forEach(({ hash, data }, i) => {
    out.set(data, offset);
    offset += data.length;
    const entry = bodyBytes + i * ENTRY_BYTES;
    out.set(hash, entry);
    view.setUint32(entry + 32, data.length, true);
  });
  const footer = out.length - FOOTER_BYTES;
  out.set(MAGIC, footer);
  view.setUint32(footer + 8, chunks.length, true);
  view.setUint32(footer + 12, bodyBytes, true);
  return out;
}

async function readRange(objects: ObjectStore, key: string, range: { offset: number; length: number } | { suffix: number }) {
  const stored = await objects.get(key, { range });
  if (!stored) throw new Error(`pack ${key} is missing`);
  return { size: stored.size, bytes: new Uint8Array(await new Response(stored.body).arrayBuffer()) };
}

export async function readPackIndex(objects: ObjectStore, key: string): Promise<PackEntry[]> {
  const tail = await readRange(objects, key, { suffix: SUFFIX_BYTES });
  const footer = tail.bytes.length - FOOTER_BYTES;
  if (footer < 0 || MAGIC.some((byte, i) => tail.bytes[footer + i] !== byte)) throw new Error(`${key} does not end in a KDPACK01 footer`);
  const view = new DataView(tail.bytes.buffer, tail.bytes.byteOffset + footer + 8, 8);
  const count = view.getUint32(0, true);
  const indexOffset = view.getUint32(4, true);
  const indexBytes = count * ENTRY_BYTES;
  if (count === 0 || indexOffset + indexBytes + FOOTER_BYTES !== tail.size) throw new Error(`${key} has a footer that does not match its size`);
  const index = count <= SUFFIX_ENTRIES ? tail.bytes.subarray(footer - indexBytes, footer) : (await readRange(objects, key, { offset: indexOffset, length: indexBytes })).bytes;
  if (index.length !== indexBytes) throw new Error(`${key} returned a short index`);
  const sizes = new DataView(index.buffer, index.byteOffset, index.byteLength);
  const entries: PackEntry[] = [];
  let offset = 0;
  for (let i = 0; i < count; i++) {
    const size = sizes.getUint32(i * ENTRY_BYTES + 32, true);
    entries.push({ hash: index.slice(i * ENTRY_BYTES, i * ENTRY_BYTES + 32), offset, size });
    offset += size;
  }
  if (offset !== indexOffset || entries.some((entry) => entry.size === 0)) throw new Error(`${key} has an index that does not match its body`);
  return entries;
}
