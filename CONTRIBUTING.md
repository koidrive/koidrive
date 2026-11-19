# Contributing to Koidrive

Anyone can open an issue. The sections below say how to report a problem, how to propose a change, and what every commit needs.

## Report a bug or ask for a change in an issue

Open an issue for a bug report, a reproduction, feedback on a deployment, or a feature request. Search the existing issues first, and add to a matching one instead of opening another. Remove credentials, such as API tokens, and personal data from logs, screenshots and files before you post them.

Do not report security vulnerabilities in issues. [SECURITY.md](SECURITY.md) explains how to report them privately.

## Only maintainers open pull requests until 1.0

Until 1.0, only the project owner and invited maintainers open pull requests. Each merged change needs review now and maintenance for as long as its code stays in Koidrive, and the maintainers can take on only so much of that work.

To propose a change, open an issue that describes the problem and the change you have in mind. Changes to the protocol in [spec/](spec/) affect both the server and the clients, so they need an issue too. An accepted issue is not an invitation to open a pull request.

Forks and experiments are welcome under the licenses below. You can link a fork or an experiment in an issue, but a link does not mean a maintainer will review or merge it.

## Store migrations

Each change to the local store's schema is one SQL file in `core/store/migrations/`, named after what it adds, such as `kept_originals.sql`. A store applies the files it lacks in name order and records each name it applied, so migrations from different pull requests merge in any order. When a migration reads or changes a table or column that another named migration creates, its first line names that migration: `-- requires: other_name`. A merged migration file never changes. A later migration corrects it. `cargo test -p koidrive-store` checks that the named migrations give the same store in every order they can run in.

Each new migration has a matching `core/store/tests/upgrade_cases/<migration>.rs`. The store's build script includes every file there in `core/store/tests/upgrade.rs`. The file's `upgrade_case!(migration, |store| { ... });` body asserts the migration's behavior through the upgraded store. The harness copies a populated release fixture, opens it with the current store, checks preserved state, and reopens it after the case. The fixture and the whole of `upgrade.rs` are pinned. If you change the store source or the migration planning without adding a migration, add a `schema_` case. `python3 scripts/check-store-upgrades.py <base> HEAD` checks the changed files and runs every case by exact name. An unrelated test edit does not satisfy the check. The job gates merges through `ci-report`.

Other tests of a migration, such as a store from before it opening with what it adds, go in `core/store/tests/migrations/<migration>.rs`. The build script makes each such file a module of `core/store/tests/store.rs`, so it starts with `use super::*;` and uses the helpers there, such as `store_before`. Tests that cover every table read the tables from the store, so a migration adds files and edits no shared list.

## Property-test regressions

Property tests use `koidrive_test_support::regression_config` to save each failure as a separate `cc-<seed>.txt` file in a `.cases` directory beside the legacy regression file. Automatic saves include a `# shrinks to` description. Commit the generated case file with the fix. Independent cases do not edit a shared list.

The properties also read the legacy files, so cases appended by an older branch still replay. To move those lines, run `python3 scripts/split-proptest-regressions.py <legacy-file>`. The command preserves each `cc` line and leaves the legacy file readable. CI replays the saved cases with `PROPTEST_CASES=0` and prints their seeds and property names.

CI rejects property blocks that use default persistence. `PROPTEST_CASES` overrides each property's default case count.

## AI assistance

You may use AI tools for reports and for invited changes. If you submit one, you are responsible for it:

- Check every claim it makes.
- Understand it well enough to answer questions about it.
- Say which tools you used and how much of the work they did.
- Give the commands that reproduce the problem or test the change.

Do not submit the output of an agent that ran without your supervision. Maintainers follow the same rules.

## Commit titles

Commit titles follow [Conventional Commits](https://www.conventionalcommits.org/), for example `feat(server): add commit validation`.

## Developer Certificate of Origin

Every commit must be signed off under the [Developer Certificate of Origin 1.1](https://developercertificate.org/). By adding a `Signed-off-by` line you certify the following:

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

Sign off with `git commit -s`, which adds a line like this, using your `user.name` and `user.email`:

```
Signed-off-by: Jane Doe <jane@example.com>
```

The name and email must match the commit author. The DCO check on every pull request fails if any commit lacks a matching sign-off. To sign off commits you already made on your branch, run `git rebase --signoff main` and force-push the branch.

## Licenses

Contributions are licensed under the license of the directory they land in: AGPL-3.0-or-later in `server/`, Apache-2.0 everywhere else. New source files start with an `SPDX-License-Identifier` comment for that license.
