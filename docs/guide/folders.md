# Folders

## Link a folder

```sh
koi folder link ~/Koidrive --space Koidrive
```

`--space` takes the space's name or its ID. If two spaces have the same name, use the ID. The folder must exist, and it may already hold files. The folder then syncs in the [background](install.md#background-sync), and `koi status` shows its state.

The first sync compares the folder with the space, path by path. Files that are the same in both stay as they are, and the space's other files download. The folder's other files and folders upload without a question. If the space already has a file with the same name and other content, the upload gets a [conflict name](conflicts.md#two-files-with-the-same-name), so Koidrive keeps both files. Sync holds a file or folder at a path that the space's trash holds, along with the new folder that contains it, because the space records a deletion at that path.

If the folder synced with the space before, sync holds every file and folder that the space doesn't have or that differs from it, because another device may have deleted them in the meantime. Sync asks one question about the held files and uploads none of them until you answer it with `koi resolve`:

- `publish` uploads the held files. If the space already has a file with the same name, the upload gets a [conflict name](conflicts.md#two-files-with-the-same-name).
- `discard` moves the held files to the [local trash](sync-rules.md#the-local-trash) and downloads the space's files at their paths.

[The first link](sync-rules.md#the-first-link-compares-the-folder-with-the-space) lists exactly what sync holds. [Koidrive refuses some folders](sync-rules.md#koidrive-refuses-some-folders) lists the folders it refuses.

## Unlink a folder

```sh
koi folder unlink ~/Koidrive
```

`koi folder unlink` keeps every file. It also keeps the folder's `.koidrive/` folder, whose local trash may still hold files that sync replaced. Delete `.koidrive/` yourself once you no longer need those files.

## Leave files out with .koiignore

Put a `.koiignore` file in any folder of a space. It uses [`.gitignore` syntax](https://git-scm.com/docs/gitignore#_pattern_format), and its patterns apply to its folder and everything below it:

```gitignore
# Build output and disk images
build/
*.iso
!keep.iso
```

`.koiignore` files sync like other files, so every device follows the same rules. Ignoring a path never deletes it. The files stay on this device's disk, on the server and on the other devices. [.koiignore rules](sync-rules.md#koiignore-rules) has the details.

## Files that .gitignore files exclude

Koidrive follows `.gitignore` files too, so `node_modules/`, build output and everything else a project's `.gitignore` excludes stay on this device, while the `.gitignore` and `.koiignore` files in a synced folder always sync, even when a pattern such as `*` matches them. Each `.gitignore` file applies to its folder and everything below it, including nested repositories and submodules. Git in a nested repository doesn't read the `.gitignore` files outside it, so Koidrive can leave out a file there that git doesn't ignore.

`.koiignore` files decide first. Koidrive reads the `.gitignore` files only for a path that no `.koiignore` pattern matches. To sync the `.env` files that a `.gitignore` file excludes, add this line to a `.koiignore` file:

```gitignore
!.env
```

This line doesn't reach into a folder that a `.gitignore` file excludes. If `.gitignore` excludes `secrets/`, add `!secrets/` to sync `secrets/.env`. That line also syncs the other files in `secrets/` that no pattern excludes. It matches a folder named `secrets` at any depth, and `!/secrets/` matches only the one in the `.koiignore` file's own folder.

Koidrive doesn't read `.git/info/exclude` or git's global excludes file. They hold one person's choices for one clone.

Koidrive leaves every `.git` folder out, whatever the `.gitignore` setting, because syncing a repository's internals can corrupt it. To sync them anyway, add `!.git/` to a `.koiignore` file.

A file that git already tracks stays on this device when a `.gitignore` pattern matches it, even though git keeps tracking it.

Each device has its own setting. To sync what `.gitignore` files exclude on this device:

```sh
koi gitignore off
```

On a terminal, the command shows how many files would start syncing in each folder, names any folder it can't read, and asks first. Without a terminal, add `--yes`. The command writes `gitignore = false` under `[sync]` in `config.toml`. `koi gitignore status` shows the setting and counts the files, and `koi gitignore on` turns it back on. Switching it either way deletes nothing. Files that a `.gitignore` file starts to exclude stay on this device's disk, on the server and on the other devices.

## Sync only part of a space on this device

Add rules for this device alone to `~/.config/koidrive/config.toml`, or `$XDG_CONFIG_HOME/koidrive/config.toml` when that variable is set. They apply to every folder this device links:

```toml
[sync]
include = ["Projects/"]
exclude = ["*.iso", "build/"]
```

With `include`, the device syncs only the listed paths, what is under them, and the folders that lead to them. `exclude` takes `.koiignore` patterns and narrows that further. The device leaves a path out when its own rules or any `.koiignore` file leave it out, so a `.koiignore` file can't widen what this device syncs. Restart the daemon after you change the file.

These rules save disk space and bandwidth. They don't hide anything. The server still sends this device the names of every file in the space, and the device's credential can read every file.
