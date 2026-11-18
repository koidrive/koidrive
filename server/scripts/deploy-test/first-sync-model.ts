// SPDX-License-Identifier: AGPL-3.0-or-later

export const MAX_REFS = 4;
export const MAX_REPETITIONS = 4;
export const MAX_MINUTES = 240;
export const JOB_MINUTES = 360;
export const RESET_MINUTES = 5;
export const SETUP_MINUTES = { disk: 15, tree: 20, pairing: 40, summary: 10, spare: 5 } as const;
export const RUNS_END_MINUTES = JOB_MINUTES - SETUP_MINUTES.summary - SETUP_MINUTES.spare;
export const TEARDOWN_MINUTES = 15;
export const HOLD_MINUTES = JOB_MINUTES - TEARDOWN_MINUTES;
export const MINUTE_MS = 60_000;
export const SAMPLE_MS = 2000;
export const STALL_SECONDS = 5;
export const TIMEOUT_MESSAGES = { didNotMove: "did not move for", noCompleteAnswer: "no complete answer within", keptFailing: "kept failing" } as const;
export type TimeoutKind = keyof typeof TIMEOUT_MESSAGES;

const REF = /^(?!.*\.\.)(?!.*\/\/)(?!.*\.lock$)(?!.*[./]$)[A-Za-z0-9_][A-Za-z0-9._/-]*$/;

export interface Hold {
  until: number;
  cap: "sync" | "deploy";
}

export function holdDeadline(syncStart: number, deployStart: number): Hold {
  const sync = syncStart + RUNS_END_MINUTES * MINUTE_MS;
  const deploy = deployStart + HOLD_MINUTES * MINUTE_MS;
  return deploy < sync ? { until: deploy, cap: "deploy" } : { until: sync, cap: "sync" };
}

export function holdReason(hold: Hold): string {
  return hold.cap === "sync"
    ? RUNS_END_MINUTES + " minutes after the sync job started, after which it starts no run"
    : HOLD_MINUTES + " minutes after the deploy job started, because GitHub ends a job after " + JOB_MINUTES + " minutes and the teardown needs " + TEARDOWN_MINUTES;
}

export interface Inputs {
  refs: string[];
  repetitions: number;
  rotation: number;
  minutes: number;
}

export function readInputs(env: Record<string, string | undefined>): Inputs {
  const refs = (env.FIRST_SYNC_REFS ?? "").split(",");
  if (refs.length > MAX_REFS || new Set(refs).size !== refs.length || refs.some((ref) => !REF.test(ref))) {
    throw new Error("FIRST_SYNC_REFS must name 1 to " + MAX_REFS + " distinct branches, tags or commits separated by commas, not " + JSON.stringify(env.FIRST_SYNC_REFS ?? ""));
  }
  const repetitions = env.FIRST_SYNC_REPETITIONS ?? "1";
  if (!new RegExp("^[1-" + MAX_REPETITIONS + "]$").test(repetitions)) throw new Error("FIRST_SYNC_REPETITIONS must be a whole number from 1 to " + MAX_REPETITIONS + ", not " + JSON.stringify(repetitions));
  if (repetitions !== "1" && Number(repetitions) % refs.length) {
    throw new Error("FIRST_SYNC_REPETITIONS must be 1 or a multiple of the " + refs.length + " refs, so that each ref runs as often in each position, not " + repetitions);
  }
  const rotation = env.FIRST_SYNC_ROTATION ?? "0";
  if (!/^(0|[1-9][0-9]?)$/.test(rotation) || Number(rotation) >= refs.length) {
    throw new Error("FIRST_SYNC_ROTATION must be a whole number from 0 to " + (refs.length - 1) + ", the ref that runs first, not " + JSON.stringify(rotation));
  }
  const minutes = env.FIRST_SYNC_MINUTES ?? "80";
  if (!/^[1-9][0-9]{0,2}$/.test(minutes) || Number(minutes) > MAX_MINUTES) throw new Error("FIRST_SYNC_MINUTES must be a whole number from 1 to " + MAX_MINUTES + ", not " + JSON.stringify(minutes));
  const inputs = { refs, repetitions: Number(repetitions), rotation: Number(rotation), minutes: Number(minutes) };
  const runs = refs.length * inputs.repetitions;
  const setup = Object.values(SETUP_MINUTES).reduce((total, value) => total + value, 0);
  const needed = setup + runs * (inputs.minutes + RESET_MINUTES);
  if (needed > JOB_MINUTES) {
    const fit = Math.floor((JOB_MINUTES - setup) / runs) - RESET_MINUTES;
    throw new Error(
      runs + " runs of up to " + inputs.minutes + " minutes and " + RESET_MINUTES + " minutes each to reset the tree, with " + SETUP_MINUTES.disk + " minutes to free disk, " + SETUP_MINUTES.tree + " to generate the tree, " +
        SETUP_MINUTES.pairing + " to wait for the approval and the pairing codes, " + SETUP_MINUTES.summary + " for the summary and " + SETUP_MINUTES.spare + " to spare, take " + needed + " minutes, more than the macOS job's " + JOB_MINUTES +
        ". Lower FIRST_SYNC_MINUTES to " + fit + " or run fewer refs or repetitions.",
    );
  }
  return inputs;
}

