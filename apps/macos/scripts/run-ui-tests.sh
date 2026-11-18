#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs the UI tests against the fake core.
set -euo pipefail

cd "$(dirname "$0")/.."
result="${1:?usage: scripts/run-ui-tests.sh <result-bundle-path>}"

if ! automationmodetool | grep -q "DOES NOT REQUIRE"; then
  echo "::error::UI tests need Automation Mode without authentication on this runner. Run 'sudo automationmodetool enable-automationmode-without-authentication' on it once, or run this job on a hosted runner."
  exit 1
fi
defaults write -g AppleKeyboardUIMode -int 2

rm -rf "$result"
mkdir -p "$(dirname "$result")"
xcodebuild test -project Koidrive.xcodeproj -scheme Koidrive -configuration Debug \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  -test-timeouts-enabled YES -default-test-execution-time-allowance 60 \
  -resultBundlePath "$result" CODE_SIGN_IDENTITY=- 2>&1 | tee "${result%.xcresult}.log"
grep -E "Test Case .*(passed|failed)|Executed [0-9]+ test" "${result%.xcresult}.log"
