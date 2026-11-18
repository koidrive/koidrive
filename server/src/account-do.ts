// SPDX-License-Identifier: AGPL-3.0-or-later

import { DurableObject } from "cloudflare:workers";
import { accountLogic } from "./account/account";
import { migrate } from "./account/schema";
import type { Database, ObjectState, Sql } from "./host";
import { countedState, meteredObject, meteredReply, meteredStub, type Reply } from "./report/meter";
import { ShareLinks } from "./share/links";

export type { Outcome } from "./share/links";

export class AccountDO extends accountLogic(DurableObject<Env>) {
  protected readonly state: ObjectState;
  protected readonly db: Database;
  protected readonly sql: Sql;
  protected readonly shares: ShareLinks;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.state = countedState(ctx, this.tally);
    this.db = this.state.storage;
    this.sql = this.db.sql;
    this.db.transactionSync(() => migrate(this.sql));
    this.reporting();
    this.shares = new ShareLinks(this.sql, {
      clock: () => this.clock(),
      serverSecret: async () => (await this.serverKey()).secret,
      spaceClosed: (spaceId) => this.spaceClosed(spaceId),
    });
    return meteredObject(this, "account", this.tally, (_event, _args, replying) => this.ended(replying));
  }

  metered(method: string, args: unknown[]): Promise<Reply<unknown>> {
    return meteredReply(this, "account", this.tally, (_event, _args, replying) => this.ended(replying), method, args);
  }

  protected spaceStub(spaceId: string) {
    return meteredStub(this.env.SPACE.get(this.env.SPACE.idFromName(spaceId)), (carried) => this.received(carried));
  }
}
