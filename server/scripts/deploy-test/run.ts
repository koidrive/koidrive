// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { appendFileSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  type Account,
  ApiError,
  type Deadline,
  DeadlineError,
  LISTING_MS,
  newestDeployment,
  createBucket,
  deleteBucket,
  deleteObject,
  deleteWorker,
  emptyBucket,
  getBucket,
  getWorker,
  graphql,
  listObjects,
  type Namespace,
  namespacesOf,
  requireAccount,
  serves,
  WORKER_KEYS,
  workerEvents,
  type WorkerKey,
  workerTag,
} from "./cloudflare.ts";
import { readState, type State, testName, writeState } from "./state.ts";

const INSTALL_MS = 5 * 60_000;
const DEPLOY_MS = 5 * 60_000;
const SERVING_MS = 3 * 60_000;
const CLAIM_LOG_MS = 10 * 60_000;
const CLAIM_LOG_EVERY_MS = 5_000;
const CLAIM_PROGRESS_EVERY_MS = 60_000;
const CLOCK_SKEW_MS = 60_000;
const CODE_LIFETIME_MS = 60 * 60_000;
const DEPLOY_ATTEMPTS = 3;
const DEPLOY_RETRY_MS = 10_000;
const PROPAGATION_MS = 15_000;
/** The first commit whose server/ deploys on its own (#38); an upgrade cannot start earlier. */
const FIRST_DEPLOYABLE = "8c9bf4a";
const CLAIM_CODE = /Koidrive claim code[^:]*: ([A-Z0-9-]+)/;

const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

class Failed extends Error {}

class Exited extends Failed {
  readonly output: string;
  constructor(message: string, output: string) {
    super(message);
    this.output = output;
  }
}

function fail(message: string): never {
  throw new Failed(message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function git(args: string[]): string {
  return execFileSync("git", args, { cwd: repo, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function tryGit(args: string[]): string | undefined {
  try {
    return git(args);
  } catch {
    return undefined;
  }
}

function withoutSecrets(): NodeJS.ProcessEnv {
  const env = { ...process.env };
  delete env.CLOUDFLARE_API_TOKEN;
  delete env.CLOUDFLARE_ACCOUNT_ID;
  return env;
}

function run(command: string, args: string[], cwd: string, timeoutMs: number, env: NodeJS.ProcessEnv): Promise<string> {
  console.log("$ " + [command, ...args].join(" "));
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd, env: { ...env, CI: "true", WRANGLER_SEND_METRICS: "false" }, detached: true });
    let output = "";
    const take = (chunk: Buffer) => {
      output += chunk;
      process.stdout.write(chunk);
    };
    child.stdout.on("data", take);
    child.stderr.on("data", take);
    const timer = setTimeout(() => {
      try {
        process.kill(-child.pid!, "SIGKILL");
      } catch {}
      reject(new Failed(command + " " + args.join(" ") + " was killed after " + timeoutMs / 1000 + " s"));
    }, timeoutMs);
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(new Failed("could not start " + command + ": " + error.message));
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) resolve(output);
      else reject(new Exited(command + " " + args.join(" ") + " exited with " + code, output));
    });
  });
}

function output(name: string, value: string): void {
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, name + "=" + value + "\n");
}

