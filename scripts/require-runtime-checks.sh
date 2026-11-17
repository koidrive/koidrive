#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

build_json=$1
units=$(jq -c 'select(.reason == "compiler-artifact" and (.target.kind | any(. == "custom-build" or . == "proc-macro") | not))' "$build_json")
if [ -z "$units" ]; then
  echo "::error::$build_json lists no compiled unit, so the runtime checks of the tested code are unknown."
  exit 1
fi
weakened=$(jq -r 'select(.profile.debug_assertions != true or .profile.overflow_checks != true) | "\(.target.name) (\(.package_id)): debug_assertions=\(.profile.debug_assertions) overflow_checks=\(.profile.overflow_checks)"' <<< "$units")
if [ -n "$weakened" ]; then
  echo "::error::These units build without debug assertions or overflow checks, so tests would miss what those checks catch:"
  echo "$weakened"
  exit 1
fi
echo "$(wc -l <<< "$units" | tr -d ' ') units build with debug assertions and overflow checks."
