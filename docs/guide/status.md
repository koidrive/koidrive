# Status and problems

## Read the status

```sh
koi status
```

```
Device: laptop (019a...) on https://koidrive.alice.workers.dev
Daemon: running
Background service: running (koidrive.service)
/home/alice/Koidrive  space Koidrive (019a...)  UpToDate  seq 1204 of 1204, 3120 remote nodes, 3118 local entries, 0 need you
```

Each linked folder has one line. `UpToDate` means the folder has the server's latest changes. A line under the folder names a problem or a retry, and the folder's pending decisions follow. [Status reference](status-reference.md) explains every state, wait and decision. For scripts, `koi status --check` exits with a status that says whether anything needs you, as [Agents and scripts](agents.md#check-health-with-koi-status---check) describes.

## Pause sync

```sh
koi pause
koi resume
```

`koi pause` stops every folder on this device until `koi resume`, across restarts of the daemon and the machine. The folders stay linked, and changes on this device wait on disk. `koi status` shows `Sync: paused`. Only `koi resume` ends a pause.

## Answer a decision

Some changes wait for you, such as a large delete or a conflict. `koi status` shows each pending decision with the command for each answer. To list and answer them:

```sh
koi resolve
koi resolve <id> <answer>
```

The first command lists every pending decision with the answers it accepts. The second gives one answer, such as `delete` or `restore`. [Decisions](status-reference.md#decisions) explains what each answer does.

## After a rewind

When the space is rewound on the server, each folder follows the rewound space on its own. Files this device created or changed since the restore point wait in decisions until you answer. `koi resolve` lists them, and on a Mac they appear in **Needs You**. [A rewind opens decisions](status-reference.md#a-rewind-opens-decisions) lists each decision and its answers.

## Restart a halted folder

Fix what the problem line names, then:

```sh
koi folder restart ~/Koidrive
```

This stops the folder and syncs it again from the start. A folder that was moved away, or whose root check fails, syncs again on its own once its path leads to the same folder again. If the daemon isn't running, run `koi service start`.

A folder whose sync database can't be opened halts, and its files stay as they are. Run `koi folder restart` to try again, or unlink the folder and link it again. If `koi status` can't name the folder's path, it shows `(path unknown)` and the space's id instead. Remove that folder's state with `koi folder unlink --space <space id>`, then link the folder again.

## Send refused changes again

The folder halts when the server refuses its changes for a reason that retrying won't fix, such as a device without access to the space. The problem line names the cause and what to run once it is fixed. After the daemon restarts, the problem line gives only the server's numeric code, such as `code 2` when the device lacks access to the space or `code 3` when the server found the changes invalid. Run one of these two commands, not both.

For a cause you fixed on the server, such as access restored to this device, send the changes again as they were:

```sh
koi folder retry ~/Koidrive --resend
```

For a cause fixed on this device, plan the changes again into a new commit:

```sh
koi folder retry ~/Koidrive --replan
```

If the fix is a newer `koi`, install it and restart the daemon first, because `folder retry` runs in the daemon. When the problem says the space's owner must confirm the changes, `folder retry` can't help, because Koidrive has no way to ask for that confirmation.

## Write a file for a problem report

`koi diagnose` writes `koi-diagnostics-<time>.txt` in the current folder and prints its path. The file holds the version of koi, the system, `koi status --json` and the last 200 lines of the daemon's log, with paths, names, the server's address and tokens replaced by placeholders such as `<folder-1>`. Read the file before you attach it to an [issue](https://github.com/koidrive/koidrive/issues).
