# State files

`koidrive_state_file` provides shared staging and publication for state files outside SQLite. `AtomicFile::prepare` creates a private stage in the destination directory. `as_file` lets credential writers set backup exclusions before writing a secret. `finish(Publish::Replace)` replaces a file, while `Publish::Create` refuses to replace an existing file.

The helper syncs the stage, publishes it, and syncs the directory. A failure after publication can leave the complete destination in place. Callers retain their domain locks and decide whether retrying the operation is safe.

Each stage holds an inode lock. Recovery holds the directory lock and removes only abandoned stages whose inode locks it can acquire without waiting. A writer does not hold the directory lock while preparing bytes or waiting for a credential response. Descriptor-relative operations keep publication and cleanup attached to the opened directory when an ancestor moves.

Dropping a stage removes it. A killed process cannot run cleanup, so the next `prepare` or explicit `recover` removes its abandoned stages. Code that lists or discloses a state directory calls `recover` first. The telemetry store also recognizes staging names from earlier writers when it removes reports from dead runs.

## Checks

| Rule | Enforcement |
| --- | --- |
| State-file publication has one owner. | `AtomicFile` owns its stage, inode lock, destination, and directory descriptor. |
| New `std::fs` and `tokio::fs` writes and renames are rejected outside that owner. | `scripts/check-state-io.py` uses Clippy's resolved `disallowed_methods` diagnostics and rejects new calls. Its error names the helper to use. |
| A mutex guard or a `RefCell` borrow cannot span an await. | Clippy's `await_holding_lock` and `await_holding_refcell_ref` run with errors enabled. |

Run `python3 scripts/test-state-io.py -v` to check the lint regressions, and `python3 scripts/check-state-io.py <base>` to check a committed change. `--inventory <file>` records every compiler-resolved direct-I/O diagnostic. The check compiles production libraries and binaries on Linux and macOS. Test fixture writes are not product persistence.

The direct-I/O check allows unchanged calls. Changing a call, its local imports, or its compilation conditions triggers a recheck. Build-script outputs are not runtime state. The check does not identify arbitrary writes through `File`, external processes, or direct syscalls, and it does not prove that a synchronous function avoids network I/O. Those boundaries need an API that withholds the corresponding capability.
