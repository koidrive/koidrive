#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import pathlib
import shutil
import subprocess
import sys
import tempfile

catalog = pathlib.Path(sys.argv[1])
derived_data = pathlib.Path(sys.argv[2])
stringsdata = sorted(p for p in derived_data.rglob("*.stringsdata") if "/Koidrive.build/" in str(p))
if not stringsdata:
    sys.exit(f"No Koidrive .stringsdata under {derived_data}. Build the app with SWIFT_EMIT_LOC_STRINGS first.")

with tempfile.TemporaryDirectory() as scratch:
    synced = pathlib.Path(scratch) / catalog.name
    shutil.copy(catalog, synced)
    arguments = [arg for path in stringsdata for arg in ("--stringsdata", str(path))]
    subprocess.run(["xcrun", "xcstringstool", "sync", str(synced), *arguments], check=True)
    before = json.loads(catalog.read_text())["strings"]
    after = json.loads(synced.read_text())["strings"]

missing = sorted(set(after) - set(before))
stale = sorted(key for key, entry in after.items() if entry.get("extractionState") == "stale")
for key in missing:
    print(f"::error file={catalog}::{json.dumps(key)} is used in the app but missing from the catalog.")
for key in stale:
    print(f"::error file={catalog}::{json.dumps(key)} is in the catalog but no longer used in the app.")
if missing or stale:
    sys.exit(f"Add the missing keys to {catalog} and remove the stale ones.")
print(f"{catalog.name} lists every string the app uses ({len(before)} keys).")
