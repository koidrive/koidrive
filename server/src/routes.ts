// SPDX-License-Identifier: AGPL-3.0-or-later

const ROUTES: [string, RegExp][] = [
  ["chunk", /^\/v1\/spaces\/[^/]+\/chunks\/[^/]+$/],
  ["chunk_bundle", /^\/v1\/spaces\/[^/]+\/chunks$/],
  ["rpc", /^\/v1\/spaces\/[^/]+\/rpc$/],
  ["ws", /^\/v1\/spaces\/[^/]+\/ws$/],
  ["links", /^\/v1\/spaces\/[^/]+\/links(?:\/[^/]+)?$/],
  ["rewind", /^\/v1\/spaces\/[^/]+\/rewind$/],
  ["usage", /^\/v1\/spaces\/[^/]+\/usage$/],
  ["spaces", /^\/v1\/spaces$/],
  ["devices", /^\/v1\/devices(?:\/[^/]+)?$/],
  ["token", /^\/v1\/token$/],
  ["claim", /^\/v1\/claim$/],
  ["pair_start", /^\/v1\/pair\/start$/],
  ["pair_complete", /^\/v1\/pair\/complete$/],
  ["server", /^\/v1\/server$/],
  ["setup", /^\/setup$/],
  ["share", /^\/v1\/share\//],
  ["share_page", /^\/s\//],
];
const METHODS = new Set(["GET", "HEAD", "POST", "PUT", "DELETE"]);

export function routeName(method: string, path: string): string {
  const name = ROUTES.find(([, pattern]) => pattern.test(path))?.[0] ?? "other";
  return (METHODS.has(method) ? method.toLowerCase() : "other") + "_" + name;
}
