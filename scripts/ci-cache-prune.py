#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from datetime import datetime, timedelta, timezone
import json
import re
import sys

HASHES = re.compile(r"-(?:[0-9a-f]{8}|[0-9a-f]{64})(?=-|$)")
IDLE = timedelta(hours=1)


def when(stamp):
    return datetime.fromisoformat(stamp.replace("Z", "+00:00"))


def superseded(caches, now):
    main = sorted((c for c in caches if c["ref"] == "refs/heads/main"), key=lambda c: c["created_at"], reverse=True)
    newest = {}
    for cache in main:
        newest.setdefault((HASHES.sub("", cache["key"]), cache["version"]), cache["id"])
    return [c for c in main if newest[(HASHES.sub("", c["key"]), c["version"])] != c["id"]
            and now - when(c["last_accessed_at"]) > IDLE]


if __name__ == "__main__":
    for cache in superseded(json.load(sys.stdin), datetime.now(timezone.utc)):
        print(f"{cache['id']}\t{cache['size_in_bytes']}\t{cache['key']}")