function upgradeFrom(input: string): void {
  const head = git(["rev-parse", "HEAD"]);
  let ref = input.trim();
  if (!ref) {
    ref = tryGit(["describe", "--tags", "--abbrev=0", "HEAD^"]) ?? "";
    if (!ref) {
      fail(
        "there is no release tag before " + head.slice(0, 12) + ", so there is no previous release to upgrade from. " +
          "Run the upgrade mode with from_ref set to an earlier commit, at or after " + FIRST_DEPLOYABLE + " (#38), the first whose server/ deploys on its own.",
      );
    }
    console.log("No from_ref given: upgrading from the newest tag before HEAD, " + ref);
  }
  const sha = tryGit(["rev-parse", "--verify", "--quiet", ref + "^{commit}"]) ?? fail("from_ref " + JSON.stringify(ref) + " is not a commit in this repository");
  if (sha === head) fail("from_ref " + ref + " is the commit under test; pick an earlier one");
  if (tryGit(["merge-base", "--is-ancestor", sha, head]) === undefined) {
    fail("from_ref " + ref + " (" + sha.slice(0, 12) + ") is not an ancestor of " + head.slice(0, 12) + "; an upgrade only moves forward");
  }
  const pkg = tryGit(["show", sha + ":server/package.json"]);
  const deployScript = pkg ? (JSON.parse(pkg) as { scripts?: Record<string, string> }).scripts?.deploy : undefined;
  if (tryGit(["cat-file", "-e", sha + ":server/pnpm-lock.yaml"]) === undefined || deployScript !== "wrangler deploy") {
    fail("server/ at " + sha.slice(0, 12) + " does not deploy on its own (no lockfile or no deploy script); pick a commit at or after " + FIRST_DEPLOYABLE);
  }
  console.log("Upgrade from " + ref + " (" + sha + ") to " + head);
  output("sha", sha);
}

function traceSampling(): number | undefined {
  const raw = process.env.KOIDRIVE_TRACE_SAMPLING ?? "";
  if (raw === "") return undefined;
  const rate = Number(raw);
  if (!(rate > 0 && rate <= 1)) fail("KOIDRIVE_TRACE_SAMPLING must be a number above 0 and at most 1, or empty for no traces, not " + JSON.stringify(raw));
  return rate;
}

function configure(dir: string, name: string, traces: number | undefined): void {
  const path = join(dir, "wrangler.jsonc");
  let config: { name?: string; r2_buckets?: { binding: string; bucket_name?: string }[]; observability?: Record<string, unknown> };
  try {
    config = JSON.parse(readFileSync(path, "utf8").replace(/^\s*\/\/.*$/gm, ""));
  } catch (error) {
    fail("cannot read " + path + " as JSON with line comments: " + (error instanceof Error ? error.message : String(error)));
  }
  const bucket = config.r2_buckets?.find((entry) => entry.binding === "BUCKET") ?? fail("wrangler.jsonc has no R2 binding named BUCKET");
  config.name = name;
  bucket.bucket_name = name;
  if (traces !== undefined) config.observability = { ...config.observability, traces: { enabled: true, head_sampling_rate: traces } };
  writeFileSync(path, JSON.stringify(config, null, 2) + "\n");
}

async function serving(url: string): Promise<{ serverVersion: string; claimed: boolean }> {
  const deadline = Date.now() + SERVING_MS;
  let last = "no response";
  for (;;) {
    try {
      const response = await fetch(url + "/v1/server", { headers: { Accept: "application/json" }, signal: AbortSignal.timeout(10_000) });
      if (response.ok) return (await response.json()) as { serverVersion: string; claimed: boolean };
      last = "HTTP " + response.status;
      await response.body?.cancel();
    } catch (error) {
      last = error instanceof Error ? error.message : String(error);
    }
    if (Date.now() > deadline) fail(url + "/v1/server did not answer within " + SERVING_MS / 1000 + " s (last: " + last + ")");
    await sleep(3000);
  }
}

/**
 * Wrangler sends the tag with the upload that creates the Worker, so the Worker carries it even if the deploy fails
 * afterwards.
 */
