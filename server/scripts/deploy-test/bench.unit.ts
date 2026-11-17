// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { MockCloudflare } from "./mock-cloudflare.ts";
import type { State } from "./state.ts";

const here = dirname(fileURLToPath(import.meta.url));
const owner = { userId: "owner-id", deviceId: "device-id", refreshCredential: "private-refresh-credential" };
let work: string;

beforeEach(() => {
  work = mkdtempSync(join(tmpdir(), "koidrive-bench-unit-"));
  mkdirSync(join(work, "bin"));
  writeState({ url: "https://koidrive-test.example.workers.dev", owner });
});

afterEach(() => rmSync(work, { recursive: true, force: true }));

function writeState(extra: Partial<State>): void {
  const state: State = { name: "koidrive-test-1-1", owned: {}, deployments: [], stats: { requests: 2, bytesUp: 3, bytesDown: 4 }, ...extra };
  writeFileSync(join(work, "state.json"), JSON.stringify(state));
}

function fakeCargo(status = 0): void {
  writeFileSync(join(work, "bin", "cargo"), `#!${process.execPath}
const { writeFileSync } = require("node:fs");
const { join } = require("node:path");
writeFileSync(join(process.env.HOME, "cargo.json"), JSON.stringify({ args: process.argv.slice(2), env: process.env, cwd: process.cwd() }));
if (process.env.KOIDRIVE_BENCH_WINDOWS) writeFileSync(process.env.KOIDRIVE_BENCH_WINDOWS, JSON.stringify([{ side: "bundles", startMs: 1700000000000, endMs: 1700000060000, requests: 626, chunks: 20000 }, { side: "puts", startMs: 1700000100000, endMs: 1700000400000, requests: 20000, chunks: 20000 }]));
console.log("| Benchmark | MiB/s | tailed by Rust |");
process.exit(${status});
`, { mode: 0o755 });
}

function run({ command, env = {} }: { command?: string | string[]; env?: NodeJS.ProcessEnv } = {}): Promise<{ status: number | null; output: string }> {
  const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [...flags, join(here, "bench.ts"), ...(command === undefined ? [] : [command].flat())], {
      cwd: join(here, "..", ".."),
      env: {
        PATH: join(work, "bin") + delimiter + process.env.PATH,
        HOME: work,
        TMPDIR: work,
        KOIDRIVE_DEPLOY_TEST_STATE: join(work, "state.json"),
        CLOUDFLARE_API_TOKEN: "private-cloudflare-token",
        CLOUDFLARE_ACCOUNT_ID: "private-cloudflare-account",
        GH_TOKEN: "private-github-token",
        KOIDRIVE_BENCH_OWNER: "must-not-use-this-owner",
        ...env,
      },
    });
    let output = "";
    child.stdout.on("data", (chunk: Buffer) => (output += chunk));
    child.stderr.on("data", (chunk: Buffer) => (output += chunk));
    child.on("error", reject);
    child.on("close", (status) => resolve({ status, output }));
  });
}

