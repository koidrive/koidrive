# RFC 0002: Commit outcomes, history fence and recovery

Status: proposed. Applies to spec v0 (format_version 1).

These are the protocol changes that the [sync-engine design §10](../../docs/design/sync-engine/protocol-rfcs.md#10-protocol-rules-this-design-depends-on) needs. Section and rule numbers refer to spec v0 unless they name the design. Sections 6 and 7 support recovery (design §6.4).

The schema changes add fields and enum values only:

| Message | Field or value | Section |
|---|---|---|
| `Commit` | `history_id = 5`, `device_seq = 6`, `require_head = 7` | 1, 2, 4 |
| `Error.Code` | `HISTORY_MISMATCH = 11`, `STALE_DEVICE_STATE = 12`, `HEAD_MOVED = 13`, `AHEAD_OF_SERVER = 14` | 1, 2, 3, 4 |
| `Welcome` | `device_seq = 10`, `device_commit_id = 13`, `device_committed_at_ms = 14` | 2 |
| `ResyncRequired` | `restore_seq = 2`, `restore_point_ms = 3` | 8 |

RFC 0001 covers part of what the design needs from the protocol: the Unicode 18.0.0 pin for NFC and case folding (RFC 0001 §1), the reserved `.koidrive` name (§2), the reject reason `NOT_FILE` (§3), `CreateDir` onto a live file (§4) and conflict names longer than 255 bytes (§5). This RFC does not repeat them. The other protocol rules the design depends on are in RFCs 0012, 0013, 0015, 0016 and 0018 ([sync-engine design §10](../../docs/design/sync-engine/protocol-rfcs.md#10-protocol-rules-this-design-depends-on)).

## 1. History fence (§8)

A `Commit` carries the `history_id` it was built in, which the client took from the `Welcome` of the history its trees belong to. The Space DO refuses a commit whose `history_id` is not its current one, an empty value included, with `HISTORY_MISMATCH`. This is the first check (§3), so nothing is applied and no stored result is returned.

A point-in-time restore removes the commit's row from `commits`. Without the fence, a commit retried into the new history would not be found by its `commit_id` and would be applied a second time. Comparing `Welcome.history_id` on the client is not enough on its own: the restore takes effect when the object restarts, and a hibernated WebSocket may outlive that restart.

## 2. Device counter (§2, §4.1, §7.2, §8)

`device_seq` numbers one device's accepted commits in the current history, starting at 1.

**Server.** For each device, the Space DO keeps the highest `device_seq` it accepted and that commit's `CommitResult`. It replaces both in the transaction that accepts the device's next commit. Only an accepted commit advances the counter. A commit refused with a call-level error (§6.3 and this RFC) does not. A commit that is applied with some ops `REJECTED` does. The reference schema (§4.1) gains:

```sql
CREATE TABLE device_commits (device_id TEXT PRIMARY KEY, device_seq INTEGER NOT NULL,
  commit_id TEXT NOT NULL, results BLOB NOT NULL);
-- The highest accepted device_seq per device and that commit's CommitResult
```

The server keeps this row when log truncation removes the commit from `commits`. It may drop the results of a device's older commits with the log. The device acknowledges a commit by sending its next one. `device_commits` lives in the Space DO, so a point-in-time restore rewinds it along with the rest of the history.

`Welcome.device_seq` reports the connecting device's highest accepted `device_seq` in the current history, or 0 if it has none. `Welcome.device_commit_id` and `Welcome.device_committed_at_ms` report that commit's token (RFC 0013): the `commit_id` the row keeps and the `committed_at_ms` of its stored `CommitResult`. Both are empty when `device_seq` is 0 or the stored result has no `committed_at_ms`.

**Client.** The client stores an accepted counter *a*: the `device_seq` of the last commit whose result it applied. A new commit takes slot *a* + 1, and the client persists that slot with the commit before sending it. A retry keeps the slot and the `commit_id`. A commit refused with a call-level error leaves the slot free, so the re-planned commit gets a new `commit_id` and the same slot. Applying a result sets *a* to the commit's slot. When the client connects, it compares `Welcome.device_seq` (*w*) with *a*:

| *w* | Meaning | Client action |
|---|---|---|
| *w* = *a* | The commit in flight, if there is one, was not accepted | Resend it unchanged |
| *w* = *a* + 1, and the commit in flight has slot *a* + 1 | The commit was accepted and its response was lost | Resend it unchanged; the server returns its stored result |
| *w* = *a* + 1 with no commit in flight, or *w* > *a* + 1, and the client keeps no result of slot *w* | The store is behind the server: it was restored from a backup or cloned | Plan nothing from this store and enter recovery (§6) |
| *w* > *a* at a slot whose result the client keeps from before an earlier loss it has not listed the tree since | The server's counter moved past *a* before the client listed the tree after that loss, as when the server is restored a second time | Act as for *w* < *a* |
| *w* < *a* | The server lost commits of this device that it had accepted: its counter only grows, and the store learns a slot only from its result | Take *a* := *w*, drop the commit in flight, and list the tree again as for `AHEAD_OF_SERVER`. The `Welcome`'s `device_commit_id` decides through which seq the server is taken to hold what the client saw (below). What was lost is planned again from the difference between the new listing and the client's synced state, so a lost edit comes back as a conflict copy and a lost delete comes back as a download |

The client compares *w* with *a* before it applies any result it got on the same connection. A result applied first raises *a* past a current *w*, so a healthy server looks like one that lost commits. When *w* < *a*, the client also discards any tree listing or commit result from an earlier connection that arrives after the `Welcome`, because it may describe commits the server lost.

A restored server gives seqs and slots out again, so a slot number does not name one commit across restores. After a loss, the client compares the new listing against a bound: the seq through which it takes the server's current history to hold what the client saw. A wrong bound breaks a guarantee in either direction. A bound too high takes changes the server lost for held ones, so they are not sent again, and another device's write that only this client holds can be replaced by older content. A bound too low takes held changes for lost ones and sends them again, so an edit brings back a file another user deleted after seeing it, and a kept write lands in a recovery copy. So no bound is safe by default. The client keeps the commit token of each slot it keeps, from the commit's result or its own record, and the `Welcome`'s token decides the bound. The bound is a server seq, so the `Welcome` has to name the same application of the commit, the one the client saw at the seq it kept: the same `commit_id`, and the same `committed_at_ms` when both are known. A server that applied this device's commit again after a loss holds it at another seq and gives it another `committed_at_ms` (RFC 0013 §1). A `committed_at_ms` of 0, from a server before RFC 0013, leaves the application unknown, so the `commit_id` alone decides, and a result without one keeps the time the commit's record gave the slot:

- When it names the application the client keeps for slot *w*, the current history holds that commit at the seq the client kept (RFC 0013), and that seq is the bound.
- When it names another commit, or another application of the commit the client keeps for slot *w*, the server gave slot *w* out again or applied the commit again at another seq, and the bound is the seq of the newest slot below *w* that the client keeps, or 0 when it keeps none. A store copied after a loss and before the listing, which comes back after the server gave a lost slot out again, shares that slot with the server.
- When either `commit_id` is empty, as from a server without these fields or for a slot the client kept before it stored tokens, the bound is the seq of the newest slot at or below *a* that the client keeps, as before tokens.

A result the client applies on the connection makes its own slot the bound. The bound applies whichever way the loss showed: a `Welcome` with *w* < *a*, or `AHEAD_OF_SERVER` after a `Welcome` with *w* = *a*.

Two cases stay open, and RFC 0029 (GUN-1012) removes this bound:

- A server that also gave out again the kept slots below *w*, as after a restore to before them followed by a restore to a backup that holds the originals, shares none of them, but the `Welcome` names only slot *w*, so the bound is too high there.
- A store copied before the listing can come back after the live store sent its edit again and another device deleted the file. The `Welcome` then names the resend at slot *w*, which the copy never saw, so the bound falls to the slot below and the copy takes the held edit for a lost one. Recovery must not send an edit the server holds again over a later delete.

## 3. Check order (§8 step 4)

The Space DO checks a `Commit` in this order. The first check that applies decides, and every refusal leaves the state unchanged:

1. **History fence.** If `history_id` is not the current history, refuse with `HISTORY_MISMATCH`.
2. **Stored result.** If a result is stored for the `commit_id`, in `commits` or in `device_commits`, return it.
3. **Counter.** Let *h* be the device's highest accepted `device_seq`, or 0. If `device_seq` is 0, refuse with `INVALID_ARGUMENT`. If it is *h* + 1, the commit is new and checking continues. If it is at or below *h*, refuse with `STALE_DEVICE_STATE`. The commit comes from a store older than the device's history on the server, and its outcome cannot be known. If it is above *h* + 1, refuse with `INVALID_ARGUMENT`.
4. **Base.** If `base_seq` is above `head_seq`, refuse with `AHEAD_OF_SERVER`. Step 1 has already established that the history is the same, so this means the server is behind what the device saw, not that the commit is malformed. It is therefore not `INVALID_ARGUMENT`, and the client resyncs (§6). If `require_head` is set and `base_seq` is below `head_seq`, refuse with `HEAD_MOVED` (§4).
5. **The remaining checks** of §6.3 and §8 step 4, in their current order, then the rules of §6.

Step 2 comes before steps 3 and 4. So a retry of a commit that was already accepted gets its result back, even if the device has committed since or the head has moved. A `base_seq` below `log_floor_seq` is not refused. It only decides which changes count as unseen (§6), and an old base is merely conservative.

## 4. Commit precondition (§8)

`require_head` asks the server to apply the commit only if the space has not changed since `base_seq`. If `head_seq` is above `base_seq`, the commit is refused with `HEAD_MOVED`, as a whole and with nothing applied. Recovery answers use it (§6). The client simulates an answer's ops against the server tree at `base_seq` with the §6 rules, and the precondition guarantees that the server applies them to that same tree. Any other commit in the space makes the precondition fail, and the client then has to ask again. That is acceptable for a rare recovery answer. A commit refused with `HEAD_MOVED` does not use up its slot (§2).

## 5. Ordered application and acknowledgement (§5.2, §7.3, §7.5)

**§5.2.** Clients apply change records as whole commits in contiguous seq order. The rev rule only makes an overlapping replay idempotent. It does not make out-of-order application safe, because `NodePurged` carries no revision: a purge applied before an earlier `NodeState` of the same node would be undone by that state. The v0 sentence that overlapping a snapshot with the log gives the same result holds for in-order replay only.

**§7.3 step 2.** A log replay after `Resume` includes the device's own commits. Only the live push leaves out the connection that committed. A client acknowledges its commit when it applies the `CommitResult`. It does this even when the commit's records already arrived by replay, and it applies the result's re-keying (`MERGED_INTO`, `CONFLICT_COPY`) once. A result that arrives while earlier seqs are missing is held until they are applied.

**§7.5.** Pages of a bootstrap can come from different heads. The client uses the bootstrap tree only after the log replay has reached the highest `head_seq` among its pages.

## 6. Recovery after a history change or a stale store (§7.3 step 4)

This section and §7 are a draft. Recovery has to pass the simulation scenarios of design §7.5 before 0.1.0, so these two sections may still change.

Step 4 of §7.3 keeps its v0 meaning for `LOG_TRUNCATED` and `AHEAD_OF_SERVER`. That covers both the `ResyncRequired` reason and a commit refused with the `AHEAD_OF_SERVER` error code: the client bootstraps, then plans its local changes again against the new tree. The refused commit was not applied, so it is dropped and its slot stays free (§2). If the server lost this device's own commits too, `Welcome.device_seq` shows it on reconnect, and the last case below applies.

A client must not use step 4 in these cases:

- `ResyncRequired` with `HISTORY_CHANGED`;
- a `Welcome` whose `history_id` differs from the stored one;
- a commit refused with `HISTORY_MISMATCH` or `STALE_DEVICE_STATE`;
- a `Welcome.device_seq` that the store cannot account for (§2).

In each of these, the client does not commit its local-only changes again on its own, and it never resends or re-plans the commit in flight. It recovers as follows:

1. It keeps its trees from before the reset, and its commit in flight, as evidence.
2. It reads the current server state with both `ListTree` and `ListTrash`. A node in the trash counts as present, so the client never takes a trashed `node_id` for absent and publishes it again, which the server would refuse with `ID_EXISTS`. `TrashPage` carries no `head_seq`, so the client works as in a bootstrap (§7.5):
   - it reads every `ListTree` page first, then every `ListTrash` page;
   - it sends `Resume` with `since_seq` set to the first tree page's `head_seq`;
   - it uses the result only after `CaughtUp`.

   The server sends that `CaughtUp` after both listings, so the replay covers every change made between and during them.
3. It reconciles the two by `node_id`. A node is applied without asking only when the outcome is certain: the local version equals the current server version, or the device had not changed the node since its last sync and the client follows the server.
4. Everything else is held until the user answers, with nothing published and nothing changed on disk. That covers nodes created after the restore point, nodes changed on this device, nodes created on this device, nodes deleted on this device, and operations requested before the reset.
5. Before it sends an answer, it simulates the answer's ops with the §6 rules against a copy of the current server state. An op can change nodes outside the answered set: R8 revives a whole trash group, R2 merges directories, and a move carries its subtree. Every node the simulation changes outside that set must end in its current server state, or in the state that the evidence shows this device's user chose. The client may add compensating ops to the same commit, but only when a new simulation confirms the result. Otherwise those nodes join the question, which is asked again before anything is sent. The server picks conflict names from the commit's date and the device name, so the commit may not end exactly as simulated. The client compares the commit's op results and change records with its simulation. Every node whose outcome differs is classified again, and it is asked about again unless a certain row now applies.
6. It commits each answer with `require_head` (§4). Every `Delete` an answer produces goes through the client's mass-delete guard.

Design §6.4 gives the full classification. The server's part is §1 to §4 and §7 to §8.

## 7. Reusing a `node_id` (§2, R11)

A `CreateDir` or `CreateFile` may use a `node_id` that no node of the current history holds, live or trashed. That includes an ID that a node held in an earlier history, or that belonged to a purged node. Recovery uses this to publish again, under its own ID, a node created after the restore point. `ID_EXISTS` (R11) refers to the nodes of the current history only. The server needs no other record of IDs used before.

## 8. Restore point in `ResyncRequired` (§7.2, §7.4)

When the current history began with a rewind, `ResyncRequired` with `HISTORY_CHANGED` carries that rewind's restore point:

- `restore_seq`: the `head_seq` right after the rewind;
- `restore_point_ms`: the point in time it restored, in Unix milliseconds.

Both are 0 when the history began with the creation of the space. The Account DO, which directs the restore (§7.4), writes them to the Space DO's `meta` as `restore_seq` and `restore_point_ms`, in the same step that records the new `history_id`. After several rewinds they describe the latest one. A client that sees a new `history_id` in `Welcome` can still send `Resume` with its stored `history_id` to get this `ResyncRequired`. The fields only name the restore point in the recovery notice. Correctness does not depend on them.

## 9. Delete kept by a descendant (R7)

A `Delete` whose target stays live because a descendant has an unseen change also returns `SUPERSEDED`, as when the target's own change keeps it. `SUPERSEDED` means that the target is still live after the op.

A `Delete` trashes a node only if the committing device has seen where the node is now, meaning that it is live under its current parent. R7's conditions on the node's own content and metadata still apply. A placement is the commit that last put the node under its current parent or brought it back from the trash. That commit's device counts only when its op named both the node and the parent, as `CreateDir`, `CreateFile`, `Move` and `Restore` do. A `Restore` without a target parent names the parent the node had in the trash. The committing device has seen a placement when the placement's seq is at most `base_seq` or the placement's device is the committing device. A node whose placement it has not seen stays live with its whole subtree, and its ancestors stay live too. So a node that another device created, moved or restored under the target after `base_seq` stays. So do an R3 conflict copy, a node that R8 or a restored trash group brought back, and the folder that an R2 merge reused, because no op of the committing device named that node with its parent. A `Move` that names the node's current parent, a no-op `Move` included, makes the placement the committing device's. A node that another device moved between two folders inside the target also stays. A rename that keeps the parent is not a placement. It is an ordinary unseen change, so R7 keeps the renamed node and its ancestors and trashes its unchanged descendants.

## 10. Renamed revival on UpdateContent (R8)

An `UpdateContent` whose target R8 revives under a conflict name, because the revived name clashes under R1, returns `APPLIED_RENAMED` with the name used, as `Move`, `Restore`, `CreateDir` and `CreateFile` do. `APPLIED_RENAMED` replaces only a result that would otherwise be `APPLIED`. If the update also has an unseen content change, so that R3 applies, the result stays `CONFLICT_COPY`. A client acknowledges an `APPLIED_RENAMED` update as it would an `APPLIED` one: the content it sent becomes synced. The new name and position reach the disk through the node's `NodeState` in the result's changes, like any remote rename.

## 11. Missing-chunk list (§8 step 4)

A `MISSING_CHUNKS` error lists at most 1,000 hashes, the most one `PrepareUpload` takes, so the error always fits in one message. Its `message` gives the total number of missing chunks. To get the rest, the client runs `PrepareUpload` for the listed chunks and uploads them. It then resends the commit with the same `commit_id`, `device_seq` and ops, and adds the receipts of the new uploads to `receipts`. Each retry lists up to 1,000 more, until none are missing and the commit is applied. If the bytes of a listed chunk are gone, because the file changed after it was chunked, the client abandons the commit and plans it again under a new `commit_id` in the same `device_seq` slot. A refused commit does not use up its slot (§2).

A receipt expires after an hour, so the receipts of earlier batches can expire while later ones upload. A `pending` chunk whose receipt is missing or expired therefore gets the R2 `head` check that spec §8 step 4 already applies to a direct-mode chunk without a receipt: before the transaction, the server checks that the object exists under the row's key (RFC 0003 §5) with the expected size. If it does, the chunk counts as uploaded, and the server extends the row's hold as `PrepareUpload` does. One commit call checks at most a fixed number of such chunks, so it stays well inside a Worker invocation's subrequest limit, and lists the others as missing. The server records each confirmation on the row, so the next retry checks the next ones. In the transaction, the server checks again that the row is still `pending` and holds the incarnation whose key it checked. If not, it lists the chunk as missing. So a long series of retries does not list the same chunks again, and a commit never refers to an object that GC deleted after the check.
