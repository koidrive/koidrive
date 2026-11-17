# RFC 0028: Error causes

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §13 (errors). Adds a field to `Error`.

`CODE_INTERNAL`, `CODE_RETRY_LATER` and `CODE_QUOTA_EXCEEDED` tell a client what to do next, but not what failed. A rate-limited R2 write, an overloaded Durable Object and a bug in the server all reach the client the same way. This RFC lets the server name the platform failure behind a refusal, so a client can count it without a message string.

## 1. Messages

- `Error.cause = 5`, an `Error.Cause`.

No field is removed or renumbered.

## 2. Server rules

1. The server sets `cause` when a call fails for a reason other than the request: an exception in its own code or a failure of the platform it runs on. A refusal of the request itself (`CODE_UNAUTHENTICATED`, `CODE_INVALID_ARGUMENT` and the other codes about the request) carries `CAUSE_UNSPECIFIED`.
2. The server picks the first cause that matches, in this order:

   | Cause | The error |
   |---|---|
   | `CAUSE_DAILY_LIMIT` | a Durable Objects free-tier daily limit |
   | `CAUSE_STORAGE_FULL` | `SQLITE_FULL` from a Durable Object's storage |
   | `CAUSE_SUBREQUEST_LIMIT` | the invocation's subrequest limit |
   | `CAUSE_CPU_EXCEEDED` | a Durable Object past its CPU time limit |
   | `CAUSE_MEMORY_EXCEEDED` | an isolate past its memory limit |
   | `CAUSE_R2_RATE_LIMITED` | R2 error 10058 |
   | `CAUSE_R2_UNAVAILABLE` | R2 error 10001 or 10043 |
   | `CAUSE_R2_ERROR` | any other R2 error |
   | `CAUSE_DO_OVERLOADED` | a Durable Object error with `overloaded` set |
   | `CAUSE_DO_UNAVAILABLE` | a Durable Object error with `retryable` set |
   | `CAUSE_EXCEPTION` | anything else |

3. A route that catches a platform failure itself keeps the code and HTTP status it answers and adds the cause. An error that no route catches is answered by its cause:

   | Cause | Code | HTTP status |
   |---|---|---|
   | a platform limit that §13 covers | `CODE_QUOTA_EXCEEDED` | 429 |
   | `CAUSE_R2_RATE_LIMITED` | `CODE_RETRY_LATER` | 429 |
   | `CAUSE_R2_UNAVAILABLE`, `CAUSE_DO_OVERLOADED`, `CAUSE_DO_UNAVAILABLE`, `CAUSE_CPU_EXCEEDED`, `CAUSE_MEMORY_EXCEEDED` | `CODE_RETRY_LATER` | 503 |
   | any other | `CODE_INTERNAL` | 500 |

   A Space DO RPC that throws follows the same table.
4. `message` stays free text for people. The cause is the only part of a failure meant for counting.

## 3. Client rules

1. A client treats a refusal by its code and HTTP status alone. It never retries, waits or stops differently because of the cause. A client that keeps a request id so it can send a create again keeps it through any 5xx answer and forgets it only on a 4xx answer.
2. A client may count causes. A value it does not know counts as an unknown cause.
3. A server without this RFC sends no cause. The client reads that as `CAUSE_UNSPECIFIED`.
4. When the Worker gives no answer, Cloudflare's edge answers in its place with a non-protobuf body that holds `error code: ` and a number ([error codes](https://developers.cloudflare.com/workers/observability/errors/)): 1101 for an exception the Worker did not catch, 1102 for a Worker past its CPU time or memory limit, and 1027 for the Workers Free daily request limit. A client may read that number from an error body of at most 64 KiB with a known length, and count it like a cause.
