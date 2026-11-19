// SPDX-License-Identifier: AGPL-3.0-or-later

export const GIT_TRACKED = "git_tracked";

const ON_BY_DEFAULT = [GIT_TRACKED];

export function configuredFeatures(variable: string | undefined): Set<string> {
  const configured = new Set(ON_BY_DEFAULT);
  for (const entry of (variable ?? "").split(",").map((each) => each.trim())) {
    const name = entry.startsWith("-") ? entry.slice(1) : entry;
    if (!ON_BY_DEFAULT.includes(name)) continue;
    if (entry.startsWith("-")) configured.delete(name);
    else configured.add(name);
  }
  return configured;
}
