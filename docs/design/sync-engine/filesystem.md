# 5. Local file system

## 5.1 Watcher events only choose what to rescan

Watcher events are hints about where to look, never facts about what changed. A lost or coalesced event (`inotify`'s `IN_Q_OVERFLOW`, FSEvents' `MustScanSubDirs`, `KernelDropped`, `UserDropped`) becomes a wider rescan, never an assumption. A rescan compares the disk with the last snapshot and reports created, modified, renamed and removed entries with their file identities.

The fs layer turns these patterns into the right observation:

- A rename over an existing file, as in an editor's atomic save, is a modification of the original, so the node keeps its ID and version history.
- A delete and re-create at the same path within the debounce window, with the same kind, is a modification.
- A swap or rotation through a third name (`mv a tmp; mv b a; mv tmp b`), of files or folders, is reported as renames: an entry whose identity was at another path before the rescan, while the identity its path held is at another path after it. The planner sends them through a pending name (§2.6).
- Ignored names (`.koidrive/`, the fixed platform files such as `.DS_Store` and `Thumbs.db`, and paths matched by `.koiignore`, §5.10) are invisible to the engine but still occupy names on disk. An effect that needs such a name fails its check and is reported as a problem. Platform files do not keep a folder from being removed (§5.2).

## 5.2 Effects never destroy user bytes

The driver gives three guarantees and claims nothing beyond them:

- Atomic names: every change to a name in the sync root is one `rename` or `mkdir` that never replaces an existing entry. A replacement is two such changes, so between steps 3 and 4 below the target name is briefly absent. Another process sees the old file, no file or the new file, never a partial one.
- No lost user bytes: an effect never unlinks or overwrites an entry. Anything it displaces is renamed into `.koidrive/quarantine/`, compared with what the engine expected, and then moved to the local trash when it is exactly the synced content, which the server also holds, or returned to the user. This holds across process crashes for the writers the contract below supports.
- Durability: the effect record and the ordering below guarantee process-crash recovery. Survival of a power loss depends on the file system and the drive honoring flushes and is not claimed beyond that.

### Required primitives

The driver needs a rename that fails instead of replacing: `renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)` on macOS. It probes for it in `.koidrive/` when a space is linked and on every start. A file system without it is refused: the space is not synced and a problem is reported. There is no check-then-`rename` fallback, and exchange renames are not used.

### Path resolution

