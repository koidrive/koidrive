// SPDX-License-Identifier: AGPL-3.0-or-later

import { errorText } from "./errors";
import { meteredStub } from "./report/meter";
import { UsageTally } from "./report/usage";

export const traffic = new UsageTally();
const gaugesDue = new Set<string>();

export function account(env: Env) {
  return meteredStub(env.ACCOUNT.get(env.ACCOUNT.idFromName("account")), (carried) => traffic.receive(carried, true));
}

export function space(env: Env, spaceId: string) {
  return meteredStub(
    env.SPACE.get(env.SPACE.idFromName(spaceId)),
    (carried) => {
      traffic.receive(carried, false);
      if (carried.gaugesDue) gaugesDue.add(spaceId);
    },
    { gauges: true },
  );
}

export async function noteGauges(env: Env): Promise<void> {
  const spaceIds = [...gaugesDue];
  gaugesDue.clear();
  await Promise.all(spaceIds.map((spaceId) => space(env, spaceId).noteStored().catch((error: unknown) => console.error("stored sizes not noted", errorText(error)))));
}
