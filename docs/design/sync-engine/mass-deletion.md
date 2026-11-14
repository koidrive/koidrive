# 6. Mass-deletion safety

## 6.1 A large delete waits for confirmation

The guard (`core/engine/src/guard.rs`) holds local deletions that would remove too much of the space and asks the person.

### What is counted

- A local deletion is planned as one `Delete` per topmost deleted node: a node of S that is absent from L while its parent is present in L (§2.6). Each topmost deleted node is one deletion, a file or a whole folder. Emptying a folder but keeping it makes each removed child a deletion.
- Separately, the guard counts the affected files of each deletion: the live files of S in its subtree, leaving out nodes the server will keep (§2.4) and nodes moved elsewhere first.
- A delete of a folder that holds excluded nodes is split (§5.10) and still counts as one deletion.

### Thresholds

The guard holds deletions when either limit is crossed:

- the deletions in the rolling 10-minute window, the new ones included, number more than 1,000;
- the affected files in the window, the new ones included, are more than 50% of the space, for a space of at least 20 files. The denominator is the live files of S plus the files already deleted in the window.

### Durable state

The engine persists, in the store transaction of the commit or hold that changes them:

- the ledger: for each topmost deleted node in the window, its `node_id`, the time it was planned, and its affected-file count. Keying by `node_id` makes a resent or re-planned commit, a restart, or a node planned twice count once. Each plan rewrites the affected-file count from the current S. An entry is removed when its node is present in L again, so a later deletion of the same node is new, and otherwise 10 minutes after its time. Time comes from the `now_ms` of inputs; an entry whose time is later than `now_ms` is treated as made at `now_ms`, which keeps it in the window longer, never shorter;
- the held set: the topmost deleted nodes being held, each with its affected-file count and the revisions in R (`content_rev`, `meta_rev`) of every node in its subtree when it was held;
- the question: an ID, the held set it asks about, the counts the person was shown, the thresholds crossed, also those of the questions it replaced, and its answer until the engine applies it;
- the confirmed set: the deletions answered with Delete, planned without counting until the commit that makes them is acknowledged. A deletion split over several commits stays confirmed while each of its pending nodes keeps the revisions it was confirmed with. When one changes, what is left is held under a new question, also below the thresholds.

### Holding and asking

- The guard counts every deletion planning could make when a commit is built, not only those that fit in that commit, so a removal larger than one commit is asked about before any of it is sent.
- When the deletions would cross a threshold, the engine holds all of them and emits `AskUser`. Everything else keeps syncing: other local changes, downloads and remote deletions.
- Held nodes are excluded from planning; they are absent locally and stay present in R. A held node that changes remotely leaves the held set and is downloaded again, as the keep rule would do.
- Deletions planned while a question is open join the held set, and a new question replaces the old one, so an answer always refers to the set the user saw. A held set that shrinks is replaced by a new question too, also when the rest no longer crosses a threshold. The question ends only when nothing is left. A question whose held set does not change stays open until answered, also after its window ends.

### Open questions

The guard's question is one of the engine's open questions, and every hold that waits for the person uses the same record. Each open question has an ID, the time it was asked, a kind (`QuestionKind`), and the items it holds: each topmost node with the nodes below it that the answer applies to and their revisions in R, or none for a node R does not hold. The store keeps every open question with its items. No commit carries an op for a node that an open question holds, and an answer is routed by the question's kind.

### Answers

An answer is bound to the question ID, and through it to the exact nodes and revisions listed. The engine applies an answer when it next plans, after it has counted the deletions pending then, so a deletion made before the answer reached the engine, also while the device was stopped, is never covered by it.

- Delete: when the pending deletions are still exactly the held set, with the same nodes and revisions, they are committed without counting against the thresholds again. Otherwise the answer is dropped and a new question asks about everything pending.
- Restore: S := ⊥ for every node of each held subtree that is still deleted, so the whole subtree is downloaded again rather than re-planned as deletions, and their ledger entries are cleared. Nodes are matched by ID, under whichever topmost node they are deleted now, and any other pending node stays held under a new question.

### Root health

Deletions are planned only from observations whose batch carries a passing root check (§6.2). A batch that reports removals without one, or with a failing one, is not trusted: its removals are ignored, the space pauses if the check failed, and a full rescan is requested. This applies to every observation that removes something, subtree rescans included.

### Remote deletions

Remote deletions are applied locally, since the server is authoritative, and go to the local trash. The guard counts them once applied, so its notice follows the removal; a restore (§6.3) brings back what the server still holds.

