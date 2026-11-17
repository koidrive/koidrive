// SPDX-License-Identifier: AGPL-3.0-or-later

import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { VIEWER_HEADERS } from "../src/share/routes";
import worker from "../src/worker";

const BASE = "https://koidrive.test";
const LINK = "AAECAwQFBgcICQoLDA0ODw";

function expectViewerHeaders(response: Response) {
  for (const [name, value] of Object.entries(VIEWER_HEADERS)) expect(response.headers.get(name), name).toBe(value);
}

describe("share links: web viewer (RFC 0005 section 8)", () => {
  it("serves the same page for every well-formed link id without looking the link up", async () => {
    const unreachable = { ...env, ACCOUNT: { idFromName: () => { throw new Error("the viewer page reached the Account DO"); } } } as unknown as Env;
    const response = await worker.fetch(new Request(`${BASE}/s/${LINK}`) as Parameters<typeof worker.fetch>[0], unreachable, { waitUntil() {} } as unknown as ExecutionContext);
    expect(response.status).toBe(200);
    expect(response.headers.get("Content-Type")).toBe("text/html; charset=utf-8");
    expect(response.headers.get("Cache-Control")).toBe("no-store");
    expectViewerHeaders(response);
    const page = await response.text();
    expect(page).toMatch(/<script type="module" (crossorigin )?src="\/viewer\/viewer\.js"><\/script>/);
    expect(page).toMatch(/<link rel="stylesheet" (crossorigin )?href="\/viewer\/viewer\.css">/);
    for (const script of page.match(/<script\b[^>]*>/g) ?? []) expect(script, "every script is a same-origin file").toMatch(/ src="\/[^/]/);
    expect(page).not.toMatch(/<style\b| style=| on[a-z]+=|https?:\/\//i);

    const head = await exports.default.fetch(`${BASE}/s/${LINK}`, { method: "HEAD" });
    expect(head.status).toBe(200);
    expectViewerHeaders(head);
  });

  it("answers a malformed link id with 404 and another method with 405, under the same headers", async () => {
    for (const id of [LINK.slice(1), LINK + "A", LINK.replace(/.$/, "B"), "", "..%2F.."]) {
      const response = await exports.default.fetch(`${BASE}/s/${id}`);
      expect(response.status, id).toBe(404);
      expectViewerHeaders(response);
    }
    const post = await exports.default.fetch(`${BASE}/s/${LINK}`, { method: "POST" });
    expect(post.status).toBe(405);
    expectViewerHeaders(post);
  });

  it("serves its assets with the viewer headers, and they load nothing from another origin", async () => {
    for (const [path, type] of [
      ["/viewer/viewer.js", "javascript"],
      ["/viewer/viewer.css", "text/css"],
    ] as const) {
      const response = await env.ASSETS.fetch(`${BASE}${path}`);
      expect(response.status, path).toBe(200);
      expect(response.headers.get("Content-Type")).toContain(type);
      expectViewerHeaders(response);
      const text = await response.text();
      expect(text).not.toMatch(/https?:\/\//);
      expect(text).not.toMatch(/innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(|new Function/);
    }
  });
});
