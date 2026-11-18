# Status reference

This page explains what `koi status` and `koi resolve` show. [Status and problems](status.md) covers what to do about it.

## Folder states

| State | Meaning |
|---|---|
| `Stopped` | The daemon isn't running, or sync is paused. The `Background service` and `Sync` lines say which |
| `Connecting` | Opening a session with the server |
| `Bootstrapping` | Reading the space's whole file tree, the first time or after a long time offline |
| `Syncing` | Sending this device's changes to the server, or applying changes from other devices. `--json` calls it `catching_up` |
| `UpToDate` | The folder has the server's latest changes, and new ones arrive as they happen. Items that wait on a problem stay out of sync, and the line below names them |
| `Offline` | The server can't be reached. The folder keeps trying |
| `Halted` | A problem stopped the folder. The line below says which |

When a newer release is out, an `Update:` line after `Background service` names it and the command that installs it, `koi update`. [Updates](service.md#updates) describes the check behind it.

While a folder bootstraps or syncs, a line under it shows the files it still sends or fetches and how many bytes are done:

```
/home/alice/Koidrive  space Koidrive (019a...)  Syncing  seq 1204 of 1204, 3120 remote nodes, 3240 local entries, 0 need you
  120 files to upload; 310.5 MB of 1.2 GB; now Photos/IMG_0412.HEIC
```

If Koidrive finds another file meanwhile, the numbers grow at once. The bytes done never go backwards, and content the server holds already counts as done without being sent. `--json` returns the same numbers under `progress`.

`seq` counts the changes the space has had: how many this device has applied, and how many the server has. A second line under the folder shows a problem, or the last failure the folder is retrying. Below that come the folder's pending decisions. `--json` returns all of it.

A pause holds until `koi resume`. Without a running daemon, it takes effect when the daemon next starts. `koi service start` and the commands that link folders leave a pause as it is.

## The server can put calls off

Sometimes the server asks the device to wait, or a call keeps failing for a while. The folder keeps retrying. The status shows the failure and the time of the next try once a call has failed for a minute. When the server asks for a wait of a minute or more, the status shows the wait and the time of the next try at once:

```
  uploading a file's content failed: ...; trying again at 14:05 UTC
```

### Quota waits

On the Workers Free plan, Cloudflare stops the server's Durable Objects for the rest of the day once they use up a daily allowance (see [Limits](limits.md)). The server then answers that it reached a daily limit, and the folder waits until the next 00:00 UTC, when the allowances reset:

```
  the server did not take a commit: QUOTA_EXCEEDED: the server reached a Workers Free daily limit; it resets at 00:00 UTC; trying again at 00:00 UTC
```

Nothing is lost while it waits. Changes on this device stay on disk and go out after the reset. In the first 30 minutes after midnight UTC the folder tries again every 5 minutes, because these errors can outlast the reset by a few minutes.

When a space's storage on the server is full, the server answers "the server's storage is full", and the folder retries on the same schedule. Full storage doesn't clear at midnight. On the Workers Free plan each space can store 1 GB of file tree, history and change log, and the Workers Paid plan raises that to 10 GB ([Durable Objects limits](https://developers.cloudflare.com/durable-objects/platform/limits/)). File contents are in R2 and don't count toward it.

## Decisions

Some changes wait for you to decide. `koi status` shows each pending decision under its folder. It shows what the decision holds, and for each answer what the answer does and the command that gives it:

```
/home/alice/Koidrive  space Koidrive (019a...)  UpToDate  seq 1210 of 1210, 3120 remote nodes, 2002 local entries, 1 needs you
  019b...: held 1 deletions that would remove 1116 of 3118 files, more than half of the space in 10 minutes; first path: Photos
  Delete these files on the server and other devices:
    koi resolve 019b... delete
  Download these files to this device again:
    koi resolve 019b... restore
```

While a decision is pending and the daemon runs, every other command ends with a line on stderr that says so. With `--json`, the envelope lists them under `decisions` instead. The daemon shows a desktop notification when a new decision opens. On Linux it uses the notification service of the desktop session, and on macOS it uses Notification Center. A daemon started without a desktop session, such as a system service or one started over SSH, shows none.

### A large delete waits for an answer

Sync asks before it deletes many files at once. It holds this device's deletions when, within 10 minutes, they number more than 1,000, or they would remove more than half of the files of a space that has at least 20. A removed folder counts as one deletion of all the files it held. Sync then holds every deletion it has not sent yet and waits for one decision. Until you answer, the files stay on the server and on your other devices, and every other change keeps syncing, also after a restart.

- `delete` deletes the held files on the server and on every device.
- `restore` downloads them to this device again.

`koi resolve` lists the first 20 held files and folders, and `--json` lists all of them. If the held deletions change before you answer, for example because you put a file back or deleted more, a new decision with a new ID replaces the old one. Sync keeps holding what is left until you answer, even when it no longer crosses a limit. An answer covers only the files its decision listed. If you deleted more before sync took your answer, it asks again.

### A large delete from another device opens a notice

Sync applies deletions from your other devices here without asking, and moves the removed files to this folder's [local trash](sync-rules.md#the-local-trash). When they cross the same limits, `koi status` and `koi resolve` show a notice once enough files are removed. The notice can appear before a removed folder is gone. Its counts grow while more deletions arrive, and it waits until you answer `dismiss`, which only closes it. The rest of a deletion it counted, such as the rest of that folder, opens no new notice. To bring the files back, [restore](history.md#restore-a-deleted-file-or-folder) them from the space's trash.

### The first link can hold files

Sync also asks after the first link of a folder when the comparison holds some of the folder's files. The comparison holds a file at a name the space's trash holds. If the folder synced with the space before, the comparison also holds every entry the space doesn't have at the same path, and every file whose content differs from the space's. [The first link](sync-rules.md#the-first-link-compares-the-folder-with-the-space) describes the comparison. `koi status` and `koi resolve` show that decision as "files on this device that the space does not hold", and it takes `publish` or `discard`.

### A rewind opens decisions

When the space is rewound on the server, the folder reads the server's tree again and follows it. What it can't settle alone waits in one decision per group:

| `koi status` shows | Answers |
|---|---|
| files created after the restore point, which the restore removed from the server | `keep` puts these files back on the server. `discard` moves this device's copies to the local trash and follows the server. |
| files changed on this device since it last synced | `publish` sends this device's version to the server. `discard` moves this device's copies to the local trash and follows the server. |
| files on this device that the space does not hold | `publish` sends this device's version to the server. `discard` moves this device's copies to the local trash and follows the server. |
| files deleted on this device that the server still holds | `delete` deletes these files on the server and other devices. `restore` downloads these files to this device again. |
| restores asked for before recovery began, which the server does not show | `repeat` sends the request again. `cancel` drops the request. |

Following the server downloads the server's version of a file when the space holds the file outside its trash. Otherwise this device's copy goes to the local trash.

### A conflict opens a decision

When two devices change the same file, the server keeps both changes as two files (see [Conflicts](conflicts.md)). The device whose change got the conflict name also opens a decision. `koi status` and `koi resolve` show both files and the name of the device that wrote each one:

```
  019c...: Reports/report.txt has a conflict copy
    Original: Reports/report.txt, written by desktop
    Conflict copy: 'Reports/report (conflict from laptop 2026-11-10).txt', written by laptop (this device)
  Keep Reports/report.txt and move 'Reports/report (conflict from laptop 2026-11-10).txt' to the trash:
    koi resolve 019c... keep-node:019d...
  Give Reports/report.txt the content of 'Reports/report (conflict from laptop 2026-11-10).txt', keeping the old content in its version history, and move 'Reports/report (conflict from laptop 2026-11-10).txt' to the trash:
    koi resolve 019c... keep-node:019e...
  Keep both files:
    koi resolve 019c... keep-both
```

The answer names the file to keep by its node ID, so it means the same file on every device.

- `keep-node:<id>` with the original's ID keeps the original and moves the copy to the space's trash.
- `keep-node:<id>` with the copy's ID gives the original the copy's content and moves the copy to the trash. The original's old content stays in its [versions](history.md#see-the-versions-of-a-file).
- `keep-both` keeps both files and closes the decision.

Paths with spaces or other characters a shell reads specially are in single quotes. Both files keep syncing while the decision waits. If the content of either file changes, a new decision with a new ID replaces it. If you rename, move or delete either file yourself, the decision closes. With `--json`, the decision's `answers` have the tokens `keep-node:<id>` and `keep-both`, each with its `argv`.
