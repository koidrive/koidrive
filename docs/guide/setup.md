# Set up

You need one server, in a Cloudflare account you own, and at least one device signed in to it. The first device claims the server and becomes its owner's device. Every other device joins with a pairing code.

On a Mac, follow [Install and set up this Mac](macos.md#install-and-set-up-this-mac) to sign in and link a folder in the app. The steps below use the CLI.

## Deploy a server and claim it

You need a Cloudflare account with R2 turned on ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)) and the [`koi` command](install.md).

1. Deploy the server with the deploy button, or with `pnpm run deploy` in `server/`, as [Deploying and upgrading a Koidrive server](../deploy.md#deploy) describes.
2. Open `<your server>/setup` in a browser. The server writes a one-time claim code, valid for one hour, to the Worker's log. In the Cloudflare dashboard, open your Worker, then **Observability**, and search for `Koidrive claim code`.
3. Claim the server with this device. Give the server's address, the claim code and your name:

   ```sh
   koi server claim koidrive.alice.workers.dev ABCD-EFGH-JKLM --user "Alice Park"
   ```

   It prints `Paired as laptop (019a...) with https://koidrive.alice.workers.dev.` The device takes this machine's host name. Give it another with `--name`. A server that already has an owner refuses the claim code.
4. [Create a space](#create-a-space) and [link a folder](folders.md#link-a-folder) to it.

## Add another device

On a device that is already signed in, run:

```sh
koi device invite
```

It prints a pairing code, which works once and for 10 minutes, and the command to run on the new machine:

```
K7QM-2XPD@koidrive.alice.workers.dev
On the new device, run:
  koi join K7QM-2XPD@koidrive.alice.workers.dev
The code works once, for about 10 minutes.
```

Only the code goes to stdout, so `code=$(koi device invite)` captures it. The rest goes to stderr.

The macOS app shows a pairing code without the server's address, under **Settings**, **Account**, **Add a Device**. To use that code with the `koi` command, add the address: `koi join K7QM-2XPD --server https://koidrive.alice.workers.dev`.

## Create a space

```sh
koi space create Photos
```

This prints the new space's ID. `koi folder link --space` finds a space by its name, so the server refuses a name you already use. Names are compared exactly after leading and trailing whitespace is removed, so `Photos` and `photos` are two names. `koi space list` lists your spaces with their IDs. Then [link a folder](folders.md#link-a-folder) to the space.

## Set up with the Cloudflare login

`koi setup` can do all of this in one guided run on a terminal. It logs in with Cloudflare, deploys a server or finds yours, signs in this machine, and links a folder. The Cloudflare login is in early access for members of the project's test account. Everyone else deploys and claims the server as described above. The same login is behind `koi server deploy`, `add-device`, `upgrade` and `delete`. [koi setup](koi-setup.md) describes each question and option.

## Upgrade the server

With the Cloudflare login, run this on a terminal:

```sh
koi server upgrade --interactive
```

It uploads the server version built into your `koi`. Without the login, upgrade as [Deploying and upgrading a Koidrive server](../deploy.md#upgrade) describes. Your files, devices and settings stay. Open sync sessions move to the new version within 5 minutes. The command stops if the server runs a newer release than your `koi`, or stores its data in a newer format. It also stops if the server was deployed from a branch and does not report its data format. `--allow-downgrade` uploads anyway, but a server older than its data stops answering until a newer one replaces it.

A server deployed with the deploy button deploys again whenever its repository changes, and that would replace the upgrade. The command warns you and asks first. Update that repository too, or answer no. `--yes` goes on without asking.

Never go back to an older server version. Once a newer version has moved the server's stored data forward, an older one refuses to start.
