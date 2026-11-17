// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
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

function fakeCargo(status = 0, name = "cargo"): void {
  writeFileSync(join(work, "bin", name), `#!${process.execPath}
const { writeFileSync } = require("node:fs");
const { join } = require("node:path");
writeFileSync(join(process.env.HOME, "${name}.json"), JSON.stringify({ args: process.argv.slice(2), env: process.env, cwd: process.cwd() }));
console.log("| Benchmark | MiB/s | tailed by Rust |");
process.exit(${status});
`, { mode: 0o755 });
}

function run({ command, args = [], env = {} }: { command?: string; args?: string[]; env?: NodeJS.ProcessEnv } = {}): Promise<{ status: number | null; output: string }> {
  const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [...flags, join(here, "bench.ts"), ...(command ? [command] : []), ...args], {
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

describe("first-sync benchmark", () => {
  it("runs the exact release driver test on the tree with private state credentials and an allowlisted environment", async () => {
    fakeCargo();
    const tree = join(work, "tree");
    const result = await run({ command: "first-sync", env: { KOIDRIVE_BENCH_TREE: tree, CARGO_TARGET_DIR: join(work, "target") } });
    expect(result.status).toBe(0);
    const child = JSON.parse(readFileSync(join(work, "cargo.json"), "utf8"));
    expect(child.args).toEqual(["test", "-p", "koidrive-driver", "--release", "--locked", "--test", "first_sync_bench", "first_sync_of_a_large_folder", "--", "--ignored", "--exact", "--nocapture"]);
    expect(child.cwd).toBe(join(here, "..", "..", ".."));
    expect(child.env).toMatchObject({ KOIDRIVE_BENCH_URL: "https://koidrive-test.example.workers.dev", KOIDRIVE_BENCH_OWNER: JSON.stringify(owner), KOIDRIVE_BENCH_TREE: tree, CARGO_TARGET_DIR: join(work, "target") });
    for (const key of ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "GH_TOKEN", "KOIDRIVE_DEPLOY_TEST_STATE", "KOIDRIVE_BENCH_NS"]) expect(child.env).not.toHaveProperty(key);
    expect(result.output).not.toContain(owner.refreshCredential);
  });

  it.each([undefined, "60"])("passes the deadline KOIDRIVE_BENCH_SECONDS=%s on to the driver test", async (seconds) => {
    fakeCargo();
    expect((await run({ command: "first-sync", env: { KOIDRIVE_BENCH_TREE: join(work, "tree"), KOIDRIVE_BENCH_SECONDS: seconds } })).status).toBe(0);
    expect(JSON.parse(readFileSync(join(work, "cargo.json"), "utf8")).env.KOIDRIVE_BENCH_SECONDS).toBe(seconds);
  });

  it("refuses to start without a tree", async () => {
    fakeCargo();
    const result = await run({ command: "first-sync" });
    expect(result.status).toBe(1);
    expect(result.output).toContain("KOIDRIVE_BENCH_TREE");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });

  it("installs the pinned tree with npm ci and no install scripts, without reading the state", async () => {
    fakeCargo(0, "npm");
    writeFileSync(join(work, "state.json"), "invalid private state");
    const dir = join(work, "first-sync");
    const result = await run({ command: "tree", args: [dir] });
    expect(result).toEqual({ status: 0, output: "| Benchmark | MiB/s | tailed by Rust |\n" });
    const child = JSON.parse(readFileSync(join(work, "npm.json"), "utf8"));
    expect(child.args).toEqual(["ci", "--ignore-scripts", "--no-audit", "--no-fund"]);
    expect(child.cwd).toBe(realpathSync(dir));
    for (const key of ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "GH_TOKEN", "KOIDRIVE_DEPLOY_TEST_STATE", "KOIDRIVE_BENCH_OWNER", "KOIDRIVE_BENCH_URL"]) expect(child.env).not.toHaveProperty(key);
    const lock = readFileSync(join(here, "first-sync-tree.json"), "utf8");
    expect(readFileSync(join(dir, "package-lock.json"), "utf8")).toBe(lock);
    const root = JSON.parse(lock).packages[""];
    expect(JSON.parse(readFileSync(join(dir, "package.json"), "utf8"))).toEqual({ name: root.name, version: root.version, private: true, dependencies: root.dependencies });
  });

  it.each([[["tree"]], [["first-sync", "extra"]], [["upload"]], [["tree", "a", "b"]]])("rejects the arguments %j", async (argv) => {
    fakeCargo();
    const [command, ...args] = argv;
    const result = await run({ command, args });
    expect(result.status).toBe(1);
    expect(result.output).toContain("usage: bench.ts");
    expect(existsSync(join(work, "cargo.json"))).toBe(false);
  });
});
