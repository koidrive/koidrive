# RFC 0022: Server features

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §15 (`GET /v1/server`) and defines `Welcome.features`. Adds a field to `ServerInfo`.

A client can't tell whether the server it talks to offers a capability that a newer RFC adds. The owner upgrades the server on their own schedule (`docs/design/deploy-and-upgrade.md` §6), so a new client often meets an old server, and the protocol version alone does not say which routes exist. Work that ships before it is finished also needs a switch: a release can carry routes that stay off until the work is complete.

This RFC lets the server announce the features it has turned on, under names that later RFCs register. A bare section number such as §2 refers to this RFC.

## 1. Messages

- `ServerInfo`: `features = 7`, a repeated string.
- `Welcome.features` (8) exists and is empty today. It now carries the features that the sending Space DO has on (§3 rule 2). Outside the 300 seconds after a deploy, that is the same list as `ServerInfo.features`.

No field is removed or renumbered.

## 2. Feature names

A feature name is 1 to 64 characters of `a` to `z`, `0` to `9` and `_`, starting with a letter. The RFC that adds a feature registers its name in the table below and states what turning it on enables. Names are never reused for a different meaning.

| Name | RFC | Enables |
|---|---|---|
| `people` | RFC 0020 | Inviting people, and every principal of a user other than the deployment's owner |
| `human_auth` | RFC 0021 | Passkeys and browser sessions |
| `device_authorization` | RFC 0021 | The device authorization routes |
| `hosted_auth` | RFC 0021 | Sign-in through an external identity provider |
| `chunk_bundles` | RFC 0027 | Several chunk uploads in one request to `POST /v1/spaces/{space_id}/chunks` |
| `field_intents` | RFC 0028 | Clients send and read the intents of user changes, and recover from a server loss by comparing them |

The namespace is shared with the `Hello.features` that RFC 0006 §5 uses for change-record kinds. A record-kind feature appears in `ServerInfo.features` when the server can send records of that kind.

## 3. Server rules

1. **Configured.** Each release fixes which features are on by default. A deployment can change that only through the Worker variable `KOIDRIVE_FEATURES`, a comma-separated list where `name` turns a feature on and `-name` turns it off. A feature name the server does not know is ignored. The result is the configured list.
2. **Effective.** The Account DO and each Space DO can run an older release than the Worker for up to 300 seconds after a deploy (`code_update_strategy` in `server/wrangler.jsonc`). A configured feature is effective once the Account DO answers that it runs code that implements the feature, as `ServerInfo.setup_attempts` does today. `ServerInfo.features` lists every effective feature, in any order, without duplicates. `Welcome.features` lists the configured features that the sending Space DO's own code implements, so it can lag `ServerInfo.features` for the same 300 seconds.
3. **Not ready.** A route of a configured feature answers `RETRY_LATER` and changes nothing when the Account DO, or the Space DO of the space it names, still runs code without the feature. Each Space DO answers for its own space: the call the route makes fails on the older code, and the route turns that failure into `RETRY_LATER`. Every object runs the new code within 300 seconds, so the answer clears by itself, and a client with a stale list gets the same answer as a client with a current one. A route that writes and also calls a Space DO makes that call before its first write, so a `RETRY_LATER` leaves nothing stored.
4. **Off.** A feature that is not configured is off. Every route it enables answers 404, as an unknown path does today, except the routes of rule 6. An RPC or a WebSocket message of an off feature is answered as a request kind the server does not know (`INVALID_ARGUMENT`, `server/src/space/space.ts`). No message, record or reply of the feature is sent. Data that the feature wrote while it was on stays in storage.
5. **Off fails closed.** Turning a feature off never weakens a check and never keeps an access the feature granted. Authorization that an RFC adds applies whether its feature is on or off. While `people` is off, the server refuses every principal whose user is not the deployment's owner: token refresh answers `FORBIDDEN`, session checks answer closed (RFC 0014 §3), and no invite is redeemed. While `human_auth` is off, it refuses every browser principal the same way. Neither changes a stored row, so the principals work again when the feature is turned on.
6. **Routes that only reduce access are never gated.** Whatever the configured list says, the server answers the routes that read people data or only take access away, with the authorization their RFC sets: listing users, members, invites and sessions, removing a user, removing a member, revoking an invite, and signing out or revoking a browser session. `PUT /v1/spaces/{space_id}/members/{user_id}` answers while its feature is off only when it lowers an existing member's role. A deployment that turns a feature off can therefore always withdraw what the feature gave.
7. **Where the variable is set.** `KOIDRIVE_FEATURES` exists for tests and test deployments and is not part of the owner guide. Deploy-test passes it as a plain variable at deploy time. A CI check fails when it appears in the tracked `server/wrangler.jsonc`, which every new deployment copies. `koi setup` never sets it, and `koi server upgrade` removes a `KOIDRIVE_FEATURES` binding of any type, a secret included, which `keep_bindings` in `core/cloudflare/src/bundle.rs` would otherwise keep. A deployment that `koi` manages therefore carries the variable at most until its next upgrade. The server does not try to tell a test deployment from another one, because an owner who can set a Worker variable can deploy any code.
8. **Record kinds.** A record-kind feature that is off is left out of every connection, including connections whose `Hello.features` list it, in the way RFC 0006 §5 rule 3 describes. Records left out while it was off are not sent after it is turned on again. RFC 0006 §5 rule 4 makes this safe.

