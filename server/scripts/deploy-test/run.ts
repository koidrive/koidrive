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
  deleteNamespace,
  deleteObject,
  deleteWorker,
  EXPIRY_SECONDS,
  expireAllObjects,
  getBucket,
  getWorker,
  graphql,
  listBuckets,
  listNamespaces,
  listObjectPage,
  listWorkers,
  type Namespace,
  namespacesOf,
  type R2Object,
  requireAccount,
  serves,
  WORKER_KEYS,
  workerEvents,
  type WorkerKey,
  workerTag,
} from "./cloudflare.ts";
import { CHUNK_PACKS, configuredFeatures } from "../../src/features.ts";
import { NAME_PREFIX, readState, type State, testName, writeState } from "./state.ts";

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

function features(): string | undefined {
  const raw = process.env.KOIDRIVE_FEATURES ?? "";
  if (raw === "") return undefined;
  if (!raw.split(",").every((entry) => /^-?[a-z][a-z0-9_]{0,63}$/.test(entry))) {
    fail("KOIDRIVE_FEATURES must be feature names separated by commas, each with an optional leading -, not " + JSON.stringify(raw));
  }
  return raw;
}

function configure(dir: string, name: string, traces: number | undefined, featureList: string | undefined): void {
  const path = join(dir, "wrangler.jsonc");
  let config: { name?: string; r2_buckets?: { binding: string; bucket_name?: string }[]; observability?: Record<string, unknown>; vars?: Record<string, string> };
  try {
    config = JSON.parse(readFileSync(path, "utf8").replace(/^\s*\/\/.*$/gm, ""));
  } catch (error) {
    fail("cannot read " + path + " as JSON with line comments: " + (error instanceof Error ? error.message : String(error)));
  }
  const bucket = config.r2_buckets?.find((entry) => entry.binding === "BUCKET") ?? fail("wrangler.jsonc has no R2 binding named BUCKET");
  config.name = name;
  bucket.bucket_name = name;
  if (traces !== undefined) config.observability = { ...config.observability, traces: { enabled: true, head_sampling_rate: traces } };
  if (featureList !== undefined) config.vars = { ...config.vars, KOIDRIVE_FEATURES: featureList };
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
  const featureList = features();
  output("chunk_packs", String(configuredFeatures(featureList).has(CHUNK_PACKS)));
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
    await expireAllObjects(account, state.name);
    console.log("Created the R2 bucket " + state.name + " (created " + state.owned.bucketCreationDate + "), whose lifecycle rule deletes each object " + EXPIRY_SECONDS / 3600 + " hours after its upload");
  } else if (!state.owned.workerTag || (await workerTag(account, state.name, state.owned.workerTag)) !== "tagged") {
    fail("the Worker " + state.name + " is no longer the one this run created; refusing to deploy over it");
  }

  const dir = mkdtempSync(join(tmpdir(), "koidrive-deploy-test-"));
  execFileSync("sh", ["-c", 'git archive --format=tar "$1:server" | tar -x -C "$2"', "sh", sha, dir], { cwd: repo, stdio: "inherit" });
  configure(dir, state.name, traces, featureList);
  console.log(
    "Copied server/ at " + sha + " to " + dir + " as Worker and R2 bucket " + state.name +
      (traces === undefined ? "" : ", tracing " + traces * 100 + "% of requests") +
      (featureList === undefined ? "" : ", KOIDRIVE_FEATURES=" + featureList),
  );

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
  state.features = featureList;
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

function described(namespaces: Namespace[]): string {
  return namespaces.map((namespace) => namespace.class + " " + namespace.id).join(", ");
}

async function deleteNamespacesOf(account: Account, script: string): Promise<Namespace[]> {
  const left = await namespacesOf(account, script);
  for (const namespace of left) await deleteNamespace(account, namespace.id);
  const remaining = await namespacesOf(account, script);
  if (remaining.length > 0) throw new Error("Durable Object namespaces of " + script + " remain after deleting them: " + described(remaining));
  return left;
}

