// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { buildEnv } from "./first-sync.ts";
import {
  countTimeouts,
  holdDeadline,
  holdReason,
  median,
  type PlannedEntry,
  planTree,
  progress,
  readInputs,
  type RunResult,
  runOrder,
  runTable,
  type Sample,
  SETUP_MINUTES,
  type Status,
  shapeOf,
  summary,
} from "./first-sync-model.ts";

const ENTRIES = 1000;

function status(remoteNodes: number, extra: Partial<Status> = {}): Status {
  return { state: "bootstrapping", remoteNodes, localEntries: ENTRIES, headSeq: remoteNodes, filesToUpload: 10, bytesDone: 0, ...extra };
}

function samples(...statuses: (Status | null)[]): Sample[] {
  return statuses.map((status, i) => ({ seconds: i * 2, status, cpuSeconds: i, rssBytes: 1 << 20 }));
}

const done = status(ENTRIES + 1, { state: "up_to_date", filesToUpload: 0 });

describe("first-sync inputs", () => {
  it.each(["main", "v0.1.0-alpha.7", "main,fix/koi-938,v0.1.0-alpha.7", "a,b,c,d", "9808ff5e9"])("accepts the refs %s", (refs) => {
    expect(readInputs({ FIRST_SYNC_REFS: refs, FIRST_SYNC_MINUTES: "60" }).refs).toEqual(refs.split(","));
  });

  it.each(["", "main,", "main,main", "a,b,c,d,e", "-x", "a b", "a;b", "a..b", "a/", "a//b", "a.lock", "$(id)", "../main"])("rejects the refs %j", (refs) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: refs })).toThrow("FIRST_SYNC_REFS");
  });

  it("runs each ref once from the first, with 80 minutes each, when the inputs are left out", () => {
    expect(readInputs({ FIRST_SYNC_REFS: "v0.1.0-alpha.7,main,fix" })).toEqual({ refs: ["v0.1.0-alpha.7", "main", "fix"], repetitions: 1, rotation: 0, minutes: 80 });
  });

  it.each(["", "0", "5", "1.5", "01", "2;id"])("rejects %j repetitions", (repetitions) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "main", FIRST_SYNC_REPETITIONS: repetitions, FIRST_SYNC_MINUTES: "10" })).toThrow("FIRST_SYNC_REPETITIONS");
  });

  it.each([
    ["v0.1.0-alpha.7,main", "3"],
    ["a,b,c", "2"],
    ["a,b,c", "4"],
    ["a,b,c,d", "2"],
  ])("refuses %s with %s repetitions, which puts some refs first more often than others", (refs, repetitions) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: refs, FIRST_SYNC_REPETITIONS: repetitions, FIRST_SYNC_MINUTES: "5" })).toThrow("FIRST_SYNC_REPETITIONS must be 1 or a multiple of the " + refs.split(",").length + " refs");
  });

  it.each([
    ["main", "3"],
    ["a,b", "2"],
    ["a,b", "4"],
    ["a,b,c", "3"],
    ["a,b,c", "1"],
  ])("accepts %s with %s repetitions", (refs, repetitions) => {
    expect(readInputs({ FIRST_SYNC_REFS: refs, FIRST_SYNC_REPETITIONS: repetitions, FIRST_SYNC_MINUTES: "5" }).repetitions).toBe(Number(repetitions));
  });

  it.each(["0", "1", "2"])("accepts rotation %s for three refs", (rotation) => {
    expect(readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_ROTATION: rotation }).rotation).toBe(Number(rotation));
  });

  it.each(["", "3", "-1", "01", "1.0", "1;id"])("rejects rotation %j for three refs", (rotation) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_ROTATION: rotation })).toThrow("FIRST_SYNC_ROTATION must be a whole number from 0 to 2");
  });

  it.each(["", "0", "241", "1e2", "030", "30 "])("rejects %j minutes", (minutes) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "main", FIRST_SYNC_MINUTES: minutes })).toThrow("FIRST_SYNC_MINUTES");
  });

  it("refuses runs that with the job's setup do not fit its 6 hours, and names the cap that fits", () => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c,d" })).toThrow(
      "4 runs of up to 80 minutes and 5 minutes each to reset the tree, with 15 minutes to free disk, 20 to generate the tree, 40 to wait for the approval and the pairing codes, 10 for the summary and 5 to spare, take 430 minutes, more than the macOS job's 360. Lower FIRST_SYNC_MINUTES to 62 or run fewer refs or repetitions.",
    );
    expect(readInputs({ FIRST_SYNC_REFS: "a,b,c,d", FIRST_SYNC_MINUTES: "62" }).minutes).toBe(62);
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c,d", FIRST_SYNC_MINUTES: "63" })).toThrow("take 362 minutes");
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_REPETITIONS: "3" })).toThrow("Lower FIRST_SYNC_MINUTES to 25");
    expect(readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_MINUTES: "85" }).minutes).toBe(85);
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_MINUTES: "86" })).toThrow("take 363 minutes");
  });
});

