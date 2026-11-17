// SPDX-License-Identifier: AGPL-3.0-or-later

import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { create } from "@bufbuild/protobuf";
import { afterEach, describe, expect, it } from "vitest";
import { CommitRecordSchema, NodeKind, type NodeState, NodeStateSchema, NodeStatus } from "../../src/gen/koidrive/v1/model_pb";
import { type Content, contents, format, keypair, listing, matches, parseListing, removedByRewind, seal, startDaemon, trees, unseal } from "./e2e.ts";
import { replay, rewindMismatch } from "./rewind.ts";

const roots: string[] = [];

function folder(files: Record<string, Content>): string {
  const root = mkdtempSync(join(tmpdir(), "koidrive-e2e-unit-"));
  roots.push(root);
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), contents[content]);
  }
  return root;
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

const conflicted: Record<string, Content> = {
  "docs/from-mac-renamed.txt": "fromMacEdited",
  "notes/random-moved.bin": "randomMac",
  "notes/random-from-linux.bin": "randomLinux",
  "shared.txt": "sharedMac",
  "shared (conflicted copy from linux).txt": "sharedLinux",
};

describe("a folder against a scenario's tree", () => {
  it("matches exactly the tree, finds a conflict copy by its pattern and skips the .koidrive folder", () => {
    const root = folder({ ...conflicted, ".koidrive/store.db": "fromMac" });
    expect(matches(listing(root), trees.conflicted)).toBe(true);
    expect(matches(listing(root), trees.deleted)).toBe(false);
  });

  it("takes either content under either name of a conflict", () => {
    const root = folder({ ...conflicted, "shared.txt": "sharedLinux", "shared (conflicted copy from linux).txt": "sharedMac" });
    expect(matches(listing(root), trees.conflicted)).toBe(true);
  });

  it("refuses a missing file, an extra file, other content and a file at another path", () => {
    const { "shared.txt": _, ...missing } = conflicted;
    expect(matches(listing(folder(missing)), trees.conflicted)).toBe(false);
    expect(matches(listing(folder({ ...conflicted, "notes/extra.txt": "fromMac" })), trees.conflicted)).toBe(false);
    expect(matches(listing(folder({ ...conflicted, "notes/random-moved.bin": "randomLinux" })), trees.conflicted)).toBe(false);
    const { "notes/random-moved.bin": moved, ...rest } = conflicted;
    expect(matches(listing(folder({ ...rest, "random-moved.bin": moved! })), trees.conflicted)).toBe(false);
  });

  it("lists files by path with / separators in byte order, as sha256sum prints them", () => {
    const text = format(listing(folder({ "b.txt": "fromMac", "a/c.txt": "fromLinux" })));
    expect(text.split("\n").map((line) => line.slice(66))).toEqual(["a/c.txt", "b.txt", ""]);
    expect(text).toMatch(/^[0-9a-f]{64} {2}a\/c\.txt\n/);
  });
});

describe("the files a rewind removed", () => {
  it("names files the restored seq lacks by path and content, not files it moved or holds at their place", () => {
    const before = new Map([
      ["after-point-mac.bin", "09bb"],
      ["docs/after-point-linux.txt", "5941"],
      ["docs/from-mac-renamed.txt", "0958"],
      ["notes/random-from-linux.bin", "0f21"],
      ["notes/random-moved.bin", "00cb"],
      ["shared (conflict from linux 2026-11-17).txt", "c5b2"],
      ["shared.txt", "7a5a"],
    ]);
    const rewound = new Map([
      ["docs/from-mac-renamed.txt", "0958"],
      ["notes/from-linux.txt", "f3cf"],
      ["notes/random-from-linux.bin", "0f21"],
      ["random-from-mac.bin", "00cb"],
      ["shared.txt", "af41"],
    ]);
    expect(removedByRewind(before, rewound)).toEqual(["after-point-mac.bin", "docs/after-point-linux.txt", "shared (conflict from linux 2026-11-17).txt"]);
  });

  it("reads back a listing it formatted", () => {
    const files = new Map([["a b/c (1).txt", "f".repeat(64)], ["d.txt", "0".repeat(64)]]);
    expect(parseListing(format(files))).toEqual(files);
  });
});

