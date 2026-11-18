// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from "vitest";
import {
  countTimeouts,
  median,
  type PlannedEntry,
  planTree,
  progress,
  readInputs,
  type RunResult,
  runOrder,
  runTable,
  type Sample,
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
    expect(readInputs({ FIRST_SYNC_REFS: refs, FIRST_SYNC_REPETITIONS: "1", FIRST_SYNC_MINUTES: "20" }).refs).toEqual(refs.split(","));
  });

  it.each(["", "main,", "main,main", "a,b,c,d,e", "-x", "a b", "a;b", "a..b", "a/", "a//b", "a.lock", "$(id)", "../main"])("rejects the refs %j", (refs) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: refs })).toThrow("FIRST_SYNC_REFS");
  });

  it("uses three repetitions and 35 minutes when the inputs are left out", () => {
    expect(readInputs({ FIRST_SYNC_REFS: "main,v0.1.0-alpha.7" })).toEqual({ refs: ["main", "v0.1.0-alpha.7"], repetitions: 3, minutes: 35 });
  });

  it.each(["", "0", "6", "1.5", "01", "2;id"])("rejects %j repetitions", (repetitions) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "main", FIRST_SYNC_REPETITIONS: repetitions })).toThrow("FIRST_SYNC_REPETITIONS");
  });

  it.each(["", "0", "121", "1e2", "030", "30 "])("rejects %j minutes", (minutes) => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "main", FIRST_SYNC_MINUTES: minutes })).toThrow("FIRST_SYNC_MINUTES");
  });

  it("refuses builds and runs that do not fit the macOS job and names the cap that fits", () => {
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_REPETITIONS: "3", FIRST_SYNC_MINUTES: "35" })).toThrow(
      "3 builds of about 35 minutes and 9 runs of up to 35 minutes plus 5 minutes each to reset the tree take 465 minutes, but the macOS job has 320 minutes for them. Lower FIRST_SYNC_MINUTES to 18",
    );
    expect(readInputs({ FIRST_SYNC_REFS: "a,b,c", FIRST_SYNC_REPETITIONS: "3", FIRST_SYNC_MINUTES: "18" }).minutes).toBe(18);
    expect(readInputs({ FIRST_SYNC_REFS: "a,b", FIRST_SYNC_REPETITIONS: "3", FIRST_SYNC_MINUTES: "35" }).minutes).toBe(35);
    expect(() => readInputs({ FIRST_SYNC_REFS: "a,b,c,d", FIRST_SYNC_REPETITIONS: "5", FIRST_SYNC_MINUTES: "10" })).toThrow("Lower FIRST_SYNC_MINUTES to 4 or run fewer refs or repetitions.");
    expect(readInputs({ FIRST_SYNC_REFS: "a,b,c,d", FIRST_SYNC_REPETITIONS: "5", FIRST_SYNC_MINUTES: "4" }).minutes).toBe(4);
  });
});

describe("run order", () => {
  it("runs every ref once per repetition and starts each repetition one ref later", () => {
    const runs = runOrder(3, 3);
    expect(runs.map((run) => "ABC"[run.ref]).join("")).toBe("ABCBCACAB");
    expect(runs.map((run) => run.rep)).toEqual([0, 0, 0, 1, 1, 1, 2, 2, 2]);
    expect(runs.map((run) => run.index)).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8]);
    for (const ref of [0, 1, 2]) expect(runs.filter((run) => run.ref === ref).map((run) => run.order).sort()).toEqual([0, 1, 2]);
  });

  it("alternates two refs", () => {
    expect(runOrder(2, 3).map((run) => run.ref)).toEqual([0, 1, 1, 0, 0, 1]);
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
