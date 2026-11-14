# Usage reports

Koidrive can send anonymous usage reports to the Koidrive project. They help identify failed setup, crashes, and slow sync. Reports never hold your files' names, paths or contents, your server's address or your account.

## Turn reports on or off

The guided `koi setup` on a terminal asks after setup succeeds. Turning reports on here applies to future commands, not the setup that just finished. If you cancel or close the question without answering, it returns after the next successful interactive command:

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
```

Other commands ask only to finish an unanswered setup question. They send reports only when this profile already chose to send them, or when `KOIDRIVE_TELEMETRY` turns them on. `koi setup`, `koi join`, `koi server deploy`, `koi server add-device` and `koi server claim` also take `--telemetry on` or `--telemetry off`, which stores the choice without asking. Questions need a terminal on stdin and stderr and never appear with `--json`. The `CI` environment variable suppresses them, but with `--interactive`, setup can ask when `CI` is `0` or `false`.

A running daemon follows `koi telemetry on`, `off` and `reset` at its next tick, within a minute.

These environment variables override the choice:

| Variable | Effect |
|---|---|
| `DO_NOT_TRACK=1` | Turns reports off. It wins over everything else |
| `KOIDRIVE_TELEMETRY=0` or `off` | Turns reports off |
| `KOIDRIVE_TELEMETRY=1` or `on` | Turns reports on |
| `KOIDRIVE_TELEMETRY=debug` | Prints each report to stderr instead of sending it |

The choice belongs to one profile on one machine. It lives in `telemetry/settings.json` in the profile's state directory (`~/.local/state/koidrive` by default), never in `config.toml`.

## What a report holds

Every report carries:

- A random install id, 32 hex characters, drawn when you turn reports on. It isn't derived from your machine, your account or your Koidrive device, and it stays until you reset it.
- Which program sent it (the daemon, a command or the macOS app), the Koidrive version, and whether it's a release or development build.
- The operating system, its major and minor version, the Linux distribution when it's a common one, and the processor architecture.
- A time in UTC. An event carries the second it happened. Sums carry the start of the hour they cover.

On top of that, reports hold:

- How many times each `koi` command group, such as `koi folder`, ran in an hour. Only the group's name, never its action or arguments.
- The steps of `koi setup` and `koi server ...` (log in, R2, bucket, Worker upload, serving, claim, pairing), `koi server upgrade` and `koi server delete`: whether each one succeeded and roughly how long it took.
- Where Koidrive panicked: the source file and line in Koidrive's own code. Never the panic's message.
- Exact hourly totals from the daemon: commits sent and received, bytes of file content uploaded and downloaded, and the hour's highest number of linked spaces and of the files and folders they hold, trash included. Only totals over all your spaces, never a count per space or per file.
- How many times each kind of error happened in an hour: why a folder stopped syncing, how an upload or download failed, the code of a server refusal and of each item a refused commit left out, each commit too large to send and each item it left out, which kind of question a folder asked, and why the daemon could not start. Never the error's message.
- Hourly distributions of sync timings, described below. Each holds 16 power-of-two buckets and a sample count, without identifying a folder. Duration histograms omit exact sums; throughput histograms retain sums.
- The daemon's user and system CPU time in milliseconds, summed between one-minute samples and at shutdown, each under the hour it ends in. Gaps longer than an hour and unavailable clock readings are omitted.

### Sync timing distributions

- First-sync milliseconds, from linking to the first "up to date" status with no scheduled disk-effect retry, grouped by a power-of-two range of live remote file counts. Directories and trash do not count. The ranges start at zero, one, and two to three, and end at 1,048,576 or more. Exact per-folder counts are not sent. Eligibility survives restarts. Links created before timing records existed or while reports were off, links whose reports were turned off at any point before the first sync, and storage failures leave no sample.
- Upload and download files per second and bytes per second. A burst starts with that direction's first work and ends at "up to date" with no scheduled disk-effect retry, including connection waits and retries. Files count successful versions, including empty files and deduplicated content. Bytes count verified chunks written to staging or chunks stored by the server, including retransfers. Rates round down to whole files or bytes per second. Bursts with no successful file, no elapsed time, or no completion before shutdown leave no sample.
- Remote-apply milliseconds, from a commit's server time to each successful disk effect with a matching origin and revision. One commit can produce several samples. Missing, future, replayed, or superseded origins leave no sample. Snapshot entries and restarts can lack origins. This is not an atomic whole-commit duration and depends on the server and device clocks.
- Continuous syncing milliseconds, from bootstrapping or catching up until "up to date" with no scheduled disk-effect retry. Interrupted spans are discarded. Connecting, offline, halted, and stopped time do not count. No unfinished-span status or age is sent.

Completed durations and rates belong to the completion hour. Spans can cross an hour without resetting, but restart when the folder's sync loop restarts. Elapsed segments use a monotonic clock. Sync durations are bounded to zero through 30 days. Duration buckets distinguish minutes through several hours; the last bucket holds durations of about 4.7 hours or more.

## What a report never holds

- File and folder names, paths, contents, or hashes of any of them. Sizes are sent only as hourly totals or throughput distributions.
- Space, device, user and node ids, and the names of spaces, devices and users.
- Your server's address, Worker and bucket names, and your Cloudflare account id.
- Pairing codes, claim codes, recovery keys, tokens or credentials.
- Error messages, log lines, stack traces, `.koiignore` rules and configuration values.
- Your host name or user name.

Your own Koidrive server sends nothing to the project. Its Worker logs stay in your Cloudflare account.

## Where reports go

Reports go to `https://tel.koidrive.com`, which forwards them to the project's [PostHog](https://posthog.com) project. Each report is an anonymous PostHog event: PostHog makes no person profile for it and doesn't look up a location from your address. The daemon sends as it starts, then every minute, and once more as it stops. Each send holds the events recorded so far and the sums of hours that have ended in UTC, so an event goes within a minute and an hour's sums within a minute after the hour ends. A send that fails goes again at the next minute. A command that recorded an event sends it as it exits, waiting at most two seconds. A report waits on your machine for up to 14 days while it can't be sent, and is then dropped.
