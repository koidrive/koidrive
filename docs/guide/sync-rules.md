# What syncs

This page describes what Koidrive syncs in a linked folder and the settings that change it. [Folders](folders.md) covers linking a folder and leaving files out.

## Koidrive syncs folders and regular files

Koidrive syncs folders and regular files: a file's bytes and whether it is executable.

It doesn't sync:

- symbolic links, sockets, FIFOs and device files. Koidrive skips them and never follows a symbolic link inside a linked folder.
- permissions other than the executable bit, the owner and group, ACLs and extended attributes such as macOS Finder tags.
- hard links as links. Each name of a hard-linked file syncs as a separate file.
- the platform files `.DS_Store`, `Thumbs.db`, `desktop.ini` and `Icon\r`, and the `.koidrive/` folder at the top of a linked folder. No rule can bring them back. A `.koidrive` folder deeper down is an ordinary folder and syncs.

When sync removes a folder that another device deleted, it deletes these platform files in it too, so that the folder can go. If the folder still holds anything else that Koidrive doesn't sync, such as a file your rules leave out or a symbolic link, Koidrive keeps the folder with those entries, and the status names them. Remove them and run `koi folder restart <folder>`, and the folder goes.

Names compare without regard to case or Unicode normalization. Two names in one folder that differ only in case, such as `Report.txt` and `report.txt`, can exist on a Linux disk but not in a space, so the second to arrive is renamed as a [conflict](conflicts.md#two-files-with-the-same-name). A name is at most 255 bytes, and a path at most 256 folders deep.

## The first link compares the folder with the space

If the space holds no live files or folders when sync first reads it, the folder's files upload as they are, and files another device adds after that sync as usual. Otherwise the first sync compares the folder with the space, path by path. Here names must match exactly, including case and Unicode normalization, so `Report.txt` doesn't match `report.txt`:

- A folder that the space also holds at the same path stays, and its contents are compared one by one.
- A file that the space holds at the same path, with the same content and the same executable bit, stays as it is.
- Files and folders that only the space holds are downloaded, except where a held entry has the same name.
- Entries that `.koiignore` or this device's rules leave out are skipped.

Every other file and folder is held, and sync asks one question about them, which takes `publish` or `discard`. A file you create later inside a held folder joins the question. Files you create anywhere else sync as usual. If the held entries change before sync applies your answer, sync replaces the question with a new one that has a new ID and drops your answer. An entry you edit or move stays held, and one you delete leaves the question. Run `koi resolve` again and answer the new question.

## Koidrive refuses some folders

A space syncs to at most one folder on each device. Koidrive refuses a folder that:

- is inside another linked folder, or holds one.
- holds Koidrive's own state or configuration folder, or sits inside it.
- another device linked and hasn't unlinked.
- is on a file system without a rename that refuses to replace its target, which Koidrive needs to write files safely.

## .koiignore rules

- A deeper `.koiignore` takes precedence over a shallower one, and within a file the last matching pattern wins.
- Patterns match names regardless of case and Unicode normalization, so a pattern means the same on every file system. `.KOIIGNORE` is a rules file too.
- As in git, a file can't be brought back with `!` when a folder above it is ignored, and a `.koiignore` inside an ignored folder has no effect.
- The device that ignores a path stops syncing it. A new file in an ignored path stays on the device where you created it.

If you move a synced file into an ignored folder while the daemon isn't running, and Koidrive can't tell afterwards that it was moved, it counts the file as deleted. The server keeps it in the [trash](history.md) for 30 days.

## The local trash

When sync replaces or removes a file in a linked folder, Koidrive moves the old file to `.koidrive/trash/` in that folder instead of deleting it. A file stays there for 7 days. While the trash holds more than 2048 MB, the oldest files leave first. A megabyte here is 1,000,000 bytes. Set another size in `config.toml`:

```toml
[trash]
max_size_mb = 4096
```

The local trash is separate from the space's trash on the server, which [Trash, versions and restore](history.md) covers.

## Uploads at once

Koidrive uploads up to 4 chunks of file content at once. The limit counts across all folders this profile links. Set another number from 1 to 16 in `config.toml`. With 1, Koidrive sends one chunk at a time:

```toml
[upload]
concurrency = 1
```

Restart the daemon after you change it.
