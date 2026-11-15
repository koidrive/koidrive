# 3. Correctness properties

These properties are claims about this design, checked by the tests of §7.

## 3.1 Unison's terms map to nodes

The terms follow Pierce and Vouillon ([2004](https://www.cis.upenn.edu/~bcpierce/papers/unisonspec.pdf)) and Balasubramaniam and Pierce ([1998](https://doi.org/10.1145/288235.288261)), applied per node instead of per path, with the server as one replica:

| Unison, B&P | Koidrive |
|---|---|
| archive `o` | S |
| replica `a` | L and the disk it observes |
| replica `b` | R, the server state this device knows |
| `dirty_a(p)` | L(n) ≠ S(n). Update detection is the fs rescans. It may over-report but never miss a change, so a lossy watcher triggers a rescan, never a guess (§5.1) |
| run | one planner step: an effect, a commit or a tree update |

## 3.2 Safety holds after every step

Both conditions hold after every step, since a crash can end a run anywhere:

- Preserve user changes: if L(n) ≠ S(n), no effect changes n on disk. The only disk operations on a locally changed node are those the user's own change implies after the server answers: renaming a fork to its conflict name (§2.7), or giving a recovered file a free name (§5.2). Both keep the content.
- Propagate only user changes: every effect makes L(n) equal to R(n), and every op makes R(n) equal to L(n). The two exceptions are conflict names, which the server chooses (spec §6.1), and parked locations (§2.5), which are hidden and never uploaded.

The engine resolves every conflict deterministically and keeps both sides (spec §1 principle 4). It never stops at a conflict to ask; §3.3 states what that must preserve.

## 3.3 No write is lost

The property counts writes, not contents:

- A write is one user action that leaves a file with some bytes: a create or an overwrite on some device. Two writes are different even when their bytes are identical.
- A write's home is the node it was written to, or the node the server put it in instead: the conflict copy for `CONFLICT_COPY`, the surviving directory's child for `MERGED_INTO`.
- A replica, a device's disk or the server's live tree, holds a write if the live file at its home has the write's bytes.
- A write is published once it reached the server in an applied commit.

A write w is superseded only by a user action on w itself:

- An unpublished write is superseded when the user of the device that wrote it overwrites or deletes the file holding it before it is published. An offline create followed by a delete, or a temporary file that lives between two rescans, leaves nothing to publish.
- A published write is superseded when, on some device whose disk showed w at its home, the user overwrote or deleted that file and the server accepted the change: an overwrite applied rather than turned into a conflict copy, or a delete applied rather than superseded under R7.
- An action on another node never supersedes w, including the other side of the same conflict. One device's action never supersedes another device's unpublished write. Sync effects never supersede.

A rewind of the space by its owner (RFC 0003) retires, on the server, every write published only after the restore point. A retired write is not superseded. Until it is published again, the devices that hold it carry the obligation: a copy held by recovery (§6.4) is conserved until answered, published again if the answer is Publish or Keep, and in the device's local trash if it is Discard.

Property: for writers within the contract of §5.2, once the system is quiescent (§3.4), every write that was neither superseded nor retired is held by the server and by every device on which its home is eligible. On a device where its home is excluded (§5.10), the obligation is conservation: if that device held the write when the home became excluded, it still holds it there.

A superseded write stays recoverable from the server for the retention period, as a version or in the trash (spec §12.2).

## 3.4 Quiescent devices converge

Quiescence means no user actions, every device online and connected, every message delivered, no injected faults, every question (§6.1) answered, a fair scheduler, and completion of every effect the driver starts.

A node is eligible unless it or an ancestor is a problem (unreadable, a name the local file system cannot hold, an ignored file occupying the name), is held for a decision nobody gives, or belongs to a space in recovery (§6.4) or paused after a failed root check.

Property: from any reachable state, after quiescence starts, every device eventually reaches L = S = R for every eligible node, every eligible node on every disk matches the server's live tree (path, kind, content), no parked node remains, and nothing is in flight. Ineligible nodes stay unchanged on disk and are reported. The property promises eventual convergence, not a bound. Tests use a step budget, and exceeding it is reported as a likely livelock.

## 3.5 No resurrection

Property: a node trashed by a delete whose author had seen its latest state stays trashed unless a later op revives it. Every op a device sends derives from one specific user action on that device that has not been propagated yet (an edit, move, rename, create or delete it observed on disk), or from a user's restore request.

The engine sends ops only for L ≠ S, L changes only through observations, and S moves only toward what was sent or what R says. Two paths could make a known file look new, and both are guarded: a crash, handled by effect recovery (§2.8, §5.3), and a lost store, handled as a first link that matches local files against R before anything is uploaded and holds the unmatched ones when the root marker counts effects the store lacks, or when the first listing held a live node and the server counts commits of this device. With nothing live in the first listing, it holds the ones at a path R's trash holds, so a deleted file that the trash no longer holds can come back (§6.4).

## 3.6 Moves

- No duplication: a node is in exactly one place in every tree at every step. A move is one op and one `rename` on disk.
- No cycles: R, S and L are trees at every step. The server rejects a cycle-creating move (R6), the client reverts it (§2.6), and parking (§2.5) avoids transient cycles on disk.
- Last commit wins between concurrent moves of one node (R5).
