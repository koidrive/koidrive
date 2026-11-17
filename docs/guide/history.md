# Trash, versions and restore

The server keeps what you delete and the earlier contents of files you change, for 30 days each:

- A deleted file or folder stays in the space's trash for 30 days.
- When a file's content changes, the earlier version stays for 30 days after it was replaced. The current version of a file never expires.

After that, the server removes them the next time it cleans up. These commands read the server, so the device must be online. They don't need the daemon.

## See what is in the trash

```sh
koi history trash ~/Koidrive
```

This lists what was deleted in the linked folder and is still in the trash, with the size of each item. A deleted folder is one line, with the number of files and bytes that went to the trash with it. Name a folder inside a linked folder to list only what was deleted under it. The command lists a folder deleted as a whole only at its own path. For example, if `notes/` went to the trash with everything in it, `koi history trash ~/Koidrive/notes/drafts` lists nothing, and `koi history trash ~/Koidrive` shows `notes/`. Without a path, the command uses the current folder. `--json` prints each item with its node ID.

## See the versions of a file

```sh
koi history versions ~/Koidrive/notes.txt
```

```
notes.txt: 2 versions, newest first
current       20 B  2 h ago         019a...e1
              10 B  1 d ago         019a...7c
```

The last column is the version ID. `--json` prints each version with its size, the device that wrote it and when.

## Restore a deleted file or folder

```sh
koi history restore ~/Koidrive/notes/old.txt
```

Name the path the file or folder had. Koidrive finds the deletion in the trash and brings it back where it was, with its versions. A folder comes back with everything that went to the trash with it. A file that went to the trash with its folder can come back alone. The folders above it come back too, without their other files.

When several deletions had the same path, the command lists them and asks you to pick one with `--node <id>`. `koi history trash --json` shows the node IDs.

## Restore an earlier version

```sh
koi history restore ~/Koidrive/notes.txt --version <version-id>
```

Take the full version ID from the last column of `koi history versions`. The old content becomes the file's current content, as a new version. The content it replaces stays as an earlier version for 30 days.

## Follow a restore

`koi history restore` checks the request against the server, records it, and returns the request's ID. The daemon sends it with the folder's next change to the server, after any changes to the same file that this device hasn't sent yet. The restored file then arrives in the folder like a change from another device. If the daemon isn't running, the request waits until the daemon starts.

```sh
koi history requests ~/Koidrive
```

This lists the restores asked for in the folder's space and how each went:

| Result | Meaning |
|---|---|
| restored | The file or folder is back |
| restored as `<name>`, since its name was taken | Something else now has its name, so it came back under a conflict name |
| restored as a new file `<name>`, since another device changed the file meanwhile | The earlier version came back as a [conflict copy](conflicts.md#two-edits-of-one-file) next to the file |
| refused | The server said why. For example, the server deleted the item for good after 30 days |
| not sent | It brings back more files and folders than one change can hold |
| pending | It hasn't been sent yet. The list says what the folder is waiting for, such as a stopped daemon or a [problem](status.md) |

## See what each space stores

```sh
koi space usage
```

This shows, for each space, the bytes in live files, in the trash and in old versions, then the unreferenced bytes and the total. Unreferenced chunks are uploads not committed yet and chunks that wait out a grace period before the server deletes them. A chunk shared by several files counts once.
