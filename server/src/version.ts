// SPDX-License-Identifier: AGPL-3.0-or-later

import release from "../release.json";

const build: { version: string; commit?: string } = release;

export const SERVER_VERSION = build.version;
export const SERVER_COMMIT = build.commit ?? "";
export const PROTO_MIN = 1;
export const PROTO_MAX = 1;
