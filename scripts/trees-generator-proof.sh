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

lock=$(git rev-parse --git-path trees-generator-proof.lock)
if ! mkdir "$lock" 2>/dev/null; then
  echo "another proof run in this checkout holds $lock" >&2
  exit 2
fi
scratch=
written() {
  if [ -f "$scratch/$1.applied" ]; then
    echo "$scratch/$1.applied"
  else
    echo "$scratch/$1"
  fi
}
restore() {
  set +e
  local failed=0 edited=0
  if [ -n "$scratch" ]; then
    for kept in "$L:classify.rs" "$T:trees.rs"; do
      local file=${kept%%:*} name=${kept#*:}
      [ -f "$scratch/$name" ] || continue
      if ! cmp -s "$file" "$(written "$name")"; then
        echo "kept the edit saved to $file during the run; its original is $scratch/$name" >&2
        edited=1
      elif ! { cp "$scratch/$name" "$file" && touch "$file"; }; then
        echo "could not restore $file from $scratch/$name" >&2
        failed=1
      fi
    done
    if [ -d "$scratch/cases" ] && ! { rm -rf "$cases" && mv "$scratch/cases" "$cases"; }; then
      echo "could not restore $cases from $scratch/cases" >&2
      failed=1
    fi
    if [ "$failed" -eq 0 ] && [ "$edited" -eq 0 ]; then
      rm -rf "$scratch"
    fi
  fi
  if [ "$failed" -ne 0 ]; then
    echo "copy each file above back from $scratch by hand, then remove $scratch and $lock" >&2
    exit 2
  fi
  rmdir "$lock"
}
trap restore EXIT
scratch=$(mktemp -d "${TMPDIR:-/tmp}/trees-generator-proof.XXXXXX")
back_up() {
  cp -R "$1" "$scratch/$2.partial"
  mv "$scratch/$2.partial" "$scratch/$2"
}
back_up "$L" classify.rs
back_up "$T" trees.rs
back_up "$cases" cases
rm -rf "$cases"
mkdir "$cases"

apply() {
  if ! cmp -s "$1" "$(written "$(basename "$1")")"; then
    echo "$1 changed during the run" >&2
    exit 2
  fi
  perl -0pi -e "$2" "$1"
  if cmp -s "$1" "$scratch/$(basename "$1")"; then
    echo "STALE: the pattern no longer matches $1" >&2
    exit 2
  fi
  cp "$1" "$scratch/$(basename "$1").applied"
}

failed=0
measure() {
  failed=0
  for _ in $(seq "$runs"); do
    rm -f "$cases"/*.txt
    if "${test[@]}" > "$scratch/log" 2>&1; then
      if ! grep -q 'test result: ok. 1 passed' "$scratch/log"; then
        cat "$scratch/log" >&2
        exit 2
      fi
    else
      if ! grep -q 'test result: FAILED' "$scratch/log"; then
        cat "$scratch/log" >&2
        exit 2
      fi
      if [ "${2:-}" = show ]; then
        cat "$scratch/log" >&2
      fi
      failed=$((failed + 1))
    fi
  done
  echo "$1: failed $failed of $runs runs at $KOIDRIVE_BATTERY_CASES cases"
}

measure "fix, nested rules" show
fix=$failed
apply "$L" "$mutant"
measure "mutant, nested rules"
nested=$failed
apply "$T" "$root_only"
measure "mutant, root-only rules"
[ "$fix" -eq 0 ] && [ "$nested" -gt 0 ]
