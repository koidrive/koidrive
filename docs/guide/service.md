# Background service and files

Koidrive syncs linked folders through a daemon, `koi _daemon`. A background service runs the daemon. On Linux it is a systemd user service, and on macOS it is a launch agent. [Install](install.md#background-sync) covers starting it. This page also covers [updates](#updates) and the [files Koidrive keeps](#files-on-this-machine).

## The linked folders decide whether the service runs

When you link the first folder, Koidrive installs the service, starts it and waits until the daemon answers. If the service can't start, the folder stays linked and the command says why. When you unlink the last folder, Koidrive stops the service and removes it. `koi setup`, `koi join`, `koi folder link` and `koi folder unlink` bring the service in line with the linked folders, and `koi service start` repairs a stopped or missing service.

`koi status` shows the service on its `Background service` line. With `--json`, `status` and the commands above add it as `service`. Its `state` is `running`, `stopped`, `switched_off`, `not_installed`, `unavailable` or `off`. When koi knows the cause, `problem` says why the service isn't running.

When more than one `koi` is installed, for example one in `~/.local/bin` and one inside the macOS app, the newest runs the service. A command that brings the service in line keeps the service's binary if that binary reports a version at least as new as the command's own. Otherwise the command points the service at itself. An older `koi` hands `koi service start` to the newer binary, so it never opens a profile that a newer version upgraded. If the running daemon is older than the binary the service then runs, the command restarts the daemon on that binary.

Give each profile its own `XDG_STATE_HOME`. Koidrive names the service and the daemon's socket with a hash of the state folder, so profiles that share `XDG_RUNTIME_DIR` sync side by side. Two profiles that share a state folder can't.

The macOS app runs the `koi` inside it, so the app shows the same device, folders and service as the command. When you open the app, it brings the service in line, as the commands above do. It never starts a service you switched off.

## Linux runs a systemd user service

The service is `koidrive.service`. Koidrive writes the unit, based on [cli/systemd/koidrive.service](../../cli/systemd/koidrive.service), to `~/.local/state/koidrive/systemd/` and enables it with `systemctl --user enable`. Read its log with `journalctl --user -u koidrive`, or with the unit name that `koi status` shows for another profile.

A user service stops when you log out and starts at your next login, unless lingering is on for your user. With lingering, it starts when the machine boots and keeps running after you log out ([systemd/User](https://wiki.archlinux.org/title/Systemd/User)). Koidrive turns lingering on with `loginctl enable-linger` when the system allows it without a password. When it doesn't, for example over SSH, the link says so. Then run:

```sh
sudo loginctl enable-linger "$USER"
```

Koidrive never turns lingering off, since other services of yours may need it. Run `loginctl disable-linger` yourself to turn it off.

Containers, WSL without systemd, and shells started with `su` or `sudo -u` have no systemd user session. There the link still succeeds and says that Koidrive can't sync in the background. Log in as the user directly, or turn lingering on and log in again, then run `koi service start`. On a system without systemd, [run the daemon yourself](#run-the-daemon-yourself).

## macOS runs a launch agent

Koidrive installs the launch agent `~/Library/LaunchAgents/dev.koidrive.koi.plist` and loads it with `launchctl bootstrap gui/$UID`. It runs while you are logged in and starts again at each login. Its log is in `~/Library/Logs/Koidrive/`. The daemon keeps the log under 10 MiB and moves the previous one to a file ending in `.log.1`.

The first time, macOS shows a "Background Items Added" notification. Leave koi switched on in System Settings > General > Login Items > Allow in the Background. If you switch it off, macOS stops the agent, Koidrive leaves it off, and `koi status` shows `switched off in Login Items`. To turn it back on, switch koi on and run `koi service start`. Over SSH with nobody logged in to the desktop, launchd has no session for the agent. Log in to the Mac once, then run `koi service start`.

macOS privacy settings treat the launch agent as a program of its own, so the access you gave Terminal doesn't carry over. When you link a folder in Documents, Desktop, Downloads or iCloud Drive, or on an external or network volume, macOS asks whether koi may access it. Click **Allow**. If you clicked **Don't Allow**, `koi status` names the folder. Open System Settings > Privacy & Security > Files & Folders and switch the folder on under koi, or add koi under Full Disk Access. Then run `koi folder restart <folder>`. macOS keeps these permissions across upgrades for a koi signed with a Developer ID. For an unsigned build it asks again after each upgrade.

## Run the daemon yourself

For scripts and machines where something else runs the daemon, turn the service off in `config.toml`:

```toml
[service]
background = false
```

Koidrive then never installs, starts or removes the service. Unlink every folder before you turn the service off, and link them again after. Folders then sync only while this command runs:

```sh
koi _daemon
```

`_daemon` is the service's entry point, so `koi --help` doesn't list it. It runs in the foreground until you press Ctrl-C or send it SIGTERM. Commands that change this device's folders, such as `folder link`, `folder restart`, `folder retry`, `folder relink` and `device leave`, talk to the daemon through a socket when it runs, and open the profile themselves when it doesn't. The other `device` commands and the `space` commands talk to the server directly. `history trash` and `history versions` read from the server too, but if the daemon runs, they first ask it which space the folder syncs.

The daemon logs at the `info` level. Set `KOIDRIVE_LOG` to change it, for example `KOIDRIVE_LOG=debug`. `--log-format json` writes one JSON object per line.

## Updates

### koi update upgrades one install

`koi update` follows links to identify the running executable and its installation method:

- For the Homebrew cask in `/Applications` or `~/Applications`, it updates the app like a manually installed app, described below. The cask declares `auto_updates`, so once the app has updated itself, `brew upgrade` leaves it alone. If the update fails, `koi update` prints `brew upgrade --cask koidrive` as the command to run.
- For the formula, it runs `brew upgrade koidrive`, and for a cask app in another folder, `brew upgrade --cask koidrive`. It uses the brew executable of the Homebrew that owns the install, whatever PATH selects. First it refreshes the `koidrive/tap` tap alone, and it runs brew with `HOMEBREW_NO_AUTO_UPDATE=1`, so it doesn't update Homebrew or its other taps. If that brew is missing, the tap can't be refreshed or brew fails, `koi update` says so and prints the command to run.
- For an installer copy, it asks GitHub for the newest release and runs `https://koidrive.com/install.sh` with `KOIDRIVE_INSTALL_DIR` set to the executable's real folder and `KOIDRIVE_VERSION` set to the release.
- For a manually installed app in `/Applications` or `~/Applications`, it downloads and verifies the release's app, then replaces the older copy. For an app in another location, it gives the release's download link.
- Development builds do not update to releases.

After an upgrade, the new `koi` takes over the background service. When the `koi` already runs the newest release, `koi update` changes nothing and reports `unchanged`. It upgrades Koidrive for one user, so it refuses to run as root. It shows one line per step. The output of the installer and of Homebrew appears only when a step fails, or with `koi update --verbose`. Each run writes what it ran and printed to `update.log`, in `~/Library/Logs/Koidrive/` on macOS and in the state folder on Linux.

On Apple silicon Macs, a command that brings the service in line also makes sure the installed app is from the same release as the running `koi`, without asking. It never replaces a verified newer app with an older one. The `koi` inside the app skips this step. The installer keeps the replaced app until the new daemon confirms its version. If Koidrive can't confirm the handoff, the installer leaves the replaced app in place. If the app install fails, the command's service result and exit status stay the same. When the `CI` environment variable is set, no command installs the app.

The daemon also looks every hour for a newer `koi` from Homebrew, the installer or the app, and hands the service to the newest one. So after another install upgrades `koi`, sync runs the new release within the hour.

### The daemon checks for a release once a day

The daemon asks GitHub once a day whether a newer release is out, and keeps the answer in `update-check.json` in the state folder. When one is out, `koi status` shows it after the background service:

```
Update: Koidrive 0.1.2 is out (this device runs 0.1.0). Run: /home/ada/.local/bin/koi update
```

The command depends on how the running executable was installed. An installer copy or a Homebrew cask in an Applications folder names its resolved executable, with shell quoting when needed. A formula shows `brew upgrade koidrive`, and a cask app in another folder shows `brew upgrade --cask koidrive`.

Each check reads Koidrive's public list of releases, `https://api.github.com/repos/koidrive/koidrive/releases`, following its pages with ordinary GitHub requests. It sends the user agent `koidrive` and, for a cached single-page list, the list's ETag, a hash that is the same for everyone. It sends no version, device, account or server address. GitHub sees your IP address, as it does for any download. The check isn't a [usage report](telemetry.md). Nothing goes to Koidrive, and turning reports off leaves the check on.

Until a stable release exists, the notice follows prereleases. After that it follows stable releases only, so a computer on a prerelease shows no notice until a newer stable release is out. A release counts once it lists the tarball for this system.

To turn the check off, set this in `config.toml` and restart the daemon:

```toml
[updates]
check = false
```

`KOIDRIVE_UPDATE_CHECK` overrides the file. `0`, `off` or `false` turns the check off, and `1`, `on` or `true` turns it on. With the check off, the daemon sends nothing and deletes `update-check.json`. An installer copy's `koi update` still asks GitHub when you run it.

## Files on this machine

| Path | What it holds |
|---|---|
| `~/.config/koidrive/config.toml` | Your settings: [selective sync](folders.md#sync-only-part-of-a-space-on-this-device), the [local trash](sync-rules.md#the-local-trash) size, how many [uploads run at once](sync-rules.md#uploads-at-once), whether the [background service](#run-the-daemon-yourself) is on, and whether the daemon runs the [update check](#the-daemon-checks-for-a-release-once-a-day). You edit it. Koidrive never writes it |
| `~/.local/state/koidrive/update-check.json` | GitHub's last answer to the [update check](#the-daemon-checks-for-a-release-once-a-day) |
| `~/.local/state/koidrive/update.log` | On Linux, what each [`koi update`](#koi-update-upgrades-one-install) ran and printed. On macOS it is `~/Library/Logs/Koidrive/update.log` |
| `~/.local/state/koidrive/credential.json` | This device's credential. On macOS, Koidrive keeps it out of Time Machine backups |
| `~/.local/state/koidrive/spaces/` | The sync state of each linked folder |
| `~/.local/state/koidrive/systemd/koidrive.service` | On Linux, the background service's unit |
| `~/Library/LaunchAgents/dev.koidrive.koi.plist` | On macOS, the background service's launch agent. Its log is in `~/Library/Logs/Koidrive/` |
| `.koidrive/` in each linked folder | The folder's marker, the record of the device that linked it, and the local trash |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` replace `~/.config` and `~/.local/state` when they are set to absolute paths, on macOS too. Koidrive ignores a relative value. The daemon's socket is `$XDG_RUNTIME_DIR/koidrive/daemon-<hash>.sock`, named with a hash of the state folder. When `XDG_RUNTIME_DIR` isn't set to an absolute path, the socket is `daemon.sock` in the state folder.
