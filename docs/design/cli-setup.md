# CLI setup

`koi setup` deploys a Koidrive server to the user's own Cloudflare account and connects this device to it, with one browser login and no Wrangler, Node or API token. The CLI carries the server of its own release, so `koi` 0.3.0 deploys and upgrades to server 0.3.0. The code is in `cli/src/setup.rs`, `cli/src/setup/` and `core/cloudflare`. Commands, API calls, scopes and time limits are in [cli-setup-reference.md](cli-setup-reference.md).

## 1. One command picks the route

On a terminal, `koi setup` asks one question: join with a pairing code from a device that is already set up, or log in with Cloudflare. After the login it looks in the account (section 6). With no server it deploys and claims one, with one it adds this device, and with several it asks which. Without a terminal, or with `--json` or `CI` set, `koi setup` changes nothing and lists the commands for each route. Those commands never ask a question unless `--interactive` is given, so scripts can run them. A pairing code carries its server as `code@host`, such as `K7QM-2XPD@koidrive.alice.workers.dev`, so `koi join` needs nothing else.

## 2. The login is OAuth with PKCE on a loopback port

The CLI is a public OAuth client with no secret. It uses Authorization Code with PKCE (S256) and redirects to `http://127.0.0.1:<port>/oauth/callback`. Cloudflare's authorization server lists the device-code grant ([OpenID configuration](https://dash.cloudflare.com/.well-known/openid-configuration)), but the Koidrive client is not registered for it. On a machine without a browser on its screen (an SSH session, or Linux with neither `DISPLAY` nor `WAYLAND_DISPLAY`), the CLI prints the login page and reads back the address the browser ended on. `--no-browser` and a failed browser launch do the same. A second device can also join with a pairing code, since a server needs one login, not one per device. Cloudflare revokes the whole grant when a code or a refresh token is used twice, so the CLI never resends either. A failed exchange or refresh ends the command.

## 3. The grant reaches the whole account, so it lives for one command

Workers Admin and R2 Write are the narrowest scopes that can upload a Worker and its assets. Cloudflare does not limit them to Koidrive: whoever holds the grant can replace any Worker and empty any bucket in the account. So each command keeps its tokens in memory and revokes the grant when it ends (section 7).

## 4. A deploy uploads the server and claims it with a one-time secret

### 4.1 R2 is enabled in the dashboard

R2 cannot be enabled through the API. When listing buckets fails with error 10042, the CLI opens the account's R2 page and waits until the list succeeds.

### 4.2 The CLI embeds its release's server bundle

`server/scripts/pack-bundle.mjs` builds the Worker with `wrangler deploy --dry-run` and packs it with the web assets and a manifest read from `server/wrangler.jsonc`. `cli/build.rs` embeds that directory. A Wrangler deploy and the CLI deploy therefore run the same configuration.

### 4.3 The upload goes through the Workers API

Before creating a bucket or uploading, the CLI reads the account's workers.dev subdomain. If none exists (error 10007), it suggests `koidrive-` plus eight random hex digits and lets the user edit it. `--subdomain` supplies the name without that question. The CLI registers the name with `PUT /accounts/{account_id}/workers/subdomain`. A taken name (10031) gets a random suffix, with at most five attempts. Existing subdomains stay unchanged ([Workers subdomains API](https://developers.cloudflare.com/api/resources/workers/subresources/subdomains/methods/update/)).

The CLI creates a bucket named `koidrive-` plus 16 random hex digits, so it never adopts another application's bucket. It uploads the assets, then the Worker under `koidrive` or the next free name, tagged `koidrive-server`. Then it turns on the workers.dev route and waits for `GET /v1/server`.

### 4.4 A one-time secret claims the new server

The script upload sets `KOIDRIVE_RECOVERY_KEY` to 32 random bytes, so the Worker never runs without it. On a server with no owner, a correct unused key claims it (see [deploy-and-upgrade.md](deploy-and-upgrade.md#3-an-owner-recovers-with-a-secret-they-set-on-the-worker)). The CLI claims, saves the device credential, then deletes the secret and checks that it is gone. `/setup` still issues claim codes on a CLI deploy, so someone who reads the Worker's log could claim first in the seconds before the CLI does.

### 4.5 An existing server gets a new device the same way

On a claimed server the key takes the recovery path and adds a device for the owner. The CLI stops if `KOIDRIVE_RECOVERY_KEY` is already set, because the owner set it for recovery. It claims only when `ServerInfo.setup_attempts` is true, and offers to upgrade an older server first.

### 4.6 An interrupted run resumes from a state file

Before each step that creates or deletes something, the CLI writes `setup-state.json` in the state directory. It names the account, Worker, bucket and last step, and holds no secret. A rerun continues from it. Each claim of a run sends the same `setup_attempt` id. The server revokes the earlier devices of that attempt that never refreshed a token, so lost claim responses end with one device. Before claiming, the CLI tests any saved credential with `POST /v1/token` and keeps it if the server accepts it.

## 5. Upgrade replaces the code, and delete empties the bucket first

`koi server upgrade` uploads the embedded bundle under the same names, keeping bindings, secrets and data. It refuses when the server reports a storage version above the bundle's, and when the server runs a newer release ([RFC 0025](../../spec/rfc/0025-server-build-and-storage.md)). `--allow-downgrade` uploads anyway. Each Durable Object moves to the new code within 300 seconds and migrates its storage then. A button deploy gets a warning first, because its repository's next push replaces the upgrade. `koi server delete` deletes the Worker, then empties and deletes the bucket. It keeps a bucket that another Worker binds or that holds keys outside `spaces/`.

## 6. A server is found by its tag, then by its classes

The CLI keeps Workers tagged `koidrive-server` and untagged Workers that bind `AccountDO` and `SpaceDO`, such as a renamed button deploy. It keeps only those that answer `GET /v1/server`, since anyone with the account can tag a script. A tagged Worker that binds a bucket but does not answer is a deploy that stopped halfway, and `koi setup` finishes it.

## 7. No Cloudflare credential stays on the device

The CLI revokes the grant on success, failure, Ctrl-C and SIGTERM, and retries a failed revocation three times. A killed process leaves the grant active until it expires or the user revokes it in the dashboard. The Koidrive device credential stays where the driver keeps it ([driver.md](driver.md)), and the Cloudflare grant never reaches the server.
