# 2. State model

## 2.1 The engine persists observations, not work

The engine derives every disk effect and every commit op from the differences between three trees (§2.2). The only intents it persists are the commit in flight, the effects in flight, the queued restores and the open questions. Each is recorded before it starts and bound to the server history it was planned against.

## 2.2 Three trees, keyed by node ID

R, L and S are keyed by `node_id` (spec §2) and share the root node (`node_id = space_id`). Each is a valid tree on its own: every non-root node has a parent that is a directory in the same tree, there are no cycles, and in R and S no two live children of a directory share a `name_key` (spec §6.1).

- R, the remote tree, holds the `NodeState` records (spec §5.2) this device has applied, trashed nodes and their revs included, with `history_id` and `last_seq`, the highest seq up to which every record has been applied. A trashed node counts as absent when trees are compared.
- L, the local tree, is the disk as last observed. Each node has a parent, its exact on-disk name, kind, `Content` for files, and the file identity from `core/fs` (`dev`, `ino`, birth time) that tells a rename from a delete and a create. A parked node (§2.5) keeps its S position for comparison.
- S, the synced tree, holds per node the shape (parent, name, kind, content) on which L and R last agreed. S is the merge base: L ≠ S is a local change, R ≠ S a remote change. S also keeps the pending mark R carried at that position (RFC 0017).

Names are compared after NFC normalization. Whether a name is taken is decided by `name_key = NFD(casefold(NFD(name)))` (RFC 0001 §1) on every platform, never by the local file system's case rules.

Besides the trees, the engine persists:

- the commit in flight, at most one (spec §8): `commit_id`, its `device_seq` (§2.8), the `history_id` and `base_seq` it was built against with the commit token at `base_seq` (RFC 0013), the ops with the shape each op sent, and a received result that cannot be applied yet (§4.4);
- the effects in flight, at most one batch: an effect record of §5.2 for each;
- the mass-delete state of §6.1;
- the restores a person asked for (§2.6, §6.3): each one without an outcome under its request ID, and the outcomes of the last 1,000 that ended;
- the unsent marks (RFC 0017): each folder L holds where S held it marked when S dropped it, while R holds no such node, with its mark.

Nodes the commit in flight touches are frozen: the planner neither changes them on disk nor rebases them until the result is applied.

## 2.3 Classification decides the direction of each change

The planner looks only at dirty nodes, those whose L, S or R entry changed since they were last found in sync.

| L vs S | R vs S | Meaning | Action |
|---|---|---|---|
| equal | equal | in sync | none; the node leaves the dirty set |
| equal | differs | remote change | apply R to disk (§2.5) |
| differs | equal | local change | upload as ops (§2.6) |
| differs | differs, L = R | both sides made the same change ("same") | S := R |
| differs | differs, L ≠ R | concurrent change | rebase on the client (§2.4) |

A node that is not dirty on one side takes the other side's value. A node dirty on both sides is never overwritten by the engine.

S := R for a change both sides made alike can wait, because S stays a valid tree (§2.7) and the new name may still be held in S. S then takes R's content at once and the position once it fits, so the next edit on either side is compared with content both sides hold. A node whose acknowledged update waits keeps that update instead.

## 2.4 The client rebases the conflicts it has seen

A commit claims, through `base_seq = last_seq`, that the device has seen everything up to `last_seq`, while a local change was made against S, which may be older than R. So before building a commit the client rebases each concurrent change onto R with the outcome the server would choose had it not been seen (spec §6.2):

| Local (L vs S) | Remote (R vs S) | Client action | Matches |
|---|---|---|---|
| content edit | content edit | Fork: the local file becomes a new node Y with the local content (`CreateFile` under the original name), which the server gives a conflict name; the original is downloaded again with the remote content | R3, R1 |
| content edit | rename or move | Send the edit; both apply | R4 |
| delete | any change: content, move, rename, or a new child | Keep: drop the delete for that node and every ancestor still needed, and set S := ⊥ for them so they are downloaded again. Delete only the nodes that did not change remotely | R7 |
| content edit or move | delete (trashed) | Send the op; the server revives the node and the ancestors it needs | R8 |
| move | move | Send the move; the later commit wins | R5 |
| move that would close a cycle against R | any | Revert: the node is reverted (§2.6), S is unchanged, and effects move it back to its R position | R6 |
| create | create of the same name | Send the create; the server renames a file (R1) or merges a directory (R2) | R1, R2 |

