# RFC 0003: Space rewind

Status: proposed. Applies to spec v0 (format_version 1).

The owner of a space can rewind it to an earlier point in time. The rewind restores the Space DO's SQLite database with point-in-time recovery (PITR) and starts a new history (§7.4). Devices then recover as RFC 0002 §6 describes. Section numbers refer to spec v0.

The schema changes add messages and fields only:

| Message | Fields | Section |
|---|---|---|
| `RewindRequest` (new, `auth.proto`) | `restore_point_ms = 1`, `expected_history_id = 2`, `rewind_id = 3` | 1 |
| `RewindResult` (new, `auth.proto`) | `history_id = 1`, `restore_seq = 2`, `restore_point_ms = 3`, `previous_history_id = 4`, `previous_head_seq = 5` | 1 |
| `Welcome` | `restore_seq = 11`, `restore_point_ms = 12` | 2 |

The chunk key incarnations of §5 are internal to the server and change no message.

PITR behaves as [the SQLite storage API documentation](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#pitr-point-in-time-recovery-api) describes (accessed 2026-11-10):
- it restores to any point in the past 30 days;
- `getBookmarkForTime` returns a bookmark for approximately the given time;
- bookmarks compare lexically in time order;
- `onNextSessionRestoreBookmark` restores the storage when the object next starts.

## 1. API (§15.1)

`POST /v1/spaces/{space_id}/rewind` takes a `RewindRequest` and returns a `RewindResult`. The body is protobuf, like the other endpoints of §15.1. The client call is `Api::rewind` in `core/net`. `koi` has no command for it.

`rewind_id` is a UUIDv7 that the caller chooses once per intended rewind and reuses on every retry. The Account DO looks it up before checking anything else:

- **Same `rewind_id`, same fields.** The request returns that rewind's state:
  - the stored `RewindResult` once the rewind is complete, even though the space's history has since changed;
  - `RETRY_LATER` while it runs;
  - its stored error if it failed.
- **Same `rewind_id`, different `restore_point_ms` or `expected_history_id`.** `INVALID_ARGUMENT`.
- **New `rewind_id`.** The checks below apply. A new request built on the old `expected_history_id` fails the history check, so it cannot start a second rewind.

The Account DO keeps every rewind record for at least 30 days after it ends.

A new `rewind_id` is refused, with nothing recorded and nothing changed, in these cases:

| Condition | Error code | HTTP status |
|---|---|---|
| The device is revoked, or the caller is not the space's owner | `UNAUTHENTICATED` or `FORBIDDEN` | 401 or 403 |
| `restore_point_ms` is not admissible (§2) | `INVALID_ARGUMENT` | 400 |
| `expected_history_id` is not the space's current history | `HISTORY_MISMATCH` | 409 |
| Another rewind of the space has not ended | `RETRY_LATER` | 503 |
| The current history's descriptor is not yet durable (§7) | `RETRY_LATER` | 503 |

## 2. Admissible restore points

At admission, `restore_point_ms` (P) must meet all of these:
- it is at least one minute and at most 29 days before the request;
- it is not before the space was created;
- it is not inside the interval during which an earlier rewind of the space held the space closed (§3);
- PITR has a bookmark for it. PITR's history of a new Space DO begins some time after the object is created, and `getBookmarkForTime` refuses an earlier time with "Requested time is before this database existed." In a deploy test, a point 30 s after creation was refused. The documentation states no bound.

The Account DO checks the PITR condition through the Space DO at admission, and refuses with `INVALID_ARGUMENT` like the others, since no retry can make PITR serve that point.

The 29-day bound keeps P inside the 30-day PITR range, with a day for the rewind to finish. The window is checked once, at admission. A rewind already admitted is never refused later because time has passed.

P is the requested point. The restored state is the state just after the last commit at or before P, the commit before the first one whose `committed_at_ms` is later than P. `getBookmarkForTime` gives only a bookmark approximately at P, which can be several commits earlier, so the Space DO stores bookmarks itself:
- After every commit it appends, a GC purge included, it reads `getCurrentBookmark()` and stores it for that seq, usable for any P at or after the commit's `committed_at_ms`.
- Rewind completion deletes every stored bookmark, because one taken before the completion restores a state without the completion's changes (§3). It then stores a bookmark for `restore_seq`, usable for any P at or after the completion.
- A bookmark more than 29 days old is not used, and GC deletes it. PITR keeps 30 days, so a bookmark used at admission stays restorable for at least a day.

The rewind restores the stored bookmark of the last commit at or before P when it is usable for P. Otherwise it restores the bookmark `getBookmarkForTime` selects for P. That happens when:
- a server version that stored no bookmarks made the commit;
- reading the bookmark failed, or the object stopped before the read;
- another commit landed before the read finished, so the bookmark would include it;
- P is before an earlier rewind's completion, and the commit is from before that completion;
- the bookmark is more than 29 days old. P is then at most 29 days old, so no commit lies between that commit and P.

`RewindResult.restore_seq`, `Welcome.restore_seq` and `ResyncRequired.restore_seq` give the exact `head_seq` of the restored state, and `restore_point_ms` is always the requested P.

## 3. A rewind resumes from its recorded phase after a crash

A failed rewind leaves the space unchanged. After a crash or a call with an unknown outcome, the rewind resumes from its recorded phase, and it never arms a second restore before it has settled the first.

The Account DO directs the rewind and keeps a record of it in its own storage, which PITR on the Space DO does not touch. The record holds:
- the request;
- the selected bookmark B;
- the new `history_id`;
- the rewind number (§5, rule 6);
- the previous history and head;
- the attempt number;
- the phase;
- the result or the error.

An Account DO alarm drives the phases with backoff from one second up to ten minutes. Each phase is recorded before the call it starts, so a crash or an unknown call outcome resumes from the recorded phase.

1. **Admission.** The Account DO answers checks of the space as closed (RFC 0014 §3), then asks the Space DO, through a call that stores nothing, for:
   - its current `history_id` and `head_seq`;
   - whether the current history's descriptor is durable (§7);
   - the bookmark B for P.

   It then checks §1 and §2. Any failure refuses the request, records nothing, and reopens the space within one lease (RFC 0014 §3). Otherwise it records the rewind as `admitted`, with B, the new `history_id` and the rewind number. From then on the space is closed (§4).
2. **Preparing attempt k.** The Space DO:
   - closes every WebSocket after sending `Goodbye` with close code 4004 (space rewound);
   - writes a marker row for the rewind and attempt k, and reads its current bookmark M;
   - returns its `history_id` and `head_seq`, and whether B is earlier than M.

   If B is not earlier than M, the restored state could contain the marker, and the rewind fails (step 5). Otherwise the Account DO records the phase `prepared`, and, on the first attempt, the returned history and head as the previous history and head. All of this happens before any restore can be armed. Preparing is idempotent: repeating it for the same attempt leaves one marker.
3. **Arming attempt k.** The Account DO records the phase `arming`. Then the Space DO calls `onNextSessionRestoreBookmark(B)` and restarts with `ctx.abort()`.
4. **Settling.** After an arming call, whether it answered, failed or has an unknown outcome, the Account DO first restarts the Space DO once more. A restore that is pending applies when the object starts, so after this restart none can still be pending. The Account DO then asks the Space DO to complete the rewind:
   - **Marker of attempt k present.** No restore happened and none is pending. The Account DO goes back to step 2 with attempt k + 1, or ends in step 6 if B can no longer be restored.
   - **No marker, but a completion receipt for this rewind in `meta`.** Completion already ran, and the Space DO returns that receipt again.
   - **Neither.** The storage was restored to B, which predates every marker of this rewind. The Space DO reads the space's retired incarnations from the Account DO (§5, rule 9). It then completes the rewind in one transaction:
     - sets the new `history_id` and the rewind number, plus `restore_seq` (its `head_seq`) and `restore_point_ms` in `meta` (RFC 0002 §8);
     - adds every device that the Account DO has revoked to `revoked_devices`;
     - applies the chunk rules of §5;
     - deletes the stored commit bookmarks, and after the transaction stores one for `restore_seq` (§2);
     - resets the export state (§7);
     - stores the completion receipt.

   Every completion call, a replay included, then sets the Space DO's alarm from its `schedule` table. A repeated call therefore returns the same receipt and leaves the alarm set.

   An Account DO that restarts in the phase `arming` resumes at this step. So it never arms again before it has settled the earlier attempt.
5. **Reopening.** The Account DO stores the `RewindResult` and records the phase `completed`, which reopens the space.
6. **Failure.** A rewind ends as `failed` only when no restore can have happened or be pending. That is the case when B is not earlier than M in step 2, or after step 4 has proved that the marker is still present and B can no longer be restored. The Account DO then removes the markers, records the error, and reopens the space unchanged.

   Once step 4 has found the marker gone, the rewind can only complete. If completion keeps failing, the space stays closed: reopening a restored database under its old history would let devices treat it as that history. This can only happen through a server bug. A retried request with the same `rewind_id` reports `RETRY_LATER` and the last error.

## 4. A closed space serves no client and runs no work

While a rewind runs, no client and no alarm work reaches the space, so nothing reads the restored database before it carries its new history.

From admission until the record is `completed` or `failed`, the space is closed:
- every HTTP RPC and WebSocket upgrade gets `RETRY_LATER`;
- the Space DO answers every socket message by closing the socket with 4004;
- the Space DO's alarm runs no work and leaves the `schedule` table as it is;
- the Account DO records no chunk retirements (§5, rule 8).

A Space DO may go on answering the space as closed for up to one lease after the record is `completed` or `failed` (RFC 0014 §3).

A closed alarm sets the alarm again for at most ten minutes later. Work that comes due during the closure, such as GC, the orphan sweep or export, therefore runs within ten minutes of reopening, even with no client activity.

The Account DO decides whether the space is closed. The Worker asks it about every request, the Space DO asks it about every request and message or uses a lease that admission drops (RFC 0014 §3), and the alarm handler also asks it before it runs any work. So the restored database cannot run GC before completion has applied §5, and no client reaches the restored state before it carries its new history.

Operators must not restore a Space DO's storage outside the rewind flow. A Space DO whose `history_id` is not the history of the space's last completed rewind refuses every HTTP RPC and WebSocket upgrade with `HISTORY_MISMATCH`, closes every socket with 1013, and runs no alarm work, until its storage holds that history again.

## 5. Chunks outlive every state a rewind can restore

Two invariants protect chunk objects:
- **Retention.** No chunk object is deleted earlier than `chunk_grace_days` after the last moment at which any history referenced it.
- **Delete isolation.** An R2 delete never removes the object of a `pending` or `live` chunk row of the current history, however late R2 applies the delete. [R2 lets overlapping writes to one key finish in any order](https://developers.cloudflare.com/r2/reference/consistency/), and a delete whose outcome is unknown may still take effect later. So once a delete of a key may have been issued, the server never relies on that key again.

Rules 1 to 4 keep the retention invariant:

1. GC deletes a chunk no earlier than `chunk_grace_days` after its refcount reaches 0, as in §12.
2. At completion, every chunk row whose refcount is 0, in any state, becomes `pending` with `hold_until` set to the completion time plus `chunk_grace_days`. The discarded history may have referenced the chunk until the rewind, and GC may already have deleted the object. As `pending`, `PrepareUpload` answers `UPLOAD`, so a commit that needs the chunk stores it again. A conditional PUT that finds the object present under the row's incarnation counts as stored; rule 9 has already made sure that incarnation is not retired. If no commit needs the chunk, it ages out through the normal grace.
3. The orphan sweep deletes an object whose incarnation no chunk row holds only once both its upload time and the space's latest rewind completion are more than `chunk_grace_days` in the past. The Account DO keeps the latest completion time per space, and every completion copies it into `meta`, so a later rewind cannot lose it.
4. `chunk_grace_days` cannot be set below 30.

Any admissible target T satisfies T ≥ now − 29 days, and a chunk referenced at T was referenced until some time no earlier than T. By the retention invariant it is kept until at least T + 30 days, which is after now. The bound holds across repeated rewinds, including a target in an interval that an earlier rewind discarded. It does not depend on how `chunk_grace_days` changed over time, because rule 4 makes 30 days the floor. Rewinds therefore do not narrow the window. The cost is that the chunks only a discarded history used stay in R2 for 30 days after the rewind.

Rules 5 to 10 keep delete isolation with key incarnations:

5. A chunk object is stored under `spaces/<space_id>/chunks/<first 2 hex>/<hex>/<incarnation>`, which replaces the key of §10. A chunk row holds at most one incarnation: the key its object is stored under. When `PrepareUpload` answers `UPLOAD` for a row that holds none, the Space DO gives the row a fresh incarnation. Spec v0 has not been released, so the layout changes without a migration.
6. An incarnation is `<r>.<n>`. r is the rewind number of the current history: 0 for the space's first history, and otherwise the number that the Account DO gave the rewind at admission from a per-space count that only goes up. n comes from a counter in the Space DO's `meta` that only goes up. The Space DO allocates incarnations only while the space is open. Each rewind number belongs to one history, because completion runs once per rewind. Within one history the Space DO's storage only moves forward, because a restored database takes a new rewind number before the space reopens (§3 step 4). So no incarnation is allocated twice.
7. Clients still address chunks by hash (§10), so the wire format does not change. The server puts the row's incarnation into the key of every upload and download target it issues, relay tickets and presigned URLs alike. An upload counts only for the incarnation it wrote. If the row no longer holds that incarnation, the object is an orphan for rule 3.
8. **Retirement.** Before the Space DO deletes the object of incarnation i, through GC (§12 step 4) or the orphan sweep, it asks the Account DO to record i as retired. It issues the delete only after the Account DO has confirmed, and the delete targets that one key. GC makes one such call per batch of deletes. GC marks a row `deleting` before it retires the row's incarnation, and the orphan sweep retires only incarnations that no row holds. If the call's outcome is unknown, the Space DO issues no delete and asks again later. The Account DO keeps the records in its own storage, out of PITR's reach. It refuses new retirements while the space is closed (§4), so the set of retired incarnations does not change while a rewind runs.
9. At completion, before its transaction, the Space DO reads the space's retired incarnations from the Account DO. In the transaction, a restored row counts as absent if its incarnation is retired or the row is `deleting`. It loses its incarnation and becomes `pending` under rule 2, keeping its refcount, so a commit that needs the chunk uploads it again under a fresh incarnation that no earlier delete can reach. The retention invariant means that only rows with refcount 0 should be affected, but this rule does not rely on that.
10. The Account DO keeps each retirement record for at least 30 days, and never prunes a space's records while a rewind of that space is open. Every Space DO state written after a retirement holds the retired incarnation, if at all, on a `deleting` row, which rule 9 treats as absent anyway. A state written before the retirement can only be restored by a rewind armed within PITR's 30 days of it, and the record still exists until that rewind ends.

A delete, however late R2 applies it, removes only the object of a retired incarnation (rule 8). No `pending` or `live` row of the current history holds a retired incarnation: rows take only fresh incarnations (rules 5 and 6), GC retires a row's incarnation only after marking the row `deleting`, and completion strips retired incarnations from restored rows (rule 9), using a set that cannot change during the rewind. Delete isolation therefore holds whatever order R2 applies overlapping requests in, and it needs no assumption about requests sent by an object instance that has ended. The cost is that a chunk whose incarnation a rewind strips is uploaded and stored again, and the old object stays until the orphan sweep removes it.

## 6. A rewind restores the Space DO and keeps the Account DO

A rewind replaces the space's whole Space DO state with the state at the restore point and changes nothing in the Account DO. Devices keep only what is on their disks.

- Everything in the Space DO is restored to the restore point. That includes:
  - the tree, every node's versions and the trash;
  - the commit log, chunk rows, members, the space's name and settings;
  - `device_commits` (RFC 0002 §2).
- The Account DO's state is kept. That includes devices, revocations, spaces, share links, rewind records and chunk retirement records. A share link that points to a node absent after the rewind finds nothing.
- Every commit that the previous history held and the restored state does not is removed from the current history. `restore_seq` and `previous_head_seq` each count in their own history, and those differ when the restore point lies in an earlier history, so the two numbers do not bound these commits. They include:
  - every intermediate version written after the restore point, not only the latest one;
  - nodes trashed after the restore point, and their trash state;
  - changes that no device downloaded before the rewind.

  For a while, PITR and the earlier history's export files still hold their metadata, and §5 keeps their chunks for 30 days. Within the window, a later rewind to a point before this one can bring them back. Nothing guarantees that, and once the window and the grace have passed they are gone.
- **What devices keep.** Devices keep only what is on their disks, through recovery (RFC 0002 §6):
  - A local file the device changed is held for the user's answer.
  - A local file the device did not change follows the restored server: the newer local copy moves to the device's local trash, which keeps it only as long as that trash's own retention allows.
  - No device is guaranteed to have a copy of a version it never had on disk, such as an intermediate version B in A → B → C after the restore point. Such a version exists only where the previous bullet allows, until it expires.
- **Undo.** There is no undo operation, and this RFC does not guarantee one. A later rewind to a point before this one began restores the state at that point like any other admissible target (§2 and §5), as yet another history. It does not reverse what devices did during their recovery.

## 7. Each history exports to its own prefix (§11)

Each history keeps its own export files, and a new history starts from a full baseline:

- Log segments are written under `meta/<history_id>/log/`, and snapshots under `meta/<history_id>/snap/`. The files of earlier histories are never changed.
- Each history has its own descriptor, `meta/<history_id>/space.json`, in place of the single `meta/space.json` of §11. It holds the fields of §11 plus `history_id` and the history's rewind number (§5, rule 6), and its `chunk_key` names the key layout of §5, rule 5. The Space DO writes only the current history's descriptor. So a write, however late R2 applies it, can only change the descriptor of the history that issued it.
- An R2-only restore lists the descriptors under `meta/`, takes the one with the highest rewind number, and reads only that history's files. It finds a chunk by listing `chunks/<first 2 hex>/<hex>/` and using any incarnation there. Every incarnation of a hash holds the same bytes, which the hash verifies.
- Completion resets the export state for the new history:
  - the exported log starts after `restore_seq`;
  - a full snapshot is due at once;
  - no snapshot of the new history exists yet.
- The exporter writes the new history's first snapshot and the log segments from that snapshot's `start_seq + 1` through its `end_seq`. Only once all of them are durable does it write the new history's descriptor. Until then, the previous history's descriptor has the highest rewind number, and an R2-only restore yields the previous history's exported state. That state may lag the previous history's head, as spec §11 allows for any export. Admission refuses a further rewind with `RETRY_LATER` until then (§1), so the descriptor with the highest rewind number always belongs to the current history or the one just before it. A space's first history writes its descriptor when the space is created.
