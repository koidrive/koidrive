#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
dir="$(dirname "$0")"
shift
printf '%s\n' "$*" >> "$dir/calls"
printf '%s %s %s\n' "$HOME" "$XDG_STATE_HOME" "${XDG_RUNTIME_DIR-unset}" > "$dir/env"
case "$1-$2" in
  join-*|server-claim|share-create) cat > "$dir/stdin-$1" ;;
esac
if [ "$1" = status ] && [ -e "$dir/snapshot-next-status" ] && mv "$dir/snapshot-next-status" "$dir/snapshot-status-taken" 2>/dev/null; then
  cat "$dir/status.json" > "$dir/status-snapshot.json"
  touch "$dir/status-snapshot-ready"
  while [ -e "$dir/hold-status-snapshot" ]; do sleep 0.01; done
  cat "$dir/status-snapshot.json"
  exit 0
fi
while [ -e "$dir/hold-$1" ]; do sleep 0.01; done
for name in "$1-$2" "$1"; do
  if [ -f "$dir/$name.json" ]; then exec cat "$dir/$name.json"; fi
done
exit 3
