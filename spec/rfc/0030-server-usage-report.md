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
5. While it reports, the server counts each request by route and outcome, and each Space DO RPC by kind and outcome. The route and the kind are fixed names, such as `put_chunk` or `rpc_commit`. The outcome is the HTTP status, followed by the refusal code and the RFC 0028 cause when there is one. Each route or kind also gets 16 power-of-two latency buckets in milliseconds. Each process hands its counts to the Account DO in batches, and the Account DO writes one row per hour in a batch, never one per request. The first count after a hand-over opens a batch. A batch is due 10 minutes after it opened, or once a later UTC hour has started. A process counts only once a session check has told it that the server reports, and counts nothing and arms nothing otherwise. If a hand-over fails, its counts go with the next one. The Account DO keeps the time reporting last started and drops a batch opened before it, so a process that missed a stop can't send counts taken before it.
   - The Worker counts its requests and the Space DO RPCs that arrive over HTTP. Each request made while the server reports waits in `ctx.waitUntil()`, which can run for up to 30 seconds after the response is sent ([Context](https://developers.cloudflare.com/workers/runtime-apis/context/)). The Worker hands its batch over once the batch is due, or once 25 seconds pass with no new count, which leaves the hand-over call 5 seconds inside that limit. So a request that comes more than 25 seconds after the one before it costs at most one Account DO call and one row written, and requests closer together share one. A Worker is billed for CPU time, not for that wait ([Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/)). An isolate learns whether the server reports from the session check of an authenticated request or from the answer to a token refresh, so it counts nothing before the first of those.
   - A Space DO counts the RPCs that arrive over its WebSockets and sets no timer: a pending timer keeps a Durable Object from hibernating ([Durable Object lifecycle](https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/)), and an object that can't hibernate is billed for its time in memory ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)). After each RPC it parks its counts in the attachment of the socket that carried the RPC, which survives hibernation ([WebSockets](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)). When the batch is due, when that socket is gone, or when the attachment would pass 8 KiB, it hands over its own counts and those parked on every socket in one call.
   - When counts are first parked, the Space DO sets its alarm for one minute past the next UTC hour if it hasn't already, and the alarm hands over what is still parked before the Account DO drops that hour (rule 7). A socket that closes passes its parked counts to another open socket, or hands them over when no other socket is open.
   - A shutdown loses counts. A deploy, a runtime update or a crash drops the Worker's open batch: up to 10 minutes of one isolate's counts. A Space DO's parked counts survive hibernation, and a code update that the object adopts after hibernating, because both keep its sockets connected. A crash, or a code update that resets an object still active when the update's delay runs out, disconnects its sockets and drops what was parked on them: up to 10 minutes of that object's counts ([Durable Objects code updates](https://developers.cloudflare.com/durable-objects/deployments/durable-objects-code-updates/)). Only a storage write per request could prevent that, and the server doesn't make one.
6. About five minutes after each UTC hour ends, the server posts the hour's counts as one `server_report` event in a PostHog `/batch/` body to `https://tel.koidrive.com/batch/`. The event has a fresh random id, used as both `uuid` and `distinct_id`. Its other properties are `$process_person_profile: false`, `$geoip_disable: true`, `schema`, `server_version`, `proto_min`, `proto_max`, the counts as `c.<route>.<outcome>`, the buckets as `h.<route>` and the billing figures of section 4 as `b.<name>`. Nothing else is sent. Cloudflare adds a `CF-Worker` header naming the zone that owns the Worker to every subrequest ([HTTP headers](https://developers.cloudflare.com/fundamentals/reference/http-headers/#cf-worker)), so the endpoint receives that zone.
7. Counts for an hour whose post is already due are dropped, so each hour is posted once.
8. A failed post is tried again an hour later. Counts older than 24 hours are deleted unsent.

## 4. Billing figures

While it reports, the server also counts what Cloudflare bills for, so the project can see what a Koidrive server costs to run. These counters travel with the counts of section 3. Where this section differs from rule 5 of section 3, this section applies to both.

Each Durable Object keeps its reporting flag in storage and nothing else in memory between events:

- **Flag.** A Space DO reads its flag from one `meta` row in its constructor, and the Account DO reads the outcome of the votes (section 3, rule 3) in its constructor, so both know the flag before they count their first request. A Space DO writes the row only when an Account DO answer changes it; a new space takes the flag from the Account DO's `init` call. While the flag is off an object counts, parks, arms, calls and writes nothing for the report, so reports off cost one row read per wake.
- **Calls.** Every call into a Durable Object goes through one `metered(method, args)` entry. When the method returns, the object adds its active time and returns `{ value, usage, reporting }`: the value, every count the object made since its last event ended (the call itself, its rows, `setAlarm` calls and R2 operations, and the counts other objects returned to it during the call), and its flag. One stub adapter, used wherever a stub is made, unwraps the reply and gives `usage` and `reporting` to the caller: the Worker, a Space DO calling the Account DO, or the Account DO calling a Space DO. The caller adds the counts to its own and sends them on with its own event. An HTTP RPC's counts reach the Worker this way, with the RPC's whole active time. A call that throws ends like an event with no caller (below).
- **Events with no caller.** After a fetch (a WebSocket upgrade), a WebSocket message or close, or an alarm, a Space DO parks its counts on an open socket as section 3 describes. When no socket is open, or a parked batch is due, it hands everything over to the Account DO inside the event. An event that started a lease renewal waits for its answer, so the Account DO's counts for it leave with the event. The Account DO adds the counts it holds to the `usage_hours` rows `recordUsage` writes, and at the end of its alarm writes its own counts into the current hour's row. While reports are on, that row keeps the Account DO's report alarm running once an hour until the votes lapse.
- **Worker.** An isolate keeps its batch in memory, as section 3 describes. Every Durable Object reply carries a flag, so an isolate learns it from its first call into a Durable Object, not only from a session check. Until then it counts and hands nothing over, and it drops what it counted when the flag is off. An Account DO flag replaces the one the isolate holds; a Space DO flag only fills one the isolate doesn't have.

These counts can still be lost. The bounds are per day for one busy Worker isolate and one busy space, which is the load the overhead test in `server/test/report-cost.test.ts` models (144 Worker and 144 Space DO hand-overs a day), with 2^30-byte GB-s at 0.125 GB per object:

| Loss | Why | Bound per day |
| --- | --- | --- |
| An event killed mid-event | Its counts leave when it ends. A deploy that resets an active object, or a crash, ends it first. | One event per object per reset: with one deploy a day, at most 3 requests (one Worker request, one Space DO event, one Account DO event) and their rows, about 100 rows for an RPC. The Worker's open batch and a crash's parked counts are bounded in section 3. |
| An isolate that ends before a Durable Object reply tells it the flag | It holds what it counted while the flag was unknown. Only `GET /v1/server` and requests refused before any call into a Durable Object can come first. | At most one such request per isolate start: 24 requests and their response bytes at one start an hour. |
| The tail of a Worker hand-over | The rows the Account DO writes for a `recordUsage` call, and its time after it took its own counts, return to the isolate and wait for its next hand-over. | One tail per isolate end, at most 4 rows written and 5 rows read: 96 rows written and 120 rows read at one isolate end an hour. |
| The tail of a Space DO hand-over with no socket open | The same tail, and the Space DO's wait for it, come back to an object with nowhere to park them, so it drops them. With a socket open the tail is parked; it is dropped only if its hour is due before it is handed over. | At most 4 rows written, 5 rows read and 2 × 20 ms of active time per hand-over (2 and 3 rows when the hour's row exists): 576 rows written (0.58% of the Free daily limit), 720 rows read (0.01%) and 0.72 GB-s (0.006%) at 144 hand-overs a day. |
| The tail of the Account DO alarm | The row it writes for its own counts and the alarm it sets after that. | At most 3 rows written and 4 rows read per alarm: 72 rows written and 96 rows read at 24 alarms a day. |
| A failed hand-over with no socket open | The Space DO can't park the batch, and keeping it in memory would lose it at the next eviction anyway. | One alarm's or one last close's counts per failed call. A failing Account DO also fails every session check, so this happens only while the server is down. |
| The first events of a space after this change is deployed | A space created before it has no flag row, so it counts nothing until an Account DO answer tells it the flag, usually the session check of its first event. | Once per space: the events of its first wake. |
| R2 objects outside the gauges | Item 3 below. | Up to 288 export log segments a day per space with commits (one per 5 minutes at most, each at most 16 MiB), 2 kept snapshots, and the chunks whose upload isn't confirmed yet. |

| Name | What it counts |
| --- | --- |
| `worker_requests` | Requests the Worker handled. |
| `egress_bytes` | Response body bytes the Worker sent: the `Content-Length` when the response has one, otherwise the bytes as the body streams out. WebSocket messages aren't counted. Cloudflare doesn't bill egress. |
| `do_account_requests`, `do_space_requests` | Fetches, RPC calls and alarms the Account DO and the Space DOs handled. A call an object makes on itself isn't counted. |
| `do_ws_messages` | WebSocket messages the Space DOs received. Pings the runtime answers for the object don't reach it. |
| `do_account_active_ms`, `do_space_active_ms` | Milliseconds in which an object ran at least one event, summed over objects. Events that overlap in one object count once. |
| `do_rows_read`, `do_rows_written` | The `rowsRead` and `rowsWritten` of every SQL statement either class ran, and one row written for each `setAlarm` call, which Cloudflare bills as one row written ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)). |
| `r2_class_a` | R2 `put`, `list`, `createMultipartUpload`, `uploadPart` and `complete` calls. |
| `r2_class_b` | R2 `get` and `head` calls. Deletes and aborts are free and aren't counted. |

