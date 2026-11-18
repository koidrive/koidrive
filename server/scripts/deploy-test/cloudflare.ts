// SPDX-License-Identifier: AGPL-3.0-or-later

const API = process.env.CLOUDFLARE_API_BASE_URL || "https://api.cloudflare.com/client/v4";
const ATTEMPTS = 4;
const REQUEST_MS = 60_000;

export interface Account {
  id: string;
  token: string;
}

export interface R2Object {
  key: string;
  size: number;
}

export interface Namespace {
  id: string;
  class: string;
  script: string;
}

export class ApiError extends Error {
  readonly status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

export class DeadlineError extends ApiError {
  constructor(message: string) {
    super(message, 0);
  }
}

export interface Deadline {
  atMs: number;
  what: string;
}

export function accountFromEnv(): Account | undefined {
  const id = process.env.CLOUDFLARE_ACCOUNT_ID ?? "";
  const token = process.env.CLOUDFLARE_API_TOKEN ?? "";
  return id && token ? { id, token } : undefined;
}

export function requireAccount(): Account {
  const account = accountFromEnv();
  if (!account) throw new Error("CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID must be set; see docs/deploy-test.md");
  return account;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export const LISTING_MS = 60_000;
const LISTING_EVERY_MS = 5_000;

async function listed<T>(read: (deadline: Deadline) => Promise<T | undefined>): Promise<T | undefined> {
  const deadline = { atMs: Date.now() + LISTING_MS, what: "the listing did not appear within " + LISTING_MS / 1000 + " s" };
  for (;;) {
    let value: T | undefined;
    try {
      value = await read(deadline);
    } catch (error) {
      if (error instanceof DeadlineError) return undefined;
      throw error;
    }
    const remaining = deadline.atMs - Date.now();
    if (value !== undefined || remaining <= 0) return value;
    await sleep(Math.min(LISTING_EVERY_MS, remaining));
  }
}

function keyPath(key: string): string {
  return key.split("/").map(encodeURIComponent).join("/");
}

async function send(account: Account, method: string, path: string, init: { body?: unknown; allowMissing?: boolean; deadline?: Deadline; retry?: boolean } = {}): Promise<Response | undefined> {
  const what = method + " " + path.split("?")[0];
  for (let attempt = 1; ; attempt++) {
    const remaining = init.deadline === undefined ? REQUEST_MS : init.deadline.atMs - Date.now();
    if (remaining <= 0) throw new DeadlineError(init.deadline!.what);
    let response: Response;
    try {
      response = await fetch(API + path, {
        method,
        headers: { Authorization: "Bearer " + account.token, ...(init.body === undefined ? {} : { "Content-Type": "application/json" }) },
        body: init.body === undefined ? undefined : JSON.stringify(init.body),
        signal: AbortSignal.timeout(Math.min(REQUEST_MS, remaining)),
      });
    } catch (error) {
      if (init.deadline && Date.now() >= init.deadline.atMs) throw new DeadlineError(init.deadline.what);
      if (init.retry === false || attempt >= ATTEMPTS) throw new ApiError(what + " failed: " + (error instanceof Error ? error.message : String(error)), 0);
      await sleep(Math.max(0, Math.min(2000 * attempt, init.deadline === undefined ? Infinity : init.deadline.atMs - Date.now())));
      continue;
    }
    if (response.ok) return response;
    if (response.status === 404 && init.allowMissing) {
      await response.body?.cancel();
      return undefined;
    }
    if ((response.status === 429 || response.status >= 500) && init.retry !== false && attempt < ATTEMPTS) {
      await response.body?.cancel();
      await sleep(Math.max(0, Math.min(2000 * attempt, init.deadline === undefined ? Infinity : init.deadline.atMs - Date.now())));
      continue;
    }
    const text = await response.text();
    let detail = text.slice(0, 300);
    try {
      const errors = (JSON.parse(text) as { errors?: { code: number; message: string }[] }).errors ?? [];
      if (errors.length > 0) detail = errors.map((error) => error.code + " " + error.message).join("; ");
    } catch {}
    const hint = response.status === 403 ? " (the token lacks a permission this call needs; see docs/deploy-test.md)" : "";
    throw new ApiError(what + " returned HTTP " + response.status + ": " + detail + hint, response.status);
  }
}

interface Envelope<T> {
  result: T;
  result_info?: { cursor?: string; is_truncated?: boolean; page?: number; total_pages?: number };
}

async function json<T>(response: Response, deadline?: Deadline): Promise<T> {
  try {
    return (await response.json()) as T;
  } catch (error) {
    if (deadline && Date.now() >= deadline.atMs) throw new DeadlineError(deadline.what);
    throw error;
  }
}

async function call<T>(account: Account, method: string, path: string, body?: unknown, deadline?: Deadline): Promise<Envelope<T>> {
  const response = await send(account, method, path, { body, deadline });
  return json<Envelope<T>>(response!, deadline);
}

export async function createBucket(account: Account, name: string): Promise<void> {
  await call(account, "POST", `/accounts/${account.id}/r2/buckets`, { name });
}

export async function getBucket(account: Account, name: string): Promise<{ creationDate: string } | undefined> {
  const response = await send(account, "GET", `/accounts/${account.id}/r2/buckets/${name}`, { allowMissing: true });
  if (!response) return undefined;
  const bucket = ((await response.json()) as Envelope<{ creation_date?: string }>).result;
  if (!bucket?.creation_date) throw new ApiError("GET /accounts/{id}/r2/buckets/" + name + " returned no creation_date", 200);
  return { creationDate: bucket.creation_date };
}

export async function getWorker(account: Account, name: string): Promise<{ id: string } | undefined> {
  const response = await send(account, "GET", `/accounts/${account.id}/workers/workers/${name}`, { allowMissing: true });
  if (!response) return undefined;
  const worker = ((await response.json()) as Envelope<{ id?: string }>).result;
  if (!worker?.id) throw new ApiError("GET /accounts/{id}/workers/workers/" + name + " returned no id", 200);
  return { id: worker.id };
}

export type WorkerTag = "tagged" | "untagged" | "unlisted";

export async function workerTag(account: Account, name: string, tag: string): Promise<WorkerTag> {
  const found = await listed(async (deadline): Promise<WorkerTag | undefined> => {
    for (let page = 1; ; page++) {
      const response = await send(account, "GET", `/accounts/${account.id}/workers/scripts/${name}/versions?per_page=100&page=${page}`, { allowMissing: true, deadline });
      if (!response) return page === 1 ? undefined : "untagged";
      const items = (await json<Envelope<{ items?: { id: string }[] }>>(response, deadline)).result?.items ?? [];
      if (page === 1 && items.length === 0) return undefined;
      for (const item of items) {
        const version = await call<{ annotations?: Record<string, string> }>(account, "GET", `/accounts/${account.id}/workers/scripts/${name}/versions/${item.id}`, undefined, deadline);
        if (version.result?.annotations?.["workers/tag"] === tag) return "tagged";
      }
      if (items.length < 100) return "untagged";
    }
  });
  return found ?? "unlisted";
}

export async function listObjects(account: Account, bucket: string, prefix = ""): Promise<R2Object[] | undefined> {
  const objects: R2Object[] = [];
  let cursor = "";
  for (;;) {
    const query = new URLSearchParams({ per_page: "1000", ...(prefix ? { prefix } : {}), ...(cursor ? { cursor } : {}) });
    const response = await send(account, "GET", `/accounts/${account.id}/r2/buckets/${bucket}/objects?${query}`, { allowMissing: true });
    if (!response) return undefined;
    const page = (await response.json()) as Envelope<{ key: string; size: number }[]>;
    for (const object of page.result ?? []) objects.push({ key: object.key, size: Number(object.size) });
    cursor = page.result_info?.cursor ?? "";
    if (!page.result_info?.is_truncated || !cursor) return objects;
  }
}

function bucketJob(value: unknown): { id: string; status: "ENQUEUED" | "RUNNING" | "COMPLETED" | "FAILED" | "CANCELLED" } {
  if (!value || typeof value !== "object" || !("result" in value) || !value.result || typeof value.result !== "object") {
    throw new ApiError("bucket-emptying job returned an invalid response", 200);
  }
  const job = value.result;
  if (!("id" in job) || typeof job.id !== "string" || !job.id || !("status" in job)) {
    throw new ApiError("bucket-emptying job returned no id or status", 200);
  }
  const status = job.status;
  if (status !== "ENQUEUED" && status !== "RUNNING" && status !== "COMPLETED" && status !== "FAILED" && status !== "CANCELLED") {
    throw new ApiError("bucket-emptying job returned an unknown status", 200);
  }
  return { id: job.id, status };
}

export async function emptyBucket(account: Account, bucket: string): Promise<void> {
  const path = `/accounts/${account.id}/r2/buckets/${bucket}/jobs`;
  const deadline = { atMs: Date.now() + 6 * 60_000, what: "bucket-emptying job exceeded its six-minute deadline" };
  const created = await send(account, "POST", path, { body: { jobType: "prefixDelete", prefix: "" }, deadline, retry: false });
  let job = bucketJob(await json(created!, deadline));
  const id = job.id;
  while (job.status !== "COMPLETED") {
    if (job.status === "FAILED" || job.status === "CANCELLED") throw new ApiError("bucket-emptying job " + job.status, 200);
    const remaining = deadline.atMs - Date.now();
    if (remaining <= 0) throw new DeadlineError(deadline.what);
    await sleep(Math.min(5000, remaining));
    const response = await send(account, "GET", path + "/" + encodeURIComponent(id), { deadline });
    job = bucketJob(await json(response!, deadline));
  }
}

export async function getObject(account: Account, bucket: string, key: string): Promise<Uint8Array | undefined> {
  const response = await send(account, "GET", `/accounts/${account.id}/r2/buckets/${bucket}/objects/${keyPath(key)}`, { allowMissing: true });
  return response ? new Uint8Array(await response.arrayBuffer()) : undefined;
}

export async function deleteObject(account: Account, bucket: string, key: string): Promise<void> {
  await send(account, "DELETE", `/accounts/${account.id}/r2/buckets/${bucket}/objects/${keyPath(key)}`, { allowMissing: true });
}

export async function deleteBucket(account: Account, bucket: string): Promise<boolean> {
  return (await send(account, "DELETE", `/accounts/${account.id}/r2/buckets/${bucket}`, { allowMissing: true })) !== undefined;
}

/** With force, Cloudflare also deletes the Durable Object namespaces it implements. */
export async function deleteWorker(account: Account, name: string): Promise<boolean> {
  return (await send(account, "DELETE", `/accounts/${account.id}/workers/scripts/${name}?force=true`, { allowMissing: true })) !== undefined;
}

export async function namespacesOf(account: Account, script: string): Promise<Namespace[]> {
  const found: Namespace[] = [];
  for (let page = 1; ; page++) {
    const listed = await call<Namespace[]>(account, "GET", `/accounts/${account.id}/workers/durable_objects/namespaces?per_page=1000&page=${page}`);
    found.push(...(listed.result ?? []).filter((namespace) => namespace.script === script));
    if (!listed.result_info?.total_pages || page >= listed.result_info.total_pages) return found;
  }
}

export const WORKER_KEYS = ["$metadata.service", "$workers.scriptName"] as const;
export type WorkerKey = (typeof WORKER_KEYS)[number];

export type TelemetryEvent = Record<string, Record<string, unknown> | undefined>;

export const TELEMETRY_LIMIT = 2000;

export async function telemetryEvents(account: Account, key: WorkerKey, name: string, fromMs: number, toMs: number, deadline?: Deadline): Promise<TelemetryEvent[]> {
  const query = {
    queryId: "deploy-test-" + name,
    view: "events",
    dry: true,
    limit: TELEMETRY_LIMIT,
    parameters: { filters: [{ key, operation: "eq", type: "string", value: name }] },
    timeframe: { from: fromMs, to: toMs },
  };
  const response = await call<{ events?: { events?: TelemetryEvent[] } }>(account, "POST", `/accounts/${account.id}/workers/observability/telemetry/query`, query, deadline);
  return response.result?.events?.events ?? [];
}

export async function workerEvents(account: Account, key: WorkerKey, name: string, fromMs: number, deadline: Deadline): Promise<{ timestamp: unknown; text: string }[]> {
  const [group, field] = key.split(".") as [string, string];
  return (await telemetryEvents(account, key, name, fromMs, Date.now(), deadline)).filter((event) => event[group]?.[field] === name).map((event) => ({ timestamp: event.timestamp, text: JSON.stringify(event) }));
}

export interface Deployment {
  created_on: string;
  versions: { version_id: string; percentage: number }[];
}

export function serves(deployment: Deployment | undefined, versionId: string): boolean {
  return deployment?.versions.some((version) => version.version_id === versionId && version.percentage === 100) ?? false;
}

export async function newestDeployment(account: Account, name: string, versionId: string): Promise<Deployment | undefined> {
  let newest: Deployment | undefined;
  await listed(async (deadline) => {
    const response = await call<{ deployments: Deployment[] }>(account, "GET", `/accounts/${account.id}/workers/scripts/${name}/deployments`, undefined, deadline);
    newest = [...(response.result?.deployments ?? [])].sort((a, b) => b.created_on.localeCompare(a.created_on))[0];
    return serves(newest, versionId) ? newest : undefined;
  });
  return newest;
}

export interface Subscription {
  id: string;
  price?: number;
  state?: string;
  rate_plan?: { id?: string; public_name?: string };
}

export async function subscriptions(account: Account): Promise<Subscription[]> {
  return (await call<Subscription[]>(account, "GET", `/accounts/${account.id}/subscriptions`)).result ?? [];
}

export async function graphql<T>(account: Account, query: string, variables: Record<string, unknown>, deadline?: Deadline): Promise<T> {
  const response = await call<never>(account, "POST", "/graphql", { query, variables }, deadline);
  const body = response as unknown as { data?: T; errors?: { message: string }[] | null };
  if (body.errors?.length) throw new ApiError("GraphQL: " + body.errors.map((error) => error.message).join("; "), 200);
  return body.data as T;
}
