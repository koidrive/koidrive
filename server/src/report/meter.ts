// SPDX-License-Identifier: AGPL-3.0-or-later

import type { ObjectState, Sql, SqlCursor, SqlRow } from "../host";
import type { Carried, Counter, UsageTally } from "./usage";

type Methods<T> = { [K in keyof T]: T[K] extends (...args: never[]) => unknown ? K : never }[keyof T];
type R2Op = { counter?: Counter; returns?: Record<string, R2Op> };

const CLASS_A: R2Op = { counter: "r2_class_a" };
const CLASS_B: R2Op = { counter: "r2_class_b" };
const FREE: R2Op = {};
const UPLOAD: Record<Methods<R2MultipartUpload>, R2Op> = { uploadPart: CLASS_A, complete: CLASS_A, abort: FREE };
const BUCKET: Record<Methods<R2Bucket>, R2Op> = {
  head: CLASS_B,
  get: CLASS_B,
  put: CLASS_A,
  list: CLASS_A,
  delete: FREE,
  createMultipartUpload: { ...CLASS_A, returns: UPLOAD },
  resumeMultipartUpload: { returns: UPLOAD },
};

function countedOps<T extends object>(target: T, ops: Record<string, R2Op>, tally: UsageTally): T {
  return new Proxy(target, {
    get(object, name) {
      const value: unknown = Reflect.get(object, name);
      const op = typeof name === "string" ? ops[name] : undefined;
      if (typeof value !== "function" || !op) return value;
      return (...args: unknown[]) => {
        if (op.counter) tally.add(op.counter, 1);
        const result: unknown = value.apply(object, args);
        const returns = op.returns;
        if (!returns) return result;
        return result instanceof Promise ? result.then((made: object) => countedOps(made, returns, tally)) : countedOps(result as object, returns, tally);
      };
    },
  });
}

export function countedBucket(bucket: R2Bucket, tally: UsageTally): R2Bucket {
  return countedOps(bucket, BUCKET, tally);
}

function countedCursor<T extends SqlRow>(cursor: SqlCursor<T>, tally: UsageTally): SqlCursor<T> {
  let read = 0;
  let written = 0;
  const settle = () => {
    tally.add("do_rows_read", cursor.rowsRead - read);
    tally.add("do_rows_written", cursor.rowsWritten - written);
    read = cursor.rowsRead;
    written = cursor.rowsWritten;
  };
  settle();
  return {
    toArray() {
      try {
        return cursor.toArray();
      } finally {
        settle();
      }
    },
    one() {
      try {
        return cursor.one();
      } finally {
        settle();
      }
    },
    *[Symbol.iterator]() {
      try {
        yield* cursor;
      } finally {
        settle();
      }
    },
    get rowsRead() {
      return cursor.rowsRead;
    },
    get rowsWritten() {
      return cursor.rowsWritten;
    },
  };
}

export function countedState(state: ObjectState, tally: UsageTally): ObjectState {
  const storage = state.storage;
  const sql: Sql = {
    exec: (query, ...bindings) => countedCursor(storage.sql.exec(query, ...bindings), tally),
    get databaseSize() {
      return storage.sql.databaseSize;
    },
  };
  return {
    id: state.id,
    storage: {
      sql,
      transactionSync: (closure) => storage.transactionSync(closure),
      sync: () => storage.sync(),
      setAlarm: (scheduledTimeMs) => {
        tally.add("do_rows_written", 1);
        return storage.setAlarm(scheduledTimeMs);
      },
      deleteAlarm: () => storage.deleteAlarm(),
    },
    getWebSockets: (tag) => state.getWebSockets(tag),
    blockConcurrencyWhile: (callback) => state.blockConcurrencyWhile(callback),
    abort: (reason) => state.abort(reason),
  };
}

export function countedEgress(response: Response, tally: UsageTally): Response {
  if (response.body === null || tally.reporting === false) return response;
  const length = response.headers.get("Content-Length");
  if (length !== null) {
    tally.add("egress_bytes", Number(length));
    return response;
  }
  const counter = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      tally.add("egress_bytes", chunk.byteLength);
      controller.enqueue(chunk);
    },
  });
  return new Response(response.body.pipeThrough(counter), response);
}

export type ObjectKind = "account" | "space";

export interface Thrown {
  name: string;
  message: string;
  overloaded?: unknown;
  retryable?: unknown;
}

export interface Reply<T> extends Carried {
  value: T;
  thrown?: Thrown;
}

export const CARRIED_HEADER = "Koidrive-Carried";

export interface MeteredCall {
  keep?: boolean;
  gauges?: boolean;
}

