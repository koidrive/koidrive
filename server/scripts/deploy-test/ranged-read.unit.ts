// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from "vitest";
import { authorization, percentile } from "./ranged-read.ts";

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
});
