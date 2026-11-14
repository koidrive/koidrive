# Install

On macOS 26 or later with Apple silicon, [install and set up the Mac app](macos.md#install-and-set-up-this-mac). It includes `koi` and shares the CLI's device, folders and background sync service.

## Install the CLI

For Linux, servers and scripts, install the `koi` command on Linux (x86_64 or arm64) or a Mac with Apple silicon:

```sh
curl -fsSL https://koidrive.com/install.sh | sh
```

On a Mac, the installer continues into guided setup in your terminal. Setup installs and verifies the macOS app and helps you connect this machine. On Linux, run `~/.local/bin/koi setup` after installation.

The installer downloads the latest [GitHub release](https://github.com/koidrive/koidrive/releases) for your system, checks it against the release's `SHA256SUMS` file, checks that the file is signed with the Koidrive release key and installs `koi` in `~/.local/bin`. It downloads only over HTTPS and never needs root. The release includes the server, so you can deploy a server without Node.js or Rust. On macOS, the release's `koi` is signed with a Developer ID and notarized, and the installer refuses a `koi` without Koidrive's Developer ID signature.

## Use koi in your shell

In an attended terminal, the installer adds one marked entry if the install directory is missing from `PATH`. It uses `~/.zprofile` for zsh, `~/.bash_profile` for bash, or `$XDG_CONFIG_HOME/fish/conf.d/koidrive.fish` for fish, with `~/.config` as the default configuration directory. An exported `ZDOTDIR` selects zsh's profile directory. When creating `.bash_profile`, it preserves loading an existing `.bash_login` or `.profile`. It prints the file and lines it changes. Start a new login shell to run `koi` by name. Setup uses the installed command's absolute path, so it does not need a new shell.

If the installer warns that `koi` on `PATH` runs another program, put the install directory before it. The installer never replaces a `koi` in the install directory that is another program. Other shells get a PATH instruction without a profile change.

## Install without setup or profile changes

Use `--no-setup` to install without starting setup, `--no-modify-path` to leave shell profiles alone, or `--no-app` to skip installing and opening the macOS app during setup:

```sh
curl -fsSL https://koidrive.com/install.sh | sh -s -- --no-app
```

You can also set `KOIDRIVE_NO_SETUP=1` or `KOIDRIVE_NO_MODIFY_PATH=1` for `sh`:

```sh
curl -fsSL https://koidrive.com/install.sh | KOIDRIVE_NO_SETUP=1 KOIDRIVE_NO_MODIFY_PATH=1 sh
```

CI and unattended installs do not change profiles or start setup. Automatic setup requires terminal output and error streams, a controlling terminal for input, and no `CI` environment variable. To start it later, run `~/.local/bin/koi setup`, or use your custom install directory. `koi setup --no-app` runs setup without the app.

## Install with Homebrew

On macOS 26 or later with Apple silicon and [Homebrew](https://brew.sh), install the app and its bundled CLI:

```sh
brew install --cask koidrive/tap/koidrive
```

The cask links the app's `koi` into Homebrew's `bin`, so the app and command share a version. Upgrade both with `brew update && brew upgrade --cask koidrive/tap/koidrive`.

The cask and CLI-only formula are alternatives. Before switching from the formula, run `brew uninstall --formula koidrive/tap/koidrive`. To switch to the formula, uninstall the cask first.

After switching, run `koi service start` with the newly installed `koi`. This updates the background service to the new executable.

For full removal of the app and its settings:

```sh
brew uninstall --zap --cask koidrive
```

This quits the app, unregisters its login item, removes the app and CLI link, and deletes `~/Library/Application Support/Koidrive` and `~/Library/Preferences/dev.koidrive.Koidrive.plist`. It keeps synced folders and CLI state.

For the CLI only on macOS or Linux:

```sh
brew install --formula koidrive/tap/koidrive
koi setup
```

To upgrade:

```sh
brew update && brew upgrade --formula koidrive/tap/koidrive
```

## Update Koidrive

```sh
koi update
```

`koi update` follows links to identify the running installation. For Homebrew, it prints an upgrade command for the owning cask or formula without changing files. For an installer copy, it asks GitHub for the newest release and runs the installer again in the executable's real folder. The new `koi` then takes over the [background service](#background-sync). When the installer copy already runs the newest release, it reports that version and changes nothing. Running the install command again also upgrades `koi`, and restarts every background service that runs that `koi`, in every profile.

`koi status` shows an `Update:` line when a newer release is out. [Updates](service.md#updates) covers which install `koi update` upgrades, the daily update check and how to turn it off.

To install a given version or into another directory, set `KOIDRIVE_VERSION` or `KOIDRIVE_INSTALL_DIR` for `sh`:

```sh
curl -fsSL https://koidrive.com/install.sh | KOIDRIVE_VERSION=v0.1.0-alpha.4 KOIDRIVE_INSTALL_DIR="$HOME/bin" sh
```

The installer needs `curl`, `tar`, `awk`, `grep`, `sed`, `mktemp`, and `sha256sum` or `shasum`, plus `codesign` on macOS. It checks the release signature with `ssh-keygen` from OpenSSH 8.1 or later; without it, the installer warns and installs without that check. `KOIDRIVE_RELEASE_BASE_URL` replaces `https://github.com/koidrive/koidrive/releases` with an `https://` mirror that serves the same files. The installer refuses files the release key did not sign, but a mirror can serve an older signed release unless `KOIDRIVE_VERSION` names the version.

## Build from source

You need Git, Rust installed with [rustup](https://rustup.rs/), Node.js 24 and pnpm. rustup installs the toolchain that `rust-toolchain.toml` pins the first time you build, and `server/package.json` names the pnpm version.

```sh
git clone https://github.com/koidrive/koidrive.git
cd koidrive/server
pnpm install --frozen-lockfile
node scripts/pack-bundle.mjs ../target/server-bundle
cd ..
KOIDRIVE_SERVER_BUNDLE="$PWD/target/server-bundle" cargo install --locked --path cli
```

`pack-bundle.mjs` packs the server into `target/server-bundle`, without a Cloudflare account. `cargo install` builds `koi` with that server inside it and puts it in `~/.cargo/bin`. Check it with `koi --version`.

A build without `KOIDRIVE_SERVER_BUNDLE` can't find or deploy a server in your Cloudflare account. `koi setup` with the Cloudflare login, `koi server deploy`, `add-device` and `upgrade`, and `koi server delete` without `--bucket` stop with "this koi was built without a server bundle". Everything else works, including `koi server claim` and `koi join`.

## Background sync

Linking the first folder installs a background service that runs the sync daemon and starts it. Unlinking the last folder stops the service and removes it. If the service stopped or is missing, run `koi service start`. The `Background service` line of `koi status` shows its state.

[Background service and files](service.md) describes the service on Linux and macOS, its logs, how to run the daemon yourself and the files Koidrive keeps on this machine.
