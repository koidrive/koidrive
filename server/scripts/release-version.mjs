// SPDX-License-Identifier: AGPL-3.0-or-later

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export function releaseVersion(server = join(dirname(fileURLToPath(import.meta.url)), "..")) {
  const cargo = join(server, "..", "Cargo.toml");
  const snapshot = join(server, "release.json");
  const exported = existsSync(snapshot) ? JSON.parse(readFileSync(snapshot, "utf8")) : {};
  const version = existsSync(cargo)
    ? readFileSync(cargo, "utf8").split(/^\[/m).find((section) => section.startsWith("workspace.package]"))?.match(/^version\s*=\s*"([^"]+)"\s*(?:#.*)?$/m)?.[1]
    : exported.version;
  if (typeof version !== "string" || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error("Koidrive needs a release version in Cargo.toml, or release.json for a standalone server");
  }
  if (exported.version !== version) writeFileSync(snapshot, JSON.stringify({ version }, null, 2) + "\n");
  return version;
}

if (import.meta.main) releaseVersion();
