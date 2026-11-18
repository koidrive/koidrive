// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { type Account, accountFromEnv, ApiError, type Deadline, DeadlineError, graphql } from "./cloudflare.ts";
import { readState } from "./state.ts";

class Failed extends Error {}

const SUPPORTED_NS = ["1", "2", "4", "8"];
const ANALYTICS_MS = 10 * 60_000;
const ANALYTICS_EVERY_MS = 30_000;
const LAST_QUERY_MS = 60_000;

type Kind = "chunks" | "small";

interface Inputs {
  kind: Kind;
  env: Record<string, string>;
}

function bounded(name: string, value: string, min: number, max: number): string {
  if (!/^[1-9][0-9]*$/.test(value) || Number(value) < min || Number(value) > max) throw new Failed(name + " must be an integer from " + min + " to " + max);
  return value;
}

function inputs(kind: Kind): Inputs {
  if (kind === "small") {
    return {
      kind,
      env: {
        KOIDRIVE_BENCH_SMALL_FILES: bounded("KOIDRIVE_BENCH_SMALL_FILES", process.env.KOIDRIVE_BENCH_SMALL_FILES ?? "20000", 32, 50_000),
        KOIDRIVE_BENCH_SMALL_KIB: bounded("KOIDRIVE_BENCH_SMALL_KIB", process.env.KOIDRIVE_BENCH_SMALL_KIB ?? "16", 1, 255),
        KOIDRIVE_BENCH_SMALL_RUNS: bounded("KOIDRIVE_BENCH_SMALL_RUNS", process.env.KOIDRIVE_BENCH_SMALL_RUNS ?? "3", 1, 5),
      },
    };
  }
  const ns = process.env.KOIDRIVE_BENCH_NS ?? SUPPORTED_NS.join(",");
  const values = ns.split(",");
  if (values.length > 4 || new Set(values).size !== values.length || values.some((n) => !SUPPORTED_NS.includes(n))) {
    throw new Failed("KOIDRIVE_BENCH_NS supports only " + SUPPORTED_NS.join(",") + " on this build; provide distinct comma-separated values");
  }
  const repetitions = process.env.KOIDRIVE_BENCH_REPETITIONS ?? "3";
  if (!/^[1-3]$/.test(repetitions)) throw new Failed("KOIDRIVE_BENCH_REPETITIONS must be an integer from 1 to 3");
  return { kind, env: { KOIDRIVE_BENCH_NS: ns, KOIDRIVE_BENCH_REPETITIONS: repetitions } };
}

function emit(markdown: string): void {
  console.log(markdown);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, markdown + "\n");
}

