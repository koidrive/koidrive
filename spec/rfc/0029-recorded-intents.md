# RFC 0029: Recorded intents

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 R3, R7 and R8, RFC 0004 §3 to §5, RFC 0006 §8, RFC 0017 §2 and §3, RFC 0018 §4, §5 and §7, RFC 0019 §4, RFC 0022 §2, RFC 0026 §3, and the sync-engine design §2.8, §4.4 and §6.4. Adds messages and fields to `Op`, `NodeState`, `NodePurged`, `NodeSnapshot`, `Delete` and `CreateFile`.

When a server loses commits (a restore within one history, RFC 0013), each device has to work out again which changes its own user made that the server no longer holds, which values it holds that another user made and the server lost, and which of its deletes are still owed. The protocol carries none of these facts. A device reads them from its remote, synced and local trees (R, S and L, design §2.2) and from side tables keyed by seq. Seqs are reused after a loss, and S is a snapshot of a server state that no longer exists, so each rule guesses.

This RFC gives every user change an identity, the **intent**, which the change keeps on every send, by its author or by any other device. The server records, per node and field, the intent that set the field. A device records each change its user makes when the change happens, with what the user saw, and each delete with the values its user held. After a loss, a device compares intent ids and needs no seq bound.

A bare section number such as §2 refers to this RFC.

## 1. Terms

