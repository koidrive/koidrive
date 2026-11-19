# RFC 0032: Chunk packs

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0027 §2 and §6, RFC 0003 §5 and §7, and RFC 0004 §5. Adds the `chunk_packs` feature (RFC 0022). No `.proto` message changes.

A bundle (RFC 0027) still costs one R2 write per chunk. In deploy-test run 44391491657 (1.9M entries, 1.52M of them small files), a bundle of 32 chunks took 4.1 s on the Worker for 16 ms of CPU: 32 writes, 6 at a time, at a p50 of 661 ms each. Six is the Workers limit on connections waiting for response headers ([Workers limits, simultaneous open connections](https://developers.cloudflare.com/workers/platform/limits/#simultaneous-open-connections)). Per-write latency rose from 221 to 687 ms as concurrent writes grew to about 90, so the file phase levels off near 130 writes/s.

This RFC stores the chunks of one bundle as one R2 object, a pack. The server records each chunk's location as (pack, offset, size). Clients still address chunks by hash, and dedup stays per chunk. A bundle of 32 then costs one Class A operation instead of 32 ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)). Chunks that are not bundled stay single objects, and both formats stay readable for good. A bare section number such as §3 refers to this RFC.

## 1. Pack objects

1. A pack is stored under `spaces/<space_id>/packs/<pack_id>`. `pack_id` is a UUIDv7 that the Worker mints for each pack write (`uuidv7` in `server/src/ids.ts:5-17`). A pack id is never written twice, so a pack key plays the role an incarnation plays for a chunk key (RFC 0003 §5 rule 6). The write uses `onlyIf: { etagDoesNotMatch: "*" }`, so a collision fails the write instead of replacing an object.
2. A pack is the chunk bodies in index order with no padding, then the index, then a 16-byte footer:
   - index: one 36-byte entry per chunk, the 32-byte SHA-256 then the size as a little-endian u32. A chunk's offset is the sum of the sizes before it.
   - footer: the 8 ASCII bytes `KDPACK01`, the entry count as a little-endian u32, and the index offset (the body length) as a little-endian u32.
3. A pack holds distinct chunks of 1 byte to 1 MiB, with a body of at most 4 MiB. A bundle writes 2 to 32 of them, within the bundle limits of `server/src/worker.ts:206-207` and `server/src/chunk-bundle.ts:7`. A compaction output (§7) may hold any number. R2 allows 5 GiB in one write ([R2 limits](https://developers.cloudflare.com/r2/platform/limits/)).
4. The index makes a pack self-describing for an R2-only restore (§8). The server never reads it, because it keeps locations in the Space DO (§4).

## 2. Writing packs (amends RFC 0027 §2)

1. While `chunk_packs` is effective (RFC 0022 §3 rule 2), a Space DO adds `pack=1` to the query of every upload target it issues (`chunkUrl`, `server/src/space/space.ts:1108-1110`). The Worker packs only chunks whose target carries it. A Space DO can run older code than the Worker for 300 seconds after a deploy (RFC 0022 §3 rule 2), so the flag, not the Worker's version, decides that the DO which will confirm a chunk can read its receipt. `pack=1` is not signed: a client that adds it to an older DO's target gets a receipt that DO rejects, which fails only that client's commit.
2. The bundle route checks every chunk as today (`server/src/worker.ts:211-239`) and verifies its `put` ticket (`server/src/worker.ts:190`). For each flagged chunk whose ticket verifies, the Worker computes its SHA-256 itself, because R2 checks only the `sha256` of a whole object ([R2 Workers API, R2PutOptions](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#r2putoptions)). A mismatch answers that chunk 400, as `server/src/worker.ts:198` does.
3. When 2 or more flagged chunks pass, the Worker writes them as one pack, in request order, with one write. Every other chunk takes the single-object path of `storeChunk` (`server/src/worker.ts:178-204`), at most 6 at once.
4. If the pack write fails, every chunk in it gets the status a `PUT` of it would have got for that error: 429 when R2 rate-limits, otherwise 503 (`server/src/worker.ts:199-200`). A write refused by its precondition answers 503. So one storage failure now fails every chunk of the pack, which replaces the RFC 0027 §2 sentence "one chunk's failure does not change another chunk's result" for storage failures only. The client already resends each failed chunk (RFC 0027 §4 rules 3 and 4), and a resend goes into a new pack.
5. Each packed chunk's receipt token is `v2.<expires_at_ms>.<pack_id>.<offset>.<pack_size>.<mac>`. The MAC covers the grant `packed`, the space, the hash, the incarnation, the chunk size, the expiry, the pack id, the offset and the pack's total size. A v1 token stays `v1.<expires_at_ms>.<mac>` (`server/src/crypto.ts:51-58`). The client checks only a receipt's hash and size (`core/net/src/chunks.rs:893-896`) and sends the token back unread, so the location reaches the commit with no client or `.proto` change, and the receipt, not the client, is the authority on location.

RFC 0027 §6 changes accordingly: "a storage failure fails only its chunk" and "a bundle of 32 makes 32 writes with at most 6 at once" hold for targets without `pack=1`. For flagged targets, a bundle of 32 makes 1 write, and a failed write fails all 32 with the `PUT` status.

Unverified: the Worker's CPU time per bundle with per-chunk hashing. The measured 16 ms already exceeds the 10 ms per request of Workers Free ([Workers limits, CPU time](https://developers.cloudflare.com/workers/platform/limits/#cpu-time)). Pack writing removes 31 write calls and adds up to 4 MiB of SHA-256. PR 4 (§11) measures it on a Free account before `chunk_packs` is on by default.

## 3. Confirming locations at commit

`confirmUploads` (`server/src/space/space.ts:1068-1106`) gains these rules. A row is "confirmed" when it is `pending` and either has a location, or has `head_checked = 1`, or has a valid v1 receipt in this commit.

1. A v2 token counts for a row only when the row is `pending`, holds the token's incarnation (as v1 does today, `server/src/space/space.ts:1075-1080`), has no location yet, and the pack has no `packs` row or a `live` one. It also needs `uuidv7Ms(pack_id)` above now minus 6 days. An unexpired token meets that by construction, because the Worker mints the pack id and the expiry from one clock reading (`server/src/worker.ts:241`).
2. In one transaction before the commit transaction, the DO records each accepted location: it inserts the `packs` row (§4) if missing, sets `pack_id` and `pack_offset` where the row is still `pending`, holds the same incarnation and has no location, adds the chunk to the pack's counts, and extends `hold_until_ms` by `HOLD_MS`, as the HEAD check does (`server/src/space/space.ts:1092-1104`). If two uploads stored one chunk, the first location recorded wins and the other copy is dead bytes from the start.
3. **HEAD checks.** A row without a location whose only v2 token has expired gets the HEAD check of RFC 0002 §11, made against the pack instead of a chunk key. The MAC is checked without the expiry, the DO issues one `head` of the pack key per distinct pack, and accepts the location if the object exists with the token's pack size. A HEAD counts once per pack against `MAX_HEAD_CHECKS` (`server/src/space/limits.ts:13`), so one HEAD can confirm up to 32 chunks. Rule 1's 6-day bound keeps this safe: GC never frees a pack younger than 7 days (§6), and the orphan sweep never touches one younger than `chunk_grace_days`, so a pack that passes the HEAD has not had a delete issued. Single-object rows keep the HEAD check of `server/src/space/space.ts:1086-1104` unchanged.
4. **Dedup.** `PrepareUpload` answers `PRESENT` for a `live` row in either format (`server/src/space/space.ts:1150`) and, new here, for a confirmed `pending` row. Today such a row gets `UPLOAD` (`server/src/space/space.ts:1151`), which with packs would store the chunk again. A commit that relies on `PRESENT` for a confirmed row passes the readiness check without a receipt, as a row with `head_checked = 1` does today (`server/src/space/space.ts:1077`, 1004-1012). So a chunk the server knows in any pack or as a single object is never uploaded again. A chunk whose receipt no commit has presented yet is unknown to the server, and a second device can upload it in the meantime. Its second copy is dead bytes that §7 reclaims.

## 4. Schema

One Space DO migration, appended to `MIGRATIONS` (`server/src/space/schema.ts:6-128`):

```sql
ALTER TABLE chunks ADD COLUMN pack_id TEXT;
ALTER TABLE chunks ADD COLUMN pack_offset INTEGER;
CREATE INDEX chunks_pack ON chunks(pack_id) WHERE pack_id IS NOT NULL;
CREATE TABLE packs (pack_id TEXT PRIMARY KEY, size INTEGER NOT NULL, state INTEGER NOT NULL,
  live_count INTEGER NOT NULL, live_bytes INTEGER NOT NULL, hold_until_ms INTEGER NOT NULL);
CREATE INDEX packs_sparse ON packs(live_bytes) WHERE state = 2 AND live_bytes * 2 < size;
```

- A row with `pack_id` NULL is a single object under `chunkKey(space, hash, incarnation)`, exactly as today. A chunk's length is its existing `size` column, and a pack's key and creation time derive from `pack_id`, so neither is stored twice.
- Pack states are `writing` (1, a compaction output before its swap), `live` (2), `drained` (3, emptied by compaction and kept until `hold_until_ms`) and `deleting` (4).
- `live_count` and `live_bytes` count the rows in state `pending` or `live` that point at the pack. Every transaction that adds, moves or deletes such a row updates them.
- The usage note's object and byte counts (`server/src/space/space.ts:748`) count single-object rows plus `packs` rows, with each pack's full size.

## 5. Reading packs

1. `GetDownloadTargets` (`server/src/space/space.ts:1174-1192`) issues, for a packed row, `{origin}/v1/spaces/{space_id}/chunks/{hash}?pack={pack_id}&off={offset}&ticket={ticket}`. The `get` ticket binds the space, the hash, the pack id, the offset and the size. A single-object row keeps its `inc` target.
2. The Worker's chunk `GET` (`server/src/worker.ts:157-166`) handles a `pack` target with a ranged read of the pack key, `range: { offset, length: size }` ([R2 Workers API, ranged reads](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#ranged-reads)). It answers `Content-Length` from the ticket's size. Today it uses `stored.size` (`server/src/worker.ts:164`), which is the whole object's size ([R2Object](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#r2object-definition)). R2 may return fewer bytes than asked, so a short body answers 503.
3. The client needs no change. It uses the target URL as given and checks every body against the chunk's hash and size (`core/net/src/chunks.rs:717-771`).
4. Share-link downloads read chunk objects directly: `shareFile` resolves each chunk to a key (`server/src/share/space.ts:137-151`) and `openChunk` checks the stored SHA-256 and size (`server/src/share/routes.ts:78-93`). A packed chunk resolves to its pack key and range. `openChunk` reads the range, at most 1 MiB, into memory and checks its SHA-256 before it streams it, since a pack has no per-chunk stored checksum.

## 6. Pack lifecycle and GC (amends RFC 0003 §5)

A pack is deleted only when no `pending` or `live` row points at it. GC (`collectGarbage`, `server/src/space/space.ts:1281-1420`) changes as follows.

1. **Packed rows.** The mark step (`server/src/space/space.ts:1347-1359`) selects rows by the same rule. A single-object row becomes `deleting` as today. A packed row is deleted in that transaction, and its pack's counts drop. Deleting a packed row issues no R2 delete, so it needs no `deleting` state and no retirement.
2. **Freeing.** A `live` or `drained` pack becomes `deleting` when `live_count` is 0, `hold_until_ms` has passed, and `uuidv7Ms(pack_id)` is more than 7 days ago. A `writing` pack that no running compaction owns becomes `deleting` too, at any age (§7 step 2). That is safe for §3 rule 3 because only the Worker's bundle packs appear in receipts.
3. **Retirement.** The delete loop (`server/src/space/space.ts:1386-1411`) adds `deleting` packs to each batch. `retireChunks` (`server/src/account/account.ts:562-576`) takes the batch's pack ids beside its incarnations and records them in a new Account DO table `pack_retirements (space_id, pack_id, retired_at_ms)`, kept and pruned as `chunk_retirements` is (RFC 0003 §5 rule 10). Only after the Account DO confirms does the DO delete the pack keys, and then it deletes their `packs` rows. A crash at any point leaves the pack `deleting`, and the next pass repeats retirement and delete, both idempotent.
4. **Orphan sweep.** The sweep (`server/src/space/space.ts:1361-1384`) also lists `spaces/<space_id>/packs/`, with its own cursor. A pack key with no `packs` row, uploaded more than `chunk_grace_days` ago, with no rewind completed in that time, gets a `deleting` row and goes through rule 3. This removes packs whose receipts no commit ever presented, and outputs abandoned by a crashed compaction.
5. **Rewind completion.** Completion (`server/src/space/space.ts:1461-1499`) also reads the space's retired pack ids (as `server/src/account/account.ts:896-899` reads incarnations). In its transaction, every restored pack whose id is retired, or whose state is `writing`, becomes `deleting`. Every row that points at a `deleting` pack, or at a pack with no row, loses its location and incarnation and becomes `pending` with its refcount kept, as RFC 0003 §5 rule 9 does for retired incarnations. Every other packed row keeps its location through rule 2 of RFC 0003 §5, so it counts as confirmed and is not uploaded again.
6. **Retention.** Compaction sets a drained pack's `hold_until_ms` to the swap time plus `chunk_grace_days` (§7 step 5). GC deletes only rows whose refcount is 0, and a `live` row only `chunk_grace_days` after its refcount reached 0 (rule 1, `server/src/space/space.ts:1350-1351`). So no pack is deleted earlier than `chunk_grace_days` after the last moment a row with refcount above 0 pointed at it, which is the retention invariant of RFC 0003 §5 for packs. A restore target T is at least now minus 29 days, so every pack a restored row names still exists, and rule 5 can only strip rows whose refcount is 0. Like RFC 0003 §5 rule 9, rule 5 does not rely on that.

## 7. Compaction

A pack whose live bytes fall under half its size is rewritten. GC runs compaction after rule 3 of §6, on at most 16 source packs per pass.

1. **Select.** Read up to 16 `live` packs from `packs_sparse`, older than 7 days, fewest live bytes first, with their `pending` and `live` rows. The plan fills outputs of at most 4 MiB of body each. Steps 2 to 5 run once per output.
2. **Record.** One transaction inserts the output's `packs` row in state `writing`, with a pack id the DO mints, owned by this pass in memory. A crash from here to step 5 leaves a `writing` row with no running owner, which the next pass frees (§6 rule 2). No row points at it, so freeing it loses nothing.
3. **Read.** Read each source whole with one `get` and check each planned chunk's SHA-256. A source that is missing or has a mismatch leaves the plan and is logged. An empty plan frees the output.
4. **Write.** Write the output, chunks and index as in §1, with `onlyIf: { etagDoesNotMatch: "*" }`. A failure, or an unknown outcome, frees the output. A write that lands after that delete leaves a key with no row, which the orphan sweep removes (§6 rule 4).
5. **Swap.** One transaction checks that the output is still `writing` and that every `pending` or `live` row on each source is in the plan. If either check fails, the output is freed. Otherwise every planned row that still exists moves to the output, the output becomes `live` with its counts, and each source becomes `drained` with counts 0 and `hold_until_ms` set to now plus `chunk_grace_days`. Rows GC deleted since step 1 are not moved, and their bytes are dead in the output.
6. **Free.** §6 rules 2 and 3 delete the sources once their hold passes.

Steps 2 to 5 interleave with commits, uploads and GC on the same object, because a Durable Object handles other events while it awaits R2. A commit changes refcount and state, not location, so a row it touches moves correctly at step 5. A receipt cannot add a row to a source, because §3 rule 1 accepts no pack older than 6 days. It cannot add one to the output either, which stays `writing` until step 5. A download target issued before step 5 names the source, which exists for `chunk_grace_days`, far longer than the 1-hour ticket (`server/src/space/limits.ts:9`). A rewind restarts the object (`server/src/space/space.ts:1448-1451`), and completion frees restored `writing` packs (§6 rule 5).

**Threshold.** A pack is compacted when `live_bytes * 2 < size`. At that point it holds more dead bytes than live ones, so each compaction writes fewer bytes than it reclaims. Every pack older than 7 days that is not waiting for a pass holds at least half live bytes, so pack storage stays under 2 times the live bytes of those packs. On top of that come packs younger than 7 days and the sources of the last `chunk_grace_days` of compactions. A threshold of 1/4 would allow 4 times the live bytes, and 3/4 would rewrite 3 bytes for each byte reclaimed. Storage costs $0.015 per GB-month and a write $4.50 per million ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)), so 10 GB of dead bytes costs $0.15 a month, and the rewrites cost less than that.

**Cost per GC pass.** At most 16 Class B `get`s, at most 8 Class A writes (16 sources with under 2 MiB live each fill at most 8 outputs of 4 MiB), one `retireChunks` call, and deletes, which R2 does not charge for (same page). That is at most about $0.00004 per pass. Memory peaks near one source plus one output, about 8 MiB. The pass sets `more` (`server/src/space/space.ts:1418`) when it used all 16 sources.

## 8. Export and restore (amends RFC 0004 §5 and RFC 0003 §7)

The exporter writes only files under `meta/<history_id>/` (`server/src/space/export.ts:171`) and never reads chunk rows, so it does not change. The descriptor (`server/src/space/export.ts:374-392`) does not change either: a history writes it once (RFC 0004 §6), so the descriptors of existing histories can never name packs, and a reader cannot depend on a field there.

An R2-only restore finds a chunk under `chunks/<hex2>/<hex>/`, as RFC 0004 §2 says. For a hash it does not find there, it reads packs. It lists `spaces/<space_id>/packs/`, reads each pack's index with one suffix read ([ranged reads](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/#ranged-reads)), and takes any pack that lists the hash. It checks the chunk's bytes against its hash, as for any incarnation, so a drained or orphan pack is as good a source as a live one. A reader refuses a pack whose footer magic is not `KDPACK01`. At 32 chunks per pack, the 1.52M small files of run 44391491657 make about 47,500 packs, so a restore makes about 48 list calls and 47,500 Class B reads.

## 9. Compatibility

- **Existing single objects need no migration.** Every path above keeps today's behaviour for a row with `pack_id` NULL, and that format stays permanent for chunks of 256 KiB and more, which the client never bundles (`core/net/src/chunks.rs:105`, 948). A backfill into packs would rewrite R2 data that already works, for a saving only in Class B reads. The migration in §4 adds nullable columns and new tables only.
- **Downgrade.** The migration raises `storage.space`, so a client refuses to install an older server over this one (RFC 0025 §3 rule 1). A Space DO refuses to start on a newer schema (RFC 0025, introduction).
- **Deploy window.** §2 rule 1 covers the 300 seconds after a deploy. A Worker that has not updated yet ignores `pack=1` and writes single objects.
- **Turning it off.** `KOIDRIVE_FEATURES=-chunk_packs` (RFC 0022 §3 rule 1) stops new packs. A Space DO accepts v2 receipts and reads and collects existing packs whether or not the feature is on.
- **Clients.** Tokens and download URLs are opaque to clients, so every client release works with packs.

## 10. Tests

**Pack GC property.** A state-machine property test drives the Space DO and Account DO code against a model R2 in which every write and delete may apply at once, never, or at any later step, in any order ([R2 consistency](https://developers.cloudflare.com/r2/reference/consistency/)).

State:
- chunk rows: state, refcount, incarnation, `head_checked`, location;
- `packs` rows: state, size, counts, hold, in-memory owner;
- R2 objects, and the in-flight writes and deletes;
- retired incarnations and pack ids in the Account DO;
- receipts and download targets the clients hold;
- the Space DO's earlier states that a rewind may restore, and the clock.

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
15. A rewind to a state at most 29 days old, with completion.
16. A download of a held target.
17. The clock moving forward.

After every step:
1. **No live chunk's pack is freed.** Every `pending` or `live` row with a location names a pack that exists in R2, is not retired, and holds the row's bytes at the row's offset. A confirmed single-object row has the same property for its chunk key.
2. **Retention.** No rewind leaves a row with refcount above 0 without a readable location.
3. **Counts.** Each pack's `live_count` and `live_bytes` equal its `pending` and `live` rows. No such row points at a `writing`, `drained` or `deleting` pack.
4. **Delete isolation.** Every delete issued names a retired key.
5. **Downloads.** A target returns the chunk's bytes until it expires.

A second property checks progress. After a quiet period with enough clock and GC passes, every pack without rows is deleted, and every `live` pack older than 7 days holds at least half live bytes.

**Server tests.** These tests cover the rest:
- A bundle of flagged chunks makes one write, and its receipts are accepted by a commit.
- An unflagged bundle keeps the RFC 0027 §6 behaviour.
- A failed pack write fails every packed chunk with the `PUT` status.
- A wrong-hash chunk is left out of its pack.
- A confirmed `pending` row gets `PRESENT`.
- An expired v2 receipt is accepted after one HEAD per pack.
- A ranged download and a share-link download each return exactly the chunk.
- A restore reader finds packed chunks from the index.

## 11. Delivery

Each item is one PR, in this order (CREW.md "Engine PR structure"). Items 2 and 3 are dormant in production because nothing writes a pack until item 4.

1. **GC property harness.** Test only. It covers the single-object steps of §10 first, and items 2 to 4 add the pack steps. It can merge at any time.
2. **Pack format and server read path.** It brings the §4 migration, the pack codec, v2 receipts and §3 at commit, §5 for downloads and share links, and the usage counts. GC leaves rows with a location alone. Tests create packs as fixtures.
3. **GC and compaction.** It brings §6 (including the Account DO table and the rewind rules) and §7. It needs item 2.
4. **Pack writing.** It brings the `chunk_packs` feature, `pack=1` targets and the Worker write (§2), the RFC 0027 §6 test changes, and the CPU measurement on Workers Free. It needs items 2 and 3 in the same or an earlier release. Reverting it stops new packs, and items 2 and 3 keep existing packs readable and collected.
5. **Client.** Larger bundles are possible once each bundle costs one write. This item can follow item 4 at any time, and needs its own RFC 0027 amendment and the CPU numbers from item 4. Item 4 cuts writes 32 times without it.

## 12. Alternatives considered

1. **Pack keys under the chunk key and incarnation scheme.** Incarnations belong to chunk rows: the Space DO allocates them in `PrepareUpload` (`server/src/space/space.ts:1112-1116`), and the bundle route never calls the DO. A `pending` row gives the same incarnation to every uploader (`server/src/space/space.ts:1151`), so two bundles could name one key with different bytes. A restore that lists `chunks/<hex2>/<hex>/` would also read a pack as a chunk.
2. **An index file written by the exporter.** It would order index writes against compaction and add an R2 write per change. The in-pack index costs 36 bytes per chunk and no write.
3. **The Worker telling the Space DO about each pack at upload time.** Dedup would know a chunk at once, and receipts would not carry locations. It adds a Space DO round trip to every bundle, which is the hot path this RFC shortens.
4. **No compaction.** A pack's waste is then bounded only by its chunk count, up to 31/32 of its size, so storage could reach 32 times the live bytes.
