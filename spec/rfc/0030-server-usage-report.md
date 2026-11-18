# RFC 0030: The server's usage report

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §15 (`POST /v1/token`). Adds a field to `TokenRequest`.

A client sees a failed call only from the outside. The server sees which calls fail, why and how slowly. This RFC lets a server send an anonymous hourly summary of that to the Koidrive project, following the usage-report choice of the devices that use it. The person makes that choice once, when setup asks.

## 1. Messages

- `TokenRequest.telemetry = 6`, a `TokenRequest.Telemetry`: `TELEMETRY_UNSPECIFIED`, `TELEMETRY_ON` or `TELEMETRY_OFF`.

No field is removed or renumbered.

## 2. Client rules

1. A client sends `TELEMETRY_ON` while it sends usage reports, `TELEMETRY_OFF` while the person or the environment has turned them off, and `TELEMETRY_UNSPECIFIED` while the person hasn't answered.
2. A client that only prints reports and sends nothing (`KOIDRIVE_TELEMETRY=debug`) sends `TELEMETRY_OFF`.
3. The value goes on every token request. When the person changes the choice, the client sends it at once with a token refresh. If the server can't be reached, the change goes with the next refresh.

## 3. Server rules

1. The server stores the value on the device that sent it, so each token refresh replaces that device's previous value.
2. The server reports while no device that isn't revoked last sent `TELEMETRY_OFF`, and at least one such device sent `TELEMETRY_ON` with a refresh in the last 24 hours, rounded up to the next hour. A running client refreshes every 15 minutes and rotates its credential every 24 hours, so a device that hasn't refreshed for a day isn't running. An OFF vote doesn't lapse.
3. The server keeps the outcome of the votes as one stored value. It recounts the devices only when a device's vote changes or a device is revoked, so a session check reads one row however many devices the server has had.
4. While the server doesn't report, it keeps no counts and sends nothing. When reporting stops, every process drops the counts it holds, and the server deletes the counts it hasn't sent.
5. While it reports, the server counts each request by route and outcome, and each Space DO RPC by kind and outcome. The route and the kind are fixed names, such as `put_chunk` or `rpc_commit`. The outcome is the HTTP status, followed by the refusal code and the RFC 0028 cause when there is one. Each route or kind also gets 16 power-of-two latency buckets in milliseconds. Each process hands its counts to the Account DO in batches, and the Account DO writes one row per hour in a batch, never one per request. The first count after a hand-over opens a batch. A batch is due 10 minutes after it opened, or once a later UTC hour has started. A process counts only once a session check has told it that the server reports, and counts nothing and arms nothing otherwise. If a hand-over fails, its counts go with the next one.
   - The Worker counts its requests and the Space DO RPCs that arrive over HTTP. Each request made while the server reports waits in `ctx.waitUntil()`, which can run for up to 30 seconds after the response is sent ([Context](https://developers.cloudflare.com/workers/runtime-apis/context/)). The Worker hands its batch over once the batch is due, or once 25 seconds pass with no new count, which leaves the hand-over call 5 seconds inside that limit. So a request that comes more than 25 seconds after the one before it costs at most one Account DO call and one row written, and requests closer together share one. A Worker is billed for CPU time, not for that wait ([Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/)). An isolate learns whether the server reports from the session check of an authenticated request, so it counts nothing before its first authenticated request.
   - A Space DO counts the RPCs that arrive over its WebSockets and sets no timer: a pending timer keeps a Durable Object from hibernating ([Durable Object lifecycle](https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/)), and an object that can't hibernate is billed for its time in memory ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)). After each RPC it parks its counts in the attachment of the socket that carried the RPC, which survives hibernation ([WebSockets](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)). When the batch is due, when that socket is gone, or when the attachment would pass 8 KiB, it hands over its own counts and those parked on every socket in one call.
   - When counts are first parked, the Space DO sets its alarm for one minute past the next UTC hour if it hasn't already, and the alarm hands over what is still parked before the Account DO drops that hour (rule 7). A socket that closes passes its parked counts to another open socket, or hands them over when no other socket is open.
   - A shutdown loses counts. A deploy, a runtime update or a crash drops the Worker's open batch: up to 10 minutes of one isolate's counts. A Space DO's parked counts survive hibernation, and a code update that the object adopts after hibernating, because both keep its sockets connected. A crash, or a code update that resets an object still active when the update's delay runs out, disconnects its sockets and drops what was parked on them: up to 10 minutes of that object's counts ([Durable Objects code updates](https://developers.cloudflare.com/durable-objects/deployments/durable-objects-code-updates/)). Only a storage write per request could prevent that, and the server doesn't make one.
6. About five minutes after each UTC hour ends, the server posts the hour's counts as one `server_report` event in a PostHog `/batch/` body to `https://tel.koidrive.com/batch/`. The event has a fresh random id, used as both `uuid` and `distinct_id`. Its other properties are `$process_person_profile: false`, `$geoip_disable: true`, `schema`, `server_version`, `proto_min`, `proto_max`, the counts as `c.<route>.<outcome>` and the buckets as `h.<route>`. Nothing else is sent. Cloudflare adds a `CF-Worker` header naming the zone that owns the Worker to every subrequest ([HTTP headers](https://developers.cloudflare.com/fundamentals/reference/http-headers/#cf-worker)), so the endpoint receives that zone.
7. Counts for an hour whose post is already due are dropped, so each hour is posted once.
8. A failed post is tried again an hour later. Counts older than 24 hours are deleted unsent.

## 4. Alternatives

- Workers Analytics Engine: a `writeDataPoint()` per request is durable and costs no storage write, but the hourly report would have to read the points back through the SQL API, which needs an account API token stored as a secret on every server ([Analytics Engine](https://developers.cloudflare.com/analytics/analytics-engine/), [SQL API](https://developers.cloudflare.com/analytics/analytics-engine/sql-api/#authentication)). That is a new credential and a new setup step.
