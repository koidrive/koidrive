// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { appendFileSync, chmodSync, copyFileSync, createWriteStream, existsSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { createGzip } from "node:zlib";
import { requireAccount, TELEMETRY_LIMIT, telemetryEvents, WORKER_KEYS } from "./cloudflare.ts";
import { download, invitation, keypair, peer, seal, unseal } from "./e2e.ts";
import { readState } from "./state.ts";
import {
  type Build,
  collectEvents,
  type Collected,
  countFields,
  countTimeouts,
  type Hold,
  holdDeadline,
  holdReason,
  HOLD_MINUTES,
  type Inputs,
  MINUTE_MS,
  type PlannedEntry,
  planTree,
  pool,
  progress,
  readInputs,
  RESET_MINUTES,
  type Run,
  type RunResult,
  runOrder,
  runTable,
  SAMPLE_MS,
  serverEvent,
  type Sample,
  SETUP_MINUTES,
  type Shape,
  shapeOf,
  type Status,
  summary,
  synced,
  TEARDOWN_MINUTES,
  uploadedEntries,
} from "./first-sync-model.ts";

const SEED = 1;
const DEFAULT_ENTRIES = 1_900_000;
const KEY_WAIT_MS = (SETUP_MINUTES.disk + SETUP_MINUTES.tree + 55) * MINUTE_MS;
const CODES_WAIT_MS = (SETUP_MINUTES.pairing - 5) * MINUTE_MS;
const DAEMON_START_MS = 60_000;
const DAEMON_STOP_MS = 60_000;
const STATUS_TIMEOUT_MS = 15_000;
const NODE_XATTR = "com.koidrive.node";
const BLOCK_BYTES = 4096;
const STORE_DISK_BYTES = 4 * 2 ** 30;
const GIB = 2 ** 30;
const TELEMETRY_LAG_MS = 3 * MINUTE_MS;
const TELEMETRY_MINUTES = 30;
const TELEMETRY_WINDOW_MS = 1000;
const BUILD_ENV = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "CARGO_HOME", "RUSTUP_HOME", "DEVELOPER_DIR", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "SSL_CERT_FILE", "SSL_CERT_DIR", "RUSTC_WRAPPER", "CARGO_INCREMENTAL"];

class Failed extends Error {}

