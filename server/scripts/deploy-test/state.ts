// SPDX-License-Identifier: AGPL-3.0-or-later

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const NAME_PREFIX = "koidrive-test-";

export interface Deployment {
  ref: string;
  sha: string;
  dir: string;
  versionId: string;
  atMs: number;
}

export interface Credentials {
  userId: string;
  deviceId: string;
  refreshCredential: string;
}

export interface Stats {
  requests: number;
  bytesUp: number;
  bytesDown: number;
}

export interface Written {
  spaceId: string;
  files: { nodeId: string; name: string; sha256: string; size: number }[];
  linkId: string;
  headSeq: string;
  second: Credentials;
}

export interface Owned {
  bucketCreationDate?: string;
  workerTag?: string;
}

export interface State {
  name: string;
  url?: string;
  owned: Owned;
  traceSampling?: number;
  logKey?: string;
  deployments: Deployment[];
  owner?: Credentials;
  stats: Stats;
  written?: Written;
}

export function statePath(): string {
  return process.env.KOIDRIVE_DEPLOY_TEST_STATE || join(process.env.RUNNER_TEMP || tmpdir(), "koidrive-deploy-test.json");
}

export function testName(): string {
  const name = process.env.KOIDRIVE_TEST_NAME ?? "";
  if (!name.startsWith(NAME_PREFIX) || !/^[a-z0-9-]{3,63}$/.test(name)) {
    throw new Error("KOIDRIVE_TEST_NAME must be a lowercase name of at most 63 characters starting with " + JSON.stringify(NAME_PREFIX));
  }
  return name;
}

export function readState(): State {
  const path = statePath();
  if (!existsSync(path)) return { name: testName(), owned: {}, deployments: [], stats: { requests: 0, bytesUp: 0, bytesDown: 0 } };
  return JSON.parse(readFileSync(path, "utf8")) as State;
}

export function writeState(state: State): void {
  writeFileSync(statePath(), JSON.stringify(state, null, 2), { mode: 0o600 });
}

export function addStats(stats: Stats): void {
  const state = readState();
  state.stats.requests += stats.requests;
  state.stats.bytesUp += stats.bytesUp;
  state.stats.bytesDown += stats.bytesDown;
  writeState(state);
}
