// SPDX-License-Identifier: AGPL-3.0-or-later

import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { configDefaults, defineConfig } from "vitest/config";

const bindings = { KOIDRIVE_RECOVERY_KEY: "test-recovery-key-for-local-workerd-only" };
/** Fills a Durable Object with sql.setMaxPageCountForTest, which workerd offers only under its experimental flag. */
const STORAGE_FULL = "test/storage-full.test.ts";
const GC_MODEL = "test/gc-model.test.ts";
const SETUP = "test/setup.ts";
const provide = { batteryCases: process.env.KOIDRIVE_BATTERY_CASES ?? "", deterministic: process.env.KOIDRIVE_PROPTEST_DETERMINISTIC ?? "" };

export default defineConfig({
  test: {
    projects: [
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./wrangler.jsonc" }, miniflare: { bindings } })],
        test: { name: "server", exclude: [...configDefaults.exclude, STORAGE_FULL, GC_MODEL], setupFiles: [SETUP] },
      },
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./wrangler.jsonc" }, miniflare: { bindings } })],
        test: { name: "gc-model", include: [GC_MODEL], setupFiles: [SETUP], provide, sequence: { groupOrder: 1 } },
      },
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./wrangler.jsonc" }, miniflare: { bindings, compatibilityFlags: ["experimental"] } })],
        test: { name: "storage-full", include: [STORAGE_FULL], setupFiles: [SETUP] },
      },
    ],
  },
});
