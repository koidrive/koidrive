// SPDX-License-Identifier: AGPL-3.0-or-later

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fromJson, toJson } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { CommitRecordSchema } from "../../src/gen/koidrive/v1/model_pb";
import { Device, Server, SpaceClient, stats } from "./client";
import { filePaths, removedByRewind, rewindMismatch } from "./rewind";
import { addStats, readState } from "./state";

const env = (name: string) => process.env[name] ?? "";

describe.skipIf(!env("REWIND_CHECK"))("the space around the e2e rewind", () => {
  it("is the state the server's log held at restore_seq", async () => {
    const { url, owner } = readState();
    if (!url || !owner) throw new Error("run.ts deploy and run.ts claim must run first: the state file has no server or owner");
    const space = new SpaceClient(new Device(new Server(url), owner), env("SPACE_ID"));
    const kept = join(env("E2E_TEMP"), "log-before-rewind.json");
    try {
      if (env("REWIND_CHECK") === "before") {
        const log = await space.log(env("HISTORY_ID"));
        writeFileSync(kept, JSON.stringify(log.map((commit) => toJson(CommitRecordSchema, commit))));
        console.log("Kept the log of history " + env("HISTORY_ID") + ": " + log.length + " commits");
        return;
      }
      const before = (JSON.parse(readFileSync(kept, "utf8")) as unknown[]).map((json) => fromJson(CommitRecordSchema, json as never));
      const tree = await space.tree();
      const restoreSeq = BigInt(env("RESTORE_SEQ"));
      expect(rewindMismatch(before, await space.log(env("HISTORY_ID")), tree, restoreSeq)).toBeUndefined();
      writeFileSync(join(env("E2E_TEMP"), "removed-by-rewind.json"), JSON.stringify(removedByRewind(before, restoreSeq)));
      let listing = "";
      for (const [path, node] of filePaths(tree)) {
        const content = node.current?.content;
        if (!content) throw new Error(path + " has no content on the server");
        listing += (await space.download(content)) + "  " + path + "\n";
      }
      writeFileSync(join(env("E2E_TEMP"), "rewound-server-listing.txt"), listing);
      console.log("The server restored seq " + env("RESTORE_SEQ") + " of " + before.length + ", the state its log held there:\n" + listing);
    } finally {
      addStats(stats);
    }
  });
});
