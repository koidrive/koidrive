// SPDX-License-Identifier: AGPL-3.0-or-later

import { DurableObject } from "cloudflare:workers";
import { Error_Code } from "./gen/koidrive/v1/rpc_pb";
import type { Database, ObjectState, Sql } from "./host";
import { errorReply } from "./http";
import { uuidv7 } from "./ids";
import { countedBucket, countedState, meteredObject, meteredReply, meteredStub, type Reply } from "./report/meter";
import { bootstrapExport, requestSnapshot } from "./space/export";
import { backfillMigratedIntents } from "./space/intents";
import { type Pitr, pitrBackend } from "./space/pitr";
import { migrate } from "./space/schema";
import { type Attachment, CheckFailed, CONNECT_HEADER, type Connect, RETIRED_HISTORY, spaceLogic, TOKEN_GRACE_MS } from "./space/space";

export { Close, CONNECT_HEADER, GRANT_MS, HOLD_MS, MAX_HEAD_CHECKS } from "./space/space";
export type { Role, Session } from "./space/space";

export class SpaceDO extends spaceLogic(DurableObject<Env>) {
  protected readonly state: ObjectState;
  protected readonly db: Database;
  protected readonly sql: Sql;
  protected readonly pitr: Pitr;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.state = countedState(ctx, this.traffic);
    this.db = this.state.storage;
    this.sql = this.db.sql;
    this.pitr = pitrBackend.open(ctx);
    this.db.transactionSync(() => {
      migrate(this.sql);
      if (backfillMigratedIntents(this.sql)) requestSnapshot(this.sql, this.clock());
      bootstrapExport(this.sql, this.clock());
    });
    this.loadFlag();
    void ctx.blockConcurrencyWhile(() => this.armAlarm());
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
    return meteredObject(this, "space", this.traffic, (event, args, waited) => this.ended(event, args, waited));
  }

  metered(method: string, args: unknown[]): Promise<Reply<unknown>> {
    return meteredReply(this, "space", this.traffic, (event, called, waited) => this.ended(event, called, waited), method, args);
  }

  protected get objects(): R2Bucket {
    return countedBucket(this.env.BUCKET, this.traffic);
  }

  protected account({ returnCounts } = { returnCounts: true }) {
    return meteredStub(this.env.ACCOUNT.get(this.env.ACCOUNT.idFromName("account")), (carried) => this.received(carried), returnCounts);
  }

  override async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") return new Response("expected a WebSocket upgrade", { status: 426 });
    const connect = JSON.parse(request.headers.get(CONNECT_HEADER) ?? "null") as Connect | null;
    if (!connect) return new Response("missing identity", { status: 401 });
    const role = this.memberRole(connect.identity.userId);
    if (!role) return new Response("not a member of this space", { status: 403 });
    const check = await this.admitDevice(connect.identity.deviceId);
    if (check instanceof CheckFailed) return errorReply(request, Error_Code.RETRY_LATER, "could not check the device; retry later", 503, check.cause);
    if (!check.active) return new Response("device revoked", { status: 401 });
    if (check.closed) return new Response("the space is being rewound; retry later", { status: 503 });
    if (this.historyRetired(check)) return errorReply(request, Error_Code.HISTORY_MISMATCH, RETIRED_HISTORY, 409);
    const [client, server] = Object.values(new WebSocketPair()) as [WebSocket, WebSocket];
    this.ctx.acceptWebSocket(server, [connect.identity.deviceId]);
    const attachment: Attachment = { connId: uuidv7(), identity: connect.identity, origin: connect.origin, role, maxFormat: 0, hello: false, live: false };
    server.serializeAttachment(attachment);
    await this.scheduleTokenSweep(connect.identity.expiresAtMs + TOKEN_GRACE_MS);
    return new Response(null, { status: 101, webSocket: client });
  }
}