async function deploy(account: Account, ref: string): Promise<void> {
  const state = readState();
  const sha = tryGit(["rev-parse", "--verify", "--quiet", ref + "^{commit}"]) ?? fail(JSON.stringify(ref) + " is not a commit in this repository");
  const traces = traceSampling();
  const first = state.deployments.length === 0;
  if (first) {
    if (state.owned.workerTag || state.owned.bucketCreationDate) fail("the state file already records resources but no deployment; start a new run");
    const worker = await getWorker(account, state.name);
    if (worker) fail("a Worker named " + state.name + " already exists (id " + worker.id + "); this run did not create it and leaves it alone");
    if (await getBucket(account, state.name)) fail("an R2 bucket named " + state.name + " already exists; this run did not create it and leaves it alone");
    try {
      await createBucket(account, state.name);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("10042")) fail("R2 is not activated for this account. Activate it in the dashboard first (docs/deploy.md, What you need): " + message);
      throw error;
    }
    state.owned.bucketCreationDate = (await getBucket(account, state.name) ?? fail("the bucket " + state.name + " was created but cannot be read back")).creationDate;
    writeState(state);
    console.log("Created the R2 bucket " + state.name + " (created " + state.owned.bucketCreationDate + ")");
  } else if (!state.owned.workerTag || (await workerTag(account, state.name, state.owned.workerTag)) !== "tagged") {
    fail("the Worker " + state.name + " is no longer the one this run created; refusing to deploy over it");
  }

  const dir = mkdtempSync(join(tmpdir(), "koidrive-deploy-test-"));
  execFileSync("sh", ["-c", 'git archive --format=tar "$1:server" | tar -x -C "$2"', "sh", sha, dir], { cwd: repo, stdio: "inherit" });
  configure(dir, state.name, traces);
  console.log("Copied server/ at " + sha + " to " + dir + " as Worker and R2 bucket " + state.name + (traces === undefined ? "" : ", tracing " + traces * 100 + "% of requests"));

  await run("pnpm", ["install", "--frozen-lockfile"], dir, INSTALL_MS, withoutSecrets());
  if (first) {
    const worker = await getWorker(account, state.name);
    if (worker) fail("a Worker named " + state.name + " appeared before this run deployed (id " + worker.id + "); this run did not create it and leaves it alone");
    state.owned.workerTag = randomBytes(8).toString("hex");
    writeState(state);
  }
  let deployed: string;
  for (let attempt = 1; ; attempt++) {
    try {
      deployed = await run("pnpm", ["run", "deploy", "--tag", state.owned.workerTag!], dir, DEPLOY_MS, process.env);
      break;
    } catch (error) {
      const unlisted = error instanceof Exited && error.output.includes("Uploaded " + state.name) && error.output.includes("[code: 10007]");
      if (!unlisted || attempt >= DEPLOY_ATTEMPTS) throw error;
      const waitMs = DEPLOY_RETRY_MS * 3 ** (attempt - 1);
      console.log("Wrangler uploaded " + state.name + ", but the API did not list the Worker yet (code 10007); deploying again in " + waitMs / 1000 + " s");
      await sleep(waitMs);
    }
  }
  const url = /https:\/\/[a-z0-9.-]+\.workers\.dev/.exec(deployed)?.[0];
  if (!url) fail("wrangler deploy printed no workers.dev address; the account needs a workers.dev subdomain (docs/deploy-test.md)");
  const versionId = /Current Version ID: ([0-9a-f-]{36})/.exec(deployed)?.[1] ?? fail("wrangler deploy printed no version ID");
  const newest = await newestDeployment(account, state.name, versionId);
  if (!serves(newest, versionId)) {
    const listedAs = newest ? "sends " + newest.versions.map((version) => version.percentage + "% to " + version.version_id).join(" and ") : "is not listed";
    fail("after " + LISTING_MS / 1000 + " s the newest deployment of " + state.name + " " + listedAs + ", not 100% to " + versionId);
  }
  console.log("Deployment of " + state.name + " sends all traffic to version " + versionId);
  state.url = url;
  state.traceSampling = traces;
  state.deployments.push({ ref, sha, dir, versionId, atMs: Date.now() });
  writeState(state);

  if (state.deployments.length > 1) {
    console.log("Waiting " + PROPAGATION_MS / 1000 + " s for the new version to reach the edge");
    await sleep(PROPAGATION_MS);
  }
  const info = await serving(url);
  console.log(url + " serves Koidrive " + info.serverVersion + (info.claimed ? ", claimed" : ", not claimed yet"));
}

