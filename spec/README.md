# Koidrive spec

The protocol schema shared by the server (`server/`) and the Rust clients (`core/`), as Protocol Buffers under `proto/koidrive/v1/`. This is draft v0: field numbers and messages may still change until the first release.

Run `buf lint` and `buf generate` in this directory. Generation writes TypeScript to `server/src/gen/` and Rust to `core/proto/src/gen/`; both are checked in.

## The RFCs change spec v0

Spec v0 was drafted outside this repository, and the schema here is its current form. Each RFC in [`rfc/`](rfc/) fixes a rule that v0 leaves open or changes one it states. Read an RFC together with the RFCs it builds on. Where an RFC amends an earlier RFC, the earlier one already holds the change unless the table says otherwise. Engine terms such as R, S and L and "design §N" refer to the [sync-engine design](../docs/design/sync-engine/README.md).

| RFC | What it settles | Builds on or amends |
|---|---|---|
| [0001](rfc/0001-v0-clarifications.md) | Name keys, the reserved `.koidrive` name, conflict names, commit size, share-link passwords and the wait after a platform limit | Spec v0 §6.1, §8, §13 and §15.2 |
| [0002](rfc/0002-commit-outcomes-and-recovery.md) | The history fence, the device counter, the order of commit checks, and recovery after a history change | Spec v0 §2, §4.1, §5.2, §7, §8 and R7, R8, R11; RFC 0001; design §6.4 and §10 |
| [0003](rfc/0003-space-rewind.md) | Rewinding a space to an earlier point, and keeping chunks safe across rewinds | Spec v0 §7.4, §10, §11, §12 and §15.1; RFC 0002 §6 and §8; RFC 0014 |
| [0004](rfc/0004-r2-export-format.md) | The file formats and write order of the R2 metadata export | Spec v0 §11; RFC 0002 §5; RFC 0003 §5 and §7 |
| [0005](rfc/0005-share-links.md) | Read-only share links: owner API, public API, viewer and reach | Spec v0 §15; RFC 0001 §8, whose lockout rule 1 it answers as `NOT_FOUND`; RFC 0003 |
| [0006](rfc/0006-agent-readiness-hooks.md) | Client handling of every error code, principals, authorization points, the device routes and stored-result checks | Spec v0 §2, §4.2, §6, §8, §9.1 and §15; RFC 0002 §2 and §3 step 2 (not folded into RFC 0002) |
| [0007](rfc/0007-name-limits-and-space-list-pages.md) | 255-byte limits on stored names, and paged space lists | Spec v0 §15; RFC 0006 §7 |
| [0008](rfc/0008-user-name-in-device-credentials.md) | The user's name in `DeviceCredentials` | Spec v0 §15; RFC 0007 |
| [0009](rfc/0009-device-revocation.md) | Who may revoke a device, what a revocation stops, and signing out | RFC 0006 §2, §3 and §7; RFC 0014 |
| [0010](rfc/0010-space-usage.md) | The read that reports a space's chunk bytes | RFC 0003 §4 and §5; RFC 0006 §3 |
| [0011](rfc/0011-refresh-credential-rotation.md) | Daily rotation of a device's refresh credential | Spec v0 §15.2; RFC 0006 §7; RFC 0009 |
| [0012](rfc/0012-trash-groups-across-a-loss.md) | Trash groups named by `commit_id`, so a lost commit's group is never reused | Spec v0 §4.1; RFC 0002 §2; design §6.1 |
| [0013](rfc/0013-commit-tokens.md) | Commit tokens, so a client notices commits the server lost | Spec v0 §7.3 and §7.5; RFC 0002 §3 step 4 (not folded into RFC 0002) |
| [0014](rfc/0014-session-leases.md) | Session leases between the Space DO and the Account DO | RFC 0003 §3 and §4; RFC 0006 §7; RFC 0009 §2 |
| [0015](rfc/0015-delete-preconditions.md) | A `Delete` that requires the kept file of a conflict answer | Spec v0 §6; RFC 0002 §4 and §9; RFC 0006 §8; design §2.7 and §4.4 |
| [0016](rfc/0016-identical-content.md) | An edit to the content a file already holds | Spec v0 §6.2 R3; design §4.4 |
| 0017 | Pending marks: the server keeps the name a folder waits for | Not in `rfc/`. RFC 0018 and RFC 0019 cite it, and its fields take `NodeState` 10 and 11 and `Move` 4 |
| [0018](rfc/0018-resends-and-deletes.md) | A delete beats a resend of a lost edit that it did not see | Spec v0 R7 and R8; RFC 0016 §1, RFC 0006 §8 and RFC 0004 §5 (not folded into them); RFC 0017 |
| [0019](rfc/0019-move-preconditions.md) | A `Move` that yields to changes it did not see | Spec v0 §6 and R7; RFC 0001 §6, RFC 0017 §3 and RFC 0018 §4, §7 and §8 (not folded into them) |
| [0020](rfc/0020-multi-user.md) | Several people share a deployment, with a role per space | Spec v0 §4.1, §5.2 and §7.1; RFC 0003 §1 and §6, RFC 0004 §4, RFC 0005 §2, RFC 0006 §2, §3 and §6, RFC 0009 §1 and §2, RFC 0010 §1 and RFC 0014 §2 (not folded into them); RFC 0022 |
| [0021](rfc/0021-human-auth.md) | A person signs in with a passkey or approves a new device | RFC 0006 §2 and §6, RFC 0009 §1 and RFC 0020 §2.2 and §3.1 (not folded into them); RFC 0020, RFC 0022 |
| [0022](rfc/0022-server-features.md) | The server announces the features it has turned on | Spec v0 §15 (`GET /v1/server`) and `Welcome` (not folded into it) |
| [0023](rfc/0023-trash-deletion-time.md) | When and from which device each node in the trash was deleted | Spec v0 §4.1, R8 and R9; RFC 0012 |
| [0024](rfc/0024-create-retries.md) | A resent create returns what the first request made | Spec v0 §15; RFC 0005 §2 and RFC 0008 (not folded into them) |
| [0025](rfc/0025-server-build-and-storage.md) | The server reports its storage versions and the commit it was built from | Spec v0 §15 (`GET /v1/server`); RFC 0022 |
| [0027](rfc/0027-chunk-bundles.md) | Several chunk uploads in one request | Spec v0 §15 chunk relay; RFC 0022 |