- The driver holds a descriptor for the sync root and resolves every path from it one component at a time without following links: on Linux with `openat2` and `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, elsewhere with `openat(O_NOFOLLOW | O_DIRECTORY)` per component.
- Each directory's `(dev, ino)` is compared with L before use, and every rename is a `renameat` between directory descriptors. A parent replaced by a link or another directory after the scan fails the identity check.
- Immediately before each rename, the driver resolves the path again from the root and compares the result with the held descriptor's identity; a mismatch fails the effect. A move in the window between that check and the rename can only move the entry within the same file system, never destroy it.

### The effect record

Before an effect that creates, replaces, moves or removes anything reaches the driver, the engine persists its record in the transaction that emits it: `effect_id`, kind, the target as (parent directory identity, name), the expected state of the entry it displaces (identity, size, mtime, manifest digest), the new content for a write, and two private paths, `.koidrive/staging/<effect_id>` and `.koidrive/quarantine/<effect_id>`. Staging holds only bytes downloaded from the server and verified against their hashes; the driver never moves a user's file into staging. Quarantine holds displaced entries and is never swept.

### Replacing a file

`WriteFile` with `replaces` runs these steps:

1. Download chunks into the content-addressed chunk cache, verifying each chunk's SHA-256 (spec §10). Assemble the file at the staging path, verify the manifest digest, set the executable bit and flush the file.
2. Compare the target with the expected state. On a mismatch, stop with `EffectFailed`; nothing was displaced.
3. Rename the target to the quarantine path (no-replace). From here on, whatever the user had at the target, even a write made after step 2, is in quarantine and intact.
4. Compare the quarantined entry with the expected state, including its content digest.
   - On a match, rename staging to the target (no-replace). If the target name was taken meanwhile, leave the new entry alone and delete the staging file; the quarantined file is synced content and still goes through step 5, after which the effect reports `EffectFailed`.
   - On a mismatch, the user wrote between steps 2 and 3. Rename the quarantined entry back to the target (no-replace) and report `EffectFailed`. If the target name was taken again, the quarantined entry becomes a recovered file (below).
5. Move the quarantined entry, which equals the synced content, to `.koidrive/trash/` and report `EffectDone`.

### Removing, creating and moving

- Removing (`RemoveFile`, `RemoveDir`) is steps 2 to 5 without staging: quarantine, compare, then trash or put back. A directory must be empty once quarantined; an entry created in it meanwhile is a mismatch.
- A quarantined directory that holds nothing but regular files with the exact names of the fixed platform list (`.DS_Store`, `Thumbs.db`, `desktop.ini`, `Icon\r`) matches. The driver deletes those files, which scans leave out, and then moves the directory to the trash empty.
- Any other entry keeps the folder, including a file that `.koiignore` or the device-local list excludes: the effect fails, everything is put back, and the folder is reported as a problem that names those entries. The driver does not plan the removal again after a wait. The status names the entries until the effect planned for the folder changes or the folder restarts. A wait for another failed effect leaves it blocked, and a folder to remove that holds it waits behind it without keeping the folder from being up to date.
- Creating (`CreateDir`, `WriteFile` without `replaces`) is a no-replace `mkdir`, or a no-replace rename from staging.
- Moving is one no-replace rename after the source's identity check. A rename never destroys content, so a source the user changed after the check moves with its new content, and the next rescan reports that content as a local edit.

A recovered file is a quarantined entry that cannot go back to its name. The driver renames it into the same directory as `<stem> (recovered <YYYY-MM-DD>)<ext>` (no-replace, with a counter on collision) and reports it as an observation, so it becomes a new local file and is uploaded. If that directory no longer resolves to the one the effect expected, the recovered file goes to the sync root.

### Supported writers

The no-lost-bytes guarantee covers every write that reaches the file before step 4's comparison, and every write through a descriptor opened after the effect. That includes programs that save by writing a temporary file and renaming it into place, and programs that open, write and close. It does not cover a process that keeps a descriptor open across the effect and writes after step 4. Those writes go to the displaced file in the local trash and last only as long as its trash entry.

### The local trash

- `.koidrive/trash/` holds every entry an effect displaced for 7 days (`TRASH_RETENTION`), or less when the folder's size cap (`[trash]` in the driver config) evicts it earlier, oldest first.
- Before reclaiming a file, the collector compares its size and content with the values recorded when it entered the trash, which the trash keeps in its own index. A changed file becomes a recovered file at the sync root instead. A change of metadata alone, such as `chmod -R` or a copy of the folder to another disk, is not a change. The check is best effort: a write between the check and the unlink is lost.
- An empty folder takes no room under the cap, so only the period reclaims it.
- The server's trash and version history remain the primary way to recover a file.

### Batches

The engine plans up to 1,000 independent effects at a time, whose writes hold at most 64 MiB of content besides the first. An effect joins a batch only when every name it changes is under the root, outside the parked area and not a rules file (§5.10), and no other effect of the batch takes its name. A move of a folder, an effect on a parked entry and a write of a rules file are planned alone, and nothing is planned while a batch is in flight. Each effect keeps its own record, and one store transaction persists them all.

The driver downloads the distinct chunks of the batch's writes in one transfer, so a chunk that several files hold is fetched once. A chunk the server lost fails only the writes that list it. The driver then applies the batch in one section and acknowledges every outcome in one store transaction.

### Ordering

The driver applies effects in sections: it writes the root marker, carries out the effects, and acknowledges them in one store transaction. Every staged file is flushed before its section begins. After every rename or `mkdir`, the driver flushes the directories it changed, source first, since a cross-directory rename changes both.

On macOS `fsync` hands the data to the drive, which may write it late and out of order, so the driver also uses `F_BARRIERFSYNC` and `F_FULLFSYNC` ([fcntl(2)](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/fcntl.2)). The marker, like every file replaced through a temporary name, is flushed behind a barrier before its rename, and that barrier also orders every staged file ahead of the section's renames. Each displacing step (the rename into quarantine, the return of a quarantined entry, the move to the local trash) ends with a barrier. The section ends with one `F_FULLFSYNC` of the root, and the store acknowledges nothing before it. That flush reaches only the root's device, so a folder on another device, such as a volume mounted below the root, gets an `F_FULLFSYNC` after each change in it. Elsewhere every flush is an `fsync`.

Process-crash recovery needs only the order of the steps and the records.

## 5.3 Recovering an interrupted effect

After a crash the driver recovers the effects in flight, if any, in one section before it rescans. It inspects the target, the staging path and the quarantine path, and every combination has one safe action:

| Found | Meaning | Action |
|---|---|---|
| a quarantine entry | the effect reached step 3 | Compare the quarantined entry with the expected state, whatever the target holds now. On a match: when the staging file still exists and the target name is free, rename it into place; then move the entry to the trash and report `EffectDone`, or `EffectFailed` as overtaken if the staging file could not be placed. On a mismatch: rename it back when the target name is free, otherwise make it a recovered file, and report `EffectFailed` |
| no quarantine entry, no staging file, target in the effect's final state (the new content, the moved entry, or absent after a removal whose entry is in the local trash under the effect's ID) | the effect finished | Report `EffectDone` |
| no quarantine entry, no staging file, the replaced entry in the local trash under the effect's ID, and the target without the new content | the effect finished, and the user changed or removed the file it wrote | Report `EffectFailed` as overtaken |
| no quarantine entry, anything else | the effect stopped before step 3, or the user changed the target | Delete the staging file (server bytes, downloadable again) and report `EffectFailed`, so the next plan sees the current disk |

These rules complete the table:

- The second row judges the target by name and content alone, so the engine takes `EffectDone` from recovery only when L confirms that the entry found is the effect's own. An entry L holds as another node is that node's. A removal whose node L places elsewhere found the name empty because the user moved the node. In both cases the driver reports `EffectFailed`.
- A removal that finds its name empty also needs its entry in the local trash. Without one, the user moved or deleted the entry before the effect reached it, and recovery reports `EffectFailed` with the entry missing, so the rescan does not upload the moved entry as a new file.
- A write is overtaken whenever the entry it replaced matched and went to the local trash while its target does not hold the new content: the user changed or removed the written file, or took its name before it was placed. An overtaken write leaves the node out of S and L, so R's content is downloaded again and what the user left on disk is kept beside it as a new file. S never stays at the content the write replaced.
- In every row, quarantine is either moved to the trash after it is verified as synced content, or returned to the user.
- Recovery writes the effect's counter to the marker again before it looks, since the crash may have come before the marker write (§2.8). A crash during recovery repeats recovery, which is idempotent.
- Staging files with no effect record are deleted at startup; they hold only server bytes. Verified chunks stay in the cache, so a download resumes. Uploads resume through `PrepareUpload`, which answers `PRESENT` for chunks the server already has (spec §8 step 1).

## 5.4 Case sensitivity

The server enforces a unique `name_key` per directory on every platform (spec §6.1), while macOS APFS is usually case-insensitive and Linux file systems are case-sensitive.

- On Linux, `Report.txt` and `report.txt` in one folder cannot both sync. The one committed second gets a conflict name (R1), and the Linux device renames its local file to match.
- The engine decides whether a name is taken by `name_key`, never by asking the file system.
- A case-only rename must reach the engine as a rename; the fs layer recognizes it by file identity.
- A case-only or normalization-only rename the engine applies goes through `.koidrive/parked/` in two no-replace renames, because on a case-insensitive volume the source itself holds the target name.

## 5.5 Unicode normalization

L keeps the exact on-disk name, so paths resolve, and names are compared after NFC normalization, so an NFD name from HFS+ does not cause an endless rename. On Linux, two names that differ only in normalization collide in `name_key`, like a case collision. RFC 0001 §1 pins case folding and normalization to Unicode 18.0.0. The server and the engine both fold with CaseFolding 18.0.0; the server normalizes with its runtime's data and the engine with the `unicode-normalization` crate, which can lag that version (RFC 0001 §1 states the limitation).

## 5.6 Symbolic links, special files and hard links

Symbolic links, sockets, devices and FIFOs are not synced (spec §5.1) and are skipped by `core/fs`. The engine never follows a link, so nothing outside the root is read or written through one. A remote file whose name a skipped entry takes locally cannot be applied and is reported as a problem. Hard links sync as independent files.

## 5.7 Permissions and the executable bit

Only the executable bit is synced (spec §5.1), as part of `Content`. Owners, groups, ACLs, extended attributes and resource forks are not. A file the engine cannot read is not a deletion: it is reported as a problem and left out of planning until it becomes readable. A file the engine cannot write fails its effect and is reported.

## 5.8 Files that change while being chunked or uploaded

The fs driver records identity, size and mtime before reading a file and checks them again after chunking. If they changed, it discards the result and rescans after the file has been quiet for the debounce interval. Chunk bytes are read again on upload and must match their hash, which the server also checks (spec §8 step 2). If the file changed after chunking, the upload fails, the commit gets `MISSING_CHUNKS`, and §4.2 abandons it and re-plans from the newer observation. A file that never stays still is retried with backoff and reported as busy after a limit.

## 5.9 Very large directories and trees

The trees are indexed by `(parent, name)` and `(parent, name_key)`, and planning visits only dirty nodes. Commits are cut at 1,000 ops in dependency order, bootstrap reads `ListTree` page by page (spec §7.5), and the initial scan streams entries to the engine in batches. The engine holds the trees in memory and the store persists changes row by row.

## 5.10 `.koiignore`

Becoming ignored never deletes anything, on either side. An ignored path is outside the sync tree on the device that ignores it, and that device stops syncing it. The one exception is the ambiguous disappearance below, which the engine counts as a deletion until it opens its question.

### Rules files

- A `.koiignore` file can be in any directory of the space and uses `.gitignore` syntax: blank lines and `#` comments, `!` negation, a leading or middle `/` anchoring a pattern to the file's directory, a trailing `/` matching only directories, `*`, `?`, `[...]` and `**`. Its patterns apply to its directory's subtree. A deeper file's patterns take precedence over a shallower one's, and within a file the last matching pattern wins. A file cannot be re-included when one of its parent directories is excluded.
- Names are matched after NFC normalization and case folding (`name_key`, §5.5), so a pattern means the same on every volume. An unparsable line is ignored and reported as a problem on the `.koiignore` file.
- `.koiignore` files are ordinary synced files, so every device converges on the same rules. A `.koiignore` file in an included directory is never ignored itself. One inside an excluded subtree is excluded with it and not read.
- The built-in exclusions of §5.1 (`.koidrive/`, platform files) apply in addition and cannot be negated.
- A rules file is any file whose `name_key` is that of `.koiignore`, so `.KOIIGNORE` is one too, and scans hold each folder's rules under the `name_key` of its path: renaming a folder or its rules file only in case keeps the rules.

