# Set up

You need one server, in a Cloudflare account you own, and at least one device signed in to it. The first device claims the server and becomes its owner's device. Every other device joins with a pairing code.

On a Mac, follow [Install and set up this Mac](macos.md#install-and-set-up-this-mac) to sign in and link a folder in the app. The steps below use the CLI.

## Deploy a server and claim it

You need a Cloudflare account and the [`koi` command](install.md). R2 needs a payment method on your Cloudflare account. Usage within the free tier costs nothing ([R2 pricing](https://developers.cloudflare.com/r2/pricing/)).

Run `koi setup` and choose **Log in with Cloudflare**. Setup logs in with your Cloudflare account, deploys a server to it and claims the server with this device, then links `~/Koidrive`. If the account already has a Koidrive server, setup adds this device to it. [koi setup](koi-setup.md#the-cloudflare-login-and-what-it-may-do) describes what the login may do. `koi server deploy --interactive` deploys and claims without linking a folder.

To deploy by hand instead, deploy with Wrangler as [Deploying and upgrading a Koidrive server](../deploy.md#deploy) describes. Then run `koi setup` and choose **I deployed a server**. Enter the server's address. Setup asks the server for a one-time claim code, which the server writes to the Worker's log. In the Cloudflare dashboard, open **Workers & Pages**, select your Worker, open **Observability** and search for `Koidrive claim code`. Enter the code.

To claim a server you deployed by hand without questions, for example in a script:

1. Open `<your server>/setup` in a browser. The server writes a one-time claim code, valid for one hour, to the Worker's log. Find it in **Observability** as above.
2. Claim the server with this device. Give the server's address, the claim code and your name:

   ```sh
   koi server claim koidrive.alice.workers.dev ABCD-EFGH-JKLM --user "Alice Park"
   ```

   It prints `Paired as laptop (019a...) with https://koidrive.alice.workers.dev.` The device takes this machine's host name. Give it another with `--name`. A server that already has an owner refuses the claim code.
3. [Create a space](#create-a-space) and [link a folder](folders.md#link-a-folder) to it.

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

## Upgrade the server

Run this on a terminal:

```sh
koi server upgrade --interactive
```

It logs in with Cloudflare and uploads the server version built into your `koi`. To upgrade by hand with Wrangler, follow [Deploying and upgrading a Koidrive server](../deploy.md#upgrade). Your files, devices and settings stay. Open sync sessions move to the new version within 5 minutes. The command stops if the server runs a newer release than your `koi`, or stores its data in a newer format. `--allow-downgrade` uploads anyway, but a server older than its data stops answering until a newer one replaces it.

Never go back to an older server version. Once a newer version has moved the server's stored data forward, an older one refuses to start.
