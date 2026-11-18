#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

scripts="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin"

cat >"$work/bin/xcodebuild" <<'STUB'
#!/usr/bin/env bash
touch "$STUB/xcodebuild-ran"
while [ "$#" -gt 0 ] && [ "$1" != -resultBundlePath ]; do shift; done
result="$2"
case "$(cat "$STUB/mode")" in
  fails) echo "Testing failed"; exit 65 ;;
  no-bundle) echo "Executed 0 tests, with 0 failures (0 unexpected)" ;;
  *)
    mkdir "$result"
    cp "$STUB/summary.json" "$result/summary.json"
    echo "Test Case '-[KoidriveUITests.StubUITests testStub]' passed (1.000 seconds)."
    echo "Executed 1 test, with 0 failures (0 unexpected)"
    ;;
esac
STUB
cat >"$work/bin/xcrun" <<'STUB'
#!/usr/bin/env bash
[ "$1 $2 $3 $4" = "xcresulttool get test-results summary" ] || exit 64
[ -f "$6/summary.json" ] || { echo "Error: File or directory doesn't exist at path: $6." >&2; exit 64; }
cat "$6/summary.json"
STUB
printf '#!/usr/bin/env bash\necho "Automation Mode DOES NOT REQUIRE user authentication."\n' >"$work/bin/automationmodetool"
printf '#!/usr/bin/env bash\n' >"$work/bin/defaults"
chmod +x "$work/bin/"*

failures=0
check() {
  local name="$1" mode="$2" executed="$3" want="$4"
  local stub="$work/$name"
  mkdir -p "$stub"
  echo "$mode" >"$stub/mode"
  printf '{"passedTests":%s,"failedTests":0,"expectedFailures":0,"skippedTests":2}' "$executed" >"$stub/summary.json"
  local result="${5:-$stub/out/UITests.xcresult}"
  local status=0
  STUB="$stub" PATH="$work/bin:$PATH" "$scripts/run-ui-tests.sh" "$result" >"$stub/output" 2>&1 || status=$?
  if [ "$status" -ne "$want" ]; then
    echo "FAIL $name: exit $status, expected $want"
    sed 's/^/  /' "$stub/output"
    failures=$((failures + 1))
  else
    echo "ok   $name (exit $status)"
  fi
}

check "a run that executes tests passes" passes 7 0
check "a failing xcodebuild keeps its exit status" fails 7 65
check "a run that leaves no result bundle fails" no-bundle 0 1
check "a result bundle with no executed test fails" passes 0 1

mkdir -p "$work/existing/data"
touch "$work/existing/data/keep"
check "an existing output path is refused" passes 7 1 "$work/existing/data"
if [ ! -e "$work/existing/data/keep" ]; then
  echo "FAIL an existing output path is refused: the directory was changed"
  failures=$((failures + 1))
fi
if [ -e "$work/an existing output path is refused/xcodebuild-ran" ]; then
  echo "FAIL an existing output path is refused: xcodebuild ran"
  failures=$((failures + 1))
fi

mkdir -p "$work/log-exists"
touch "$work/log-exists/UITests.log"
check "an existing log path is refused" passes 7 1 "$work/log-exists/UITests.xcresult"

[ "$failures" -eq 0 ]
