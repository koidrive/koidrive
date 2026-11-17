// SPDX-License-Identifier: AGPL-3.0-or-later

import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { configDefaults, defineConfig } from "vitest/config";

const bindings = { KOIDRIVE_RECOVERY_KEY: "test-recovery-key-for-local-workerd-only" };
/** Fills a Durable Object with sql.setMaxPageCountForTest, which workerd offers only under its experimental flag. */
const STORAGE_FULL = "test/storage-full.test.ts";
const SETUP = "test/setup.ts";

export default defineConfig({
  test: {
    projects: [
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./wrangler.jsonc" }, miniflare: { bindings } })],
        test: { name: "server", exclude: [...configDefaults.exclude, STORAGE_FULL], setupFiles: [SETUP] },
      },
      {
        plugins: [cloudflareTest({ wrangler: { configPath: "./wrangler.jsonc" }, miniflare: { bindings, compatibilityFlags: ["experimental"] } })],
        test: { name: "storage-full", include: [STORAGE_FULL], setupFiles: [SETUP] },
      },
    ],
  },
});