function fail(message: string): never {
  throw new Failed(message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function work(...path: string[]): string {
  return join(process.env.FIRST_SYNC_WORK || join(process.env.RUNNER_TEMP || tmpdir(), "koidrive-first-sync"), ...path);
}

const repository = fileURLToPath(new URL("../../../", import.meta.url));

function entries(): number {
  const text = process.env.FIRST_SYNC_ENTRIES;
  if (text === undefined || process.env.GITHUB_ACTIONS) return DEFAULT_ENTRIES;
  return /^[1-9][0-9]*$/.test(text) ? Number(text) : fail("FIRST_SYNC_ENTRIES must be a whole number of at least 1, not " + JSON.stringify(text));
}

function inputs(): Inputs {
  try {
    return readInputs(process.env);
  } catch (error) {
    fail((error as Error).message);
  }
}

function git(args: string[], cwd = repository): string {
  return execFileSync("git", args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function resolve(ref: string): string {
  for (const candidate of ["refs/tags/" + ref, "refs/remotes/origin/" + ref, ref]) {
    try {
      return git(["rev-parse", "--verify", "--quiet", "--end-of-options", candidate + "^{commit}"]);
    } catch {}
  }
  fail("the ref " + JSON.stringify(ref) + " names no tag, branch or commit in this checkout");
}

function startedAt(): number {
  const seconds = Number(process.env.FIRST_SYNC_JOB_START ?? "");
  return seconds > 0 ? seconds * 1000 : fail("FIRST_SYNC_JOB_START names no start time");
}

function gib(bytes: number): string {
  return (bytes / GIB).toFixed(1) + " GiB";
}

function describe(shape: Shape): string {
  return shape.entries + " entries: " + shape.files + " files and " + shape.folders + " folders, " + shape.bytes + " bytes (" + gib(shape.bytes) + ") of file content, at most " + shape.maxDepth + " levels deep";
}

function readJson<T>(file: string, what: string): T {
  if (!existsSync(file)) fail(what + " is missing: " + file + " does not exist");
  return JSON.parse(readFileSync(file, "utf8")) as T;
}

interface BuiltKoi extends Build {
  index: number;
  koi: string;
}

function builds(): BuiltKoi[] {
  const { refs } = inputs();
  const dir = work("bin");
  const found = (existsSync(dir) ? readdirSync(dir) : []).map((name) => {
    const build = readJson<BuiltKoi>(join(dir, name, "build.json"), "the build record of " + name);
    const koi = join(dir, name, "koi");
    chmodSync(koi, 0o755);
    return { ...build, koi };
  });
  const ordered = refs.map((ref, index) => found.find((build) => build.index === index && build.ref === ref) ?? fail("no koi build for " + ref + " is under " + dir));
  if (found.length !== refs.length) fail(dir + " holds " + found.length + " koi builds for " + refs.length + " refs");
  return ordered;
}

const treeShape = () => readJson<Shape>(work("shape.json"), "the generated tree's shape");

interface Profile {
  name: string;
  dir: string;
  koi: string;
  env: Record<string, string>;
}

function profile(index: number, koi: string): Profile {
  const dir = work("p" + index);
  const env: Record<string, string> = {
    PATH: process.env.PATH ?? "/usr/bin:/bin",
    TMPDIR: process.env.TMPDIR ?? tmpdir(),
    HOME: join(dir, "home"),
    XDG_CONFIG_HOME: join(dir, "config"),
    XDG_STATE_HOME: join(dir, "state"),
    XDG_RUNTIME_DIR: join(dir, "run"),
    KOIDRIVE_UPDATE_CHECK: "0",
    KOIDRIVE_TELEMETRY: "0",
  };
  return { name: "first-sync-" + index, dir, koi, env };
}

function koi(on: Profile, args: string[], shown: string[] | null = args, input?: string): string {
  if (shown) console.log("\n$ koi " + shown.join(" ") + "   # " + on.name);
  try {
    const out = execFileSync(on.koi, args, { env: on.env, encoding: "utf8", input, stdio: [input === undefined ? "ignore" : "pipe", "pipe", "pipe"], timeout: STATUS_TIMEOUT_MS * 4 });
    if (shown) process.stdout.write(out);
    return out;
  } catch (error) {
    fail("koi " + (shown ?? args).join(" ") + " failed on " + on.name + ": " + String((error as { stderr?: unknown }).stderr ?? error).trim());
  }
}

interface FolderJson {
  space_id: string;
  state: string;
  head_seq: number | null;
  remote_nodes: number;
  local_entries: number | null;
  progress?: { files_to_upload: number; bytes_done: number };
}

function folderStatus(on: Profile): (Status & { spaceId: string; running: boolean }) | null {
  try {
    const out = execFileSync(on.koi, ["status", "--json"], { env: on.env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: STATUS_TIMEOUT_MS });
    const { data } = JSON.parse(out) as { data: { running: boolean; folders: FolderJson[] } };
    const folder = data.folders[0];
    if (!folder) return null;
    return {
      spaceId: folder.space_id,
      running: data.running,
      state: folder.state,
      remoteNodes: folder.remote_nodes,
      localEntries: folder.local_entries,
      headSeq: folder.head_seq ?? 0,
      filesToUpload: folder.progress?.files_to_upload ?? 0,
      bytesDone: folder.progress?.bytes_done ?? 0,
    };
  } catch {
    return null;
  }
}

function usage(pid: number): { cpuSeconds: number; rssBytes: number } | null {
  try {
    const [time = "", rss = ""] = execFileSync("ps", ["-o", "cputime=,rss=", "-p", String(pid)], { encoding: "utf8" }).trim().split(/\s+/);
    const cpuSeconds = time.split(/[:-]/).reduce((total, part) => total * 60 + Number(part), 0);
    return Number.isFinite(cpuSeconds) ? { cpuSeconds, rssBytes: Number(rss) * 1024 } : null;
  } catch {
    return null;
  }
}

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function resetTree(tree: string): void {
  rmSync(join(tree, ".koidrive"), { recursive: true, force: true });
  execFileSync("sh", ["-c", 'find "$1" -xattrname "$2" -print0 | xargs -0 -n 2000 xattr -d "$2"', "sh", tree, NODE_XATTR], { stdio: "inherit" });
  const left = execFileSync("find", [tree, "-xattrname", NODE_XATTR, "-print", "-quit"], { encoding: "utf8" }).trim();
  if (left) fail("the tree still holds Koidrive's mark " + NODE_XATTR + " after the reset, for example on " + left);
  if (existsSync(join(tree, ".koidrive"))) fail("the tree still holds .koidrive after the reset");
}

function storeBytes(on: Profile, spaceId: string): number {
  const dir = join(on.env.XDG_STATE_HOME!, "koidrive", "spaces", spaceId);
  return ["store.sqlite", "store.sqlite-wal"].reduce((total, name) => total + (existsSync(join(dir, name)) ? statSync(join(dir, name)).size : 0), 0);
}

function failedRun(run: Run, build: BuiltKoi, shape: Shape, capMs: number, failure: string): RunResult {
  return {
    index: run.index,
    ref: build.ref,
    sha: build.sha,
    rep: run.rep,
    order: run.order,
    entries: shape.entries,
    ended: "failed",
    failure,
    capSeconds: capMs / 1000,
    elapsedSeconds: 0,
    linkSeconds: 0,
    syncedEntries: 0,
    firstUploadSeconds: null,
    syncedSeconds: null,
    stallSeconds: 0,
    stalls: 0,
    longestStallSeconds: 0,
    timeouts: { didNotMove: 0, noCompleteAnswer: 0, keptFailing: 0 },
    cpuSeconds: null,
    peakRssBytes: null,
    storeBytes: 0,
    storeBytesPerEntry: null,
    samples: 0,
  };
}

async function measureRun(run: Run, build: BuiltKoi, shape: Shape, capMs: number, label: string): Promise<RunResult> {
  const on = profile(run.ref, build.koi);
  const tree = work("tree");
  const resetAt = Date.now();
  resetTree(tree);
  console.log("Reset the tree in " + ((Date.now() - resetAt) / 1000).toFixed(1) + " s");
  const space = "first-sync-" + (run.index + 1) + "-" + Date.now().toString(36);
  koi(on, ["space", "create", space]);
  const linkAt = Date.now();
  koi(on, ["folder", "link", tree, "--space", space]);
  const linkSeconds = (Date.now() - linkAt) / 1000;
  const before = folderStatus(on) ?? fail("koi status reports no linked folder on " + on.name + " after the link");
  const log = join(work("results"), label + ".daemon.log");
  console.log("\n$ koi _daemon >> " + log + " &   # " + on.name);
  const output = openSync(log, "a");
  const daemon = spawn(on.koi, ["_daemon"], { env: { ...on.env, KOIDRIVE_LOG: "info" }, detached: true, stdio: ["ignore", output, output] });
  daemon.unref();
  const started = Date.now();
  const pid = daemon.pid ?? fail("the daemon of " + on.name + " did not start");
  const samples: Sample[] = [{ seconds: 0, status: before, cpuSeconds: null, rssBytes: null }];
  let ended: RunResult["ended"] = "capped";
  let failure: string | null = null;
  let store = 0;
  try {
    while (!folderStatus(on)?.running) {
      if (!alive(pid)) fail(on.name + "'s daemon exited before it answered: " + readFileSync(log, "utf8").slice(-2000));
      if (Date.now() - started > DAEMON_START_MS) fail(on.name + "'s daemon did not answer within " + DAEMON_START_MS / 1000 + " s");
      await sleep(1000);
    }
    for (let tick = 1; ; tick++) {
      const status = folderStatus(on);
      const used = usage(pid);
      const seconds = (Date.now() - started) / 1000;
      samples.push({ seconds, status, cpuSeconds: used?.cpuSeconds ?? null, rssBytes: used?.rssBytes ?? null });
      if (synced(status, shape.entries)) {
        ended = "synced";
        break;
      }
      if (!used) {
        ended = "failed";
        failure = "the daemon exited after " + Math.round(seconds) + " s";
        break;
      }
      if (Date.now() - started >= capMs) break;
      if (tick % 30 === 0) console.log(Math.round(seconds) + " s: " + JSON.stringify(status));
      await sleep(Math.max(0, started + tick * SAMPLE_MS - Date.now()));
    }
  } finally {
    store = storeBytes(on, before.spaceId);
    if (alive(pid)) {
      process.kill(pid, "SIGTERM");
      const deadline = Date.now() + DAEMON_STOP_MS;
      while (alive(pid) && Date.now() < deadline) await sleep(500);
      if (alive(pid)) {
        console.log("::warning::" + on.name + "'s daemon did not exit within " + DAEMON_STOP_MS / 1000 + " s of SIGTERM, so the run killed it");
        process.kill(pid, "SIGKILL");
      }
    }
    rmSync(join(on.env.XDG_STATE_HOME!, "koidrive", "spaces", before.spaceId), { recursive: true, force: true });
    rmSync(join(on.env.XDG_RUNTIME_DIR!, "koidrive"), { recursive: true, force: true });
  }
  writeFileSync(join(work("results"), label + ".samples.json"), JSON.stringify(samples));
  const last = samples.findLast((sample) => sample.status !== null)?.status;
  const measured = samples.filter((sample) => sample.cpuSeconds !== null);
  const syncedEntries = last ? uploadedEntries(last) : 0;
  return {
    index: run.index,
    ref: build.ref,
    sha: build.sha,
    rep: run.rep,
    order: run.order,
    entries: shape.entries,
    ended,
    failure,
    capSeconds: capMs / 1000,
    elapsedSeconds: samples.at(-1)!.seconds,
    linkSeconds,
    syncedEntries,
    ...progress(samples, shape.entries),
    timeouts: countTimeouts(readFileSync(log, "utf8")),
    cpuSeconds: measured.at(-1)?.cpuSeconds ?? null,
    peakRssBytes: measured.length ? Math.max(...measured.map((sample) => sample.rssBytes!)) : null,
    storeBytes: store,
    storeBytesPerEntry: syncedEntries ? store / syncedEntries : null,
    samples: samples.length,
  };
}

function* writing(root: string, planned: Iterable<PlannedEntry>): Generator<PlannedEntry> {
  const bytes = pool(SEED);
  let written = 0;
  for (const entry of planned) {
    const path = join(root, entry.path);
    if (entry.size === null) mkdirSync(path);
    else writeFileSync(path, bytes.subarray(entry.offset, entry.offset + entry.size));
    if (++written % 100_000 === 0) console.log("Wrote " + written + " entries");
    yield entry;
  }
}

export async function sealCodes(): Promise<void> {
  const { refs } = inputs();
  const publicKey = readFileSync(work("key", "public.pem"), "utf8");
  mkdirSync(work("join"), { recursive: true });
  for (const index of refs.keys()) writeFileSync(work("join", "join-" + index + ".bin"), seal(publicKey, await invitation()));
  writeFileSync(work("join", "server"), readState().url ?? fail("the deploy-test state names no server"));
  const syncStart = Number(readFileSync(work("key", "started-at"), "utf8"));
  if (!(syncStart > 0)) fail("the sync job's key names no start time");
  const hold = holdDeadline(syncStart, startedAt());
  writeFileSync(work("join", "hold.json"), JSON.stringify(hold));
  console.log("The Worker stays until the sync job ends, at the latest until " + new Date(hold.until).toISOString() + ", " + holdReason(hold));
  console.log("Sealed " + refs.length + " pairing codes, one for each ref's device, to the macOS job's public key");
}

function joinProfiles(): void {
  const privateKey = readFileSync(work("private.pem"), "utf8");
  const server = readFileSync(work("join", "server"), "utf8");
  for (const build of builds()) {
    const on = profile(build.index, build.koi);
    rmSync(on.dir, { recursive: true, force: true });
    for (const name of ["HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"]) mkdirSync(on.env[name]!, { recursive: true, mode: 0o700 });
    mkdirSync(join(on.env.XDG_CONFIG_HOME!, "koidrive"), { recursive: true, mode: 0o700 });
    writeFileSync(join(on.env.XDG_CONFIG_HOME!, "koidrive", "config.toml"), "[service]\nbackground = false\n");
    const code = unseal(privateKey, readFileSync(work("join", "join-" + build.index + ".bin")));
    const args = ["join", "--code-stdin", "--server", server, "--name", on.name];
    koi(on, args, args, code + "\n");
  }
  copyFileSync(work("join", "hold.json"), work("hold.json"));
  rmSync(work("join"), { recursive: true });
}

export function buildEnv(source: NodeJS.ProcessEnv, targetDir: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (const name of BUILD_ENV) if (source[name] !== undefined) env[name] = source[name];
  env.CARGO_TARGET_DIR = targetDir;
  return env;
}

function writeSummary(text: string): void {
  mkdirSync(work("results"), { recursive: true });
  writeFileSync(work("results", "summary.md"), text);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, text);
  console.log(text);
}

const phases: Record<string, () => Promise<void>> = {
  async validate() {
    const { refs, repetitions, rotation, minutes } = inputs();
    const shas = refs.map(resolve);
    refs.forEach((ref, i) => console.log(ref + " is " + shas[i]));
    const order = runOrder(refs.length, repetitions, rotation).map((run) => refs[run.ref]);
    console.log(order.length + " runs of up to " + minutes + " minutes, in this order: " + order.join(", "));
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, "builds=" + JSON.stringify(refs.map((ref, index) => ({ index, ref, sha: shas[index] }))) + "\n");
  },

  async build() {
    const [source = "", out = ""] = process.argv.slice(3);
    if (!source || !out) fail("usage: first-sync.ts build <checkout of the ref> <output directory>");
    const index = Number(process.env.FIRST_SYNC_INDEX ?? fail("FIRST_SYNC_INDEX is not set"));
    const ref = process.env.FIRST_SYNC_REF ?? fail("FIRST_SYNC_REF is not set");
    const sha = process.env.FIRST_SYNC_SHA ?? fail("FIRST_SYNC_SHA is not set");
    const head = git(["rev-parse", "HEAD"], source);
    if (head !== sha) fail(source + " holds " + head + ", not " + sha + " that " + ref + " resolved to");
    const target = work("target", String(index));
    const env = buildEnv(process.env, target);
    console.log("## Build koi from " + ref + " (" + sha + ") with only " + Object.keys(env).sort().join(", ") + " set");
    execFileSync("sh", ["-c", "rustup show active-toolchain || rustup toolchain install"], { cwd: source, env, stdio: "inherit" });
    const at = Date.now();
    execFileSync("cargo", ["build", "--release", "--locked", "-p", "koidrive-cli", "--bin", "koi"], { cwd: source, env, stdio: "inherit" });
    console.log("Built in " + Math.round((Date.now() - at) / 1000) + " s");
    mkdirSync(out, { recursive: true });
    copyFileSync(join(target, "release", "koi"), join(out, "koi"));
    writeFileSync(join(out, "build.json"), JSON.stringify({ index, ref, sha }));
    rmSync(target, { recursive: true, force: true });
  },

  async disk() {
    const shape = shapeOf(planTree(SEED, entries()));
    const need = shape.entries * BLOCK_BYTES + shape.bytes + STORE_DISK_BYTES;
    mkdirSync(work(), { recursive: true });
    const free = Number(execFileSync("df", ["-k", work()], { encoding: "utf8" }).trim().split("\n").at(-1)!.split(/\s+/)[3]) * 1024;
    console.log("The tree will hold " + describe(shape) + ".");
    console.log("Free: " + gib(free) + ". Needed: " + gib(need) + " for the tree in " + BLOCK_BYTES + "-byte blocks and the sync store.");
    if (!(free >= need)) fail("only " + gib(free) + " is free under " + work() + ", but the tree and the sync store need " + gib(need) + ". Free more space in the step before.");
  },

  async generate() {
    const root = work("tree");
    rmSync(root, { recursive: true, force: true });
    mkdirSync(root, { recursive: true });
    const at = Date.now();
    const shape = shapeOf(writing(root, planTree(SEED, entries())));
    writeFileSync(work("shape.json"), JSON.stringify(shape));
    console.log("Generated " + describe(shape) + " in " + Math.round((Date.now() - at) / 1000) + " s.");
  },

  async keygen() {
    const { publicKey, privateKey } = keypair();
    mkdirSync(work("key"), { recursive: true });
    writeFileSync(work("private.pem"), privateKey, { mode: 0o600 });
    writeFileSync(work("key", "public.pem"), publicKey);
    writeFileSync(work("key", "started-at"), String(startedAt()));
    console.log("Made the key the deploy job seals the pairing codes to; only its public half is uploaded");
  },

  async seal() {
    await download("first-sync-macos-key", work("key"), KEY_WAIT_MS);
    await sealCodes();
  },

  async "fetch-codes"() {
    await download("first-sync-macos-join", work("join"), CODES_WAIT_MS);
  },

  async join() {
    joinProfiles();
  },

  async measure() {
    const { repetitions, rotation, minutes } = inputs();
    const built = builds();
    const shape = treeShape();
    const deadline = readJson<Hold>(work("hold.json"), "the Worker's hold deadline").until;
    mkdirSync(work("results"), { recursive: true });
    const runs = runOrder(built.length, repetitions, rotation);
    const failures: string[] = [];
    for (const run of runs) {
      if (Date.now() + (minutes + RESET_MINUTES) * MINUTE_MS > deadline) {
        failures.push("runs " + (run.index + 1) + " to " + runs.length + " did not start, because they could not finish before " + new Date(deadline).toISOString());
        break;
      }
      const build = built[run.ref]!;
      const label = "run-" + String(run.index + 1).padStart(2, "0");
      console.log("\n## Run " + (run.index + 1) + ": " + build.ref + ", repetition " + (run.rep + 1) + ", position " + (run.order + 1));
      let result: RunResult;
      try {
        result = await measureRun(run, build, shape, minutes * MINUTE_MS, label);
      } catch (error) {
        result = failedRun(run, build, shape, minutes * MINUTE_MS, error instanceof Error ? error.message : String(error));
      }
      if (result.ended === "failed") {
        failures.push("run " + (run.index + 1) + " (" + build.ref + "): " + result.failure);
        console.log("::error::Run " + (run.index + 1) + " (" + build.ref + ") failed: " + result.failure);
      }
      writeFileSync(work("results", label + ".json"), JSON.stringify(result, null, 2));
      console.log(JSON.stringify(result));
    }
    if (failures.length) fail("not every one of the " + runs.length + " runs synced: " + failures.join("; "));
  },

  async summarize() {
    const { repetitions } = inputs();
    const built = builds();
    const shape = treeShape();
    const dir = work("results");
    const results = (existsSync(dir) ? readdirSync(dir) : []).filter((name) => /^run-\d+\.json$/.test(name)).map((name) => readJson<RunResult>(join(dir, name), name));
    const head =
      "### First sync of " + shape.entries + " entries\n\n" +
      "The tree holds " + describe(shape) + ". Each cell gives the median over the runs that did not fail and, in parentheses, the lowest and highest value. ≥ marks a run stopped at its time cap before it was fully synced.\n\n";
    writeSummary(head + summary(built, results, built.length * repetitions) + "\n#### Runs\n\n" + runTable(results));
  },

  async hold() {
    const hold = readJson<Hold>(work("join", "hold.json"), "the Worker's hold deadline");
    const deadline = hold.until;
    for (;;) {
      if (Date.now() >= deadline) {
        console.log("::warning::The job " + process.env.E2E_PEER_JOB + " still runs at " + new Date(deadline).toISOString() + ", " + holdReason(hold) + ", so the Worker goes now");
        return;
      }
      try {
        const job = await peer();
        if (job?.status === "completed") {
          console.log("The job " + job.name + " ended (" + job.conclusion + "), so the Worker can go");
          return;
        }
      } catch (error) {
        console.log("::warning::Could not read the state of the job " + process.env.E2E_PEER_JOB + ": " + (error instanceof Error ? error.message : String(error)));
      }
      await sleep(Math.min(60_000, Math.max(0, deadline - Date.now())));
    }
  },

  async telemetry() {
    const account = requireAccount();
    const state = readState();
    const fromMs = state.deployments[0]?.atMs ?? fail("the state file records no deployment");
    const key = WORKER_KEYS.find((candidate) => candidate === state.logKey) ?? fail("the state file records no Workers Logs filter; run.ts claim records it");
    const stopAtMs = Math.min(startedAt() + HOLD_MINUTES * MINUTE_MS, Date.now() + TELEMETRY_MINUTES * MINUTE_MS);
    const lagMs = Math.max(0, Math.min(TELEMETRY_LAG_MS, (stopAtMs - Date.now()) / 2));
    console.log("Waiting " + Math.round(lagMs / 1000) + " s for Workers Logs to take in the last requests");
    await sleep(lagMs);
    const toMs = Date.now();
    const dir = work("telemetry");
    mkdirSync(dir, { recursive: true });
    const gzip = createGzip();
    const written = pipeline(gzip, createWriteStream(join(dir, "server-events.ndjson.gz")));
    const types = new Map<string, number>();
    const spanNames = new Map<string, number>();
    const fields = new Map<string, number>();
    const count = (counts: Map<string, number>, name: string) => counts.set(name, (counts.get(name) ?? 0) + 1);
    const deadline = { atMs: stopAtMs, what: "the telemetry step reached " + new Date(stopAtMs).toISOString() + ", which keeps the teardown's " + TEARDOWN_MINUTES + " minutes" };
    let collected: Collected;
    try {
      collected = await collectEvents(
        (from, to) => telemetryEvents(account, key, state.name, from, to, deadline),
        fromMs,
        toMs,
        (event) => {
          const kept = serverEvent(event);
          gzip.write(JSON.stringify(kept) + "\n");
          count(types, String(kept.$metadata.type ?? "none"));
          if (kept.$metadata.spanName !== undefined) count(spanNames, String(kept.$metadata.spanName));
          countFields(event, fields);
        },
        { limit: TELEMETRY_LIMIT, minWindowMs: TELEMETRY_WINDOW_MS, stopAtMs },
      );
    } finally {
      gzip.end();
      await written;
    }
    const sorted = (counts: Map<string, number>) => Object.fromEntries([...counts].sort((a, b) => b[1] - a[1]));
    const meta = { worker: state.name, filter: key, traceSampling: state.traceSampling ?? null, fromMs, toMs, ...collected, types: sorted(types), spanNames: sorted(spanNames), fields: sorted(fields) };
    writeFileSync(join(dir, "server-events.json"), JSON.stringify(meta, null, 2));
    console.log("Wrote " + collected.events + " events from " + collected.queries + " queries between " + new Date(fromMs).toISOString() + " and " + new Date(toMs).toISOString() + ". Event types: " + JSON.stringify(meta.types));
    if (collected.failure) fail("querying Workers Logs failed: " + collected.failure);
    if (collected.uncoveredFromMs !== null) console.log("::warning::Stopped at " + new Date(stopAtMs).toISOString() + " before the events after " + new Date(collected.uncoveredFromMs).toISOString() + ", so the teardown keeps its " + TEARDOWN_MINUTES + " minutes");
    if (collected.saturated.length) console.log("::warning::" + collected.saturated.length + " windows of " + TELEMETRY_WINDOW_MS + " ms or less held " + TELEMETRY_LIMIT + " events or more, so some of their events are missing");
  },
};

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [phase = ""] = process.argv.slice(2);
  try {
    await (phases[phase] ?? fail("usage: first-sync.ts " + Object.keys(phases).join(" | ")))();
  } catch (error) {
    console.error("::error::first-sync " + phase + ": " + (error instanceof Error ? error.message : String(error)));
    process.exitCode = 1;
  }
}