describe("the space after a rewind against the log before it", () => {
  const node = (nodeId: string, parentId: string, name: string, kind: NodeKind, versionId = "") =>
    create(NodeStateSchema, { nodeId, parentId, name, kind, status: NodeStatus.LIVE, current: versionId ? { versionId } : undefined });
  const commit = (seq: bigint, ...nodes: NodeState[]) =>
    create(CommitRecordSchema, { seq, commitId: "c" + seq, changes: nodes.map((value) => ({ change: { case: "node" as const, value } })) });
  const purge = (seq: bigint, nodeId: string) =>
    create(CommitRecordSchema, { seq, commitId: "c" + seq, changes: [{ change: { case: "purged", value: { nodeId } } }] });
  const root = node("root", "", "", NodeKind.DIR);
  const log = [
    commit(1n, node("d", "root", "docs", NodeKind.DIR), node("a", "d", "a.txt", NodeKind.FILE, "a1")),
    commit(2n, node("a", "d", "a.txt", NodeKind.FILE, "a2")),
    commit(3n, node("b", "root", "b.txt", NodeKind.FILE, "b1")),
    commit(4n, { ...node("b", "root", "b.txt", NodeKind.FILE, "b1"), status: NodeStatus.TRASHED }),
    purge(5n, "b"),
  ];
  const restored = (seq: bigint) => new Map([["root", root], ...replay(log, seq)]);

  it("is the state of the log at the restored seq, at any seq", () => {
    for (const seq of [1n, 2n, 3n, 4n, 5n]) expect(rewindMismatch(log, log.slice(0, Number(seq)), restored(seq), seq)).toBeUndefined();
    expect(replay(log, 4n).get("b")?.status).toBe(NodeStatus.TRASHED);
    expect(replay(log, 5n).has("b")).toBe(false);
  });

  it("is not when an edited file comes back with its earlier version or goes missing", () => {
    const older = restored(2n).set("a", node("a", "d", "a.txt", NodeKind.FILE, "a1"));
    expect(rewindMismatch(log, log.slice(0, 2), older, 2n)).toMatch(/not the tree the log before it holds at seq 2/);
    const missing = restored(2n);
    missing.delete("a");
    expect(rewindMismatch(log, log.slice(0, 2), missing, 2n)).toMatch(/not the tree the log before it holds at seq 2/);
  });

  it("is not when the server holds the state one seq away from the seq it reports", () => {
    expect(rewindMismatch(log, log.slice(0, 2), restored(2n), 3n)).toMatch(/restore_seq 3, but the log after it ends at seq 2/);
    const other = [log[0]!, { ...log[1]!, commitId: "lost" }];
    expect(rewindMismatch(log, other, restored(2n), 2n)).toMatch(/log after the rewind is \[1 c1, 2 lost\], but the log before it up to seq 2 is \[1 c1, 2 c2\]/);
    expect(rewindMismatch(log, log.slice(0, 3), restored(2n), 3n)).toMatch(/not the tree the log before it holds at seq 3/);
    expect(rewindMismatch(log, log.slice(0, 2), restored(3n), 2n)).toMatch(/not the tree the log before it holds at seq 2/);
    expect(rewindMismatch(log.slice(0, 2), log.slice(0, 2), restored(2n), 3n)).toMatch(/restore_seq 3, but the log before it ends at seq 2/);
    expect(rewindMismatch(log, [], restored(0n), 1n)).toMatch(/restore_seq 1, but the log after it ends at seq 0/);
  });

  it("is not when a file the log trashed is still live on the server", () => {
    const live = restored(4n).set("b", node("b", "root", "b.txt", NodeKind.FILE, "b1"));
    expect(rewindMismatch(log, log.slice(0, 4), live, 4n)).toMatch(/not the tree the log before it holds at seq 4/);
  });
});

describe("the sealed pairing code", () => {
  it("opens with the private key that belongs to the public key it was sealed to, and with no other", () => {
    const keys = keypair();
    const sealed = seal(keys.publicKey, "K7QM-2XPD@koidrive-test-1-1.example.workers.dev");
    expect(sealed.toString()).not.toContain("K7QM");
    expect(unseal(keys.privateKey, sealed)).toBe("K7QM-2XPD@koidrive-test-1-1.example.workers.dev");
    expect(() => unseal(keypair().privateKey, sealed)).toThrow();
  });
});

describe("starting a device's daemon", () => {
  it("refuses when another daemon already answers for the profile, without starting one", async () => {
    const dir = mkdtempSync(join(tmpdir(), "koidrive-e2e-unit-"));
    roots.push(dir);
    const koi = join(dir, "koi");
    writeFileSync(
      koi,
      '#!/bin/sh\ncase "$1" in\n  status) printf \'{"data":{"running":true,"folders":[]}}\\n\' ;;\n  _daemon) touch "$(dirname "$0")/started"; echo "another Koidrive daemon or command is using this profile" >&2; exit 1 ;;\nesac\n',
    );
    chmodSync(koi, 0o755);
    const previous = process.env.KOI;
    process.env.KOI = koi;
    try {
      const device = { name: "mac", dir, folder: join(dir, "folder"), env: { ...process.env, XDG_RUNTIME_DIR: join(dir, "run") } };
      await expect(startDaemon(device)).rejects.toThrow("a daemon already answers for mac's profile");
      expect(existsSync(join(dir, "started"))).toBe(false);
    } finally {
      if (previous === undefined) delete process.env.KOI;
      else process.env.KOI = previous;
    }
  });
});
