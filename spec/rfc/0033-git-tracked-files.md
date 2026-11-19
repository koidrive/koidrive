# RFC 0033: Files git tracks sync when a .gitignore file matches them

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0004 §5, RFC 0006 §1 and §8, RFC 0022 §2 and the sync-engine design §5.10. Adds two fields to `NodeState`, an op to `Op` and the feature `git_tracked`.

Git never ignores a file it tracks: after `git add -f dist/app.js` the file stays in the repository although `.gitignore` lists `dist/`. A device that follows `.gitignore` files leaves such a file out (design §5.10), so it never reaches the server. Reading the git index on the device that holds the repository would upload it, but the other devices hold no `.git` folder, since §5.10 leaves it out, so their `.gitignore` files still match the file and they don't download it. This RFC records on the server that git tracks the file, so every device includes it.

A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only. No field is removed or renumbered.

```proto
message NodeState {
  // True when a device's git index listed this file (RFC 0033). Only a file carries it.
  bool git_tracked = 16;
  // The seq of the newest commit that changed git_tracked (RFC 0033). 0 while it never changed.
  uint64 git_tracked_rev = 17;
}

message Op {
  oneof op {
    MarkGitTracked mark_git_tracked = 9;
  }
}

message MarkGitTracked {
  string node_id = 1;
  bool tracked = 2;
}
```

The feature `git_tracked` (RFC 0022) enables the op. It is on by default.

## 2. Server

1. `MarkGitTracked` sets `git_tracked` of a live file to `tracked`. It changes no place, status or content. When the value changes, `git_tracked_rev` takes the commit's seq, and `content_rev`, `meta_rev`, `edit_rev` and `meta_edit_rev` keep their values. So a mark never reads as a move (RFC 0019 §3), and a `Delete` whose `requires` names the node (RFC 0015) still applies. A value equal to the stored one is `APPLIED` and writes no change record.
2. A trashed or purged node answers `SUPERSEDED`. A directory answers `REJECTED`.
3. The op records no intent (RFC 0029): the server ignores its `intent` and `fields`, and `FieldIntents` gains no field. §3 rule 1 restores a mark after a server loss instead.
4. No other op changes the mark. A `Move`, `UpdateContent`, `Delete` or `Restore` of the node keeps it. A conflict copy of a marked file starts with the mark and with `git_tracked_rev` at the commit's seq, so the edit it keeps reaches every device that the original reaches. Any other node the server creates, by a create or a restore to a new node, starts without it. A purge removes it with the node.
5. Any principal that may commit to the space may send the op.
6. The request digest (RFC 0006 §8) becomes digest v7: digest v6's projection (RFC 0031) plus `Op` field 9 and `MarkGitTracked`. A request without the op has the same v7 digest as v6.
7. `NodeSnapshot.node` carries both fields, so an export keeps the marks. The test of RFC 0004 §5 for a newer `NodeState` compares `git_tracked_rev` too: a record is newer when the largest of its `content_rev`, `meta_rev`, `edit_rev`, `meta_edit_rev` and `git_tracked_rev` is above the restored node's.
8. A client takes the mark from every record it applies, also from a record in which no other revision changed, and a mark change is no user change of the node.
9. While `git_tracked` is off (RFC 0022 §3 rule 4), the op answers `REJECTED` and the commit's other ops apply. Stored marks stay and are still sent: a mark only makes a device include a file that git keeps.

## 3. Who sets and clears a mark

A **work tree** is a folder of the space that holds an entry named `.git`: a folder, which is its git directory, or a file whose `gitdir:` line names one, as in a linked worktree or a submodule. Its **index** is the file `index` in that git directory. A path belongs to the deepest work tree above it. A device **reads** a work tree's index when the git directory is on its disk. An index lists a path when it holds an entry for it at any stage whose mode is a regular file (`100644` or `100755`), or a sparse directory entry above it. So a file with a merge conflict, which has only stage 1 to 3 entries, stays listed. A device reads index versions 2, 3 and 4, a split index (the `link` extension and the shared index it names) and a sparse index (`sdir`), and skips every optional extension, whose signature starts with an uppercase letter. An index with any other required extension, whose signature starts with a lowercase letter, is unreadable (rule 3). Paths are compared by `name_key` (design §5.5), as the matcher compares names.

A device that reads the index of a work tree applies these rules to the file nodes L holds in it, whether or not the device follows `.gitignore` files:

1. **Set.** For a node at a position that a `.gitignore` file matches: while its index lists the path and R holds the node without a mark, it sends `MarkGitTracked` with `tracked` true. A mark that a server loss, an export restore or another device took away is therefore set again. Files no `.gitignore` file matches need no mark, so a large repository sends no op for them.
2. **Clear.** The device stores, per node, whether its index listed the node at its last read: for each node R marks, and for each node it sends a set for, stored at the read that makes it send the set. When a node whose stored answer is listed reads as not listed, the device sends `tracked` false once every set it sent for the node has its outcome and R marks the node. The stored answer stays listed until that clear is sent, also while R doesn't mark the node yet. It clears only on this change of its own answer. A node that moves to a path its index doesn't list counts as such a change, and so does a move out of the work tree, to a path no index of that device covers: the file has left the repository. A mark is cleared this way wherever the node is, also while no `.gitignore` file matches it, so a pattern added later doesn't bring back a file git no longer tracks.
3. **Unreadable index.** When the index is missing, being replaced, or uses a version or extension the device can't read, the device keeps the answers it read last and sends nothing for that work tree. Before its first read, it holds no answers.
4. The device reads the index again when its size, modification time or file identity changes. Git replaces the index by a rename, so a new identity shows a new index even when size and time match. It checks on every scan of the work tree and on a watcher event for the index file.

A device that reads no index for a path never sends the op for it.

### Sending the op

A device sends `MarkGitTracked` only on a connection whose `Welcome.features` lists `git_tracked`, since `ServerInfo.features` can list it up to 300 seconds before the Space DO runs the code (RFC 0022 §3 rule 2). Each commit that holds the op holds no other op. A Space DO without the code answers such a commit `INVALID_ARGUMENT` and applies nothing. For a commit that holds only `MarkGitTracked` ops, that answer is no policy halt (amending RFC 0006 §1): the device gives the commit up, its slot stays free (RFC 0002 §2), it reads `ServerInfo` again, and it sends marks again only on a later connection whose `Welcome` lists the feature.

### Where the facts live

`core/fs` reads the index of each work tree on its scans (rule 4) and passes the paths it lists to the engine with the batch, as it passes rules files (design §5.10). The engine owns the matcher and applies the marks R holds, and it passes the marked paths that the rules files would exclude to the matcher `core/fs` uses, so scans and watchers list them. A mark that is set, cleared or moved by another device's op changes what the rules include, so the driver rescans the node's path and the folders on the way to it, as after a change of a rules file.

What follows from these rules:

- **Two devices with different indexes**, such as two clones of one project linked to one space, one behind the other: a file that either index lists gets the mark. A device clears it only when its own index stops listing the file, and a device whose index still lists it sets it again. So the mark is the union of the indexes. It settles after one set and never alternates. When the clone that is behind catches up and stops listing the file, it clears the mark too.
- **The file is untracked** (`git rm --cached`): the device clears the mark. Every device that follows `.gitignore` files then leaves the file out. Nothing is deleted on any side (design §5.10, Transitions).
- **Another device moves or renames a marked file:** the node keeps its mark until a device that reads the index sees that git doesn't list the new path and clears it, which is what `git status` reports there.
- **The repository is removed** (its `.git` is deleted, or the device that holds it unlinks the folder or is removed): no device reads an index for those paths, so the marks stay and the files keep syncing. A `.koiignore` pattern still leaves any of them out (§4).
- **A conflict copy of a marked file** keeps its mark for good: no index lists its name, so no device's answer for it ever changes from listed. It syncs like the file it was copied from until a `.koiignore` pattern leaves it out or the person deletes it.
- **A clear that a server loss took away** is not sent again, since the index no longer changes. The file keeps syncing until git tracks and untracks it again or a `.koiignore` pattern leaves it out. Losses fall on the side of syncing a file, never of dropping one.

## 4. Which rule decides

For a path, a device asks in this order, and the first that decides wins:

1. The built-in exclusions.
2. The device-local list.
3. The `.koiignore` files.
4. The `.git` default.
5. **Git's index.** A node R marks is included at its position, and so is a file that an index this device reads lists. A folder on the way to such a file is included for it. When a `.gitignore` file excludes that folder, the folder's other entries stay out, with the folder's reason, as git keeps the untracked files of an ignored folder out. Step 5 includes the node, not whatever entry the device holds at its path: §5 says what happens to a file the device never synced there.
6. The `.gitignore` files, when the device follows them.