### Evaluating the rules

- The fs driver parses each `.koiignore` file it observes and passes the rules to the engine with the observation. The engine owns the matcher and applies the same rules to remote nodes; `core/fs` uses the same matcher to leave ignored paths out of scans and watchers. Rules come from the files on this device's disk.
- When a remote change creates or updates a `.koiignore` file, the planner applies that effect before any other effect in the same directory's subtree.
- After an effect that wrote, moved or removed a rules file, or that moved a folder, the driver asks for no further effect and no commit until a scan of the effect's paths has passed its rules. The engine holds each rules file by the path of its folder.
- A rules file that cannot be read (larger than 1 MiB, or unreadable) keeps the rules its folder had. When the folder had none, the scan reports the folder as unreadable and leaves what it holds as it was known, so nothing the file may exclude is uploaded.
- The engine plans no effect and no commit for a node that L, S or R places below a folder a scan reports unreadable, until a scan reports the folder's rules. It holds the folder by its node, found after the batch's changes at the path the scan reports, by exact name as L holds names: a folder renamed, also only in case, is the same folder, and two folders whose names differ only in case are two. The hold ends when L no longer holds the folder.
- Each scan reports, apart from the rules files that changed, every folder whose rules it established at the path where it listed the folder: it read the rules file, found none, or kept the rules an earlier scan read. The hold of the node at that path ends there.
- A folder that holds two rules files whose names differ only in case is reported unreadable until one is gone.
- Until the first scan of a watcher has settled every scope, a folder that is still pending is reported unreadable.
- The rules are not stored (§2.8). After a restart the engine plans no effect and no commit until the driver has passed it the device-local list and the rules files of a full scan.
- The first batch of each watcher lists every rules file its scan read, and the engine drops the rules of every folder it does not list.

