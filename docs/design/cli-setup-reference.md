# CLI setup reference

The commands, Cloudflare calls and limits behind `koi setup`. [cli-setup.md](cli-setup.md) explains how they fit together. Section numbers match that page.

## Commands (section 1)

| Command | What it does |
|---|---|
| `koi setup` | On a terminal: asks to join with a code or log in with Cloudflare, then deploys, adds this device or asks which server. Otherwise: lists the commands below and changes nothing |
| `koi setup --pairing-code-stdin --folder <PATH> --space <SPACE>` | Joins with a code read from stdin and links the folder, asking nothing |
| `koi join <code@host>` | Joins the server named in the code. `--server` gives the host for a code without one |
| `koi server deploy` | Logs in, deploys a new server and claims it. Fails if the account has a server |
| `koi server add-device` | Logs in and adds this device to the server, without a pairing code |
| `koi server claim <URL> <CODE> --user <NAME>` | Claims a server deployed by hand with the code its `/setup` page wrote to the Worker's log. Needs no Cloudflare login |
| `koi server upgrade` | Logs in and uploads this CLI's server |
| `koi server delete` | Logs in, deletes the Worker, then empties and deletes its bucket |

A command asks questions only with a terminal on stdin and stderr, and without `--json` or the `CI` variable. A command that logs in with Cloudflare runs only with `--interactive`. Without it, the command stops with `auth_required` (exit 4) and names the same command with `--interactive`. `--no-browser` also needs `--interactive`. With several servers in the account, `add-device`, `upgrade` and `delete` need `--worker <name>`. `--yes` skips the confirmation of `upgrade` and `delete`.

## Login (section 2)

