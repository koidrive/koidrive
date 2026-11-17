// SPDX-License-Identifier: AGPL-3.0-or-later

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { emptyBucket, LISTING_MS, newestDeployment, serves, workerTag } from "./cloudflare.ts";
import { MockCloudflare } from "./mock-cloudflare.ts";
import type { State } from "./state.ts";

const here = dirname(fileURLToPath(import.meta.url));
const server = join(here, "..", "..");
const NAME = "koidrive-test-1-1";

let mock: MockCloudflare;
let work: string;

beforeEach(async () => {
  mock = new MockCloudflare();
  await mock.start();
  work = mkdtempSync(join(tmpdir(), "koidrive-run-unit-"));
  mkdirSync(join(work, "home"));
  mkdirSync(join(work, "tmp"));
  mkdirSync(join(work, "bin"));
});

afterEach(async () => {
  await mock.stop();
  rmSync(work, { recursive: true, force: true });
});

function writeState(state: Partial<State>): void {
  const full: State = { name: NAME, owned: {}, deployments: [], stats: { requests: 0, bytesUp: 0, bytesDown: 0 }, ...state };
  writeFileSync(join(work, "state.json"), JSON.stringify(full));
}

function readState(): State {
  return JSON.parse(readFileSync(join(work, "state.json"), "utf8")) as State;
}

function exec(command: string, args: string[], cwd: string, env: NodeJS.ProcessEnv): Promise<{ status: number | null; output: string }> {
  return new Promise((resolve) => {
    const child = spawn(command, args, { cwd, env });
    let output = "";
    child.stdout.on("data", (chunk: Buffer) => (output += chunk));
    child.stderr.on("data", (chunk: Buffer) => (output += chunk));
    const timer = setTimeout(() => child.kill("SIGKILL"), 120_000);
    child.on("close", (status) => {
      clearTimeout(timer);
      resolve({ status, output });
    });
  });
}

function runTs(...args: string[]): Promise<{ status: number | null; output: string }> {
  const flags = Number(process.versions.node.split(".")[0]) < 23 ? ["--experimental-strip-types", "--no-warnings"] : [];
  return exec(process.execPath, [...flags, join(here, "run.ts"), ...args], server, {
      PATH: join(work, "bin") + delimiter + process.env.PATH,
      HOME: join(work, "home"),
      TMPDIR: join(work, "tmp"),
      MOCK_URL: mock.url,
      FAKE_PNPM_LOG: join(work, "pnpm.log"),
      CLOUDFLARE_API_BASE_URL: mock.url + "/client/v4",
      CLOUDFLARE_API_TOKEN: "dummy-token",
      CLOUDFLARE_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
      KOIDRIVE_TEST_NAME: NAME,
      KOIDRIVE_DEPLOY_TEST_STATE: join(work, "state.json"),
      WRANGLER_SEND_METRICS: "false",
  });
}

interface Step {
  createsWorker: boolean;
  tagged?: boolean;
  status: number;
  unlistedFirst?: number;
}

function fakePnpm(steps: { install: Step; deploy: Step }): void {
  const script = `#!${process.execPath}
const { appendFileSync, readFileSync } = require("node:fs");
const steps = ${JSON.stringify(steps)};
const step = process.argv[2] === "install" ? steps.install : steps.deploy;
const args = process.argv.slice(2);
const at = args.indexOf("--tag");
const tag = at >= 0 ? args.splice(at, 2)[1] : undefined;
appendFileSync(process.env.FAKE_PNPM_LOG, args.join(" ") + "\\n");
const deploys = readFileSync(process.env.FAKE_PNPM_LOG, "utf8").split("\\n").filter((line) => line.startsWith("run deploy")).length;
const unlisted = process.argv[2] !== "install" && deploys <= (step.unlistedFirst ?? 0);
const body = JSON.stringify(process.argv[2] !== "install" && step.tagged !== false ? { tag } : {});
const created = step.createsWorker ? fetch(process.env.MOCK_URL + "/mock/workers/${NAME}", { method: "POST", body }) : Promise.resolve();
created.then(() => {
  if (unlisted) {
    console.log("Uploaded ${NAME} (1.00 sec)");
    console.error("A request to the Cloudflare API (/accounts/x/workers/workers/${NAME}) failed.\\n  This Worker does not exist on your account. [code: 10007]");
    process.exit(1);
  }
  if (process.argv[2] !== "install" && step.status === 0) {
    console.log("Deployed ${NAME} triggers\\n  https://${NAME}.mock.workers.dev\\nCurrent Version ID: 00000000-0000-4000-8000-000000000001");
  }
  process.exit(step.status);
});
`;
  writeFileSync(join(work, "bin", "pnpm"), script, { mode: 0o755 });
}

