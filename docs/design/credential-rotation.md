# Refresh credential rotation

Each device replaces its refresh credential once a day, so a copy taken from a backup or another machine stops working within about a day of the original's next rotation. [RFC 0011](../../spec/rfc/0011-refresh-credential-rotation.md) defines the protocol: the device proposes its next credential with a deadline, and the server stores it in place of the old one if the request arrives before the deadline. This page explains how the server, `core/net`, `core/driver` and the CLI implement it.

## The server keeps one credential and a clock per device

`devices.refresh_hash` holds the one credential the server accepts, and a rotation overwrites it. The Account DO's `rotation_clock_ms` column is the device's rotation clock (RFC 0011 §1). `refreshAccess` in `server/src/account/account.ts` hashes the presented and proposed credentials first, because hashing is asynchronous. Then, with no `await` between the read and the write, it reads the time, checks the device, and computes T as the later of that time and the stored clock. One `UPDATE` sets `last_seen_ms`, advances the clock to T, and replaces `refresh_hash` when T is at most the proposal's deadline. Every refresh already writes `last_seen_ms`, so a rotation adds no write. The Durable Object's output gate holds the answer until that write is durable, so a client never sees a time the stored clock does not hold.

## One `Auth` per process rotates the credential

After a rotation the server refuses the old credential. So a process uses one `Auth` for every call that presents the credential. `Driver::auth` builds it once per profile, and folder tasks, `link`, the sign-out revocation and the macOS app's server calls (`Driver::signed_in`) all share it.

`Auth::rotating` in `core/net/src/auth.rs` follows RFC 0011 §2. When the credential is older than `ROTATION_PERIOD` (24 hours), it stages a new credential with the deadline `server_time_ms` plus 60 seconds, proposes it, and makes it current once the server answers `rotated`. One refresh runs at a time, and it alone reads or changes the slots. If the answer is lost, both slots stay, and the next refresh settles which one the server holds. If storing a promotion fails, the `Auth` presents the new credential from memory and does not rotate again until the write succeeds.

## The profile keeps two files so older builds can still read it

`credential.json` has the format every build reads. `Credential` has `deny_unknown_fields`, so an added field would make the file unreadable to an older build. Its modification time is the credential's age. `credential.next` holds the staged credential and its deadline only while a rotation's outcome is unknown, and an older build ignores it.

`CredentialFiles` in `core/driver/src/credential.rs` writes each file to a temporary file with mode 0600, excludes it from Time Machine, syncs it, renames it into place and syncs the directory. Promoting writes `credential.json` first and removes `credential.next` second. A crash in between leaves both files with the same credential, which the next refresh settles. A write goes ahead only while `credential.json` still holds this device, so a refresh that ends after a sign-out writes nothing. Sign-out deletes both files.

## Readers retry until both files hold one state

Only the process that holds the profile lock writes the files. A CLI command that calls the server directly, such as `koi device list` or `koi space usage`, uses `Auth::reloading` instead. When the server refuses its credential, the command reads the files again once and presents the current and then the staged credential. It refuses a reloaded credential for another server or device.

`Credential::load_stored` reads both files as one state. It notes the identity of `credential.next` (device, inode and modification time) and reads it, reads `credential.json` and notes its identity, then checks both identities again. The writer replaces files by renaming new ones into place, so an unchanged identity means an unchanged file. If either changed, the reader starts over, at most 10 times, and then fails with an I/O error instead of reporting a sign-out.

An older build that starts while `credential.next` exists uses `credential.json`. If the server already stored the staged credential, that build is refused and signed out. That refusal happens only on a downgrade in the minute a rotation is in flight.

## Tests cover each step of the client and the server

- `server/test/rotation.test.ts`: deadlines, a clock that moves backward, malformed and revoked proposals, and two proposals from one credential.
- `core/net/src/auth/rotation_tests.rs`: a stub server that drops, holds and delays answers for each step of RFC 0011 §2, and a reader that reloads.
- `core/driver/src/credential.rs` and `core/driver/src/driver.rs`: file age, private files, a reader that starts over, a crash between the two writes of a promotion, and one `Auth` shared by linking and sign-out.
- `cli/tests/server.rs`, against `wrangler dev`: a command that reads the credential again after a rotation, and two copies of one credential where only the one that rotates keeps working.
