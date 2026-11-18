// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { appendFileSync, copyFileSync, existsSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { download, invitation, keypair, peer, seal, unseal, until } from "./e2e.ts";
import {
  type Build,
  countTimeouts,
  type Inputs,
  type PlannedEntry,
  planTree,
  pool,
  progress,
  readInputs,
  RUN_OVERHEAD_MINUTES,
  type Run,
  type RunResult,
  runOrder,
  runTable,
  SAMPLE_MS,
  type Sample,
  type Shape,
  shapeOf,
  type Status,
  summary,
  synced,
  uploadedEntries,
} from "./first-sync-model.ts";

const SEED = 1;
const DEFAULT_ENTRIES = 1_900_000;
const KEY_WAIT_MS = 200 * 60_000;
const JOIN_WAIT_MS = 120 * 60_000;
const HOLD_MS = 350 * 60_000;
const JOB_MS = 345 * 60_000;
const DAEMON_START_MS = 60_000;
const DAEMON_STOP_MS = 60_000;
const STATUS_TIMEOUT_MS = 15_000;
const NODE_XATTR = "com.koidrive.node";
const BLOCK_BYTES = 4096;
const BUILD_DISK_BYTES = 4 * 2 ** 30;
const STORE_DISK_BYTES = 4 * 2 ** 30;
const GIB = 2 ** 30;

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
  koi: string;
}

const builds = () => readJson<BuiltKoi[]>(work("builds.json"), "the list of koi builds");
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