async function bench({ kind, env: settings }: Inputs): Promise<void> {
  let state: ReturnType<typeof readState>;
  try {
    state = readState();
  } catch {
    throw new Failed("could not read the private deploy-test state");
  }
  if (!state?.url || !state.owner) throw new Failed("benchmark requires a claimed deployment in the deploy-test state");
  const env: NodeJS.ProcessEnv = {};
  for (const key of ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "SSL_CERT_FILE", "SSL_CERT_DIR", "TMPDIR", "GITHUB_STEP_SUMMARY", "GITHUB_SHA", "RUNNER_ENVIRONMENT", "RUNNER_NAME", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_REGION", "AZURE_REGION"]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  env.KOIDRIVE_BENCH_URL = state.url;
  env.KOIDRIVE_BENCH_OWNER = JSON.stringify(state.owner);
  Object.assign(env, settings);
  const windows = join(process.env.RUNNER_TEMP || tmpdir(), "koidrive-bench-windows-" + process.pid + ".json");
  if (kind === "small") env.KOIDRIVE_BENCH_WINDOWS = windows;
  if (kind === "chunks" && !process.env.GITHUB_ACTIONS && process.env.KOIDRIVE_BENCH_MIB !== undefined) env.KOIDRIVE_BENCH_MIB = process.env.KOIDRIVE_BENCH_MIB;
  const test = kind === "small" ? "small_file_benchmark" : "upload_benchmark";
  await new Promise<void>((resolve, reject) => {
    const child = spawn("cargo", ["test", "-p", "koidrive-net", "--release", "--locked", "--test", "upload_bench", test, "--", "--ignored", "--exact", "--nocapture"], {
      cwd: fileURLToPath(new URL("../../../", import.meta.url)),
      env,
      stdio: ["ignore", "inherit", "inherit"],
    });
    child.on("error", () => reject(new Failed("could not start cargo")));
    child.on("close", (status) => {
      if (status === 0) resolve();
      else reject(new Failed(status === null ? "cargo terminated without an exit status" : "cargo exited with status " + status));
    });
  });
  if (kind === "small") await workerCpu(state.name, windows);
}

interface Window {
  side: string;
  startMs: number;
  endMs: number;
  requests: number;
}

interface Invocations {
  sum: { requests: number; errors: number };
  quantiles: { cpuTimeP50: number; cpuTimeP99: number };
}

const INVOCATIONS = `query($accountTag: string!, $scriptName: string!, $start: Time!, $end: Time!) {
  viewer { accounts(filter: { accountTag: $accountTag }) {
    workersInvocationsAdaptive(limit: 1, filter: { scriptName: $scriptName, datetime_geq: $start, datetime_leq: $end }) {
      sum { requests errors }
      quantiles { cpuTimeP50 cpuTimeP99 }
    }
  } }
}`;

async function invocations(account: Account, script: string, window: Window, deadline: Deadline): Promise<Invocations | undefined> {
  const data = await graphql<{ viewer: { accounts: { workersInvocationsAdaptive: Invocations[] }[] } }>(
    account,
    INVOCATIONS,
    {
      accountTag: account.id,
      scriptName: script,
      start: new Date(window.startMs).toISOString(),
      end: new Date(window.endMs).toISOString(),
    },
    deadline,
  );
  return data.viewer.accounts[0]?.workersInvocationsAdaptive[0];
}

function missingScope(error: unknown): boolean {
  return error instanceof ApiError && (error.status === 403 || /not authorized|unauthori[sz]ed|permission|access denied/i.test(error.message));
}

async function workerCpu(script: string, path: string): Promise<void> {
  const windows = JSON.parse(readFileSync(path, "utf8")) as Window[];
  emit("\n#### Worker CPU per request (Analytics API)\n");
  const account = accountFromEnv();
  if (!account) {
    emit("Worker CPU unavailable: CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID are not set. The sync times and request counts above are complete.");
    return;
  }
  const rows: string[] = [];
  let unavailable: string | undefined;
  const timing = (key: string, fallback: number) => (process.env.GITHUB_ACTIONS || process.env[key] === undefined ? fallback : Number(bounded(key, process.env[key], 1, fallback)));
  const budget = timing("KOIDRIVE_BENCH_ANALYTICS_MS", ANALYTICS_MS);
  const every = timing("KOIDRIVE_BENCH_ANALYTICS_EVERY_MS", ANALYTICS_EVERY_MS);
  const deadline: Deadline = { atMs: Date.now() + budget, what: "the Analytics API budget ran out" };
  const ms = (us: number | undefined) => (us === undefined ? "n/a" : (us / 1000).toFixed(2));
  const row = (window: Window, seen: Invocations | undefined) => {
    const n = seen?.sum.requests ?? 0;
    return "| " + window.side + " | " + window.requests + " | " + n + (n < window.requests ? " (partial)" : "") + " | " + ms(seen?.quantiles.cpuTimeP50) + " | " + ms(seen?.quantiles.cpuTimeP99) + " | " + (seen?.sum.errors ?? 0) + " |";
  };
  const stop = (window: Window, why: string) => "Worker CPU unavailable" + (rows.length > 0 ? " from the " + window.side + " side on" : "") + ": " + why + ". The sync times and request counts above are complete.";
  for (const [i, window] of windows.entries()) {
    let seen: Invocations | undefined;
    let failed: string | undefined;
    try {
      for (;;) {
        seen = (await invocations(account, script, window, { atMs: Math.max(deadline.atMs, Date.now() + LAST_QUERY_MS), what: deadline.what })) ?? seen;
        if ((seen?.sum.requests ?? 0) >= window.requests || Date.now() >= deadline.atMs) break;
        await new Promise((resolve) => setTimeout(resolve, Math.min(every, Math.max(0, deadline.atMs - Date.now()))));
      }
    } catch (error) {
      if (!(error instanceof DeadlineError)) failed = missingScope(error) ? "the deploy-test token lacks Account Analytics Read" : (error instanceof Error ? error.message : String(error)).slice(0, 200);
    }
    if (failed === undefined || seen) rows.push(row(window, seen));
    if (failed === undefined) continue;
    const missing = seen ? windows[i + 1] : window;
    unavailable = missing ? stop(missing, failed) : "Worker CPU for the " + window.side + " side is partial: " + failed + ". The sync times and request counts above are complete.";
    break;
  }
  if (unavailable) emit(unavailable);
  if (rows.length === 0) return;
  emit("| Side | Client upload requests | Worker invocations | CPU p50 ms | CPU p99 ms | Errors |\n|---|---|---|---|---|---|\n" + rows.join("\n"));
  emit("\nWorker invocations cover the side's whole timed window, so they include the session's WebSocket upgrade as well as the upload requests. On the bundles side, CPU per invocation is CPU per bundle.");
}

try {
  const args = process.argv.slice(2);
  const kind: Kind = args[0] === "small" ? "small" : "chunks";
  const rest = kind === "small" ? args.slice(1) : args;
  if (rest.length > 1 || (rest[0] !== undefined && rest[0] !== "validate")) throw new Failed("usage: bench.ts [small] [validate]");
  const config = inputs(kind);
  if (rest[0] !== "validate") await bench(config);
} catch (error) {
  console.error("::error::deploy benchmark: " + (error instanceof Failed ? error.message : "benchmark runner failed; private error details suppressed"));
  process.exitCode = 1;
}
