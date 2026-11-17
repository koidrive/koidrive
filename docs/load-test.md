# Load testing a deploy on a real account

The **Load test** workflow ([`load-test.yml`](../.github/workflows/load-test.yml)) measures how many commits per second one space takes before its latency rises, and whether more spaces add commits per second or share one limit. It deploys `server/` from `main` to the test account under a throwaway name, commits to it from up to 400 paired devices in steps of rising load, reads what the Durable Objects did from the Analytics API, and deletes everything again. It uses the [deploy test](deploy-test.md)'s protected `deploy-test` environment, secrets, trust checks, deploy, claim and teardown.

To run it, check that the account is on Workers Paid, start the workflow from `main`, and approve it on the run page:

```sh
gh workflow run load-test.yml --ref main -f scenario=s1 -f workers_paid=confirmed
```

A full `s1` run takes about two hours. Runs of this workflow wait for each other, but not for deploy test runs, which use other names. The scripts are `load.ts`, `load-model.ts` and `workers-plan.ts` in `server/scripts/deploy-test/`.

## Choose a scenario and its inputs

| Scenario | Steps | Each step | Answers |
|---|---|---|---|
| `s1` | Writers: 1, 5, 10, 25, 50, 100, 200 | One space | The commits per second of one space |
| `s4` | Spaces: 1, 4, 16 | 25 writers in each space | Whether commits per second grow with spaces, or stop at a limit the spaces share |

```sh
gh workflow run load-test.yml --ref main -f scenario=s4 -f workers_paid=confirmed
gh workflow run load-test.yml --ref main -f scenario=s1 -f steps=1,5 -f warmup_seconds=30 -f measure_seconds=60 -f workers_paid=confirmed
```

- `steps` replaces the scenario's list with rising numbers, for example `1,5` for a short trial. The last step may need at most 400 writers, because one runner holds every connection.
- `warmup_seconds` (default 120, at most 600) and `measure_seconds` (default 480, at most 1800) set each step's length. All steps together may take at most 180 minutes, so that setup, calibration and analytics also fit in the run step's 240 minutes. The defaults take 70 minutes for `s1` and 30 for `s4`.
- `max_rows_written` (default 40,000,000) sets the [rows-written budget](#the-rows-written-budget-is-an-estimate).
- `workers_paid=confirmed` says that you checked the account is on Workers Paid.

The run checks `steps`, the step lengths and the budget only in its scenario step, after it has deployed. A bad value fails that step, and teardown still runs.

## The rows-written budget is an estimate

Workers Paid bills rows written beyond its monthly allowance, for example $1.00 per million after 50 million, and Durable Object requests and duration too ([Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)). To limit what one run spends, the run turns `max_rows_written` into a number of commits.

Five writers commit for 2 minutes. The run then waits 3 minutes for the Analytics API to catch up. Once a minute, it reads the Space DO requests and the rows written in that time, until both cover the calibration's commits and stay unchanged for a minute. The budget is `max_rows_written` divided by 1.25 times the rows per commit. If no reading qualifies within 23 minutes, the run assumes 25 rows per commit. When the writers' commits, calibration included, reach the budget, the writers stop and the steps end.

The budget is not a hard ceiling on rows written. It does not count the setup's writes, and a step whose commits write more rows than the calibration's can pass `max_rows_written` by the difference. The results table shows each step's rows per commit. Rows written by other runs in the same month count against the same allowance, so lower `max_rows_written` when the account has already used part of it. Check the account's usage under **Workers & Pages** before and after a run.

## What a run does

1. **Trust check**, as in the deploy test.
2. **Plan check.** Workers Free allows 100,000 rows written a day, which one step uses up in minutes. `workers-plan.ts` goes on only if the account's subscriptions show an active paid Workers plan, or if you started the run with `workers_paid=confirmed`. The deploy-test token has no billing permission, so the read fails with HTTP 403 and only `workers_paid=confirmed` lets the run go on. When the check fails, the run deploys nothing.
3. **Deploy and claim**, as in the deploy test, for the commit the run started from.
4. **Setup.** The owner creates the spaces the last step needs. Each space gets two stored 1 KiB chunks and one small file per writer. The owner then pairs one device for every writer and every subscriber.
5. **Calibration**, which sets the budget.
6. **Steps.** Each step starts its writers, waits `warmup_seconds`, then measures for `measure_seconds`. Each writer is a paired device with its own WebSocket. It sends one commit at a time, a single `UpdateContent` of its own file that switches between the two stored chunks, so no commit touches R2 and no two writers touch the same node. Each space has one subscriber, so every commit pays for one push. During each step the owner times `GET /v1/spaces` every 2 seconds. That request and the Worker's token check go only to the Account DO, so the probe shows how long the Account DO keeps requests waiting.
7. **Analytics.** Three minutes after the last step, the run reads each step's window from the Durable Objects datasets of the GraphQL Analytics API ([metrics and analytics](https://developers.cloudflare.com/durable-objects/observability/metrics-and-analytics/)). It waits up to 20 minutes for a storage sample taken after the load.
8. **Results**: a table in the log and on the summary page, and `load-results.json` as a run artifact kept for 30 days. It holds every step's counts, the calibration, the storage samples and a timeline of commits.
9. **Teardown and usage summary**, as in the deploy test, whatever happened before.

## When a run stops early

The run stops after a step in which more than 10% of commits failed, the median commit took over 10 seconds, or the server accepted no commit. The log and `load-results.json` give the reason, and that step is the result you are looking for. A run also stops when the budget runs out.

Other causes:

- **"The load test needs Workers Paid ..."** The plan check failed. Check the account's plan, then start the run again with `workers_paid=confirmed`.
- **"LOAD_STEPS must be rising whole numbers ...", "... steps of ... take ... minutes", or "the last step needs ... writers".** An input is out of range. Fix it and start a new run.
- **"Analytics did not report SpaceDO requests and rows written ...".** The calibration fell back to 25 rows per commit. The run goes on, and the log lists every reading.
- **The results are marked partial.** A test of the suite failed, and the run wrote what it had.
- **Teardown failed.** See [If teardown fails](deploy-test.md#if-teardown-fails).

## Read the results table

| Column | Meaning |
|---|---|
| Commits/s | Accepted commits in the measuring window divided by its length |
| Commit ms | From sending a commit to its answer, at the runner, including the network to Cloudflare |
| Failed | Refused commits by error code, and commits with no answer within 60 seconds or before the socket closed |
| Closes | WebSocket closes by code that the run did not ask for, and failed reconnects |
| Push lag | From the commit's `committed_at_ms` to its arrival at the subscriber. Runner and server clocks can differ by some milliseconds |
| Account probe | Time of `GET /v1/spaces` |
| Generator CPU | CPU time of the run's Node.js process over the window. Near 100%, the runner limits the numbers, not the server |
| Account DO CPU | CPU time of the Account DO over the window, and its requests per second. The deployment has one Account DO, which runs on one thread, so near 100% means it is the limit |
| Busiest SpaceDO CPU | CPU time of the busiest Space DO over the window |
| Rows written per commit | Rows written by every Durable Object in the window divided by accepted commits |

Analytics figures are samples and can include a few seconds outside a window.
