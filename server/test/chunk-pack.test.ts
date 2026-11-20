// SPDX-License-Identifier: AGPL-3.0-or-later

import { env } from "cloudflare:workers";
import { afterEach, describe, expect, it, vi } from "vitest";
import { encodePack, readPackIndex, SUFFIX_BYTES } from "../src/chunk-pack";
import { sha256 } from "../src/crypto";
import { uuidv7 } from "../src/ids";

afterEach(() => {
  vi.restoreAllMocks();
});

async function chunks(count: number) {
  return Promise.all(
    Array.from({ length: count }, async (_, i) => {
      const data = crypto.getRandomValues(new Uint8Array(1 + ((i * 7919) % 300)));
      return { hash: await sha256(data), data };
    }),
  );
}

describe("chunk packs (RFC 0032 sections 1 and 8)", () => {
  for (const count of [1, 2, 32, 33]) {
    it(`reads back the index of a ${count}-entry pack, with one suffix read up to 32 entries and a second read above`, async () => {
      const packed = await chunks(count);
      const key = `packs-test/${uuidv7()}`;
      const bytes = encodePack(packed);
      await env.BUCKET.put(key, bytes);
      const get = vi.spyOn(env.BUCKET, "get");

      const entries = await readPackIndex(env.BUCKET, key);

      expect(get.mock.calls.map(([, options]) => options)).toEqual(
        count <= 32 ? [{ range: { suffix: SUFFIX_BYTES } }] : [{ range: { suffix: SUFFIX_BYTES } }, { range: expect.objectContaining({ length: count * 36 }) }],
      );
      expect(entries.map(({ hash, size }) => ({ hash, size }))).toEqual(packed.map(({ hash, data }) => ({ hash, size: data.length })));
      for (const [i, { offset, size }] of entries.entries()) expect(bytes.slice(offset, offset + size)).toEqual(packed[i]!.data);
    });
  }

  it("refuses a pack whose footer magic is not KDPACK01", async () => {
    const key = `packs-test/${uuidv7()}`;
    const bytes = encodePack(await chunks(2));
    bytes[bytes.length - 16] = "X".charCodeAt(0);
    await env.BUCKET.put(key, bytes);
    await expect(readPackIndex(env.BUCKET, key)).rejects.toThrow("KDPACK01");
  });
});