### Excluded nodes

A node is excluded on this device when any of these matches the rules: its position in R, S or L; its place in L below its R parent, where an effect would write it; or its being held by R below a folder excluded here for any of these reasons.

- The engine keeps an excluded node's S entry, freezes it, and plans nothing for it in either direction: no ops, no effects. R keeps following the server.
- A node that L shows at an included position different from S's was moved by the user out of exclusion. It is not excluded; its move is planned against S like any local move, and it leaves exclusion once the result places it at an included position in R and S.
- A node whose local copy has not moved stays excluded, so a remote move into an ignored path never moves the local copy.
- Freezing has two limits. S still takes what L and R both hold for the node. And S gives up a position that neither L nor R holds any more: with the node in L and live in R, S takes R's position, or the local copy's when S cannot hold the node at R's, and keeps its content for comparison when the node is included again; otherwise S drops the node, which is then resolved like a node never synced.
- A folder that S drops gives up its S entry together with every node S holds below it, when all of them are ignored too.
- An excluded node is not a deletion and never enters the mass-delete ledger. It is ineligible for convergence (§3.4), and its local copy is left exactly as it is.

### Parking and exclusion

- An excluded node is never parked. Parking never changes whether a node is excluded: the position L keeps for a parked node counts as its position in L, for it and every node below it.
- A parked node leaves the parked directory for its position in R. It goes back to the position L keeps for it instead when R no longer holds it and it has a change of its own (a parked node whose only change is that R deleted it is removed), when the rules exclude it, when the local copy of an excluded node holds its name in R, or when it holds an edit and the rules exclude its R position.
- A parked node the rules exclude, whose local name the local copy of an excluded node holds, goes to its R position instead, unless it holds an edit and the rules exclude that position too.
- An edited node is never parked to make room for another node while the rules exclude its R position.
- A parked node holds no name on disk, so its S entry gives up its position as above even when another node has taken the old name.
- A parked node that can reach neither position stays parked and is reported as a problem once the effects have nothing left to do.
- A parked node whose only change is remote, and whose place in R the local copy of an excluded node holds, takes its R position as the one L keeps for it. S then follows R and frees the old name.
- A node whose place the local copy of an excluded node holds cannot be put there, and is not parked to make room for another node either. It stays where it is and is reported as a problem, and so is a node that waits for its name.