The report also holds three gauges, each a sum over objects when the Account DO builds the report:

| Name | What it holds |
| --- | --- |
| `do_stored_bytes` | The latest `ctx.storage.sql.databaseSize` of each object. |
| `r2_stored_bytes`, `r2_objects` | The size and number of the chunks each Space DO holds as live or being deleted. Each Space DO recounts them once per UTC day. |

1. While a Space DO knows the server reports, it sends its object id, database size and chunk totals once an hour, with the counts of its first event of the hour. The Account DO keeps the latest values per object and puts only the sums in the report. No object id leaves the Account DO.
2. When reporting stops, the Account DO deletes the per-object values with the unsent hours.
3. R2 objects the server doesn't track by size aren't in the R2 gauges: chunks whose upload isn't confirmed, and the export of each space (log segments, snapshot pages, manifests and `space.json`). Counting them would need an R2 list.
4. `estimateMonthlyCost` in `server/src/report/cost.ts` turns an hour's or a day's figures into an estimated monthly cost on Workers Paid and the share of each Workers Free daily limit they use. Its price table names the Cloudflare page each price comes from. Duration is estimated as active seconds × 0.125 GB, because Cloudflare bills each object for 128 MB.
5. The report holds no Worker CPU time, and the estimate leaves Workers CPU out. Cloudflare bills a Worker's CPU time and doesn't charge for its duration ([Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/)), and a Worker can't read its own CPU time.

## 5. Alternatives

- Workers Analytics Engine: a `writeDataPoint()` per request is durable and costs no storage write, but the hourly report would have to read the points back through the SQL API, which needs an account API token stored as a secret on every server ([Analytics Engine](https://developers.cloudflare.com/analytics/analytics-engine/), [SQL API](https://developers.cloudflare.com/analytics/analytics-engine/sql-api/#authentication)). That is a new credential and a new setup step.