- A remote deletion is one `Delete` op of another device. It trashes the live part of its node's subtree, except the nodes the server keeps (R7), and gives every node it trashes the op's trash group; a node trashed earlier keeps its own group.
- Each removal of a node R no longer holds belongs to the deletion that trashed it, named by its trash group in R's record. The deletion's topmost node is the outermost folder above the removed node, still in L, that R holds trashed in the same group, or the node itself. So a removed folder is one deletion with all its files, and a folder deleted above a deleted one does not take over the inner deletion's files.
- When R holds no record of the node, as after a purge, the removal belongs to the deletion the guard counted or dismissed under a folder above it that R no longer holds. When there is none, it starts a deletion whose topmost node is the outermost of those folders, trashed at or before the seq up to which R holds every change. A record R learns later matches it when its seq is at or before that one and the removed node is under the topmost node.
- Without a record, deletions count by folder with the same files: those of one folder as one, and one the server applied in part as one for each node it trashed under a folder it kept.
- Every bootstrap after the first lists the trash, so R holds the group of each node the server still holds trashed.
- These counts go into a second ledger, persisted like the first, with the same thresholds. Its denominator is the live files of S plus the files it counts.
- A deletion stays in that ledger for the window after its last removal, and as long as L still holds a node R holds in its trash group, or its topmost node while that node is still the deletion's (R holds it trashed in the deletion's group, or in any group for a deletion without one, or holds no record of it). A restore below the topmost node brings that node back live on its own, and a later delete trashes it in another group, so neither keeps the deletion. A removal that pauses longer than the window and continues later counts whole.
- A deletion ends when R holds its topmost node and no node trashed in its group, because the server restored them or lost the commit that trashed them. Restoring some of its nodes does not end it. Without a group, it ends when R holds its topmost node live or trashed by another deletion. An ended deletion leaves the ledger at once, so a folder restored and deleted again is a new deletion. A purge does not end it.
- When the removals cross a threshold, the engine opens a notice: an open question of kind `RemoteMassDelete` that holds nothing and takes only Dismiss. While it is open, each later removal updates its counts and thresholds, and no entry leaves the ledger by age.
- Dismiss closes the notice at once. The deletions it counted leave the ledger and are not counted again, the rest of a folder still being removed included, until they end or this device has removed them. A restore that brings back all that is left of one ends its dismissal. A removal of a node another deletion trashed counts, so a new notice needs new deletions.
- A trash group is named after its commit's `commit_id` and the op's index (RFC 0012). A server that loses commits gives their seqs to later commits but not their groups, so the listing after the loss shows the lost deletion ended, and a delete that took its seq is a new deletion. A lost commit sent again keeps its groups, so its deletion stays counted or dismissed.

`core/engine/tests/guard_model.rs` checks this model against the engine.

## 6.2 A missing root pauses the space

A sync root that disappears or empties is an error, never "the user deleted everything". Before every scan that can report removals, full or subtree, and whenever the fs layer reports the root or its marker removed, the driver checks the following and sends the result with the observation batch (§1):

- the root exists, is a directory and is readable;
- `.koidrive/space.json` exists and names this space;
- the root is on the same file system as recorded (`st_dev`), so a different volume mounted at the same path is noticed;
- the scan did not fail anywhere; an unreadable subtree is unknown, not absent.

If any check fails, the engine pauses that space and reports a problem, and no op is built from that scan. A root that is present and marked but has no files while S has many is a mass delete and goes through §6.1.

## 6.3 Restoring after a bad delete

| Tool | What the user does | What the client does |
|---|---|---|
| Trash groups (spec §4.1, R9) | Restores a deletion: the app lists the trash (`ListTrash`), groups entries by `trash_group` and sends `Restore` ops through the engine (`UserOps`), 1,000 per commit | Restores come back as remote changes and every device downloads them. The device that deleted must not delete again: a missing root keeps it paused (§6.2), and a user action was answered at the guard prompt |
| Version restore (R10) | Restores previous versions of overwritten files | The same; the server orders the restore like any op |
| Rewind (RFC 0003) | The owner rewinds the Space DO to an earlier time with the Durable Object point-in-time recovery API; the Space DO records a new `history_id` (spec §7.4) | Recovery (§6.4) |
| R2 export (RFC 0004) | The server keeps an export of each space's tree, trash and versions in R2 | No tool imports an export yet (docs/guide/backup.md). An imported space would be re-linked and recovered as in §6.4, with node IDs kept |

Every restore goes through the engine, because a `Restore` op committed by anything else would move the server's `device_seq` past the store's accepted counter, and the next `Welcome` would read as a rolled-back store (§2.8):

