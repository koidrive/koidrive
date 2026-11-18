# RFC 0018: A delete beats a resend it did not see

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §6.2 R7 and R8, RFC 0016 §1, RFC 0006 §8 and RFC 0004 §5, and adds fields to `UpdateContent` and `NodeState`. Builds on RFC 0017, whose `NodeState` fields 10 and 11 it leaves alone.

After a server loses commits, a device that downloaded another device's edit before the loss sends it again (RFC 0016 §3, [sync-engine design §4.4](../../docs/design/sync-engine/server-order.md#44-receiving-changes)). This resend holds no change of the sending device's user. A delete that another device's user makes after the loss should beat it, since that user may have held the content the resend brings back. A resend sent with `require_head` (RFC 0002 §4) yields to a delete the server takes first. A delete the server takes after the resend was built without it: R7 counts the resend as a content change the delete did not see and keeps the file.

This RFC lets a resend say what it is, and lets a delete pass over a resend it did not see. The author's own edit still beats a later delete, whichever device's send of it the server takes first. A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only:

- `UpdateContent`: `resend = 3`, a bool.
- `NodeState`: `edit_rev = 12`, a uint64. Fields 10 and 11 are RFC 0017's.

## 2. The edit revision

Each node has an edit revision: the seq of the newest commit that created it, changed its content other than by a resend, or carried a user's edit of content a resend brought (§3). A create, an `UpdateContent` without `resend`, a `Restore` of a version and a conflict copy set it to the commit's seq, as they set `content_rev`. An `UpdateContent` with `resend` that changes the content moves `content_rev` and leaves the edit revision as it was. The server also keeps the device of the commit that set it, as it does for `content_rev`. `NodeState.edit_rev` reports it. It is below `content_rev` while the file holds content a resend brought, and above it after §3.

## 3. A user's edit of content a resend brought

This amends RFC 0016 §1. When another device's resend of a lost edit lands first, the author's own send of that edit meets unseen equal content. RFC 0016 §1 would apply it without a change, so no revision would record the author's edit, and a delete with a base below the resend would trash it.

So an `UpdateContent` without `resend` that meets unseen equal content (RFC 0016 §1) while the file's edit revision is below its `content_rev` is `APPLIED` as an edit. It makes no version and moves no `content_rev`. It sets the edit revision and its device to this commit, and R8 revives a trashed file as for any edit. The result's changes hold a `NodeState` for the file. RFC 0016 §1 stands for every other `UpdateContent` that meets unseen equal content: one with `resend`, or one that meets an edit revision at or above `content_rev`, changes nothing and leaves a trashed file in the trash.

After device B resends device A's lost edit:
- When A sends the edit next, then D's `Delete` with a base below both arrives, the delete keeps the file, since A's edit is above its base (§4).
- When D's `Delete` arrives first and trashes the file, A's edit then revives it.

## 4. Deletes

R7 reads the edit revision. A `Delete` keeps a node of the subtree it trashes, and the folders between that node and the deleted one, when the node's edit revision is above `base_seq` and another device set it, where R7 read `content_rev` before. The checks on `meta_rev` and on the parent stay as they are. So a delete trashes content that a resend put on the server after the delete's base, and keeps any user's content change it did not see, a change before the resend and one under §3 included.

R3 still reads `content_rev`. An `UpdateContent` built without the resend meets content it did not see and makes a conflict copy, as before, or is applied as RFC 0016 §1 and §3 say when its content is equal.

## 5. A resend on a trashed node

An `UpdateContent` with `resend` whose target is in the trash is answered `SUPERSEDED` and changes nothing. R8 does not revive the node. The delete that trashed it was made by a user who may have held the content, as in §4.

## 6. Request digest

The digest of a commit request (RFC 0006 §8) becomes digest v2. It is digest v1's projection, plus `UpdateContent` field 3. The server stores the digest's version beside each digest, in `commits` and in `device_commits`. A row that holds a digest from before this RFC holds v1 and is checked with v1, and a row with no digest is still checked as RFC 0006 §8 says. A reuse of a `commit_id` that differs only in `resend` is refused with `INVALID_ARGUMENT` when its row holds v2. A v1 row cannot tell such requests apart, so the reuse gets the stored result: an old server ignored `resend` when it applied the commit, so that result is what either request was answered. Since proto3 omits a false bool, the v2 digest of a request without `resend` equals its v1 digest.

