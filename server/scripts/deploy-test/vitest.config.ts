// SPDX-License-Identifier: AGPL-3.0-or-later

import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  test: {
    include: ["smoke.ts", "upgrade-write.ts", "upgrade-verify.ts", "run.unit.ts", "e2e.unit.ts", "load.ts", "load.unit.ts", "bench.unit.ts", "first-sync.unit.ts", "rewind-check.ts"],
    environment: "node",
    testTimeout: 20 * 60_000,
    fileParallelism: false,
    bail: 1,
  },
});
