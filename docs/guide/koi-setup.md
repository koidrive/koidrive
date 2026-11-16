# koi setup

`koi setup` sets up this machine in one run: it connects to a server, signs this machine in as a device, and links a folder. Deploy your server first, as [Deploying and upgrading a Koidrive server](../deploy.md#deploy) describes.

## Connect this device and link a folder

On a terminal, `koi setup` asks **How do you want to connect?** Use the arrow keys and Enter:

1. **I deployed a server.** Enter your server's address, such as `https://koidrive.alice.workers.dev`.
   - A new server has no owner yet. Setup asks it for a one-time claim code, which the server writes to its Worker's log. In the Cloudflare dashboard, open **Workers & Pages**, select your Worker, open **Observability** and search for `Koidrive claim code`. Enter the code at **Claim code**. Setup claims the server, and this device becomes your first device. Your name on the server is your login name. If the code is wrong or has expired, setup asks again.
   - If the server already has an owner, setup asks for a pairing code. On a signed-in device, run `koi device invite`, or open **Settings**, **Account**, **Add a Device…** in the macOS app.
   - If setup can't reach the address, or it isn't a Koidrive server, setup asks again.
2. **Enter a pairing code** from another device, written as `<code>@<host>`. On that device, run `koi device invite`. For a code without the host, choose **I deployed a server** and enter the server's address first.

Setup uses `~/Koidrive` for your files:

- With no shared folders on the server, setup creates one named `Koidrive`. Otherwise, select a shared folder or **Create a shared folder**. Enter a new name. If the name exists, setup asks again. Shared folders with the same name show a short ID.
- If `~/Koidrive` is missing or empty, setup creates or links it without another question. Files excluded by the built-in rules, such as `.DS_Store`, do not make a folder nonempty.
- If the folder holds files, **What should Koidrive do?** offers **Use another folder** first and **Sync these files** second. Syncing uploads the local files and downloads the shared folder's files. To keep those local files out of sync, select **Use another folder** and enter a path at **Folder to sync**. A path to a file, an unreadable path, or an unwritable folder also leads to this path question.

Setup resolves the shared folder before asking about local files. If it finds an existing shared folder while creating `Koidrive`, select which shared folder to sync. The file question names that folder and asks for consent once.

Setup links the folder and starts background sync. If you cancel a folder question, this device stays connected and no folder is linked.

The daemon checks the folder again before it links it. If the folder was replaced, or files you didn't approve arrived while setup waited, it refuses the link, and you run `koi setup` again.

The last question is **Send usage reports?**, unless you already chose or an environment variable decides it. Setup asks it at the end of the run, whether setup succeeded, failed or was cancelled. Until you answer, the reports of this setup wait on your machine. Yes sends them and later reports. No, or closing the question, deletes them. Press Enter for Yes. [Usage reports](telemetry.md) lists what they hold.

When background sync is running, setup ends with:

```text
Setup complete. Add files to ~/Koidrive.
Check sync: koi status
```

If background sync is off in `config.toml`, setup reports **Folder linked. Background sync is off in config.toml.** A daemon you started separately can still run. Set `background = true` under `[service]` in `config.toml`, then run `koi service start`. Paused, blocked, or failed sync shows its reason and recovery command rather than a successful finish. If a folder has pending decisions, review them with `koi resolve`.

On Linux, if sync stops when you log out, run `sudo loginctl enable-linger "$USER"` to keep sync running without a login session.

If the final status check does not answer within five seconds, setup reports **Folder linked. Could not confirm background sync.** Run `koi status` to check again.

On an Apple silicon Mac, attended setup also installs the matching release of Koidrive.app in `/Applications`, or `~/Applications` when `/Applications` is not writable, and requests that it open. It verifies the release checksum, the app's Developer ID and notarization before replacing an older copy. A verified equal or newer app needs no download. Add `--no-app` to skip installation and opening. JSON, unattended setup, CI and other platforms skip this step.

If the app step fails, the device stays connected and its linked folders stay linked. Setup reports the completed work and the app failure separately. The JSON `data.app` field reports `skipped`, `present` or `failed`; an incomplete launch, handoff or cleanup appears as `problem` on a `present` result. A requested launch is not confirmation that the app attached. The `data.service` result describes background sync independently.

## Run it again to finish or check

If setup stops halfway, run `koi setup` again. A connected device skips the connect question. With linked folders, it lists them without changing their links. When sync is running, it ends with **Setup is already complete. Your folders have not changed.** and `Check sync: koi status`. Without a linked folder, it continues with folder setup.

## Options

| Option | Effect |
|---|---|
| `--name <name>` | The device's name. The default is this machine's host name |
| `--plain` | Numbered, line-oriented questions without color or animation. Enter a number to select an option. `KOI_PLAIN=1`, narrow terminals, and `TERM=dumb` do the same. A nonempty `NO_COLOR` turns off color only |
| `--interactive` | Runs the guided setup even where setup would otherwise ask nothing. Fails with exit status 2 without a terminal, and can't be used with `--json` |
| `--no-browser` | Prints the Cloudflare login page's address instead of opening a browser. Needs `--interactive` |
| `--port <port>` | The local port for the Cloudflare login callback, for example to forward it over SSH |
| `--telemetry on` or `off` | Stores the usage-report choice without asking |
| `--no-app` | Skips installing and opening the macOS app |
| `--folder`, `--space`, `--pairing-code-stdin`, `--accept-existing-files` | Run a [scripted setup](#scripted-setup) |

Claim codes, pairing codes and pasted login callback addresses are hidden while you type.

With `--no-browser`, open the printed address on any device, log in, and select **Authorize**. The browser then fails to load a page on `127.0.0.1`. Copy that page's address and paste it into the terminal.

If opening the browser fails, setup prints the full address to open. If no tab appears despite a successful browser request, rerun with `--interactive --no-browser`.

## Without a terminal, setup asks nothing

Without a terminal on stdin and stderr, with `--json`, or with the `CI` environment variable set, `koi setup` asks nothing. Unless `--folder` asks for a scripted setup, it lists the setup commands and exits 0. With `--json` they are in `data.choices` and `next_commands`:

- `koi server claim <server-url> <code> --user <name>` claims the server you deployed. Open `<server-url>/setup` first, then find the code in the Worker's log.
- `koi join <code>` signs this device in with a pairing code.
- `koi setup --interactive` runs the guided setup on a terminal.

## The Cloudflare login and what it may do

`koi server deploy --interactive` and `koi server add-device --interactive` log in with Cloudflare. The login works only for members of the project's test account ([Set up with the Cloudflare login](setup.md#set-up-with-the-cloudflare-login)), so `koi setup` doesn't offer it.

The login asks for four permissions: reading the account's settings, managing its Workers, writing to its R2 buckets, and keeping access while the command runs (`account-settings.read`, `workers-scripts.admin`, `workers-r2.write` and `offline_access`). Koidrive revokes the login when the command ends. If it can't, it says so, and you can revoke it under **Manage OAuth authorizations** in the Cloudflare dashboard.

`koi server deploy --interactive` deploys a new server and claims it, and stops if the account already has one. If R2 isn't turned on in the account, it opens the R2 page of the dashboard and waits up to 30 minutes while you complete its checkout. `koi server add-device --interactive` adds this device to the server in the account without a pairing code. `--worker <name>` names the Worker when there are several. Without `--interactive` and a terminal, these commands open no browser. They stop with `auth_required` (exit 4) and name the same command with `--interactive`.

## Scripted setup

To pair and link a folder in one command, give the pairing code on stdin:

```sh
printf '%s\n' "$KOI_PAIRING_CODE" | koi setup --pairing-code-stdin \
  --folder "$HOME/Koidrive" --space "$KOI_SPACE" --telemetry off --json
```

With `--folder`, setup asks nothing, with or without a terminal. `--space` takes a space's ID or name, as `koi space list` shows it, and must name a space that exists. A name that matches several spaces is refused with their IDs. Setup creates a missing folder. A folder that already holds files needs `--accept-existing-files`: its files upload, and the space's files download into it. Setup checks the arguments, the folder and the code before it uses the code, which works once.

On a machine that is already connected, `--pairing-code-stdin` is optional, and setup links the folder without pairing again. A code for another server is refused. A machine that isn't connected needs the code, or setup stops with `auth_required` (exit 4). Running the same command again changes nothing and reports `unchanged`.

With `--json`, `data` holds `account` (the device this machine is connected as), `folder` (the linked folder, with its `space_id`, `space_name` and `state`), `service` (the background service) and `app` (the macOS app step). [docs/json-schema/v1/koi-setup.json](../json-schema/v1/koi-setup.json) is the schema.