The server resolves the conflicts committed after `base_seq`, which the client has not seen, with the same rules. Every concurrent pair is resolved exactly once.

## 2.5 Applying remote changes to disk

For a node with a remote change and no local change, the planner emits one effect:

| Remote change | Effect | Expected state the driver verifies (§5.2) |
|---|---|---|
| new directory | `CreateDir(path)` | the parent has the recorded identity; the name is free (no-replace create) |
| new file | `WriteFile(path, content)` | as above; the content is fully downloaded and verified in staging |
| content change | `WriteFile(path, content, replaces)` | the displaced file, once quarantined, has the identity and content `replaces` (S's content) |
| move or rename | `Move(from, to)` | the source has the recorded identity; the target name is free (no-replace rename) |
| trashed file | `RemoveFile(path)` | the quarantined file has S's content |
| trashed directory | `RemoveDir(path)` | the quarantined directory is empty or holds only platform files (§5.2); children are removed first, bottom up |

Every effect follows these rules:

- An effect that displaces an entry goes through the quarantine protocol of §5.2. Only an entry that matches the expected state reaches the local trash.
- Paths come from L when the effect is emitted. The effect record is persisted before the effect reaches the driver.
- On a mismatch the driver puts the entry back and reports `EffectFailed` with fresh observations. The engine updates L, and the next plan sees a local or concurrent change.
- The engine never retries an effect blindly and never emits an effect for a node with a local change.
- At most one effect is outstanding at a time.

Some remote states cannot be reached one move at a time, such as two files that swap names or a directory that moves under its former child. When no remote move applies but some are pending, the planner parks one blocking node: it moves the node into `.koidrive/parked/<node_id>` and moves it to its final place later. Parking follows these rules, and §5.10 adds those for excluded nodes:

- Parked nodes are excluded from local-change detection. The rescan after a crash finds them in the parked directory and puts them back into L instead of treating them as deleted.
- Parking moves an entry whole and never changes its content.
- A reverted node (§2.6) that blocks another placement is parked like a node whose only change is remote, so two reverted nodes that hold each other's names are placed one after the other.
- A parked reverted node that R no longer has and that holds no unuploaded edit is removed from the parked directory.
- A parked node that R no longer has and that holds an unuploaded edit, reverted or not, becomes a new node and moves back to its local position, so the edit is uploaded as a create. While an entry holds that position, while R gives it to another node this device does not exclude, or while L no longer holds the folder, the move back is not made: the new node is created from its local position while still parked, and the effects then move it to the position R gives it.
- A parked file re-keyed this way keeps its entry name in the parked directory, the ID it was parked under, until it leaves; effects address it by that name.
- A folder L deletes while a node under it is parked is reverted, so it is created again and the node moves into it.
- A node whose S update waits (§2.7) is not planned, unless it is reverted (§2.6) and the update keeps it in S or L no longer holds it. When it blocks a placement it is parked too, and the effects then place it at its R position. A local rename it held onto a name another node needs is not kept.

## 2.6 Building a commit

The planner builds a commit only when the device is connected, caught up (§4.4) and has no commit in flight. Local changes become ops:

- New nodes become `CreateDir` or `CreateFile`, parents first.
- Position changes become `Move`.
- Content changes become `UpdateContent`.
- Deleted subtrees become one `Delete` per topmost deleted node, after the keep rule of §2.4 and the guard of §6.1.

The planner orders ops by simulating them on a scratch copy of R. It repeatedly emits an op that is valid there (parent present, name free, no cycle, and for a delete no surviving child under it) and applies it.

An op into a folder R holds in the trash revives that folder and every trashed folder above it at their old positions (R8). A `Move` or `Restore` of a trashed folder brings back only the folder and its trash group, at the position it names. So a create, move, edit or restore that would revive a trashed folder waits for a restore of that folder or of a trashed folder above it, because a restore of a live folder does nothing. It also waits for a move of such a folder whose own parent is in the trash, because going first would bring back that parent, which the move leaves behind. When the moving folder's parent is live, the op goes first and revives only what the move would bring back. A move into the node it moves, or into a folder the commit creates inside it, is a cycle (R6), and a restore into the node it restores is sent for the server to refuse. When the waits form a cycle, the planner sends one op of the cycle.

### Removals a move out of the trash brings back

A `Move` or `Restore` of a trashed folder also brings back the members of its trash group that this device's user removed. A `Delete` of such a member in the same commit is superseded, because the commit's own move changed the member after `base_seq` (RFC 0002 §9). So the member is deleted in the next commit and is never downloaded:

- A member the user removed while R held it trashed by another device's delete is recorded with that delete's seq (`removed_here`) while R holds it trashed by it. It is recorded only when R's trashed content is the content S held when the removal settled, so the user removed what they saw. When a record of this device's commit shows the member live again, S takes R's shape, so the removal is a local delete again, unless a restore of that commit brings it back.
- A `Delete` that its own commit's move or restore superseded leaves S at R's shape for the same reason.
- A member whose content in R is newer than what its user saw when removing it is downloaded, as an edit over a delete is. For a superseded `Delete` that is content newer than its commit's `base_seq`. For a recorded member in a listing, it is content newer than the recorded delete's seq. Neither acknowledgment nor a listing makes such a member a local delete again.
- A listing that replaces R shows no op either. A recorded member that the listing holds live and L doesn't hold becomes a local delete again when this device brought it back: its revision is the seq of a commit of this device that the server kept, or the seq of the result of the commit in flight. Unless committing is halted, a folder publishes a listing only after the commit it sent has its result, so that seq is known.
- The scratch copy brings back the trash group with the folder. A create or move onto the name of a member that the next commit deletes waits for that commit, so it keeps its name instead of getting a conflict name.
- A restore the user asks for brings back what it restores. The commit in flight keeps, for each `Restore` by its index, the removed members it brings back: the restored node, each member below it in its trash group, and each trashed folder above its target, which the server brings back as a parent. The server answers each op on its own, and it applies a `Restore` of a node that is already live, so the restore's own result decides, whichever op of the commit brought the member back. When the result is applied, acknowledgment clears S for each of those members, and, for each `Delete` of the same commit that planned to delete the restored node, for the restored node and each node below it that the delete planned, when R holds it live and L doesn't hold it, so it is downloaded. This covers a `Delete` that another device's concurrent delete and the commit's own move turned into a superseded one. Otherwise each stays a local delete. A restore is ordered against the other ops of its commit as any restore is.

### Name cycles and pending names

- When no op is valid, the planner looks for a name cycle: moves that each wait only for a name held by a node whose own move is in the cycle, as in a swap or a rotation through a third name. It moves one node of the cycle to a free pending name under its target folder, which frees the old name. The other moves become valid, and that node moves from the pending name to its target last. A move into a trashed folder that waits for the folder's own move counts as waiting for that move, so a trashed folder moving onto the name of a folder that moves into it goes through a pending name. Change records hold each node's final state, so other devices never see the pending name, and every node keeps its identity and exact name.
- A cycle is sent only when the commit has room for all its moves. A longer cycle is split, and the moved-aside node keeps its pending name until a later commit.
- A move that waits for a name held by a node that does not move away is a real conflict. It is sent at the end for the server to resolve (R1).
- A `CreateDir` may be merged into the folder that holds its name (R2), and that can be a folder another device renamed after R, under the same parent or under a folder the parent may itself be merged into. A delete of such a folder goes before the `CreateDir` when it can. Otherwise it waits for the next commit, and so does a create or move that waits for the deleted folder's name. Sent after the `CreateDir` in the same commit, the delete would trash what the merged folder brings: R7 keeps the renamed folder but not the nodes the commit put in it.
- When the node holding a new folder's name, a folder or a file, is itself moving away, the new folder is created under a free pending name and renamed in a later commit. A pending name is `<name> (pending)`, or `<name> (pending 2)` and so on when that is taken, cut at a character boundary so that its NFC form fits in 255 bytes.
- A `CreateDir` or `Move` that leaves a node at a pending name carries the name the node is pending for as a mark (RFC 0017), and the server keeps it on the node until a move clears it. The server also keeps the device whose commit set the mark. When nothing else is to be sent, a device whose L, S and R hold a marked node at the same position sends the move to that name itself, in a commit that requires the head it was built on, and the effects follow the result. Any device does so when no other node of L or R holds the name. The device that set the mark does so also when another node holds it, and the server then gives the node another name under R1; other devices wait, since that node may be about to move away in a commit of the device that set the mark.
- So the rename never depends on the store of the device that owes it. It is finished after that store is rolled back, since the device finds itself in R, after a rewind to a copy taken between the two commits, when the keep rule (R7) brings back a folder the person deleted and the effects download it at the pending name, and by another device when that device never comes back and the name is free (GUN-514, GUN-547). When that device never comes back and another node holds the name for good, the node keeps its pending name.
- A device whose finishing commit finds the head moved sends no other one until it has applied a newer head, and then plans again from the new R. A node no mark names keeps a name that only looks pending.

### Reverted nodes

A reverted node is one whose local position the server will not take: a move that closes a cycle against R (never sent) or that the server rejects with `CYCLE`, a create or move the server renamed (`APPLIED_RENAMED`, §2.7), or a folder merged into one that L holds elsewhere, with the nodes created in it (§2.7).

- The planner sends no create or move for a reverted node, but sends its content edits and a delete the user makes.
- The effects of §2.5 make its position on disk equal to R, as for a remote change, but never replace its content.
- When R no longer has it, the effects remove it only if it holds no unuploaded edit. A file that holds one becomes a new node and the edit is uploaded as a create.
- A reverted node that the server lost, so that R does not hold it at all, stops being reverted when S forgets it or drops its waiting update (§4.4). It keeps its ID and is created again at the position L holds.
- A folder that L deleted on the way from a reverted or parked node to the root is reverted too, so it is created again. So is a folder that the keep rule cannot clear from S because S still has children under it.
- A node stops being reverted once L has it at its R position.

### Size limits

- A create whose parent is neither on the server nor in this commit waits for a later commit.
- A commit holds at most 1,000 ops (spec §13). The cut keeps parents before children.
- A commit's change records and its result must each fit in one 1 MiB message (spec §7.2). The planner stops at an estimate of 512 KiB of change records, counting a record with name and content for every node an op changes. A delete whose subtree exceeds the estimate is planned as deletes of the folder's contents first, deepest first, with the folder deleted in a later commit. `LIMIT_EXCEEDED` (§4.2) splits further.

### Restores

A restore leaves no difference between L and S, so the engine queues each request as an explicit intent under its request ID, and the planner adds a `Restore` op for it:

- Queued restores come first, in request order, so the first commit with room carries them.
- A restore follows every op the commit has for its own node: an unsent edit is sent before the restore of an earlier version, and a node deleted here is deleted before it is restored. While such an op is left for a later commit, the restore waits with it.
- A restore waits while the planner leaves its node out (a problem node of §2.7, a node whose S update waits, a parked node), and while L and R agree on its node but S has not taken that yet (class "same", §2.3). Sent earlier, the restore would be replaced by an unsent edit the person made before asking, or the engine would upload the disk's content over the restored version.
- A restore from the trash of a node R holds as trashed places the node in the scratch copy at the position the request names, or at the one it was trashed at. A folder is then not deleted in the same commit, because the delete would trash the restored node again; the keep rule brings the folder back after the commit.
- The size estimate counts a record for every node R holds in the node's trash group, or one record when R does not hold the node as trashed.
- The engine does not judge a restore against R. The server answers every case (§2.7).
- A commit may hold only restores. A queued restore stays queued while committing is halted for recovery or by a policy halt (§4.2). On a read-only device it ends as rejected.

The commit is persisted as in flight, with a fresh `commit_id` and the current `history_id`, before `SendCommit` is emitted.

## 2.7 After a commit result

Applying a result's records to R and acknowledging the commit are separate steps. Records are applied only in seq order (§4.4). Acknowledging makes the S and L updates below, including any re-keying, and clears the in-flight record, in one store transaction.

- A result whose seq is `last_seq + 1` is applied to R and acknowledged at once.
- A result further ahead is stored with the commit in flight. Its nodes stay frozen, and it is applied and acknowledged once the commits before it are applied.
- A result at or below `last_seq` is not an error: a log replay already brought its records. Its records are skipped and the commit is acknowledged at once.

S is updated from what was sent, not from what the server returned, so any difference shows up as a remote change and the disk follows the server.

| Op | Result | S | L |
|---|---|---|---|
| `CreateDir`, `CreateFile` n | `APPLIED` | S(n) := sent shape, with the agreed-position exception below. For `CreateFile`, S(n) keeps its content when that content changed since the commit was built, and takes the sent content otherwise | unchanged |
| `CreateDir`, `CreateFile` n | `APPLIED_RENAMED` | sent shape with the server's name, with the same exception | n is reverted (§2.6): the disk takes the server's name, unless S(n) took or waits for a newer position |
| `CreateDir` A | `MERGED_INTO` B | S(B) := sent shape | Re-key A as B; children follow. If B is already in L elsewhere, A keeps its ID and S(A) stays ⊥; A and the nodes created in it are reverted, so on disk they move into B and A is removed once empty. Later creates and moves into A are sent into B and reverted the same way, and so is a node in A that B already holds |
| `UpdateContent` n | `APPLIED` | S(n).content := sent content, unless S(n)'s content changed since the commit was built, even back to its old value; S(n) then keeps what it holds | unchanged |
| `UpdateContent` n | `CONFLICT_COPY` Y | S(Y) := sent shape and content; S(n) := ⊥ | re-key the local file as Y; n is downloaded again |
| `Move` n | `APPLIED` | S(n) position := sent position, with the agreed-position exception below | unchanged |
| `Move` n | `APPLIED_RENAMED` | sent parent with the server's name, with the same exception | n is reverted: the disk takes the server's name, unless S(n) took or waits for a newer position |
| `Move` n | `REJECTED` (`CYCLE`) | unchanged | n is reverted: the disk moves it back to its R position |
| `Delete` | `APPLIED`, `SUPERSEDED` | S := ⊥ for every node the client planned to delete | unchanged; nodes the server kept are live in R and absent in L and S, so they are downloaded again. Each node R holds trashed by this commit is recorded as trashed here (§4.4) |
| any but `Restore` | other `REJECTED` | unchanged | the node is a problem, excluded from planning until the next rescan or a user action |
| `Restore` | any | unchanged | unchanged; see below |

The store keeps, for each node the commit creates or updates, whether S's content changed since the build, and for each node it creates or moves, the latest position L and R agreed on since the build, with the revision of R's record that held it. The agreed-position exception uses that record:

- If L and R agreed on a position since the build, at a revision of R after the result's seq, S(n) takes that position, whether or not S(n) already took it.
- If S cannot hold that position once the acknowledgement's other updates are made, because another node holds the name or the parent is gone, S(n) takes the sent position and the agreed position waits as n's S update while L or R still holds n there. n's position is not planned while it waits, an edit L makes at the agreed position is planned, and S takes the agreed position once it fits. Agreed positions that only fit together, as in a swap, are made together when another update of the acknowledgement does not fit.
- A position only R came back to is a remote change: S(n) takes the sent position and the effects follow R.
- A position from a record at or before the result's seq does not count. The server applies a move whatever other devices did after `base_seq` (§3.6), so the commit replaced it.

A `Restore` op sets nothing from what was sent. Its result takes the request out of the queue with an outcome:

- `APPLIED`: restored, also for a node that was already live.
- `APPLIED_RENAMED`: restored under the server's name.
- `CONFLICT_COPY` Y: the version came back as the new file Y.
- `REJECTED`: rejected with the reason, `NOT_FOUND` for a node, version or target folder that was purged or expired. Any other code is a rejection that names the code.

The node is never marked as a problem, reverted or re-keyed. What the restore brought back is a remote change once R holds the result's records, and the effects of §2.5 apply it. The transaction that clears the commit in flight saves the outcome, so a restore is neither lost nor sent twice.

### S stays a valid tree

An S update above that would orphan a node, take a sibling's name key or close a cycle waits, and is made together with the updates it depends on, including the §2.3 updates both sides agree on.

- A node with a waiting update is not planned until the update is made, unless L edited it, or moved it away from the waiting position while R still holds it there. Such a node is rebased and planned against the waiting shape, also when R changed its content.
- An edit L made is uploaded wherever R holds the node, even when the node it waits behind is a problem node that never moves. A move is planned, because the update may never fit while L holds the node elsewhere.
- Until the update is made, the waiting shape stands in for S when the engine asks whether L holds an unuploaded edit. When R drops such a node, the effects remove it if it holds none, and the commit layer makes it a new node if it does.
- The effects compare L and R with the waiting shape, not with S. A node whose L matches S but not the waiting shape is not changed only remotely, so its content is never replaced, also when it is moved because it is reverted or parked. A node whose L still holds the waiting shape while R differs is changed only remotely, and the effects give it R's shape.
- A failed effect on a waiting node blocks it until the effect planned for it changes.
- A waiting update is dropped once anything else sets S for the node, such as settling a change both sides made alike or the keep rule bringing back a node L deleted, and once neither L nor R holds the node.
- A waiting removal is also dropped, even while R holds the node again, once something S holds under the node will not leave it with L's commits, such as a reverted node that R holds there. The keep rule then brings the folder back for that node.
- The keep rule compares R with the waiting shape, so a node R holds as the update left it has no remote change to keep.
- A waiting position is stale once R no longer has the node there and L has left it too. It then falls back to the position S holds, while its content stays the base for a later edit.
- Waiting updates that fit are made even while committing is halted, since downloads go on.

### Conflict questions

A conflict opens a question about a pair of files. The question holds nothing, so both files keep syncing.

- An `UpdateContent n` answered with `CONFLICT_COPY` Y, and a file this device created or moved that the server answered with `APPLIED_RENAMED` because a file n held the name, open a question: n is the original, Y or the renamed file the copy. Only the device whose write the server turned aside asks.
- A create sent because the person answered Publish to a first link's question (§6.4) opens none. The commit layer records the entries that answer released until the server applies their create, across several commits and a forgotten commit. A later move of such a file that takes a conflict name asks as any move does.
- The question records both node IDs and their revisions in R, and is persisted with the other open questions (§6.1).
- Before each commit, and while the question is open, the engine compares the pair with R and L. The question ends when R no longer holds either file live at the meta revision it was asked with, or when L moved or deleted either file that S, or its waiting update, holds. When R holds another content revision of either file, the question is asked again under a new ID, and an answer to the old ID changes nothing.
- Keep both ends the question. Keep the original sends `Delete(Y)`. Keep the copy first sends `UpdateContent n` with Y's content, so n's own content stays in its version history; once that result has arrived and the download of §2.5 has brought the new content to L, it sends `Delete(Y)`.
- Each `Delete(Y)` requires n live at the revisions R holds for it when the commit is built (RFC 0015). If another device renamed, moved or deleted n first, the server answers `SUPERSEDED` and trashes nothing. An applied `Delete(Y)` counts as this device placing n, so a `Delete(n)` another device built before the answer keeps n (R7). The record of Y as trashed here (§4.4) keeps n's ID, so a `Delete(Y)` sent again after a server loss carries the same precondition at the revisions R then holds, and none is sent once R no longer holds n live.
- Each answer sends one op per commit, so it fits after `LIMIT_EXCEEDED` has cut the op count to one. A build sends no op for a node another answer in it already took.
- The engine sends answer ops only while L, S and R agree on both files, and leaves S as it is when the result arrives, so the disk follows as for another device's edit and delete. A file the person edited meanwhile is kept (R7).
- Only an op of exactly that shape counts as the answer's op when its result arrives: a `Delete(Y)`, or an `UpdateContent n` with Y's content in R. An upload of an edit of n is acknowledged as any op is, and its new content revision asks the question again.
- The question closes once the server applied its `Delete`. Any other result clears the answer and the driver shows the question again; when the server refused because a file changed, the next build asks it under a new ID.
- An `UpdateContent n` that the server turns into a conflict copy opens a question about that copy too.
- When R's records of Y leave out its chunk list (more than 16,384 chunks), keep the copy cannot be sent, and its answer is cleared.

## 2.8 Persistence and crash recovery

The store writes, per `handle` call and in one transaction:

- R with `history_id`, `last_seq` and whether the server reported that it is behind `last_seq` (§4.4), S, and L with file identities;
- the commit in flight with its `history_id`, `device_seq` and any buffered result, and the accepted device counter;
- the effects in flight and the effect counter;
- the mass-delete state of §6.1 (`koidrive_engine::guard::GuardState`);
- the commit layer's reverted nodes, merged folders (§2.7), folders whose delete is split (§2.6, §5.10), the reduced op count after `LIMIT_EXCEEDED`, and the waiting S updates with the slot each came from;
- the nodes recorded as trashed or edited here, and the seqs of the last acknowledged commits by slot (§4.4);
- parked nodes with the name of their parked entry, and held and problem nodes;
- the queued restores with the outcomes of those that ended;
- the entries a Publish answer to a first link released, until the server applies their create (§2.7);
- the nodes recorded as moved into an ignored folder that is no node, each with its file identity once another entry took its path, and which of them are folders the disk lost that L keeps only for recorded nodes below them (§5.10);
- every open question with what it holds;
- the ID generator's state (`IdGen::state`).

Connection state and queued outputs other than the commit and effects in flight are not persisted. Restoring `koidrive_engine::state` refuses a state that breaks an invariant. After a restart the engine emits `AskUser` again for every unanswered question; the driver shows a question ID once, so a repeat is harmless.

### Restart

After a crash the fs driver first recovers the effects in flight from their records (§5.3) and reports the outcomes. Only then does it rescan the whole root. Everything on disk that L does not know is then a new local node. A device with no store, on a first link or after losing its store, adopts entries by path and content instead (§6.4).

### The device counter detects a store older than the server's history

A store restored from a backup or copied to another machine looks valid. The engine detects it through a per-device commit counter, `device_seq` (RFC 0002 §2):

- The store keeps the accepted counter *a*, the `device_seq` of the last commit whose result this device applied. A new commit takes slot *a* + 1, persisted with the commit before it is sent.
- A commit whose outcome is uncertain keeps its slot and its `commit_id` on every retry.
- A commit refused as a whole by a call-level error such as `MISSING_CHUNKS` or `UNSUPPORTED_VERSION`, so that nothing was applied, does not use up its slot. The re-planned commit gets a new `commit_id` and the same slot *a* + 1.
- Applying a commit's result sets *a* to its slot, in the transaction that clears the commit in flight.

The server keeps, per device, the highest `device_seq` it accepted and that commit's result, and reports the counter in `Welcome` as *w*. On reconnect:

| *w* against the store | Meaning | Action |
|---|---|---|
| *w* = *a* | the commit in flight, if any, was not accepted | Resend it unchanged. If its result has already arrived, the server accepted it and lost it: act as in the last row |
| *w* = *a* + 1, and the commit in flight has slot *a* + 1 | the commit was accepted and its response lost | Resend it unchanged; the server returns the stored result. Only `STALE_DEVICE_STATE`, meaning the server does not know that `commit_id`, makes this a rollback |
| *w* = *a* + 1 with no commit in flight, or *w* > *a* + 1 | the store is behind the server: restored or copied | Plan nothing from this store; the space enters recovery (§6.4) with the store as evidence |
| *w* < *a*, or *w* = *a* with the result of the commit in flight held | the server lost accepted commits of this device | *a* := *w*; drop the commit in flight and keep its restores queued; record R as ahead of the server and bootstrap as for `AHEAD_OF_SERVER` (§4.4) |

These ordering rules keep the counter comparison sound:

- The root marker's counter (below) is checked when the task starts, and the history before the device counter.
- The driver takes a `Welcome` before the result of any commit the session answered after it, so *w* is never older than a commit the store acknowledged.
- A new connection drops the results the session held behind a gap on the old one, so no event after a `Welcome` comes from an earlier connection, and a bootstrap after *w* < *a* takes nothing the server lost.
- A listing still out when such a `Welcome` arrives is dropped and listed again.

### The effect counter detects a store older than the disk

A store restored to before it applied a remote change, with no commit of its own since, still shows *w* = *a*, yet the disk holds effects the store does not remember. So the store keeps an effect counter *e*, the number of disk effects it has acknowledged, and the root marker `.koidrive/space.json` (§6.2) keeps a copy *m*.

- The effects of a batch of *n* are recorded in the store first (§5.2). Before the batch touches the disk, the driver writes *e* + *n* to the marker and flushes it. The transaction that clears the batch, each effect done or failed, sets *e* to that value. A healthy device has *m* = *e*, or *m* = *e* + *n* while a batch of *n* is in flight.
- On startup, before recovering the effects in flight (§5.3) and before planning from a rescan, the driver compares the two. When *m* is above *e* plus the number of effects recorded in flight, the store is older than the disk: nothing is recovered or planned from it, and the space enters recovery (§6.4) with the store as evidence.
- When *m* is below *e*, the disk or its marker was restored from an older state. The space pauses with a problem (§6.2) instead of reading the older files as local edits.

### The commit in flight follows its history

| Situation after reconnecting | Action |
|---|---|
| `Welcome.history_id` equals the commit's `history_id` | Resend unchanged. `commit_id` makes the server return the stored result if it applied the commit (spec §8 step 4), and the history fence (RFC 0002 §1) refuses it if the history changed in between |
| the server does not know the commit, and its `device_seq` is the next one the server expects | The commit was never applied, so the server applies it as new. Its old `base_seq` only makes more changes count as unseen; nothing is re-planned |
| `STALE_DEVICE_STATE`: the commit's `device_seq` is at or below the last one accepted (RFC 0002 §3) | The store is older than the device's history and the outcome cannot be known. The commit is never resent or re-planned; the space enters recovery (§6.4) |
| the history changed | Never resend or re-plan the commit. Its ops are recovery evidence and the space enters recovery (§6.4) |

A crash can cut on either side of a result's store transaction. Before it, the commit is still in flight and is resent, and the server returns the stored result, any `MERGED_INTO` or `CONFLICT_COPY` re-keying included, which is applied once. After it, nothing is in flight.

A restore follows the same cuts, because a `Restore` op exists on the wire only inside the commit in flight:

- A request is durable once its own transaction commits, in an inbox table of the store that the driver's API writes and the folder's task reads. Taking it, queuing it in the engine and saving the engine state are one transaction.
- The restore leaves the queue in the transaction that clears the commit and advances *a*.
- A commit abandoned because the server proved it was not applied only forgets the in-flight record, so its restores go into the next commit.
- When the space enters recovery, the commit in flight and every queued restore are kept as they are and never resent or planned again. They are the intents I₀ of §6.4.