async function provenKey(account: Account, name: string, fromMs: number, marker: string, deadline: Deadline): Promise<{ key: WorkerKey; timestamp: unknown } | undefined> {
  for (const key of WORKER_KEYS) {
    let events: { timestamp: unknown; text: string }[];
    try {
      events = await workerEvents(account, key, name, fromMs, deadline);
    } catch (error) {
      if (error instanceof ApiError && error.status === 400) continue;
      throw error;
    }
    const probe = events.find((event) => event.text.includes(marker));
    if (probe) return { key, timestamp: probe.timestamp };
  }
  return undefined;
}

async function claimCode(account: Account, key: WorkerKey, name: string, fromMs: number, deadline: Deadline): Promise<string | undefined> {
  const live = Date.now() - CODE_LIFETIME_MS;
  const codes = (await workerEvents(account, key, name, fromMs, deadline))
    .map((event) => ({ atMs: typeof event.timestamp === "number" ? event.timestamp : Infinity, code: CLAIM_CODE.exec(event.text)?.[1] }))
    .filter((event) => event.code !== undefined && event.atMs > live);
  return codes.sort((a, b) => b.atMs - a.atMs)[0]?.code;
}

async function claim(account: Account): Promise<void> {
  const state = readState();
  const first = state.deployments[0] ?? fail("nothing is deployed yet");
  const url = state.url!;
  const fromMs = first.atMs - CLOCK_SKEW_MS;
  const marker = "/deploy-test-probe-" + randomBytes(8).toString("hex");
  const sendProbe = () => fetch(url + marker, { signal: AbortSignal.timeout(15_000) }).then((response) => response.body?.cancel(), () => undefined);
  const startedMs = Date.now();
  let reportedMs = startedMs;
  const progress = (what: string) => {
    if (Date.now() - reportedMs < CLAIM_PROGRESS_EVERY_MS) return;
    reportedMs = Date.now();
    console.log("Still waiting for " + what + " in this Worker's logs after " + Math.round((reportedMs - startedMs) / 1000) + " s");
  };
  await sendProbe();
  const probe = { atMs: Date.now() + CLAIM_LOG_MS, what: "no query of this Worker's logs showed its request to " + marker + " within " + CLAIM_LOG_MS / 1000 + " s" };
  let proven: { key: WorkerKey; timestamp: unknown } | undefined;
  let probes = 1;
  try {
    while (!(proven = await provenKey(account, state.name, fromMs, marker, probe))) {
      if (Date.now() > probe.atMs) throw new DeadlineError(probe.what + " (" + probes + " probe requests sent)");
      progress("the request to " + marker);
      await sleep(CLAIM_LOG_EVERY_MS);
      await sendProbe();
      probes++;
    }
  } catch (error) {
    if (!(error instanceof ApiError)) throw error;
    fail(error.message + ", so /setup was not opened and no claim code was spent");
  }
  const { key } = proven;
  console.log("Workers Logs shows this Worker's request to " + marker + " when filtered on " + key + ", with event timestamp " + JSON.stringify(proven.timestamp) + " (Date.now() " + Date.now() + ")");
  const setup = await fetch(url + "/setup", { signal: AbortSignal.timeout(15_000) });
  const page = await setup.text();
  if (!setup.ok || !page.includes("claim code")) fail("/setup did not issue a claim code (HTTP " + setup.status + "): " + page.slice(0, 200));
  const wait = { atMs: Date.now() + CLAIM_LOG_MS, what: "the claim code did not appear in this Worker's logs within " + CLAIM_LOG_MS / 1000 + " s" };
  let code: string | undefined;
  while (!(code = await claimCode(account, key, state.name, fromMs, wait))) {
    if (Date.now() > wait.atMs) fail(wait.what);
    progress("the claim code");
    await sleep(CLAIM_LOG_EVERY_MS);
  }
  console.log("Read the claim code from this Worker's logs");
  const response = await fetch(url + "/v1/claim", {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({ code, userName: "Deploy test owner", deviceName: "deploy-test owner", platform: "linux" }),
    signal: AbortSignal.timeout(15_000),
  });
  const body = (await response.json()) as { userId?: string; deviceId?: string; refreshCredential?: string; message?: string };
  if (!response.ok || !body.userId || !body.deviceId || !body.refreshCredential) {
    fail("the claim failed with HTTP " + response.status + ": " + (body.message ?? "no message").replaceAll(code, "<claim code>"));
  }
  state.owner = { userId: body.userId, deviceId: body.deviceId, refreshCredential: body.refreshCredential };
  state.logKey = key;
  writeState(state);
  console.log("Claimed " + url + ": owner " + body.userId + ", first device " + body.deviceId);
}