### Transitions

| Situation | What happens on the device with the rule |
|---|---|
| A synced node becomes ignored (a rule is added) | It becomes excluded. Nothing is deleted locally or remotely; other devices keep syncing it. Local edits made while it is ignored are not uploaded |
| An ignored node becomes included again (the rule is removed) | The driver rescans the path; the node re-enters L with its kept S entry, and §2.3 resolves it: an edit made while ignored is uploaded, a remote change is downloaded, both are a concurrent change. A path never synced appears as a new local node |
| A remote node matches a local rule | It is not downloaded and becomes excluded, with its whole subtree for a directory |
| The user moves a synced node into an ignored path | The fs layer reports the move from the rename (file identity), and the node becomes excluded at its last synced position: no op is sent. When events were lost, see below |
| The user moves an ignored entry into a synced path | A new local node, or, for an excluded node returning, a move of that node plus whatever edit it carries |
| A remote change moves a node into a path ignored here, or into a folder whose path here the rules exclude | The node becomes excluded and its local copy stays exactly where it is, with no effect of any kind. Deleting or editing it later sends nothing |
| A remote change trashes an excluded node | R follows; the local copy is left alone until the node is included again |
| The user deletes a synced folder that holds excluded nodes | The deletion is split: each topmost deleted node with no excluded node in its subtree gets its `Delete`, and the folder and the other ancestors of excluded nodes stay on the server and become excluded here, so they are not created again locally. When another device adds a node below such a folder that this device syncs, R7 brings the folder back. The guard counts the folder as one deletion with the affected files it removes |
| The person discards a first link's held folder (§6.4) that holds an entry the rules exclude | The folder can't go to the local trash with that entry in it, so it stays on disk where it is and becomes excluded here. It is not parked, and one parked before goes back to its place, so the entry stays where the user sees it. A server node whose name it takes is not downloaded and is reported as held by it. Once the rules let the entry in, the entry and the folder go to the local trash. Nothing is sent for either. When the user changes something in the folder, it is the user's again: S drops it with every excluded entry below it, including a server node the user moved in, and the folder is held again |

