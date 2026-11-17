# Usage reports

Koidrive can send usage reports to the Koidrive project. They help the project find failed setups, crashes and slow sync. Reports never hold your files' names, paths or contents, your server's address or your account. Each report carries your IP address, which the project stores and uses to estimate your country and city.

## Turn reports on or off

On a terminal, the guided `koi setup` asks at the end of the run, even when setup failed or you cancelled it. Until you answer, the reports of that run wait on your machine and nothing is sent. `koi telemetry preview` shows them meanwhile. Yes sends them and the reports of later commands. No deletes them. If you close the question without answering, Koidrive deletes them and asks again after the next successful interactive command:

```
Send reports? [Y/n]
```

Press Enter or type `y` to send them, or type `n` to not send them. Change your answer at any time:

```sh
koi telemetry status    # whether reports are sent, why, and what waits to be sent
koi telemetry on
koi telemetry off       # also deletes the install id and the reports not sent yet
koi telemetry preview   # prints the waiting reports as the daemon would send them
koi telemetry reset     # draws a new install id and deletes the reports not sent yet
koi telemetry internal on   # marks this machine's reports internal, for people who build Koidrive
```

Other commands ask only when a setup question is still unanswered. `koi join`, `koi server deploy`, `koi server add-device` and `koi server claim` then ask at the end of the run whatever its result, and their reports also wait until you answer. Other commands ask only after they succeed. Commands send reports only when this profile already chose to send them, or when `KOIDRIVE_TELEMETRY` turns them on. `koi setup`, `koi join`, `koi server deploy`, `koi server add-device` and `koi server claim` also take `--telemetry on` or `--telemetry off`, which stores the choice without asking. Questions need a terminal on stdin and stderr and never appear with `--json`. The `CI` environment variable suppresses them, but with `--interactive`, setup can ask when `CI` is `0` or `false`.

A running daemon follows `koi telemetry on`, `off` and `reset` at its next tick, within a minute.

These environment variables override the choice:

| Variable | Effect |
|---|---|
| `DO_NOT_TRACK=1` | Turns reports off. It wins over everything else |
| `KOIDRIVE_TELEMETRY=0` or `off` | Turns reports off |
| `KOIDRIVE_TELEMETRY=1` or `on` | Turns reports on |
| `KOIDRIVE_TELEMETRY=internal` | Turns reports on and marks them internal |
| `KOIDRIVE_TELEMETRY=debug` | Prints each report to stderr instead of sending it |

The choice belongs to one profile on one machine. It lives in `telemetry/settings.json` in the profile's state directory (`~/.local/state/koidrive` by default), never in `config.toml`.

## What a report holds

Every report carries:

- A random install id, 32 hex characters, drawn when you turn reports on. It isn't derived from your machine, your account or your Koidrive device, and it stays until you reset it.
- Which program sent it (the daemon, a command or the macOS app), the Koidrive version, whether it's a release or development build, and its channel: `release`, `prerelease` for alphas and release candidates, or `dev`.
- Whether the machine is marked internal with `koi telemetry internal on`.
- The operating system, its major and minor version, the Linux distribution when it's a common one, and the processor architecture.
- A time in UTC. An event carries the second it happened. Sums carry the start of the hour they cover.

On top of that, reports hold:

