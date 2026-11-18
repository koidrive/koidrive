# Testing a deploy on a real account

The **Deploy test** workflow ([`deploy-test.yml`](../.github/workflows/deploy-test.yml)) deploys `server/` from `main` to the test Cloudflare account under a throwaway name, claims it, tests it, and deletes everything again. It checks what CI's dry run and `wrangler dev` cannot: Durable Objects and R2 on Cloudflare's network, the claim code in the Worker's own log, point-in-time recovery for a rewind, and an upgrade over existing data. Maintainers start it by hand, and each run waits for approval because the workflow holds the account's API token.

To run a smoke test, start the workflow from `main` and approve it on the run page:

```sh
gh workflow run deploy-test.yml --ref main -f mode=smoke
```

In the browser, open **Actions**, **Deploy test**, **Run workflow**, keep `main` under "Use workflow from", and pick the mode. When the run page asks, click **Review deployments**, select `deploy-test` and **Approve and deploy**. A second run waits until the first one ends. The scripts are in `server/scripts/deploy-test/`.

## Modes

| Mode | What it checks |
|---|---|
| `smoke` | The server's HTTP API, R2 and a rewind on the real Durable Object |
| `upgrade` | Data written by an earlier release survives an upgrade to this commit |
| `e2e` | A macOS and a Linux device sync one space with `koi` |
| `bench` | Rust chunk upload speed at several concurrency values |
| `bench-small` | Upload time, request count and Worker CPU for many small files, with and without bundles |
| `first-sync` | How a macOS client built from several refs syncs 1.9 million small entries for the first time |

```sh
gh workflow run deploy-test.yml --ref main -f mode=upgrade -f from_ref=<commit or tag>
gh workflow run deploy-test.yml --ref main -f mode=e2e
gh workflow run deploy-test.yml --ref main -f mode=bench -f bench_ns=1,2,4,8 -f bench_repetitions=3
gh workflow run deploy-test.yml --ref main -f mode=bench-small -f bench_small_files=20000 -f bench_small_kib=16 -f bench_small_runs=3
gh workflow run deploy-test.yml --ref main -f mode=first-sync -f first_sync_refs=v0.1.0-alpha.7,main,<branch> -f first_sync_rotation=0
```

**smoke** runs `smoke.ts`, a Vitest suite that stops at the first failure. It checks that:

- `/v1/server` reports an owner, `/setup` keeps its notice, and the server refuses a second claim.
- A second device pairs and refreshes its own token, and both devices list a new space.
- Chunks upload through the Worker into R2, which checks each SHA-256, and the client does not upload committed chunks again.
- The second device reads the tree and downloads every file.
- Share links serve a file without a password, and serve a folder only after the recipient enters its password. A revoked link serves nothing.
- R2 holds the chunks and the space's export: `space.json` and the first log segment, which the server writes five minutes after the first commit ([RFC 0004](../spec/rfc/0004-r2-export-format.md)).
- A rewind to a point between two commits ([RFC 0003](../spec/rfc/0003-space-rewind.md)) restores that tree, refuses commits from the old history, writes the new history's snapshot to R2, and takes a new commit.

