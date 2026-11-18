# RFC 0001: v0 clarifications

Status: proposed. Applies to spec v0 (format_version 1).

Spec v0 was drafted outside this repository. This RFC records rules that the first server and engine implementations needed and that v0 leaves open. Section and rule numbers refer to spec v0. None of these changes touches the schema.

## 1. Name keys (§6.1)

`name_key` is `NFD(casefold(NFD(name)))`, the Unicode canonical caseless match (Unicode D145), instead of `casefold(NFC(name))`. Full case folding of an NFC string can produce a string that is not normalized. For example, `ΐ` (U+0390) and `Ϊ́` (U+03AA U+0301) are case variants of each other, but the v0 formula gives them different keys, so both could be live in one folder.

Pinned data:

- `casefold` maps each code point through `CaseFolding.txt` from Unicode 18.0.0, statuses C and F. Statuses S and T are not used, and code points without an entry map to themselves.
- NFC and NFD are the forms defined by UAX #15 for Unicode 18.0.0.

Implementations must produce the results of those versions. Under the Unicode stability policies, the case folding and normalization of an assigned code point never change. A library built on an earlier version therefore gives the pinned result for every name whose code points are all assigned in that version.

Known limitation: this is the one exception to the requirement above. The server folds case with its own copy of the 18.0.0 `CaseFolding.txt`, but it normalizes with the runtime's ICU data. If that data is older than 18.0.0, the server's NFD can differ from the pinned NFD for names that contain code points the data treats as unassigned. Such names are still accepted, because refusing to sync a file would be worse. Their stored `name_key` can differ from the pinned key. Two canonically equivalent names can then both stay live, and so can two case variants whose match depends on decomposing such a code point.

A runtime that later reaches 18.0.0 normalization does not repair this: keys already stored and clashes already live stay as they are. No repair migration exists yet; one would recompute every stored `name_key` and resolve the clashes it finds, under rules set by the RFC that adds it. Moving to a newer Unicode pin is a separate change. It raises `format_version` and recomputes every key, as described below.

The server computes `name_key` and is the only party that enforces uniqueness among live siblings. A client that computes a different key has only mispredicted a clash, and the server resolves it with R1 or R2.

The versions belong to the space format (§9.2). Moving to a newer Unicode version raises `format_version`, and the server recomputes every stored `name_key` in that migration. The RFC that raises the version defines how a clash created by the new data is resolved.

[`spec/vectors/name-keys.json`](../vectors/name-keys.json) lists test vectors: each name with its NFC form and its `name_key`. Both implementations test against it.

## 2. Reserved name at the root

`.koidrive` is reserved directly under the space root, compared by `name_key`, so `.Koidrive` and `.Koidrive` (with the Kelvin sign) are reserved too. A `CreateDir`, `CreateFile`, `Move` or `Restore` that would put a live node with that name directly under the root is rejected with `REJECTED` and reason `INVALID_NAME`. A node that R8 revives directly under the root with a reserved stored name is treated as an R1 clash and gets a conflict name. The name is allowed at any other depth. Clients keep their local state in `.koidrive/` at the root of the synced folder and never sync it.

## 3. UpdateContent on a directory

An `UpdateContent` whose target is a directory is rejected with `REJECTED` and reason `NOT_FILE` (R11). `NOT_FILE` joins the reject reasons listed in §8.

## 4. CreateDir onto a live file

R2 (merge) applies only when the existing live node with the same `name_key` is a directory. When it is a file, the new directory goes through R1: it is created under a conflict name and the result is `APPLIED_RENAMED`.

## 5. Conflict names (§6.1)

A conflict name is `<stem> (conflict from <device> <YYYY-MM-DD>[ <n>])<ext>`, built in this order:

1. Remove `/` and NUL from the committing device's name. Keep its first 32 Unicode scalar values.
2. Remove scalar values from the end of the result until its NFC form is at most 128 bytes. If nothing is left, use `unknown device`.
3. Measure the whole name on its NFC form, because NFC can make text longer. For example, U+0A5E becomes U+0A2B U+0A3C, and U+1D160 grows from 4 to 12 bytes.
4. If the name is longer than 255 bytes, remove scalar values from the end of the stem until it fits.
5. If it is still too long with an empty stem, remove scalar values from the end of the extension until it fits.
6. The result is the NFC form.

Step 2 keeps the part from ` (conflict from` through `)` within 156 bytes plus the digits of `n`. So the date and the attempt number are never cut, and different attempts always give different names.

A client also gives a conflict name. Suppose a device keeps a folder it does not sync, such as a folder it keeps after another device deleted it or one its rules leave out, and another device makes a live file at that folder's name. The first device does not download the node there. It renames the node to a conflict name built as above with its own device name, by a `Move` with `from` (RFC 0019) that names the node's current place. It sends this only for a node with no pending name (RFC 0017) that no open question or unknown rules hold back. The `from` check makes the rename yield to every change of the node it did not see. So when several devices keep that name, the first rename applies, the others are `SUPERSEDED`, and every device downloads the node under the name that won. A folder made at that name is not renamed: a kept folder takes it over, and the rules that leave out a folder leave out a folder of the same name too. A file this device keeps out of sync gives no conflict name, and the server's file at its name stays reported as a name taken here.

