# Testing

[CI](../.github/workflows/ci.yml) runs the test suites a change can reach on each pull request and in the merge queue, and every Linux suite on `main`. The [property battery](#the-property-battery-runs-nightly-and-on-demand) runs opted-in stress properties at high case counts every night and when you start it. The [heavy tests workflow](#heavy-tests-run-on-github-actions) runs one package's tests on Ubuntu and macOS as many times as you ask. To run the Rust checks that CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --features koidrive-cli/test-install-inventory,koidrive-cli/test-setup -- -D warnings
cargo test --workspace --locked --features koidrive-cli/test-install-inventory,koidrive-cli/test-setup
```

## CI runs these jobs

Each job runs on Ubuntu, and the jobs marked macOS run on macOS. The `changes` job picks the build and test jobs a change can reach. `scripts/ci-select` runs [determinator](https://docs.rs/determinator) on the Cargo graphs of the base and the change, which finds the packages whose files, dependencies or features changed and every package that builds or tests them. [`.github/ci-select.toml`](../.github/ci-select.toml) maps those packages and the files outside Rust packages to jobs. A `[[read]]` rule there names files that a package's tests read from outside that package, so a change to them also runs every job that tests the reading package. A file that no rule covers and no package holds runs every job, and so does a change to `ci.yml`, the selector, the toolchain or the root `Cargo.toml`. The merge queue selects against the queue's base, which holds every pull request ahead in the queue. The `changes` log names each job with the file or package that selects it. If the selector fails, every job runs.

| Job | What it checks | Run it locally |
|---|---|---|
| `rust lint` (macOS) | Workspace clippy with the `test-install-inventory` and `test-setup` features; on Ubuntu also fmt, the doctests, the property-persistence check and the tests of the CI tools | The fmt and clippy commands above, `cargo test --doc --workspace --exclude koidrive-sim --locked --features koidrive-cli/test-install-inventory,koidrive-cli/test-setup`, and the [property-persistence check](#property-tests) below |
| `rust build` (macOS) | Builds every selected test binary once into a [nextest archive](https://nexte.st/docs/ci-features/archiving/) | `cargo nextest archive --workspace --exclude koidrive-sim --locked --features koidrive-cli/test-install-inventory,koidrive-cli/test-setup --archive-file tests.tar.zst` |
| `rust tests` (macOS) | Runs the archive's tests in shards, [balanced by measured test time](#shards-follow-measured-test-times) | To rerun shard N, check out the run's commit, download its `rust-tests-<os>` artifact into `rust-tests/` at the checkout's root, then run `KOIDRIVE_PROPTEST_DETERMINISTIC=<mode> cargo nextest run --archive-file rust-tests/archive.tar.zst --workspace-remap . --config-file rust-tests/shards.toml --profile shard-N` there, with `true` for a merge queue run and `false` for a pull request or push run, as CI sets it. Without shards: `cargo nextest run --workspace --exclude koidrive-sim --locked --features koidrive-cli/test-install-inventory,koidrive-cli/test-setup` |
| `rust special` (macOS) | The tests that need mounted disks, saved-case replays without random cases, or a release version stamped into the CLI | The commands in the job's steps |
| `rust tests (…, sim)` (macOS) | `koidrive-sim` in the `checked` profile | `cargo test -p koidrive-sim --profile checked --features koidrive-engine/serde` |
| `rust test report` | Every listed test ran in exactly one shard; per-test timings and flaky tests | Download the run's `rust-test-inventory-<os>` artifact into `inventory/` and its `rust-junit-<os>-*` artifacts into `junit/`, then `python3 scripts/ci-rust-tests.py report --os <os> --list inventory/list.json --junit junit --out report` |
| `server` | Type checks, the Worker's Vitest suite, and the unit tests of the deploy-test scripts | In `server/`: `pnpm install --frozen-lockfile`, `pnpm typecheck`, `pnpm test`, then `pnpm exec vitest run --config scripts/deploy-test/vitest.config.ts run.unit e2e.unit load.unit bench.unit first-sync.unit` |
| `server deploy check` | A copy of `server/` deploys on its own (a dry run, then `wrangler dev` and a claim), and the server bundle that the CLI embeds packs cleanly | `node server/scripts/check-deploy.mjs` |
| `net integration`, `driver integration`, `cli integration`, `ffi integration` | The tests whose `#[ignore]` reason says they need the Worker, against the Worker under `wrangler dev` | After `pnpm install` in `server/`: `core/net/scripts/integration-test.sh`, `core/driver/scripts/integration-test.sh`, `cli/scripts/integration-test.sh` or `core/ffi/scripts/integration-test.sh` |
| `spec` | `buf lint`, generated code that matches `spec/proto`, and no breaking change against the base | In `spec/`: `buf lint`, then `buf generate` and check that `git status` shows no change |
| `comments` | The change adds no code comments outside the allowed kinds, and the tests of that check, the design-document splitter, the property battery workflows and the test-value harness pass | `python3 scripts/check-new-comments.py origin/main HEAD` |
| `macos app` (macOS) | The app builds, the Swift bindings' tests pass, and an unsigned disk image packages and verifies | In `apps/macos/`: `scripts/build-ffi.sh`, then `swift test --package-path KoidriveFFI` |
| `installer` (macOS) | `install.sh` parses and passes shellcheck, and the isolated installer journeys pass | `sh -n install.sh`, `shellcheck -s sh install.sh scripts/test-install.sh`, then `python3 scripts/test-install-journey.py -v` |
| `test lines` | Reports the net Rust test lines the change adds against its allowance | `python3 scripts/check-test-lines.py` |
| `test split mapping` | A test split moves every item unchanged and `mapping.json` lists it, and every changed `include!` fragment is rustfmt-formatted | `python3 scripts/split-tests.py verify --source-ref origin/main` and `python3 scripts/split-tests.py check-format --base-ref origin/main` |
| `Store upgrade` | Every store migration has upgrade cases, and they pass | `python3 scripts/check-store-upgrades.py origin/main HEAD` |
| `state file safety` (macOS) | The change adds no new calls to the `write`, `rename`, `copy`, `hard_link` or `persist` functions of `std::fs`, `tokio::fs` or `tempfile`, and no async lock borrows | `python3 scripts/check-state-io.py origin/main` |

On a pull request, CI adds macOS jobs only where a change needs them. The macOS Rust jobs and `macos app` run when the change touches a file that builds or behaves differently on macOS, as `scripts/macos-changes.sh` lists them. Every macOS job, `server (macos)` and `spec (macos)` included, runs when it touches `ci.yml`. The merge queue runs the macOS leg of every job it selects, and a push to `main` skips the macOS jobs only when CI finds a merge queue run that passed CI on the same commit. If that lookup fails or finds no such run, the push runs all four macOS groups. The macOS jobs run on GitHub-hosted runners.

Two tests in `cli/tests/server/service.rs` also need a systemd user instance. The `cli integration` job starts one, and locally they skip themselves unless `KOIDRIVE_TEST_SYSTEMD` is set.

Four other workflows run on pull requests. [CLI distribution](../.github/workflows/release-cli.yml) builds the CLI tarballs, tests the installer and installs the CLI from a local Homebrew tap. [DCO](../.github/workflows/dco.yml) checks the `Signed-off-by` line of every commit. [Dependency advisories](../.github/workflows/dependency-advisories.yml) checks the Rust dependencies against RustSec advisories. [Installer](../.github/workflows/installer.yml) runs only on a pull request that changes it, and checks that koidrive.com serves the `install.sh` on `main`.

### Shards follow measured test times

`rust build` lists the archive's tests and writes one nextest profile per shard with `scripts/ci-rust-tests.py plan`. The plan reads per-test times from [`.github/rust-test-times.json`](../.github/rust-test-times.json), splits large binaries by test, and gives each shard the same share of measured time. A binary missing from that file counts half a second per test. Each shard retries a failed test once to tell flaky tests from failing ones, and a test that passes only on the retry still fails the run. The `rust-test-report` artifact holds each system's flaky tests, a `test-times.tsv` and a fresh `rust-test-times.json`. Copy that file's entries into `.github/rust-test-times.json` when the shards drift apart.

### Some slow tests wait for the merge queue

Pull request runs leave out the tests listed in [`.github/rust-tests-merge-only.txt`](../.github/rust-tests-merge-only.txt). Merge queue runs, pushes to `main` and every other event run them, so each still runs once before code reaches `main`. Mutation testing found that none of them catches a mutant a faster test misses (see [Test value](#test-value)). `plan` leaves them out of the shards on `pull_request`, `report` counts them as left for the merge queue and fails if one ran, and the sim job passes them to `cargo test` as `--skip` arguments. Both commands fail when a listed test no longer exists, so a rename has to update the list. Add a test to the list only when it is slow and catches nothing a faster test misses; never list a work-bound or timeout test.

## Some tests run only on request

`cargo test` skips a test marked `#[ignore]`. `grep -rn '#\[ignore' core cli` lists them. They are of these kinds:

- The integration tests that need a Worker. They run through the scripts in the table above.
- Benchmarks, timing probes and models that take too long for every run, such as `upload_benchmark` in `core/net/tests/upload_bench.rs`, which runs in the [deploy test's bench mode](deploy-test.md#modes) or against a local Worker. For a local run, start `wrangler dev` in `server/` with a new `--persist-to` directory, because a claimed Worker issues no claim code. Then set `KOIDRIVE_TEST_SERVER` to its URL, `KOIDRIVE_TEST_SERVER_LOG` to the file that holds its output, where the test reads the claim code, and `KOIDRIVE_BENCH_MIB=64`. Run one with `cargo test -p <package> <name> -- --ignored`, adding `--release` where the test says so.
- Known failures. Their ignore reason names the issue, and the fix removes the `#[ignore]`.
- Helpers that another test starts as a process or calls, such as the other process in `core/ipc/src/bind.rs` and the fixtures in `core/test-support/tests/budgets.rs`.
- Tests that CI runs on its own runners, such as the signing test in `cli/src/app_install/macos/tests/verification.rs`, which creates a throwaway keychain.

## Property tests

The property tests use [proptest](https://docs.rs/proptest/1.12.0/proptest/). They are the `proptest!` blocks under `core/`, in integration tests and unit tests alike, and `grep -rln 'proptest!' core` lists them. Each block sets its own case count. Environment variables control the case budgets and seeds:

- `PROPTEST_CASES`: the number of cases each property runs.
- `KOIDRIVE_BATTERY_CASES`: the case budget for properties that opt in with `koidrive_test_support::battery_config`. Other properties use `regression_config` and keep their own budgets. Invalid battery budgets fail opted-in properties.
- `PROPTEST_RNG_SEED`: a fixed seed, a whole number. The same seed, case count and commit give the same cases. Without it, each run picks a random seed.

The properties run in one of two modes, set by `KOIDRIVE_PROPTEST_DETERMINISTIC` (`true` or `false`, unset means `false`):

- Exploring (`false`): each run picks a random seed, so every run tries new cases. Pull requests, pushes to `main`, the property battery and local runs use this mode.
- Deterministic (`true`): every property starts from seed 0, so the same commit runs the same cases on every run. The merge queue uses this mode, so a known bug on `main` that a random seed happens to reach does not remove an unrelated pull request from the queue.

Both modes replay the saved cases first. `PROPTEST_RNG_SEED` overrides seed 0, and `PROPTEST_CASES` overrides the case count in both modes. `ci.yml` sets the variable once for the whole workflow, to `true` only for `merge_group` events.

When a property fails, proptest prints the shrunk input. The shared configuration saves its `cc` line and a single-line `# shrinks to` description in a `<suite>.cases/cc-<seed>.txt` file, for example under `core/sim/tests/engine_sync.cases/`. Later runs replay saved cases first. Replay a failure by its `cc` line rather than by `PROPTEST_RNG_SEED`. Commit the case file with the fix so later runs keep replaying it.

Every property block uses `koidrive_test_support::battery_config`. The two driver properties for unreadable uploads and unwritable downloads use `regression_config` with eight generated cases because they wait on real retry deadlines. The checker records these exceptions by source path, test name and case count. A new bounded property requires an explicit entry. CI checks budget classes and regression persistence with:

```sh
cargo run --manifest-path scripts/check-proptest-persistence/Cargo.toml --locked --
```

The loader also reads legacy `.proptest-regressions` files. To move their lines into case files, run `python3 scripts/split-proptest-regressions.py <legacy-file>`.

### Several devices

`core/sim/tests/devices.rs` runs two or three devices against one reference server while effects fail, messages are lost, repeated or reordered, commits are refused, and devices crash between store transactions or in the middle of an effect (`core/sim/src/world`). Its properties come in pairs:

- `devices_lose_no_write...` fails when a write is lost, or when the engine, the store or the server refuses a state.
- `devices_converge...` also fails when a device does not reach the server's tree once the faults stop, or when a commit makes a change that the user of its device never made.

The plain pair runs without server losses. The `..._across_server_restores` pair adds servers restored from backups, and the `..._across_rewinds_and_rollbacks` pair adds rewinds, copied stores and rolled-back stores. Further properties cover folders rescanned one at a time, a folder another device replaces, and renames a device still owes, with and without servers restored from backups. `cargo test` and the battery skip the properties marked `#[ignore]`. To print the state after every step of one test:

```sh
KOIDRIVE_SIM_TRACE=1 cargo test -p koidrive-sim --test devices <test name> -- --nocapture
```

Add `--ignored` to run an ignored test.

## The property battery runs nightly and on demand

The [property battery](../.github/workflows/property-battery.yml) runs every property test on GitHub Actions. It runs every night on `main` with 48 shards and a budget of 4096 generated cases per stress property on each shard. Only the two deadline-bound driver properties keep their own budgets. CI rejects a property that does not use its recorded budget class. To start it for a branch, use `scripts/battery.sh`. It needs [gh](https://cli.github.com/) signed in to an account that can run workflows on the repository:

```sh
scripts/battery.sh                         # the current branch, as pushed
scripts/battery.sh --cases 16384 --shards 48 feat/my-branch
scripts/battery.sh --filter a_device --wait
```

The script prints the run URL and returns. With `--wait` it follows the run and exits with its result.

| Option | Workflow input | Default |
|---|---|---|
| `REF` | The branch or tag to test | The current branch |
| `--cases N` | `cases`, the `KOIDRIVE_BATTERY_CASES` budget for opted-in stress properties on each shard | 4096 |
| `--shards N` | `shards`, at most 256 | 48 |
| `--filter TEXT` | `filter`, a test name filter as for `cargo test` | None |
| `--mutations` | `mutations`, which also runs the [mutation set](#the-mutation-set) | Off |

The build job compiles every test binary that contains a `proptest!` block, including blocks pulled in with `mod` or `include!` and library unit tests. It fails if a `proptest!` block is in no binary, if a binary does not list a property it compiles, if a property's name is also the last part of another test's name in the same binary, if `ONCE_PACKAGES` names a package without property tests, or if the filter selects no test. Every shard runs each property test, which is each test that the property-persistence check's `--list` mode finds in a `proptest!` block, with its own seed, `PROPTEST_RNG_SEED` = run ID × 1000 + shard number, so every run covers new cases. The workflow's `ONCE_PACKAGES` lists the packages whose other tests do not depend on the seed, timers, sockets or the order they run in. The shards split those tests, and each runs once per battery. Every other test runs on every shard, so a race between two driver or net tests in one process can still show up. A shard fails if a binary runs a different number of tests than the shard gave it. A failing shard's repro command names the tests that failed. The summary job lists each shard's seed, result and log. It also lists each property that failed on a shard without saving a case. Such a property stopped before it generated any case there, usually on a committed case that fails, so its budget on that shard checked nothing. Battery triage fails for each one unless a case that another test of its binary saved on that shard reproduces the failure.

The workflow runs on the ref it tests, with that ref's own copy of the workflow file, so the ref must contain it. A branch's battery can then write only that branch's Actions cache, never `main`'s.

### When a shard fails

The shard's job summary prints one command per failing test target, for example:

```sh
git checkout <commit> && env -u PROPTEST_CASES KOIDRIVE_BATTERY_CASES=4096 PROPTEST_RNG_SEED=<seed> cargo test --locked --profile checked -p koidrive-sim --test engine_sync -- --exact a_device_with_local_changes_converges_with_the_server_and_keeps_every_write
```

The command reproduces the failure locally. The `failure-shard-<k>` artifact holds the seed, the commands, the test logs and any new `*.cases/cc-<seed>.txt` files, including their `# shrinks to` descriptions. Copy the case files into the same paths in the checkout, then run the failing property with `PROPTEST_CASES=0` to replay saved cases without generating random cases.

## Heavy tests run on GitHub Actions

The [heavy tests workflow](../.github/workflows/heavy-tests.yml) runs one package's tests on GitHub-hosted Ubuntu and macOS runners, once or many times. Use it for long property runs, simulation sweeps and checks for flaky tests, rather than a local machine. Start it from `main` with [gh](https://cli.github.com/) and name the branch, tag or commit to test in `ref`:

```sh
gh workflow run heavy-tests.yml --ref main -f ref=feat/my-branch -f crate=koidrive-sim -f test=engine_sync -f cases=4096
gh workflow run heavy-tests.yml --ref main -f ref=feat/my-branch -f crate=koidrive-driver -f filter=watcher -f repeat=20 -f os=macos
gh run list --workflow heavy-tests.yml --limit 1   # the run's ID, for gh run watch <id>
```

| Input | What it sets | Default |
|---|---|---|
| `ref` | The branch, tag or commit to test | The ref the run starts from |
| `crate` | The package, as for `cargo test -p` | Required, except with `mode` `macos-ui`, which takes none |
| `test` | One integration test target, as for `cargo test --test` | Every target of the package |
| `filter` | A test name filter, as for `cargo test` | Every test |
| `cases` | `KOIDRIVE_BATTERY_CASES`, for stress properties only | Each property's own count |
| `mode` | `cargo` for Cargo tests, `worker` for the integration launchers against a fresh local Worker (crate `koidrive-net` or `koidrive-driver`, which run their `server` and `sync` targets), `server` for the Vitest suite of `server/` (crate `koidrive-server`, where `test` filters test files by a name without `/` or `.`, such as `storage-full`, and `filter` filters test names), or `macos-ui` for the macOS app's UI tests on a fresh runner per run, which takes only `ref`, `repeat` and `os` `macos` or `both` | `cargo` |
| `repeat` | How many times each OS runs the selection, 1 to 100 | 1 |
| `parallel` | How many repetitions run at once on the same host, 1 to 8. Only 1 with `mode` `server` or `macos-ui` | 1 |
| `seed` | `PROPTEST_RNG_SEED` of the first run; run k uses seed + k - 1 | Run ID × 1000 |
| `features` | Cargo features, comma-separated, such as `koidrive-engine/serde` | None |
| `ignored` | Also runs the tests marked `#[ignore]` | Off |
| `release` | Builds and runs the tests with `--release`, which turns off debug assertions and overflow checks. Only with `mode` `cargo` and `parallel` 1 | Off |
| `env` | Extra environment for the test run, as space-separated `NAME=value` pairs, such as `MODEL_TABLE_SLOTS=1500000000`. Takes `MODEL_*`, `GUARD_MODEL_*`, `KOIDRIVE_*`, `KOI_*`, `RUST_BACKTRACE`, `RUST_LOG`, `RUST_TEST_THREADS`, `RUST_MIN_STACK` and `PROPTEST_VERBOSE`, except the case budget variables. `PROPTEST_VERBOSE` takes a level from 0 to 9, and a level above 0 also passes `--nocapture`, so the log shows proptest's messages; at `2` it prints `Test case passed` for each generated case. Only with `mode` `cargo` and `parallel` 1 | None |
| `os` | `both`, `ubuntu` or `macos` | `both` |

The workflow clears inherited case overrides. The `cases` input sets only the stress budget, so the two deadline-bound driver properties retain eight generated cases even when `cases` is set to zero. Use `PROPTEST_CASES=0` in a manual reproduction to replay saved cases without generating any cases.

The workflow tests one commit. It resolves `ref` once, so both operating systems test the same code even if the branch moves. Each OS job builds the tests once and then runs them `repeat` times. Its summary has one row per run with the seed, the result and the passed, failed and ignored counts, then the failing tests with the number of runs each failed in, and a command that reproduces a run from its seed. The log of the job's `Run the tests` step ends with the same summary, so `gh run view <run-id> --log` shows it too. A run whose test binary crashes, or that runs no test, counts as not finished cleanly. The job fails unless every run passed, and stops after 340 minutes. Each run starts from the case files in the commit. The job moves the case files a run saves into the `heavy-tests-<OS>` artifact under `regressions/run-<k>/`, so a later run never replays them, and a run's seed reproduces that run on a clean checkout. The artifact also holds the summary and each run's log. To replay a saved case, copy its file to the same path and run the test with `PROPTEST_CASES=0`.

To compare a fix with the code before it, start two runs with the same `seed` and `repeat`: one with `ref` set to the commit before the fix and one with the fix. While the properties' strategies are the same, run k of each tests the same cases.

## The mutation set

`scripts/engine-mutations.sh` lists mutations of the engine's commit and effects code that the tests must catch, each a perl substitution in one file. `scripts/mutations.sh` checks every selector, then runs an unmodified baseline before applying mutations one at a time. Each mutation runs the commit tests (`cargo test -p koidrive-engine --test commit`), the interleaving property (`cargo test -p koidrive-sim --test engine_sync`), both, or the tests of several devices (`cargo test -p koidrive-sim --test devices`). Each selected suite runs once, with the same fixed seed and checked-in case files as the baseline. The default property case count is 4096. The script restores the source and fixtures after each mutation and prints failure logs and generated cc cases before cleanup. A mutation fails the run when:

- no test catches it (`SURVIVED`),
- its pattern no longer matches the source (`STALE`), or
- the mutated code does not compile (`BROKEN`).

When the code a mutation targets changes, update the mutation. When the code is removed, remove the mutation too.

The battery runs the set with `--mutations`, spread over up to as many jobs as there are shards. The script edits files and restores them with git, so it refuses a checkout with uncommitted changes. To run it locally:

```sh
scripts/mutations.sh               # every mutation
scripts/mutations.sh --shard 0/4   # every fourth mutation, starting with the first
```

`MUTATION_CASES` sets the property case count. `MUTATION_SEED` sets one unsigned 64-bit seed for the baseline and every mutation, defaulting to `0`. The hosted mutation jobs use seed `0` too. Shard numbers do not change it.

`scripts/trees-generator-proof.sh` compares how often the trees property catches a missed mark with rules below the root and with rules only at the root. It hides the saved cases and plants a mutation that skips the walk below a changed node that is not itself left out. It then runs `settling_what_changed_ends_where_settling_everything_ends` `PROOF_RUNS` times (default 5) at `PROOF_CASES` cases (default 2000): unmutated, with the mutation, and with the mutation and rules only at the root. It prints how many runs failed in each, restores the files, and fails unless the unmutated runs pass and the mutation fails at least one run with nested rules. The root-only count is printed for comparison and does not decide the result.

## Test value

The `Test value` workflow (`.github/workflows/test-value.yml`, run by hand) measures what each test catches against what it costs. `scripts/test-value.py` defines three samples: `driver` (the driver lib tests against mutants of `core/driver/src`), `guard` (the engine tests against mutants of `core/engine/src/guard.rs`) and `commit` (the engine tests and the sim `engine_sync` and `devices` tests against mutants of `core/engine/src/commit.rs`, plus the hand-written set from `scripts/engine-mutations.sh`). cargo-mutants generates the mutants. The plan job runs the sample's tests three times under nextest without mutations, to time each test and set aside any test that does not pass every time. Each shard then applies its mutants one at a time and records which tests fail. Tests that took 10 seconds or more in the baseline (the `slow` input) run only on the mutants the faster tests miss. A slow test's kills therefore count only those mutants, and a fast test's unique kills are unique among the tests that ran on that mutant: a slow test might also have caught it. Each test may run for 3 times its baseline time or 60 seconds, whichever is longer, before nextest stops it; a test stopped this way counts as a kill. The `ref` input must name a commit that contains `scripts/test-value.py`. A hosted runner can stop in the middle of a shard; to redo only those shards, run the workflow again with the same `ref` (as a commit sha), `sample` and `shards`, and list the shard indexes in `only`, such as `10,17`. Then pool the new run's results files with the first run's in one directory before `report`. The report job writes three CSV files: one row per test, one row per mutant, and the surviving mutants. Each test row has the test's baseline time, its kills, its unique kills (mutants no other test catches) and a verdict. Verdicts cover only the mutated code named in the summary and in each row. What a test catches outside that code is not measured.

The verdicts are chosen so that all of them can be applied together, and each names the mutated code it was judged in. Fast tests are considered from the slowest down. A test is `redundant within <code>` only when every mutant it kills there is still killed by a test that stays. Slow tests are then considered from the slowest down. One is `slow, catches nothing else within <code>` when moving it leaves no killed mutant without a killer on the merge path. With `--few N`, slow tests move while every moved slow test, with all the others moved too, alone catches at most N mutants, and each is reported as `slow, alone catches N within <code>`; `merge_path_loses` gives that count, and the summary gives the total. `covered_by` names the tests that stay and catch everything else the test catches, chosen greedily after every verdict is set. A verdict says nothing about what a test guards outside the mutated code. A work-bound or memory test, for example, cannot fail on any mutant there. Before deleting or moving a test, check that its own subject is the mutated code. Property tests and tests that failed in the baseline are never candidates. When a mutant has no result (a failed shard, a nextest error, or a run cut off at its time limit), every verdict is marked provisional. To pool several samples, download their runs and pass each run's directory to `scripts/test-value.py report --runs`.
