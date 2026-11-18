# Split test scenarios

`../split-tests.py` moves Rust items according to `mapping.json`. Each source file has an explicit assignment for every test, helper, import, constant, type, and implementation. Unmapped items stop the command before it writes files.

Scenario files use `include!` in the original module. This preserves the fully qualified test names and private helper access. `common.rs` holds shared helpers once. Module items stay in the original file to preserve relative paths.

The parser uses `syn` spans to copy source bytes, not pretty-printed syntax. `verify` compares every original item with its destination, including attributes, signatures, and bodies. It reads include statements from the Rust syntax tree, rejects attributed includes, and ignores include text in comments or strings. Scenario fragments cannot contain includes or inner attributes, and their Rust file set must match the mapping exactly. It checks the module header and rejects missing or duplicated items. `compare-lists` compares the full Cargo test-name multiset, not only a count.

## Regenerate and verify

Set a separate `CARGO_TARGET_DIR` for this checkout. The script runs its parser build at nice level 10.

```sh
export CARGO_TARGET_DIR="$(mktemp -d)"
python3 scripts/test-split/test_split.py
python3 scripts/split-tests.py split --source-ref <unsplit-revision>
python3 scripts/split-tests.py verify --source-ref <unsplit-revision>
```

`split` without `--source-ref` reads the working tree, including files already split. Repeating either form with the same inputs produces identical files. Edit `mapping.json` to change an item's scenario. Ordinary test changes can edit their scenario files directly; then run `python3 scripts/split-tests.py map`, which assigns each item the mapping lacks to the file that holds it and drops entries for items that no longer exist. `map` refuses a proptest block whose functions sat in another file at `git merge-base origin/main HEAD`. CI runs `verify --source-ref HEAD --base-ref <base>` on every pull request, so a test added without `map` fails there, and so does a proptest function that changed file since the base. `cargo fmt` does not follow `include!`, so CI also runs `split-tests.py check-format`, which runs rustfmt on the fragments a change touches.

Capture the test inventory before and after the split with the same command and environment:

```sh
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > before.txt
python3 scripts/split-tests.py split
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > after.txt
python3 scripts/split-tests.py compare-lists before.txt after.txt
```

## Port an open branch

Keep the old branch as a snapshot. Start a new local branch on the split main. Apply the old branch's non-test diff normally, excluding the four sources named in `mapping.json`. Then port their item-level changes:

```sh
python3 scripts/split-tests.py port --base-ref <old-merge-base> --branch-ref <old-head> \
  --assign 'core/engine/tests/commit.rs::fn:new_test=commit_moves_and_conflicts'
```

Supply one `--assign PATH::ITEM=SCENARIO` for each added test or helper. Helpers belong in `common`. The error lists unassigned items. `port` applies a three-way merge to each changed item, preserves main's unrelated items, and refuses overlapping edits before writing files. It also refuses a change to which functions a `proptest!` block holds, including an added or removed block; apply that change by hand. It does not switch branches, commit, or push. Run the crate tests after porting.