- A node has three **fields**: its **place** (parent, name and RFC 0017 mark), its **content** (the current version, files only) and its **status** (live or trashed).
- An **intent** is one change of one user to one node: a create, an edit, a move or rename, a delete, or a version restore. Its **id** is a 16-byte UUIDv7 that the author's device mints when its user makes the change. Its **author** is that device.
- A send of an intent is a **replay** when the intent was in a commit whose result the server has lost (§5.4). The author can replay it, and so can another device that holds the value, which **carries** it (RFC 0018's resend).
- A field's value is **carried** on the server when the op that set it was carried (§3.2). A `NodeState` marks a carried value with `replay` on the field's intent.

## 2. Messages

New fields only. No field is removed or renumbered.

```proto
message Intent {
  bytes id = 1;
  string author_device_id = 2;
  bool replay = 3;
  bytes over = 4;
  int64 applied_at_ms = 5;
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
  repeated bytes trash_held = 15;
}

message NodePurged {
  string node_id = 1;
  string last_parent_id = 2;
  FieldIntents intents = 3;
  repeated bytes held = 4;
}

message NodeSnapshot {
  // fields 1 to 3 as before
  NodePurged tombstone = 4;
}
```

- `Op.intent` names the intent an op sends. `over` is the intent the sender expects a node's place or status to hold: such an op applies only while the field holds it (§3.1). Content ops leave it empty. `Op.fields` is set only on `CreateDir` and `CreateFile` and names a separate intent for each field, for a create whose fields different users set (§5.4). A field it leaves empty takes `Op.intent`.
- On a `NodeState`, `intents` names the intent that set each field, `replay` on each one means the value is carried, and `applied_at_ms` is when the server first applied it (§3.1). A replayed op always sets `applied_at_ms` (§5.4). On a trashed node, `intents.status.over` is the status intent the trash replaced and `trash_held` is the trash's `held` list (§3.3).
- `Delete.unseen` and `Delete.held` describe what the deleting user saw (§3.3, §5.3).
- `CreateFile.pending_for` is RFC 0017's mark, as on `CreateDir`.

The feature `field_intents` (RFC 0022 §2) announces this RFC.

## 3. Server

### 3.1 Recording intents

- An applied op sets each field it changes to the op's intent for that field, with `replay` set when the op is carried (§3.2) and cleared when it is not. A change of a node's mark is a change of its place.
- An op without an intent, from a client before this RFC, gets one the server mints: a new UUIDv7. Its author is the committing device, except that an op with RFC 0018 `resend`, RFC 0019 `from` or a `CreateFile.source_version_id` gets an empty author, so it is carried (§3.2) and its fields store it with `replay` set.
- The server stamps each field's intent with its **applied time**: the `committed_at_ms` (RFC 0013 §2) of the commit that first applied the intent. A replay keeps the `applied_at_ms` it sends, clamped to the server's clock, and is never stamped with its landing time; any other op gets its own commit's time. The server stamps each commit with its clock, and commits only once its clock reads later than its head's stamp, waiting until it does, so no stamp leads the clock that made it. Times therefore also grow across a restore of its storage, which the server doesn't see, as long as the clock reads later after the restore than at the newest commit the restore lost: a commit after the restore happens later than every commit the restore lost. A restore keeps a prefix of the server's commits, so a field intent applied at or before a lost commit's time was set before that commit, and one applied later was set after the restore.
- An op whose intent and value equal what the field holds changes nothing and is answered `APPLIED`, except that a `Delete`'s `held` is added to the trash (§3.3), so an unchanged retry changes nothing. A create takes this path only when every field it sets holds its intent and value; otherwise a live node with its id answers it `REJECTED` `ID_EXISTS`, as today, and §5.2 sends its rows again on that node. Among the checks an op meets, this one runs first, ahead of the rest of this section and §3.4 to §3.8. A retry of a stored `commit_id` gets its stored result before any op check (RFC 0006 §8).
- A `Move` or `Delete` with `over` is `SUPERSEDED` and changes nothing unless the field the op sets (place for a `Move`, status for a `Delete`) holds `over`, with one exception: a `Delete` whose node is trashed, and whose `over` is the status intent that the current trash replaced, adds its `held` to the trash (§3.3) and is `APPLIED` without other change. Place and status replays carry `over` (§5.4), and so does an owner's finishing move (§6.1), which sends the intent that set the mark with `over` set to that same intent, so a user's rename or mark clear in between supersedes it. A content op has no `over`: R3 and RFC 0026 §3 decide it from `made_over`.
- An op that RFC 0016 makes a no-op because the content is equal keeps the field's intent, and the result reports the `NodeState`.
- A conflict copy (R3) gets the op's content intent, and place and status intents that the server mints. When the op is carried, these are carried too, with the content intent's author (§3.5). A merge (R2) keeps the target's intents.
- When the server revokes a device (RFC 0009), it finishes each mark that device owns (§6.1) whose folder holds no live node at the owed name. A mark whose owner is revoked counts as ownerless wherever it is read, including one that a restore or a replay brings back, and RFC 0017 §3 lets any device finish it.
- Each change of a field's intent or carried flag, and each `held` merge (§3.3), writes a change record and moves a revision that RFC 0004 §5 compares: `content_rev` for content, `meta_rev` for place and status.
- An upgrading server gives every existing field a legacy intent (§7.2), and an RFC 0004 §5 rebuild gives one to each record without intents.

### 3.2 Carried values

An op is carried when its author is another device, whether or not it is a replay. An empty author counts as another device. The author's own replay is carried too (§4). A carried value never brings a node out of the trash, and it never keeps a node that a delete trashes, except as §3.3 says.

### 3.3 Deletes

A `Delete` with `unseen` or `held` set states what its user saw. `unseen` lists the intents its device held in R, on the target and below it, whose values L never held. `held` lists the intents L held there whose values R lacked when the delete was built: values a loss took.

The server takes the delete's **view** of each node in the subtree to be the intents in `held`, plus the node's field intents that were set at or before `base_seq` and are not in `unseen`. R7 keeps a node of the subtree, and the folders between it and the deleted node, when a field holds an intent that is not carried and not in the view, and that is either in `unseen` or set by another device after `base_seq`. A carried content intent that is not in the view lands as a conflict copy beside the node (§3.5), and the node is trashed. A node whose place is carried, such as an earlier copy of carried content, keeps no folder by its place: when its content is not in the view, it stays in its folder if R7 keeps that folder and otherwise moves to the folder's nearest live ancestor, and when its content is in the view, it is trashed with the subtree. When that node also holds an unseen intent that is not carried, R7 keeps it and its folders as above. A carried place intent is passed over.

The trash keeps each trashed node's field intents and the status intent it replaced, and stores `held` with it. `NodeState` reports both (§2), so an RFC 0004 §5 rebuild keeps them. A later delete of the node while it stays in the trash adds its `held` to the stored list. A restore or revival of the node clears the list.

When a delete trashes a node, or adds `held` to its trash, the server also trashes, in the same trash group, every conflict copy that §3.5 made from an intent in the delete's view whose fields still hold the intents the copy got. §3.6's index finds them. A carried copy that landed before the deleting device reconnected thus goes once its user's view arrives.

Purging a node keeps a tombstone: its id, its last parent, its field intents and its `held` list. `NodePurged` carries them in the log, and a snapshot page holds one `NodeSnapshot` per tombstone, with `tombstone` set and no `node`, in node id order with the node records, so an RFC 0004 §5 rebuild keeps every tombstone. A server that records intents writes every `koidrive.snapshot` file at `format_version` 2, in its `manifest.json` and in each page's header, so a reader of RFC 0004's version 1 refuses the snapshot instead of reading a record with no node. A version 2 reader accepts snapshots at version 1 and 2, because a bucket keeps the snapshots written before the upgrade. It reads a record with `tombstone` set as a tombstone and is otherwise a version 1 reader. Log segments, `space.json` and the space's own `format_version` stay at 1, since `NodePurged` only gains fields. A replay or a carried op (§3.2) that names a purged node is `SUPERSEDED` for place and status, and so is a create that names it; another op that names it is answered as today. Its content lands as a conflict copy under the nearest live ancestor of the node's last parent, unless the tombstone holds or lists the content intent. A carried create or move into a purged folder is `SUPERSEDED` for place and status when the folder's tombstone holds or lists the op's intent for that field, as §3.4 does for a trashed parent, and its content follows the tombstone rule above. Any other create or move into a purged folder is `APPLIED` under the nearest live ancestor of that folder's last parent, so a user's new file there still uploads. The result's `NodeState` reports that place, and the device's place row becomes `held` with it: L takes the reported place, and the device does not send the purged folder again. A name clash there is answered `APPLIED_RENAMED`, as for any move.

### 3.4 Replays reaching the trash

A carried op whose node, or the parent it moves or creates into, is trashed:

- is `SUPERSEDED` for a field whose intent the trashed node holds or the trash's `held` lists: a deleting user saw that value;
- otherwise, its place and status are `SUPERSEDED`, and its content, for an `UpdateContent` or `CreateFile`, lands as a conflict copy (§3.5).

An op that is not carried follows R8. An old client's `resend` and `from` keep RFC 0018's and RFC 0019's answers on the trash.

§3.3 to §3.6 read each field's own intent: on a create, the entry of `Op.fields` for that field, else `Op.intent`.

### 3.5 Conflict copies from carried content

The server makes the copy as R3 does: a new node with a conflict name beside the node, holding the op's content, with the op's content intent and carried place and status intents with that intent's author (§3.1). When the node's parent is trashed, the copy goes under the nearest live ancestor of the place the node last held.

### 3.6 Replays the server already holds

The server keeps, per space, an index from each intent that some field holds to the nodes and fields that hold it. A replay whose intent the index holds on another node, as on a conflict copy, is `SUPERSEDED`, and its result reports that node's `NodeState`. The index rolls back with the rest of the space when the server is restored.

### 3.7 Commit digest

The request digest (RFC 0006 §8) becomes digest v5: v4's projection (RFC 0026 §5) plus `Op` fields 7 and 8, `Delete` fields 3 and 4 and `CreateFile` field 7. Stored v1 to v4 rows are checked with their own projection. Empty fields encode no bytes, so the v5 digest of a request without them equals its v4 digest. If another RFC takes v5 first, this one takes the next version.

### 3.8 Content conflicts by ancestry

For an `UpdateContent` with an intent, R3 compares versions, not seqs: the op lands as a conflict copy when its expanded `made_over` (RFC 0026 §3) lacks the file's current version, whatever its `base_seq`. A device that listed a version does not thereby overwrite it; only a user's edit made over that version does. An op without an intent, or with an empty `made_over`, keeps R3's seq test.

RFC 0026 §3's checks for a resend apply to every replay, after §3.1's same-intent check. A replay whose `source_version_id` is in the target's current chain, and whose intent the field does not hold, is `SUPERSEDED`.

## 4. A lost change against a later delete

The author's own replay of a change that the server lost is carried (§3.2), even when another device deleted the node after the change. A lost change never undoes a later delete. A lost rename of a deleted node is dropped. A lost edit comes back as a conflict copy beside where the file was, unless the deleting user held it. The deleting user sees the delete done, and no write is lost.

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
| held_as | The intent the server's `NodeState` reported for the field in the result, and for content the version id. It differs from the row's intent when RFC 0016 made the op a no-op. |
| time | The `applied_at_ms` that the row's result reports for the field and the row's `held_as` (§3.1): the row's own intent, or the intent the field kept when RFC 0016 made the op a no-op. A replay keeps it; it is never the replay commit's time. Before a result, R's head time when the op was built. |
| replay | The row's intent was in a commit the server lost. |

The device writes an own row in the same transaction as the scan that finds the change (design §5.1). A later change of the same field replaces the row with a new intent:

- The new row keeps the old row's base while the old row is `local`. Once the old row was sent, the new row's base is the old row's `held_as` when a result reported one, else the old intent, which §5.2 rewrites when its result arrives.
- The commit in flight keeps every intent it sends with its value and base. A row whose value returns to the value of an intent still in flight becomes that intent again, in state `sent`.
- A row whose value equals its base's value, while no intent of the field is in flight, sends nothing while R holds the base. It ends when R takes a value that is not carried, such as another user's delete. While R holds a carried value, the row is sent, so a user's edit back to older bytes beats a carried replay that arrives later.

A delete is a status row while L lacks the node, and it keeps the **seen list**: every intent that L held for the node and the nodes below it, at the time of the delete and before, up to 64 per node and field.

The device also keeps, per node and field, the ids and applied times of the newest 64 intents that R held, in the order R first held them, each marked when L held its value too: the **history**. An intent that R holds again keeps its first position. It gives a delete its seen list. When a result makes a row `held`, the device adds its `held_as` to the history, since the server held that value. A row's own intent need not be in the history: §5.4 orders R's intents against the row's base. A trashed node's `intents.status.over` is not added to the history.

A row ends when:
- for a `held` row, R holds an intent for the field that the history places after the row's `held_as`. A record that reaches R while the row is `local` or `sent` does not end it: the row's op is still sent, and the server decides between the two (R3, or the later place);
- the row's op is answered `SUPERSEDED`;
- the user changes the field again, which replaces the row;
- for a status row, the user creates or restores the node again, or R has lacked the node for 30 days.

A row in state `held`, own or carried, stays until one of these, because a later restore can take back any commit.

### 5.2 Sending and acknowledging

- Every op names the intent of the row it sends. A create names one intent per field when the rows have different intents.
- A result that applies an op makes each row it proves `held`, with `held_as` from the result's `NodeState`, and adds `held_as` to the history. A result proves only the fields its op set.
- When a result reports a field for an intent the commit sent, every row whose base is that intent takes the reported intent as its base: `held_as` when the op applied, the op's own intent on a `CONFLICT_COPY` (the copy holds it), else the field's intent in the result's `NodeState`. This holds whether or not the sent intent still has a row.
- A row whose op was answered `SUPERSEDED` ends, and the change that superseded it reaches L as a remote change. When the result's `NodeState` is another node that holds the row's intent for the field (§3.6), the row moves to that node and becomes `held` there with its own intent as `held_as`, as on `CONFLICT_COPY`. A content replay answered `SUPERSEDED` on its own node ends, since §3.3, §3.4 and §3.8 supersede it only when a user saw its value. That move comes first. A place or status replay superseded on a trashed node ends (§3.4). It is the exception only when the result's `NodeState` is the row's own node, live, and holds, for the field, an intent applied before the row's time, or one that case 2 of §5.4 accepts against the row, such as the row's base replayed by another device first. That intent came before the row on the server, so the row is sent again with `over` set to it.
- `CONFLICT_COPY`: the content row moves to the copy node, which L's file now is (design §2.7), and becomes `held` there. The original node's rows for this device stay.
- `MERGED_INTO`: the node's place and status rows end, since the target's stand for them. Its content row and the rows of its children move to the target. No row names the merged node afterwards, so no loss creates it again.
- `APPLIED_RENAMED`: the place row becomes `held` with the reported intent, and L takes the server's name.
- `ID_EXISTS` on a create: each of the create's rows is sent again on the existing node as a `Move`, `UpdateContent` or `Delete`, with its own intent. A status row whose value is live sends nothing and ends, since the existing node is live.
- A result without intents, such as a stored result of a commit an old server applied, gives `held_as` as the §7.2 id of the result's `NodeState`.
- An owner's finishing move (§6.1) sends the intent of the place row that set the mark, with `over` set to it.

### 5.3 Deletes

A status row is owed while R holds its node live. The commit builder sends the `Delete` with:

- `unseen`: the intents of R's fields, on the node and below it, that the seen list does not hold and whose values no intent in the seen list holds;
- `held`: the intents in the seen list that R's fields there do not hold.

The server then applies §3.3 and §3.4. A delete with no loss since its user acted lists only the values that reached R but not L, so it also stops R7 from treating a record that arrived before the build as seen. When R holds the node trashed and the seen list holds intents that the trashed node's fields do not, the device sends the `Delete` once more, with `over` set to the row's base and `held` set to those intents, so the trash adds them (§3.1). Otherwise, or after that op's result, the row has nothing to send. When R lacks the node, the row waits, so a carried create of the node is deleted again.

### 5.4 After a loss

The device detects a loss as before: `AHEAD_OF_SERVER`, or a `Welcome` whose device counter or slot token shows that the server lacks a commit of this device (design §2.8). It sets `replay` on each row whose state is `sent` or `held`, and keeps the commit in flight's intents with their values. A `local` row stays as it is. Where the cases below compare O with a row's base, and that base is an own intent the history lacks, they compare O with that intent's base instead, as the commit in flight or §5.2's rewrite kept it, until they reach an intent the history holds. These steps are the row's **own chain**: changes of this device's user that the new R lacks. The device keeps the old R until the listing of the new R is complete (design §4.4 Bootstrap). Then, for each node and field, with O the new R's intent:

0. **A change not yet sent.** A `local` row is sent as a fresh user op, with its intent and without `replay`. Its wire base is the row's own, except that a content row whose own chain ends at O takes O's version as its wire base, since this device's user made every change between O and L. When the chain ends elsewhere, the original wire base stays, and R3 keeps O beside the row's value. S takes the new R's value for the field.
1. **The server holds the row's intent.** O is the row's intent or its `held_as`. When the new R holds the row's value, for a marked place the owed name, the row becomes `held`. When the new R holds the stand-in, the row stays `sent` with `replay` and the owner sends the finish again (§6.1). S takes the new R's value.
2. **A loss took this device's change.** A row with `replay`, own or carried, and O was set before the loss: O's applied time is at or before the row's time, O is the row's base, the history places O before the base, or the new R lacks the node. A restore keeps a prefix of the server's commits (§3.1), so such an O came before the lost commit, whether or not the history holds it. S takes the new R's value, and the row is sent with `replay`, its intent, its wire base and `applied_at_ms`: the time its result reported, else the row's time, else 0 for a migrated value with no known time. A carried row always sends the time of the value it carries, never a build time. A place or status row also sends `over` set to O, or no `over` when the new R lacks the node. A replayed move's `from` (RFC 0019 §4) is the new R's place, not the row's wire base. A content replay whose earlier result reported a version id sends it as `source_version_id`, so an edit made over that version on another device still finds it in the chain (RFC 0026 §2).
3. **Another user changed the field after the loss.** An own row with `replay`, O's applied time is after the row's time, and O is neither the row's base nor placed before it by the history. A place row ends; a status row whose node R holds trashed sends its seen list as §5.3 says and then ends, and any other status row ends; a content row is sent with its wire base, whose `made_over` lacks O's version, so R3 keeps both values (§3.8). A carried content row is sent as in case 2 while O's `made_over` chain lacks the row's version, so R3 keeps both values (§3.8), and it ends once the server holds that version or one made over it. Any other carried row ends.
4. **No row, and L holds what S holds.** When the new R holds S's value too, S takes the new R's intent. Otherwise X is the old R's intent for the field. The device writes a carried row for X when the new R holds X on no node and either of these is true:
   - the history places O before X, O's applied time is earlier than X's, or the new R lacks the node;
   - for content, O's `made_over` chain lacks X's version, even when O's applied time is later: nobody who changed the field saw X, so the carried replay lands as a conflict copy (§3.8, §3.5).

   The row's value is X's value in the old R. For place and status, L takes it as a remote change once R holds it again, even when L had not yet taken it. A content row is written only when L holds X's bytes; otherwise this device writes no content row, and the author or another device that holds the bytes carries X. The row has X's author, `replay`, X's applied time from the history as its time, and X's predecessor in the history as its base. A carried status row for a trash takes as its seen list every intent that the old R's trash held: the field intents of the trashed node and the nodes below it, the status intent the trash replaced on each of them (`intents.status.over`), and its `trash_held`. A content intent that the old R also holds on a live node outside the trash, such as a conflict copy that §3.3 made because the delete did not see it, is left out. The replayed `Delete` then lists it as `unseen`, and the server makes that copy again (§3.3); the device does not create it. It sends `held` and `unseen` from that seen list as §5.3 says, so a value that reached the new R after the loss, such as a file created in the folder after a restore, is `unseen` and R7 keeps it. The server's view is then the one the lost delete had. The row is sent as in case 2, and S takes the new R's value.
5. **Anything else.** R's value reaches L as a remote change.

A node that the new R lacks is created again by its rows, or by carried rows, in one create that names each field's intent. Its parents come first, by the same cases. A server-made conflict copy whose content intent the new R holds on another node is not created again. A node that the new R lacks and that no row covers is a value nobody can show the server lost: S forgets it and it is not sent.

These cases replace `forget_lost` and the rules of design §4.4 "Commits the server lost": the bound, the slot seqs, `resends`, `lost_creates`, `lost_positions`, `unsent_edits`, the kept and forgotten shapes, and the resend-yield rules.

### 5.5 History changes and store rollbacks

Recovery after a history change or a store rollback (design §6.4) counts a node as changed on this device when a `local` or `sent` row covers it, or when L differs from the recovery's baseline. A `held` row does not count, and neither does a row whose value the new R holds for the field, when that value is not carried: that row becomes `held` with the new R's intent as `held_as`, as an RFC 0016 no-op would make it.

A Publish answer sends each row of the node as case 0 of §5.4 does, with the row's own wire base, so R3 keeps the new R's content beside the row's value when the user's change was not made over it (§3.8). A Discard answer ends the node's rows. A copy that a recovery answer makes takes the content intent of the value it keeps, carried with that intent's author, and place and status intents that this device mints for its user's answer, which are not carried. A copy that the new R lacks, whether the server or a recovery answer made it, is not created again when the new R holds its content intent on another node, whatever the answer for that node and in whichever order the answers come. The questions and answers of design §6.4 are otherwise unchanged.

## 6. Pending names

### 6.1 Owner only

A pending name is owed by one device, the **owner**: the author of the intent that set the mark. The server records it in `pending_by` (RFC 0017 §2) from that intent's author, where it records the committing device today. A carried replay of a create that sets a mark keeps the original owner. A revoked owner's mark follows §3.1.

Only the owner finishes the rename, unless the mark is ownerless (§3.1). This replaces RFC 0017 §3's rule that any device may finish it when no other node holds the name. Another device never finishes a mark: its effects leave the node at the stand-in name until the owner's finish arrives in R. A user's move of the node on any device is sent like any move and clears the mark (§6.2).

The finish completes the owner's intent that set the mark. The place row's value is the owed name, and the stand-in is a step on the way: the row is `held` only when R holds its intent at the owed name. The finish sends that intent with `over` set to it (§3.1), so a user's rename or mark clear that lands first supersedes it. An owner whose store lost the row finds itself in `pending_by` and sends the finish with the intent R holds for the place, which it authored.

### 6.2 The mark on every create

`CreateFile.pending_for` carries a file's mark, as `CreateDir.pending_for` carries a folder's. A create that a replay sends for a marked node carries the mark.

A user's move of a marked node clears the mark and sets a new place intent, also when the move ends at the node's place. The journal keeps such a place row even though its parent and name equal its base's, since the mark differs.

## 7. Migrations

### 7.1 Client store

One migration adds:

- `journal`, keyed by node and field, with the columns of §5.1;
- `delete_views(node_id, view_node_id, field, intent, value)` for the seen lists of status rows, where the value is the version id and manifest digest, the parent and name, or the status;
- `intent_history(node_id, field, ord, intent, in_local)`;
- `remote_nodes.intents` and `synced_nodes.intents`: the node's three intents.

It fills them in this order. A later step does not overwrite a row an earlier one wrote, except that step 3 writes the row for L's value when the user changed a field again after an op step 2 read, with that op's intent as its base:

1. `resends` and `lost_creates`: a carried row with an empty author, whose intent is the legacy intent (§7.2) of L's value, for each field where L differs from R, and for every field when R lacks the node. Neither table records who made the value. The rows are carried by their author (§3.2), so they are never sent as user changes.
2. The commit in flight, in state `sent`. An own row for an op whose node `local_changes`, `edited_here` or `moved_here` holds and that has no `resend`, `from` other than a pending finish, or `source_version_id`. A carried row with an empty author for every other op. The op's provenance decides the author, never the device that sent it. A value whose author the store can't show is carried, which can cost a conflict copy but never a write. Then `unsent_edits`: own `local` rows. Each row gets `replay` when the store records a loss.
3. Every other field where L differs from the acknowledged value (the waiting update when one waits, else S; `Trees::acknowledged`), and every node L lacks and that value holds: an own `local` row with a new intent, whose base is the legacy intent of the acknowledged value. A waiting acknowledged op goes to step 4.
4. `edited_here`, `moved_here`, `trashed_here` and waiting acknowledged ops: own `held` rows whose intent and `held_as` are the field's legacy intent, and whose base is the legacy intent of the `replaced_versions` or `past_positions` entry the change replaced, or of the status before the delete.
5. `remote_nodes.intents`, `synced_nodes.intents` and `intent_history`: legacy intents, one per field, plus one for each `replaced_versions` and `past_positions` entry. S's place intent is R's when S's place equals R's, else that of the matching `past_positions` entry. A status row's seen list takes the history entries L held, and, for a node L lacks, the entries S holds. For a `trashed_here` node it takes R's trashed record and the `past_positions` entry that record replaced.

On first contact with a server that announces `field_intents`, a migrated store lists the space again before any row takes a base from R. It replaces each id it computed, in R, S, the history and the rows' intents, `held_as`, bases and seen lists, with the server's intent for the same node, field and value.

A store with a loss recorded but not yet listed keeps the record, and the bootstrap that follows recovers with these rows.

A later migration, in the change that switches recovery to §5.4, drops `resends`, `lost_creates`, `lost_positions`, `lost_shapes`, `forgotten_shapes`, `dropped_edits`, `unsent_edits`, `unsent_moves`, `unsent_marks`, `pending_finishes`, `content_provenance`, `local_changes`, `edited_here`, `moved_here`, `past_positions`, `trashed_here`, `replaced_versions`, `unseen_changes`, `removed_here` and `waiting_updates.slot`. `commit_slots` stays for the device counter and its tokens, and `kept_originals` stays for RFC 0015.

### 7.2 Legacy intents

A field set before either side knew intents gets a legacy intent: an empty author, `replay` false, and an id that the client and the server compute alike:

```
id = UUIDv5(701471d0-f5ea-47eb-936a-485a91d0891e, utf8(node_id) || 0x00 || utf8(field) || 0x00 || utf8(key))
```

UUIDv5 is RFC 9562 §5.5. `field` is `place`, `content` or `status`. The key is:

- content: the version id;
- place: `decimal(meta_rev) || 0x00 || parent_id || 0x00 || name`, with the parent id as `NodeState` reports it (the space id for a child of the root, `""` for the root itself) and the stored NFC name;
- status: `trashed:` followed by the trash group, or `live:` followed by the decimal seq of the commit that last made the node live.

`spec/vectors/legacy-intents.json` lists vectors that both implementations test against. A client that can't know that seq uses the earliest `meta_rev` it holds for the live node, until the first-contact listing replaces the id. A server migration gives every existing node these ids. An RFC 0004 §5 rebuild of a record without intents reads the seq that last made the node live, and the commit at each field's revision, from the current history's log segments at or below the snapshot's `start_seq`. When they lack that seq, it uses the earliest `meta_rev` it finds for the live node, as a client does, and when they lack the commit, the applied time is 0. A rebuilt status id can then differ from the one the lost server gave. A device that still holds the old id gets the same answers, because §5.3 and §5.4 compare status by value, and a device that detects a loss takes the new id through §5.4. A client computes the same ids for a field that a server without `field_intents` sends with no intent. A legacy intent's applied time is the `committed_at_ms` of the commit at the field's revision, or 0 when the server no longer holds that commit, so a backfill rerun after a restore never stamps it later than a lost commit.

## 8. Mixed versions

- **Old client, new server.** The client sends no intents and no `unseen` or `held`. The server mints intents (§3.1), carried for a resend, applies R3 and R7 by seqs and revisions as before, and keeps no `held` on the trash. The old client's loss rules keep running, with their known faults. One of them reaches new devices: an old client's `CreateDir` that re-creates another device's folder carries no resend marker, so its intent is not carried. An old client's edit has no `made_over`, so a bystander also carries a lost value that edit was made over (§5.4 case 4). That costs an extra conflict copy, not a write. A create or move into a purged folder is `APPLIED` under the nearest live ancestor (§3.3) instead of `REJECTED`. The old client learns that place from the change record, as a remote move.
- **Before `field_intents` is announced.** A server records and reports intents from the deploy that adds them. A client sends no intents and reads none until the server announces the feature. A store that migrated in between lists again on first contact (§7.1).
- **New client, old server.** The server sends no intents. The client keeps its journal and computes legacy intents for R. It does not recover from a loss automatically: it halts and asks, as design §6.4 does for a store rollback, because without server intents case 4 of §5.4 can't tell a lost value from a later one. It sets `UpdateContent.resend` and `Move.from` on carried replays, so an old server applies RFC 0018 and RFC 0019 to them.
- **Mixed devices on a new server.** A field an old client sets has a server-minted intent, which new devices compare like any other.

## 9. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` gains:

- an op without an intent gets a minted one, and `NodeState` reports all three;
- an op whose intent and value the field holds is `APPLIED` and changes nothing, except that a same-intent `Delete` adds its `held` to the trash (§3.1);
- a create with `fields` records a different intent per field;
- a carried `UpdateContent` on a trashed file makes a conflict copy, and one whose intent is in the trash's `held` is `SUPERSEDED`;
- an author's replayed `Move` on a trashed node is `SUPERSEDED`;
- a `Delete` listing an unseen user intent keeps the node; one listing an unseen carried content intent trashes the node and makes a conflict copy;
- a carried `Delete` of a folder, replayed with `base_seq` past a create made in that folder after a restore and `unseen` listing that create's intents, keeps the created node and the folder (R7), and the same replay with no create after the restore trashes the folder;
- a replay of an intent the index holds on another node is `SUPERSEDED`;
- a carried `CreateDir` with a mark keeps its author as `pending_by`, and stores no mark when that author is revoked;
- a `Move` without `pending_for` to a marked node's own place clears the mark and sets a new place intent;
- an owner's finish with the marking intent and `over` applies, and is `SUPERSEDED` after a user's same-place mark clear;
- a `Move` or `Delete` whose `over` the field does not hold is `SUPERSEDED`; a `Delete` whose `over` is the status the trash replaced merges its `held` and is `APPLIED`;
- an `UpdateContent` with an intent whose `made_over` lacks the current version makes a conflict copy, even when its `base_seq` covers that version;
- a `Delete` sent to a node another delete trashed, with `over` set to the status that trash replaced, adds its `held` to the trash;
- a second delete of a trashed node adds its `held` to the first one's, and a carried replay that either lists is `SUPERSEDED`;
- a carried `CreateFile` whose place intent the trash's `held` lists and whose content intent it does not makes a conflict copy;
- digest v5 refuses a reused `commit_id` that differs only in an intent;
- each field intent carries the time of the commit that first applied it, a replay that sends `applied_at_ms` keeps it, and commit times grow across a restore of storage;
- an op without an intent that has `resend`, `from` or `source_version_id` gets a carried intent with an empty author;
- a conflict copy made from a carried op has carried place and status intents, and a later `Delete` of its folder moves it to the nearest live ancestor;
- a `Delete` whose view holds a carried copy's content intent trashes that copy in its group, also when it only adds `held` to an existing trash;
- revoking a device finishes its marks whose names are free and leaves the rest ownerless;
- a replay of a purged node makes a content copy unless the tombstone lists it;
- a carried create or `Move` into a purged folder whose tombstone holds or lists its intent is `SUPERSEDED` for place and status, and any other create into a purged folder is `APPLIED` under the nearest live ancestor, or `APPLIED_RENAMED` on a name clash;
- a create on an existing id whose fields do not all hold its intents and values is `REJECTED` `ID_EXISTS`;
- a same-intent `Delete` with a larger `held` adds it to the trash and writes a change record, and an unchanged retry writes none;
- `NodeState.trash_held` exports the trash's `held`, and an RFC 0004 §5 rebuild keeps it.

## 10. Cost

- Each op grows by about 60 bytes, and a create with `fields` by up to 180.
- Each `NodeState` grows by up to 200 bytes, so a listing of 100,000 nodes grows by about 20 MB before compression.
- A replayed `Delete` grows by 16 bytes per intent in `unseen` and `held`: the values a loss took or that reached R but not L.
- The server stores three intents per node, an index row per current intent, a carried flag per field and the `held` list on trashed nodes.
- The server keeps a tombstone per purged node, about 100 bytes plus its `held` list, for the life of the history.
- The client stores up to three journal rows per changed node, up to 64 history entries per node and field, and a seen list per pending delete.

## 11. Not covered

- **History beyond 64 intents.** A restore that brings back a value older than a device's history makes case 5 of §5.4 apply where case 4 should. The device downloads the older value, and the author still replays its change. A delete counts such a value as unseen and keeps the node. No write is lost.
- **A carried create after 30 days.** A status row whose node R lacks ends after 30 days. A carried create that reaches the server later brings the node back.
- **The owner never comes back.** A mark whose owner never commits again and is not revoked keeps its stand-in name.
- **A row whose result never arrived.** Its time is R's head time when its op was built. Another device's change applied between that head and the lost commit counts as later (case 3), so this device's lost place or status change gives way to it.
- **An old client re-creating a folder.** Its intent is not carried (§8), so a new device's waiting delete of that folder can keep it.

## Appendix A. Failure cases each rule settles

- **Authors on every send (§3.2, §5.4 cases 2 and 4).** A create that another device resent after a second loss keeps its author, so this device neither takes it for its own create nor fails to delete it again. A move that S shows is replayed with its author's intent. A bystander that resent another device's create, or a copy of another device's lost write, replays it as carried after a second restore, so a deleted folder stays deleted. No device needs a table of the values it took from others.
- **Applied times instead of seqs (§3.1, §5.4 case 2).** A later rename replays over a carried create, guarded by the new R's place. An edit that the server made a no-op is replayed whether or not its intent reached R. A revert or an untouched download over an edit in flight is told apart from the edit by its row.
- **Rows per field (§5.1, §5.5).** A rename writes a place row only, so a file's unchanged content never counts as changed here. A row equal to its base ends when R takes another user's value. An edit back to older bytes beats a carried replay that arrives later. A rekeyed folder's children count as changed here only when a `local` or `sent` row covers them.
- **The seen list and `unseen` (§5.3, §3.3, §3.4).** A delete stays owed after S forgets the node, and after another device's rename changes the shape. A rename that reached R but not L keeps the node, and a carried move built before that rename is superseded. A lost rename that the deleting user held is sent as `held`, so the trash keeps it. A delete over a value that a restore brought back sees that value. A replay that a delete's view held is superseded on the trash.
- **Content ancestry (§3.8, §5.4 case 2).** An edit replays over the version it was made over, not over R's current content. A replay of a version that the current chain already includes is superseded.
- **Carried copies (§3.3, §3.5, §3.6).** A copy of a lost write that lands before a delete of its folder moves to the nearest live ancestor, or goes to the trash with the folder when the deleting user saw the write. A re-keyed conflict copy of carried content is never taken for a user's node. A lost conflict copy whose content the new R holds on another node is not created again.
- **Recovery copies (§5.5).** A lost copy of a write that a server file holds is not created again while the server holds it. An offline edit of that file, answered Publish or Discard before or after the copy, leaves one file per write. A same-bytes offline rewrite becomes `held`, so a later edit is made over that value.
- **Merges (§5.2).** A folder merged into another is not created again after a loss.
- **A lost change against a later delete (§4).** The delete wins, and a lost edit comes back as a conflict copy.
- **Pending names (§6).** Only the owner finishes a pending name. A created file carries its mark. A stand-in name is told apart from a name the user chose.
- **Intents on listings (§2, §5.2).** A listed version or revision shows whether it came from a resend or from this device's commit.

Other fixes settle commit order, a scan adopting a mark, compensation planning, a restore that brings back commits this device dropped, which local node owns a disk path, and a finish that arrives before the name's holder moves away.

## Appendix B. Related RFCs

- **RFC 0013** commit tokens stay. The slot token in `Welcome` detects a loss. This RFC removes the bound (`held_through`) that recovery read from the slots.
- **RFC 0018 and RFC 0019** stay for clients and servers without `field_intents`.
- **RFC 0026** edit ancestry stays: §3.8 makes `made_over` the content base that R3 reads, and extends its resend checks to every replay.
