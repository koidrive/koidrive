// SPDX-License-Identifier: AGPL-3.0-or-later

import { afterEach, describe, expect, it, vi } from "vitest";
import type { State } from "./state.ts";
import { authorization, type Case, measure, ownedBucket, percentile } from "./ranged-read.ts";

describe("ranged-read benchmark", () => {
  it("signs a ranged GET as AWS's Signature Version 4 example does", () => {
    const signed = authorization(
      { id: "AKIAIOSFODNN7EXAMPLE", secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY" },
      {
        method: "GET",
        url: new URL("https://examplebucket.s3.amazonaws.com/test.txt"),
        headers: { Range: "bytes=0-9" },
        payloadHash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        amzDate: "20130524T000000Z",
        region: "us-east-1",
      },
    );
    expect(signed).toBe(
      "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
    );
  });

  it("takes nearest-rank percentiles", () => {
    const sorted = Array.from({ length: 200 }, (_, i) => i + 1);
    expect([percentile(sorted, 50), percentile(sorted, 90), percentile(sorted, 99)]).toEqual([100, 180, 198]);
  });

  describe("bucket ownership", () => {
    const name = "koidrive-test-1-1";
    const account = { id: "1".repeat(32), token: "token" };
    const state = (owned: State["owned"], stateName = name): State => ({ name: stateName, owned, deployments: [], stats: { requests: 0, bytesUp: 0, bytesDown: 0 } });
    const bucketCreated = (creationDate: string) => {
      const requests: string[] = [];
      vi.stubGlobal("fetch", async (input: string | URL) => {
        requests.push(new URL(input).pathname);
        return Response.json({ success: true, result: { name, creation_date: creationDate } });
      });
      return requests;
    };
    afterEach(() => vi.unstubAllGlobals());

    it("accepts the bucket this run created", async () => {
      const requests = bucketCreated("2026-11-20T00:00:00.000Z");
      await expect(ownedBucket(account, state({ bucketCreationDate: "2026-11-20T00:00:00.000Z" }), name)).resolves.toBe(name);
      expect(requests).toEqual([`/client/v4/accounts/${account.id}/r2/buckets/${name}`]);
    });

    it.each([
      ["names another bucket", state({ bucketCreationDate: "2026-11-20T00:00:00.000Z" }, "fixture-unowned")],
      ["records no bucket", state({})],
      ["records a bucket created at another time", state({ bucketCreationDate: "2026-11-19T00:00:00.000Z" })],
    ])("refuses a state file that %s", async (_, given) => {
      bucketCreated("2026-11-20T00:00:00.000Z");
      await expect(ownedBucket(account, given, name)).rejects.toThrow(/bucket/);
    });
  });

  it("reads every target once before the warm rounds and keeps one warm sample per kind per round", async () => {
    const calls: string[] = [];
    const cases: Case[] = ["a", "b"].map((name, k) => ({ name, count: k + 2, read: async (i) => (calls.push(`${name}${i}`), 1) }));
    const { first, warm } = await measure(cases, 7);
    expect(calls.slice(0, 5).sort()).toEqual(["a0", "a1", "b0", "b1", "b2"]);
    expect([first.get("a")!.length, first.get("b")!.length, warm.get("a")!.length, warm.get("b")!.length]).toEqual([2, 3, 7, 7]);
  });
});