function pnpmCalls(): string[] {
  return existsSync(join(work, "pnpm.log")) ? readFileSync(join(work, "pnpm.log"), "utf8").trim().split("\n") : [];
}

function filesContaining(root: string, text: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(root, { withFileTypes: true, recursive: true })) {
    const path = join(entry.parentPath, entry.name);
    if (entry.isFile() && statSync(path).size < 64 * 1024 * 1024 && readFileSync(path).includes(text)) found.push(path);
  }
  return found;
}

describe("teardown", () => {
  it("empties 3072 chunk objects across four listing pages with one prefix-delete job and one status poll", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 3072 }, (_, i) => "spaces/bench/chunks/aa/bb/" + i));
    for (const key of bucket.objects.keys()) bucket.objects.set(key, 4 * 1024 * 1024);
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobStatuses = ["RUNNING", "COMPLETED"];
    const started = performance.now();
    const { status, output } = await runTs("teardown");
    expect({ status, errors: output.includes("::error::") }).toEqual({ status: 0, errors: false });
    expect(mock.buckets.has(NAME)).toBe(false);
    expect(mock.requests.filter((request) => request.startsWith("GET ") && request.endsWith("/objects"))).toHaveLength(5);
    expect(mock.deletes().filter((request) => request.includes("/objects/"))).toEqual([]);
    expect(mock.bucketJobBodies).toEqual([{ jobType: "prefixDelete", prefix: "" }]);
    expect(mock.requests.filter((request) => request.includes("/jobs/"))).toHaveLength(1);
    expect(mock.requests).toHaveLength(10);
    expect(output).toContain("3072 objects");
    console.info("3072-object teardown mock: " + (performance.now() - started).toFixed(0) + " ms, " + mock.requests.length + " API requests, four initial listing pages, one fresh listing, one job and one poll, no network retries or analytics wait");
  });

  it("accepts a prefix-delete job that completes synchronously", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    expect((await runTs("teardown")).status).toBe(0);
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.requests.filter((request) => request.includes("/jobs/"))).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it("retries deleting the bucket when it still appears nonempty after the emptying job", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketDeleteFailures = 1;
    const leftover = "chunk-0";
    mock.bucketJobLeftovers = [leftover];
    const { status, output } = await runTs("teardown");
    expect({ status, errors: output.includes("::error::") }).toEqual({ status: 0, errors: false });
    expect(mock.bucketJobBodies).toEqual([{ jobType: "prefixDelete", prefix: "" }]);
    expect(mock.deletes()).toHaveLength(3);
    expect(mock.deletes()[0]).toContain("/objects/" + leftover);
    expect(mock.deletes().slice(1).every((request) => request.endsWith("/r2/buckets/" + NAME))).toBe(true);
    expect(mock.bucketDeleteFailures).toBe(0);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it.each(["FAILED", "CANCELLED", "unexpected"])("leaves the bucket and reports a prefix-delete job with status %s", async (jobStatus) => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobStatuses = ["RUNNING", jobStatus];
    const { status, output } = await runTs("teardown");
    expect(status).toBe(1);
    expect(output).toContain("bucket-emptying job");
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it("does not retry an ambiguous prefix-delete creation failure", async () => {
    const bucket = mock.addBucket(NAME, undefined, Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: bucket.creationDate } });
    mock.bucketJobCreateStatus = 503;
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.bucketJobBodies).toHaveLength(1);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it.each([undefined, "2020-01-01T00:00:00.000Z"])("does not start a bulk job for a large bucket with unproven ownership %s", async (recordedDate) => {
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", Array.from({ length: 1001 }, (_, i) => "chunk-" + i));
    writeState({ owned: { bucketCreationDate: recordedDate } });
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.bucketJobBodies).toEqual([]);
    expect(mock.buckets.get(NAME)?.objects.size).toBe(1001);
    expect(mock.deletes()).toEqual([]);
  });

  it("stops polling a running bucket-emptying job at six minutes", async () => {
    vi.useFakeTimers();
    const fetchJob = vi.fn(async () => Response.json({ result: { id: "empty-1", status: "RUNNING" } }));
    vi.stubGlobal("fetch", fetchJob);
    try {
      const finished = expect(emptyBucket({ id: "dummy-account", token: "dummy-token" }, NAME)).rejects.toThrow("six-minute deadline");
      await vi.advanceTimersByTimeAsync(6 * 60_000);
      await finished;
      expect(fetchJob).toHaveBeenCalledTimes(72);
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  it("leaves a Worker and a bucket alone that this run did not record creating", async () => {
    mock.addWorker(NAME);
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", ["preexisting-user-data.txt"]);
    const { status, output } = await runTs("teardown");
    expect(status).toBe(1);
    expect(mock.deletes()).toEqual([]);
    expect(output).toContain("did not record creating it, so it was left alone");
    expect(mock.buckets.get(NAME)?.objects.has("preexisting-user-data.txt")).toBe(true);
  });

  it("deletes the Worker, its Durable Object namespaces, the objects and the bucket whose identity this run recorded", async () => {
    mock.addWorker(NAME, undefined, "this-runs-tag");
    const bucket = mock.addBucket(NAME, undefined, ["spaces/a/chunks/aa/aa/1", "spaces/a/meta/h/space.json"]);
    writeState({ owned: { workerTag: "this-runs-tag", bucketCreationDate: bucket.creationDate } });
    const { status, output } = await runTs("teardown");
    expect(output).not.toContain("::error::");
    expect(status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.namespaces).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it("leaves a Worker and a bucket alone that replaced the ones this run created", async () => {
    mock.addWorker(NAME, "someone-elses-worker");
    mock.addBucket(NAME, "2026-02-02T00:00:00.000Z", ["theirs.txt"]);
    writeState({ owned: { workerTag: "this-runs-tag", bucketCreationDate: "2026-01-01T00:00:00.000Z" } });
    const { status } = await runTs("teardown");
    expect(status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });
});

describe("reading what the API lists late", () => {
  const account = { id: "dummy-account", token: "dummy-token" };

  async function withFetch<T>(fetchStub: (url: string) => Promise<Response>, read: () => Promise<T>, advanceMs: number): Promise<T> {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", fetchStub);
    const timeout = vi.spyOn(AbortSignal, "timeout").mockImplementation((ms) => {
      const controller = new AbortController();
      setTimeout(() => controller.abort(new DOMException("The operation was aborted due to timeout", "TimeoutError")), ms);
      return controller.signal;
    });
    try {
      const result = read();
      await vi.advanceTimersByTimeAsync(advanceMs);
      return await result;
    } finally {
      timeout.mockRestore();
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  }

  it("reads the deployments again until the new version takes all traffic", async () => {
    const deployment = { created_on: "2026-11-12T10:00:00Z", versions: [{ version_id: "v2", percentage: 100 }] };
    const listings = [[], [{ created_on: "2026-11-12T09:00:00Z", versions: [{ version_id: "v1", percentage: 100 }] }]];
    const fetchStub = vi.fn(async () => Response.json({ result: { deployments: listings.shift() ?? [deployment] } }));
    expect(await withFetch(fetchStub, () => newestDeployment(account, NAME, "v2"), 10_000)).toEqual(deployment);
    expect(fetchStub).toHaveBeenCalledTimes(3);
  });

  it("gives up after 60 s and returns what the newest deployment serves", async () => {
    const split = { created_on: "2026-11-12T10:00:00Z", versions: [{ version_id: "v1", percentage: 50 }, { version_id: "v2", percentage: 50 }] };
    const fetchStub = vi.fn(async () => Response.json({ result: { deployments: [split] } }));
    const newest = await withFetch(fetchStub, () => newestDeployment(account, NAME, "v2"), LISTING_MS);
    expect(newest).toEqual(split);
    expect(serves(newest, "v2")).toBe(false);
    expect(fetchStub).toHaveBeenCalledTimes(12);
  });

  it("reads a new Worker's versions again until they are listed", async () => {
    let emptyListings = 2;
    const fetchStub = vi.fn(async (url: string) =>
      url.includes("/versions/")
        ? Response.json({ result: { id: "v1", annotations: { "workers/tag": "this-runs-tag" } } })
        : Response.json({ result: { items: emptyListings-- > 0 ? [] : [{ id: "v1" }] } }),
    );
    expect(await withFetch(fetchStub, () => workerTag(account, NAME, "this-runs-tag"), 10_000)).toBe("tagged");
    expect(fetchStub).toHaveBeenCalledTimes(4);
  });

  it("tells a listed Worker without the run's tag apart at once, and one whose versions stay unlisted for 60 s", async () => {
    const untagged = vi.fn(async (url: string) =>
      url.includes("/versions/") ? Response.json({ result: { id: "v9", annotations: {} } }) : Response.json({ result: { items: [{ id: "v9" }] } }),
    );
    expect(await withFetch(untagged, () => workerTag(account, NAME, "this-runs-tag"), 0)).toBe("untagged");
    expect(untagged).toHaveBeenCalledTimes(2);
    const unlisted = vi.fn(async () => Response.json({ result: { items: [] } }));
    expect(await withFetch(unlisted, () => workerTag(account, NAME, "this-runs-tag"), LISTING_MS)).toBe("unlisted");
    expect(unlisted).toHaveBeenCalledTimes(12);
  });

  function slowFetch(delayMs: (url: string) => number, body: (url: string) => unknown, started: number[]) {
    return vi.fn(
      (url: string, init?: RequestInit) =>
        new Promise<Response>((resolve, reject) => {
          started.push(Date.now());
          const timer = setTimeout(() => resolve(Response.json(body(url))), delayMs(url));
          init?.signal?.addEventListener("abort", () => {
            clearTimeout(timer);
            reject(init.signal!.reason);
          });
        }),
    );
  }

  it("stops at 60 s when a listing's headers arrive but its body stalls", async () => {
    const stalled = vi.fn(
      async (_url: string, init?: RequestInit) =>
        new Response(
          new ReadableStream({
            start(controller) {
              init?.signal?.addEventListener("abort", () => controller.error(init.signal!.reason));
            },
          }),
        ),
    );
    const read = async <T>(helper: () => Promise<T>) => {
      const startMs = Date.now();
      const value = await helper();
      return { value, tookMs: Date.now() - startMs };
    };
    const tag = await withFetch(stalled, () => read(() => workerTag(account, NAME, "this-runs-tag")), LISTING_MS);
    expect(tag).toEqual({ value: "unlisted", tookMs: LISTING_MS });
    const newest = await withFetch(stalled, () => read(() => newestDeployment(account, NAME, "v2")), LISTING_MS);
    expect(newest).toEqual({ value: undefined, tookMs: LISTING_MS });
    expect(stalled).toHaveBeenCalledTimes(2);
  });

  it("stops reading slow deployments at 60 s and keeps the last listing it got", async () => {
    const split = { created_on: "2026-11-12T10:00:00Z", versions: [{ version_id: "v1", percentage: 50 }, { version_id: "v2", percentage: 50 }] };
    const started: number[] = [];
    const fetchStub = slowFetch(() => 50_000, () => ({ result: { deployments: [split] } }), started);
    const run = await withFetch(
      fetchStub,
      async () => {
        const startMs = Date.now();
        const newest = await newestDeployment(account, NAME, "v2");
        return { newest, tookMs: Date.now() - startMs, startedMs: started.map((at) => at - startMs) };
      },
      LISTING_MS,
    );
    expect(run.newest).toEqual(split);
    expect(run.tookMs).toBeLessThanOrEqual(LISTING_MS);
    expect(run.startedMs).toEqual([0, 55_000]);
  });

  it("stops reading a slow Worker's versions at 60 s and reports them unlisted", async () => {
    const started: number[] = [];
    const fetchStub = slowFetch(
      () => 40_000,
      (url) => (url.includes("/versions/") ? { result: { id: "v1", annotations: { "workers/tag": "this-runs-tag" } } } : { result: { items: [{ id: "v1" }] } }),
      started,
    );
    const run = await withFetch(
      fetchStub,
      async () => {
        const startMs = Date.now();
        const tag = await workerTag(account, NAME, "this-runs-tag");
        return { tag, tookMs: Date.now() - startMs, startedMs: started.map((at) => at - startMs) };
      },
      LISTING_MS,
    );
    expect(run.tag).toBe("unlisted");
    expect(run.tookMs).toBeLessThanOrEqual(LISTING_MS);
    expect(run.startedMs).toEqual([0, 40_000]);
  });
});

describe("deploy", () => {
  it("stops before creating anything when a bucket already has the run's name, and teardown then deletes nothing", async () => {
    mock.addBucket(NAME, "2026-01-01T00:00:00.000Z", ["preexisting-user-data.txt"]);
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("already exists");
    expect(mock.requests.filter((request) => request.startsWith("POST "))).toEqual([]);
    expect(existsSync(join(work, "state.json")) ? readState().owned : {}).toEqual({});
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });

  it("stops before creating anything when a Worker already has the run's name", async () => {
    mock.addWorker(NAME);
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(mock.requests.filter((request) => request.startsWith("POST "))).toEqual([]);
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
  });

  it("records nothing when creating the bucket fails, so teardown leaves the bucket that won the name alone", async () => {
    mock.createBucketStatus = 409;
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(existsSync(join(work, "state.json")) ? readState().owned : {}).toEqual({});
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes()).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(true);
  });

  it("records no Worker when the install fails while another creator takes the name, so teardown deletes nothing", async () => {
    fakePnpm({ install: { createsWorker: true, status: 1 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile"]);
    expect(readState().owned.workerTag).toBeUndefined();
    const id = mock.workers.get(NAME)?.id;
    expect(id).toBeDefined();
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.get(NAME)?.id).toBe(id);
    expect(mock.namespaces.filter((namespace) => namespace.script === NAME)).toHaveLength(2);
  });

  it("does not deploy over a Worker that another creator made while dependencies were installing", async () => {
    fakePnpm({ install: { createsWorker: true, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("appeared before this run deployed");
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile"]);
    expect(readState().owned.workerTag).toBeUndefined();
    expect((await runTs("teardown")).status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.has(NAME)).toBe(true);
  });

  it("records the Worker its own deploy created, even when a later step fails, so teardown deletes it", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy"]);
    expect(deployed.status).toBe(1);
    expect(mock.workers.get(NAME)?.versions[0]?.tag).toBe(readState().owned.workerTag);
    expect(readState().owned.workerTag).toMatch(/^[0-9a-f]{16}$/);
    const tornDown = await runTs("teardown");
    expect(tornDown.output).not.toContain("::error::");
    expect(tornDown.status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.buckets.has(NAME)).toBe(false);
  });
});

describe("a deploy whose upload the API does not list yet", () => {
  it("deploys again with the same tag after a backoff when Wrangler's read after the upload finds no Worker", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0, unlistedFirst: 1 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy", "run deploy"]);
    expect(deployed.output).toContain("did not list the Worker yet (code 10007); deploying again in 10 s");
    expect(mock.workers.get(NAME)?.versions.map((version) => version.tag)).toEqual([readState().owned.workerTag, readState().owned.workerTag]);
  });

  it("gives up after three uploads the API never lists", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0, unlistedFirst: 3 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy", "run deploy", "run deploy"]);
    expect(deployed.output).toContain("deploy test deploy: pnpm run deploy --tag");
  });
});

describe("a later deploy of the same run", () => {
  it("refuses to deploy over a Worker that carries no version of this run's deploy", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 0 } });
    mock.addWorker(NAME, "someone-elses-worker");
    writeState({ owned: { workerTag: "this-runs-tag" }, deployments: [{ ref: "HEAD", sha: "0".repeat(40), dir: server, versionId: "v", atMs: Date.now() }] });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(deployed.output).toContain("is no longer the one this run created");
    expect(pnpmCalls()).toEqual([]);
  });
});

describe("a deploy that fails after creating the Worker", () => {
  it("leaves proof that this run created the Worker, so teardown deletes it, its Durable Objects and the bucket", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 1 } });
    const deployed = await runTs("deploy", "HEAD");
    expect(deployed.status).toBe(1);
    expect(pnpmCalls()).toEqual(["install --frozen-lockfile", "run deploy"]);
    expect(mock.workers.has(NAME)).toBe(true);
    const tornDown = await runTs("teardown");
    expect(tornDown.output).not.toContain("::error::");
    expect(tornDown.status).toBe(0);
    expect(mock.workers.has(NAME)).toBe(false);
    expect(mock.namespaces.filter((namespace) => namespace.script === NAME)).toEqual([]);
    expect(mock.buckets.has(NAME)).toBe(false);
  });

  it("leaves the Worker alone and reports only the error when its versions cannot be read", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, status: 1 } });
    expect((await runTs("deploy", "HEAD")).status).toBe(1);
    mock.versionsStatus = 403;
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(tornDown.output).toContain("::error::teardown: reading the Worker's version tags: GET");
    expect(tornDown.output).not.toContain("none of its versions carries");
    expect(tornDown.output).not.toContain("did not record creating it");
  });

  it("fails teardown loudly and names the Worker to delete by hand when no upload of this run's deploy is on it", async () => {
    fakePnpm({ install: { createsWorker: false, status: 0 }, deploy: { createsWorker: true, tagged: false, status: 1 } });
    expect((await runTs("deploy", "HEAD")).status).toBe(1);
    const tornDown = await runTs("teardown");
    expect(tornDown.status).toBe(1);
    expect(mock.deletes().filter((request) => request.includes("/workers/"))).toEqual([]);
    expect(mock.workers.has(NAME)).toBe(true);
    expect(tornDown.output).toContain("::error::teardown: a Worker named " + NAME);
    expect(tornDown.output).toContain("delete the Worker " + NAME + " by hand");
  });
});