function storageOf(objects: R2Object[]): Storage {
  const storage: Storage = { objects: 0, bytes: 0, chunkObjects: 0, chunkBytes: 0, exportObjects: 0, exportBytes: 0 };
  for (const object of objects) {
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
  return storage;
}

async function deleteOrExpireBucket(account: Account, name: string): Promise<Storage | "expiring" | undefined> {
  const page = await listObjectPage(account, name);
  if (!page) return undefined;
  if (!page.next) {
    for (let i = 0; i < page.objects.length; i += 8) await Promise.all(page.objects.slice(i, i + 8).map((object) => deleteObject(account, name, object.key)));
    try {
      await deleteBucket(account, name);
      return storageOf(page.objects);
    } catch (error) {
      if (!(error instanceof ApiError && error.status === 409)) throw error;
    }
  }
  await expireAllObjects(account, name);
  return "expiring";
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
  const worker = await attempt("reading the Worker", async () => (await getWorker(account, name)) ?? null);
  const tag = state.owned.workerTag;
  const found = worker && tag ? await attempt("reading the Worker's version tags", () => workerTag(account, name, tag)) : undefined;
  if (worker && found === "tagged") {
    namespaces = (await attempt("listing the Durable Object namespaces", () => namespacesOf(account, name))) ?? [];
    const deleted = await attempt("deleting the Worker", () => deleteWorker(account, name));
    if (deleted) console.log("Deleted the Worker " + name + " (id " + worker.id + "), which carries the tag of this run's deploy");
    const left = deleted === undefined ? undefined : await attempt("deleting the Durable Object namespaces of " + name, () => deleteNamespacesOf(account, name));
    if (left?.length) console.log("Deleted the Durable Object namespaces " + described(left) + ", which outlived the Worker");
    else if (left) console.log("No Durable Object namespace of " + name + " remains (" + (namespaces.map((namespace) => namespace.class).join(", ") || "none") + " deleted with the Worker)");
  } else if (worker && (found === "untagged" || found === "unlisted")) {
    problems.push(
      "a Worker named " + name + " (id " + worker.id + ") exists, but " + (found === "untagged" ? "none of its versions carries the tag of this run's deploy" : "it listed no versions within " + LISTING_MS / 1000 + " s") +
        ", so this run cannot prove it created the Worker and left it alone. If this run's deploy created it, delete the Worker " + name + " by hand under Workers & Pages, which also deletes its Durable Objects",
    );
  } else if (worker && !tag) {
    problems.push("a Worker named " + name + " (id " + worker.id + ") exists, but this run did not record creating it, so it was left alone");
  } else if (worker === null && tag) {
    console.log("No Worker named " + name + " exists, so this run's deploy left none to delete");
    const left = await attempt("deleting the Durable Object namespaces of " + name, () => deleteNamespacesOf(account, name));
    if (left?.length) console.log("Deleted the Durable Object namespaces " + described(left) + ", which outlived the Worker");
  }

  let storage: Storage | "expiring" | undefined;
  const bucket = await attempt("reading the R2 bucket", () => getBucket(account, name));
  if (bucket && bucket.creationDate === state.owned.bucketCreationDate) {
    storage = await attempt("deleting the R2 bucket", () => deleteOrExpireBucket(account, name));
    if (storage === "expiring") console.log("The R2 bucket " + name + " holds more objects than one listing page. Its lifecycle rule deletes them within a day, and the sweep of a later run deletes the empty bucket");
    else if (storage) console.log("Deleted the R2 bucket " + name + " and its " + storage.objects + " objects");
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
    ["R2 storage at teardown", storage === "expiring" ? "over 1000 objects, left to the bucket's lifecycle rule" : storage ? storage.objects + " objects, " + bytes(storage.bytes) : "no bucket deleted"],
    ["  chunks", storage && storage !== "expiring" ? storage.chunkObjects + " objects, " + bytes(storage.chunkBytes) : "-"],
    ["  metadata export", storage && storage !== "expiring" ? storage.exportObjects + " objects, " + bytes(storage.exportBytes) : "-"],
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

const RUN_NAME = /^koidrive-test-([1-9]\d*)-([1-9]\d*)$/;
const SWEEP_MS = 3 * 60_000;
const LOOKBACK_DAYS = 14;
const RERUN_DAYS = 30;
const NAMESPACE_ID = /^[0-9a-f]{32}$/;
const KNOWN_CLASSES = ["AccountDO", "SpaceDO"];
const TEST_WORKFLOWS = [".github/workflows/deploy-test.yml", ".github/workflows/load-test.yml"];

interface RunAttempt {
  path: string;
  status: string;
  run_started_at: string;
  updated_at: string;
}

class GitHubError extends Error {}

async function github<T>(path: string, what: string, deadline: Deadline): Promise<T | undefined> {
  const api = process.env.GITHUB_API_URL || "https://api.github.com";
  const token = process.env.GH_TOKEN ?? "";
  const remaining = deadline.atMs - Date.now();
  if (remaining <= 0) throw new DeadlineError(deadline.what);
  try {
    const response = await fetch(api + path, {
      headers: { Accept: "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28", ...(token ? { Authorization: "Bearer " + token } : {}) },
      signal: AbortSignal.timeout(Math.min(30_000, remaining)),
    });
    if (response.status === 404) {
      await response.body?.cancel();
      return undefined;
    }
    if (!response.ok) throw new GitHubError("GitHub returned HTTP " + response.status + " for " + what);
    return (await response.json()) as T;
  } catch (error) {
    if (Date.now() >= deadline.atMs) throw new DeadlineError(deadline.what);
    if (error instanceof GitHubError) throw error;
    throw new GitHubError("the GitHub request for " + what + " failed");
  }
}

function runAttempt(repository: string, id: string, attempt: string, deadline: Deadline): Promise<RunAttempt | undefined> {
  return github<RunAttempt>("/repos/" + repository + "/actions/runs/" + id + "/attempts/" + attempt, "run " + id + " attempt " + attempt, deadline);
}

async function finishedRunNames(repository: string, deadline: Deadline): Promise<string[]> {
  const since = Date.now() - LOOKBACK_DAYS * 86_400_000;
  const created = ">=" + new Date(since - RERUN_DAYS * 86_400_000).toISOString().slice(0, 10);
  const names: string[] = [];
  for (const workflow of TEST_WORKFLOWS) {
    const file = workflow.slice(workflow.lastIndexOf("/") + 1);
    for (let page = 1; ; page++) {
      const query = new URLSearchParams({ status: "completed", created, per_page: "100", page: String(page) });
      const what = "the finished " + file + " runs";
      const listed = await github<{ workflow_runs?: { id?: unknown; run_attempt?: unknown; updated_at?: unknown }[] }>("/repos/" + repository + "/actions/workflows/" + file + "/runs?" + query, what, deadline);
      if (!listed) throw new GitHubError("GitHub returned HTTP 404 for " + what);
      const runs = listed.workflow_runs ?? [];
      for (const run of runs) {
        if (typeof run.id !== "number" || typeof run.run_attempt !== "number" || typeof run.updated_at !== "string" || !(Date.parse(run.updated_at) >= since)) continue;
        names.push(NAME_PREFIX + run.id + "-" + run.run_attempt);
        for (let attempt = 1; attempt < run.run_attempt; attempt++) {
          const earlier = await runAttempt(repository, String(run.id), String(attempt), deadline);
          if (earlier?.status === "completed" && Date.parse(earlier.updated_at) >= since) names.push(NAME_PREFIX + run.id + "-" + attempt);
        }
      }
      if (runs.length < 100) break;
    }
  }
  return names;
}

type Verdict = "sweep" | "running" | { leftAlone: string };

function reason(error: unknown): string {
  if (error instanceof ApiError) return error.summary;
  if (error instanceof GitHubError) return error.message;
  return "an unexpected " + (error instanceof Error ? error.name : "error");
}

function label(namespace: Namespace): string {
  return (KNOWN_CLASSES.includes(namespace.class) ? namespace.class : "(a class Koidrive does not define)") + " " + (NAMESPACE_ID.test(namespace.id) ? namespace.id : "(an id that is not 32 hex digits)");
}

function instant(value: string): string | undefined {
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? new Date(ms).toISOString() : undefined;
}

function shown(name: string): string {
  return RUN_NAME.test(name) ? name : "with a name that is not koidrive-test-<run id>-<attempt>";
}

function inventory(kind: string, found: { name: string }[]): string {
  const named = found.filter((item) => RUN_NAME.test(item.name)).map((item) => item.name);
  const other = found.length - named.length;
  return "The sweep listed " + found.length + " " + kind + " named " + NAME_PREFIX + "*" + (found.length ? ": " : "") + [...named, ...(other ? [other + " not named koidrive-test-<run id>-<attempt>"] : [])].join(", ");
}

async function sweep(account: Account): Promise<void> {
  const secrets = [account.token, account.id, process.env.GH_TOKEN ?? ""].filter((secret) => secret.length > 0);
  const redacted = (text: string) => secrets.reduce((line, secret) => line.replaceAll(secret, "***"), text);
  const say = (text: string) => console.log(redacted(text));
  const current = testName();
  const repository = process.env.GITHUB_REPOSITORY || fail("GITHUB_REPOSITORY must name the repository whose runs the sweep checks");
  const budgetMs = Number(process.env.KOIDRIVE_SWEEP_BUDGET_MS || SWEEP_MS);
  const what = "the sweep used its " + budgetMs / 1000 + " s budget";
  let deadline: Deadline = { atMs: 0, what };
  const startPhase = () => (deadline = { atMs: Date.now() + budgetMs / 4, what });
  let unfinished = false;
  const attempts = new Map<string, RunAttempt | undefined>();
  const finishedRun = async (name: string): Promise<RunAttempt | Exclude<Verdict, "sweep">> => {
    if (name === current) return "running";
    const [, id, attempt] = RUN_NAME.exec(name) ?? [];
    if (!id || !attempt) return { leftAlone: "its name is not koidrive-test-<run id>-<attempt>" };
    if (!attempts.has(name)) attempts.set(name, await runAttempt(repository, id, attempt, deadline));
    const run = attempts.get(name);
    if (!run || !TEST_WORKFLOWS.includes(run.path.replace(/@.*$/, ""))) return { leftAlone: "no deploy-test or load-test run of " + repository + " has id " + id + " and attempt " + attempt };
    return run.status === "completed" ? run : "running";
  };
  const createdDuring = async (name: string, createdOn: string | undefined): Promise<Verdict> => {
    const run = await finishedRun(name);
    if (run === "running" || "leftAlone" in run) return run;
    const created = createdOn === undefined ? undefined : instant(createdOn);
    if (!created) return { leftAlone: "the API gave no valid creation time for it" };
    const started = instant(run.run_started_at);
    const ended = instant(run.updated_at);
    if (!started || !ended) return { leftAlone: "GitHub gave no valid start and end time for its run attempt" };
    if (created >= started && created <= ended) return "sweep";
    return { leftAlone: "it was created at " + created + ", outside the run attempt that ran from " + started + " to " + ended };
  };
  const ofFinishedRun = async (script: string): Promise<Verdict> => {
    const run = await finishedRun(script);
    return run === "running" || "leftAlone" in run ? run : "sweep";
  };
  const orphaned = async (script: string): Promise<Verdict> => {
    const found = await ofFinishedRun(script);
    if (found !== "sweep") return found;
    return (await getWorker(account, script, deadline)) ? { leftAlone: "its Worker " + script + " still exists" } : "sweep";
  };
  const problems: string[] = [];
  const leftAlone: string[] = [];
  const visit = async (what: string, decide: () => Promise<Verdict>, remove: () => Promise<void>) => {
    if (Date.now() >= deadline.atMs) {
      unfinished = true;
      return;
    }
    try {
      const found = await decide();
      if (found === "running") say("Skipped the " + what + ": its run is still going");
      else if (found === "sweep") await remove();
      else leftAlone.push("the " + what + ", because " + found.leftAlone);
    } catch (error) {
      failed(what, error);
    }
  };

  const failed = (what: string, error: unknown) => {
    if (error instanceof DeadlineError) unfinished = true;
    else problems.push(what + ": " + reason(error));
  };

  startPhase();
  const workers = await listWorkers(account, deadline).catch((error: unknown) => void failed("listing the Workers", error));
  const live = new Set(workers?.map((worker) => worker.name));
  const testWorkers = workers?.filter((candidate) => candidate.name.startsWith(NAME_PREFIX));
  if (testWorkers) say(inventory("Workers", testWorkers));
  for (const worker of testWorkers ?? []) {
    await visit("Worker " + shown(worker.name), () => createdDuring(worker.name, worker.createdOn), async () => {
      await deleteWorker(account, worker.name, deadline);
      say("Deleted the Worker " + worker.name + " and its Durable Objects, which its finished run left behind");
      live.delete(worker.name);
    });
  }
  startPhase();
  const buckets = await listBuckets(account, NAME_PREFIX, deadline).catch((error: unknown) => void failed("listing the R2 buckets", error));
  if (buckets) say(inventory("R2 buckets", buckets));
  for (const bucket of buckets ?? []) {
    await visit("R2 bucket " + shown(bucket.name), () => createdDuring(bucket.name, bucket.creationDate), async () => {
      try {
        await deleteBucket(account, bucket.name, deadline);
        say("Deleted the empty R2 bucket " + bucket.name + ", which its finished run left behind");
      } catch (error) {
        if (!(error instanceof ApiError && error.status === 409)) throw error;
        await expireAllObjects(account, bucket.name, deadline);
        say("The R2 bucket " + bucket.name + " still holds objects. Its lifecycle rule deletes them within a day, and a later run's sweep deletes the bucket");
      }
    });
  }
  startPhase();
  const listed = await listNamespaces(account, deadline).catch((error: unknown) => void failed("listing the Durable Object namespaces", error));
  const namespaces = listed?.namespaces;
  const unnamed = namespaces?.filter((namespace) => namespace.script === undefined) ?? [];
  if (listed) say("The sweep listed " + (namespaces!.length + listed.malformed) + " Durable Object namespaces, " + unnamed.length + " of them with no Worker name and " + listed.malformed + " with no id or class");
  for (const namespace of unnamed) leftAlone.push("the Durable Object namespace " + label(namespace) + ", because the API names no Worker for it");
  if (listed?.malformed) leftAlone.push(listed.malformed + " Durable Object namespaces the API listed with no id or class");
  for (const namespace of namespaces ?? []) {
    const script = namespace.script;
    if (script === undefined || !script.startsWith(NAME_PREFIX) || live.has(script)) continue;
    if (!NAMESPACE_ID.test(namespace.id)) {
      leftAlone.push("the Durable Object namespace " + label(namespace) + " of the deleted Worker " + shown(script) + ", because its id is not 32 hex digits");
      continue;
    }
    await visit("Durable Object namespace " + label(namespace) + " of the deleted Worker " + shown(script), () => orphaned(script), async () => {
      await deleteNamespace(account, namespace.id, deadline);
      say("Deleted the Durable Object namespace " + label(namespace) + ", which outlived the Worker " + script + " of a finished run");
    });
  }

  startPhase();
  const unchecked = (what: string, error: unknown) => {
    if (error instanceof DeadlineError) unfinished = true;
    else say("::warning::The sweep could not check for leftovers the lists left out, " + what + ": " + reason(error));
  };
  const recent = await finishedRunNames(repository, deadline).catch((error: unknown) => void unchecked("listing the finished test runs", error));
  if (recent && (testWorkers || buckets)) {
    const listedWorkers = new Set(testWorkers?.map((worker) => worker.name));
    const listedBuckets = new Set(buckets?.map((bucket) => bucket.name));
    let checked = 0;
    let found = 0;
    let lookupsFailed = 0;
    const lookUp = async (what: string, exists: () => Promise<unknown>) => {
      try {
        if (await exists()) {
          found++;
          leftAlone.push("the " + what + ", which exists although its list left it out");
        }
      } catch (error) {
        lookupsFailed++;
        unchecked("looking up the " + what, error);
      }
    };
    for (const name of recent) {
      if (name === current || (listedWorkers.has(name) && listedBuckets.has(name))) continue;
      if (Date.now() >= deadline.atMs) {
        unfinished = true;
        break;
      }
      checked++;
      if (testWorkers && !listedWorkers.has(name)) await lookUp("Worker " + name, () => getWorker(account, name, deadline));
      if (buckets && !listedBuckets.has(name)) await lookUp("R2 bucket " + name, () => getBucket(account, name, deadline));
    }
    say("The sweep looked up the Worker and R2 bucket of " + checked + " test run attempts that finished in the last " + LOOKBACK_DAYS + " days and that the lists did not show: " + found + " exist, " + lookupsFailed + " lookups failed");
  }

  for (const item of leftAlone) say("::warning::The sweep left alone " + item);
  if (unfinished) say("::notice::The sweep stopped because " + what + ". The next run's sweep checks what is left");
  if (problems.length > 0) {
    for (const problem of problems) console.error(redacted("::error::sweep: " + problem));
    fail("the sweep could not check or delete everything earlier runs left behind");
  }
}

const [command, arg = ""] = process.argv.slice(2);
try {
  if (command === "upgrade-from") upgradeFrom(arg);
  else if (command === "deploy") await deploy(requireAccount(), arg || "HEAD");
  else if (command === "claim") await claim(requireAccount());
  else if (command === "teardown") await teardown(requireAccount());
  else if (command === "sweep") await sweep(requireAccount());
  else fail("usage: run.ts upgrade-from [<ref>] | deploy <ref> | claim | teardown | sweep");
} catch (error) {
  console.error("::error::deploy test " + command + ": " + (error instanceof Error ? error.message : String(error)));
  process.exitCode = 1;
}
