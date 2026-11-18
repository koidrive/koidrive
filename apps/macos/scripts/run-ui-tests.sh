#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

cd "$(dirname "$0")/.."
result="${1:?usage: scripts/run-ui-tests.sh <new-result-bundle-path>}"
log="${result%.xcresult}.log"

for path in "$result" "$log"; do
  if [ -e "$path" ]; then
    echo "::error::$path already exists. Pass a path for a new result bundle."
    exit 1
  fi
done

if ! automationmodetool | grep -q "DOES NOT REQUIRE"; then
  echo "::error::UI tests need Automation Mode without authentication on this runner. Run 'sudo automationmodetool enable-automationmode-without-authentication' on it once, or run this job on a hosted runner."
  exit 1
fi
defaults write -g AppleKeyboardUIMode -int 2

mkdir -p "$(dirname "$result")"
xcodebuild test -project Koidrive.xcodeproj -scheme Koidrive -configuration Debug \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  -test-timeouts-enabled YES -default-test-execution-time-allowance 60 \
  -resultBundlePath "$result" CODE_SIGN_IDENTITY=- 2>&1 | tee "$log"

if ! summary="$(xcrun xcresulttool get test-results summary --path "$result" --compact)"; then
  echo "::error::xcodebuild left no readable result bundle at $result."
  exit 1
fi
executed="$(python3 -I -c 'import json, sys; s = json.load(sys.stdin); print(s["passedTests"] + s["failedTests"] + s["expectedFailures"])' <<<"$summary")"
if [ "$executed" -eq 0 ]; then
  echo "::error::$result records no executed UI test."
  exit 1
fi
grep -E "Test Case .*(passed|failed)|Executed [0-9]+ test" "$log"