function koi(on: Profile, args: string[], shown: string[] | null = args): string {
  if (shown) console.log("\n$ koi " + shown.join(" ") + "   # " + on.name);
  try {
    const out = execFileSync(on.koi, args, { env: on.env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: STATUS_TIMEOUT_MS * 4 });
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

async function measureRun(run: Run, build: BuiltKoi, shape: Shape, capMs: number): Promise<RunResult> {
  const on = profile(run.ref, build.koi);
  const tree = work("tree");
  const label = "run-" + String(run.index + 1).padStart(2, "0");
  console.log("\n## Run " + (run.index + 1) + ": " + build.ref + ", repetition " + (run.rep + 1) + ", position " + (run.order + 1));
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
  let store = 0;
  try {
    await until(on.name + "'s daemon to answer", DAEMON_START_MS, () => {
      if (!alive(pid)) fail(on.name + "'s daemon exited: " + readFileSync(log, "utf8").slice(-2000));
      return folderStatus(on)?.running ? true : undefined;
    });
    for (let tick = 1; ; tick++) {
      const status = folderStatus(on);
      const used = usage(pid);
      samples.push({ seconds: (Date.now() - started) / 1000, status, cpuSeconds: used?.cpuSeconds ?? null, rssBytes: used?.rssBytes ?? null });
      if (synced(status, shape.entries)) {
        ended = "synced";
        break;
      }
      if (!used) {
        ended = "daemon exited";
        break;
      }
      if (Date.now() - started >= capMs) break;
      if (tick % 30 === 0) console.log(Math.round((Date.now() - started) / 1000) + " s: " + JSON.stringify(status));
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
  console.log("Sealed " + refs.length + " pairing codes, one for each ref's device, to the macOS job's public key");
}

export function joinProfiles(): void {
  const privateKey = readFileSync(work("private.pem"), "utf8");
  for (const [index, build] of builds().entries()) {
    const on = profile(index, build.koi);
    rmSync(on.dir, { recursive: true, force: true });
    for (const name of ["HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"]) mkdirSync(on.env[name]!, { recursive: true, mode: 0o700 });
    mkdirSync(join(on.env.XDG_CONFIG_HOME!, "koidrive"), { recursive: true, mode: 0o700 });
    writeFileSync(join(on.env.XDG_CONFIG_HOME!, "koidrive", "config.toml"), "[service]\nbackground = false\n");
    const code = unseal(privateKey, readFileSync(work("join", "join-" + index + ".bin")));
    koi(on, ["join", code, "--name", on.name], ["join", "<pairing code>", "--name", on.name]);
  }
  rmSync(work("join"), { recursive: true });
}

function writeSummary(text: string): void {
  mkdirSync(work("results"), { recursive: true });
  writeFileSync(work("results", "summary.md"), text);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, text);
  console.log(text);
}

const phases: Record<string, () => Promise<void>> = {
  async validate() {
    const { refs, repetitions, minutes } = inputs();
    const shas = refs.map(resolve);
    refs.forEach((ref, i) => console.log(ref + " is " + shas[i]));
    const order = runOrder(refs.length, repetitions).map((run) => refs[run.ref]);
    console.log(order.length + " runs of up to " + minutes + " minutes, in this order: " + order.join(", "));
  },

  async disk() {
    const { refs } = inputs();
    const shape = shapeOf(planTree(SEED, entries()));
    const need = shape.entries * BLOCK_BYTES + shape.bytes + BUILD_DISK_BYTES + STORE_DISK_BYTES;
    mkdirSync(work(), { recursive: true });
    const free = Number(execFileSync("df", ["-k", work()], { encoding: "utf8" }).trim().split("\n").at(-1)!.split(/\s+/)[3]) * 1024;
    console.log("The tree will hold " + describe(shape) + ".");
    console.log("Free: " + gib(free) + ". Needed: " + gib(need) + " for the tree in " + BLOCK_BYTES + "-byte blocks, one koi build at a time for " + refs.length + " refs, and the sync store.");
    if (!(free >= need)) fail("only " + gib(free) + " is free under " + work() + ", but the tree, a koi build and the sync store need " + gib(need) + ". Free more space in the step before.");
  },

  async build() {
    const { refs } = inputs();
    const built: BuiltKoi[] = [];
    for (const [index, ref] of refs.entries()) {
      const sha = resolve(ref);
      const src = work("src", String(index));
      const target = work("target", String(index));
      const koiPath = work("bin", String(index), "koi");
      console.log("\n## Build koi from " + ref + " (" + sha + ")");
      rmSync(src, { recursive: true, force: true });
      git(["worktree", "prune"]);
      git(["worktree", "add", "--detach", "--force", src, sha]);
      try {
        execFileSync("sh", ["-c", "rustup show active-toolchain || rustup toolchain install"], { cwd: src, stdio: "inherit" });
        const at = Date.now();
        execFileSync("cargo", ["build", "--release", "--locked", "-p", "koidrive-cli", "--bin", "koi"], { cwd: src, env: { ...process.env, CARGO_TARGET_DIR: target }, stdio: "inherit" });
        console.log("Built in " + Math.round((Date.now() - at) / 1000) + " s");
        mkdirSync(work("bin", String(index)), { recursive: true });
        copyFileSync(join(target, "release", "koi"), koiPath);
        execFileSync("chmod", ["755", koiPath]);
      } finally {
        rmSync(target, { recursive: true, force: true });
        git(["worktree", "remove", "--force", src]);
      }
      console.log(execFileSync(koiPath, ["--version"], { encoding: "utf8" }).trim());
      built.push({ ref, sha, koi: koiPath });
    }
    writeFileSync(work("builds.json"), JSON.stringify(built, null, 2));
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
    console.log("Made the key the deploy job seals the pairing codes to; only its public half is uploaded");
  },

  async seal() {
    await download("first-sync-macos-key", work("key"), KEY_WAIT_MS);
    await sealCodes();
  },

  async join() {
    await download("first-sync-macos-join", work("join"), JOIN_WAIT_MS);
    joinProfiles();
  },

  async measure() {
    const { repetitions, minutes } = inputs();
    const built = builds();
    const shape = treeShape();
    const start = Number(process.env.FIRST_SYNC_JOB_START ?? "");
    const deadline = start > 0 ? start * 1000 + JOB_MS : Infinity;
    mkdirSync(work("results"), { recursive: true });
    const runs = runOrder(built.length, repetitions);
    for (const run of runs) {
      if (Date.now() + (minutes + RUN_OVERHEAD_MINUTES) * 60_000 > deadline) {
        console.log("::warning::The job has too little time left for run " + (run.index + 1) + " of " + runs.length + ", so it stops here");
        return;
      }
      const result = await measureRun(run, built[run.ref]!, shape, minutes * 60_000);
      writeFileSync(work("results", "run-" + String(run.index + 1).padStart(2, "0") + ".json"), JSON.stringify(result, null, 2));
      console.log(JSON.stringify(result));
    }
  },

  async summarize() {
    const { repetitions } = inputs();
    const built = builds();
    const shape = treeShape();
    const dir = work("results");
    const results = (existsSync(dir) ? readdirSync(dir) : []).filter((name) => /^run-\d+\.json$/.test(name)).map((name) => readJson<RunResult>(join(dir, name), name));
    const head =
      "### First sync of " + shape.entries + " entries\n\n" +
      "The tree holds " + describe(shape) + ". Each cell gives the median over the repetitions and, in parentheses, the lowest and highest value. ≥ marks a run stopped at its time cap before it was fully synced.\n\n";
    writeSummary(head + summary(built, results, built.length * repetitions) + "\n#### Runs\n\n" + runTable(results));
  },

  async hold() {
    const job = await until(
      "the job " + process.env.E2E_PEER_JOB + " to end",
      HOLD_MS,
      async () => {
        const found = await peer();
        return found?.status === "completed" ? found : undefined;
      },
      60_000,
    );
    console.log("The job " + job.name + " ended (" + job.conclusion + "), so the Worker can go");
  },
};

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [phase = ""] = process.argv.slice(2);
  try {
    await (phases[phase] ?? fail("usage: first-sync.ts " + Object.keys(phases).join(" | ")))();
  } catch (error) {
    console.error("::error::first-sync " + phase + ": " + (error instanceof Failed ? error.message : String(error)));
    process.exitCode = 1;
  }
}
