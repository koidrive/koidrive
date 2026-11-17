# Sync driver and Linux daemon

`core/driver` (`koidrive-driver`) is everything a client runs on one device around the sync engine: the profile, one task per linked folder, the session with each space, and the observer of each folder. The Linux daemon `koi _daemon` and the macOS app both run it. Paths, configuration keys, the API and the event handling are in [driver-reference.md](driver-reference.md). Engine sections cited as §N are in [the sync engine reference](sync-engine/README.md).

## Both clients share one library

The driver is a library with an async API. It never prints or exits, and it logs through `tracing`. The daemon calls it from its Tokio runtime. The macOS app calls it in-process through `core/ffi`, whose `DriverCore` wraps a `Driver`. `DriverCore` sends the calls the driver does not make, such as listing devices or spaces, through `core/net`'s `Api` with the driver's credential (`Driver::signed_in`). A `koi` command sends its request to the daemon's socket, or opens a driver of its own when no daemon runs.

## One credential has one writer

Two processes that share a device credential would overwrite each other's commits or refuse them, because the server does not count a device's own changes as unseen and `device_seq` is strict. The driver enforces the rule in five places:

1. `pair` and `claim` refuse to replace a credential. The app and the CLI on one machine are separate profiles and pair separately.
2. Opening a driver takes the profile lock, so a second daemon is refused.
3. Each store records the device and server that linked it. A store of another device halts instead of syncing.
4. `link` refuses a folder that holds or sits inside the profile's directories or another linked folder, compared by path and by folder identity, so links and mounts cannot hide an overlap.
5. A linked folder's `.koidrive/device.json` names its device, and a running folder holds an `flock` on the folder itself. Another profile's `link` refuses the folder, and its task halts.

On macOS the driver excludes the credential file from Time Machine before it writes anything to it. A Mac restored from a backup has its stores and folders but no credential, so those folders halt as signed out until the device pairs again.

## A folder task runs one loop

A task checks the root (§6.2), checks placement and the folder's claim, starts the observer on its own thread, and opens a `core/net` `Session` that reconnects with backoff. A failed root check halts the folder and checks it again later. Any other halt waits for a person.

`Folder` in `folder.rs` owns the engine, the store and the disk, and is their only writer. Each of its methods takes one input: a session event, an observation batch, or the result of work it asked for. The method changes the engine, ends with `Engine::checkpoint` in the same store transaction, and returns the work to start. No transaction contains an `await`, so a store loaded after any step gives the same engine. Around it, one `select!` hands each input to the folder and runs the returned work: a tree listing, staging a batch of effects, uploading one commit's chunks, the commit call, or a wait.

After each input the folder plans in the order of the engine's reference loop (`core/sim/tests/engine_sync.rs`). The folder plans nothing before the first scan or while the store is behind the log. Effects run in batches of independent effects (sync engine §5.2), and only while no commit is in flight. The folder builds a commit when no effect is planned and the store holds the server's head. A refused commit goes back to the engine, which sends it again, plans its changes again or stops committing.

## A first link adopts what matches and holds the rest

A folder with no cursor in its store waits for its first scan before it takes the server's tree. Then `koidrive_engine::first_link::adopt` matches the folder against the space from the root down (§6.4). An entry with the same name, kind and content as a live node takes that node. Everything else is held under one question, sending nothing, until the person publishes it or moves it to the local trash. Entries created after the first link are sent at once.

## Bootstrap reads the whole tree, then follows the log

A bootstrap pages through `ListTree`, and once the folder has synced, through `ListTrash` too, then replays the log onto that tree. The tree replaces the store's remote tree in one transaction once the replay reaches the server's head, so the store holds a valid tree at every commit. After `LOG_TRUNCATED` or `AHEAD_OF_SERVER`, the folder bootstraps again. A space rewound on the server (`HISTORY_CHANGED`) halts the folder.

## The observer reports changes with their content

`FsObserver` runs `core/fs`'s watcher (inotify on Linux, FSEvents on macOS) and rescans every 30 seconds where the watcher cannot cover the tree. Each batch carries the changes, the content of every created or modified file chunked with `core/chunk`, and the root check. The observer starts from a snapshot rebuilt from the store, so the first scan after a restart reports only what changed meanwhile and reads only the files that differ.

## The daemon serves the CLI over a socket

`koi _daemon` opens the driver, binds a socket only its user can reach, starts every folder, and serves one JSON line each way per connection until SIGTERM or SIGINT. It shows a desktop notification when a decision opens. Its logs never carry a refresh credential, an access token or a pairing code. Linux runs it as a systemd user service, and macOS as a launch agent.

## Progress counts the files left to sync

A folder counts each file whose content differs between the folder and the server until both hold the same content. A file that a rule, a question or a problem keeps out of sync does not count. A file found while others sync counts at once in `files_to_upload` or `files_to_download` and in `bytes_total`. The counts start from zero when the folder has files to sync again after none were left.

Until then, `bytes_done` never decreases, and `bytes_total` grows only when new work arrives. The fraction falls when a folder finds more to sync, and it never falls because credit for work already done is lost.

`bytes_done` counts the content that both sides hold: each chunk once it is stored or fetched, or once the server reports it present already, and the whole content of a file once it is in sync. A chunk sent again after a cut-off upload or a restart counts once, so `bytes_done` never decreases. When a file leaves without syncing, because it was edited again or removed, the folder takes its remaining bytes out of `bytes_total`. `current` is the file whose content moved last.

`status()` reports progress while a folder bootstraps or catches up, and none otherwise. A change of progress alone marks no `watch()` receiver unless progress begins or ends. `changes()` reports it at most once every 250 ms (`PROGRESS_INTERVAL`), so the daemon's `watch` socket, `koi status` and the app's `SessionEvent::Progress` carry the same numbers at a bounded rate, and an idle folder sends nothing. `status()` also sums them over the folders in `Status.progress`, which the app shows, while a folder syncs. A folder that finishes, abandons a file, goes offline or stops keeps the bytes it moved in that sum until no folder has a file left, so a paused driver reports no sum and shows the same one when it syncs again. Unlinking a folder or signing out takes the folder out of the sum. The sum takes each folder's own count of what synced and what left, so it stays exact however the watch snapshots coalesce.

## A rewound space halts its folders

A rewound space halts each folder that syncs it, because the driver does not run the engine's recovery (§6.4) for it.
