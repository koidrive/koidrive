// SPDX-License-Identifier: AGPL-3.0-or-later

export const CHUNK_BUNDLES = "chunk_bundles";
export const GIT_TRACKED = "git_tracked";

export const IMPLEMENTED: readonly string[] = [CHUNK_BUNDLES, GIT_TRACKED];

export function configuredFeatures(variable: string | undefined): Set<string> {
  const configured = new Set(IMPLEMENTED);
  for (const entry of (variable ?? "").split(",").map((each) => each.trim())) {
    const name = entry.startsWith("-") ? entry.slice(1) : entry;
    if (!IMPLEMENTED.includes(name)) continue;
    if (entry.startsWith("-")) configured.delete(name);
    else configured.add(name);
  }
  return configured;
}

export function featuresOf(env: Env): Set<string> {
  return configuredFeatures((env as Env & { KOIDRIVE_FEATURES?: string }).KOIDRIVE_FEATURES);
}
