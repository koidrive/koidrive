#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

units=$(jq -c 'select(.reason == "compiler-artifact" and (.target.kind | any(. == "custom-build" or . == "proc-macro") | not))' "$@")
if [ -z "$units" ]; then
  echo "::error::$* lists no compiled unit, so the runtime checks of the tested code are unknown."
  exit 1
fi
weakened=$(jq -r 'select(.profile.debug_assertions != true or .profile.overflow_checks != true) | "\(.target.name) (\(.package_id)): debug_assertions=\(.profile.debug_assertions) overflow_checks=\(.profile.overflow_checks)"' <<< "$units")
if [ -n "$weakened" ]; then
  echo "::error::These units build without debug assertions or overflow checks, so tests would miss what those checks catch:"
  echo "$weakened"
  exit 1
fi
probe=$(jq -r 'select(.target.name == "koidrive_test_support" and .profile.test and .executable != null) | .executable' <<< "$units" | head -n 1)
if [ -z "$probe" ] || ! "$probe" --exact tests::the_build_keeps_debug_assertions_and_overflow_checks | grep -q '^test result: ok\. 1 passed'; then
  echo "::error::tests::the_build_keeps_debug_assertions_and_overflow_checks from koidrive-test-support must be built with these flags and pass, so a debug_assert! and an integer overflow are known to panic; RUSTFLAGS can turn both off without changing the profile Cargo reports."
  exit 1
fi
echo "$(wc -l <<< "$units" | tr -d ' ') units build with debug assertions and overflow checks, and the runtime probe passed."
