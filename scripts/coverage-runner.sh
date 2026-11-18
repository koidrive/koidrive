#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -uo pipefail

if [ -z "${NEXTEST_TEST_NAME:-}" ]; then
  exec "$@"
fi
folder="$COVERAGE_PROFILES/$(printf '%s %s' "$NEXTEST_BINARY_ID" "$NEXTEST_TEST_NAME" | shasum -a 256 | cut -c 1-32)"
attempt="$folder/attempt-$NEXTEST_ATTEMPT"
mkdir -p "$attempt"
printf '%s\n%s\n%s\n' "$NEXTEST_BINARY_ID" "$NEXTEST_TEST_NAME" "$1" > "$folder/test"
LLVM_PROFILE_FILE="$attempt/%p.profraw" "$@"
status=$?
shopt -s nullglob
raw=("$attempt"/*.profraw)
if [ "${#raw[@]}" -gt 0 ]; then
  "$COVERAGE_LLVM/llvm-profdata" merge -sparse -o "$attempt/test.profdata" "${raw[@]}" && rm -f "${raw[@]}"
fi
exit "$status"
