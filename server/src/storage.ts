// SPDX-License-Identifier: AGPL-3.0-or-later

import { MIGRATIONS as ACCOUNT } from "./account/schema";
import { MIGRATIONS as SPACE } from "./space/schema";

export const STORAGE = { account: ACCOUNT.length, space: SPACE.length };
