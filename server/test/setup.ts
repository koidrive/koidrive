// SPDX-License-Identifier: AGPL-3.0-or-later

import { beforeEach } from "vitest";
import { BATCH_MS, UsageTally } from "../src/report/usage";

beforeEach(() => {
  UsageTally.quietMs = 0;
  UsageTally.batchMs = BATCH_MS;
});
