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

`split` without `--source-ref` reads the working tree, including files already split. Repeating either form with the same inputs produces identical files. Edit `mapping.json` to change an item's scenario, then run `python3 scripts/split-tests.py map`, which assigns each item the mapping lacks to the file that holds it, drops entries for items that no longer exist, and drops every source already split at `git merge-base origin/main HEAD`.

`mapping.json` holds only splits that have not landed. CI runs `verify --source-ref <base>` on every pull request and checks only the sources that are still one file at the base, so it proves the move of the split the pull request makes and ignores everything else. A change that turns a file into `include!` fragments without a `mapping.json` entry for it fails there too. A test added to a file that is already split, or to a file with no planned split, never needs `mapping.json`. `cargo fmt` does not follow `include!`, so CI also runs `split-tests.py check-format`, which runs rustfmt on the lines a change writes in any `include!` fragment.

Capture the test inventory before and after the split with the same command and environment:

```sh
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > before.txt
python3 scripts/split-tests.py split
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > after.txt
python3 scripts/split-tests.py compare-lists before.txt after.txt
```

## Port an open branch

Keep the old branch as a snapshot. Start a new local branch on the split main. `port` reads the split's assignments from `mapping.json`, which drops them once the split lands, so restore the file from the split's merge commit first with `git show <split-commit>:scripts/test-split/mapping.json > scripts/test-split/mapping.json`. Apply the old branch's non-test diff normally, excluding the sources named there. Then port their item-level changes:

```sh
python3 scripts/split-tests.py port --base-ref <old-merge-base> --branch-ref <old-head> \
  --assign 'core/engine/tests/commit.rs::fn:new_test=commit_moves_and_conflicts'
```

Supply one `--assign PATH::ITEM=SCENARIO` for each added test or helper. Helpers belong in `common`. The error lists unassigned items. `port` applies a three-way merge to each changed item, preserves main's unrelated items, and refuses overlapping edits before writing files. It also refuses a change to which functions a `proptest!` block holds, including an added or removed block; apply that change by hand. It does not switch branches, commit, or push. Run the crate tests after porting.

## Replay an open branch across a module split

`replay` carries an open branch's edits to a file onto the files it was split into, for a split into child modules (`mod account;` beside an `impl` block spread over several files) as well as into `include!` fragments. It needs no `mapping.json` entry, so it works after the split lands. Start a new local branch on the split main, apply the old branch's diff to every other file, then replay the split file. Also leave out of the `git apply` any `include!` fragment the file already had before the split, because `replay` carries those edits too:

```sh
git diff <old-merge-base> <old-head> -- . ':!core/driver/src/driver.rs' | git apply -3
python3 scripts/split-tests.py replay --base-ref <old-merge-base> --branch-ref <old-head> core/driver/src/driver.rs
```

It finds every item of the old file in the new tree by kind, name and `cfg`, with `impl` members and the contents of inline modules keyed by their container. A module the old file declared inline (`mod tests { … }`) keeps its name when it moves to a file; a module the split added is transparent. Each item the branch changed gets a three-way merge where it now lives, at its new indentation and with the visibility the split gave it. Lines inside a multi-line string literal keep their indentation. An added item goes right after the item that precedes it in the branch, or before the one that follows it. A private item that lands in a module the split added gets `pub(super)`, as the split's own items there needed, and the output names it so you can drop the marker where nothing outside that module uses it. An added `use` is skipped only when the module it lands in already holds it. A removed item is deleted from the file that holds it. The command prints where each item went and refuses, before it writes anything, an edit to text outside any item, an item key that repeats (such as two `const _` or two calls of one macro), a changed import the split rewrote, a change to or removal of an item the split tree lacks, and overlapping edits; apply those by hand. It does not switch branches, commit, or push. Build and run the crate's tests afterwards: an import the branch adds lands in the root module, and a child that names it without `use super::*` needs it too.