export interface Run {
  index: number;
  ref: number;
  rep: number;
  order: number;
}

export function runOrder(refs: number, repetitions: number, rotation: number): Run[] {
  const runs: Run[] = [];
  for (let rep = 0; rep < repetitions; rep++) {
    for (let order = 0; order < refs; order++) runs.push({ index: runs.length, ref: (rotation + rep + order) % refs, rep, order });
  }
  return runs;
}

export interface PlannedEntry {
  path: string;
  size: number | null;
  offset: number;
}

export interface Shape {
  entries: number;
  files: number;
  folders: number;
  bytes: number;
  maxDepth: number;
}

export const POOL_BYTES = 64 << 20;

export function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function pool(seed: number): Buffer {
  const next = random(seed ^ 0x5eed);
  const words = new Uint32Array(POOL_BYTES / 4);
  for (let i = 0; i < words.length; i++) words[i] = next() * 4294967296;
  return Buffer.from(words.buffer);
}

const SOURCE_NAMES = ["index", "util", "types", "client", "server", "config", "helpers", "constants", "parser", "render"];
const SOURCE_EXTENSIONS = [".js", ".ts", ".mjs", ".cjs", ".d.ts", ".json", ".map", ".md"];
const FOLDER_NAMES = ["lib", "dist", "src", "esm", "cjs", "types", "internal", "utils", "core", "components", "test", "build"];
const MAX_FOLDER_DEPTH = 8;

export function* planTree(seed: number, target: number): Generator<PlannedEntry> {
  const next = random(seed);
  const pick = <T>(list: readonly T[]): T => list[Math.floor(next() * list.length)]!;
  const between = (low: number, high: number) => low + Math.floor(next() * (high - low + 1));
  let made = 0;
  const folder = (path: string): PlannedEntry => (made++, { path, size: null, offset: 0 });
  const file = (path: string): PlannedEntry => {
    made++;
    const size = next() < 0.97 ? Math.floor(next() ** 2 * 2049) : between(2049, 65536);
    return { path, size, offset: Math.floor(next() * (POOL_BYTES - size)) };
  };
  function* nested(path: string, depth: number, deepest: number): Generator<PlannedEntry> {
    if (made >= target) return;
    yield folder(path);
    const files = between(1, 8);
    for (let i = 0; i < files && made < target; i++) yield file(path + "/" + pick(SOURCE_NAMES) + "-" + i + pick(SOURCE_EXTENSIONS));
    if (depth >= deepest) return;
    const folders = between(depth < 3 ? 1 : 0, 2);
    for (let i = 0; i < folders; i++) yield* nested(path + "/" + pick(FOLDER_NAMES) + "-" + i, depth + 1, deepest);
  }
  for (let project = 0; made < target; project++) {
    const root = "project-" + String(project).padStart(3, "0");
    yield folder(root);
    for (const name of ["package.json", "README.md", "tsconfig.json"]) if (made < target) yield file(root + "/" + name);
    yield* nested(root + "/src", 2, between(3, MAX_FOLDER_DEPTH));
    if (made >= target) return;
    yield folder(root + "/node_modules");
    const packages = between(150, 450);
    const scopes = new Set<string>();
    for (let i = 0; i < packages && made < target; i++) {
      let path = root + "/node_modules/";
      let depth = 3;
      if (next() < 0.2) {
        const scope = "@scope-" + between(0, 19);
        path += scope + "/";
        depth++;
        if (!scopes.has(scope)) {
          scopes.add(scope);
          yield folder(path.slice(0, -1));
          if (made >= target) return;
        }
      }
      path += "package-" + i;
      yield folder(path);
      for (const name of ["package.json", "LICENSE", "README.md"]) if (made < target) yield file(path + "/" + name);
      yield* nested(path + "/" + pick(FOLDER_NAMES), depth + 1, between(Math.max(depth + 1, 3), MAX_FOLDER_DEPTH));
    }
  }
}