## 4. Client rules

1. A client offers a feature to the user only when the server lists it and the client implements it. The actions of §3 rule 6 are the exception: a client offers them whenever it implements them, because they must work while the feature is off. A 404 from one of them means the server does not implement the feature.
2. Before a client tells the user that the server lacks a feature, it reads `ServerInfo` again. If the feature is still absent, the client says that this server does not offer it, shows the server's `server_version`, and says that a newer server release may add it. The client does not try the feature's routes to find out.
3. A client reads `ServerInfo.features` when it sets up or adds a device, each time the app starts, and when `Welcome.server_version` or `Welcome.features` differs from what it stored, at most once per connection.
4. A 404 or `RETRY_LATER` from a route of a feature the client believes is effective means its list may be stale. The client reads `ServerInfo` again before it reports anything, and a 404 alone never tells it that an item is gone.
5. A client ignores names it does not know.

A 0.1 server sends no `features` field, which a client reads as an empty list.

## 5. Tests

- The configured list is the release default with the variable applied. `ServerInfo` lists exactly the effective features, and `Welcome` lists the configured features that the sending Space DO implements.
- An Account DO on the older code: the feature is absent from `ServerInfo`, and a direct call to one of its routes answers `RETRY_LATER` and changes nothing. A Space DO on the older code: the same call for that space answers `RETRY_LATER`, and the call for a space on the new code succeeds.
- A route of a feature that is off answers 404 for every principal, its RPC answers `INVALID_ARGUMENT`, and the authorization checks of that feature's RFC still hold.
- With `people` off, a member's device is refused at token refresh and at the session check, and works again after `people` is turned on. With `people` off, the owner lists users, removes a user, removes a member, lowers a role and revokes an invite, and raising a role answers 404. With `human_auth` off, a browser session is refused, and signing it out still works.
- The CI check fails on a copy of `server/wrangler.jsonc` that sets `KOIDRIVE_FEATURES`. `koi server upgrade` of a Worker that carries it as a plain variable, and of one that carries it as a secret, leaves neither.
- A client that finds a needed feature absent reads `ServerInfo` again, then reports the server's version and calls none of the feature's routes. A client that stored a list without `people`, against a server that has since turned it on, offers it after the next start without a `Welcome`. A 404 from a route of a feature the client believes is on makes it read `ServerInfo` again and delete nothing.

## 6. Alternatives considered

1. **Infer features from `server_version`.** A client would need a table of releases and features, and a test build would report a release number that does not match what it enables.
2. **Probe the routes.** A 404 does not distinguish a missing feature from a wrong path, and a probe of a route that changes state is unsafe.
3. **A build-time switch only.** A deployment for tests could not turn a feature on without a separate build, so the deploy test would not exercise the release that ships.