interface Storage {
  objects: number;
  bytes: number;
  chunkObjects: number;
  chunkBytes: number;
  exportObjects: number;
  exportBytes: number;
}

async function emptyAndDeleteBucket(account: Account, name: string): Promise<Storage | undefined> {
  const first = await listObjects(account, name);
  if (!first) return undefined;
  const storage: Storage = { objects: 0, bytes: 0, chunkObjects: 0, chunkBytes: 0, exportObjects: 0, exportBytes: 0 };
  for (const object of first) {
    storage.objects++;
    storage.bytes += object.size;
    if (/^spaces\/[^/]+\/chunks\//.test(object.key)) {
      storage.chunkObjects++;
      storage.chunkBytes += object.size;
    } else if (/^spaces\/[^/]+\/meta\//.test(object.key)) {
      storage.exportObjects++;
      storage.exportBytes += object.size;
    }
  }
  let objects = first;
  if (first.length > 1000) {
    await emptyBucket(account, name);
    objects = (await listObjects(account, name)) ?? [];
  }
  for (let attempt = 1; ; attempt++) {
    for (let i = 0; i < objects.length; i += 8) await Promise.all(objects.slice(i, i + 8).map((object) => deleteObject(account, name, object.key)));
    try {
      await deleteBucket(account, name);
      return storage;
    } catch (error) {
      if (attempt >= 5) throw error;
      await sleep(5000 * attempt);
      objects = (await listObjects(account, name)) ?? [];
    }
  }
}

function bytes(n: number): string {
  return n < 1024 ? n + " B" : n < 1024 * 1024 ? (n / 1024).toFixed(1) + " KiB" : (n / 1024 / 1024).toFixed(2) + " MiB";
}

