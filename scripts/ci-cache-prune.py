#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import json
import re
import sys

HASHES = re.compile(r"(?:-(?:[0-9a-f]{8}|[0-9a-f]{64}))+$")


def superseded(caches):
    main = sorted((c for c in caches if c["ref"] == "refs/heads/main"), key=lambda c: c["created_at"], reverse=True)
    newest = {}
    for cache in main:
        newest.setdefault(HASHES.sub("", cache["key"]), cache["id"])
    return [c for c in main if newest[HASHES.sub("", c["key"])] != c["id"]]


if __name__ == "__main__":
    for cache in superseded(json.load(sys.stdin)):
        print(f"{cache['id']}\t{cache['size_in_bytes']}\t{cache['key']}")
