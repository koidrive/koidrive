# RFC 0023: When and where a trashed node was deleted

Status: proposed. Applies to spec v0 (format_version 1). Adds a field to `TrashPage` and the message `Trashed`.

A person who deleted a document by mistake looks for it in the trash by when it went and from which computer. `ListTrash` returns each trashed node as a `NodeState`, which says neither. This RFC adds both to `TrashPage`. A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only:

- `TrashPage`: `trashed = 3`, a repeated `Trashed`.
- `Trashed`: `node_id = 1`, a string; `trashed_at_ms = 2`, an int64; `device_id = 3`, a string.

No field is removed or renumbered. `NodeState` does not change, so the change records and commit results that carry it do not grow.

## 2. Server

For each node in `TrashPage.nodes`, the server adds one `Trashed` entry with the same `node_id`. `trashed_at_ms` is the `committed_at_ms` of the commit whose `Delete` trashed the node, and `device_id` is the device that sent that commit.

- A `Delete` of a folder gives every node it trashes the same entry.
- A node trashed before its folder keeps its own entry, as it keeps its own trash group (RFC 0012 §1).
- A node that R8 or R9 revives and a later `Delete` trashes again gets the entry of the later commit.
- A node that a commit stores in the trash without a `Delete`, as a carried create into a trashed folder (RFC 0029 §3.4), gets the entry of that commit.

The entries count toward the 1 MiB page limit, and each page carries the entries of its own nodes.

## 3. Clients

A client matches an entry to its node by `node_id`. It shows the deletion time and device only for a node that has an entry. A server without this RFC sends no entries. `device_id` is the id that `GET /v1/devices` lists (RFC 0006 §7).

No `Welcome` feature announces this RFC, because a client reads the absence of entries as an unknown deletion.

## 4. Cost

An entry adds about 85 bytes per trashed node to a page: two UUIDs and a timestamp. The server reads no more rows, because it keeps the deletion time and the device on each node it trashes.
