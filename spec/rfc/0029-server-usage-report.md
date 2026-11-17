# RFC 0029: The server's usage report

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
5. While it reports, the server counts each request by route and outcome, and each Space DO RPC by kind and outcome. The route and the kind are fixed names, such as `put_chunk` or `rpc_commit`. The outcome is the HTTP status, followed by the refusal code and the RFC 0028 cause when there is one. Each route or kind also gets 16 power-of-two latency buckets in milliseconds. Each process adds its counts to the Account DO at most once a minute, so up to a minute of one process's counts can be lost when the process ends.
6. About five minutes after each UTC hour ends, the server posts the hour's counts as one `server_report` event in a PostHog `/batch/` body to `https://tel.koidrive.com/batch/`. The event has a fresh random id, used as both `uuid` and `distinct_id`. Its other properties are `$process_person_profile: false`, `$geoip_disable: true`, `schema`, `server_version`, `proto_min`, `proto_max`, the counts as `c.<route>.<outcome>` and the buckets as `h.<route>`. Nothing else is sent. Cloudflare adds a `CF-Worker` header naming the zone that owns the Worker to every subrequest ([HTTP headers](https://developers.cloudflare.com/fundamentals/reference/http-headers/#cf-worker)), so the endpoint receives that zone.
7. Counts for an hour whose post is already due are dropped, so each hour is posted once.
8. A failed post is tried again an hour later. Counts older than 24 hours are deleted unsent.
