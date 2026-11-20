// SPDX-License-Identifier: AGPL-3.0-or-later

import { createHash, createHmac, randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { type Account, requireAccount } from "./cloudflare.ts";
import { readState } from "./state.ts";

const API = process.env.CLOUDFLARE_API_BASE_URL || "https://api.cloudflare.com/client/v4";
const SMALL = 64 * 1024;
const SMALL_CHUNKS = 64;
const LARGE = 1024 * 1024;
const LARGE_CHUNKS = 4;
const WARMUP_ROUNDS = 10;
const ROUNDS = 200;

class Failed extends Error {}

export interface S3Key {
  id: string;
  secret: string;
}

export interface S3Request {
  method: "GET" | "PUT";
  url: URL;
  headers: Record<string, string>;
  payloadHash: string;
  amzDate: string;
  region: string;
}

const sha256Hex = (data: string | Uint8Array) => createHash("sha256").update(data).digest("hex");
const hmac = (key: string | Buffer, data: string) => createHmac("sha256", key).update(data).digest();

export function authorization(key: S3Key, request: S3Request): string {
  const headers = { ...Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [name.toLowerCase(), value.trim()])), host: request.url.host, "x-amz-content-sha256": request.payloadHash, "x-amz-date": request.amzDate };
  const names = Object.keys(headers).sort();
  const canonical = [request.method, request.url.pathname, "", ...names.map((name) => `${name}:${headers[name as keyof typeof headers]}`), "", names.join(";"), request.payloadHash].join("\n");
  const date = request.amzDate.slice(0, 8);
  const scope = `${date}/${request.region}/s3/aws4_request`;
  const signing = [date, request.region, "s3", "aws4_request"].reduce<string | Buffer>((secret, part) => hmac(secret, part), "AWS4" + key.secret);
  const signature = createHmac("sha256", signing).update(["AWS4-HMAC-SHA256", request.amzDate, scope, sha256Hex(canonical)].join("\n")).digest("hex");
  return `AWS4-HMAC-SHA256 Credential=${key.id}/${scope}, SignedHeaders=${names.join(";")}, Signature=${signature}`;
}

export function percentile(sorted: readonly number[], p: number): number {
  return sorted[Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1)]!;
}

async function s3Key(account: Account): Promise<S3Key> {
  const response = await fetch(`${API}/accounts/${account.id}/tokens/verify`, { headers: { Authorization: `Bearer ${account.token}` } });
  const id = ((await response.json()) as { result?: { id?: string } }).result?.id;
  if (!response.ok || !id) throw new Failed(`verifying the deploy-test token answered ${response.status} with no token id`);
  return { id, secret: sha256Hex(account.token) };
}

async function send(key: S3Key, url: URL, init: { method: "GET" | "PUT"; body?: Uint8Array; range?: string }): Promise<Uint8Array> {
  const amzDate = new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d{3}/, "");
  const headers: Record<string, string> = init.range ? { range: init.range } : {};
  const payloadHash = init.body ? sha256Hex(init.body) : "UNSIGNED-PAYLOAD";
  const signed = { ...headers, "x-amz-content-sha256": payloadHash, "x-amz-date": amzDate, authorization: authorization(key, { method: init.method, url, headers, payloadHash, amzDate, region: "auto" }) };
  const response = await fetch(url, { method: init.method, headers: signed, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (!response.ok) throw new Failed(`${init.method} ${url.pathname} answered ${response.status}`);
  return bytes;
}

interface Case {
  name: string;
  read: (i: number) => Promise<void>;
  count: number;
}

async function bench(): Promise<void> {
  const account = requireAccount();
  const key = await s3Key(account);
  const base = `https://${account.id}.r2.cloudflarestorage.com/${readState().name}/bench/`;
  const url = (path: string) => new URL(base + path);
  const small = Array.from({ length: SMALL_CHUNKS }, () => randomBytes(SMALL));
  const large = Array.from({ length: LARGE_CHUNKS }, () => randomBytes(LARGE));
  const largeHashes = large.map((chunk) => sha256Hex(chunk));
  await send(key, url("pack-64k"), { method: "PUT", body: Buffer.concat(small) });
  await send(key, url("pack-1m"), { method: "PUT", body: Buffer.concat(large) });
  for (const [i, chunk] of small.entries()) await send(key, url(`single-64k/${i}`), { method: "PUT", body: chunk });
  for (const [i, chunk] of large.entries()) await send(key, url(`single-1m/${i}`), { method: "PUT", body: chunk });

  const same = (got: Uint8Array, want: Uint8Array, what: string) => {
    if (!Buffer.from(got).equals(want)) throw new Failed(`${what} returned other bytes`);
  };
  const cases: Case[] = [
    { name: "64 KiB ranged read of a 4 MiB pack", count: SMALL_CHUNKS, read: async (i) => same(await send(key, url("pack-64k"), { method: "GET", range: `bytes=${i * SMALL}-${(i + 1) * SMALL - 1}` }), small[i]!, "a 64 KiB range") },
    { name: "64 KiB whole single object", count: SMALL_CHUNKS, read: async (i) => same(await send(key, url(`single-64k/${i}`), { method: "GET" }), small[i]!, "a 64 KiB object") },
    {
      name: "1 MiB ranged read of a 4 MiB pack, with SHA-256",
      count: LARGE_CHUNKS,
      read: async (i) => {
        const bytes = await send(key, url("pack-1m"), { method: "GET", range: `bytes=${i * LARGE}-${(i + 1) * LARGE - 1}` });
        if (bytes.length !== LARGE || sha256Hex(bytes) !== largeHashes[i]) throw new Failed("a 1 MiB range failed its SHA-256 check");
      },
    },
    { name: "1 MiB whole single object", count: LARGE_CHUNKS, read: async (i) => same(await send(key, url(`single-1m/${i}`), { method: "GET" }), large[i]!, "a 1 MiB object") },
  ];
  const timings = new Map<string, number[]>(cases.map(({ name }) => [name, []]));
  for (let round = 0; round < WARMUP_ROUNDS + ROUNDS; round++) {
    for (const one of [...cases].sort(() => Math.random() - 0.5)) {
      const started = performance.now();
      await one.read(Math.floor(Math.random() * one.count));
      if (round >= WARMUP_ROUNDS) timings.get(one.name)!.push(performance.now() - started);
    }
  }
  const rows = cases.map(({ name }) => {
    const sorted = timings.get(name)!.sort((a, b) => a - b);
    return { name, n: sorted.length, p50: percentile(sorted, 50), p90: percentile(sorted, 90), p99: percentile(sorted, 99) };
  });
  const table = [
    "| Read | Reads | p50 ms | p90 ms | p99 ms |",
    "|---|---|---|---|---|",
    ...rows.map((row) => `| ${row.name} | ${row.n} | ${row.p50.toFixed(1)} | ${row.p90.toFixed(1)} | ${row.p99.toFixed(1)} |`),
    "",
    `Each read went from this runner to R2 through the S3 API, one at a time, in a random order of kinds per round, after ${WARMUP_ROUNDS} warm-up rounds. Every body was compared with the bytes written. The time runs from the request to the last byte, so it includes the network path between the runner and R2, which every kind shares.`,
  ].join("\n");
  console.log(table);
  console.log("ranged-read-bench " + JSON.stringify(rows));
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, "## R2 ranged reads\n\n" + table + "\n");
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    await bench();
  } catch (error) {
    console.error("::error::ranged-read benchmark: " + (error instanceof Failed ? error.message : "benchmark failed; private error details suppressed"));
    process.exitCode = 1;
  }
}
