# RFC 0029: The server's usage report

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §15 (`POST /v1/token`). Adds a field to `TokenRequest`.

A client sees a failed call only from the outside. The server sees which calls fail, why and how slowly. This RFC lets a server send an anonymous hourly summary of that to the Koidrive project, following the usage-report choice of the devices that use it. The person makes that choice once, when setup asks.

## 1. Messages

- `TokenRequest.telemetry = 6`, a `TokenRequest.Telemetry`: `TELEMETRY_UNSPECIFIED`, `TELEMETRY_ON` or `TELEMETRY_OFF`.

No field is removed or renumbered.

## 2. Client rules

1. A client sends `TELEMETRY_ON` while it sends usage reports, `TELEMETRY_OFF` while the person or the environment has turned them off, and `TELEMETRY_UNSPECIFIED` while the person hasn't answered.
2. A client that only prints reports and sends nothing (`KOIDRIVE_TELEMETRY=debug`) sends `TELEMETRY_OFF`.
3. The value goes on every token request. A change reaches the server with the next refresh, so within one access-token lifetime while the client runs.

## 3. Server rules

1. The server stores the value on the device that sent it, so each token refresh replaces that device's previous value.
2. The server reports while at least one active device last sent `TELEMETRY_ON` and no active device last sent `TELEMETRY_OFF`. A device that is revoked or whose credential expired doesn't count.
3. While the server doesn't report, it keeps no counts and sends nothing. When reporting stops, the server deletes the counts it hasn't sent.
4. While it reports, the server counts each request by route and outcome, and each Space DO RPC by kind and outcome. The route and the kind are fixed names, such as `put_chunk` or `rpc_commit`. The outcome is the HTTP status, followed by the refusal code and the RFC 0027 cause when there is one. Each route or kind also gets 16 power-of-two latency buckets in milliseconds. Each process adds its counts to the Account DO at most once a minute, so up to a minute of one process's counts can be lost when the process ends.
5. About five minutes after each UTC hour ends, the server posts the hour's counts as one `server_report` event in a PostHog `/batch/` body to `https://tel.koidrive.com/batch/`. The event has a fresh random id, used as both `uuid` and `distinct_id`. Its other properties are `$process_person_profile: false`, `$geoip_disable: true`, `schema`, `server_version`, `proto_min`, `proto_max`, the counts as `c.<route>.<outcome>` and the buckets as `h.<route>`. Nothing else is sent.
6. A failed post is tried again an hour later. Counts older than 24 hours are deleted unsent.