- Endpoints: `https://dash.cloudflare.com/oauth2/auth`, `/oauth2/token` and `/oauth2/revoke` ([Integrate](https://developers.cloudflare.com/fundamentals/oauth/integrate-with-cloudflare/)). The client id is `CLIENT_ID` in `core/cloudflare/src/oauth.rs`.
- The listener binds `127.0.0.1` only, on `--port` or a free port. Cloudflare accepts any port on `http://127.0.0.1`.
- The code is exchanged once. Exchanging a code twice, or reusing a rotated refresh token, revokes the whole grant.
- A token answer with fewer scopes than requested revokes the new grant and fails with `MissingScopes`.
- `GET /client/v4/accounts` returns only the account picked on the consent page.

## Scopes (section 3)

| Scope | Needed for |
|---|---|
| `account-settings.read` | `GET /accounts`, to learn the picked account |
| `workers-scripts.admin` | Script and asset upload, secrets, the workers.dev route, settings, script delete |
| `workers-r2.write` | Bucket list, create and delete, object list and delete |
| `offline_access` | The refresh token |

Workers Editor (`workers-scripts.edit`) gets 403 on script and asset uploads. The dashboard's scope picker does not offer Workers Scripts Write.

## Deploy calls (section 4)

All calls go to `https://api.cloudflare.com/client/v4/accounts/<id>` with the access token.

1. `GET /r2/buckets`. Error 10042 means R2 is off: the CLI opens `https://dash.cloudflare.com/<id>/r2/overview` and polls every 5 seconds.
2. `POST /r2/buckets` with the name from the state file. A 409 for that name means an earlier try of this deploy created it.
3. `POST /workers/scripts/<name>/assets-upload-session`, then `POST /workers/assets/upload?base64=true` per bucket of hashes. An empty bucket list means every file is uploaded, and the session's JWT completes the upload ([Direct uploads](https://developers.cloudflare.com/workers/static-assets/direct-upload/)).
4. `PUT /workers/scripts/<name>` with the module and metadata: the bindings, `tags: ["koidrive-server"]`, `keep_bindings: ["secret_text", "secret_key"]`, the migrations, `observability`, `code_update_strategy: {"mode": "deferred", "max_delay": 300}`, and on a deploy the `KOIDRIVE_RECOVERY_KEY` secret.
5. `POST /workers/scripts/<name>/subdomain` with `{"enabled": true, "previews_enabled": false}`. The server is at `https://<name>.<subdomain>.workers.dev`.
6. Poll `GET <url>/v1/server` every 3 seconds.

Migrations follow Wrangler's rule. A script with no tag gets every step. A script with a tag gets the steps after it, or none. A deployed tag the bundle does not know means a newer deployment, and the CLI stops.

## Claim (sections 4.4 and 4.5)

- The key is 32 random bytes, base64url without padding. The server's floor is 32 characters.
- `POST <url>/v1/claim` sends `ClaimRequest` with the key as `code` and the run's `setup_attempt` (16 random bytes).
- The CLI retries while the server cannot see the key yet: "no valid claim code; open /setup to issue one" and "this deployment already has an owner". It also retries `RETRY_LATER`, 5xx answers and transport errors.
- "this recovery key was already used; set a new one", and the refusal of a claim that a later claim of the same attempt replaced, spend the key. The CLI sets a new key, at most 5 per run.
- After a claim: save the credential, then `DELETE .../secrets/KOIDRIVE_RECOVERY_KEY` and list the secrets to confirm. Other secrets stay.

## Limits

| Wait | Limit |
|---|---|
| Browser login | 10 minutes |
| R2 enabled in the dashboard | 30 minutes |
| Server answers after an upload | 90 seconds |
| Claim on a server that already runs the key claim | 60 seconds per key |
| Claim on an unclaimed or just-upgraded server | 330 seconds per key |
| `setup_attempts` after an upgrade | 330 seconds, polled every 5 seconds |
| Access token refreshed before a long step | when less than 5 minutes are left |
| Grant revocation | 1 try and 3 retries, 2 seconds apart |

## State file (section 4.6)

`setup-state.json` in the state directory, written by atomic rename. Fields: `account`, `script`, `bucket`, `step`, `secret_set`, `setup_attempt`, `jurisdiction`, and during an uninstall `script_created_on` and `bucket_created_on`. A command stops before calling the account if the file belongs to another unfinished run: an uninstall, another Worker's deploy, or another account.

| Stopped after | A rerun |
|---|---|
| Names chosen | Creates the bucket the file names |
| Bucket created | Uploads the script. A Worker under the saved name that is not this deploy's moves it to the next free name |
| Script uploaded | Sets a new key, turns on workers.dev and claims |
| Serving | Sets a new key and claims with the same `setup_attempt` |
| Credential saved, key not deleted | Deletes the key |
| Uninstalling | Continues the uninstall |

Before a claim, the CLI sends a saved credential for this server to `POST /v1/token`. An access token means this device is set up, so the CLI skips the claim. `UNAUTHENTICATED` removes the credential and the claim goes on. Any other answer stops the run and changes nothing.

## Delete (section 5)

1. Find the server, read the bucket and its jurisdiction from the R2 binding, count the objects and ask for confirmation.
2. Record the Worker's `created_on` and the bucket's `creation_date` in the state file.
3. Delete the Worker with `?force=true`, unless its `created_on` changed, which means a new Worker took the name.
4. Keep the bucket if its `creation_date` or jurisdiction changed, if another Worker binds it, or if it holds a key outside `spaces/`.
5. List and delete the objects page by page until a list comes back empty.
6. Delete the bucket, then the state file.

Every R2 call sends `cf-r2-jurisdiction`. A 404 counts as done, and a missing bucket ends the uninstall as complete. With the Worker already gone, `koi server delete --bucket <name> [--jurisdiction <name>]` runs steps 1 and 2 for the bucket alone, then 4 to 6.

## Public OAuth client

`CLIENT_ID` is the public client "Koidrive", owned by the account that holds the koidrive.com zone. Any Cloudflare user can authorize it. It allows the grants `authorization_code` and `refresh_token`, the token method `none`, the scopes above and the redirect `http://127.0.0.1:8976/oauth/callback`; Cloudflare accepts any port on `127.0.0.1`. Its client URL `https://koidrive.com` is verified by a DNS `TXT` record `cloudflare_oauth_client_publisher=…` on koidrive.com. Public visibility and the verified domain can't be undone or changed ([Create an OAuth client](https://developers.cloudflare.com/fundamentals/oauth/create-an-oauth-client/)).
