# Sync driver reference

The files, settings, calls and rules of `core/driver`. [driver.md](driver.md) explains how they fit together. Engine sections cited as §N are in [the sync engine reference](sync-engine/README.md).

## Profile layout

The CLI uses the XDG base directories. The macOS app passes its own container to `Paths::under`.

| Path | Contents |
|---|---|
| `$XDG_CONFIG_HOME/koidrive/config.toml` | The device-local configuration. No secrets, no state, never synced |
| `$XDG_STATE_HOME/koidrive/` (mode 0700) | Machine-bound state, never copied |
| `…/credential.json` (mode 0600) | The device credential: server, user and device ids, user name, device name, refresh credential. Excluded from Time Machine. Its modification time is when the refresh credential last changed |
| `…/credential.next` (mode 0600) | The staged refresh credential and its deadline, only while a rotation's outcome is unknown ([credential-rotation.md](credential-rotation.md)) |
| `…/lock` | The profile lock (`flock`), held by the one process that writes the profile |
| `…/paused` | Present while sync is paused, also across restarts |
| `…/spaces/<space_id>/store.sqlite` | One `core/store` per linked space |
| `$XDG_RUNTIME_DIR/koidrive/daemon-<hash>.sock` (mode 0600) | The daemon's socket, named with a hash of the state directory. `daemon.sock` in the state directory when `XDG_RUNTIME_DIR` is unset |
| `<folder>/.koidrive/` | The root marker `space.json`, the owner record `device.json`, `staging`, `quarantine` and the local trash |

## Configuration

```toml
[sync]
include = ["Projects/"]   # when not empty, only these paths are synced
exclude = ["*.iso"]       # .koiignore patterns
gitignore = true          # the default: .gitignore files apply too

[trash]
max_size_mb = 2048        # the default

[upload]
concurrency = 4           # the default, from 1 to 16

[service]
background = true         # the default
```

- `[sync]` applies to every folder of the profile. A path is excluded when the built-in exclusions, these rules or the folder's rules files exclude it, so no rules file widens what the device syncs. `gitignore = false` leaves the folder's `.gitignore` files out of the rules (§5.10). The rules save bandwidth and disk only. The server still sends every name, and the credential can read everything.
- `[trash]` caps each folder's local trash (§5.2). A running folder reclaims it at start and once a day: entries past their retention go first, then the oldest while the trash holds more than `max_size_mb` megabytes.
- `[upload]` limits chunk uploads in flight across all folders of the profile, through one `koidrive_net::UploadGate`. The driver refuses a value outside 1 to 16 when it opens.
- `[service]` with `background = false` leaves the background service alone, and the person runs `koi _daemon` some other way.

## Driver API

| Call | What it does |
|---|---|
| `Driver::open(paths)` | Takes the profile lock and reads the configuration, the credential and the linked folders. Nothing syncs until `start` |
| `pair(server, code, name)` | `POST /v1/pair/complete`, then stores the credential. Once per profile |
| `claim(server, code, user_name, device_name)` | `POST /v1/claim` with the code from the server's log, then stores the credential |
| `save_credential(credential)` | Stores a credential issued elsewhere, such as by `koi setup` |
| `sign_out(confirm_last_device)` | Stops, revokes the device on the server, unlinks every folder (the files stay) and deletes the credential ([RFC 0009](../../spec/rfc/0009-device-revocation.md) §5). Without `confirm_last_device`, it goes on only once the server revoked the device, and returns `LastDevice` for an HTTP 409 |
| `sign_out_device(device_id)` | Signs out with `confirm_last_device`, only while the saved credential is that device's |
| `link(folder, space)`, `link_approved(…)` | Checks the folder, writes the root marker and the owner record, and creates the store. `LinkApproval` accepts a folder that already holds files |
| `unlink(folder)` | Stops the folder, removes its owner record and its store. The files and `.koidrive/` stay. A failed removal leaves the folder linked and halted |
| `status()` | The account, whether the driver runs or is paused, and per folder: state, cursor, head, node and entry counts, open questions, problem and wait |
| `questions()`, `answer(id, answer)`, `answered_question(id)` | Lists open questions, records an answer, and reads an answered one. The same answer again changes nothing, and another answer is refused |
| `restore(space, request)`, `restores(space)` | Records a restore for the engine's next commit (§6.3) and lists restores, ended and pending |
| `start()`, `stop()` | Starts one task per folder unless paused. Stops them and waits until every task has ended |
| `pause()`, `resume()` | Stops every folder and writes the paused file. Removes it and starts the folders |
| `restart(folder)`, `lift(folder, lift)` | Syncs the folder again from the start. `lift` also clears a refusal's halt by resending the commit or planning it again |
| `follows_gitignore()`, `set_gitignore(follow)` | Whether the folders follow their `.gitignore` files. Writes `gitignore` under `[sync]`, keeping the rest of the file, and restarts each running folder with the new rules; the same value restarts nothing |
| `gitignore_only(folder)` | The regular files and bytes of the folder on disk that only its `.gitignore` files leave out, whatever the setting is |
| `watch()` | A receiver marked whenever `status`, `questions` or `restores` may have changed |
| `signed_in()` | The profile's one `Auth`, for calls the driver does not make |