export function shapeOf(entries: Iterable<PlannedEntry>): Shape {
  const shape: Shape = { entries: 0, files: 0, folders: 0, bytes: 0, maxDepth: 0 };
  for (const entry of entries) {
    shape.entries++;
    if (entry.size === null) shape.folders++;
    else {
      shape.files++;
      shape.bytes += entry.size;
    }
    shape.maxDepth = Math.max(shape.maxDepth, entry.path.split("/").length);
  }
  return shape;
}

export interface Status {
  state: string;
  remoteNodes: number;
  localEntries: number | null;
  headSeq: number;
  filesToUpload: number;
  bytesDone: number;
}

export interface Sample {
  seconds: number;
  status: Status | null;
  cpuSeconds: number | null;
  rssBytes: number | null;
}

export function uploadedEntries(status: Status): number {
  return Math.max(0, status.remoteNodes - 1);
}

export function synced(status: Status | null, entries: number): boolean {
  return status !== null && status.state === "up_to_date" && status.filesToUpload === 0 && uploadedEntries(status) >= entries;
}

function advanced(status: Status, best: Status): boolean {
  return status.remoteNodes > best.remoteNodes || status.bytesDone > best.bytesDone || status.headSeq > best.headSeq;
}

function raise(best: Status, status: Status): Status {
  return { ...best, remoteNodes: Math.max(best.remoteNodes, status.remoteNodes), bytesDone: Math.max(best.bytesDone, status.bytesDone), headSeq: Math.max(best.headSeq, status.headSeq) };
}

export interface Progress {
  firstUploadSeconds: number | null;
  syncedSeconds: number | null;
  stallSeconds: number;
  stalls: number;
  longestStallSeconds: number;
}

export function progress(samples: Sample[], entries: number, minStallSeconds = STALL_SECONDS): Progress {
  const result: Progress = { firstUploadSeconds: null, syncedSeconds: null, stallSeconds: 0, stalls: 0, longestStallSeconds: 0 };
  const stall = (seconds: number) => {
    if (seconds < minStallSeconds) return;
    result.stalls++;
    result.stallSeconds += seconds;
    result.longestStallSeconds = Math.max(result.longestStallSeconds, seconds);
  };
  let base: Status | null = null;
  let best: Status | null = null;
  let lastAdvance = 0;
  for (const sample of samples) {
    const status = sample.status;
    if (synced(status, entries)) {
      if (result.firstUploadSeconds === null) result.firstUploadSeconds = sample.seconds;
      else stall(sample.seconds - lastAdvance);
      result.syncedSeconds = sample.seconds;
      return result;
    }
    if (!status) continue;
    if (!base || !best) {
      base = best = status;
      continue;
    }
    if (result.firstUploadSeconds === null) {
      best = raise(best, status);
      if (status.headSeq <= base.headSeq && status.bytesDone <= base.bytesDone) continue;
      result.firstUploadSeconds = lastAdvance = sample.seconds;
      continue;
    }
    if (!advanced(status, best)) continue;
    best = raise(best, status);
    stall(sample.seconds - lastAdvance);
    lastAdvance = sample.seconds;
  }
  const last = samples.at(-1);
  if (last && result.firstUploadSeconds !== null) stall(last.seconds - lastAdvance);
  return result;
}

export interface Build {
  ref: string;
  sha: string;
}

export interface RunResult extends Progress {
  index: number;
  ref: string;
  sha: string;
  rep: number;
  order: number;
  entries: number;
  ended: "synced" | "capped" | "failed";
  failure: string | null;
  capSeconds: number;
  elapsedSeconds: number;
  linkSeconds: number;
  syncedEntries: number;
  timeouts: Record<TimeoutKind, number>;
  cpuSeconds: number | null;
  peakRssBytes: number | null;
  storeBytes: number;
  storeBytesPerEntry: number | null;
  samples: number;
}

export function countTimeouts(log: string): Record<TimeoutKind, number> {
  const counts = { didNotMove: 0, noCompleteAnswer: 0, keptFailing: 0 };
  for (const line of log.split("\n")) {
    for (const [kind, text] of Object.entries(TIMEOUT_MESSAGES) as [TimeoutKind, string][]) if (line.includes(text)) counts[kind]++;
  }
  return counts;
}

export function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 ? sorted[middle]! : (sorted[middle - 1]! + sorted[middle]!) / 2;
}

interface Measured {
  value: number;
  capped: boolean;
}

