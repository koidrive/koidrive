// SPDX-License-Identifier: AGPL-3.0-or-later

import { createHash, createHmac, randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { type Account, getBucket, requireAccount } from "./cloudflare.ts";
import { readState, type State, testName } from "./state.ts";

const API = process.env.CLOUDFLARE_API_BASE_URL || "https://api.cloudflare.com/client/v4";
const SMALL = 64 * 1024;
const SMALL_CHUNKS = 64;
const LARGE = 1024 * 1024;
const LARGE_CHUNKS = 4;
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

async function send(key: S3Key, url: URL, init: { method: "GET" | "PUT"; body?: Uint8Array; range?: string }): Promise<{ bytes: Uint8Array; ms: number }> {
  const amzDate = new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d{3}/, "");
  const headers: Record<string, string> = init.range ? { range: init.range } : {};
  const payloadHash = init.body ? sha256Hex(init.body) : "UNSIGNED-PAYLOAD";
  const signed = { ...headers, "x-amz-content-sha256": payloadHash, "x-amz-date": amzDate, authorization: authorization(key, { method: init.method, url, headers, payloadHash, amzDate, region: "auto" }) };
  const started = performance.now();
  const response = await fetch(url, { method: init.method, headers: signed, body: init.body });
  const bytes = new Uint8Array(await response.arrayBuffer());
  const ms = performance.now() - started;
  if (!response.ok) throw new Failed(`${init.method} ${url.pathname} answered ${response.status}`);
  return { bytes, ms };
}

export async function ownedBucket(account: Account, state: State, name: string): Promise<string> {
  if (state.name !== name) throw new Failed(`the state file names the bucket ${state.name}, not this run's ${name}`);
  const recorded = state.owned.bucketCreationDate;
  if (!recorded) throw new Failed(`this run did not record creating the bucket ${name}`);
  const bucket = await getBucket(account, name);
  if (bucket?.creationDate !== recorded) throw new Failed(`the bucket ${name} is not the one this run created on ${recorded}`);
  return name;
}

export interface Case {
  name: string;
  count: number;
  read: (i: number) => Promise<number>;
}

function shuffled<T>(items: readonly T[]): T[] {
  const out = [...items];
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [out[i], out[j]] = [out[j]!, out[i]!];
  }
  return out;
}

export async function measure(cases: readonly Case[], rounds: number): Promise<{ first: Map<string, number[]>; warm: Map<string, number[]> }> {
  const first = new Map(cases.map(({ name }) => [name, [] as number[]]));
  const warm = new Map(cases.map(({ name }) => [name, [] as number[]]));
  for (const { one, i } of shuffled(cases.flatMap((one) => Array.from({ length: one.count }, (_, i) => ({ one, i }))))) {
    first.get(one.name)!.push(await one.read(i));
  }
  for (let round = 0; round < rounds; round++) {
    for (const one of shuffled(cases)) warm.get(one.name)!.push(await one.read(Math.floor(Math.random() * one.count)));
  }
  return { first, warm };
}

async function bench(): Promise<void> {
  const account = requireAccount();
  const bucket = await ownedBucket(account, readState(), testName());
  const key = await s3Key(account);
  const url = (path: string) => new URL(`https://${account.id}.r2.cloudflarestorage.com/${bucket}/bench/${path}`);
  const small = Array.from({ length: SMALL_CHUNKS }, () => randomBytes(SMALL));
  const large = Array.from({ length: LARGE_CHUNKS }, () => randomBytes(LARGE));
  await send(key, url("pack-64k"), { method: "PUT", body: Buffer.concat(small) });
  await send(key, url("pack-1m"), { method: "PUT", body: Buffer.concat(large) });
  for (const [i, chunk] of small.entries()) await send(key, url(`single-64k/${i}`), { method: "PUT", body: chunk });
  for (const [i, chunk] of large.entries()) await send(key, url(`single-1m/${i}`), { method: "PUT", body: chunk });

  const checked = async (read: Promise<{ bytes: Uint8Array; ms: number }>, want: Uint8Array, what: string) => {
    const { bytes, ms } = await read;
    if (!Buffer.from(bytes).equals(want)) throw new Failed(`${what} returned other bytes`);
    return ms;
  };
  const ranged = (path: string, size: number, i: number) => send(key, url(path), { method: "GET", range: `bytes=${i * size}-${(i + 1) * size - 1}` });
  const cases: Case[] = [
    { name: "64 KiB range of a 4 MiB pack", count: SMALL_CHUNKS, read: (i) => checked(ranged("pack-64k", SMALL, i), small[i]!, "a 64 KiB range") },
    { name: "64 KiB single object", count: SMALL_CHUNKS, read: (i) => checked(send(key, url(`single-64k/${i}`), { method: "GET" }), small[i]!, "a 64 KiB object") },
    { name: "1 MiB range of a 4 MiB pack", count: LARGE_CHUNKS, read: (i) => checked(ranged("pack-1m", LARGE, i), large[i]!, "a 1 MiB range") },
    { name: "1 MiB single object", count: LARGE_CHUNKS, read: (i) => checked(send(key, url(`single-1m/${i}`), { method: "GET" }), large[i]!, "a 1 MiB object") },
  ];
  const { first, warm } = await measure(cases, ROUNDS);
  const rows = cases.map(({ name }) => {
    const once = first.get(name)!.sort((a, b) => a - b);
    const again = warm.get(name)!.sort((a, b) => a - b);
    return { name, first: { n: once.length, p50: percentile(once, 50), max: once.at(-1)! }, warm: { n: again.length, p50: percentile(again, 50), p90: percentile(again, 90), p99: percentile(again, 99) } };
  });
  const ms = (value: number) => value.toFixed(1);
  const table = [
    "| Read | First reads | First p50 ms | First max ms | Warm reads | Warm p50 ms | Warm p90 ms | Warm p99 ms |",
    "|---|---|---|---|---|---|---|---|",
    ...rows.map(({ name, first, warm }) => `| ${name} | ${first.n} | ${ms(first.p50)} | ${ms(first.max)} | ${warm.n} | ${ms(warm.p50)} | ${ms(warm.p90)} | ${ms(warm.p99)} |`),
    "",
    `First reads touch each range or object once, in a random order across all four kinds. Warm reads follow: ${ROUNDS} rounds, one random read of each kind per round in a random order. Each read went from this runner to R2 through the S3 API, one at a time. Its time runs from the request to the last byte, so it includes the network path between the runner and R2, which every kind shares. The body is compared with the bytes written after the timer stops.`,
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
