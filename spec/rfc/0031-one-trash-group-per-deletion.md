# RFC 0031: One trash group for a deletion that takes several ops

Status: proposed. Applies to spec v0 (format_version 1). Adds a field to `Delete`.

A folder whose change records do not fit in one commit is deleted in several ops, contents first, over several commits (sync-engine design §2.4). Each `Delete` makes its own trash group (RFC 0012 §1), and a restore of the folder brings back only its own group (R9). This RFC lets every `Delete` of such a deletion trash into one group, so the folder comes back whole. A bare section number such as §2 refers to this RFC.

## 1. Message

`Delete` gains `trash_group = 5`, a string. Empty means the op's own group, `<commit_id>:<i>`.

## 2. Server

A `Delete` that names a group trashes every node it trashes into that group, when the group is `<commit_id>:<j>` with `j` in canonical decimal and either:

- `commit_id` is the commit that holds the op, and `j` is at most the op's index; or
- `commit_id` is a stored commit of the device that sends the op, and the trash holds a node in that group.

Otherwise the op takes its own group, as if the field were empty. The field never makes an op fail. A `Delete` that trashes nothing is `SUPERSEDED` as before, whatever the group already holds. R7, R8, R9 and purges treat a joined group as any other group: a restore of a node brings back the members of its group below it.

The digest of a commit request (RFC 0006 §8) becomes digest v6: digest v5's projection plus `Delete` field 5. Since proto3 omits an empty string, the v6 digest of a request whose deletes name no group equals its v5 digest.

## 3. Clients

A client that deletes a folder in several ops names, on each `Delete` of that deletion, the group of its first `Delete`, the folder's own last `Delete` included. It keeps the group for the folder while the folder is deleted locally and still synced, and forgets it when the commit that opened the group is given up.

A restore of a node from the trash whose group does not fit in one commit is sent as restores of the topmost members of its group below the node, over several commits. Each brings back the node too, as R8 does for an op below a trashed folder. Once no member is left in the trash, the restore of the node itself ends the request.

## 4. Mixed versions

A server without this RFC ignores the field and gives each `Delete` its own group, so a restore brings back what it did before. Nodes trashed before the upgrade keep their groups. No `Welcome` feature announces this RFC.