- The app and the CLI ask the driver, which records the request in the folder's store and returns its ID. This works while the folder is stopped, offline or halted. The folder's task hands the request to the engine when it is recorded or when it next opens the folder.
- The engine sends the op in the next commit (§2.6) and keeps the outcome (§2.7): restored, restored under another name, restored as a copy, rejected with the server's reason, or not fitting in one commit.
- The driver lists pending requests and outcomes. A request whose node is a problem stays pending until the problem is cleared, and the list names that reason. A restore that waits for S (§2.6) has no reason in the list; a surface explains it as waiting for the folder to sync, shows the folder's problems, and does not promise that it goes through by itself.
- The store keeps the outcomes of the last 1,000 restores that ended and drops older ones.
- A request cannot be cancelled, and a rejected one is not retried; asking again is a new request.
- A malformed ID is refused when the request is made, because the server refuses a whole commit for one, and that refusal is a policy halt (§4.2).

After `HISTORY_CHANGED` the engine never resyncs by path and content, which would turn a rename made after the restore point into two files and upload again the stale files the owner meant to roll back. It recovers as §6.4 says.

## 6.4 Recovery after a rewind or a rolled-back store

The engine detects every trigger below and stops: the folder halts with a problem, nothing is committed, the commit in flight and the queued restores are kept, and the store stays as evidence (`Halt::Recovery` in `core/engine/src/commit.rs`; `Stop::StoreBehind` in `core/driver/src/disk.rs`). The classification, questions and answers below are the design for leaving recovery. The engine implements them only for a device with no store (`core/engine/src/first_link.rs`).

### Triggers

| Trigger | Cause |
|---|---|
| `ResyncRequired(HISTORY_CHANGED)`, a `Welcome` with another `history_id`, or `HISTORY_MISMATCH` | The owner rewound the space, or the Durable Object was restored |
| `Welcome` reports a `device_seq` the store cannot account for, or a commit is refused with `STALE_DEVICE_STATE` (§2.8) | This device's store was restored from a backup or copied from another machine |
| The root marker's effect counter is ahead of the store's (§2.8) | The store was restored to before disk effects it had already applied, with no commit of its own since |
| A re-link to a space imported from an R2 export | The space was rebuilt elsewhere; the old root maps to the new one |

### Persisted state

Entering recovery is one store transaction that copies R, S and L into evidence tables (R₀, S₀, L₀) and records the trigger, the old `history_id`, and the commit in flight with its `device_seq`. The commit in flight is never resent. Its ops that change the tree survive as differences between L₀ and S₀. Its other ops, a version or trash `Restore` the user asked for, leave no trace in the trees, so they are kept as explicit intents I₀. The evidence stays until every held node and every intent in I₀ has a disposition, even when no file is held. Every later step is idempotent, so a crash during recovery resumes it.

### Steps

1. Pause: no commits and no effects, except the recovery of an effect already in flight (§5.3).
2. Bootstrap the new tree R′ into a staging tree (§4.4): the live tree with `ListTree`, the trash with `ListTrash`, then the log replay up to the highest page head. For a store rollback within one history, R′ is the server's current tree, and the log is also read from the store's cursor, which with R₀ shows every place the server gave each node since then. Adopt the new `history_id`, and continue `device_seq` from the value the server reported.
3. Classify every node ID in R′, S₀ or L, and every intent in I₀, by whether this device changed it since its last sync (L against S₀), never by path or content. Ops a user could have taken more than one way are never inferred.
4. Apply the certain cases at once through the ordinary planner: set S for those nodes as the table says, and let §2.3 produce the effects. Hold the rest.
5. Resume normal sync for every node that is not held. Held nodes are excluded from planning, like guard holds (§6.1), and stay on disk exactly as they are.
6. Ask the user about the held groups. Each answer applies to the group's listed nodes at the revisions and contents it showed; anything that changed since needs a new question.
7. Leave recovery when no held node and no undecided intent remains, and drop the evidence tables.

### Classification

"In R′" includes R′'s trash, so a node trashed before the restore point counts as known. "Unchanged" means L(n) = S₀(n), comparing parent, name, kind and content.