## 7. Clients

R, S and L are the engine's remote, synced and local trees (sync-engine design §2.2).

A device sets `resend` on an `UpdateContent` that sends again content another device made and the server lost, with no change of its own user in it. The author of a lost edit sends it again without `resend`, so its edit still beats a later delete (§3, R8).

A device takes a missing `edit_rev` (0) as equal to `content_rev`.

A device whose user deleted a file it held as S does, before a resend of it arrived in the log, plans the delete against the resend. S takes the shape of a record of another device's commit when all of these hold:
- the record moves the file's `content_rev` and changes nothing else: its `edit_rev`, `meta_rev`, parent and name equal R's just before it;
- R held S's content and position just before it;
- L no longer holds the file.

The delete is then sent with a base past the resend, and §4 applies it. A record that also renames or moves the file is not taken, so R7 still keeps a move the delete did not see. A listing shows no such step and changes nothing here.

A record of another device's commit that moves a file's edit revision but not its `content_rev` (§3) is a remote edit to a device whose L no longer holds the file. So is a record that moves both while R held content a resend brought, with the edit revision below `content_rev`: the author saw the resend before it sent its edit, so the server made a new version with the same bytes. S forgets the file, and the device downloads it again, as it does for an edit that brings new content. A device that adopted a resend above therefore keeps the author's edit that follows it, and it sends no delete over that edit. A record of the device's own commit is neither: the author that deletes the file after sending its edit sends the delete, with a base past its own edit, and §4 applies it.

A device that upgrades its store sets each stored node's edit revision to its `content_rev`. It does this with an `ALTER TABLE` that adds the column, then an `UPDATE`, or with a nullable column whose NULL reads as `content_rev`.

## 8. Mixed versions and stored state

- An old client never sets `resend`, so a new server changes nothing for it, beyond §3's revision on an equal edit.
- An old server ignores `resend` and sends no `edit_rev`. A new client reads `edit_rev` as `content_rev`, so it keeps a file a resend changed, as before. It keeps `require_head` on resends, which old and new servers both honour.
- A server that upgrades sets the edit revision of each node to its `content_rev` and device, and gives every stored digest v1.
- An export (RFC 0004) carries `edit_rev` in `NodeState`, but not its device. A restore gives the edit revision the device of `current` when `edit_rev` equals `content_rev`, and no device otherwise. R7 then counts that edit as another device's for every delete. So a restore can make a delete keep a file it would have trashed, and never makes it trash one. A record without `edit_rev` gets `content_rev` and its device.
- RFC 0004 §5's replay compares `max(content_rev, meta_rev, edit_rev)` where it compared `max(content_rev, meta_rev)`, since a §3 record moves the edit revision alone.

## 9. Conformance

`spec/testdata/scenarios/space-conformance-v1.json` gains these scenarios:
- A resend, then a `Delete` with a base below it, is `APPLIED` and trashes the file.
- After a resend, an `UpdateContent` with a base below it and other content makes a conflict copy.
- A resend on a trashed file is `SUPERSEDED`, and the file stays in the trash.
- After a resend, an equal `UpdateContent` without `resend` and with a base below it is `APPLIED` with a `NodeState` whose `edit_rev` is its seq. A `Delete` with a base below the resend then keeps the file.
- After a resend and a `Delete` that trashes the file, an equal `UpdateContent` without `resend` revives it.
- A reuse of a `commit_id` whose request differs only in `resend` is `INVALID_ARGUMENT`.

## 10. Cost

An `UpdateContent` grows by two bytes when `resend` is set. Each node stores one more revision and device, and each stored commit result stores its digest version. `NodeState` grows by up to 11 bytes.

## 11. Not covered

- A lost rename has the same race in its `Move` resend, once a delete of such a file is sent at all.
- The rule in §7 runs on log records only, not on listings.
- A copy a device keeps of a resend beside the server's node is sent as a create. A create carries no `resend` and sets the edit revision (§2). `require_head` makes it yield to a delete the server takes first, but a delete that lands after it, built without it, keeps it under §4, and with it the folders between it and the deleted one, so a folder a user deleted after the loss comes back.