describe("benchmark input validation before deployment", () => {
  it.each(["1", "2", "4", "8", "1,2,4,8", "8,1"])("accepts the supported workflow list %s without state or cargo", async (ns) => {
    rmSync(join(work, "state.json"));
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_NS: ns, KOIDRIVE_BENCH_REPETITIONS: "3" } });
    expect(result).toEqual({ status: 0, output: "" });
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it.each(["", "0", "3", "16", "1,1", "1,2,4,8,1", "1, 2", "1,", "1;echo injected"])("rejects invalid list %s before cargo", async (ns) => {
    fakeCargo();
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_NS: ns } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("KOIDRIVE_BENCH_NS");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it.each(["", "0", "4", "1.5", "01", "2e0", "2;echo injected"])("rejects invalid repetitions %s", async (repetitions) => {
    const result = await run({ command: "validate", env: { KOIDRIVE_BENCH_REPETITIONS: repetitions } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("KOIDRIVE_BENCH_REPETITIONS");
  });
});

describe("Rust benchmark child", () => {
  it("runs the exact release test with private state credentials and an allowlisted environment", async () => {
    fakeCargo();
    const result = await run({ env: { GITHUB_STEP_SUMMARY: join(work, "summary.md"), RUNNER_OS: "Linux", CARGO_TARGET_DIR: join(work, "target") } });
    expect(result.status).toBe(0);
    expect(result.output).toContain("| Benchmark |");
    const child = JSON.parse(readFileSync(join(work, "cargo.json"), "utf8"));
    expect(child.args).toEqual(["test", "-p", "koidrive-net", "--release", "--locked", "--test", "upload_bench", "upload_benchmark", "--", "--ignored", "--exact", "--nocapture"]);
    expect(child.cwd).toBe(join(here, "..", "..", ".."));
    expect(child.env).toMatchObject({ KOIDRIVE_BENCH_URL: "https://koidrive-test.example.workers.dev", KOIDRIVE_BENCH_OWNER: JSON.stringify(owner), KOIDRIVE_BENCH_NS: "1,2,4,8", KOIDRIVE_BENCH_REPETITIONS: "3", RUNNER_OS: "Linux", GITHUB_STEP_SUMMARY: join(work, "summary.md"), CARGO_TARGET_DIR: join(work, "target") });
    expect(child.env).not.toHaveProperty("CLOUDFLARE_API_TOKEN");
    expect(child.env).not.toHaveProperty("CLOUDFLARE_ACCOUNT_ID");
    expect(child.env).not.toHaveProperty("GH_TOKEN");
    expect(child.env).not.toHaveProperty("KOIDRIVE_DEPLOY_TEST_STATE");
    expect(result.output).not.toContain(owner.refreshCredential);
    expect(JSON.parse(readFileSync(join(work, "state.json"), "utf8")).stats).toEqual({ requests: 2, bytesUp: 3, bytesDown: 4 });
  });

  it.each(["0", "3", "16"])("refuses unsupported concurrency %s without reading state or launching cargo", async (ns) => {
    fakeCargo();
    writeFileSync(join(work, "state.json"), "invalid private state");
    const result = await run({ env: { KOIDRIVE_BENCH_NS: ns, KOIDRIVE_BENCH_REPETITIONS: "1" } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("supports only 1,2,4,8 on this build");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("reports cargo failure without credentials", async () => {
    fakeCargo(7);
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("cargo exited with status 7");
    expect(result.output).not.toContain(owner.refreshCredential);
  });

  it.each([{ name: "URL", state: { url: undefined } }, { name: "owner", state: { owner: undefined } }])("refuses state missing $name before launching cargo", async ({ state }) => {
    writeState({ url: "https://koidrive-test.example.workers.dev", owner, ...state });
    fakeCargo();
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("claimed deployment");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("does not expose malformed credential-bearing state in exceptions", async () => {
    writeFileSync(join(work, "state.json"), '{"refreshCredential":"private-refresh-credential"');
    const result = await run();
    expect(result.status).toBe(1);
    expect(result.output).toContain("could not read the private deploy-test state");
    expect(result.output).not.toContain(owner.refreshCredential);
    expect(result.output).not.toContain("SyntaxError");
  });

  it("sanitizes spawn errors even when PATH contains a credential", async () => {
    const result = await run({ env: { PATH: join(work, owner.refreshCredential) } });
    expect(result.status).toBe(1);
    expect(result.output).toContain("could not start cargo");
    expect(result.output).not.toContain(owner.refreshCredential);
  });

  it.each([undefined, "true"])("allows a smaller payload only outside GitHub Actions, GITHUB_ACTIONS=%s", async (actions) => {
    fakeCargo();
    expect((await run({ env: { GITHUB_ACTIONS: actions, KOIDRIVE_BENCH_MIB: "8" } })).status).toBe(0);
    expect(JSON.parse(readFileSync(join(work, "cargo.json"), "utf8")).env.KOIDRIVE_BENCH_MIB).toBe(actions ? undefined : "8");
  });
});


describe("small-file benchmark", () => {
  let cloudflare: MockCloudflare;
  let api: string;

  beforeEach(async () => {
    cloudflare = new MockCloudflare();
    api = (await cloudflare.start()) + "/client/v4";
  });

  afterEach(() => cloudflare.stop());

  const invocations = (requests: number, p50 = 1234, p99 = 5678) => ({
    status: 200,
    body: { data: { viewer: { accounts: [{ workersInvocationsAdaptive: [{ sum: { requests, errors: 0 }, quantiles: { cpuTimeP50: p50, cpuTimeP99: p99 } }] }] } }, errors: null },
  });

  it.each([
    ["KOIDRIVE_BENCH_SMALL_FILES", "31"],
    ["KOIDRIVE_BENCH_SMALL_FILES", "50001"],
    ["KOIDRIVE_BENCH_SMALL_FILES", "2e4"],
    ["KOIDRIVE_BENCH_SMALL_KIB", "0"],
    ["KOIDRIVE_BENCH_SMALL_KIB", "256"],
    ["KOIDRIVE_BENCH_SMALL_KIB", "16;echo injected"],
    ["KOIDRIVE_BENCH_SMALL_RUNS", "0"],
    ["KOIDRIVE_BENCH_SMALL_RUNS", "6"],
  ])("rejects %s=%s before cargo", async (key, value) => {
    fakeCargo();
    const result = await run({ command: ["small", "validate"], env: { [key]: value } });
    expect(result.status).toBe(1);
    expect(result.output).toContain(key);
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("runs the exact small-file test without Cloudflare credentials and reports Worker CPU per request", async () => {
    fakeCargo();
    cloudflare.graphqlAnswers = [invocations(627), invocations(20001, 900, 2100)];
    const result = await run({ command: "small", env: { CLOUDFLARE_API_BASE_URL: api, GITHUB_STEP_SUMMARY: join(work, "summary.md") } });
    expect(result.status).toBe(0);
    const child = JSON.parse(readFileSync(join(work, "cargo.json"), "utf8"));
    expect(child.args).toEqual(["test", "-p", "koidrive-net", "--release", "--locked", "--test", "upload_bench", "small_file_benchmark", "--", "--ignored", "--exact", "--nocapture"]);
    expect(child.env).toMatchObject({ KOIDRIVE_BENCH_SMALL_FILES: "20000", KOIDRIVE_BENCH_SMALL_KIB: "16", KOIDRIVE_BENCH_SMALL_RUNS: "3" });
    expect(child.env).not.toHaveProperty("CLOUDFLARE_API_TOKEN");
    expect(child.env).not.toHaveProperty("KOIDRIVE_BENCH_NS");
    expect(cloudflare.graphqlVariables[0]).toMatchObject({ accountTag: "private-cloudflare-account", scriptName: "koidrive-test-1-1", start: "2023-11-14T22:13:20.000Z", end: "2023-11-14T22:14:20.000Z" });
    const summary = readFileSync(join(work, "summary.md"), "utf8");
    expect(summary).toContain("| bundles | 626 | 627 | 1.23 | 5.68 | 0 |");
    expect(summary).toContain("| puts | 20000 | 20001 | 0.90 | 2.10 | 0 |");
  });

  it.each([
    { name: "HTTP 403", answer: { status: 403, body: { success: false, errors: [{ code: 10000, message: "Authentication error" }] } } },
    { name: "a GraphQL authorization error", answer: { status: 200, body: { data: null, errors: [{ message: "not authorized for that account" }] } } },
  ])("still passes with sync numbers when the token lacks Account Analytics Read ($name)", async ({ answer }) => {
    fakeCargo();
    cloudflare.graphqlAnswers = [answer];
    const result = await run({ command: "small", env: { CLOUDFLARE_API_BASE_URL: api } });
    expect(result.status).toBe(0);
    expect(result.output).toContain("Worker CPU unavailable: the deploy-test token lacks Account Analytics Read");
    expect(result.output).not.toContain("private-cloudflare-token");
  });

  it("keeps the first side's Worker CPU when the query for the second side fails", async () => {
    fakeCargo();
    cloudflare.graphqlAnswers = [invocations(627), { status: 500, body: { success: false, errors: [{ code: 10000, message: "internal" }] } }];
    const result = await run({ command: "small", env: { CLOUDFLARE_API_BASE_URL: api } });
    expect(result.status).toBe(0);
    expect(result.output).toContain("| bundles | 626 | 627 | 1.23 | 5.68 | 0 |");
    expect(result.output).toContain("Worker CPU unavailable from the puts side on:");
    expect(result.output).not.toContain("| puts |");
  });

  it("still passes when no Cloudflare credentials are set", async () => {
    fakeCargo();
    const result = await run({ command: "small", env: { CLOUDFLARE_API_TOKEN: "", CLOUDFLARE_ACCOUNT_ID: "" } });
    expect(result.status).toBe(0);
    expect(result.output).toContain("Worker CPU unavailable: CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID are not set");
  });
});

