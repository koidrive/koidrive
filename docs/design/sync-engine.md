# Sync engine design

The sync engine (`core/engine`) decides, for one space on one device, what to download, what to upload and what to ask the person. It is a pure state machine: `handle(now_ms, input)` takes an observation or a server message and queues outputs, and the driver stores the new state in one transaction before running them. It does no I/O, so the same code runs on a disk and in the simulator.

This page explains why. The numbered sections that code and RFCs cite are in the [sync engine reference](sync-engine/README.md): [1 interface](sync-engine/context.md), [2 state model](sync-engine/state-model.md), [3 correctness properties](sync-engine/correctness.md), [4 server order](sync-engine/server-order.md), [5 local file system](sync-engine/filesystem.md), [6 mass-deletion safety](sync-engine/mass-deletion.md), [7 testing](sync-engine/testing.md), [8 server guarantees](sync-engine/server-guarantees.md), [9 other crates](sync-engine/interfaces.md) and [10 protocol RFCs](sync-engine/protocol-rfcs.md).

## Three trees tell the engine which side changed

The engine keeps three trees keyed by node ID. R is the server state this device has applied, L is the disk as last observed, and S is the shape on which both last agreed. A node whose L differs from S changed here, one whose R differs from S changed elsewhere, and one that differs on both sides is a conflict ([§2.3](sync-engine/state-model.md#23-classification-decides-the-direction-of-each-change)).

The engine stores observations, not a queue of work. Dropbox found that a work queue drifts from the disk after a lost event or a crash, and that a move queued as a delete plus an add loses the file when only the delete runs ([Jayakar 2020](https://dropbox.tech/infrastructure/rewriting-the-heart-of-our-sync-engine)). With three trees a restart needs only a rescan, and sync is done when all three agree. Node IDs instead of paths make a rename one move on every device.

## The server's commit order decides conflicts

Each space's Durable Object orders every commit, so moves need no CRDT. The server rejects a move that would close a cycle, and the later commit wins between two moves ([Kleppmann et al. 2021](https://martin.kleppmann.com/papers/move-op.pdf), §5.2).

A device sends one commit at a time, built against the last server state it applied. Before sending, it resolves the conflicts it has seen with the rules the server would apply, and the server resolves the ones it had not seen. Both sides survive: a concurrent edit becomes a conflict copy, and an edit beats a delete ([§2.4](sync-engine/state-model.md#24-the-client-rebases-the-conflicts-it-has-seen)). Koidrive runs unattended, so it keeps both sides and states its no-data-loss property per write ([§3.3](sync-engine/correctness.md#33-no-write-is-lost)).

## Effects never destroy bytes a user wrote

A disk change never overwrites or unlinks an entry. The driver renames the entry it replaces or removes into `.koidrive/quarantine/`, compares it with what the engine expected, and moves it to the local trash only if it is the synced content the server also holds. Anything else goes back to the user. Renames never replace their target, so a write that races an effect ends in place, in quarantine or as a recovered file ([§5.2](sync-engine/filesystem.md#52-effects-never-destroy-user-bytes)). A process that keeps a file open across a sync change writes into the trashed copy, which the driver reclaims after up to seven days. A write that lands between the reclaim's check and its unlink is lost.

## A crash leaves state that explains itself

The engine records each effect, with every path it touches, before the disk changes, and each commit before sending it. After a crash the driver finishes or undoes the recorded effect, then rescans. The engine resends a commit with an unknown outcome under the same ID, and the server returns its stored result. The engine never infers an outcome from the current tree, because a wrong guess can revive a file another device deleted ([§4.2](sync-engine/server-order.md#42-errors-and-retries)).

The server counts each device's accepted commits and the root marker counts disk effects. When the space was rewound, or a counter shows the store came from a backup, the folder stops and keeps its trees as evidence. It neither uploads stale files nor resends a commit ([§2.8](sync-engine/state-model.md#28-persistence-and-crash-recovery)). The engine then recovers as [§6.4](sync-engine/mass-deletion.md#64-recovery-after-a-rewind-or-a-rolled-back-store) says. A folder with a store classifies every node by whether this device changed it since its last sync, applies the certain cases, and asks one question per group about the rest. A first link has no store to compare with, so it matches the folder's entries against the space's tree by path, kind and content, and adopts the ones that match. It asks one question about the rest when the folder synced the space before, and otherwise only about entries at a name the space's trash holds, held together with any folder above them that matches no folder of the space and everything in it that the rules don't exclude.

## A large delete waits for confirmation

The engine holds a local deletion, and asks the person before it leaves the device, when that deletion makes the last ten minutes pass 1,000 deletions or delete more than half of the files in a space of at least 20. A deletion is one topmost deleted node, so a deleted folder counts once, and deletions sent earlier in the window stay sent. The count is stored, so a restart does not reset it, and an answer covers exactly the files it listed ([§6.1](sync-engine/mass-deletion.md#61-a-large-delete-waits-for-confirmation)). A root that disappears, loses its `.koidrive/space.json` marker or sits on another volume pauses the space instead of reading as "everything was deleted" ([§6.2](sync-engine/mass-deletion.md#62-a-missing-root-pauses-the-space)). Deletions from other devices are applied and counted the same way, and their files stay in both trashes ([§6.3](sync-engine/mass-deletion.md#63-restoring-after-a-bad-delete)).

## Tests check properties, not code paths

A deterministic simulator in `core/sim` runs several devices against a reference server through lost messages, crashes inside effects, server losses and rewinds, and checks from its own records that no write is lost and that devices converge ([§7](sync-engine/testing.md)). [Testing](../testing.md) explains how to run them.
