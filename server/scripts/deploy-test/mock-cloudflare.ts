// SPDX-License-Identifier: AGPL-3.0-or-later

import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

export interface MockWorker {
  id: string;
  versions: { id: string; tag?: string }[];
}

export interface MockBucket {
  creationDate: string;
  objects: Map<string, number>;
}

export class MockCloudflare {
  readonly workers = new Map<string, MockWorker>();
  readonly buckets = new Map<string, MockBucket>();
  readonly namespaces: { id: string; class: string; script: string }[] = [];
  readonly requests: string[] = [];
  createBucketStatus = 200;
  claimStatus = 200;
  versionsStatus = 200;
  bucketJobStatuses: string[] = ["COMPLETED"];
  bucketJobCreateStatus = 200;
  bucketDeleteFailures = 0;
  bucketJobLeftovers: string[] = [];
  readonly bucketJobBodies: unknown[] = [];
  readonly claimCode = "ZQ7K-M2XR-9WPD";
  readonly logs: { script: string; timestamp: number; message?: string; path?: string; visibleFrom?: number }[] = [];
  readonly queryDryFlags: unknown[] = [];
  workerName = "";
  filterKeys = ["$metadata.service", "$workers.scriptName"];
  rejectedKeys: string[] = [];
  codeLagQueries = 0;
  probeLagQueries = 0;
  probeRequests = 0;
  observabilityStatus = 200;
  logQueries = 0;
  setupRequests = 0;
  graphqlAnswers: { status: number; body: unknown }[] = [];
  readonly graphqlVariables: Record<string, unknown>[] = [];
  url = "";
  private server: Server | undefined;

  async start(): Promise<string> {
    this.server = createServer((request, response) => this.handle(request, response));
    await new Promise<void>((resolve) => this.server!.listen(0, "127.0.0.1", resolve));
    const address = this.server.address();
    this.url = "http://127.0.0.1:" + (typeof address === "object" && address ? address.port : 0);
    return this.url;
  }

  async stop(): Promise<void> {
    this.server?.closeAllConnections();
    await new Promise<void>((resolve) => this.server?.close(() => resolve()) ?? resolve());
  }

  addWorker(name: string, id: string = crypto.randomUUID(), tag?: string): MockWorker {
    const worker = { id, versions: [{ id: crypto.randomUUID(), tag }] };
    this.workers.set(name, worker);
    this.namespaces.push({ id: id + "-account", class: "AccountDO", script: name }, { id: id + "-space", class: "SpaceDO", script: name });
    return worker;
  }

  addBucket(name: string, creationDate = new Date().toISOString(), keys: string[] = []): MockBucket {
    const bucket = { creationDate, objects: new Map(keys.map((key) => [key, 10])) };
    this.buckets.set(name, bucket);
    return bucket;
  }

  deletes(): string[] {
    return this.requests.filter((request) => request.startsWith("DELETE "));
  }

