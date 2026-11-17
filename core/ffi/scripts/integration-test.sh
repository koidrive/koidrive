#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Needs the server's dependencies (pnpm install in server/). Extra arguments go to the test binary.

set -euo pipefail
set -m

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
work="$(mktemp -d)"
port="${KOIDRIVE_TEST_PORT:-0}"
log="$work/wrangler.log"
wrangler="$root/server/node_modules/.bin/wrangler"

if [ ! -x "$wrangler" ]; then
  echo "error: $wrangler is missing; run 'pnpm install' in server/ first." >&2
  exit 1
fi

cleanup() {
  status=$?
  if [ -n "${server_pid:-}" ]; then
    kill -TERM -- "-$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "--- wrangler dev output ($log) ---" >&2
    tail -n 200 "$log" >&2 || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# Build first, so the Worker is not kept waiting for the compiler.
cd "$root"
cargo build -p koidrive-cli --locked
cargo test -p koidrive-ffi --locked --lib --no-run
koi="${CARGO_TARGET_DIR:-$root/target}/debug/koi"

# set -m puts the server in its own process group, so cleanup stops workerd with it.
(cd "$root/server" && WRANGLER_SEND_METRICS=false exec "$wrangler" dev --ip 127.0.0.1 --port "$port" \
  --inspector-port 0 --persist-to "$work/state" --show-interactive-dev-session=false) >"$log" 2>&1 &
server_pid=$!

for _ in $(seq 1 120); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "error: wrangler dev exited before it was ready." >&2
    exit 1
  fi
  if server_url=$(python3 "$root/scripts/worker_ready.py" "$log"); then
    break
  else
    parse_status=$?
    if [ "$parse_status" -ne 1 ]; then
      exit "$parse_status"
    fi
  fi
  sleep 0.5
done
if [ -z "${server_url:-}" ]; then
  echo "error: wrangler dev was not ready on port $port after 60 seconds." >&2
  exit 1
fi

KOIDRIVE_TEST_SERVER="$server_url" KOIDRIVE_TEST_SERVER_LOG="$log" KOIDRIVE_TEST_KOI="$koi" DO_NOT_TRACK=1 \
  cargo test -p koidrive-ffi --locked --lib -- --ignored --nocapture "$@"
