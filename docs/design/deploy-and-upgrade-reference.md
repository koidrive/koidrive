# Deploy and upgrade reference

The values behind [deploy-and-upgrade.md](deploy-and-upgrade.md). Section numbers match that page.

## Deploy configuration (section 1)

`server/wrangler.jsonc` sets:

| Key | Value |
|---|---|
| `name` | `koidrive` |
| `durable_objects.bindings` | `ACCOUNT` to `AccountDO`, `SPACE` to `SpaceDO` |
| `migrations` | Tag `v1` with `new_sqlite_classes: ["AccountDO", "SpaceDO"]`. Tags are append-only |
| `r2_buckets` | `BUCKET` to the bucket `koidrive` |
| `observability` | `enabled: true`, `redact_query_string: true` |
| `durable_objects.code_update_strategy` | `{"mode": "deferred", "max_delay": 300}` |

`server/scripts/check-deploy.mjs` fails CI when observability or query-string redaction is off, a binding or the default bucket name is missing, or the migration tags no longer start with `v1`. It also fails if `server/` holds `.dev.vars.example` or `.env.example`. Workers Logs keeps logs for 3 days on Workers Free and 7 days on Workers Paid ([Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/)).

## Claim and recovery (sections 2 and 3)

| Value | Setting |
|---|---|
| Claim code | 12 characters from a 32-symbol alphabet (60 bits), logged as `XXXX-XXXX-XXXX` |
| Claim code lifetime | 1 hour. At most one code is live |
| Wrong claim codes or recovery keys | 10 per client address in a 10-minute window, then `RETRY_LATER` until the window ends |
| Wrong pairing codes | 20 per client address in a 10-minute window |
| Client address | `CF-Connecting-IP`. Requests without it share one separate bucket |
| Failure rows kept | 10,000 at most |
| Recovery key | At least 32 characters. Each key works once |
| `setup_attempt` | At most 64 characters |
| Code comparison | SHA-256 digests compared with `crypto.subtle.timingSafeEqual` |

The log line reads `Koidrive claim code (valid for 1 hour): <code>`. The claim code is the only secret the server logs. `server/test/logging.test.ts` checks that no ticket, token or credential reaches the log during an upload and a download.

| Server state | Code presented | Answer |
|---|---|---|
| No owner | Live claim code | Creates the owner and the first device |
| No owner | Unused recovery key | Creates the owner and the first device, and deletes any claim code |
| No owner | Wrong code while one is live | `FORBIDDEN` "wrong claim code", counted as a wrong code |
| No owner | No live code | `FORBIDDEN` "no valid claim code; open /setup to issue one" |
| Owner | Unused recovery key | A new device of the owner |
| Owner | Used recovery key | `FORBIDDEN` "this recovery key was already used; set a new one" |
| Owner | Anything else | `FORBIDDEN` "this deployment already has an owner", counted as a wrong code |

## Storage (section 4)

| Class | Version table | Steps |
|---|---|---|
| `AccountDO` | `settings` | `MIGRATIONS` in `server/src/account/schema.ts` |
| `SpaceDO` | `meta` | `MIGRATIONS` in `server/src/space/schema.ts` |

A stored version above the step count throws "storage schema version N is newer than this server knows". `server/test/migrations.test.ts` upgrades storage from every earlier version and pins the schema after every step.

## Versions (section 5)

| Field | Source |
|---|---|
| `server_version` | `server/release.json`, written from `[workspace.package].version` in `Cargo.toml` by `server/scripts/release-version.mjs` |
| `commit` | `commit` in `server/release.json`, which only a release commit sets |
| `storage` | `STORAGE` in `server/src/storage.ts`: the length of each class's `MIGRATIONS`. `server/scripts/pack-bundle.mjs` writes the same numbers to the bundle manifest |
| `proto_min`, `proto_max` | `PROTO_MIN` and `PROTO_MAX` in `server/src/version.ts` |

`server/test/protocol.test.ts` keeps `PROTO_MIN <= max(1, PROTO_MAX - 1)`.