describe("run order", () => {
  it("starts each dispatch's single round at its rotation, so three dispatches put each ref first once", () => {
    const order = (rotation: number) => runOrder(3, 1, rotation).map((run) => "ABC"[run.ref]).join("");
    expect([0, 1, 2].map(order)).toEqual(["ABC", "BCA", "CAB"]);
  });

  it("starts each repetition one ref later, continuing from the rotation", () => {
    const runs = runOrder(3, 3, 0);
    expect(runs.map((run) => "ABC"[run.ref]).join("")).toBe("ABCBCACAB");
    expect(runs.map((run) => run.rep)).toEqual([0, 0, 0, 1, 1, 1, 2, 2, 2]);
    expect(runs.map((run) => run.index)).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8]);
    expect(runOrder(3, 3, 2).map((run) => "ABC"[run.ref]).join("")).toBe("CABABCBCA");
    expect(runOrder(2, 2, 1).map((run) => "AB"[run.ref]).join("")).toBe("BAAB");
  });

  it.each([
    [2, 2],
    [2, 4],
    [3, 3],
    [4, 4],
  ])("puts each of %i refs in each position equally often over %i repetitions", (refs, repetitions) => {
    for (let rotation = 0; rotation < refs; rotation++) {
      const runs = runOrder(refs, repetitions, rotation);
      for (let ref = 0; ref < refs; ref++) {
        for (let order = 0; order < refs; order++) expect(runs.filter((run) => run.ref === ref && run.order === order)).toHaveLength(repetitions / refs);
      }
    }
  });
});