## 6. No-op Move

A `Move` of a live node to its current parent under its current name, compared byte for byte after NFC, is `APPLIED` and touches no node. `meta_rev` and `meta_dev` keep their values. So a no-op Move never counts as an unseen change, and it can never make another device's concurrent `Delete` keep the node under R7. Other ops in the same commit may still emit the node's state, and a commit whose ops are all no-ops still takes a seq. A rename that changes only case is not a no-op.

## 7. Commit size (§13)

A commit is accepted only if both of these fit in one `ServerMessage` of at most 1 MiB (§7.2):
- its `CommitRecord`, as the only record of a `Changes` message;
- its `CommitResult`, as the result of an `RpcResponse` whose `req_id` is the largest `uint64`, so that the outcome does not depend on the `req_id` of the request or of a retry.

A `CommitRecord` cannot be split across messages, so this is the effective bound on a commit. A commit that exceeds it is rejected with `LIMIT_EXCEEDED` and changes nothing.

The server stores each commit's change records in one log row, and the serialized `CommitRecord` and `CommitResult` together must stay under 7,900,000 bytes. That bound sits below the 8 MB Durable Object SQLite row limit ([Durable Objects limits](https://developers.cloudflare.com/durable-objects/platform/limits/)). It is a storage ceiling only: the 1 MiB bound always keeps a commit far below it, so it never decides whether a commit is accepted.

A client that gets `LIMIT_EXCEEDED` for a commit splits its changes into several commits. To delete a very large subtree, it deletes the subtree's contents, files and folders alike, in bounded batches over earlier commits, deepest first.

## 8. Share-link passwords (§15.2)

### 8.1 Key derivation

- The input is the UTF-8 encoding of the password exactly as given, with no Unicode normalization and no trimming.
- The salt is 16 bytes from a cryptographically secure random generator, fresh for each protected link.
- The derived key is PBKDF2-HMAC-SHA256 with 100,000 iterations and a 32-byte output.
- The Account DO stores four columns for each protected link in `share_links`:
  - `password_kdf`: TEXT, `pbkdf2-sha256`.
  - `password_salt`: the 16-byte salt, as a BLOB.
  - `password_iterations`: INTEGER.
  - `password_hash`: the 32-byte derived key, as a BLOB.
  
  A link without a password has NULL in all four.
- A server that does not support a link's `password_kdf` refuses to open the link, with `INTERNAL`, and does not fall back to another KDF.
- A stronger KDF gets a new identifier. Existing links keep theirs and need no migration.

Known deviation: the [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) recommends 600,000 iterations for PBKDF2-HMAC-SHA256. Cloudflare's production Workers runtime limits WebCrypto PBKDF2 to 100,000 iterations. Per [cloudflare/workerd#1346](https://github.com/cloudflare/workerd/issues/1346), standalone workerd made that limit configurable while production kept it.

### 8.2 Lockout

Each protected link keeps `failed_attempts` and `locked_until_ms`. This state belongs to the link alone, not to a link-and-client pair, and every caller shares it. Opening a link works as follows:

1. An unknown, revoked or expired link is refused with `FORBIDDEN` before any password check.
2. While `now < locked_until_ms`, the attempt returns `RETRY_LATER`. The password is not checked, and neither the counter nor the lock changes.
3. Otherwise a missing or wrong password is a failure. Each failure adds one to `failed_attempts` and returns `UNAUTHENTICATED`. The fifth consecutive failure also sets `locked_until_ms = now + 15 minutes` and resets `failed_attempts` to 0.
4. A correct password resets `failed_attempts` to 0 and opens the link.
5. When the lock expires, the counter is already 0, so five more failures are needed to lock the link again.

The public share API answers rule 1's refusal with `NOT_FOUND` (RFC 0005 §4).

The Account DO applies attempts on one link one at a time. After deriving the key, it re-reads the link, then checks the lock and updates the counter in one step with no other work in between. Concurrent attempts are therefore counted one by one, and any attempt that finishes after the lock is set gets `RETRY_LATER`. Because the counter is per link, anyone who knows a link can lock it for its legitimate users. v0 accepts this trade-off.

## 9. Wait after a platform limit (§13)

§13 gives `QUOTA_EXCEEDED` for a platform daily limit a `retry_after_ms` until the next 00:00 UTC, when the Workers Free daily limits reset. Users report that Durable Objects free-tier errors can continue for some minutes after the reset. In two reports they lasted until about 00:20 UTC ([summary](https://billflare.dev/guides/community-durable-objects-free-sql-quota-after-reset)). A client that retries in that window would get a wait until the next midnight and stop syncing for almost another day.

So the server sets `retry_after_ms` from the time it sends `QUOTA_EXCEEDED`:

- less than 30 minutes after 00:00 UTC: 5 minutes;
- at any other time: the time until the next 00:00 UTC.

The rule covers every `QUOTA_EXCEEDED` the server sends for a platform limit, a full Durable Object storage included.
