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

describe("chunk packs: format bounds (RFC 0032 section 1 rule 3)", () => {
  const MiB = 1 << 20;
  const hash = (n: number) => Uint8Array.of(n & 255, n >> 8, ...new Uint8Array(30));
  const member = (n: number, size: number) => ({ hash: hash(n), data: new Uint8Array(size) });
  const invalid = [
    ["chunk over 1 MiB", [member(1, MiB + 1)]],
    ["body over 4 MiB", [1, 2, 3, 4, 5].map((n) => member(n, MiB))],
    ["chunk listed twice", [member(1, 1), member(1, 1)]],
    ["pack of more than 4,096 chunks", Array.from({ length: 4097 }, (_, n) => member(n, 1))],
  ] as const;
  const unchecked = (packed: readonly { hash: Uint8Array; data: Uint8Array }[]) => {
    const body = packed.reduce((n, member) => n + member.data.length, 0);
    const bytes = new Uint8Array(body + packed.length * 36 + 16);
    const footer = bytes.length - 16;
    const view = new DataView(bytes.buffer);
    packed.forEach(({ hash, data }, i) => {
      bytes.set(hash, body + i * 36);
      view.setUint32(body + i * 36 + 32, data.length, true);
    });
    bytes.set(new TextEncoder().encode("KDPACK01"), footer);
    view.setUint32(footer + 8, packed.length, true);
    view.setUint32(footer + 12, body, true);
    return bytes;
  };
  for (const [name, packed] of invalid) {
    it(`rejects a ${name} when encoding`, () => expect(() => encodePack(packed)).toThrow());
    it(`rejects a ${name} when reading the index`, async () => {
      const key = `packs-test/${uuidv7()}`;
      await env.BUCKET.put(key, unchecked(packed));
      await expect(readPackIndex(env.BUCKET, key)).rejects.toThrow();
    });
  }
  it("rejects an invalid hash width, an empty member and an empty pack when encoding", () => {
    const cases = [[{ hash: new Uint8Array(31), data: new Uint8Array(1) }], [member(1, 0)], []];
    for (const packed of cases) expect(() => encodePack(packed)).toThrow();
  });
  it("rejects truncation, corrupt cumulative sizes and a short second index read", async () => {
    const sample = Array.from({ length: 33 }, (_, i) => member(i, 2));
    const bytes = encodePack(sample);
    const key = `packs-test/${uuidv7()}`;
    await env.BUCKET.put(key, bytes.slice(0, -1));
    await expect(readPackIndex(env.BUCKET, key)).rejects.toThrow();
    const wrong = bytes.slice();
    new DataView(wrong.buffer).setUint32(33 * 2 + 32, 3, true);
    await env.BUCKET.put(key, wrong);
    await expect(readPackIndex(env.BUCKET, key)).rejects.toThrow();
    await env.BUCKET.put(key, bytes);
    const get = env.BUCKET.get.bind(env.BUCKET);
    vi.spyOn(env.BUCKET, "get").mockImplementation((async (key: string, options?: R2GetOptions) => {
      const found = await get(key, options);
      if (!found || !options?.range || !("offset" in options.range)) return found;
      return { size: found.size, body: new Blob([new Uint8Array(33 * 36 - 1)]).stream() };
    }) as never);
    await expect(readPackIndex(env.BUCKET, key)).rejects.toThrow("short index");
  });
});