| L against S₀ | R′ | Certain? | Action |
|---|---|---|---|
| any | L(n) equals R′(n) | yes | S(n) := R′(n); nothing to do |
| unchanged | live or trashed | yes | Follow R′: S(n) := L(n), so R′ shows as a remote change and the disk is moved, renamed, updated or cleared to the local trash. A rename made after the restore point is reverted as one move |
| unchanged | absent (created after the restore point, or purged) | no | Held as created after the restore point. Keep: commit it again under the same `node_id` (RFC 0002 §7). Discard: move the local copy to the local trash |
| changed (an edit or a move) | live or trashed | no | Held as changed on this device. Publish: S(n) := R′(n), so the local version is committed against R′ with the normal rules. Discard: move the local version to the local trash and set S(n) := L(n), so R′ is downloaded |
| changed (an edit or a move) | absent | no | Held as changed on this device. Publish: commit it again under the same `node_id` with its local content. Discard: move it to the local trash |
| new on this device (S₀ and R′ have no such node) | absent | no | Held as created on this device. Publish: commit it as new. Discard: move it to the local trash |
| deleted on this device (in S₀, not in L) | live | no | Held as deleted on this device. Delete: commit the delete against R′, through the guard of §6.1. Restore: S(n) := ⊥, so R′ is downloaded |
| deleted on this device | absent or trashed | yes | Nothing to do |
| absent (not in S₀ or L) | live | yes | Download |

- Held nodes are classified again, with the same table against S₀, whenever R′ or L changes for them. A question is dropped only when a certain row applies, such as the local version and R′(n) becoming equal; a node that merely reappears in R′ is not certain. If another device publishes n = C while this device holds n = B, then B ≠ C, or B = C at another position, stays held with its question refreshed to C's revision, and B = C at the same position becomes certain.
- A recovery commit rejected with `ID_EXISTS` is classified again the same way instead of becoming a problem.
- An excluded node (§5.10) stays excluded and is not classified.

### Intents

An intent in I₀ whose result R′ already shows (the node live at the requested position with the requested version's content) is done. Otherwise it is held as requested before the reset: Repeat sends it again against R′ as a new op under the normal rules, and Cancel drops it. It is never repeated without an answer.

### Answers act on the whole server transition

An answer names a set of nodes, but an op can change others on the server: R8 revives the trash group of a node it touches, R2 merges directories, and a move carries its subtree.

- Before sending an answer, the engine applies its ops to a copy of R′ with the server's rules (§7.1). Every node the result changes outside the answered set must end where it was meant to be: in its R′ state, or in the state the evidence shows this device's user chose. The engine adds the ops for that to the same commit only when the simulated result confirms them. When no such commit exists, the extra nodes join the question, which is asked again before anything is sent.
- For example, S₀ has directory D with child F; the user moved D and deleted F; R′ has D and F trashed in one trash group. Publishing D revives both under R8, so the commit is `Move(D)` followed by `Delete(F)`, sent only if the simulation leaves F trashed.
- An answer's commit sets `require_head` (RFC 0002 §4): the server applies it only if its head is still the `base_seq` the simulation used, and otherwise refuses the whole commit without applying anything. The engine then updates R′ and asks again.
- Following R′ can remove many files at once, since a rewind removes everything created after the restore point. Those removals go to the local trash, trigger the remote-deletion notice of §6.1, and never touch held nodes. The guard applies to every `Delete` an answer produces.

### What the user sees

One recovery notice per space names the trigger and, when the server reports it (`restore_point_ms`, RFC 0002 §8), the restore point. Five groups list counts and paths: created after the restore point, changed on this device, created on this device, deleted on this device, and requested before the reset (`RecoveryGroup`). Each group offers its two answers for all of its nodes. Until answered, held nodes are listed as not synced, and nothing about them leaves the device.

Nothing local is published, and nothing is deleted on either side, without an answer, except what the device had not changed since its last sync, and that goes to the local trash.

### Entries the store lost track of

A store rolled back to before a download has no S₀ for what the download put on disk. Before classifying, each local entry S₀ does not hold is matched against R′ as on a first link (below), then against the one node R′ holds in the trash at its place, and then against the places R₀ and the log since the store's cursor show: the one node of R′ that the server held at exactly that place, with the same kind and, for a file, the same content. An entry is also held when S cannot take the match's shape there, as when S would hold a cycle. A matched entry takes the node's ID, and S(n) takes the shape that matched, so the entry is unchanged and follows R′, also when the server moved, renamed or deleted the node since. When the log no longer reaches back to the store's cursor, the entry is held as created on this device.

### A device with no store

A first link to an existing space, or a lost store, has no S₀. The engine matches local entries against R′ from the root down: an entry matches the live node at the same parent with exactly its name and kind, and for a file the same content. A match is adopted (L's node takes R's ID and S takes R's shape), and a matching folder's children are matched in turn. Every other local entry is held as created on this device until the person publishes or discards it, and R′'s other nodes are downloaded. An entry the rules exclude is neither adopted nor held.
