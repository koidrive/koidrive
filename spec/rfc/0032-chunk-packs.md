# RFC 0032: Chunk packs

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0027 §2 and §6, RFC 0003 §5 and §7, and RFC 0004 §5. Adds the `chunk_packs` feature (RFC 0022). No `.proto` message changes.

A bundle (RFC 0027) still costs one R2 write per chunk. In deploy-test run 44391491657 (1.9M entries, 1.52M of them small files), a 32-chunk bundle took 4.1 s on the Worker for 16 ms of CPU: 32 writes, 6 at a time, p50 661 ms each. Six is the Workers limit on connections waiting for response headers ([Workers limits, simultaneous open connections](https://developers.cloudflare.com/workers/platform/limits/#simultaneous-open-connections)). Per-write latency rose from 221 to 687 ms as concurrent writes grew to about 90, so the file phase levels off near 130 writes/s.

This RFC stores the chunks of one bundle as one R2 object, a pack, and records each chunk's location as (pack, offset, size). Clients still address chunks by hash, and dedup stays per chunk. A bundle of 32 costs one Class A operation instead of 32 ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)). Unbundled chunks stay single objects. A bare section number such as §3 refers to this RFC.

## 1. Pack objects

1. A pack is stored under `spaces/<space_id>/packs/<pack_id>`. `pack_id` is a UUIDv7 that the Worker mints for each pack write (`uuidv7` in `server/src/ids.ts:5-17`). A pack id is never written twice, so it plays the role of an incarnation (RFC 0003 §5 rule 6). Uniqueness rests on the UUIDv7's 74 random bits, not on a counter. The write uses `onlyIf: { etagDoesNotMatch: "*" }`, so a collision fails safe: nothing is replaced, and the chunks get 503 and are resent into a new pack (§2 rule 4).
2. A pack is the chunk bodies in index order with no padding, then the index, then a 16-byte footer:
   - index: one 36-byte entry per chunk, the 32-byte SHA-256 then the size as a little-endian u32. A chunk's offset is the sum of the sizes before it.
   - footer: the 8 ASCII bytes `KDPACK01`, the entry count as a little-endian u32, and the index offset (the body length) as a little-endian u32.
3. A pack holds distinct chunks of 1 byte to 1 MiB, with a body of at most 4 MiB. A bundle writes 2 to 32 of them, within the bundle limits of `server/src/worker.ts:206-207` and `server/src/chunk-bundle.ts:7`. A compaction output (§7) may hold any number. R2 allows 5 GiB in one write ([R2 limits](https://developers.cloudflare.com/r2/platform/limits/)).
4. The index serves an R2-only restore (§8). The server keeps locations in the Space DO (§4) and never reads it.

## 2. Writing packs (amends RFC 0027 §2)

1. While `chunk_packs` is effective (RFC 0022 §3 rule 2), a Space DO adds `pack=1` to the query of every upload target it issues (`chunkUrl`, `server/src/space/space.ts:1108-1110`). The Worker packs only chunks whose target carries it. A Space DO can run older code than the Worker for 300 seconds after a deploy (RFC 0022 §3 rule 2), so the flag tells the Worker that the confirming DO reads v2 receipts. `pack=1` is not signed. A client that adds it itself can only fail its own commit.
2. The bundle route checks every chunk as today (`server/src/worker.ts:211-239`) and verifies its `put` ticket (`server/src/worker.ts:190`). For each flagged chunk whose ticket verifies, the Worker computes its SHA-256 itself, because R2 checks only the `sha256` of a whole object ([R2 Workers API, R2PutOptions](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#r2putoptions)). A mismatch answers that chunk 400, as `server/src/worker.ts:198` does.
3. When 2 or more flagged chunks pass, the Worker writes them as one pack, in request order, with one write. Every other chunk takes the single-object path of `storeChunk` (`server/src/worker.ts:178-204`), at most 6 at once.
4. If the pack write fails, every chunk in it gets the status a `PUT` would have got: 429 when R2 rate-limits, otherwise 503 (`server/src/worker.ts:199-200`). R2 answers a write its precondition refuses with `null` and does not throw, so a `null` result answers 503. Today's `storeChunk` ignores the result (`server/src/worker.ts:194`). Copying that would issue receipts for another pack's bytes on a collision. For storage failures only, this replaces the RFC 0027 §2 sentence "one chunk's failure does not change another chunk's result". The client resends each failed chunk (RFC 0027 §4 rules 3 and 4) into a new pack.
5. Each packed chunk's receipt token is `v2.<expires_at_ms>.<pack_id>.<offset>.<pack_size>.<mac>`. The MAC covers the grant `packed`, the space, the hash, the incarnation, the chunk size, the expiry, the pack id, the offset and the pack's total size. A v1 token stays `v1.<expires_at_ms>.<mac>` (`server/src/crypto.ts:51-58`). The client checks only a receipt's hash and size (`core/net/src/chunks.rs:893-896`) and sends the token back unread. So the location reaches the commit with no client or `.proto` change, and the receipt is the authority on it.

In RFC 0027 §6, "a storage failure fails only its chunk" and "a bundle of 32 makes 32 writes with at most 6 at once" now hold only for targets without `pack=1`.

Today's bundle route used 16 ms of CPU per bundle in run 44391491657, on deploy-test's plan. That is already above the 10 ms per request of Workers Free ([Workers limits, CPU time](https://developers.cloudflare.com/workers/platform/limits/#cpu-time)). Pack writing removes 31 write calls and adds up to 4 MiB of SHA-256. `chunk_packs` is off by default until item 6 of §11 measures p50 and p99 CPU per bundle on a Free account. It turns on by default only if p99 fits the Free limit.

## 3. Confirming locations at commit

`confirmUploads` (`server/src/space/space.ts:1068-1106`) gains these rules. A row is "confirmed" when it is `pending` and either has a location, or has `head_checked = 1`, or has a valid v1 receipt in this commit.

1. A v2 token counts for a row only when the row is `pending`, holds the token's incarnation (as v1 does today, `server/src/space/space.ts:1075-1080`), has no location yet, and the pack has no `packs` row or a `live` one. It also needs `uuidv7Ms(pack_id)` above now minus 6 days. An unexpired token meets that by construction, because the Worker mints the pack id and the expiry from one clock reading (`server/src/worker.ts:241`).
2. In one transaction before the commit transaction, the DO records each accepted location: it inserts the `packs` row (§4) if missing, sets `pack_id` and `pack_offset` where the row is still `pending`, holds the same incarnation, has no location and has `head_checked = 0`, adds the chunk to the pack's counts, and extends `hold_until_ms` by `HOLD_MS`, as the HEAD check does (`server/src/space/space.ts:1092-1104`). If two uploads stored one chunk, the first location recorded wins and the other copy is dead bytes from the start. A row with `head_checked = 1` is already confirmed as a single object, and a location would keep that object held where the orphan sweep never removes it.
3. **HEAD checks.** A row without a location whose only v2 token has expired gets the HEAD check of RFC 0002 §11, made against the pack instead of a chunk key. The MAC is checked without the expiry, the DO issues one `head` of the pack key per distinct pack, and accepts the location if the object exists with the token's pack size. A HEAD counts once per pack against `MAX_HEAD_CHECKS` (`server/src/space/limits.ts:13`) and can confirm up to 32 chunks. It is safe because of rule 1's 6-day bound: GC frees no pack younger than 7 days (§6) and the orphan sweep none younger than `chunk_grace_days`. Single-object rows keep their HEAD check (`server/src/space/space.ts:1086-1104`).
4. **Dedup.** `PrepareUpload` answers `PRESENT` for a `live` row in either format (`server/src/space/space.ts:1150`) and, new here, for a confirmed `pending` row. Today such a row gets `UPLOAD` (`server/src/space/space.ts:1151`), which with packs would store the chunk again. A `PRESENT` answer for a `pending` row MUST extend its `hold_until_ms` to now plus `HOLD_MS`, as `UPLOAD` does, or GC could delete the row before the commit that relies on it. Today's code extends the hold of every existing row it does not answer `RETRY_LATER` before choosing (`server/src/space/space.ts:1149`), so the new answer goes after that line. The commit then passes without a receipt, as a `head_checked = 1` row does (`server/src/space/space.ts:1077`, 1004-1012). So a chunk the server knows is never uploaded again. A chunk whose receipt no commit has presented is unknown, and a second device may upload a dead copy that §7 reclaims.

## 4. Schema

A step must leave a database that already has its effect unchanged (`docs/design/deploy-and-upgrade.md` §4). So the Space DO step appended to `MIGRATIONS` (`server/src/space/schema.ts:6-128`) is a guarded function like `server/src/space/schema.ts:62-67`. It adds `chunks.pack_id TEXT` and `chunks.pack_offset INTEGER` only where `pragma_table_info('chunks')` lacks them, then runs:

```sql
CREATE INDEX IF NOT EXISTS chunks_pack ON chunks(pack_id) WHERE pack_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS packs (pack_id TEXT PRIMARY KEY, size INTEGER NOT NULL, state INTEGER NOT NULL,
  live_count INTEGER NOT NULL, live_bytes INTEGER NOT NULL, hold_until_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS packs_sparse ON packs(live_bytes) WHERE state = 2 AND live_bytes * 2 < size;
```

The Account DO step (`server/src/account/schema.ts:6`) runs `CREATE TABLE IF NOT EXISTS pack_retirements (space_id TEXT NOT NULL, pack_id TEXT NOT NULL, retired_at_ms INTEGER NOT NULL, PRIMARY KEY (space_id, pack_id))`. Both `storage.space` and `storage.account` rise by one (RFC 0025 §2 rule 1).

- A row with `pack_id` NULL is a single object under `chunkKey(space, hash, incarnation)`, as today. A chunk's length is its `size` column, and a pack's key and creation time derive from `pack_id`.
- Pack states are `writing` (1, a compaction output before its swap), `live` (2), `drained` (3, emptied by compaction and kept until `hold_until_ms`) and `deleting` (4).
- `live_count` and `live_bytes` count the rows in state `pending` or `live` that point at the pack. Every transaction that adds, moves or deletes such a row updates them in the same transaction.
- The usage note's object and byte counts (`server/src/space/space.ts:748`) count single-object rows plus `packs` rows, with each pack's full size.

## 5. Reading packs

1. `GetDownloadTargets` (`server/src/space/space.ts:1174-1192`) issues, for a packed row, `{origin}/v1/spaces/{space_id}/chunks/{hash}?pack={pack_id}&off={offset}&len={size}&ticket={ticket}`. The ticket is `v1.<expires_at_ms>.<mac>`, with the MAC over `packed-get|<space>|<hash>|<pack_id>|<offset>|<size>|<expires_at_ms>`. Its own grant string keeps it from verifying as a `get` ticket of a single object. A single-object row keeps its `inc` target.
2. The Worker's chunk `GET` (`server/src/worker.ts:157-166`) handles a `pack` target with a ranged read of the pack key, `range: { offset, length: size }` ([R2 Workers API, ranged reads](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#ranged-reads)). It answers `Content-Length` from `len`, which the ticket binds. Today it uses `stored.size` (`server/src/worker.ts:164`), which is the whole object's size ([R2Object](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#r2object-definition)). R2 may return fewer bytes than asked, so a short body answers 503.
3. The client needs no change. It uses the target URL as given and checks every body against the chunk's hash and size (`core/net/src/chunks.rs:717-771`).
4. Share-link downloads resolve each chunk to a key (`server/src/share/space.ts:137-151`), and `openChunk` checks R2's stored SHA-256 and size (`server/src/share/routes.ts:78-93`). A packed chunk resolves to its pack key and range, and `openChunk` reads the range, at most 1 MiB, and checks its SHA-256 before streaming it.

## 6. Pack lifecycle and GC (amends RFC 0003 §5)

A pack is deleted only when no `pending` or `live` row points at it. GC (`collectGarbage`, `server/src/space/space.ts:1281-1420`) changes as follows.

1. **Packed rows.** The mark step (`server/src/space/space.ts:1347-1359`) selects rows by the same rule. A single-object row becomes `deleting` as today. A packed row is deleted in that transaction and its pack's counts drop. That issues no R2 delete, so it needs no `deleting` state and no retirement.
2. **Freeing.** A `live` or `drained` pack becomes `deleting` when `live_count` is 0, `hold_until_ms` has passed, and `uuidv7Ms(pack_id)` is more than 7 days ago. A `writing` pack that no running compaction owns becomes `deleting` too, at any age (§7 step 2). Completion turns every restored `writing` pack `live` (rule 5), so such a pack is younger than the last rewind and no history had a row on it. It never appears in a receipt either.
3. **Retirement.** The delete loop (`server/src/space/space.ts:1386-1411`) adds `deleting` packs to each batch. A new Account DO method `retirePacks(spaceId, packIds)` records them in `pack_retirements` (§4), refusing and pruning as `retireChunks` does (`server/src/account/account.ts:562-576`, RFC 0003 §5 rule 10). A new `retireChunks` argument would not do: an Account DO on older code would ignore it and answer ok, and the Space DO would delete packs with no record. On older code the new method fails, so no delete is issued (RFC 0003 §5 rule 8). Only after the Account DO confirms does the DO delete the pack keys, and then it deletes their `packs` rows. A crash at any point leaves the pack `deleting`, and the next pass repeats retirement and delete, both idempotent.
4. **Orphan sweep.** The sweep (`server/src/space/space.ts:1361-1384`) also lists `spaces/<space_id>/packs/`, with its own cursor. A pack key with no `packs` row, uploaded more than `chunk_grace_days` ago, with no rewind completed in that time, gets a `deleting` row and goes through rule 3. This removes packs no commit ever confirmed and outputs a crashed compaction left behind.
5. **Rewind completion.** Completion (`server/src/space/space.ts:1461-1499`) also reads the space's retired pack ids (as `server/src/account/account.ts:896-899` reads incarnations). In its transaction:
   - Every restored pack whose id is retired becomes `deleting`. Every row on it loses its location and incarnation and becomes `pending` with its refcount kept, as RFC 0003 §5 rule 9 does for retired incarnations.
   - Every other restored pack gets `hold_until_ms` of at least the completion time plus `chunk_grace_days`, the pack form of RFC 0003 §5 rule 2. A `writing` or `deleting` one becomes `live`, with counts recomputed from the restored rows, 0 if none point at it. GC frees it later through rule 2, once the hold has passed and the count is 0.
   - Every other packed row keeps its location and counts as confirmed.
6. **Retention.** No pack is retired earlier than `chunk_grace_days` after the last moment at which any history, discarded ones included, had a row with refcount above 0 on it. This is the retention invariant of RFC 0003 §5 for packs. A `pending` row on a pack has refcount 0, because the commit that raises a refcount also makes the row `live` (`server/src/space/commit.ts:515`).
   - Within one history, the last such row leaves a pack in one of two ways. Compaction moves it and sets the source's hold to the swap time plus `chunk_grace_days` (§7 step 5). Or GC deletes it `chunk_grace_days` after its refcount reached 0 (rule 1, `server/src/space/space.ts:1350-1351`). Rule 2 frees no pack before then.
   - Across a rewind that completes at time c, every unretired pack the restored state holds keeps its hold until at least c plus `chunk_grace_days` (rule 5). A pack that only the discarded history created has no row in the restored state, and the orphan sweep leaves it alone for `chunk_grace_days` after c (rule 4). Every moment the discarded history referenced a pack lies before c, so neither pack is retired before that moment plus `chunk_grace_days`.

   An admissible target T, in any history, is at least now minus 29 days (RFC 0003 §5), and a pack with a refcount-above-0 row at T is not retired before T plus 30 days. So rule 5 strips only rows with refcount 0, also on a second rewind into a discarded interval. For example:
   1. A pass records compaction output O as `writing`.
   2. A commit lands at tc. The state captured then has O `writing`.
   3. The swap at t5 moves rows to O, and O becomes `live`.
   4. At t5 + 1 h the owner rewinds to tc. Completion makes O `live` with count 0 and hold t5 + 1 h + 30 d. O's object, written before t5, stays.
   5. At t5 + 2 h the owner rewinds to t5 + 30 min, in the discarded interval. O's hold has not passed, so O is not retired, and the restored rows on O keep their location.

   A `live` pack whose count was 0 at the first target is covered the same way.

## 7. Compaction

A pack whose live bytes fall under half its size is rewritten. GC runs compaction after rule 3 of §6, on at most 16 source packs per pass.

1. **Select.** Read up to 16 `live` packs from `packs_sparse`, older than 7 days, fewest live bytes first, with their `pending` and `live` rows. The plan fills outputs of at most 4 MiB of body each. Steps 2 to 5 run once per output.
2. **Record.** One transaction inserts the output's `packs` row as `writing`, with a pack id the DO mints, owned by this pass in memory. A crash before step 5 leaves it without a running owner, and the next pass frees it (§6 rule 2). No row points at it, so that loses nothing.
3. **Read.** Read each source with one `get` and check each planned chunk's SHA-256. A missing or mismatched source leaves the plan and is logged. An empty plan frees the output.
4. **Write.** Write the output, chunks and index as in §1, with `onlyIf: { etagDoesNotMatch: "*" }`. A failure, or an unknown outcome, frees the output. A write that lands after that delete leaves a key with no row, which the orphan sweep removes (§6 rule 4).
5. **Swap.** One transaction checks that the output is still `writing` and that every `pending` or `live` row on each source is in the plan. If either check fails, the output is freed. Otherwise every planned row that still points at its planned source and offset moves to the output, the output becomes `live` with its counts, and each source becomes `drained` with counts 0 and `hold_until_ms` set to now plus `chunk_grace_days`. A row GC deleted since step 1 is not moved, even if a new upload recreated its hash in another pack, and its bytes are dead in the output.
6. **Free.** §6 rules 2 and 3 delete the sources once their hold passes.

Steps 2 to 5 interleave with commits, uploads and GC, since a Durable Object handles other events while it awaits R2. A commit changes refcount and state, not location. A receipt cannot add a row to a source (§3 rule 1 accepts no pack older than 6 days) or to the output, which stays `writing` until step 5. A download target issued before step 5 names the source, which outlives the 1-hour ticket (`server/src/space/limits.ts:9`). A rewind restarts the object (`server/src/space/space.ts:1448-1451`), and completion turns a restored `writing` pack `live` under a hold (§6 rule 5).

**Threshold.** A pack is compacted when `live_bytes * 2 < size`, so each compaction writes fewer bytes than it reclaims. Packs older than 7 days and not waiting for a pass are at least half live, so their storage stays under 2 times their live bytes, plus young packs and the sources of the last `chunk_grace_days` of compactions. A threshold of 1/4 would allow 4 times the live bytes, and 3/4 would rewrite 3 bytes for each byte reclaimed. At $0.015 per GB-month ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)), 10 GB of dead bytes costs $0.15 a month.

**Cost per GC pass.** At most 16 Class B `get`s, at most 8 Class A writes (16 sources under 2 MiB live fill at most 8 outputs), one `retirePacks` call, and deletes, which R2 does not charge for (same page). That is at most about $0.00004 per pass. Memory peaks near 8 MiB, one source plus one output. The pass sets `more` (`server/src/space/space.ts:1418`) when it used all 16 sources.

## 8. Export and restore (amends RFC 0004 §5 and RFC 0003 §7)

The exporter writes only files under `meta/<history_id>/` (`server/src/space/export.ts:171`) and never reads chunk rows, so it does not change. The descriptor (`server/src/space/export.ts:374-392`) does not change either: a history writes it once (RFC 0004 §6), so existing descriptors can never name packs.

An R2-only restore finds a chunk under `chunks/<hex2>/<hex>/`, as RFC 0004 §2 says. For a hash it does not find there, it reads packs. It lists `spaces/<space_id>/packs/` and reads each pack's footer and index ([ranged reads](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#ranged-reads)). One suffix read of 1,168 bytes covers a bundle pack of up to 32 entries. A compaction output may hold more, so for it the reader reads the footer first and then the index. It takes any pack that lists the hash and checks the bytes against the hash, so a drained or orphan pack serves as well as a live one. A reader refuses a pack whose footer magic is not `KDPACK01`. The 1.52M small files of run 44391491657 make about 47,500 packs of 32, so a restore makes about 48 list calls and 47,500 Class B reads.

## 9. Compatibility

- **Existing single objects need no migration.** A row with `pack_id` NULL behaves as today, and that format stays permanent for chunks of 256 KiB and more, which the client never bundles (`core/net/src/chunks.rs:105`, 948). A backfill would rewrite working data and save no writes. The §4 migrations add nullable columns, tables and indexes only.
- **Downgrade.** The migrations raise `storage.space` and `storage.account`, so a client refuses to install an older server over this one (RFC 0025 §3 rule 1).
- **Deploy window.** §2 rule 1 covers a DO older than the Worker. A Worker older than the DO ignores `pack=1` and writes single objects.
- **Turning it off.** `KOIDRIVE_FEATURES=-chunk_packs` (RFC 0022 §3 rule 1) stops new packs. A Space DO accepts v2 receipts and reads and collects existing packs whether or not the feature is on.
- **Clients.** Tokens and download URLs are opaque to clients, so every client release works with packs.

## 10. Tests

**Pack GC property.** A state-machine property test drives the Space DO and Account DO code against a model R2 in which every write and delete may apply at once, never, or at any later step, in any order ([R2 consistency](https://developers.cloudflare.com/r2/reference/consistency/)).

The state is the chunk rows (state, refcount, incarnation, `head_checked`, location), the `packs` rows (state, size, counts, hold, in-memory owner), R2 objects with in-flight writes and deletes, the Account DO's retired incarnations and pack ids, the receipts and targets clients hold, every Space DO state of the last 29 days in every history, and the clock.

Steps:
1. `PrepareUpload` of some hashes.
2. A bundle POST, with or without `pack=1` targets.
3. A chunk `PUT`.
4. A commit with any subset of the receipts held, expired ones included.
5. A version drop.
6. A GC mark step.
7. A free step.
8. A retirement call that succeeds, fails or has an unknown outcome.
9. Delete issue.
10. Row removal.
11. One page of the orphan sweep.
12. Each of compaction steps 1 to 5.
13. One in-flight R2 operation applying.
14. A crash, which drops every in-memory continuation and owner and keeps SQLite.
15. A rewind to any state of the last 29 days, including states in an interval an earlier rewind discarded, with completion.
16. A download of a held target.
17. The clock moving forward.

After every step:
1. **No live chunk's pack is freed.** Every `pending` or `live` row with a location names a pack that exists in R2, is not retired, and holds the row's bytes at the row's offset. A confirmed single-object row has the same property for its chunk key.
2. **Retention.** No rewind leaves a row with refcount above 0 without a readable location.
3. **Counts.** Each pack's `live_count` and `live_bytes` equal its `pending` and `live` rows. No such row points at a `writing`, `drained` or `deleting` pack.
4. **Delete isolation.** Every delete issued names a retired key.
5. **Downloads.** A target returns the chunk's bytes until it expires.

The harness must go red first on the sequence of §6 rule 6 against the mutant rule 5 that sets restored `writing` packs to `deleting` and leaves restored pack holds unchanged. A second property checks progress: after enough quiet GC passes, every pack without rows is deleted, and every `live` pack older than 7 days is at least half live.

**Server tests.** A flagged bundle makes one write whose receipts a commit accepts. A failed or `null` pack write fails every packed chunk with the `PUT` status, and a wrong-hash chunk stays out of the pack. A confirmed `pending` row gets `PRESENT` with its hold extended. An expired v2 receipt is accepted after one HEAD per pack. A ranged download and a share-link download each return exactly the chunk. A restore reader finds packed chunks from either index read.

## 11. Delivery

Each item is one PR, in this order (CREW.md "Engine PR structure"). Items 3 to 5 are dormant in production because nothing writes a pack until item 6.

1. **GC property harness.** Test only. It covers the single-object steps of §10 first, and items 3 to 6 add the pack steps. It can merge at any time.
2. **`PRESENT` for a confirmed `pending` row.** §3 rule 4 for single-object rows with `head_checked = 1`. It changes single-object behaviour, so it lands alone.
3. **Pack format and server read path.** The Space DO migration of §4, the pack codec, v2 receipts and the rest of §3, §5, and the usage counts. GC leaves rows with a location alone. Tests create packs as fixtures.
4. **Pack GC.** §6: freeing, `retirePacks` with the Account DO migration, the orphan sweep of `packs/`, and the rewind rules. It needs item 3.
5. **Compaction.** §7. It needs item 4, and only the waste bound depends on it.
6. **Pack writing.** The `chunk_packs` feature, `pack=1` targets, the Worker write (§2), the RFC 0027 §6 test changes, and the CPU measurement of §2. It needs items 3 and 4 in the same or an earlier release, and item 5 before `chunk_packs` is on by default. Reverting it stops new packs, and items 3 to 5 keep existing packs readable and collected.
7. **Client bundles.** Packs alone do not reach the target. At about 0.7 s per pack write and 4 bundles in flight, a client sends about 5.7 bundles/s, which is about 180 files/s against the roughly 550 entries/s needed. This item raises the chunks per bundle and sizes the client's bundle concurrency to the link instead of a constant, with numbers from item 6's measurement. It amends RFC 0027 §2 (the 32-chunk cap) and §4 rule 7 (bundle permits) for connections whose `Welcome` lists `chunk_packs`.

## 12. Alternatives considered

1. **Pack keys under the chunk key and incarnation scheme.** The Space DO allocates incarnations per row in `PrepareUpload` (`server/src/space/space.ts:1112-1116`), the bundle route never calls it, and a `pending` row gives one incarnation to every uploader (`server/src/space/space.ts:1151`). Two bundles could name one key with different bytes, and a restore listing `chunks/<hex2>/<hex>/` would read a pack as a chunk.
2. **An index file written by the exporter.** It adds an R2 write per change and must be ordered against compaction. The in-pack index costs 36 bytes per chunk.
3. **The Worker telling the Space DO about each pack at upload time.** Dedup would know a chunk at once, but every bundle, the hot path this RFC shortens, would pay a Space DO round trip.
4. **No compaction.** Up to 31/32 of a pack could be dead, so storage could reach 32 times the live bytes.
5. **Recording compaction moves in the Account DO.** Completion could then relocate restored rows, so sources could be freed sooner than the 30-day hold of §7 step 5. For 0.1 the hold is accepted instead.