export type Ended = (event: string, args: unknown[], replying: boolean) => Promise<void> | void;

const HANDLERS = new Set(["fetch", "alarm", "webSocketMessage", "webSocketClose", "webSocketError"]);

function thrownOf(error: unknown): Thrown {
  if (!(error instanceof Error)) return { name: "Error", message: String(error) };
  const { overloaded, retryable } = error as { overloaded?: unknown; retryable?: unknown };
  return { name: error.name, message: error.message, overloaded, retryable };
}

function rethrow({ name, message, overloaded, retryable }: Thrown): never {
  const error = new Error(message);
  error.name = name;
  if (overloaded !== undefined) Object.assign(error, { overloaded });
  if (retryable !== undefined) Object.assign(error, { retryable });
  throw error;
}

function carriedBy(tally: UsageTally, gauges: boolean | undefined): Carried {
  const gaugesDue = (gauges && tally.gaugesDue) || undefined;
  if (gaugesDue) tally.gaugesDue = false;
  return { usage: tally.take(), reporting: tally.reporting, startedMs: tally.startedMs, gaugesDue };
}

function withHeader(response: Response, headers: Headers): Response {
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers, webSocket: response.webSocket });
}

export async function meteredReply(object: object, kind: ObjectKind, tally: UsageTally, ended: Ended, method: string, args: unknown[], call: MeteredCall = {}): Promise<Reply<unknown>> {
  const value: unknown = Reflect.get(object, method);
  if (typeof value !== "function" || method === "constructor" || method === "metered" || HANDLERS.has(method) || Object.hasOwn(object, method)) throw new Error(`no method ${method}`);
  tally.enter();
  tally.add(`do_${kind}_requests`, 1);
  let result: unknown;
  let thrown: Thrown | undefined;
  try {
    result = await value.apply(object, args);
  } catch (error) {
    thrown = thrownOf(error);
  } finally {
    tally.lapse();
    await ended(method, args, true);
    if (call.keep) tally.keep();
    tally.leave();
  }
  return { value: result, thrown, ...carriedBy(tally, call.gauges) };
}

export function meteredObject<T extends object>(object: T, kind: ObjectKind, tally: UsageTally, ended: Ended): T {
  return new Proxy(object, {
    get(target, name) {
      const value: unknown = Reflect.get(target, name);
      if (typeof value !== "function" || typeof name !== "string" || !HANDLERS.has(name)) return value;
      return async (...args: unknown[]) => {
        tally.enter();
        if (name === "webSocketMessage") tally.add("do_ws_messages", 1);
        else if (!name.startsWith("webSocket")) tally.add(`do_${kind}_requests`, 1);
        const replying = name === "fetch";
        let result: unknown;
        let thrown: Thrown | undefined;
        try {
          result = await value.apply(target, args);
        } catch (error) {
          if (!replying) throw error;
          thrown = thrownOf(error);
        } finally {
          tally.lapse();
          await ended(name, args, replying);
          tally.leave();
        }
        if (!replying) return result;
        const response = thrown ? new Response(null, { status: 500 }) : (result as Response);
        const headers = new Headers(response.headers);
        headers.set(CARRIED_HEADER, JSON.stringify({ thrown, ...carriedBy(tally, true) }));
        return withHeader(response, headers);
      };
    },
  });
}

const PASSED = new Set<string | symbol>(["then", "id", "name", "connect", "dup"]);

export function meteredStub<T extends object>(stub: T, receive: (carried: Carried) => void, call: MeteredCall = {}): T {
  return new Proxy(stub, {
    get(target, name) {
      if (typeof name !== "string" || PASSED.has(name)) {
        const value: unknown = Reflect.get(target, name);
        return typeof value === "function" ? value.bind(target) : value;
      }
      if (name === "fetch") {
        return async (...args: Parameters<typeof fetch>) => {
          const response = await (target as unknown as { fetch: typeof fetch }).fetch(...args);
          const header = response.headers.get(CARRIED_HEADER);
          if (header === null) return response;
          const { thrown, ...carried } = JSON.parse(header) as Reply<undefined>;
          receive(carried);
          if (thrown) rethrow(thrown);
          const headers = new Headers(response.headers);
          headers.delete(CARRIED_HEADER);
          return withHeader(response, headers);
        };
      }
      return async (...args: unknown[]) => {
        const { value, thrown, ...carried } = await (target as unknown as { metered(method: string, args: unknown[], call: MeteredCall): Promise<Reply<unknown>> }).metered(name, args, call);
        receive(carried);
        if (thrown) rethrow(thrown);
        return value;
      };
    },
  });
}
