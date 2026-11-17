# RFC 0028: Recorded intents

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 R3, R7 and R8, RFC 0004 §5, RFC 0006 §8, RFC 0017 §2 and §3, RFC 0018 §4, §5 and §7, RFC 0019 §4, RFC 0022 §2, RFC 0026 §3, and the sync-engine design §2.8, §4.4 and §6.4. Adds messages and fields to `Op`, `NodeState`, `Delete` and `CreateFile`.

When a server loses commits (a restore within one history, RFC 0013), each device has to work out again which changes its own user made that the server no longer holds, which values it holds that another user made and the server lost, and which of its deletes are still owed. The protocol carries none of these facts. A device reads them from its remote, synced and local trees (R, S and L, design §2.2) and from side tables keyed by seq. Seqs are reused after a loss, and S is a snapshot of a server state that no longer exists, so each rule guesses, and each fix to a guess has broken another one.

This RFC gives every user change an identity, the **intent**, which the change keeps on every send, by its author or by any other device. The server records, per node and field, the intent that set the field. A device records each change its user makes when the change happens, with what the user saw, and each delete with the values its user held. After a loss, a device compares intent ids and needs no seq bound.

A bare section number such as §2 refers to this RFC.

## 1. Terms

- A node has three **fields**: its **place** (parent, name and RFC 0017 mark), its **content** (the current version, files only) and its **status** (live or trashed).
- An **intent** is one change of one user to one node: a create, an edit, a move or rename, a delete, or a version restore. Its **id** is a 16-byte UUIDv7 that the author's device mints when its user makes the change. Its **author** is that device.
- A send of an intent is a **replay** when the intent was in a commit whose result the server has lost (§5.4). The author can replay it, and so can another device that holds the value, which **carries** it (RFC 0018's resend).
- A field's value is **carried** on the server when it arrived in a replay that §3.2 counts as carried.

## 2. Messages

New fields only. No field is removed or renumbered.

```proto
message Intent {
  bytes id = 1;
  string author_device_id = 2;
  bool replay = 3;
  bytes over = 4;
}

message FieldIntents {
  Intent place = 1;
  Intent content = 2;
  Intent status = 3;
}

message Op {
  // oneof op = 1 to 6, as before
  Intent intent = 7;
  FieldIntents fields = 8;
}

message Delete {
  string node_id = 1;
  NodeRevisions requires = 2;
  repeated bytes unseen = 3;
  repeated bytes held = 4;
}

message CreateFile {
  // fields 1 to 6 as before
  string pending_for = 7;
}

message NodeState {
  // fields 1 to 13 as before
  FieldIntents intents = 14;
}
```

- `Op.intent` names the intent an op sends. `over` is the intent the sender expects a node's place or status to hold: such an op applies only while the field holds it (§3.1). Content ops leave it empty. `Op.fields` is set only on `CreateDir` and `CreateFile` and names a separate intent for each field, for a create whose fields different users set (§5.4). A field it leaves empty takes `Op.intent`.
- On a `NodeState`, `intents` names the intent that set each field, and `replay` on each one means the value is carried.
- `Delete.unseen` and `Delete.held` describe what the deleting user saw (§3.3, §5.3).
- `CreateFile.pending_for` is RFC 0017's mark, as on `CreateDir`.

The feature `field_intents` (RFC 0022 §2) announces this RFC.

## 3. Server

### 3.1 Recording intents

- An applied op sets each field it changes to the op's intent for that field. A change of a node's mark is a change of its place.
- An op without an intent, from a client before this RFC, gets one the server mints: a new UUIDv7, the committing device as author, and `replay` false.
- An op whose intent and value equal what the field holds changes nothing and is answered `APPLIED`. Under rule B (§4), such an op from the intent's author claims a carried value: the server clears the carried flag. Among the checks an op meets, this one runs first, ahead of the rest of this section and §3.4 to §3.8. A retry of a stored `commit_id` gets its stored result before any op check (RFC 0006 §8).
- A `Move` or `Delete` with `over` is `SUPERSEDED` and changes nothing unless the node's place or status holds `over`, with one exception: a `Delete` whose node is trashed, and whose `over` is the status intent that the current trash replaced, adds its `held` to the trash (§3.3) and is `APPLIED` without other change. Place and status replays carry `over` (§5.4), and so does an owner's finishing move (§6.1), which sends the intent that set the mark with `over` set to that same intent, so a user's rename or mark clear in between supersedes it. A content op has no `over`: R3 and RFC 0026 §3 decide it from `made_over`.
- An op that RFC 0016 makes a no-op because the content is equal keeps the field's intent, and the result reports the `NodeState`.
- A conflict copy (R3) gets the op's content intent and a place and status intent that the server mints. A merge (R2) keeps the target's intents.
- A mark whose author is revoked (RFC 0009), at the time the op applies, is stored as no mark. When the server revokes a device, it clears every mark that device set.
- Each change of a field's intent or carried flag moves a revision that RFC 0004 §5 compares. A place change moves `meta_rev`. A claim under rule B (§4) moves the edit revisions (RFC 0018 §3, RFC 0019 §3).
- An upgrading server gives every existing field a legacy intent (§7.2).

### 3.2 Carried values

A replay is carried when its author is another device. Under rule A (§4), the author's own replay is carried too. A carried value never brings a node out of the trash, and it never keeps a node that a delete trashes, except as §3.3 says.

### 3.3 Deletes

A `Delete` with `unseen` or `held` set states what its user saw. `unseen` lists the intents its device held in R, on the target and below it, whose values L never held. `held` lists the intents L held there whose values R lacked when the delete was built: values a loss took.

The server takes the delete's **view** of each node in the subtree to be the intents in `held`, plus the node's field intents that were set at or before `base_seq` and are not in `unseen`. R7 keeps a node of the subtree, and the folders between it and the deleted node, when a field holds an intent that is not carried and not in the view, and that is either in `unseen` or set by another device after `base_seq`. A carried content intent that is not in the view lands as a conflict copy beside the node (§3.5), and the node is trashed. A carried place intent is passed over.

The trash keeps each trashed node's field intents and the status intent it replaced, and stores `held` with it. A later delete of the node while it stays in the trash adds its `held` to the stored list. A restore or revival of the node clears the list.

### 3.4 Replays reaching the trash

A carried op whose node, or the parent it moves or creates into, is trashed:

- is `SUPERSEDED` for a field whose intent the trashed node holds or the trash's `held` lists: a deleting user saw that value;
- otherwise, its place and status are `SUPERSEDED`, and its content, for an `UpdateContent` or `CreateFile`, lands as a conflict copy (§3.5).

An op that is not carried follows R8, except that an author's replay under rule B is `SUPERSEDED` when its intent is one the trashed node holds or the trash's `held` lists.

§3.3 to §3.6 read each field's own intent: on a create, the entry of `Op.fields` for that field, else `Op.intent`.

### 3.5 Conflict copies from carried content

The server makes the copy as R3 does: a new node with a conflict name beside the node, holding the op's content, with the op's content intent. When the node's parent is trashed, the copy goes under the nearest live ancestor of the place the node last held.

### 3.6 Replays the server already holds

The server keeps, per space, an index from each intent that some field holds to the nodes and fields that hold it. A replay whose intent the index holds on another node, as on a conflict copy, is `SUPERSEDED`. The index rolls back with the rest of the space when the server is restored.

### 3.7 Commit digest

The request digest (RFC 0006 §8) becomes digest v5: v4's projection (RFC 0026 §5) plus `Op` fields 7 and 8, `Delete` fields 3 and 4 and `CreateFile` field 7. Stored v1 to v4 rows are checked with their own projection. Empty fields encode no bytes, so the v5 digest of a request without them equals its v4 digest. If another RFC takes v5 first, this one takes the next version.

### 3.8 Content conflicts by ancestry

For an `UpdateContent` with an intent, R3 compares versions, not seqs: the op lands as a conflict copy when its expanded `made_over` (RFC 0026 §3) lacks the file's current version, whatever its `base_seq`. A device that listed a version does not thereby overwrite it; only a user's edit made over that version does. An op without an intent, or with an empty `made_over`, keeps R3's seq test.

RFC 0026 §3's checks for a resend apply to every replay, after §3.1's same-intent check. A replay whose `source_version_id` is in the target's current chain, and whose intent the field does not hold, is `SUPERSEDED`.

## 4. A lost change against a later delete

The rule decides whether the author's own replay is carried (§3.2). It settles GUN-767: a change that the server lost, sent again by its author after another device deleted the node.

**Rule A (recommended).** The author's replay is carried. A lost change never undoes a later delete. A lost rename of a deleted node is dropped. A lost edit comes back as a conflict copy beside where the file was, unless the deleting user held it. The deleting user sees the delete done, and no write is lost.

**Rule B.** The author's replay is a user change, as before the loss. It brings the deleted node back (R8), and a delete that did not see it keeps the node (R7), unless the deleting user held that value. An author's send of an intent that the field holds as carried claims it (§3.1). A claim moves the edit revisions but does not make the value unseen to a delete whose view holds it. The deleting user sees the deleted file come back with the lost change.

Both rules keep every write. Under rule A, the attribution model's `an_authors_lost_move_beats_a_delete_that_did_not_see_it` inverts.

## 5. Clients

### 5.1 The journal

A device keeps a **journal** with one row per node and field whose value in L is not yet known to be held by the server in its current form, or that a loss could take back.

| Column | Meaning |
|---|---|
| node, field | The field. |
| intent, author | The intent of L's value. The author is this device for an **own** row, and another device for a **carried** row (§5.4). |
| base | The intent the field held when the user made the change: what the user saw. |
| wire base | What the op sends as its base: RFC 0026's `made_over` and `source_version_id` for content, kept from the content's provenance; the place the user moved the node from. |
| state | `local` (in no commit), `sent` (in the commit in flight, with its slot) or `held` (a result showed the server holding it). |
| held_as | The intent the server's `NodeState` reported for the field in the result. It differs from the row's intent when RFC 0016 made the op a no-op. |
| replay | The row's intent was in a commit the server lost. |

The device writes an own row in the same transaction as the scan that finds the change (design §5.1). A later change of the same field replaces the row with a new intent:

- The new row keeps the old row's base while the old row is `local`. Once the old row was sent, the new row's base is the old row's `held_as` when a result reported one, else the old intent.
- The commit in flight keeps every intent it sends with its value and base. A row whose value returns to the value of an intent still in flight becomes that intent again, in state `sent`.
- A row whose value equals its base's value, while no intent of the field is in flight, sends nothing while R holds the base. It ends when R takes a value that is not carried, such as another user's delete (GUN-541). While R holds a carried value, the row is sent, so a user's edit back to older bytes beats a carried replay that arrives later.

A delete is a status row while L lacks the node, and it keeps the **view**: every intent that L held for the node and the nodes below it, at the time of the delete and before, up to 64 per node and field.

The device also keeps, per node and field, the ids of the newest 64 intents that R held, in order, each marked when L held its value too: the **history**. It gives a delete its view. When a result makes a row `held`, the device adds its `held_as` to the history, since the server held that value. A row's own intent need not be in the history: §5.4 orders R's intents against the row's base.

A row ends when:
- for a `held` row, R holds an intent for the field that the history places after the row's `held_as`. A record that reaches R while the row is `local` or `sent` does not end it: the row's op is still sent, and the server decides between the two (R3, or the later place);
- the row's op is answered `SUPERSEDED`;
- the user changes the field again, which replaces the row;
- for a status row, R purges the node, the user creates or restores it again, or R has lacked the node for 30 days.

A row in state `held`, own or carried, stays until one of these, because a later restore can take back any commit.

### 5.2 Sending and acknowledging

- Every op names the intent of the row it sends. A create names one intent per field when the rows have different intents.
- A result that applies an op makes each row it proves `held`, with `held_as` from the result's `NodeState`. A result proves only the fields its op set. A row whose base is the intent of a row that the result made `held` takes that row's `held_as` as its base.
- A row whose op was answered `SUPERSEDED` ends. The change that superseded it is in R and reaches L as a remote change.
- An owner's finishing move (§6.1) sends the intent of the place row that set the mark, with `over` set to it.

### 5.3 Deletes

A status row is owed while R holds its node live. The commit builder sends the `Delete` with:

- `unseen`: the intents of R's fields, on the node and below it, that the view does not hold;
- `held`: the intents in the view that R's fields there do not hold.

The server then applies §3.3 and §3.4. A delete with no loss since its user acted lists only the values that reached R but not L, so it also stops R7 from treating a record that arrived before the build as seen. When R holds the node trashed and the view holds intents that the trashed node's fields do not, the device sends the `Delete` once more, with `over` set to the row's base and `held` set to those intents, so the trash adds them (§3.1). Otherwise, or after that op's result, the row has nothing to send. When R lacks the node, the row waits, so a carried create of the node is deleted again (GUN-862, GUN-868).

### 5.4 After a loss

The device detects a loss as before: `AHEAD_OF_SERVER`, or a `Welcome` whose device counter or slot token shows that the server lacks a commit of this device (design §2.8, GUN-1004). It sets `replay` on each row whose state is `sent` or `held`, and keeps the commit in flight's intents with their values. A `local` row stays as it is. Where the cases below compare O with a row's base, and that base is an own intent the history lacks, they compare O with that intent's base instead, as the commit in flight kept it, until they reach an intent the history holds. These steps are the row's **own chain**: changes of this device's user that the new R lacks. The device keeps the old R until the listing of the new R is complete (design §4.4 Bootstrap). Then, for each node and field, with O the new R's intent:

0. **A change not yet sent.** A `local` row is sent as a fresh user op, with its intent and without `replay`. Its wire base is the row's own, except that a content row whose own chain ends at O takes O's version as its wire base, since this device's user made every change between O and L. When the chain ends elsewhere, the original wire base stays, and R3 keeps O beside the row's value. S takes the new R's value for the field.
1. **The server holds the row's intent.** O is the row's intent or its `held_as`. When the new R holds the row's value, for a marked place the owed name, the row becomes `held`. When the new R holds the stand-in, the row stays `sent` with `replay` and the owner sends the finish again (§6.1). S takes the new R's value. Under rule B, an author's row whose O is carried is sent once more as a claim.
2. **A loss took this device's change.** A row with `replay`, own or carried, and O is the row's base, the history places O before the base, or the new R lacks the node. S takes the new R's value, and the row is sent with `replay`, its intent and its wire base. A place or status row also sends `over` set to O, or no `over` when the new R lacks the node. A replayed move's `from` (RFC 0019 §4) is the new R's place, not the row's wire base.
3. **Another user changed the field after what this device's user saw.** An own row with `replay`, and O is not in the history, or the history places it after the row's base. Under rule A, a place row ends; a status row whose node R holds trashed sends its view as §5.3 says and then ends, and any other status row ends; a content row is sent with its wire base, whose `made_over` lacks O's version, so R3 keeps both values (§3.8). Under rule B, the row is sent as in case 2. A carried row ends.
4. **No row, and L holds what S holds.** When the new R holds S's value too, S takes the new R's intent. Otherwise X is the old R's intent for the field. When the history places O before X, or the new R lacks the node, and the new R holds X on no node, the device writes a carried row for X with X's author, `replay`, and X's predecessor in the history as its base, and sends it as in case 2. For content it does so only when L holds X's bytes. The row is sent as any row, and S takes the new R's value.
5. **Anything else.** R's value reaches L as a remote change.

A node that the new R lacks is created again by its rows, or by carried rows, in one create that names each field's intent. Its parents come first, by the same cases. A server-made conflict copy whose content intent the new R holds on another node is not created again (GUN-850). A node that the new R lacks and that no row covers is a value nobody can show the server lost: S forgets it and it is not sent.

These cases replace `forget_lost` and the rules of design §4.4 "Commits the server lost": the bound, the slot seqs, `resends`, `lost_creates`, `lost_positions`, `unsent_edits`, the kept and forgotten shapes, and the resend-yield rules.

### 5.5 History changes and store rollbacks

Recovery after a history change or a store rollback (design §6.4) counts a node as changed on this device when a `local` or `sent` row covers it, or when L differs from the recovery's baseline. A `held` row does not count. The questions and answers of §6.4 are unchanged.

## 6. Pending names

### 6.1 Owner only

A pending name is owed by one device, the **owner**: the author of the intent that set the mark. The server records it in `pending_by` (RFC 0017 §2) from that intent's author, where it records the committing device today. A carried replay of a create that sets a mark keeps the original owner, unless the owner is revoked (§3.1).

Only the owner finishes the rename. This replaces RFC 0017 §3's rule that any device may finish it when no other node holds the name. Another device never sends a move to the name a mark holds, and its effects leave the node at the stand-in name until the owner's finish arrives in R.

The finish completes the owner's intent that set the mark. The place row's value is the owed name, and the stand-in is a step on the way: the row is `held` only when R holds its intent at the owed name. The finish sends that intent with `over` set to it (§3.1), so a user's rename or mark clear that lands first supersedes it. An owner whose store lost the row finds itself in `pending_by` and sends the finish with the intent R holds for the place, which it authored.

### 6.2 The mark on every create

`CreateFile.pending_for` carries a file's mark, as `CreateDir.pending_for` carries a folder's. A create that a replay sends for a marked node carries the mark (GUN-753).

A user's move of a marked node clears the mark and sets a new place intent, also when the move ends at the node's place (GUN-639). The journal keeps such a place row even though its parent and name equal its base's, since the mark differs.

## 7. Migrations

### 7.1 Client store

One migration adds:

- `journal`, keyed by node and field, with the columns of §5.1;
- `delete_views(node_id, view_node_id, field, intent)` for status rows;
- `intent_history(node_id, field, ord, intent, in_local)`;
- `remote_nodes.intents` and `synced_nodes.intents`: the node's three intents.

It fills them in this order, and a later step does not overwrite a row an earlier one wrote:

1. `resends`: a carried row for the field L differs in, with the legacy intent (§7.2) of the version L holds and an empty author, which names no device.
2. The commit in flight, in state `sent`. A carried row with an empty author for each `UpdateContent` with `resend`, each `CreateFile` with `source_version_id`, each `Move` with `from` that is not a pending finish, and each `CreateDir` whose node neither `local_changes` nor `lost_creates` holds. An own row for every other op. The op's provenance decides the author, never the device that sent it; a value whose author the store can't show is carried, which can cost a conflict copy but never a write. Then `unsent_edits` and `lost_creates`: own `local` rows. Each gets `replay` when the store records a loss.
3. Every other field where L differs from S, and every node L lacks and S holds: an own `local` row with a new intent, whose base is the field's legacy intent.
4. `edited_here`, `moved_here` and `trashed_here`: own `held` rows whose intent and `held_as` are the field's legacy intent.
5. `remote_nodes.intents`, `synced_nodes.intents` and `intent_history`: legacy intents, one per field, plus one for each `replaced_versions` and `past_positions` entry. A status row's view takes the history entries L held.

A store with a loss recorded but not yet listed keeps the record, and the bootstrap that follows recovers with these rows.

A later migration, in the change that switches recovery to §5.4, drops `resends`, `lost_creates`, `lost_positions`, `lost_shapes`, `forgotten_shapes`, `dropped_edits`, `unsent_edits`, `local_changes`, `edited_here`, `moved_here`, `past_positions`, `trashed_here`, `replaced_versions`, `unseen_changes`, `removed_here` and `waiting_updates.slot`. `commit_slots` stays for the device counter and its tokens (GUN-1004), and `kept_originals` stays for RFC 0015.

### 7.2 Legacy intents

A field set before either side knew intents gets a legacy intent: an empty author, `replay` false, and an id that the client and the server compute alike:

```
id = UUIDv5(701471d0-f5ea-47eb-936a-485a91d0891e, node_id || 0x00 || field || 0x00 || key)
```

The key is the version ID for content, `meta_rev` with the parent and name for place, and `trashed:` with the trash group, or `live`, for status. A server migration gives every existing node these ids. A client computes the same ids for a field that a server without `field_intents` sends with no intent.

## 8. Mixed versions

- **Old client, new server.** The client sends no intents and no delete view. The server mints intents (§3.1), applies R3 and R7 by seqs and revisions as before, and keeps no `held` on the trash. The old client's loss rules keep running, with their known faults.
- **New client, old server.** The server sends no intents. The client keeps its journal and computes legacy intents for R. It does not recover from a loss automatically: it halts and asks, as design §6.4 does for a store rollback, because without server intents case 4 of §5.4 can't tell a lost value from a later one. It sets `UpdateContent.resend` and `Move.from` on carried replays, so an old server applies RFC 0018 and RFC 0019 to them.
- **Mixed devices on a new server.** A field an old client sets has a server-minted intent, which new devices compare like any other.

## 9. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` gains:

- an op without an intent gets a minted one, and `NodeState` reports all three;
- an op whose intent and value the field holds is `APPLIED` and changes nothing;
- a create with `fields` records a different intent per field;
- a carried `UpdateContent` on a trashed file makes a conflict copy, and one whose intent is in the trash's `held` is `SUPERSEDED`;
- an author's replayed `Move` on a trashed node: `SUPERSEDED` under rule A, the node revived under rule B;
- a `Delete` listing an unseen user intent keeps the node; one listing an unseen carried content intent trashes the node and makes a conflict copy;
- a replay of an intent the index holds on another node is `SUPERSEDED`;
- a carried `CreateDir` with a mark keeps its author as `pending_by`, and stores no mark when that author is revoked;
- a `Move` without `pending_for` to a marked node's own place clears the mark and sets a new place intent;
- an owner's finish with the marking intent and `over` applies, and is `SUPERSEDED` after a user's same-place mark clear;
- a `Move` or `Delete` whose `over` the field does not hold is `SUPERSEDED`; a `Delete` whose `over` is the status the trash replaced merges its `held` and is `APPLIED`;
- an `UpdateContent` with an intent whose `made_over` lacks the current version makes a conflict copy, even when its `base_seq` covers that version;
- a `Delete` sent to a node another delete trashed, with `over` set to the status that trash replaced, adds its `held` to the trash;
- a second delete of a trashed node adds its `held` to the first one's, and a carried replay that either lists is `SUPERSEDED`;
- a carried `CreateFile` whose place intent the trash's `held` lists and whose content intent it does not makes a conflict copy;
- an author's claim of a carried value whose `source_version_id` is in the chain clears the carried flag;
- digest v5 refuses a reused `commit_id` that differs only in an intent.

## 10. Cost

- Each op grows by about 60 bytes, and a create with `fields` by up to 180.
- Each `NodeState` grows by up to 180 bytes, so a listing of 100,000 nodes grows by about 18 MB before compression.
- A replayed `Delete` grows by 16 bytes per intent in `unseen` and `held`: the values a loss took or that reached R but not L.
- The server stores three intents per node, an index row per current intent, a carried flag per field and the `held` list on trashed nodes.
- The client stores up to three journal rows per changed node, up to 64 history entries per node and field, and a view per pending delete.

## 11. Not covered

- **History beyond 64 intents.** A restore that brings back a value older than a device's history makes case 5 of §5.4 apply where case 4 should. The device downloads the older value, and the author still replays its change. A delete counts such a value as unseen and keeps the node. No write is lost.
- **A carried create after 30 days.** A status row whose node R lacks ends after 30 days. A carried create that reaches the server later brings the node back.
- **The owner never comes back.** A mark whose owner never commits again and is not revoked keeps its stand-in name.

## Appendix A. Open issues and the rule that fixes each

Rows marked † are cc families without a hand schedule. PR 1 of the delivery plan records them as known reds, and the recovery change must turn them green.

| Issue | What the engine guesses today | Rule |
|---|---|---|
| GUN-692 (+917, 983), GUN-761† | Who authored a resent create after a second loss | §5.4 case 4 writes a carried row with the author; §5.3 deletes the node again |
| GUN-694 | Whether S's position is this device's move | §5.4 case 4: a carried move with the author's intent. The sim check becomes "every op's intent was made by its author's user" |
| GUN-549 (+673) | Whether a create is a stale resend or a new rename | §5.4 case 2 replays the later rename over the carried create, guarded by the new R's place |
| GUN-580 | Whether R's position is a user's move or lost history | §5.3: the lost rename is in the deleter's view as `held`; §3.4 |
| GUN-581 | Whether a listed version is a resend | §2 `intents` on listings; §3.3 and §3.4 with the view |
| GUN-885 | Whether a listed meta revision is a user's | §5.3: the rename reached R but not L, so `unseen` lists it and R7 keeps the node; a carried move built before the rename has `over` and is superseded (§3.1) |
| GUN-862, GUN-868 | Whether a resent create is a user's move; whether a delete is still owed | §5.3 owed status row; §3.3 |
| GUN-589 (+869) | Whether a delete is still owed after S forgets the node | §5.1 status row with its view; §3.4 supersedes a replay the view held |
| GUN-665 | What an edit was made over once R trashed the file | §5.1 row with its wire base; §5.4 case 2 |
| GUN-689 | Whether this device made the no-op edit | §5.4 case 2 orders R's intent against the row's base, so the row is replayed whether or not its intent ever reached R |
| GUN-698 | An untouched download or a revert over an in-flight edit | §5.1 replacement rules; §5.4 cases 0 and 2 |
| GUN-866 | Whether R's content is the edit's base | §5.4 case 2 with `made_over`; an `ID_EXISTS` create is planned again per field |
| GUN-716 | Whether a holder's untaken value is its own write | §5.4 case 4 carries content only with the bytes of the old R's value |
| GUN-850 (+919) | Whether a lost conflict copy carries a lost write | §5.4: a copy whose content intent the new R holds is not created again; §3.6 |
| GUN-838 (+927), GUN-551† | Own create or another device's resend, for yielding | §5.4 cases 2 and 4 with per-field create intents; §3.4 |
| GUN-767 | A lost move against a later delete | §4, rule A or B |
| GUN-541 | Whether an unsent-edit mark is still a user change | §5.1 a row equal to its base ends when R takes a user value |
| GUN-831† (double-file, Rewind-ending, commit-took-write) | Lost create or resend; whether a restored value is older | §5.4 compares intents; §3.8 supersedes a replay the current chain includes |
| GUN-831† (a rename hides an unchanged value: 33f2d2f8, 48662363, cab9d80e, e93d66ee, f1141343, f850b1d4, 8a207106, 944abfd1) | Whether a renamed file's content changed here, compared by whole shape | §5.1 rows are per field: a rename writes a place row only |
| GUN-1006 (d2fd3d71) | Whether a bystander's resent create is its own lost create after a second restore | §5.4 case 4 writes a carried row with the other author; case 2 replays it after the second loss, and §3.3 lets the delete pass. The intent's author on the wire is the `from` fact, and it replaces the held `resent_creates` table |
| GUN-1013 | Whether a delete is owed once another device's rename changed the shape | §5.1 status row with its view; §5.3 |
| #642's constraints: a delete or edit over a value a restore brought back; 569589931b73; e86d658f | Whether the restored value is older than the deleted or edited one | §5.3 the restored value is in the view; §5.1 an edit back to older bytes beats a later carried replay; §5.4 case 4 needs the old R's bytes |
| GUN-690, GUN-701 | Whether a listed revision is this device's commit | §2 `intents` on listings; §5.2 |
| GUN-931 | Whether a rekeyed folder's children changed here | §5.5 only `local` and `sent` rows count |
| GUN-753 | (no field) | §6.2 `CreateFile.pending_for` |
| GUN-639 | Stand-in name or the user's own | §6.2 |
| GUN-755, 777, 808 (+926), 848's three non-owner cases | Who owes the pending name | §6.1 owner only |

Point fixes outside this RFC: GUN-539 and GUN-920 (a restore that brings back commits this device dropped; GUN-1004 detects it, and the counter rule needs its own fix), GUN-735, GUN-748, GUN-452, GUN-658, GUN-809 (which local node owns a disk path) and GUN-823 (a finish before the name's holder moves away, which #508's name waits order).

## Appendix B. Related work

- **RFC 0013 commit tokens and GUN-1004** stay. The slot token in `Welcome` detects a loss. This RFC removes the bound (`held_through`) that recovery read from the slots.
- **RFC 0018 and RFC 0019** stay for clients and servers without `field_intents`.
- **RFC 0026 edit ancestry** stays: §3.8 makes `made_over` the content base that R3 reads, and extends its resend checks to every replay.
- **#626** (a second restore before listing counts as a loss) is loss detection and stays.
- **#629's** finishing-move check in the sim, which accepts a finish from any device after a revival, becomes owner-only (§6.1). Its other two checks stay.
- **#508** (commit planner placement) does not read the loss tables. Recovery hands it one value per node and field, as L and S do today, and `resends()`, `unsent_edits()` and `unsent_moves` become reads of the journal.
- **#642** (parked) and the lost-create fix for GUN-831 patch the current rules. #642's tests are constraints on this RFC (Appendix A).
- **#616** (attribution model, test-only) keeps its field and author terms. Its per-field states (agreed, own, resent) are this RFC's rows, its delete view is §5.3, and its trash view is §3.3's `held`. Intent ids replace its fork-point request and its bound-only fallback, because no seq is compared after a loss.
