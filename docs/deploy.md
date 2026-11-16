# Deploying and upgrading a Koidrive server

This page deploys the server to your own Cloudflare account with the deploy button or Wrangler, claims it, and upgrades it by hand. `koi setup`, `koi server deploy` and `koi server upgrade` do the same steps through a Cloudflare login, which works only for members of the project's test account ([Set up with the Cloudflare login](guide/setup.md#set-up-with-the-cloudflare-login)). The design behind both routes is in [design/deploy-and-upgrade.md](design/deploy-and-upgrade.md).

## What you need

- A Cloudflare account with R2 activated. If you have never used R2, open the dashboard, go to **Storage & databases**, **R2**, **Overview**, and complete the checkout there. R2 includes free monthly usage and bills usage beyond it ([R2 get started](https://developers.cloudflare.com/r2/get-started/#before-you-begin)). Neither the deploy button nor Wrangler can activate R2, and without it the bucket cannot be created ([workers-sdk#15468](https://github.com/cloudflare/workers-sdk/issues/15468)).
- For the deploy button, a GitHub or GitLab account. The button copies the server into a new repository there.
- For the command line, Node.js and pnpm.
- The [Mac app](guide/macos.md) or the [`koi` command](guide/install.md), to claim the server.

## Deploy

1. Open the deploy button link for the release you want: `https://deploy.workers.cloudflare.com/?url=https://github.com/koidrive/koidrive/tree/<release>/server`. Cloudflare's [deploy button docs](https://developers.cloudflare.com/workers/platform/deploy-buttons/) show a branch in that place.
2. Sign in, pick the Git account for the new repository, and keep or change the names of the Worker and the R2 bucket. Write the names down, because an upgrade must keep them.
3. Deploy. Cloudflare creates the Worker, its two Durable Objects and the R2 bucket, builds the server and deploys it. You enter no secrets.
4. Note the Worker's address, for example `https://koidrive.<your-subdomain>.workers.dev`.

If the deploy stops with an error about the R2 subscription, R2 is not activated yet. Complete the checkout under [What you need](#what-you-need) and deploy again.

To deploy from the command line instead, clone the repository, then in `server/` run `pnpm install --frozen-lockfile` and `pnpm run deploy`. Wrangler asks you to sign in. It names the Worker and the bucket `koidrive` unless you change them in `server/wrangler.jsonc`.

## Claim your server

A new server has no owner until someone claims it, and the first valid claim makes that person the owner for good. Claim it right after you deploy.

1. Open `<your server>/setup` in a browser. It writes a one-time claim code, valid for one hour, to the Worker's log.
2. In the Cloudflare dashboard, open **Workers & Pages**, select your Worker, open **Observability** and search for `Koidrive claim code`.
3. Claim the server with `koi server claim <your server> <code> --user "<your name>"`, or enter the address and the code in the setup window of the macOS app. That device becomes your first device ([Set up](guide/setup.md#deploy-a-server-and-claim-it)).

If the code expired, open `/setup` again for a new one. Someone who finds your address cannot see the code, and their wrong guesses do not affect yours.

Anyone who can read your Worker's logs can read the code while it is valid, and could claim the server with it. That is anyone in your account, or any API token, with the Workers Metadata Read-Only role or higher ([Workers roles](https://developers.cloudflare.com/workers/authorization/workers/)). Workers Logs keeps logs for 3 days on Workers Free and 7 days on Workers Paid ([Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/)), but a code is useless once the server has an owner or after an hour. The logs hide request query strings, so they never keep file download or upload links.

## If you lose every device

`koi server add-device --interactive` adds a new device to your account through the Cloudflare login ([Devices](guide/devices.md#if-you-lose-every-device)). It sets a one-time recovery key on your Worker, claims the server with it, and deletes the key. Without that login, do the same steps by hand, as long as you can still change the Worker's settings:

1. Make a random key of at least 32 characters, for example with `openssl rand -hex 32`.
2. In the dashboard, open your Worker's **Settings**. Under **Variables and Secrets**, select **Add**, choose the type **Secret**, name it `KOIDRIVE_RECOVERY_KEY` with that value, and select **Deploy** ([Secrets](https://developers.cloudflare.com/workers/configuration/secrets/)). Or, in `server/`, run `pnpm exec wrangler secret put KOIDRIVE_RECOVERY_KEY --name <your Worker>`.
3. On the new device, claim the server with the key instead of a claim code: `koi server claim <your server> <key> --user "<your name>"`. The device joins your existing account, which keeps its name. A new secret can take a moment to reach the server. If the claim says the deployment already has an owner right after you set the key, wait a minute and run it again.
4. Delete the secret. Each key works once, so set a new one to recover again.

Recovery does not sign out your other devices. A lost device keeps working until you revoke it with `koi device revoke` ([Devices](guide/devices.md#revoke-a-device)).

## Upgrade

Your server keeps working while you upgrade. Each part moves its stored data to the new layout the first time it starts on the new release, and you can skip releases. With the Cloudflare login, `koi server upgrade` does this for you ([Set up](guide/setup.md#upgrade-the-server)). It stops if the new release is older than your server's data.

Upgrade only to a newer release. To see what your server runs, run this in a terminal:

```sh
curl -H 'Accept: application/json' https://<your server>/v1/server
```

`serverVersion` is the release your server runs, and `storage` is the version of its data format. A server deployed from `main` reports `0.0.0-dev`, and its data can be newer than the latest release. Upgrade it only to a release published after the day you deployed it.

If you deployed with the button, your repository is a copy that does not update itself:

1. Download the release's source and take its `server/` directory.
2. Replace the files in your repository with it, but keep your Worker `name` and your `bucket_name` in `wrangler.jsonc`. With other names, Cloudflare deploys a new, empty server next to yours.
3. Commit and push. Cloudflare builds and deploys the new release.

If you deployed from the command line:

1. Check out the new release.
2. If you deployed under names other than `koidrive`, put your names back into `server/wrangler.jsonc`. The release's file has the defaults, and deploying with them creates a new, empty server next to yours.
3. In `server/`, run `pnpm install --frozen-lockfile` and `pnpm run deploy`.

## Never go back to an older release

Once a newer release has moved your stored data forward, an older release refuses to start rather than misread it. Your server then stops answering until you deploy that newer release or a later one again.