async function analytics(account: Account, state: State | undefined, namespaces: Namespace[]): Promise<[string, string][]> {
  const start = new Date((state?.deployments[0]?.atMs ?? Date.now() - 3_600_000) - 60_000).toISOString();
  const end = new Date().toISOString();
  const vars = { account: account.id, script: state?.name ?? testName(), start, end, day: start.slice(0, 10), ids: namespaces.map((namespace) => namespace.id) };
  const accounts = (inner: string, args: string) =>
    `query (${args}) { viewer { accounts(filter: { accountTag: $account }) { ${inner} } } }`;
  const queries: [string, string, string, (data: unknown) => string][] = [
    [
      "Worker requests, subrequests, errors",
      "$account: string!, $script: string!, $start: Time!, $end: Time!",
      "workersInvocationsAdaptive(limit: 10000, filter: { scriptName: $script, datetime_geq: $start, datetime_leq: $end }) { sum { requests subrequests errors } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { workersInvocationsAdaptive: { sum: { requests: number; subrequests: number; errors: number } }[] }[] } }).viewer.accounts[0]!.workersInvocationsAdaptive;
        const sum = rows.reduce((a, row) => ({ r: a.r + row.sum.requests, s: a.s + row.sum.subrequests, e: a.e + row.sum.errors }), { r: 0, s: 0, e: 0 });
        return sum.r + " requests, " + sum.s + " subrequests, " + sum.e + " errors";
      },
    ],
    [
      "Durable Object requests",
      "$account: string!, $ids: [string!], $start: Time!, $end: Time!",
      "durableObjectsInvocationsAdaptiveGroups(limit: 10000, filter: { namespaceId_in: $ids, datetime_geq: $start, datetime_leq: $end }) { sum { requests } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { durableObjectsInvocationsAdaptiveGroups: { sum: { requests: number } }[] }[] } }).viewer.accounts[0]!.durableObjectsInvocationsAdaptiveGroups;
        return rows.reduce((a, row) => a + row.sum.requests, 0) + " requests";
      },
    ],
    [
      "Durable Object storage of the whole account (largest sample)",
      "$account: string!, $day: Date!",
      "durableObjectsStorageGroups(limit: 1000, filter: { date_geq: $day }) { max { storedBytes } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { durableObjectsStorageGroups: { max: { storedBytes: number } }[] }[] } }).viewer.accounts[0]!.durableObjectsStorageGroups;
        return rows.length === 0 ? "no sample yet" : bytes(Math.max(...rows.map((row) => row.max.storedBytes)));
      },
    ],
    [
      "R2 operations",
      "$account: string!, $script: string!, $start: Time!, $end: Time!",
      "r2OperationsAdaptiveGroups(limit: 10000, filter: { bucketName: $script, datetime_geq: $start, datetime_leq: $end }) { sum { requests } dimensions { actionType } }",
      (data) => {
        const rows = (data as { viewer: { accounts: { r2OperationsAdaptiveGroups: { sum: { requests: number }; dimensions: { actionType: string } }[] }[] } }).viewer.accounts[0]!.r2OperationsAdaptiveGroups;
        return rows.length === 0 ? "none recorded yet" : rows.map((row) => row.dimensions.actionType + " " + row.sum.requests).join(", ");
      },
    ],
  ];
  const rows: [string, string][] = [];
  for (const [label, args, inner, read] of queries) {
    try {
      const query = accounts(inner, args);
      const used = Object.fromEntries(Object.entries(vars).filter(([key]) => args.includes("$" + key + ":")));
      rows.push([label + " (Analytics API)", read(await graphql(account, query, used))]);
    } catch (error) {
      rows.push([label + " (Analytics API)", "unavailable: " + (error instanceof Error ? error.message : String(error)).slice(0, 200)]);
    }
  }
  return rows;
}

