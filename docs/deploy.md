# Deploying and upgrading a Koidrive server

`koi setup` deploys and claims your server through a [Cloudflare login](guide/koi-setup.md#the-cloudflare-login-and-what-it-may-do), and `koi server upgrade` upgrades it. This page does the same by hand with Wrangler. The design behind both routes is in [design/deploy-and-upgrade.md](design/deploy-and-upgrade.md).

## What you need

- A Cloudflare account with R2 activated. If you have never used R2, open the dashboard, go to **Storage & databases**, **R2**, **Overview**, and complete the checkout there. The checkout asks for a payment method. R2 includes free monthly usage and bills usage beyond it ([R2 get started](https://developers.cloudflare.com/r2/get-started/#before-you-begin), [Billing policy](https://developers.cloudflare.com/billing/understand/billing-policy/)). Wrangler can't activate R2, and it can't create the bucket until R2 is active ([workers-sdk#15468](https://github.com/cloudflare/workers-sdk/issues/15468)).
- Git, Node.js 22.12 or later, and pnpm.
- The [Mac app](guide/macos.md) or the [`koi` command](guide/install.md), to claim the server.

## Deploy

1. Clone the repository and check out the release you want.
2. In `server/`, run `pnpm install --frozen-lockfile` and `pnpm run deploy`. Wrangler asks you to sign in. It names the Worker and the bucket `koidrive` unless you change them in `server/wrangler.jsonc`.
3. Note the Worker's address, for example `https://koidrive.<your-subdomain>.workers.dev`.

If the deploy stops with an error about the R2 subscription, R2 is not activated yet. Complete the checkout under [What you need](#what-you-need) and deploy again.

## Claim your server

A new server has no owner until someone claims it, and the first valid claim makes that person the owner for good. Claim it right after you deploy.

1. Run `koi setup` and choose **I deployed a server**, or open the setup window of the macOS app. Enter your server's address. Setup asks the server for a one-time claim code. The code is valid for one hour, and the server writes it to the Worker's log. If you have neither `koi` nor the app, open `<your server>/setup` in a browser to get the code.
2. Open [Workers & Pages](https://dash.cloudflare.com/?to=/:account/workers-and-pages), select your Worker, select **Observability** and search for `Koidrive claim code` ([Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/#view-logs-from-the-dashboard)). Or, before step 1, run `pnpm exec wrangler tail <your Worker>` in `server/` and read the code there.
3. Enter the code in setup. Or claim the server with `koi server claim <your server> <code> --user "<your name>"`. That device becomes your first device ([Set up](guide/setup.md#deploy-a-server-and-claim-it)).

If the code expired, `koi setup` asks the server for a new one. If you got the code in a browser, open `/setup` again. If a claim stops before it finishes, run the same claim again within the hour with the same code. Someone who finds your address cannot see the code, and their wrong guesses do not affect yours.

Anyone who can read your Worker's logs can read the code while it is valid, and could claim the server with it. That is anyone in your account, or any API token, with the Workers Metadata Read-Only role or higher ([Workers roles](https://developers.cloudflare.com/workers/authorization/workers/)). Workers Logs keeps logs for 3 days on Workers Free and 7 days on Workers Paid ([Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/)), but a code is useless once the server has an owner or after an hour. The logs hide request query strings, so they never keep file download or upload links.

## If you lose every device

`koi server add-device --interactive` adds a new device to your account through the Cloudflare login ([Devices](guide/devices.md#if-you-lose-every-device)). It sets a one-time recovery key on your Worker, claims the server with it, and deletes the key. Without that login, do the same steps by hand, as long as you can still change the Worker's settings:

1. Make a random key of at least 32 characters, for example with `openssl rand -hex 32`.
2. In the dashboard, open your Worker's **Settings**. Under **Variables and Secrets**, select **Add**, choose the type **Secret**, name it `KOIDRIVE_RECOVERY_KEY` with that value, and select **Deploy** ([Secrets](https://developers.cloudflare.com/workers/configuration/secrets/)). Or, in `server/`, run `pnpm exec wrangler secret put KOIDRIVE_RECOVERY_KEY --name <your Worker>`.
3. On the new device, claim the server with the key instead of a claim code: `koi server claim <your server> <key> --user "<your name>"`. The device joins your existing account, which keeps its name. A new secret can take a moment to reach the server. If the claim says the deployment already has an owner right after you set the key, wait a minute and run it again.
4. Delete the secret. Each key works once, so set a new one to recover again.

Recovery does not sign out your other devices. A lost device keeps working until you revoke it with `koi device revoke` ([Devices](guide/devices.md#revoke-a-device)).

## Upgrade

Your server keeps working while you upgrade. Each part moves its stored data to the new layout the first time it starts on the new release, and you can skip releases. `koi server upgrade` runs the upgrade for you ([Set up](guide/setup.md#upgrade-the-server)). It stops if the new release is older than your server's data.

Upgrade only to a newer release. To see what your server runs, run this in a terminal:

```sh
curl -H 'Accept: application/json' https://<your server>/v1/server
```

`serverVersion` is the release your server runs, and `storage` is the version of its data format. A server deployed from `main` reports `0.0.0-dev`, and its data can be newer than the latest release. Upgrade it only to a release published after the day you deployed it.

To upgrade by hand:

1. Check out the new release.
2. If you deployed under names other than `koidrive`, put your names back into `server/wrangler.jsonc`. The release's file has the defaults, and deploying with them creates a new, empty server next to yours.
3. In `server/`, run `pnpm install --frozen-lockfile` and `pnpm run deploy`.

## Never go back to an older release

Once a newer release has moved your stored data forward, an older release refuses to start rather than misread it. Your server then stops answering until you deploy that newer release or a later one again.
