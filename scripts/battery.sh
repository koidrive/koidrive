#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Starts the property-test battery (.github/workflows/property-battery.yml) on
# GitHub Actions for a branch or tag and prints the run URL. The run uses that
# ref's own workflow file and cache scope. See docs/testing.md.
#
# Usage: scripts/battery.sh [--cases N] [--shards N] [--filter TEXT] [--mutations] [--wait] [REF]
#   REF           branch or tag to test; defaults to the current branch, which
#                 must be pushed (the battery tests what is on GitHub)
#   --cases N     cases per opted-in stress property on each shard (default 8192)
#   --shards N    number of shards, each with its own seed (default 24, at most 256)
#   --filter TEXT test name filter for the property test binaries
#   --mutations   also run the mutation set (scripts/mutations.sh)
#   --wait        wait for the run and exit with its result
set -euo pipefail

usage() { sed -n 's/^# \{0,1\}//; 7,15p' "$0"; }

fields=()
ref=""
wait=false
while [ $# -gt 0 ]; do
  case "$1" in
    --cases | --shards | --filter)
      if [ $# -lt 2 ]; then
        echo "$1 needs a value" >&2
        exit 2
      fi
      fields+=(-f "${1#--}=$2")
      shift
      ;;
    --mutations) fields+=(-f mutations=true) ;;
    --wait) wait=true ;;
    -h | --help) usage; exit 0 ;;
    -*) usage >&2; exit 2 ;;
    *)
      if [ -n "$ref" ]; then
        usage >&2
        exit 2
      fi
      ref=$1
      ;;
  esac
  shift
done

current=false
if [ -z "$ref" ]; then
  if ! ref=$(git symbolic-ref --quiet --short HEAD); then
    echo "HEAD is detached: name the branch or tag to test." >&2
    exit 2
  fi
  current=true
fi
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
remote=""
for kind in heads tags; do
  if remote=$(gh api "repos/$repo/git/ref/$kind/$ref" --jq .object.sha 2> /dev/null); then
    break
  fi
  remote=""
done
if [ -z "$remote" ]; then
  echo "$ref is not a branch or tag on $repo. Push it first: the battery runs on GitHub, on the ref itself." >&2
  exit 1
fi
if $current && [ "$remote" != "$(git rev-parse HEAD)" ]; then
  echo "Note: $ref on $repo is at ${remote:0:12}, not at your HEAD. The battery tests ${remote:0:12}." >&2
fi

output=$(gh workflow run property-battery.yml --repo "$repo" --ref "$ref" ${fields[@]+"${fields[@]}"} 2>&1) || {
  echo "$output" >&2
  echo "The run needs .github/workflows/property-battery.yml on $ref itself; a branch from before it reached main does not have it." >&2
  exit 1
}
url=$(grep -oE 'https://[^ ]+/actions/runs/[0-9]+' <<< "$output" | head -n 1 || true)
if [ -z "$url" ]; then
  echo "$output"
  echo "The battery started, but gh did not print the run URL. Find the run with: gh run list --repo $repo --workflow property-battery.yml" >&2
  if $wait; then
    echo "Cannot wait without the run URL; follow it with gh run watch <run-id> --repo $repo --exit-status." >&2
    exit 1
  fi
  exit 0
fi
echo "$url"
if $wait; then
  gh run watch "${url##*/}" --repo "$repo" --compact --exit-status --interval 60
fi