### Moves into ignored paths the rescan cannot see

When events were lost and a rescan cannot attribute a disappearance to a rename, the fs layer looks for the entry's identity in the ignored subtrees before it reports anything:

- The walk only lists and stats: no file reads, no watches. Each scan stats a bounded number of ignored entries, and an unfinished walk continues in the next scans.
- Until the walk decides an entry, the fs layer reports nothing for it. The entry leaves the scan's snapshot, which stays equal to the disk, and the walk holds it by identity at the path where L still has it, moved along by later renames. The next scan of that path reports it once the walk has decided, or as a move when its identity turns up at an included path.
- A file that takes the path while the walk holds the entry is held back with it. The scan that reports the held entry reports the file too: as a modification of the entry when the walk did not find it, and as the move followed by the new file when it did. A rename or an edit that takes the path makes the walk run to its end, so L hears of the held entry first.
- When a file takes the path in the same scan that moved the entry below a name the rules leave out, the scan does not walk for it, since a save by rename takes a path the same way. On macOS the fs layer asks the system for the path of the entry's identity (`fsgetpath`). When the previous scan saw no other link of the file, and that path is at or below a name the rules leave out and holds the file as recorded, with the same identity, size and modification time, it reports the move followed by the new file. A file that had another link, or that no scan has seen, cannot prove a move by its identity, since a save leaves the other link with that identity, so neither this lookup nor the search at the left-out names is made for it. On other platforms, and on macOS when the lookup misses or fails, the new file is a modification of the entry, as a save would be, so the moved content leaves the live tree and stays as the node's previous version on the server and in the ignored folder here.
- A watcher that starts from a snapshot it did not scan itself, after a restart from L or as the fallback watcher, holds the same way each entry whose path now holds another identity, on its first scan of each path. A reset of a scope to what L holds does the same for that scope. A folder the scan cannot read keeps this until a scan reads it, and a walk under way also lists every folder the rules leave out after it began.
- A held entry whose disappearance the scan cannot confirm, as below an unreadable folder, stays held. One whose path the rules now leave out is left to the walk once its folder can be read.
- The fs layer does not look for an entry that a rename in the same scan carries to an included path, so a link of that entry in an ignored folder does not count as a move.
- If the walk finds the identity, the node becomes excluded as for an observed move.
- A node found in an ignored folder that is itself no node keeps its L entry at the old path, and its record is stored (§2.8). When another entry takes that path, the node leaves L and the record keeps it with its file identity: the node stays excluded, nothing is planned for it, the server keeps it, and the arriving entry is created or moved under a conflict name.
- An entry with that node's identity that arrives at a synced path is the node placed again. The record ends when R no longer holds the node live.
- A folder the disk lost that L kept only for recorded nodes below it leaves L once it holds none of them, so the server removes it. A folder the user moved into an ignored folder is not such a folder and stays.

A miss proves nothing, since an editor that saved the moved file by temporary file and rename gave it a new identity. The rule for a miss:

- When the sync root holds any ignored entry, a miss makes the disappearance ambiguous: no `Delete` is sent, the node is held like a guard hold (§6.1) under a question (`QuestionKind::AmbiguousDisappearance`) that asks to delete the files everywhere or restore them here, and it stays on the server and on other devices until answered. An ignored subtree that cannot be listed counts as holding entries. `.koidrive/` is not an ignored entry for this rule; the walk still lists it, so a file a user moved into it is found.
- Only when the root holds no ignored entry is a miss a deletion.
- Inode reuse can make a deleted file look found. That errs toward keeping the node on the server.
- The engine does not open the ambiguous-disappearance question yet: `core/fs` reports every miss as a removal and the node is deleted like any other that disappeared. The server copy goes to the server trash, and the local file is still in the ignored folder. While misses count as deletions, the walk does not list `.koidrive/`, because the engine's own trash moves keep their identity there and each would be found.

Adding a rule, even `*`, excludes nodes and deletes nothing, so it never prompts the guard. Removing a rule can make many files appear at once; they are uploads. Only actual removals from disk count toward §6.1.

For every transition caused only by a rule, or by a move into an ignored path the fs layer identifies by the rename or by finding the identity, there are zero `Delete` ops and zero removal or trash effects.
