#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

targeted=false
case "${1:-}" in
  '') [ $# -eq 0 ] ;;
  --targeted) [ $# -eq 1 ]; targeted=true ;;
  *) echo "usage: bash scripts/test-engine-mutations.sh [--targeted]" >&2; exit 2 ;;
esac

names=()
files=()
exprs=()
mutation() {
  names+=("$1")
  files+=("$2")
  exprs+=("$3")
}
source scripts/engine-mutations.sh

scratch=$(mktemp -d "${TMPDIR:-/tmp}/engine-mutations.XXXXXX")
restore() {
  if [ -f "$scratch/original" ]; then
    cp "$scratch/original" "$file"
    touch "$file"
  fi
  rm -rf "$scratch"
}
trap restore EXIT

status=0
for i in "${!names[@]}"; do
  file=${files[$i]}
  if [ ! -f "$file" ]; then
    echo "${names[$i]}: STALE ($file does not exist)" >&2
    status=1
  elif ! perl -0pe "${exprs[$i]}" "$file" > "$scratch/mutated"; then
    echo "${names[$i]}: BROKEN (the mutation expression failed)" >&2
    status=1
  elif cmp -s "$file" "$scratch/mutated"; then
    echo "${names[$i]}: STALE (the pattern no longer matches $file; re-anchor it in scripts/engine-mutations.sh)" >&2
    status=1
  else
    echo "${names[$i]}: matches"
  fi
done
[ "$status" -eq 0 ] || exit "$status"
python3 scripts/test-mutations.py -v
$targeted || exit 0

git archive HEAD | tar -x -C "$scratch"
cd "$scratch"
export CARGO_TARGET_DIR="$scratch/target"
export PROPTEST_CASES=8
tests=(
  a_policy_halt_takes_a_server_loss_once
  a_policy_refusal_keeps_the_commit_and_outlasts_any_welcome
  a_renamed_result_reverts_the_local_name_to_the_server_one
)
mutants=(refused-reconnect unknown-resend renamed-revert)
for test in "${tests[@]}"; do
  nice -n 10 cargo test --locked -p koidrive-engine --test commit "$test" -- --exact
done
for n in "${!mutants[@]}"; do
  found=false
  for i in "${!names[@]}"; do
    [ "${names[$i]}" = "${mutants[$n]}" ] || continue
    found=true
    file=${files[$i]}
    cp "$file" "$scratch/original"
    perl -0pi -e "${exprs[$i]}" "$file"
    nice -n 10 cargo test --locked -p koidrive-engine --test commit --no-run
    if nice -n 10 cargo test --locked -p koidrive-engine --test commit "${tests[$n]}" -- --exact > "$scratch/result" 2>&1; then
      cat "$scratch/result"
      echo "${mutants[$n]}: SURVIVED" >&2
      exit 1
    fi
    cat "$scratch/result"
    grep -F "test ${tests[$n]} ... FAILED" "$scratch/result"
    grep -F 'test result: FAILED. 0 passed; 1 failed;' "$scratch/result"
    cp "$scratch/original" "$file"
    touch "$file"
    rm "$scratch/original"
    nice -n 10 cargo test --locked -p koidrive-engine --test commit "${tests[$n]}" -- --exact
    echo "${mutants[$n]}: killed by ${tests[$n]}"
  done
  if ! $found; then
    echo "${mutants[$n]}: missing mutation" >&2
    exit 1
  fi
done
