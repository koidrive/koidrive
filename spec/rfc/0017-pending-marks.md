# RFC 0017: Pending marks

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 and the sync-engine design §2.6 and §6.4.

A device that swaps names, or creates a folder where a node that is moving away still is, sends one node under a pending name (`<name> (pending)`) and owes the rename to its own name in a later commit (sync-engine design §2.6). Only that device's store records the debt. When the record is lost, the server keeps the pending name for good. That happens when the store is rolled back, when the space is rewound to a copy taken between the two commits, and when the device is unlinked or never starts again (GUN-547).

This RFC puts the debt on the server node. The node carries the name it is pending for, so any device that sees the node can finish the rename. A bare section number such as §2 refers to this RFC.

## 1. Messages

`CreateDir.pending_for` (field 4), `Move.pending_for` (field 4) and `NodeState.pending_for` (field 10) are strings. The empty string means no mark. `NodeState.pending_by` (field 11) is the ID of the device whose commit set the mark, and empty when there is no mark. All four fields are new and reserve nothing.

## 2. Server

- An applied `CreateDir` sets the node's mark to the op's `pending_for`. So does an applied `Move` that changes the node's place or brings the node back from the trash, and such a `Move` without a mark clears the mark. A `Move` to the place the node already holds changes nothing, its mark included.
- A mark that is not a valid name (spec v0 §3) makes the op fail with `INVALID_NAME`.
- A mark whose `name_key` equals that of the name the node ends up with is stored as no mark. This also covers a name the server gives under R1.
- A node in the trash keeps its mark. A `Restore` that gives the node a name or a folder clears the mark; one that gives neither keeps the mark and its setter, since the node comes back at the pending name it left with.
- The server records the device that committed the op setting a mark as the mark's setter, and clears the setter whenever it clears the mark or stores no mark.
- Every `NodeState` of the node carries its mark in `pending_for` and its setter in `pending_by`.

A mark is not a name: it holds no name in its folder, and the server never renames a node by it.

## 3. Clients

A client sends a mark only with the ops of §2.6 of the design that use a pending name: the move of a name cycle to its pending name, and a folder created under a pending name. The mark is the name the node takes later.

A client finishes the rename of a node that R marks with N when all of these hold:

- L, S and R hold the node at the same position;
- the node is not excluded or held by this device's rules, not parked, and its S update does not wait;
- no other node of L or R in the same folder holds N's `name_key`, or R's `pending_by` names this device;
- this device's rules do not leave N out.

A client keeps each rename it is finishing as a pending finish: the node, its source (the folder and the pending name), its target (the same folder and N), its origin (a mark in R at the `meta_rev` R held for the node when the finish started, or a record from before marks, §4), whether L has held the node at the target since the finish started, and the head of a `HEAD_MOVED` refusal it waits past. A finish holds while S and R hold the node at the source, L holds it at the source or the target, and, for a mark, R holds the node at that `meta_rev`. Another user's rename away and back therefore ends the finish, even when R shows both in one download. Positions compare by folder and name. A tree that does not hold the node yet, as for a create still in flight, does not end the finish. Any other position ends it, except while the commit that carries the finishing move is in flight: its result decides.

When no other node holds N, the client takes the name on its disk first. Its effects move the local entry from the source to the target, which starts the finish, and S stays at the source. An effect that fails because the user already made that move starts the finish the same way. The client then sends the rename as a plain `Move` to N. The setter's own finish starts at the result's seq when the result of its create or move under the pending name applies, whether or not R holds the node yet. Another user's delete or rename that lands before the finishing move therefore ends the finish, and the disk follows R. A setter whose store holds no finish for a mark R gives it starts one when it plans the `Move` to N.

When another node holds N, only the setter finishes the rename. It starts the finish and sends a plain `Move` to N first, in a commit that holds no other op, and the server gives the node another name under R1. Another device waits: the node holding N may be about to move away in a commit of the setter, as in a swap split over two commits. The setter finds itself in `pending_by` after its store is rolled back, so the rename does not depend on that store.

A commit that carries a finishing move requires the head it was built on. When the server answers `HEAD_MOVED`, the finish keeps that head, and the client sends no finishing move for it until it has applied a newer head of the same history. Any other refusal releases the finish, and so does recovery, which starts from a history the kept head does not belong to; the next commit carries the move again. When the result applies the move, the finish ends and S takes the target. A finish whose result applies before L holds the node at the target instead leaves S at the source with the mark until the disk follows R.

When the user moves the node back to the source after L held it at the target, the finish holds. The client still sends the move to N, and after its result the user's move back, so the user's name is the newest change of the node on the server.

When a finish ends because L, S or R left the source in any other way, that change wins. If L holds the node at the target only because this device renamed its disk, the node is reverted: the client sends no move for it, and its effects put it where R holds it. If L holds it anywhere else, the user moved it, and that move is sent as usual. If R holds the node at the target, S takes it there. If S already holds it there, as after a Keep (design §6.4), nothing is reverted.

A device keeps a mark it no longer finds in R. S takes R's mark whenever it takes R's position, and loses it when S takes a position without one. When S drops a node that L still holds where S held it and R does not hold, as when the person keeps a folder that a rewind removed from the server (design §6.4), the mark moves from S to the device's stored engine state. A create of that node carries the mark, so the server records this device as the setter. The mark goes when R holds the node again, or when the user renames or deletes the folder. S keeps no setter: the server records the device that sends the create, which holds the folder on disk and so can finish the rename, while the device that set the mark first may never come back.

Two cases stay open. A user's rename of a marked node away and back that no commit sees in between sends no move, as for any node, so the mark stays and some device finishes the rename. This holds whether the setter is online or not. When the setter never commits again and another node holds N for good, no device finishes the rename, and the node keeps its pending name. Other devices leave it as it is and send no move that would give it a conflict name.

A device that matches its disk against R without a store to compare with (design §6.4) also matches a local folder to a live folder of R under the same parent that R marks with the local folder's name, when R holds nothing under it that the local folder does not hold. It adopts the folder and sends the move to that name.

A node that no mark names keeps its name, whatever the name looks like. A user can name a folder `X (pending)`.

## 4. Compatibility

An older server ignores the new fields. A client then sees no marks and behaves as before: the finish it records for its own marked create or move ends only when R moves the node or changes its `meta_rev`, so R holding the node at the pending name without a mark leaves the finish in place and the client still sends the rename it owes. An older client sends no marks and ignores `NodeState.pending_for` and `NodeState.pending_by`. Its moves carry no mark, so a move it makes of a marked node clears the mark, as any move of the node by its user should.

A client whose store recorded renames it owed before marks existed keeps each as a pending finish of origin legacy (§3). The server never learns of them. A record becomes a finish when R holds its node live and S holds it at the recorded pending name, or when L holds it there and the commit in flight may still create or move it. Any other record is dropped. A legacy finish acts as the setter, also when another node holds the name, and ends by the same rules as any finish.

## 5. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` covers §2 in these scenarios:

- `rfc0017-a-mark-stays-until-a-move-without-one-clears-it`
- `rfc0017-a-mark-with-the-name-key-the-node-takes-is-not-kept`
- `rfc0017-a-mark-no-node-may-take-is-an-invalid-name`
- `rfc0017-a-mark-stays-through-the-trash-and-a-restore-that-gives-no-place`