  private handle(request: IncomingMessage, response: ServerResponse): void {
    const path = (request.url ?? "/").split("?")[0]!;
    const method = request.method ?? "GET";
    let body = "";
    request.on("data", (chunk: Buffer) => (body += chunk));
    request.on("end", () => {
      const send = (status: number, value: unknown) => {
        response.writeHead(status, { "Content-Type": "application/json" });
        response.end(JSON.stringify(value));
      };
      const ok = (result: unknown) => send(200, { success: true, errors: [], messages: [], result });
      const missing = () => send(404, { success: false, errors: [{ code: 10007, message: "not found" }], messages: [], result: null });
      let match: RegExpExecArray | null;

      if (path === "/v1/server") {
        return send(200, { serverVersion: "0.0.0", claimed: false });
      }
      if (path === "/setup") {
        this.setupRequests++;
        if (this.setupRequests === 1) {
          this.logs.push({ script: this.workerName, message: "Koidrive claim code (valid for 1 hour): " + this.claimCode, path, timestamp: Date.now(), visibleFrom: this.logQueries + this.codeLagQueries });
        }
        response.writeHead(200, { "Content-Type": "text/plain" });
        response.end("A one-time claim code, valid for one hour, is in this Worker's logs.\n");
        return;
      }
      if ((match = /^\/mock\/workers\/([^/]+)$/.exec(path)) && method === "POST") {
        const { tag } = JSON.parse(body || "{}") as { tag?: string };
        const worker = this.workers.get(match[1]!);
        if (!worker) return send(200, this.addWorker(match[1]!, undefined, tag));
        worker.versions.unshift({ id: crypto.randomUUID(), tag });
        return send(200, worker);
      }
      if (path.startsWith("/deploy-test-probe-")) {
        this.probeRequests++;
        this.logs.push({ script: this.workerName, path, timestamp: Date.now(), visibleFrom: this.logQueries + this.probeLagQueries });
        return send(404, { code: "NOT_FOUND", message: "not found" });
      }
      if (path === "/v1/claim") {
        const claimed = JSON.parse(body) as { code?: string };
        if (this.claimStatus !== 200 || claimed.code !== this.claimCode) return send(403, { code: "CODE_FORBIDDEN", message: "wrong claim code" });
        return send(200, { userId: "user-1", deviceId: "device-1", refreshCredential: "refresh-1", access: { token: "t", expiresAtMs: "0" } });
      }

      this.requests.push(method + " " + path);
      if (path === "/client/v4/graphql" && method === "POST") {
        this.graphqlVariables.push((JSON.parse(body) as { variables: Record<string, unknown> }).variables);
        const answer = this.graphqlAnswers.length > 1 ? this.graphqlAnswers.shift()! : this.graphqlAnswers[0];
        return answer ? send(answer.status, answer.body) : missing();
      }
      const api = /^\/client\/v4\/accounts\/[^/]+(\/.*)$/.exec(path)?.[1];
      if (!api) return missing();
      if ((match = /^\/workers\/workers\/([^/]+)$/.exec(api)) && method === "GET") {
        const worker = this.workers.get(match[1]!);
        return worker ? ok({ id: worker.id, name: match[1], created_on: new Date().toISOString() }) : missing();
      }
      if ((match = /^\/workers\/scripts\/([^/]+)$/.exec(api)) && method === "DELETE") {
        if (!this.workers.delete(match[1]!)) return missing();
        const name = match[1];
        for (let i = this.namespaces.length - 1; i >= 0; i--) if (this.namespaces[i]!.script === name) this.namespaces.splice(i, 1);
        return ok(null);
      }
      if ((match = /^\/workers\/scripts\/([^/]+)\/versions$/.exec(api)) && method === "GET") {
        const worker = this.workers.get(match[1]!);
        if (!worker) return missing();
        if (this.versionsStatus !== 200) return send(this.versionsStatus, { success: false, errors: [{ code: 10000, message: "Authentication error" }], messages: [], result: null });
        const page = Number(new URL(request.url ?? "/", this.url).searchParams.get("page") ?? "1");
        return ok({ items: page === 1 ? worker.versions.map((version, i) => ({ id: version.id, number: worker.versions.length - i })) : [] });
      }
      if ((match = /^\/workers\/scripts\/([^/]+)\/versions\/([^/]+)$/.exec(api)) && method === "GET") {
        const version = this.workers.get(match[1]!)?.versions.find((candidate) => candidate.id === match![2]);
        if (!version) return missing();
        return ok({ id: version.id, annotations: { "workers/triggered_by": "upload", ...(version.tag ? { "workers/tag": version.tag } : {}) } });
      }
      if (api === "/workers/observability/telemetry/query" && method === "POST") {
        if (this.observabilityStatus !== 200) return send(this.observabilityStatus, { success: false, errors: [{ code: 10000, message: "Authentication error" }], messages: [], result: null });
        const { timeframe, parameters, dry } = JSON.parse(body) as { timeframe: { from: number; to: number }; parameters: { filters: { key: string; operation: string; value: string }[] }; dry?: unknown };
        this.queryDryFlags.push(dry);
        if (parameters.filters.some((candidate) => this.rejectedKeys.includes(candidate.key))) {
          return send(400, { success: false, errors: [{ code: 10001, message: "unknown key" }], messages: [], result: null });
        }
        const filter = parameters.filters.find((candidate) => this.filterKeys.includes(candidate.key) && candidate.operation === "eq");
        const index = this.logQueries++;
        const events = this.logs
          .filter((log) => filter !== undefined && log.script === filter.value && (log.visibleFrom ?? 0) <= index && log.timestamp >= timeframe.from && log.timestamp <= timeframe.to)
          .map((log) => ({
            timestamp: log.timestamp,
            $metadata: { service: log.script, level: "log", ...(log.message ? { message: log.message } : {}) },
            $workers: { scriptName: log.script, event: { request: { url: this.url + (log.path ?? "/"), method: "GET" } } },
            ...(log.message ? { source: { message: log.message } } : {}),
          }));
        return ok({ events: { events } });
      }
      if (api === "/workers/durable_objects/namespaces" && method === "GET") {
        return send(200, { success: true, errors: [], messages: [], result: this.namespaces, result_info: { page: 1, total_pages: 1 } });
      }
      if (api === "/r2/buckets" && method === "POST") {
        const { name } = JSON.parse(body) as { name: string };
        if (this.createBucketStatus === 409) {
          this.addBucket(name);
          return send(409, { success: false, errors: [{ code: 10004, message: "The bucket you tried to create already exists" }], messages: [], result: null });
        }
        this.addBucket(name);
        return ok({ name });
      }
      if ((match = /^\/r2\/buckets\/([^/]+)$/.exec(api))) {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket) return missing();
        if (method === "GET") return ok({ name: match[1], creation_date: bucket.creationDate });
        if (method === "DELETE") {
          if (bucket.objects.size > 0) return send(409, { success: false, errors: [{ code: 10008, message: "The bucket you tried to delete is not empty" }], messages: [], result: null });
          if (this.bucketDeleteFailures > 0) {
            this.bucketDeleteFailures--;
            return send(409, { success: false, errors: [{ code: 10008, message: "The bucket you tried to delete is not empty" }], messages: [], result: null });
          }
          this.buckets.delete(match[1]!);
          return ok(null);
        }
      }
      if ((match = /^\/r2\/buckets\/([^/]+)\/objects$/.exec(api)) && method === "GET") {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket) return missing();
        const cursor = Number(new URL(request.url ?? "/", this.url).searchParams.get("cursor") ?? "0");
        const objects = [...bucket.objects].map(([key, size]) => ({ key, size }));
        const next = cursor + 1000;
        return send(200, { success: true, errors: [], messages: [], result: objects.slice(cursor, next), result_info: { is_truncated: next < objects.length, cursor: next < objects.length ? String(next) : "" } });
      }
      if ((match = /^\/r2\/buckets\/([^/]+)\/jobs(?:\/empty-1)?$/.exec(api)) && (method === "POST" || method === "GET")) {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket) return missing();
        if (method === "POST") {
          this.bucketJobBodies.push(JSON.parse(body));
          if (this.bucketJobCreateStatus !== 200) return send(this.bucketJobCreateStatus, { success: false, errors: [{ code: 10001, message: "job creation failed" }] });
        }
        const status = this.bucketJobStatuses.length > 1 ? this.bucketJobStatuses.shift() : this.bucketJobStatuses[0];
        const deletedObjects = status === "COMPLETED" ? bucket.objects.size : 0;
        if (status === "COMPLETED") {
          bucket.objects.clear();
          for (const key of this.bucketJobLeftovers) bucket.objects.set(key, 10);
        }
        return ok({ id: "empty-1", jobType: "prefixDelete", status, prefixDelete: { prefix: "", deletedObjects }, startTime: new Date().toISOString() });
      }
      if ((match = /^\/r2\/buckets\/([^/]+)\/objects\/(.+)$/.exec(api)) && method === "DELETE") {
        const bucket = this.buckets.get(match[1]!);
        if (!bucket?.objects.delete(decodeURIComponent(match[2]!))) return missing();
        return ok(null);
      }
      return missing();
    });
  }
}