describe("Worker hold deadline", () => {
  const deployStart = Date.UTC(2026, 10, 18, 9, 0);
  const minutes = (n: number) => n * 60_000;

  it("ends 345 minutes after the sync job started when both jobs start together", () => {
    expect(holdDeadline(deployStart, deployStart)).toEqual({ until: deployStart + minutes(345), cap: "sync" });
  });

  it("moves with a sync job that started before the deploy job", () => {
    expect(holdDeadline(deployStart - minutes(10), deployStart)).toEqual({ until: deployStart + minutes(335), cap: "sync" });
  });

  it("stops at the deploy job's 345 minutes when the sync runner queued later, because GitHub ends the deploy job at 360", () => {
    const hold = holdDeadline(deployStart + minutes(25), deployStart);
    expect(hold).toEqual({ until: deployStart + minutes(345), cap: "deploy" });
    expect(holdReason(hold)).toBe("345 minutes after the deploy job started, because GitHub ends a job after 360 minutes and the teardown needs 15");
  });

  it("names the sync job's limit when that one binds", () => {
    expect(holdReason(holdDeadline(deployStart, deployStart))).toBe("345 minutes after the sync job started, after which it starts no run");
  });

  it("fails the job when the Worker's hold ends before the planned runs could start", () => {
    const dir = mkdtempSync(join(tmpdir(), "koidrive-first-sync-unit-"));
    try {
      mkdirSync(join(dir, "bin", "first-sync-koi-0"), { recursive: true });
      writeFileSync(join(dir, "bin", "first-sync-koi-0", "build.json"), JSON.stringify({ index: 0, ref: "main", sha: "1111111111aaaa" }));
      writeFileSync(join(dir, "bin", "first-sync-koi-0", "koi"), "", { mode: 0o755 });
      writeFileSync(join(dir, "shape.json"), "{}");
      writeFileSync(join(dir, "hold.json"), JSON.stringify({ until: Date.now() + 60_000, cap: "sync" }));
      const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
      const measure = spawnSync(process.execPath, [...flags, join(here, "first-sync.ts"), "measure"], { env: { PATH: process.env.PATH, FIRST_SYNC_WORK: dir, FIRST_SYNC_REFS: "main" }, encoding: "utf8" });
      expect(measure.status).not.toBe(0);
      expect(measure.stdout + measure.stderr).toContain("not every one of the 1 runs synced: runs 1 to 1 did not start");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("hands the sync job's start time to the deploy job with the public key", () => {
    const dir = mkdtempSync(join(tmpdir(), "koidrive-first-sync-unit-"));
    try {
      const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
      const keygen = (start?: string) =>
        spawnSync(process.execPath, [...flags, join(here, "first-sync.ts"), "keygen"], { env: { PATH: process.env.PATH, FIRST_SYNC_WORK: dir, ...(start ? { FIRST_SYNC_JOB_START: start } : {}) }, encoding: "utf8" });
      expect(keygen().status).not.toBe(0);
      expect(keygen("1795000000").status).toBe(0);
      expect(readFileSync(join(dir, "key", "started-at"), "utf8")).toBe("1795000000000");
      expect(existsSync(join(dir, "key", "public.pem"))).toBe(true);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("generated tree", () => {
  const plan = [...planTree(7, 20_000)];

  it("holds exactly the planned number of entries with unique paths", () => {
    expect(plan).toHaveLength(20_000);
    expect(new Set(plan.map((entry) => entry.path)).size).toBe(20_000);
  });

  it("is the same for the same seed and differs for another", () => {
    expect([...planTree(7, 20_000)]).toEqual(plan);
    expect([...planTree(8, 20_000)].map((entry) => entry.path)).not.toEqual(plan.map((entry) => entry.path));
  });

  it("lists each folder before what it holds", () => {
    const folders = new Set<string>();
    for (const entry of plan) {
      const parent = entry.path.split("/").slice(0, -1).join("/");
      if (parent) expect(folders.has(parent), entry.path).toBe(true);
      if (entry.size === null) folders.add(entry.path);
    }
  });

  it("is shaped like a code workspace of small files under node_modules", () => {
    const shape = shapeOf(plan);
    const files = plan.filter((entry): entry is PlannedEntry & { size: number } => entry.size !== null);
    expect(shape.entries).toBe(20_000);
    expect(shape.files + shape.folders).toBe(20_000);
    expect(shape.maxDepth).toBeGreaterThanOrEqual(6);
    expect(shape.maxDepth).toBeLessThanOrEqual(9);
    expect(files.every((file) => file.size >= 0 && file.size <= 65536)).toBe(true);
    expect(files.filter((file) => file.size <= 2048).length / files.length).toBeGreaterThan(0.95);
    expect(files.some((file) => file.size > 32768)).toBe(true);
    expect(plan.filter((entry) => entry.path.includes("/node_modules/")).length / plan.length).toBeGreaterThan(0.8);
    expect(shape.bytes / shape.files).toBeLessThan(3000);
    expect(new Set(plan.map((entry) => entry.path.split("/")[0])).size).toBeGreaterThan(1);
  });
});

describe("progress from samples", () => {
  it("measures steady progress without stalls", () => {
    expect(progress(samples(status(0), status(0), status(100), status(400), status(800), done), ENTRIES)).toEqual({
      firstUploadSeconds: 4,
      syncedSeconds: 10,
      stallSeconds: 0,
      stalls: 0,
      longestStallSeconds: 0,
    });
  });

  it("counts a gap of 5 s or more without progress as a stall and ignores shorter gaps", () => {
    const result = progress(samples(status(0), status(10), status(10), status(20), status(20), status(20), status(20), status(20), status(30), done), ENTRIES);
    expect(result).toMatchObject({ firstUploadSeconds: 2, syncedSeconds: 18, stallSeconds: 10, stalls: 1, longestStallSeconds: 10 });
  });

  it("does not count the scan before the first upload as a stall", () => {
    const result = progress(samples(status(0), status(0), status(0), status(0), status(0), status(0), status(500), done), ENTRIES);
    expect(result).toMatchObject({ firstUploadSeconds: 12, stalls: 0 });
  });

  it("treats uploaded bytes and new commits as progress", () => {
    const result = progress(samples(status(0), status(10), status(10, { bytesDone: 1 }), status(10, { bytesDone: 2 }), status(10, { headSeq: 11, bytesDone: 2 }), done), ENTRIES);
    expect(result).toMatchObject({ stalls: 0, syncedSeconds: 10 });
  });

  it("does not take the space's root, read from the server, for an upload", () => {
    const root = status(1, { headSeq: 0 });
    const result = progress(samples(status(0), root, root, root, root, status(50), done), ENTRIES);
    expect(result).toMatchObject({ firstUploadSeconds: 10, stalls: 0 });
  });

  it("starts with the first uploaded bytes when a large file goes before any commit", () => {
    expect(progress(samples(status(0), status(0, { bytesDone: 4096 }), done), ENTRIES)).toMatchObject({ firstUploadSeconds: 2 });
  });

  it("counts samples where koi status did not answer as no progress", () => {
    const result = progress(samples(status(0), status(10), null, null, null, status(20), done), ENTRIES);
    expect(result).toMatchObject({ stalls: 1, stallSeconds: 8, longestStallSeconds: 8 });
  });

  it("counts the tail of a capped run as a stall and reports no sync time", () => {
    const result = progress(samples(status(0), status(10), status(20), status(20), status(20), status(20), status(20)), ENTRIES);
    expect(result).toEqual({ firstUploadSeconds: 2, syncedSeconds: null, stallSeconds: 8, stalls: 1, longestStallSeconds: 8 });
  });

  it("waits for every entry besides the space's root and every file before calling the folder synced", () => {
    const early = status(ENTRIES, { state: "up_to_date", filesToUpload: 0 });
    const files = status(ENTRIES + 1, { state: "up_to_date", filesToUpload: 3 });
    expect(progress(samples(status(0), early, files), ENTRIES).syncedSeconds).toBeNull();
  });

  it("reports no upload when nothing moved", () => {
    expect(progress(samples(status(0), status(0), status(0)), ENTRIES)).toEqual({ firstUploadSeconds: null, syncedSeconds: null, stallSeconds: 0, stalls: 0, longestStallSeconds: 0 });
  });
});

describe("timeouts in the daemon log", () => {
  it("counts each kind of local timeout", () => {
    const log = [
      "WARN chunk upload: the transfer of chunk ab kept failing: the body did not move for 30s",
      "WARN chunk upload: no complete answer within 120s",
      "INFO folder: synced",
      "WARN chunk upload: the body did not move for 30s",
    ].join("\n");
    expect(countTimeouts(log)).toEqual({ didNotMove: 2, noCompleteAnswer: 1, keptFailing: 1 });
  });
});

describe("summary", () => {
  function result(index: number, ref: string, extra: Partial<RunResult> = {}): RunResult {
    return {
      index,
      ref,
      sha: ref === "main" ? "1111111111aaaa" : "2222222222bbbb",
      rep: index >> 1,
      order: index % 2,
      entries: ENTRIES,
      ended: "synced",
      failure: null,
      capSeconds: 600,
      elapsedSeconds: 100,
      linkSeconds: 1,
      syncedEntries: ENTRIES,
      firstUploadSeconds: 4,
      syncedSeconds: 100,
      stallSeconds: 0,
      stalls: 0,
      longestStallSeconds: 0,
      timeouts: { didNotMove: 0, noCompleteAnswer: 0, keptFailing: 0 },
      cpuSeconds: 50,
      peakRssBytes: 100 << 20,
      storeBytes: 2 << 20,
      storeBytesPerEntry: 2000,
      samples: 50,
      ...extra,
    };
  }

  it("takes the median of an odd and an even count", () => {
    expect(median([3, 1, 2])).toBe(2);
    expect(median([4, 1, 3, 2])).toBe(2.5);
  });

  it("gives each ref's median and spread and marks runs stopped at the cap", () => {
    const builds = [
      { ref: "main", sha: "1111111111aaaa" },
      { ref: "fix", sha: "2222222222bbbb" },
    ];
    const results = [
      result(0, "main", { syncedSeconds: 120, stalls: 2, stallSeconds: 30, longestStallSeconds: 20 }),
      result(1, "fix", { syncedSeconds: 200 }),
      result(2, "fix", { syncedSeconds: 300 }),
      result(3, "main", { ended: "capped", syncedSeconds: null, elapsedSeconds: 600, stalls: 4, stallSeconds: 90, longestStallSeconds: 40, timeouts: { didNotMove: 3, noCompleteAnswer: 1, keptFailing: 2 } }),
    ];
    const table = summary(builds, results, 4).split("\n");
    expect(table[2]).toBe("| `main` (1111111111) | 1 of 2 | 4 s (4 s to 4 s) | ≥ 360 s (120 s to ≥ 600 s) | 60 s (30 s to 90 s) | 3 (2 to 4) | 30 s (20 s to 40 s) | 3 (0 to 6) | 50 s (50 s to 50 s) | 100 MiB (100 MiB to 100 MiB) | 2000 B (2000 B to 2000 B) |");
    expect(table[3]).toContain("| `fix` (2222222222) | 2 of 2 | 4 s (4 s to 4 s) | 250 s (200 s to 300 s) |");
  });

  it("marks a median as a lower bound only when a capped run sits in the middle", () => {
    const builds = [{ ref: "main", sha: "1111111111aaaa" }];
    const capped = { ended: "capped" as const, syncedSeconds: null, elapsedSeconds: 600 };
    expect(summary(builds, [result(0, "main", { syncedSeconds: 100 }), result(1, "main", { syncedSeconds: 200 }), result(2, "main", capped)], 3)).toContain("| 200 s (100 s to ≥ 600 s) |");
    expect(summary(builds, [result(0, "main", { syncedSeconds: 100 }), result(1, "main", capped), result(2, "main", capped)], 3)).toContain("| ≥ 600 s (100 s to ≥ 600 s) |");
  });

  it("leaves a run whose daemon exited out of the statistics and gives the reason", () => {
    const builds = [{ ref: "main", sha: "1111111111aaaa" }];
    const failed = result(2, "main", { ended: "failed", failure: "the daemon exited after 50 s", syncedSeconds: null, elapsedSeconds: 50, cpuSeconds: 1, stalls: 9 });
    const results = [result(0, "main", { syncedSeconds: 100 }), result(1, "main", { syncedSeconds: 200 }), failed];
    expect(summary(builds, results, 3)).toContain("| `main` (1111111111) | 2 of 3 (1 failed) | 4 s (4 s to 4 s) | 150 s (100 s to 200 s) | 0 s (0 s to 0 s) | 0 (0 to 0) |");
    expect(runTable(results)).toContain("| 3 | `main` | 2 | 1 | 4 s | failed: the daemon exited after 50 s |");
  });

  it("keeps a multi-line failure with pipes inside one table cell", () => {
    const failed = result(0, "main", { ended: "failed", failure: "the daemon exited: a | b\nsecond line\n", syncedSeconds: null });
    expect(runTable([failed])).toContain("| failed: the daemon exited: a \\| b second line | 1000 of 1000 |");
  });

  it("gives no statistics for a ref whose every run failed", () => {
    const failed = result(0, "main", { ended: "failed", failure: "koi space create failed", syncedSeconds: null });
    expect(summary([{ ref: "main", sha: "1111111111aaaa" }], [failed], 1)).toContain("| 0 of 1 (1 failed) | n/a | n/a |");
  });

  it("says how many runs of a ref did not run", () => {
    const table = summary([{ ref: "main", sha: "1111111111aaaa" }], [result(0, "main")], 3);
    expect(table).toContain("| 1 of 1 (2 not run) |");
  });

  it("lists every run in order", () => {
    const rows = runTable([result(1, "fix"), result(0, "main", { ended: "capped", syncedSeconds: null, elapsedSeconds: 600 })]).split("\n");
    expect(rows[2]).toBe("| 1 | `main` | 1 | 1 | 4 s | capped at 600 s | 1000 of 1000 | 0 s | 0 | 0 s | 0, 0, 0 | 50 s | 100 MiB | 2 MiB | 2000 B |");
    expect(rows[3]).toContain("| 2 | `fix` | 1 | 2 |");
  });
});

const here = dirname(fileURLToPath(import.meta.url));
const TOKENS = {
  GH_TOKEN: "private-github-token",
  GITHUB_TOKEN: "private-github-token",
  ACTIONS_RUNTIME_TOKEN: "private-runtime-token",
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: "private-oidc-token",
  CLOUDFLARE_API_TOKEN: "private-cloudflare-token",
  CLOUDFLARE_ACCOUNT_ID: "private-cloudflare-account",
  KOIDRIVE_DEPLOY_TEST_STATE: "/private/state.json",
};

describe("koi build", () => {
  let work: string;
  let source: string;
  let sha: string;

  beforeEach(() => {
    work = mkdtempSync(join(tmpdir(), "koidrive-first-sync-unit-"));
    source = join(work, "source");
    mkdirSync(join(work, "bin"));
    mkdirSync(source);
    const git = (...args: string[]) => execFileSync("git", ["-c", "user.name=t", "-c", "user.email=t@example.com", ...args], { cwd: source, encoding: "utf8" }).trim();
    git("init", "-q");
    git("commit", "-q", "--allow-empty", "-m", "ref under test");
    sha = git("rev-parse", "HEAD");
    for (const tool of ["cargo", "rustup"]) {
      writeFileSync(
        join(work, "bin", tool),
        `#!${process.execPath}
const { mkdirSync, writeFileSync } = require("node:fs");
const { join } = require("node:path");
writeFileSync(join(process.cwd(), "${tool}.json"), JSON.stringify({ args: process.argv.slice(2), env: process.env }));
if ("${tool}" === "cargo") {
  mkdirSync(join(process.env.CARGO_TARGET_DIR, "release"), { recursive: true });
  writeFileSync(join(process.env.CARGO_TARGET_DIR, "release", "koi"), "koi built from the ref");
}
`,
        { mode: 0o755 },
      );
    }
  });

  afterEach(() => rmSync(work, { recursive: true, force: true }));

  function build(extra: NodeJS.ProcessEnv = {}): Promise<{ status: number | null; output: string }> {
    const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
    return new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [...flags, join(here, "first-sync.ts"), "build", source, join(work, "out")], {
        env: { PATH: join(work, "bin") + delimiter + process.env.PATH, HOME: work, TMPDIR: work, FIRST_SYNC_WORK: join(work, "w"), FIRST_SYNC_INDEX: "1", FIRST_SYNC_REF: "fix/branch", FIRST_SYNC_SHA: sha, ...TOKENS, ...extra },
      });
      let output = "";
      child.stdout.on("data", (chunk: Buffer) => (output += chunk));
      child.stderr.on("data", (chunk: Buffer) => (output += chunk));
      child.on("error", reject);
      child.on("close", (status) => resolve({ status, output }));
    });
  }

  it("runs rustup and the release build of the checked-out ref without any token in their environment", async () => {
    const result = await build();
    expect(result.status, result.output).toBe(0);
    const cargo = JSON.parse(readFileSync(join(source, "cargo.json"), "utf8"));
    const rustup = JSON.parse(readFileSync(join(source, "rustup.json"), "utf8"));
    expect(cargo.args).toEqual(["build", "--release", "--locked", "-p", "koidrive-cli", "--bin", "koi"]);
    expect(rustup.args).toEqual(["show", "active-toolchain"]);
    for (const seen of [cargo.env, rustup.env]) {
      for (const name of Object.keys(TOKENS)) expect(seen).not.toHaveProperty(name);
      expect(JSON.stringify(seen)).not.toContain("private-");
      expect(seen.CARGO_TARGET_DIR).toBe(join(work, "w", "target", "1"));
    }
    expect(readFileSync(join(work, "out", "koi"), "utf8")).toBe("koi built from the ref");
    expect(JSON.parse(readFileSync(join(work, "out", "build.json"), "utf8"))).toEqual({ index: 1, ref: "fix/branch", sha });
    expect(existsSync(join(work, "w", "target", "1"))).toBe(false);
  });

  it("refuses a checkout at another commit than the ref resolved to", async () => {
    const result = await build({ FIRST_SYNC_SHA: "0".repeat(40) });
    expect(result.status).toBe(1);
    expect(result.output).toContain("holds " + sha + ", not " + "0".repeat(40));
    expect(existsSync(join(source, "cargo.json"))).toBe(false);
  });

  it("passes on only the variables a Rust build reads", () => {
    expect(buildEnv({ PATH: "/bin", HOME: "/home", CARGO_HOME: "/cargo", DEVELOPER_DIR: "/xcode", ...TOKENS, GITHUB_REPOSITORY: "koidrive/koidrive" }, "/target")).toEqual({
      PATH: "/bin",
      HOME: "/home",
      CARGO_HOME: "/cargo",
      DEVELOPER_DIR: "/xcode",
      CARGO_TARGET_DIR: "/target",
    });
  });
});

describe("deploy-test workflow", () => {
  const workflow = readFileSync(join(here, "..", "..", "..", ".github", "workflows", "deploy-test.yml"), "utf8");
  const jobs = Object.fromEntries([...workflow.split(/^jobs:\n/m)[1]!.matchAll(/^ {2}([a-z0-9-]+):\n((?: {4}.*\n|\n)*)/gm)].map((match) => [match[1]!, match[2]!]));
  const steps = (job: string) => jobs[job]!.split(/^ {6}- /m).slice(1);

  it("gives the build jobs, which run the refs' build scripts, no token, secret or environment", () => {
    const job = jobs["first-sync-build"]!;
    expect(job).toBeDefined();
    expect(job).not.toMatch(/GH_TOKEN|GITHUB_TOKEN|secrets\.|github\.token|environment:|actions: /);
    expect(job).toContain("    permissions:\n      contents: read\n    strategy:");
  });

  it("gives each setup step of the sync job the minutes the input check reserves for it", () => {
    const timeout = (name: string) => Number(/timeout-minutes: (\d+)\n/.exec(steps("first-sync-macos").find((step) => step.startsWith("name: " + name + "\n"))!)?.[1]);
    expect(timeout("Free disk space")).toBe(SETUP_MINUTES.disk);
    expect(timeout("Generate the tree")).toBe(SETUP_MINUTES.tree);
    expect(timeout("Download the sealed pairing codes")).toBe(SETUP_MINUTES.pairing);
  });

  it("records each first-sync job's start in its first step and uploads the sync job's start with its key", () => {
    for (const job of ["deploy-test", "first-sync-macos"]) expect(steps(job)[0]).toContain('run: echo "FIRST_SYNC_JOB_START=$(date +%s)" >> "$GITHUB_ENV"\n');
    const upload = steps("first-sync-macos").find((step) => step.includes("name: first-sync-macos-key-"));
    expect(upload).toContain("path: ${{ runner.temp }}/koidrive-first-sync/key/\n");
  });

  it("hands the token in the macOS sync job only to the step that downloads the pairing codes", () => {
    const job = jobs["first-sync-macos"]!;
    expect(job.split("    steps:")[0]).not.toMatch(/GH_TOKEN|github\.token/);
    expect(job).not.toMatch(/secrets\./);
    const withToken = steps("first-sync-macos").filter((step) => /GH_TOKEN|github\.token/.test(step));
    expect(withToken).toHaveLength(1);
    expect(withToken[0]).toContain("run: node scripts/deploy-test/first-sync.ts fetch-codes\n");
  });
});
