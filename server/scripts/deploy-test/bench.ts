// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { readState } from "./state.ts";

class Failed extends Error {}

const SUPPORTED_NS = ["1", "2", "4", "8"];
const ROOT = fileURLToPath(new URL("../../../", import.meta.url));
const TREE_LOCK = fileURLToPath(new URL("./first-sync-tree.json", import.meta.url));
const USAGE = "usage: bench.ts [validate | first-sync | tree DIR]";

function inputs(): { ns: string; repetitions: string } {
  const ns = process.env.KOIDRIVE_BENCH_NS ?? SUPPORTED_NS.join(",");
  const values = ns.split(",");
  if (values.length > 4 || new Set(values).size !== values.length || values.some((n) => !SUPPORTED_NS.includes(n))) {
    throw new Failed("KOIDRIVE_BENCH_NS supports only " + SUPPORTED_NS.join(",") + " on this build; provide distinct comma-separated values");
  }
  const repetitions = process.env.KOIDRIVE_BENCH_REPETITIONS ?? "3";
  if (!/^[1-3]$/.test(repetitions)) throw new Failed("KOIDRIVE_BENCH_REPETITIONS must be an integer from 1 to 3");
  return { ns, repetitions };
}

function allowlisted(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const key of ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "SSL_CERT_FILE", "SSL_CERT_DIR", "TMPDIR", "GITHUB_STEP_SUMMARY", "GITHUB_SHA", "RUNNER_ENVIRONMENT", "RUNNER_NAME", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_REGION", "AZURE_REGION"]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  return env;
}

function claimed(): NodeJS.ProcessEnv {
  let state: ReturnType<typeof readState>;
  try {
    state = readState();
  } catch {
    throw new Failed("could not read the private deploy-test state");
  }
  if (!state?.url || !state.owner) throw new Failed("benchmark requires a claimed deployment in the deploy-test state");
  return { ...allowlisted(), KOIDRIVE_BENCH_URL: state.url, KOIDRIVE_BENCH_OWNER: JSON.stringify(state.owner) };
}

function run(program: string, args: string[], env: NodeJS.ProcessEnv, cwd = ROOT): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const child = spawn(program, args, { cwd, env, stdio: ["ignore", "inherit", "inherit"] });
    child.on("error", () => reject(new Failed("could not start " + program)));
    child.on("close", (status) => {
      if (status === 0) resolve();
      else reject(new Failed(status === null ? program + " terminated without an exit status" : program + " exited with status " + status));
    });
  });
}

function rustTest(crate: string, test: string, name: string): string[] {
  return ["test", "-p", crate, "--release", "--locked", "--test", test, name, "--", "--ignored", "--exact", "--nocapture"];
}

async function uploads({ ns, repetitions }: ReturnType<typeof inputs>): Promise<void> {
  const env: NodeJS.ProcessEnv = { ...claimed(), KOIDRIVE_BENCH_NS: ns, KOIDRIVE_BENCH_REPETITIONS: repetitions };
  if (!process.env.GITHUB_ACTIONS && process.env.KOIDRIVE_BENCH_MIB !== undefined) env.KOIDRIVE_BENCH_MIB = process.env.KOIDRIVE_BENCH_MIB;
  await run("cargo", rustTest("koidrive-net", "upload_bench", "upload_benchmark"), env);
}

async function firstSync(): Promise<void> {
  const tree = process.env.KOIDRIVE_BENCH_TREE;
  if (!tree) throw new Failed("KOIDRIVE_BENCH_TREE must name the folder to sync");
  const env: NodeJS.ProcessEnv = { ...claimed(), KOIDRIVE_BENCH_TREE: tree };
  if (process.env.KOIDRIVE_BENCH_SECONDS !== undefined) env.KOIDRIVE_BENCH_SECONDS = process.env.KOIDRIVE_BENCH_SECONDS;
  await run("cargo", rustTest("koidrive-driver", "first_sync_bench", "first_sync_of_a_large_folder"), env);
}

async function installTree(dir: string): Promise<void> {
  const lock = readFileSync(TREE_LOCK, "utf8");
  const root = JSON.parse(lock).packages[""];
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: root.name, version: root.version, private: true, dependencies: root.dependencies }, null, 2) + "\n");
  writeFileSync(join(dir, "package-lock.json"), lock);
  await run("npm", ["ci", "--ignore-scripts", "--no-audit", "--no-fund"], allowlisted(), dir);
}

try {
  const config = inputs();
  const [command, dir, ...rest] = process.argv.slice(2);
  if (rest.length > 0 || (command === "tree") !== (dir !== undefined)) throw new Failed(USAGE);
  if (command === undefined) await uploads(config);
  else if (command === "first-sync") await firstSync();
  else if (command === "tree") await installTree(dir);
  else if (command !== "validate") throw new Failed(USAGE);
} catch (error) {
  console.error("::error::deploy benchmark: " + (error instanceof Failed ? error.message : "benchmark runner failed; private error details suppressed"));
  process.exitCode = 1;
}