`pair`, `claim`, `sign_out`, `link` and `unlink` run one at a time, each to its end, so no operation loses a credential the server issued.

## Session events

| Event | What the folder task does |
|---|---|
| `Connected(Welcome)` | A cursor of another history enters recovery (§6.4) and lists the tree again. Otherwise the engine compares the server's count of this device's commits with the store (§2.8), and a store that never synced takes the server's count. No cursor, or a cursor that records the server behind it: bootstrap. Any other cursor: subscribe from `last_seq` |
| `Changes` | Applied to the remote tree in one transaction, in seq order. A gap resubscribes from the cursor. An unknown change kind halts |
| `CaughtUp` | The head is known. `up_to_date` once the store holds it, the folder was scanned and nothing is left to move |
| `ResyncRequired` | `LOG_TRUNCATED` and `AHEAD_OF_SERVER`: bootstrap again. `AHEAD_OF_SERVER` first records that the server is behind, so the bootstrap forgets what the server lost (§4.4). `HISTORY_CHANGED`: enter recovery (§6.4) and reconnect. Unknown reasons: halt |
| `Disconnected` | `offline`. The session retries |
| `Stopped` | Halt: credentials refused, device revoked, space not found or forbidden, unsupported version |
| `Committed` | The result of this device's commit. Whichever of it and the commit call's answer comes first is applied |

`ListTree` and `ListTrash` page 5,000 nodes at a time.

## Refusals

`policy::after_refusal` decides what follows a refused call ([RFC 0006](../../spec/rfc/0006-agent-readiness-hooks.md)):

| Code | Disposition |
|---|---|
| `RETRY_LATER` | Retry after `retry_after_ms`, or after backoff when it is 0 |
| `QUOTA_EXCEEDED` | Retry after `retry_after_ms` when it is above 0. Halt otherwise |
| `INTERNAL` | Retry after backoff |
| Any other code, `UNSPECIFIED` or unknown | Halt and report, with the raw number for an unknown code |

## Timers

| Timer | Value |
|---|---|
| Root check after a failure | 1 second, doubling up to 30 seconds |
| Folder still at its locked path | Checked every 2 seconds between events |
| Full rescan where the watcher cannot cover the tree | Every 30 seconds |
| A file that changed while it was read | Read again after 250 ms, doubling up to 30 seconds |
| Local trash reclaim | At start and every 24 hours |

## Daemon

- Socket: one JSON line each way per connection, a request tagged `request` and a response tagged `response`. The daemon refuses a peer whose uid is not its own.
- Notifications: one per decision that opens while the daemon runs, through `org.freedesktop.Notifications` on Linux and `osascript` on macOS. A decision that replaces an open one continues its lineage and gets no new notification. Decisions open at start get no notification, and `koi status` shows them.
- Logs: stderr, as key-value text or JSON lines with `--log-format json`. `KOIDRIVE_LOG` sets the filter.
- Service: `cli/systemd/koidrive.service` (`Type=exec`, `Restart=on-failure`) on Linux, a launch agent on macOS.
