# Uninstall

## Remove Koidrive from one device

```sh
koi uninstall
```

Koidrive lists what it will remove and asks before it changes anything. Then it:

1. Quits the Koidrive app.
2. Signs this device out of the server. Other devices then list it as revoked.
3. Stops and removes the [background service](service.md).
4. Deletes the settings, the sync state and the logs: `~/.config/koidrive`, `~/.local/state/koidrive`, and on macOS `~/Library/Logs/Koidrive` and the app's files in `~/Library`.
5. Deletes the `.koidrive` folder in each folder it synced. A part of it that still holds files stays, and the summary names it. `.koidrive/trash/` holds files that sync replaced in the last 7 days.
6. Removes the line the installer added to your shell profile.
7. Removes the app and the `koi` command. For a Homebrew install, it runs `brew uninstall --cask koidrive` or `brew uninstall --formula koidrive`.

The files in your folders stay where they are. The summary lists what Koidrive removed and what it kept. If a step fails, the summary says why and Koidrive keeps the `koi` command, so you can run `koi uninstall` again to finish.

If another Koidrive profile on this computer runs its own background service, `koi uninstall` removes only this profile. It keeps the app, the `koi` command and the PATH line, and prints the command that removes the other profile.

- `--yes` removes without asking. Without a terminal, `koi uninstall` needs it.
- `--keep-signed-in` keeps this device on the server's device list. To revoke it later, run `koi device revoke <id>` on another device.
- `--last-device` signs out even if this is your account's last active device or the server doesn't answer. Read [sign-out confirmation and owner recovery](devices.md#sign-this-device-out) first. Without it, `koi uninstall` asks on a terminal. Otherwise it keeps the device signed in and removes everything else.
- `--json` prints the result as JSON. `data.removed` and `data.kept` list each path, and `data.device` says whether the device signed out.

If you installed only the app, run `/Applications/Koidrive.app/Contents/Helpers/koi uninstall`. A `koi` you built from source stays. Remove it with `cargo uninstall koidrive-cli`.

## Remove everything

This deletes the server: the Worker, its Durable Objects and the R2 bucket with every file stored on the server, including the trash and old versions. It can't be undone. The copies in your linked folders stay on each device. To keep a copy of the bucket first, see [Back up the server](backup.md).

Without the [Cloudflare login](setup.md#set-up-with-the-cloudflare-login), delete the Worker and the R2 bucket in the Cloudflare dashboard. First check that no other Worker binds the bucket and that it holds nothing but the `spaces/` folder Koidrive writes, since emptying it deletes everything in it. R2 deletes a bucket only once it is empty ([Delete buckets](https://developers.cloudflare.com/r2/buckets/delete-buckets/)). Whether deleting the Worker in the dashboard also deletes its Durable Objects' data is unverified.

With the Cloudflare login, run this on a terminal:

```sh
koi server delete --interactive
```

You log in with Cloudflare. Koidrive finds the server, names the bucket and the number of objects in it, and asks before it deletes anything. It deletes the Worker, then empties and deletes the bucket. If another Worker in the account uses the same bucket, Koidrive keeps the bucket. If the delete stops halfway, run the command again to finish it. When this device syncs with the deleted server at its workers.dev address, Koidrive also stops and removes this device's background service.

- `--worker <name>` picks the server when the account holds several.
- `--yes` deletes without asking.
- `--bucket <name>` empties and deletes a bucket whose Worker is already gone. Add `--jurisdiction` when the bucket is in the `eu`, `us`, `fedramp` or `fedramp-high` jurisdiction.

Then run `koi uninstall --keep-signed-in` on each device, since the server is gone.