Step 5 matters only where step 6 would exclude. A mark therefore never includes what a built-in exclusion, the local list or a `.koiignore` file leaves out, which is the reach git gives its index over `.gitignore`. A device that doesn't follow `.gitignore` files includes these files without the mark.

Steps 3 and 4 are unchanged, so §5.10's "`.koiignore` overrides `.gitignore` in both directions" still holds.

## 5. Design §5.10

§5.10 replaces "Git's index is not read either, so a file git tracks is left out when a `.gitignore` pattern matches it, although git keeps it" with the order of §4. The Transitions table gains these rows:

| Situation | What happens |
|---|---|
| Git tracks a file that a `.gitignore` file matches (`git add -f`), or a device first reads an index that lists one | The device with the index includes the file at once, uploads it when the server lacks it, and marks it. Every device that follows `.gitignore` files includes the node and downloads it, with the folders on the way to it |
| Git stops tracking the file (`git rm --cached`) | The device whose index stopped listing it clears the mark, unless another device's index still lists it (§3). Every device that follows `.gitignore` files excludes the node, as for any node that becomes ignored: nothing is deleted |
| Another device moves or renames a marked file | The node keeps the mark until a device that reads the index sees git doesn't list the new path |
| The repository is removed, or the device that holds it unlinks the folder | The marks stay, and the files keep syncing |
| A device holds a file it never synced at the path of a node that becomes included by a mark or by this device's own index, such as its own `.env`, its own build of `dist/app.js`, or the working copy of a second clone of the project | A file with the node's content is the node's local copy. Any other file is not an edit of the node and is not renamed: it stays at its name, the rules leave it out as before, and nothing of it is sent. The node stays excluded on this device, is not downloaded, and is reported as held by the local file, as §5.10 does for a discarded first-link folder. Once the local file is gone, moved away or holds the node's content, the node is included and downloads |

A device whose S held the node keeps §5.10's rule for an excluded node that becomes included: its local copy is the node's, and a different content is an edit.

## 6. Mixed versions

- **An old client** ignores field 16 and never sends the op. It leaves marked files out when it follows `.gitignore` files, as it does today, and its ops keep the marks (§2 rule 4).
- **A client whose store predates this RFC** dropped the field from every record it stored. When it first connects to a server that lists `git_tracked`, it reads the tree once (`ListTree`, spec v0 §7.5) and takes each node's mark from it, before it plans any effect for a node a `.gitignore` file matches.
- **An old server** doesn't list `git_tracked`, and a client then sends no `MarkGitTracked`. The device with the index still includes and uploads what its index lists (§4 step 5 reads the index directly), and the other devices leave those files out. Once the server lists the feature, §3 rule 1 sets the marks at the next scan.

## 7. Storage

- **Server.** A Space DO migration adds `git_tracked INTEGER NOT NULL DEFAULT 0` and `git_tracked_rev INTEGER NOT NULL DEFAULT 0` to the node table. The export needs no format change (§2 rule 7).
- **Client store.** Migrations are add-only: `migrations/<name>.sql`, `tests/upgrade_cases/<name>.rs` and `tests/migrations/<name>.rs`. One migration adds R's mark to the R row, adds a table holding the stored answers of §3 rule 2, and records that the store must read the tree once (§6).

## 8. What tests prove

- The order of §4: a marked file below a folder a `.gitignore` excludes is included and its unmarked siblings stay out; a `.koiignore` pattern, the local list and a built-in exclusion each beat a mark.
- §3: two indexes that disagree settle after one set; untracking clears, nothing is deleted, and the file stays on every disk; untracking while a set is in flight clears once the set applies; a move to an unlisted path clears; an unreadable index sends nothing; a mark lost to a server loss comes back; split, sparse and version 4 indexes are read.
- §3, sending: a Space DO without the code answers a marks-only commit `INVALID_ARGUMENT`, and the device keeps syncing with no halt; no mark is sent on a connection whose `Welcome` lacks the feature.
- §5: a device without the repository downloads a marked file; a local `.env` it never synced at that path stays at its name, is not sent, and the node is reported as held by it until the file is gone.
- §2: the op on a directory is `REJECTED`, on a trashed node `SUPERSEDED`; a changed mark advances `git_tracked_rev` only, so a no-op `Move` afterwards is not a user's move and a `Delete` with `requires` still applies; a conflict copy of a marked file is marked; an export round trip keeps the mark; digest v7 of a request without the op equals v6.
- §6: an old server leaves the device with the index syncing its tracked files; a store from before the migration reads the tree once and then includes marked files.
