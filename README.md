<p align="center">
  <img src="docs/assets/koidrive-logo.svg" alt="Koidrive logo" width="96">
</p>

<h1 align="center">Koidrive</h1>

<p align="center">
  <strong>File sync you deploy to your own Cloudflare account.</strong><br>
  Koidrive keeps folders in sync between your Macs and Linux machines and stores the files in your own R2 bucket.
</p>

<p align="center">
  <a href="#why-koidrive">Why Koidrive</a> ·
  <a href="#koidrive-and-dropbox-side-by-side">Koidrive and Dropbox</a> ·
  <a href="#what-it-costs">What it costs</a> ·
  <a href="#get-started">Get started</a> ·
  <a href="docs/guide/README.md">User guide</a>
</p>

> [!WARNING]
> **Koidrive is in alpha.** [Get started](#get-started) with the Mac app or the `koi` command. The protocol and storage format may change before 0.1.0. Keep another copy of anything you sync.

## Why Koidrive

**You pay for storage, not for seats.** Koidrive is free and open source. Cloudflare bills your account for the storage and requests you use, with no egress fee, as [What it costs](#what-it-costs) shows. Dropbox sells storage in fixed tiers, and its team plans charge for every person.

**Your files stay in your account.** Koidrive stores your files as chunks in an R2 bucket you own. A Durable Object in the same account keeps each space's history. No other company stores your files. If you stop using Koidrive, every linked folder still holds ordinary files.

**The same files on every machine.** An edit on your Mac reaches your Linux machine as soon as both are online, and the other way round. New files, edits, renames, moves and deletes all sync.

**Mistakes you can undo.** When two machines edit the same file, Koidrive keeps both versions. When one machine deletes a file that another just edited, the edit stays. A large delete waits for your answer before it reaches your other machines. Deleted files and old versions stay in the trash for 30 days, and one command brings them back. Every pull request runs a simulation that loses, repeats and reorders messages and crashes devices, and it fails if any device loses a write.

**Agents work in the same folder.** A linked folder holds plain files, so an agent that edits files can work in it, and Koidrive syncs its changes like yours. Every `koi` command prints JSON with `--json`, `koi status --check` reports problems through its exit status, and an agent can answer a held delete with `koi resolve`. See [Agents and scripts](docs/guide/agents.md).

## Koidrive and Dropbox, side by side

Dropbox does much more, and charges per person for it. Koidrive does the core of file sync and keeps the files in your account. The Dropbox prices below are from [dropbox.com/plans](https://www.dropbox.com/plans) on 2026-11-11.

### Price and ownership

| | Koidrive | Dropbox |
|---|---|---|
| **Price** | Free and open source. You pay Cloudflare for storage. The first 10 GB is free, then $0.015 per GB-month | Basic is free with 2 GB. Plus is $9.99 a month for one person with 2 TB, and Standard $15 and Advanced $24 per person a month, all billed yearly |
| **Where your files live** | An R2 bucket in your Cloudflare account | Dropbox's cloud |
| **Source code** | Apache-2.0, with the server under AGPL-3.0, on GitHub | Closed |
| **Deleted files and old versions** | 30 days | 30 days on Plus, 180 days on Standard, 1 year on Advanced |

### Everyday sync

| | Koidrive | Dropbox |
|---|---|---|
| **Two-way sync of whole folders** | Yes | Yes |
| **Both copies kept on a conflict** | Yes | Yes |
| **Trash and version history** | 30 days | 30 days to 1 year |
| **Share links** | Yes | Yes |
| **macOS** | Yes, for macOS 26+ | Yes |
| **Linux** | Command line and daemon | Yes |

### Where Dropbox does more

| | Koidrive | Dropbox |
|---|---|---|
| **Sharing with other people and teams** | No | Shared folders, team folders, groups and roles |
| **Files on demand** | No. Every synced file is on disk | Online-only files |
| **Windows, iPhone and Android** | No | Yes |
| **Web interface** | No | Yes |
| **Admin tools** | No | Device approvals, remote wipe and account transfer |
| **Support** | GitHub issues | Yes |

Choose Dropbox if you share folders with a team or open your files on a phone. Choose Koidrive if you want your folders on your own machines, stored in an account you control, with a bill only from Cloudflare.

## What it costs

Koidrive is free. Cloudflare runs it, at its own prices:

| | What you pay Cloudflare |
|---|---|
| **The server** | $0 on the [Workers Free](https://developers.cloudflare.com/workers/platform/pricing/) plan, within its daily allowances. When a space reaches one, its folders wait until the allowance resets at 00:00 UTC, and your changes stay on disk until then. See [Limits](docs/guide/limits.md) |
| **Storage** | The first 10 GB-month on [R2](https://developers.cloudflare.com/r2/pricing/) is free, then $0.015 per GB-month. 100 GB costs about $1.35 a month, and 500 GB about $7.35 |
| **Requests** | R2 includes 1 million writes and 10 million reads a month, then charges $4.50 per million writes and $0.36 per million reads |
| **Downloads** | No egress fee. Each download counts as read requests |

Files in the trash and old versions count as storage for the 30 days Koidrive keeps them. `koi space usage` shows how much each space stores.

## Get started

You need a Cloudflare account with R2 turned on, and a Mac with Apple silicon or a Linux machine.

### The macOS app

Koidrive for Mac is the full sync client for macOS 26 or later on Apple silicon. Set up this Mac, link folders and manage sync in the app. The menu bar shows sync status and notices, opens your folders in Finder and lets you pause or resume sync. **Needs You** asks before a large delete reaches your other machines. Deleted Files and Version History bring back deleted files and earlier versions of a file.

1. Download `Koidrive-<version>-arm64.dmg` from the [latest release](https://github.com/koidrive/koidrive/releases/latest), open it and drag Koidrive to Applications.

   With [Homebrew](https://brew.sh), one command installs the app and puts its `koi` on your `PATH`. If the `koidrive` formula is installed, first run `brew uninstall --formula koidrive/tap/koidrive`, as [Install with Homebrew](docs/guide/install.md#install-with-homebrew) explains. Install the cask with:

   ```sh
   brew install --cask koidrive/tap/koidrive
   ```

2. If you have no server, [deploy one](docs/deploy.md#deploy) in your Cloudflare account. Open `<your server>/setup` and find the claim code in the Worker's logs. For an existing account, skip this step.
3. Open Koidrive. The **Set Up Koidrive** window connects to your server, signs in this Mac and links a folder. For an existing account, use a pairing code from a signed-in device.

**Settings** has **Folders**, **Account**, **Storage**, **Problems** and **General** tabs for managing folders, devices, storage use, problems and app preferences. The [Mac app guide](docs/guide/macos.md) walks through setup and everyday use, including adding another Mac.

### The CLI for Linux, servers and scripts

The `koi` command works on Linux and Macs with Apple silicon. The Mac app includes `koi` and shares its device, profile and background sync service. Folders linked in either appear in both.

1. **Install the `koi` command.**

   ```sh
   curl -fsSL https://koidrive.com/install.sh | sh
   ```

   On macOS, the installer continues into guided setup, which installs and verifies the app and connects this machine. In an attended terminal, it configures `PATH` for zsh, bash, or fish. Start a new login shell to run `koi` by name. [Install](docs/guide/install.md) covers opting out and building from source.

   Skip steps 2 and 3 if guided setup has connected this machine and linked a folder. Otherwise, complete the steps that remain.

2. **Deploy your server and sign in this machine if you skipped guided setup.** Deploy the server with the deploy button, open `<your server>/setup` to get a claim code, and claim the server:

   ```sh
   koi server claim koidrive.alice.workers.dev ABCD-EFGH-JKLM --user "Alice Park"
   ```

   [Set up](docs/guide/setup.md#deploy-a-server-and-claim-it) walks through each step.

3. **Link a folder if setup has not linked one.**

   ```sh
   mkdir -p ~/Koidrive
   koi space create Koidrive
   koi folder link ~/Koidrive --space Koidrive
   ```

   The folder syncs in the background, including after a restart (on macOS, while you're logged in). `koi status` shows it as `UpToDate` once it has the server's files.

4. **Add your other machines.** On a machine that is signed in, run `koi device invite`. It prints a command with a pairing code. Run that command on the new machine, then link a folder there too.

The [user guide](docs/guide/README.md) covers conflicts, the trash, devices, limits and how to remove everything.

## Repository layout

| Path | Contents | License |
|---|---|---|
| [spec/](spec/) | Protocol schema in Protocol Buffers | Apache-2.0 |
| [core/](core/) | Rust sync engine crates | Apache-2.0 |
| [cli/](cli/) | Command-line client and daemon | Apache-2.0 |
| [server/](server/) | Worker and Durable Objects in TypeScript | AGPL-3.0-or-later |
| [apps/](apps/) | Desktop apps | Apache-2.0 |

## License, contributing and trademarks

The server in [server/](server/) uses the GNU AGPL v3.0 or later, and everything else uses the Apache License 2.0. [LICENSE](LICENSE) explains the split.

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you open an issue or a pull request. Report vulnerabilities privately, as [SECURITY.md](SECURITY.md) describes.

Cloudflare, Cloudflare Workers, Durable Objects and R2 are trademarks of Cloudflare, Inc. Dropbox is a trademark of Dropbox, Inc.