function spread(measured: (Measured | null)[], unit: (value: number) => string): string {
  const known = measured.filter((value): value is Measured => value !== null);
  if (!known.length) return "n/a";
  const sorted = [...known].sort((a, b) => a.value - b.value);
  const middle = sorted.length >> 1;
  const show = (entries: Measured[], value: number) => (entries.some((entry) => entry.capped) ? "≥ " : "") + unit(value);
  const text = show(sorted.length % 2 ? [sorted[middle]!] : sorted.slice(middle - 1, middle + 1), median(sorted.map((entry) => entry.value)));
  if (known.length < measured.length) return text + " (" + known.length + " of " + measured.length + " runs)";
  return known.length === 1 ? text : text + " (" + show([sorted[0]!], sorted[0]!.value) + " to " + show([sorted.at(-1)!], sorted.at(-1)!.value) + ")";
}

const seconds = (value: number) => (value >= 100 ? Math.round(value) : Math.round(value * 10) / 10) + " s";
const count = (value: number) => String(Math.round(value * 10) / 10);
const mib = (value: number) => Math.round(value / (1 << 20)) + " MiB";
const bytes = (value: number) => Math.round(value) + " B";
const plain = (value: number | null): Measured | null => (value === null ? null : { value, capped: false });

export const SUMMARY_COLUMNS = ["Ref", "Synced", "First upload", "Fully synced", "Stall time", "Stalls ≥ 5 s", "Longest stall", "Timeouts", "Daemon CPU", "Peak RSS", "Store per entry"];

export function summary(builds: Build[], results: RunResult[], planned: number): string {
  const lines = ["| " + SUMMARY_COLUMNS.join(" | ") + " |", "|" + SUMMARY_COLUMNS.map(() => "---").join("|") + "|"];
  for (const build of builds) {
    const all = results.filter((result) => result.ref === build.ref);
    const runs = all.filter((run) => run.ended !== "failed");
    const failed = all.length - runs.length;
    const notRun = planned / builds.length - all.length;
    const notes = [failed ? failed + " failed" : "", notRun ? notRun + " not run" : ""].filter(Boolean);
    const cell = (pick: (run: RunResult) => Measured | null, unit: (value: number) => string) => spread(runs.map(pick), unit);
    lines.push(
      "| " +
        [
          "`" + build.ref + "` (" + build.sha.slice(0, 10) + ")",
          runs.filter((run) => run.syncedSeconds !== null).length + " of " + all.length + (notes.length ? " (" + notes.join(", ") + ")" : ""),
          cell((run) => plain(run.firstUploadSeconds), seconds),
          cell((run) => ({ value: run.syncedSeconds ?? run.elapsedSeconds, capped: run.syncedSeconds === null }), seconds),
          cell((run) => plain(run.stallSeconds), seconds),
          cell((run) => plain(run.stalls), count),
          cell((run) => plain(run.longestStallSeconds), seconds),
          cell((run) => plain(run.timeouts.didNotMove + run.timeouts.noCompleteAnswer + run.timeouts.keptFailing), count),
          cell((run) => plain(run.cpuSeconds), seconds),
          cell((run) => plain(run.peakRssBytes), mib),
          cell((run) => plain(run.storeBytesPerEntry), bytes),
        ].join(" | ") +
        " |",
    );
  }
  return lines.join("\n") + "\n";
}

export const RUN_COLUMNS = ["Run", "Ref", "Rep", "Order", "First upload", "Fully synced", "Entries synced", "Stall time", "Stalls", "Longest stall", "Timeouts (moved, answer, failing)", "CPU", "Peak RSS", "Store", "Store per entry"];

export function runTable(results: RunResult[]): string {
  const or = (value: number | null, unit: (value: number) => string) => (value === null ? "n/a" : unit(value));
  const lines = ["| " + RUN_COLUMNS.join(" | ") + " |", "|" + RUN_COLUMNS.map(() => "---").join("|") + "|"];
  for (const run of [...results].sort((a, b) => a.index - b.index)) {
    lines.push(
      "| " +
        [
          run.index + 1,
          "`" + run.ref + "`",
          run.rep + 1,
          run.order + 1,
          or(run.firstUploadSeconds, seconds),
          run.ended === "failed" ? "failed: " + (run.failure ?? "").replace(/\s+/g, " ").trim().replaceAll("|", "\\|") : run.syncedSeconds === null ? "capped at " + seconds(run.elapsedSeconds) : seconds(run.syncedSeconds),
          run.syncedEntries + " of " + run.entries,
          seconds(run.stallSeconds),
          run.stalls,
          seconds(run.longestStallSeconds),
          run.timeouts.didNotMove + ", " + run.timeouts.noCompleteAnswer + ", " + run.timeouts.keptFailing,
          or(run.cpuSeconds, seconds),
          or(run.peakRssBytes, mib),
          mib(run.storeBytes),
          or(run.storeBytesPerEntry, bytes),
        ].join(" | ") +
        " |",
    );
  }
  return lines.join("\n") + "\n";
}
