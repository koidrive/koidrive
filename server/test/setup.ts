// SPDX-License-Identifier: AGPL-3.0-or-later

import { beforeEach } from "vitest";
import { UsageTally } from "../src/report/usage";

beforeEach(() => {
  UsageTally.windowMs = 0;
});
