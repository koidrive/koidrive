# State files

`koidrive_state_file` provides shared staging and publication for state files outside SQLite. `AtomicFile::prepare` creates a private stage in the destination directory. `as_file` lets credential writers set backup exclusions before writing a secret. `finish(Publish::Replace)` replaces a file, while `Publish::Create` refuses to replace an existing file.

The helper syncs the stage, publishes it, and syncs the directory. A failure after publication can leave the complete destination in place. Callers retain their domain locks and decide whether retrying the operation is safe.

Each stage holds an inode lock. Recovery holds the directory lock and removes only abandoned stages whose inode locks it can acquire without waiting. A writer does not hold the directory lock while preparing bytes or waiting for a credential response. Descriptor-relative operations keep publication and cleanup attached to the opened directory when an ancestor moves.

Dropping a stage removes it. A killed process cannot run cleanup, so the next `prepare` or explicit `recover` removes its abandoned stages. Code that lists or discloses a state directory calls `recover` first. The telemetry store also recognizes staging names from earlier writers when it removes reports from dead runs.

Recovery opens read-only or write-only stages. Permission errors when opening or removing a stage leave that stage untouched without blocking other writes. A destination cannot use the helper's reserved staging namespace. `finish_with_legacy` and `write_with_legacy` remove explicitly named obsolete siblings after publication, under the directory lock. They reject names that escape the directory, name the destination, or overlap the helper's stages. The caller must hold the ownership lock shared with historical writers.

Pending-claim mutations borrow the Driver's profile lease. Their legacy selector recognizes only canonical positive PID names in `.pending-claim.json.<pid>.tmp`. `finish_with_legacy_matching` selects siblings through the prepared directory descriptor, so an ancestor move cannot redirect cleanup. `remove_with_legacy_matching` selects and removes state through one directory descriptor, then syncs it without creating a stage. Claim status reads do not recover or rewrite files.

## Checks

| Rule | Enforcement |
| --- | --- |
| State-file publication has one owner. | `AtomicFile` owns its stage, inode lock, destination, and directory descriptor. |
| New direct writes, renames, copies, hard links and tempfile publications are rejected outside that owner. | `scripts/check-state-io.py` uses Clippy's resolved `disallowed_methods` diagnostics and rejects new calls. Its error names the helper to use. |
| A synchronous mutex guard or a `RefCell` borrow cannot span an await. | Forced Clippy diagnostics for `await_holding_lock` and `await_holding_refcell_ref` fail the guard at any level. |

Run `python3 scripts/test-state-io.py -v` to check the lint regressions, and `python3 scripts/check-state-io.py <base>` to check a committed change. `--inventory <file>` records every compiler-resolved direct-I/O diagnostic and the named lifecycle and historical setup exceptions. The check compiles production libraries and binaries on Linux and macOS with the CI features. Test fixture writes are not product persistence. Its jobs are dependencies of the required `ci-report` job.

The direct-I/O check allows unchanged calls only when the baseline compiler inventory contains the same diagnostic. It compiles an isolated baseline with the head's toolchain, current lint policy and matching production flags. A failed baseline build fails the check. Diagnostic identities include the target, resolved method, mapped source locations and macro invocation chain, so a parent-module change cannot grandfather a writer that was absent from the baseline production build. Changing a call, its local imports, or its compilation conditions also triggers a recheck.

Build-script outputs are identified by Cargo's target kind, not by the source filename. The lifecycle checkpoint exception pins its function and method. Historical setup stages remain because setup has no cross-process lease that distinguishes an old process's stage from an orphan. The check does not identify arbitrary writes through `File`, external processes, or direct syscalls, and it does not prove that a synchronous function avoids network I/O. Those boundaries need an API that withholds the corresponding capability.

The compiler forces all three guard lints to warning level, including through macro-generated allow and expect attributes. The checker treats those diagnostics as failures and checks the macro expansion chain for changed invocation sites. Source-attribute checks provide early diagnostics but do not determine whether the compiler reports a violation.
