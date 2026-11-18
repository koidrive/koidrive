#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

runs=${PROOF_RUNS:-5}
export KOIDRIVE_BATTERY_CASES=${PROOF_CASES:-2000}
L=core/engine/src/classify.rs
T=core/engine/tests/trees.rs
cases=core/engine/tests/trees.cases
test=(nice -n 10 cargo test -q --locked -p koidrive-engine --test trees -- --exact settling_what_changed_ends_where_settling_everything_ends)
mutant='s/        self\.mark_left_out\(&changed\);\n/        let changed: Vec<NodeId> = (changed.into_iter())\n            .filter(|id| self.unobserved.contains(id) || self.left_out(id, &self.ignore, &mut HashMap::new()))\n            .collect();\n        self.mark_left_out(&changed);\n/'
root_only='s/0usize\.\.5, 0usize\.\.3\),/0usize..5, 0usize..1),/'

scratch=$(mktemp -d "${TMPDIR:-/tmp}/trees-generator-proof.XXXXXX")
cp "$L" "$scratch/classify.rs"
cp "$T" "$scratch/trees.rs"
mv "$cases" "$scratch/cases"
mkdir "$cases"
restore() {
  cp "$scratch/classify.rs" "$L"
  cp "$scratch/trees.rs" "$T"
  touch "$L" "$T"
  rm -rf "$cases"
  mv "$scratch/cases" "$cases"
  rm -rf "$scratch"
}
trap restore EXIT

apply() {
  perl -0pi -e "$2" "$1"
  if cmp -s "$1" "$scratch/$(basename "$1")"; then
    echo "STALE: the pattern no longer matches $1" >&2
    exit 2
  fi
}

failed=0
measure() {
  failed=0
  for _ in $(seq "$runs"); do
    rm -f "$cases"/*.txt
    if ! "${test[@]}" > "$scratch/log" 2>&1; then
      if ! grep -q 'test result: FAILED' "$scratch/log"; then
        cat "$scratch/log" >&2
        exit 2
      fi
      failed=$((failed + 1))
    fi
  done
  echo "$1: failed $failed of $runs runs at $KOIDRIVE_BATTERY_CASES cases"
}

measure "fix, nested rules"
fix=$failed
apply "$L" "$mutant"
measure "mutant, nested rules"
nested=$failed
apply "$T" "$root_only"
measure "mutant, root-only rules"
[ "$fix" -eq 0 ] && [ "$nested" -gt 0 ]
