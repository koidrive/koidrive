# RFC 0025: The server's build and storage versions

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §15 (`GET /v1/server`). Adds fields to `ServerInfo`.

`ServerInfo.server_version` names a release only when the server was built from one. A server built from a branch reports `0.0.0-dev`, which says nothing about how far its storage has moved. An owner who installs an older release over it can leave the server unable to read its own data, because each Durable Object refuses to start on a `schema_version` newer than its code (`docs/design/deploy-and-upgrade.md` §4). This RFC lets the server report its storage versions and the commit it was built from, so a client can refuse such a downgrade.

## 1. Messages

- `ServerInfo.storage = 8`, a `ServerInfo.Storage` with `account = 1` and `space = 2`.
- `ServerInfo.commit = 9`, a string.

Field 7 belongs to RFC 0022. No field is removed or renumbered.

## 2. Server rules

1. `storage.account` is the number of migration steps the Account DO class runs, and `storage.space` the number the Space DO class runs. Each is the `schema_version` an object of that class has after it starts on this code. The Worker reports the numbers of its own code, so for up to 300 seconds after a deploy an object can still run an older step count.
2. `commit` is the full hexadecimal id of the koidrive/koidrive commit the release was cut from, or empty when the build did not record one.

## 3. Client rules

1. A client that installs a server over an existing one compares the existing server's `storage` with the step counts of the server it installs. It refuses when either existing count is higher, unless the person overrides the refusal.
2. A `server_version` of `0.0.0` with any pre-release or build suffix, or one that is not a semantic version, says nothing about age. A client never treats it as older than a release.
3. A server without this RFC sends no `storage`. If its `server_version` is a release, the client compares versions. Otherwise it refuses unless the person overrides the refusal.
