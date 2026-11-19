# RFC 0032: Files git tracks sync when a .gitignore file matches them

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §8, RFC 0022 §2 and the sync-engine design §5.10. Adds a field to `NodeState`, an op to `Op` and the feature `git_tracked`.

Git never ignores a file it tracks: after `git add -f dist/app.js` the file stays in the repository although `.gitignore` lists `dist/`. A device that follows `.gitignore` files leaves such a file out (design §5.10), so it never reaches the server. Reading the git index on the device that holds the repository would upload it, but the other devices hold no `.git` folder, since §5.10 leaves it out, so their `.gitignore` files still match the file and they don't download it. This RFC records on the server that git tracks the file, so every device includes it.

A bare section number such as §2 refers to this RFC.

## 1. Messages

New fields only. No field is removed or renumbered.

```proto
message NodeState {
  // True when a device's git index listed this file (RFC 0032). Only a file carries it.
  bool git_tracked = 16;
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

1. `MarkGitTracked` sets `git_tracked` of a live file to `tracked`. It changes no place, status or content. When the value changes, `meta_rev` takes the commit's seq, so an export restore (RFC 0004 §5) takes the newer record; `meta_edit_rev`, `content_rev` and `edit_rev` keep their values. A value equal to the stored one is `APPLIED` and writes no change record.
2. A trashed or purged node answers `SUPERSEDED`. A directory answers `REJECTED`.
3. The op records no intent (RFC 0029): the server ignores its `intent` and `fields`, and `FieldIntents` gains no field. §3 rule 1 restores a mark after a server loss instead.
4. No other op changes the mark. A `Move`, `UpdateContent`, `Delete` or `Restore` of the node keeps it. A node the server creates, by a create, a conflict copy or a restore to a new node, starts without it. A purge removes it with the node.
5. Any principal that may commit to the space may send the op.
6. The request digest (RFC 0006 §8) becomes digest v7: digest v6's projection (RFC 0031) plus `Op` field 9 and `MarkGitTracked`. A request without the op has the same v7 digest as v6.
7. `NodeSnapshot.node` carries the field, so an export keeps the marks.
8. While `git_tracked` is off (RFC 0022 §3 rule 4), the op answers `REJECTED` and the commit's other ops apply. Stored marks stay and are still sent: a mark only makes a device include a file that git keeps.

## 3. Who sets and clears a mark

A **work tree** is a folder of the space that holds an entry named `.git`: a folder, which is its git directory, or a file whose `gitdir:` line names one, as in a linked worktree or a submodule. Its **index** is the file `index` in that git directory. A path belongs to the deepest work tree above it. A device **reads** a work tree's index when the git directory is on its disk. An index lists a path when it holds a stage-0 entry for it whose mode is a regular file (`100644` or `100755`). Paths are compared by `name_key` (design §5.5), as the matcher compares names.

A device that reads the index of a work tree applies these rules to the file nodes L holds in it, whether or not the device follows `.gitignore` files:

1. **Set.** For a node at a position that a `.gitignore` file matches: while its index lists the path and R holds the node without a mark, it sends `MarkGitTracked` with `tracked` true. A mark that a server loss, an export restore or another device took away is therefore set again. Files no `.gitignore` file matches need no mark, so a large repository sends no op for them.
2. **Clear.** For any node R marks, wherever it is: when its index stops listing the node, it sends `tracked` false. The device stores, for each node R marks, whether its index listed the node at the last read, and clears only on its own change from listed to not listed. A node that moves to a path its index doesn't list counts as such a change. A mark is cleared this way even while no `.gitignore` file matches the file, so a pattern added later doesn't bring back a file git no longer tracks.
3. **Unreadable index.** When the index is missing, being replaced, or uses a version or extension the device can't read, the device keeps the answers it read last and sends nothing for that work tree. Before its first read, it holds no answers.
4. The device reads the index again when its size or modification time changes. It checks on every scan of the work tree and on a watcher event for the index file.

A device that reads no index for a path never sends the op for it.

What follows from these rules:

- **Two devices with different indexes**, such as two clones of one project linked to one space, one behind the other: a file that either index lists gets the mark. A device clears it only when its own index stops listing the file, and a device whose index still lists it sets it again. So the mark is the union of the indexes. It settles after one set and never alternates. When the clone that is behind catches up and stops listing the file, it clears the mark too.
- **The file is untracked** (`git rm --cached`): the device clears the mark. Every device that follows `.gitignore` files then leaves the file out. Nothing is deleted on any side (design §5.10, Transitions).
- **Another device moves or renames a marked file:** the node keeps its mark until a device that reads the index sees that git doesn't list the new path and clears it, which is what `git status` reports there.
- **The repository is removed** (its `.git` is deleted, or the device that holds it unlinks the folder or is removed): no device reads an index for those paths, so the marks stay and the files keep syncing. A `.koiignore` pattern still leaves any of them out (§4).
- **A clear that a server loss took away** is not sent again, since the index no longer changes. The file keeps syncing until git tracks and untracks it again or a `.koiignore` pattern leaves it out. Losses fall on the side of syncing a file, never of dropping one.

## 4. Which rule decides

For a path, a device asks in this order, and the first that decides wins:

1. The built-in exclusions.
2. The device-local list.
3. The `.koiignore` files.
4. The `.git` default.
5. **Git's index.** A file whose node R marks, or that an index this device reads lists, is included. A folder on the way to such a file is included for it. When a `.gitignore` file excludes that folder, the folder's other entries stay out, with the folder's reason, as git keeps the untracked files of an ignored folder out.
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
| A device holds a file it never synced at the path of a node that becomes included by a mark, such as its own build of `dist/app.js` | The local file is not an edit of the node. A file with the node's content is its local copy. Otherwise the node downloads at its name, and the local file stays on this device beside it under a conflict name, which its folder's rules leave out. Nothing of the local file is sent |

A device whose S held the node keeps §5.10's rule for an excluded node that becomes included: its local copy is the node's, and a different content is an edit.

## 6. Mixed versions

- **An old client** ignores field 16 and never sends the op. It leaves marked files out when it follows `.gitignore` files, as it does today, and its ops keep the marks (§2 rule 4).
- **A client whose store predates this RFC** dropped the field from every record it stored. When it first connects to a server that lists `git_tracked`, it reads the tree once (`ListTree`, spec v0 §7.5) and takes each node's mark from it, before it plans any effect for a node a `.gitignore` file matches.
- **An old server** doesn't list `git_tracked`, and a client then sends no `MarkGitTracked`. The device with the index still includes and uploads what its index lists (§4 step 5 reads the index directly), and the other devices leave those files out. Once the server lists the feature, §3 rule 1 sets the marks at the next scan.

## 7. Storage

- **Server.** A Space DO migration adds `git_tracked INTEGER NOT NULL DEFAULT 0` to the node table. The export needs no format change (§2 rule 7).
- **Client store.** Migrations are add-only: `migrations/<name>.sql`, `tests/upgrade_cases/<name>.rs` and `tests/migrations/<name>.rs`. One migration adds R's mark to the R row, adds a table holding, per node, whether this device's index listed it at the last read (§3 rule 2), and records that the store must read the tree once (§6).

## 8. What tests prove

- The order of §4: a marked file below a folder a `.gitignore` excludes is included and its unmarked siblings stay out; a `.koiignore` pattern, the local list and a built-in exclusion each beat a mark.
- §3: two indexes that disagree settle after one set; untracking clears, nothing is deleted, and the file stays on every disk; a move to an unlisted path clears; an unreadable index sends nothing; a mark lost to a server loss comes back.
- §5: a device without the repository downloads a marked file, and a local file it never synced at that path stays beside it and is not sent.
- §2: the op on a directory is `REJECTED`, on a trashed node `SUPERSEDED`; a changed mark advances `meta_rev` only; an export round trip keeps the mark; digest v7 of a request without the op equals v6.
- §6: an old server leaves the device with the index syncing its tracked files; a store from before the migration reads the tree once and then includes marked files.