async function teardown(account: Account): Promise<void> {
  const name = testName();
  const state = readState();
  const problems: string[] = [];
  const attempt = async <T>(what: string, step: () => Promise<T>): Promise<T | undefined> => {
    try {
      return await step();
    } catch (error) {
      problems.push(what + ": " + (error instanceof Error ? error.message : String(error)));
      return undefined;
    }
  };

  let namespaces: Namespace[] = [];
  const worker = await attempt("reading the Worker", () => getWorker(account, name));
  const tag = state.owned.workerTag;
  const found = worker && tag ? await attempt("reading the Worker's version tags", () => workerTag(account, name, tag)) : undefined;
  if (worker && found === "tagged") {
    namespaces = (await attempt("listing the Durable Object namespaces", () => namespacesOf(account, name))) ?? [];
    const deleted = await attempt("deleting the Worker", () => deleteWorker(account, name));
    if (deleted) console.log("Deleted the Worker " + name + " (id " + worker.id + "), which carries the tag of this run's deploy");
    const left = await attempt("checking the Durable Object namespaces", () => namespacesOf(account, name));
    if (left?.length) problems.push("Durable Object namespaces of " + name + " remain: " + left.map((namespace) => namespace.class + " " + namespace.id).join(", "));
    else if (left) console.log("No Durable Object namespace of " + name + " remains (" + (namespaces.map((namespace) => namespace.class).join(", ") || "none") + " deleted with the Worker)");
  } else if (worker && (found === "untagged" || found === "unlisted")) {
    problems.push(
      "a Worker named " + name + " (id " + worker.id + ") exists, but " + (found === "untagged" ? "none of its versions carries the tag of this run's deploy" : "it listed no versions within " + LISTING_MS / 1000 + " s") +
        ", so this run cannot prove it created the Worker and left it alone. If this run's deploy created it, delete the Worker " + name + " by hand under Workers & Pages, which also deletes its Durable Objects",
    );
  } else if (worker && !tag) {
    problems.push("a Worker named " + name + " (id " + worker.id + ") exists, but this run did not record creating it, so it was left alone");
  } else if (!worker && tag) {
    console.log("No Worker named " + name + " exists, so this run's deploy left none to delete");
  }

  let storage: Storage | undefined;
  const bucket = await attempt("reading the R2 bucket", () => getBucket(account, name));
  if (bucket && bucket.creationDate === state.owned.bucketCreationDate) {
    storage = await attempt("emptying and deleting the R2 bucket", () => emptyAndDeleteBucket(account, name));
    if (storage) console.log("Deleted the R2 bucket " + name + " and its " + storage.objects + " objects");
  } else if (bucket) {
    problems.push("an R2 bucket named " + name + " (created " + bucket.creationDate + ") exists, but this run did not record creating it, so it was left alone");
  } else if (state.owned.bucketCreationDate) {
    console.log("The R2 bucket " + name + " this run created is already gone");
  }
  if (!state.owned.workerTag && !state.owned.bucketCreationDate) console.log("This run created nothing, so teardown deleted nothing");

  const deployments = state.deployments;
  const rows: [string, string][] = [
    ["Worker and bucket", name],
    ["Deployed", deployments.map((deployment) => deployment.ref + " (" + deployment.sha.slice(0, 12) + ", version " + deployment.versionId + ")").join(", then ") || "nothing"],
    ["Test client requests", state.stats.requests + " HTTPS and WebSocket requests, " + bytes(state.stats.bytesUp) + " sent, " + bytes(state.stats.bytesDown) + " received"],
    ["R2 storage at teardown", storage ? storage.objects + " objects, " + bytes(storage.bytes) : "no bucket deleted"],
    ["  chunks", storage ? storage.chunkObjects + " objects, " + bytes(storage.chunkBytes) : "-"],
    ["  metadata export", storage ? storage.exportObjects + " objects, " + bytes(storage.exportBytes) : "-"],
    ["Durable Object namespaces", namespaces.map((namespace) => namespace.class).join(", ") || "none deleted"],
  ];
  if (deployments.length > 0) {
    console.log("Waiting 60 s for the Analytics API to catch up");
    await sleep(60_000);
    rows.push(...(await analytics(account, state, namespaces)));
  }
  const width = Math.max(...rows.map(([label]) => label.length));
  console.log("\nCost-relevant usage of this run (Analytics API figures can lag by several minutes):");
  for (const [label, value] of rows) console.log("  " + label.padEnd(width) + "  " + value);
  if (process.env.GITHUB_STEP_SUMMARY) {
    const table = ["| Usage | |", "|---|---|", ...rows.map(([label, value]) => "| " + label.trim() + " | " + value.replaceAll("|", "\\|") + " |")];
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, "### Deploy test usage\n\nAnalytics API figures can lag by several minutes.\n\n" + table.join("\n") + "\n");
  }

  if (problems.length > 0) {
    for (const problem of problems) console.error("::error::teardown: " + problem);
    fail("teardown could not delete everything this run created, or found resources it did not create; check the Worker and R2 bucket named " + name + " in the dashboard");
  }
}

const [command, arg = ""] = process.argv.slice(2);
try {
  if (command === "upgrade-from") upgradeFrom(arg);
  else if (command === "deploy") await deploy(requireAccount(), arg || "HEAD");
  else if (command === "claim") await claim(requireAccount());
  else if (command === "teardown") await teardown(requireAccount());
  else fail("usage: run.ts upgrade-from [<ref>] | deploy <ref> | claim | teardown");
} catch (error) {
  console.error("::error::deploy test " + command + ": " + (error instanceof Error ? error.message : String(error)));
  process.exitCode = 1;
}
