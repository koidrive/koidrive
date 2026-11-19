// SPDX-License-Identifier: AGPL-3.0-or-later

import { Error_Cause } from "../gen/koidrive/v1/rpc_pb";
import { type Identity } from "../auth";
import { type UsageBatch } from "../report/usage";

export type Role = "owner" | "editor" | "viewer";

const MAX_CLOSE_REASON_BYTES = 123;

export function closeReason(reason: string): string {
  const encoder = new TextEncoder();
  let kept = "";
  let bytes = 0;
  for (const char of reason) {
    bytes += encoder.encode(char).length;
    if (bytes > MAX_CLOSE_REASON_BYTES) break;
    kept += char;
  }
  return kept;
}

export const RETIRED_HISTORY =
  "the space's storage holds a history that a rewind replaced, so it was restored outside a rewind; the server serves it again once its operator restores the storage the rewind left (RFC 0003 section 4)";

export class CheckFailed {
  constructor(readonly cause: Error_Cause) {}
}

export const CONNECT_HEADER = "X-Koidrive-Connect";

export const TOKEN_GRACE_MS = 60_000;

export const Close = {
  TRY_AGAIN_LATER: 1013,
  PROTOCOL: 4000,
  TOKEN_EXPIRED: 4001,
  UNSUPPORTED_VERSION: 4002,
  DEVICE_REVOKED: 4003,
  SPACE_REWOUND: 4004,
} as const;

export interface Connect {
  identity: Identity;
  origin: string;
}

export interface Attachment {
  connId: string;
  identity: Identity;
  origin: string;
  role: Role;
  maxFormat: number;
  hello: boolean;
  live: boolean;
  features?: string[];
  usage?: UsageBatch;
}

export interface Session {
  deviceId: string;
  userId: string;
  deviceName: string;
  kind: string;
  scope: string | null;
  role: Role;
  maxFormat: number;
  origin: string;
}