- How many times each `koi` command group, such as `koi folder`, ran in an hour. Only the group's name, never its action or arguments.
- The steps of `koi setup`, `koi join` and `koi server ...` (log in, R2, bucket, Worker upload, serving, claim, pairing), `koi server upgrade` and `koi server delete`: whether each one succeeded and roughly how long it took.
- Where Koidrive panicked: the source file and line in Koidrive's own code, and the names of Koidrive's own functions on the panicking thread's stack, such as `koidrive_driver::folder::Folder::run`. Never the panic's message, the functions' arguments, file paths, or functions of other libraries.
- When the Mac app crashes: how it crashed (a Mach exception, a signal or an uncaught exception), the exception and signal, the app's build ID, and the positions in the app's own code of the crashed thread's frames, as numbers. [KSCrash](https://github.com/kstenerud/KSCrash) writes a report on your Mac as the app crashes. At the next launch, Koidrive takes those fields from it and deletes the report. Koidrive sends a crash only when reports were on as the app crashed and have stayed on since. Never the exception's message or name, file paths, other threads, or other libraries' code.
- Each `koi` command that fails: its group, such as `koi space`, and the error code its `--json` output carries, such as `auth_required`. Never the message.
- Exact hourly totals from the daemon: commits sent and received, bytes of file content uploaded and downloaded, and the hour's highest number of linked spaces and of the files and folders they hold, trash included. Only totals over all your spaces, never a count per space or per file.
- How well sync is going, read by the daemon every minute and kept as the hour's highest readings over all your spaces: how many spaces are halted and how many for each reason, the minutes since the space that halted longest ago halted, how many spaces were last up to date less than an hour ago, 1 to 6 hours ago, 6 hours to a day, 1 to 3 days, 3 to 7 days, more than 7 days ago or at an unknown time, how many files and folders still differ between your spaces and the server, and whether sync is paused. A space that's up to date counts as less than an hour. Never which space is halted or when a particular space last synced.
- How many times each kind of error happened in an hour: why a folder stopped syncing, how an upload or download failed, the code of a server refusal and of each item a refused commit left out, each commit too large to send and each item it left out, which kind of question a folder asked, and why the daemon could not start. Never the error's message. Each kind also goes as its own event when the daemon sees it, with how many times it happened since the daemon's previous minute. When a halted space syncs again, an event says why it had halted and how long it stayed halted, as one of the ranges above.
- Each time a linked folder's disk comes back under another device number, as after a macOS update, and Koidrive confirms it's the same folder: how it confirmed it, by the disk's ID or by the files and folders at the top of the folder, and the number of those that matched, rounded to a power of two. Never the device numbers, the disk's ID or any names.
- Hourly distributions of sync timings, described below. Each holds 16 power-of-two buckets and a sample count, without identifying a folder. Duration distributions leave out exact sums. Throughput distributions keep them.
- The daemon's user and system CPU time in milliseconds, summed between one-minute samples and at shutdown, each under the hour it ends in. The daemon leaves out gaps longer than an hour and clock readings it couldn't take.

### Sync timing distributions

- First-sync milliseconds, from linking to the first "up to date" status with no disk change waiting for a retry, grouped by the number of live files in the space, in power-of-two ranges. Directories and trash do not count. The ranges start at zero, one, and two to three, and end at 1,048,576 or more. Koidrive doesn't send a folder's exact count. A link stays eligible for a sample across restarts. A link leaves no sample if it was made before timing records existed or while reports were off, or if reports were turned off at any point before its first sync. A storage failure also leaves no sample.
- Upload and download files per second and bytes per second. A burst starts with the first upload or download in its direction and ends at "up to date" with no disk change waiting for a retry. A burst includes connection waits and retries. Files count successful versions, including empty files and deduplicated content. Bytes count verified chunks written to staging or chunks stored by the server, including retransfers. Rates round down to whole files or bytes per second. Bursts with no successful file, no elapsed time, or no completion before shutdown leave no sample.
- Remote-apply milliseconds, from the server's time for a commit to each successful disk change with a matching origin and revision. One commit can produce several samples. Missing, future, replayed, or superseded origins leave no sample. Snapshot entries and restarts can lack origins. A sample doesn't time a whole commit as one unit, and it depends on the server and device clocks.
- Continuous syncing milliseconds, from bootstrapping or catching up until "up to date" with no disk change waiting for a retry. Koidrive discards interrupted spans. Connecting, offline, halted, and stopped time do not count. Koidrive sends neither the status nor the age of a span that hasn't finished.

A completed duration or rate counts in the hour it completes. Spans can cross an hour without resetting, but restart when the folder's sync loop restarts. Elapsed segments use a monotonic clock. Sync durations are bounded to zero through 30 days. Duration buckets distinguish minutes through several hours. The last bucket holds durations of about 4.7 hours or more.

## What a report never holds

- File and folder names, paths, contents, or hashes of any of them. Koidrive sends sizes only as hourly totals or throughput distributions.
- Space, device, user and node ids, and the names of spaces, devices and users.
- Your server's address, Worker and bucket names, and your Cloudflare account id.
- Pairing codes, claim codes, recovery keys, tokens or credentials.
- Error messages, log lines, `.koiignore` rules and configuration values, and from a stack trace anything other than the names of Koidrive's own functions.
- Your host name or user name.

Your own Koidrive server sends nothing to the project. Its Worker logs stay in your Cloudflare account.

## Where reports go

Reports go to `https://tel.koidrive.com`, which forwards them to the project's [PostHog](https://posthog.com) project. PostHog makes no person profile for a report. It stores the IP address the report came from and looks up the country and city of that address. The daemon sends as it starts, then every minute, and once more as it stops. Each send holds the events recorded so far and the sums of hours that have ended in UTC, so an event goes out within a minute, and an hour's sums go out within a minute after the hour ends. Koidrive retries a failed send at the next minute. A command that recorded an event sends it as it exits, waiting at most two seconds. Koidrive writes a panic to disk as it happens and sends it with the next send, from the running daemon or from the next start. If Koidrive can't send a report, it keeps the report on your machine for up to 14 days and then drops it.