describe("claim", () => {
  function claimState(): void {
    mock.workerName = NAME;
    writeState({ url: mock.url, deployments: [{ ref: "HEAD", sha: "0".repeat(40), dir: server, versionId: "v", atMs: Date.now() }] });
  }

  it("claims with the code from the Worker's logs and leaves the code in no file and no output", async () => {
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
    expect(readState().owner?.userId).toBe("user-1");
    expect(output).toContain("filtered on $metadata.service");
    const [, eventMs, nowMs] = /with event timestamp (\d+) \(Date\.now\(\) (\d+)\)/.exec(output) ?? [];
    expect(Math.abs(Number(nowMs) - Number(eventMs))).toBeLessThan(60_000);
    expect(mock.queryDryFlags.length).toBeGreaterThan(0);
    expect(mock.queryDryFlags.every((dry) => dry === true)).toBe(true);
    expect(output).not.toContain(mock.claimCode);
    expect(filesContaining(work, mock.claimCode)).toEqual([]);
  });

  it("filters on $workers.scriptName when a query filtered on $metadata.service does not show the probe request", async () => {
    mock.filterKeys = ["$workers.scriptName"];
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("filtered on $workers.scriptName");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
  });

  it("tries $workers.scriptName when the API rejects a filter on $metadata.service", async () => {
    mock.rejectedKeys = ["$metadata.service"];
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("filtered on $workers.scriptName");
    expect(status).toBe(0);
  });

  it("keeps querying the logs until the code arrives, and opens /setup once", async () => {
    mock.codeLagQueries = 3;
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
    expect(mock.setupRequests).toBe(1);
    expect(mock.logQueries).toBe(5);
  });

  it("re-sends the probe while its request has not reached the logs, then claims", async () => {
    mock.probeLagQueries = 6;
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
    expect(mock.probeRequests).toBeGreaterThan(1);
    expect(mock.setupRequests).toBe(1);
  });

  it("ignores a claim code another Worker logged", async () => {
    mock.logs.push({ script: "koidrive-test-2-1", message: "Koidrive claim code (valid for 1 hour): AAAA-BBBB-CCCC", timestamp: Date.now() });
    claimState();
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
  });

  it("finds a code an earlier claim attempt had /setup issue minutes before", async () => {
    claimState();
    writeState({ ...readState(), deployments: [{ ...readState().deployments[0]!, atMs: Date.now() - 10 * 60_000 }] });
    mock.setupRequests = 1;
    mock.logs.push({ script: NAME, message: "Koidrive claim code (valid for 1 hour): " + mock.claimCode, timestamp: Date.now() - 5 * 60_000 });
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
  });

  it("skips a code /setup issued more than an hour ago and redeems the one it issues now", async () => {
    claimState();
    writeState({ ...readState(), deployments: [{ ...readState().deployments[0]!, atMs: Date.now() - 80 * 60_000 }] });
    mock.logs.push({ script: NAME, message: "Koidrive claim code (valid for 1 hour): AAAA-BBBB-CCCC", timestamp: Date.now() - 70 * 60_000 });
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
  });

  it("finds a live code /setup issued 58 minutes ago, past an expired one", async () => {
    claimState();
    writeState({ ...readState(), deployments: [{ ...readState().deployments[0]!, atMs: Date.now() - 130 * 60_000 }] });
    mock.setupRequests = 1;
    mock.logs.push(
      { script: NAME, message: "Koidrive claim code (valid for 1 hour): AAAA-BBBB-CCCC", timestamp: Date.now() - 120 * 60_000 },
      { script: NAME, message: "Koidrive claim code (valid for 1 hour): " + mock.claimCode, timestamp: Date.now() - 58 * 60_000 },
    );
    const { status, output } = await runTs("claim");
    expect(output).toContain("Claimed " + mock.url);
    expect(status).toBe(0);
  });

  it("opens no /setup and spends no code when the token cannot query the logs", async () => {
    mock.observabilityStatus = 403;
    claimState();
    const { status, output } = await runTs("claim");
    expect(status).toBe(1);
    expect(output).toContain("/setup was not opened and no claim code was spent");
    expect(output).toContain("the token lacks a permission this call needs");
    expect(mock.setupRequests).toBe(0);
  });

  it("leaves the code in no file and no output when the claim fails", async () => {
    mock.claimStatus = 403;
    claimState();
    const { status, output } = await runTs("claim");
    expect(status).toBe(1);
    expect(output).toContain("the claim failed with HTTP 403");
    expect(output).not.toContain(mock.claimCode);
    expect(filesContaining(work, mock.claimCode)).toEqual([]);
  });
});
