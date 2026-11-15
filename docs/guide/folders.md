# Folders

## Link a folder

```sh
koi folder link ~/Koidrive --space Koidrive
```

`--space` takes the space's name or its ID. If two spaces have the same name, use the ID. The folder must exist, and it may already hold files. The folder then syncs in the [background](install.md#background-sync), and `koi status` shows its state.

The first sync compares the folder with the space, path by path. Files that both hold alike stay as they are, and the space's other files download. The folder's other files and folders upload without a question. If the space already has a file with the same name and other content, the upload gets a [conflict name](conflicts.md#two-files-with-the-same-name), so both files are kept. A file or folder at a path the space's trash holds is held, with the new folder it is in, since the space records a deletion at that path.

If the folder synced with the space before, sync holds every file and folder that the space doesn't have or that differs from it, since another device may have deleted them since. Sync asks one question about the held files and uploads none of them until you answer it with `koi resolve`:

- `publish` uploads the held files. If the space already has a file with the same name, the upload gets a [conflict name](conflicts.md#two-files-with-the-same-name).
- `discard` moves the held files to the [local trash](sync-rules.md#the-local-trash) and downloads the space's files at their paths.

[The first link](sync-rules.md#the-first-link-compares-the-folder-with-the-space) lists exactly what is held, and [which folders Koidrive refuses](sync-rules.md#koidrive-refuses-some-folders).

## Unlink a folder

```sh
koi folder unlink ~/Koidrive
```

Unlinking keeps every file. It also keeps the folder's `.koidrive/` folder, whose local trash may still hold files that sync replaced. Delete `.koidrive/` yourself once you no longer need them.

## Leave files out with .koiignore

Put a `.koiignore` file in any folder of a space. It uses [`.gitignore` syntax](https://git-scm.com/docs/gitignore#_pattern_format), and its patterns apply to its folder and everything below it:

```gitignore
# Build output and disk images
build/
*.iso
!keep.iso
```

`.koiignore` files sync like other files, so every device follows the same rules. Ignoring a path never deletes it: the files stay on this device's disk, on the server and on the other devices. [.koiignore rules](sync-rules.md#koiignore-rules) has the details.

## Sync only part of a space on this device

Add rules for this device alone to `~/.config/koidrive/config.toml`, or `$XDG_CONFIG_HOME/koidrive/config.toml` when that variable is set. They apply to every folder this device links:

```toml
[sync]
include = ["Projects/"]
exclude = ["*.iso", "build/"]
```

With `include`, the device syncs only the listed paths, what is under them, and the folders that lead to them. `exclude` takes `.koiignore` patterns and narrows that further. A path is left out when the device's rules or any `.koiignore` file leave it out, so a `.koiignore` file can't widen what this device syncs. Restart the daemon after you change the file.

These rules save disk space and bandwidth. They don't hide anything: the server still sends this device the names of every file in the space, and the device's credential can read every file.
