# Deploy and upgrade

The server in `server/` is one Worker with two Durable Object classes, `AccountDO` and `SpaceDO`, and one R2 bucket, all in the owner's Cloudflare account. This page explains how it gets deployed, how the first owner claims it, how an owner who lost every device gets back in, and how storage and protocol versions move forward. The owner's steps are in [deploy.md](../deploy.md), the CLI route in [cli-setup.md](cli-setup.md), and the numbers in [deploy-and-upgrade-reference.md](deploy-and-upgrade-reference.md).

## 1. `server/` builds on its own

`koi` embeds a bundle packed from `server/` ([cli-setup.md](cli-setup.md)), and an owner who deploys by hand runs Wrangler in `server/` ([deploy.md](../deploy.md#deploy)). So `server/` builds without the rest of the repository. It has its own `package.json`, lockfile and license, commits its generated protobuf code, and imports nothing from outside. It lists no secrets, so a deploy asks the owner for none. `server/scripts/check-deploy.mjs` proves this in CI: it copies `server/` to an empty directory, installs from the lockfile, runs a dry-run deploy and claims a local server. The [deploy test](../deploy-test.md) covers the steps that need a real account.

R2 must be enabled on the account first. That is a dashboard checkout that no API or deploy tool can do.

## 2. The first owner claims with a code from the log

The threat is a stranger who finds the Worker's URL before the owner does. The trust root is access to the account that hosts the Worker. While the server has no owner, `GET /setup` issues a one-time claim code and the Worker writes it to its own log. `POST /v1/claim` with that code creates the owner and their first device in one transaction. After that, every claim code is refused, and nothing resets that.

`wrangler.jsonc` turns on persistent Workers Logs, so the owner finds the code in the dashboard after the fact. It also redacts query strings, because relay URLs carry chunk tickets. Anyone with the Workers Metadata Read-Only role can read those logs, so an owner who shares the account claims right after deploying.

A correct code always redeems. Wrong codes are counted per client address and throttled, and they never delete the code, so nobody can stop the owner's claim by guessing. The code's 60 bits keep guessers out, not the throttle.

## 3. An owner recovers with a secret they set on the Worker

An owner who lost every device generates a random key, sets it as the Worker secret `KOIDRIVE_RECOVERY_KEY`, and sends it as the code of `POST /v1/claim`. The server returns a new device of the existing owner, then the owner deletes the secret. Whoever can set the Worker's secrets can already replace its code, so recovery gives nobody new access. A recovery code in the log, like the claim code, would have handed the owner's files to anyone with read access to the log.

The key works once, because the Account DO keeps its hash, and the server refuses keys shorter than 32 characters. On a server with no owner, a correct key makes its sender the owner. `koi setup` uses that to claim a server it has just deployed. Recovery adds a device and revokes none, so the owner revokes lost devices afterwards ([RFC 0009](../../spec/rfc/0009-device-revocation.md)). The one exception is a claim that carries `setup_attempt` ([cli-setup.md section 4.6](cli-setup.md#46-an-interrupted-run-resumes-from-a-state-file)).

## 4. Storage migrates when each object starts

Each Durable Object class keeps an append-only list of SQLite steps in `server/src/migrations.ts` and records how many it has run as `schema_version`. Each object runs the missing steps in its constructor, in one transaction, so a deploy needs no migration command and any number of releases can be skipped. A step never changes once merged, and it must leave a database that already has its effect unchanged.

An object whose stored version is newer than its code throws, and every request to it fails. So a rollback across a migration fails closed, and the owner rolls forward instead. The space `format_version` is separate and only an explicit owner action changes it.

A Worker instance may run the previous release while an object runs the new one, so Worker-to-object RPC methods change additively within a release. The deploy defers code updates of an active object for up to 300 seconds, so open sync sessions move to a new release within 5 minutes and are not dropped at once.

## 5. Clients see the release and the protocol range

A release tag points to a release commit that `scripts/release-commit.py make` creates on a commit of `main`. That commit stamps the tag's version into `Cargo.toml`, the workspace packages in `Cargo.lock`, and `server/release.json`, and records the `main` commit's id in `server/release.json`. The release workflow checks that the tag's tree is exactly that stamp of a commit on `main`, then builds it as it is. So a copy of `server/` at a release tag, such as a deploy button's, knows its version and commit. `main` and other branches report `0.0.0-dev`. Server builds and bundle packing export the workspace version to `server/release.json`. `GET /v1/server` and `Welcome` report it as `server_version`, with the protocol range `proto_min` to `proto_max`. `GET /v1/server` also reports `commit` and `storage`, the schema versions its Durable Objects migrate to ([RFC 0025](../../spec/rfc/0025-server-build-and-storage.md)). An upgrade compares `storage`, not the version, because a server built from a branch can store data that no release reads yet. The client and server pick the highest common minor version, and a session with none closes with `Goodbye` code 4002. A test keeps the server serving clients one minor version behind. `Welcome.latest_release` exists, and the server leaves it empty.

## 6. Owners upgrade by deploying the new release

`koi server upgrade` uploads the new release's bundle under the server's Worker and bucket names ([cli-setup.md section 5](cli-setup.md#5-upgrade-replaces-the-code-and-delete-empties-the-bucket-first)). With Wrangler, the owner checks out the release, puts back the same two names in `wrangler.jsonc`, and runs `pnpm install --frozen-lockfile` and `pnpm run deploy` in `server/`. A different name deploys a new, empty Worker.