**upgrade** deploys the earlier release, writes a space with files and a share link from two devices (`upgrade-write.ts`), deploys this commit to the same Worker and bucket, and checks that the credentials, the space and the link still work (`upgrade-verify.ts`). `from_ref` is the release to upgrade from. Left empty, it is the newest tag before this commit. It must be an ancestor of this commit, at or after `8c9bf4a`, the first commit whose `server/` deploys on its own. Each Durable Object runs its missing schema steps when it first starts on new code ([Storage migrates when each object starts](design/deploy-and-upgrade.md#4-storage-migrates-when-each-object-starts)). When both commits have the same number of steps, none runs, and the run shows only that the data survives the new code. The `upgrade-verify.ts` log gives each class's step count before and after.

**e2e** runs a Linux device in the deploy job and a macOS device in a second job on a GitHub-hosted `macos-15` runner, which gets no secrets. Both build `koi` from this commit and run `koi _daemon` themselves (`e2e.ts`). The deploy job sends the macOS device its pairing code as a run artifact sealed to a key that only the macOS job holds. The scenarios run in order:

1. Create: each device writes files, among them a 3 MB and a 2 MB file.
2. Edit: each device rewrites one of its own files.
3. Rename and move: at the same time, Linux renames a file the macOS device wrote, and macOS moves a file into a folder Linux made.
4. Delete: the macOS device deletes a file, and `koi history trash` on Linux lists it.
5. Conflict: both devices edit `shared.txt` while Linux is offline. Both end with two `shared…txt` files, one with each edit.
6. Third device: a second `koi` profile on Linux pairs and downloads the whole tree.
7. Rewind: each device writes a file after a restore point, and the owner rewinds the space to that point. Each folder recovers and asks one decision that holds both files. Linux answers `discard` with `koi resolve` and must then hold the rewound tree. Point-in-time recovery restores only approximately the requested point ([RFC 0003 §2](../spec/rfc/0003-space-rewind.md#2-admissible-restore-points)), so the run takes that tree from the server's log (`rewind-check.ts`), not from what the devices saw. Then macOS answers `keep`, which puts both files back on the server, and both devices must end with them.

A job moves on only when its folder holds exactly the expected tree. The last step fails unless the macOS and Linux trees match. Each job's summary gives every scenario's tree hash, the SHA-256 of a `sha256sum`-style listing of the tree. Approve an e2e run within about 45 minutes. The macOS job builds `koi` in the meantime and fails if it gets no pairing code by then.

**bench** runs the Rust release test `upload_benchmark` (`core/net/tests/upload_bench.rs`). `bench_ns` takes up to four values from `1,2,4,8`, and `bench_repetitions` takes 1 to 3. For each value, the test sets the upload concurrency to N and uploads 1 GiB of fresh 4 MiB chunks to a new space, so the default sweep uploads 12 GiB. The test times only `Transfer::upload`, with its hashing, planning, PUTs and retry waits. The time includes Worker and R2 work together. The log and summary give MB/s in decimal megabytes, the HTTP version, and the retries seen.

**bench-small** runs the Rust release test `small_file_benchmark` in the same file. `bench_small_files` takes 32 to 50,000, `bench_small_kib` takes 1 to 255, and `bench_small_runs` takes 1 to 5. Each run uploads that many fresh files of that size twice, each time to a new space and through its own HTTP connection pool: once with bundles, and once with one PUT per chunk. Odd runs start with bundles and even runs with puts. For each run and side the summary gives the upload time, files per second, the upload requests the client sent with retries included, and chunks per request, then each side's median time, time range and median request count. After the test, `bench.ts` asks the Cloudflare GraphQL Analytics API for the Worker's invocations and CPU time p50 and p99 in each side's time window, retrying for up to 10 minutes in all while the analytics catch up. On the bundles side, CPU per invocation is CPU per bundle. If the token lacks Account Analytics Read or the query fails, the summary says so and the run still passes with the upload numbers.

**first-sync** measures a first sync of a large tree on GitHub-hosted `macos-15` runners against a Worker deployed from `main` (`first-sync.ts`). One build job for each ref in `first_sync_refs` (at most four) builds `koi` in release mode from that ref, so the client under test can be an older tag or a branch that is not on `main`. The build jobs get no token and no secrets. When they finish, the deploy job waits for approval and deploys, and the sync job starts. The sync job gets no secrets. It frees disk space and generates one seeded tree of 1,900,000 entries shaped like a code workspace: about 300 projects, each with a `node_modules` of 150 to 450 packages, folders up to 8 levels deep, mostly files of 2 KiB or less and a few up to 64 KiB, about 2.4 GiB in all. The deploy job seals one pairing code for each ref, as in e2e, and keeps the Worker until the sync job ends. The approval request appears when the builds finish. Approve it within 30 minutes of the builds finishing. The sync job starts then and, once its tree is ready, waits 35 minutes for the pairing codes, which the deploy job seals only after the approval and the deploy.

Each dispatch runs every ref once, starting with the ref at index `first_sync_rotation`. Dispatch once with each index, so that every ref runs first once: with refs A, B and C, rotations 0, 1 and 2 give A B C, B C A and C A B. `first_sync_repetitions` runs more rounds in one dispatch, each starting one ref later. It must be 1 or a multiple of the number of refs, so that each ref runs as often in each position. Before each run the job removes `.koidrive` and the `com.koidrive.node` mark from the tree and checks that none is left. Then it creates a new space, links the tree, starts `koi _daemon`, and reads `koi status --json` and the daemon's CPU time and memory every 2 seconds. A run ends when the server holds every entry and nothing is left to upload, after `first_sync_minutes` (80 by default), or when the daemon exits, which fails the run and the job. The validation step adds up the runs, 5 minutes to reset the tree before each, and the time to free disk, generate the tree, wait for the approval and write the summary. It refuses inputs that do not fit the sync job's 6 hours. The sync job hands its start time to the deploy job with its public key. The deploy job keeps the Worker until the sync job ends, at most 345 minutes after the sync job started and 345 minutes after the deploy job started, because GitHub ends a job after 6 hours. The sync job starts no run that could not end before then.

For each run the job records the time to the first upload and to a full sync, the stalls (5 seconds or more in which the uploaded entries, bytes and commits did not grow while work remained), the daemon log's local timeouts ("did not move for", "no complete answer within", "kept failing"), the daemon's CPU seconds and peak memory, and the size of `store.sqlite` and its WAL per synced entry. The `first-sync-results` artifact holds each run's JSON, its samples and its daemon log. The summary gives each ref's median and the lowest and highest value over the runs that did not fail, and the reason for each failed run.

The Worker of a first-sync run traces 5% of requests (`KOIDRIVE_TRACE_SAMPLING`, which `run.ts` writes into the deploy copy's `wrangler.jsonc` only). After the sync job ends, the deploy job waits up to 3 minutes for Workers Logs to take in the last requests and reads the Worker's events since the deploy through the [telemetry query API](https://developers.cloudflare.com/api/resources/workers/subresources/observability/subresources/telemetry/methods/query/). It halves a time window whose query returns the 2,000-event limit. It stops reading 15 minutes before GitHub would end the job, so the teardown keeps its time, and it uploads the artifact after the teardown. When the sync job ran to the hold's cap, it reads few events or none. The `first-sync-telemetry` artifact holds `server-events.ndjson.gz` and `server-events.json`. The first file has one line per event: its `timestamp` in Unix milliseconds, its `$metadata` fields `id`, `type`, `service`, `origin`, `route` (the route name, such as `post_chunk_bundle`, in place of the request's path; a trigger without a path stays as `trigger`), `requestId`, `traceId`, `spanId`, `parentSpanId`, `spanName`, `startTime`, `endTime`, `duration`, `traceDuration` and `statusCode`, its `$workers` fields `eventType`, `entrypoint`, `executionModel`, `outcome`, `wallTimeMs`, `cpuTimeMs`, `requestId`, `traceId`, `spanId` and `durableObjectId`, and the span attributes `cloudflare.r2.operation`, `cloudflare.r2.response.success`, `cloudflare.r2.error.code` and `cloudflare.durable_object.id`. Other R2 attributes, such as object keys and sizes, are left out, as are log messages, headers and any value that holds a URL or path. A failed query's message names the account as `<account>`. The second file gives the time window, the queries, the event counts by type and span name, the names of the fields the API returned, any window that still held 2,000 events at 1 second, and `uncoveredFromMs`: when the step stopped at its time limit or a query failed, the Unix millisecond time from which events are missing, otherwise `null`. A failed query also fills `failure` and fails the step. Join the events to the daemon log's `measure` lines by time: both use UTC.

## Each run deploys, tests and deletes one server

Every run uses one name, `koidrive-test-<run id>-<run attempt>`, for its Worker and its R2 bucket.

1. **Trust check.** A job without secrets refuses the run unless it started from `main` of `koidrive/koidrive` for a commit on `main`, and the `deploy-test` environment is [protected](#set-up-the-protected-environment). Only then does the job with the secrets wait for approval.
2. **Preflight.** If a Worker or a bucket already has the run's name, the run stops before creating anything.
3. **Deploy.** The run copies the tracked files of `server/`, sets the Worker and bucket names in `wrangler.jsonc` as an owner does before a Wrangler deploy, creates the bucket, and runs `pnpm run deploy` with a random `--tag`. The tag marks the Worker as this run's even if the deploy fails after the upload. The run then waits until the new version takes all traffic and `/v1/server` answers.
4. **Claim.** The run requests a random path and queries Workers Logs until that request shows up, sending the request again on every query for up to 10 minutes, which proves the token can read this Worker's logs. Then it opens `/setup`, reads the claim code from the log and redeems it. The run never prints the code.
5. **Test**, in the chosen mode.
6. **Teardown**, whatever happened before. It deletes the Worker, and with it the Worker's Durable Objects, only if a version carries the run's tag. It deletes the bucket only if its creation date is the one the run recorded. It fails the job if anything it created is left, or if something with the run's name was not its own.
7. **Usage summary**, in the log and on the summary page: the test client's requests and bytes, what R2 held at teardown, and Worker, Durable Object and R2 figures from the Analytics API. Those figures can lag by several minutes, and the Durable Object storage figure covers the whole test account.

## When a run fails

The failing step's log names the cause. The common ones:

- **The trust check refuses the run.** The run did not start from `main`, or the environment lost its protection. Check the environment against [Set up the protected environment](#set-up-the-protected-environment).
- **"The deploy test needs the environment secrets ..."** One of the [secrets](#secrets) is missing. The run deployed nothing.
- **"R2 is not activated for this account"** (code 10042). Activate R2 as [What you need](deploy.md#what-you-need) describes. The run deployed nothing.
- **"wrangler deploy printed no workers.dev address".** The account has no `workers.dev` subdomain. Open **Workers & Pages** in the dashboard once to create it.
- **"a Worker named ... already exists" or "an R2 bucket named ... already exists".** Something has the run's name. The run created nothing and left it alone.
- **"no query of this Worker's logs showed its request ..."** The token cannot read Workers Logs, or the logs lagged by more than 10 minutes. The run did not open `/setup`, so no claim code was spent. Rerun the failed jobs.
- **HTTP 403 with "the token lacks a permission this call needs".** Compare the token with [The token's permissions](#the-tokens-permissions).
- **Code 10007 after an upload.** The API lists a new Worker late. The run deploys again after 10 and 30 seconds and stops after the third upload.

### If teardown fails

The job names what is left. Delete the run's own leftovers by hand: the Worker `koidrive-test-<run id>-<attempt>` under **Workers & Pages**, which removes its Durable Objects, then empty and delete the R2 bucket of the same name. A cancelled run still tears down, because the step's condition is `always()` ([workflow cancellation](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-cancellation)). A force-cancelled run ([force cancel](https://docs.github.com/en/rest/actions/workflow-runs#force-cancel-a-workflow-run)) or a dead runner skips teardown, so check for `koidrive-test-` Workers and buckets afterwards.

## Set up the test account

The `deploy-test` environment and both secrets exist. Use this section to check them after a change, or to recreate them.

The account needs R2 activated, as for any owner ([What you need](deploy.md#what-you-need)), and a [`workers.dev` subdomain](https://developers.cloudflare.com/workers/configuration/routing/workers-dev/), which the account gets the first time someone opens **Workers & Pages**.

### Set up the protected environment

The token can deploy and delete every Worker and bucket in the test account, so it must reach only code merged to `main`, and only after you approve. The workflow checks these repository settings but cannot apply them, and its first job refuses the run unless they hold.

1. Open the repository's **Settings**, then **Environments**, then **New environment**.
2. Type `deploy-test` and click **Configure environment**.
3. Select **Required reviewers**, add `gunwooterry` and nobody else, and leave **Prevent self-review** unselected, because the same person starts and approves runs. Any one listed reviewer can approve a run ([required reviewers](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments#required-reviewers)), so the workflow refuses a list with anyone else on it. Click **Save protection rules**.
4. Deselect **Allow administrators to bypass configured protection rules** and click **Save protection rules**. The API may not report this setting, so the workflow refuses a run only when the API reports bypass as allowed. Check it by hand after every change to the environment.
5. Under "Deployment branches and tags", choose **Selected branches and tags**, add a **Branch** rule for `main`, and add no other rule.

Your approval of a run from `main` is the boundary. The workflow's own checks run code from the commit under test, so they are a second line of defence.

### Secrets

Keep both secrets under **Environment secrets** of `deploy-test`. To replace one, for example when you roll the token, run `gh secret set <NAME> --env deploy-test`, which reads the value from standard input. Never add them as repository or organization secrets. A workflow run for a pull request from a branch of this repository receives those, so anyone who can push a branch could read them.

| Secret | Holds |
|---|---|
| `CLOUDFLARE_API_TOKEN` | An account API token with the permissions below, limited to the test account |
| `CLOUDFLARE_ACCOUNT_ID` | The 32-character ID of the test account, shown under **Account ID** on its Workers & Pages overview |

### The token's permissions

Create a custom token (**Manage account**, **Account API tokens**, **Create Token**, **Custom token**). Under **Account Resources**, include only the test account. Give it these permissions and nothing else: no zone permissions, no administrator role and never the Global API Key.

| Permission | API name | Used to |
|---|---|---|
| Account, Workers Scripts, Edit | Workers Scripts Write | Deploy the Worker and its Durable Object classes, read its versions and deployments, list its Durable Object namespaces, and delete it |
| Account, Workers Observability, Edit | Workers Observability Write | [Query Workers Logs](https://developers.cloudflare.com/api/resources/workers/subresources/observability/subresources/telemetry/methods/query/) for the claim code |
| Account, Workers R2 Storage, Edit | Workers R2 Storage Write | Create the bucket and read its creation date, list, read and delete its objects, and delete it |
| Account, Account Analytics, Read | Account Analytics Read | Read the usage summary's figures and `bench-small`'s Worker CPU. Without it they show as unavailable |

Anyone with this token can deploy, read and delete every Worker and bucket in the account. Use an account that holds nothing else of value, give the token an expiry date, and roll it if a run log ever shows it.
