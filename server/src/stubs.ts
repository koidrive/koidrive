// SPDX-License-Identifier: AGPL-3.0-or-later

import { meteredStub } from "./report/meter";
import { UsageTally } from "./report/usage";

export const traffic = new UsageTally();

export function account(env: Env) {
  return meteredStub(env.ACCOUNT.get(env.ACCOUNT.idFromName("account")), (carried) => traffic.receive(carried, true));
}

export function space(env: Env, spaceId: string) {
  return meteredStub(env.SPACE.get(env.SPACE.idFromName(spaceId)), (carried) => traffic.receive(carried, false));
}
