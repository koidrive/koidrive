// SPDX-License-Identifier: AGPL-3.0-or-later

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readlinkSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import "./check-release-version.mjs";

const packer = join(dirname(fileURLToPath(import.meta.url)), "pack-bundle.mjs");
const DRY_RUN = "$ wrangler deploy --dry-run";
const assets = join(dirname(packer), "..", "web", "dist");

test("the packed Worker and manifest use the CLI release version", () => {
  const dir = mkdtempSync(join(tmpdir(), "pack-bundle-version-"));
  try {
    const cargo = readFileSync(join(dirname(packer), "../../Cargo.toml"), "utf8");
    const version = cargo.split("[workspace.package]")[1].match(/^version = "([^"]+)"/m)[1];
    const output = join(dir, "out");
    const run = spawnSync(process.execPath, [packer, output], { encoding: "utf8" });
    assert.equal(run.status, 0, run.stderr);
    const manifest = JSON.parse(readFileSync(join(output, "manifest.json"), "utf8"));
    assert.equal(manifest.version, version);
    for (const kind of ["account", "space"]) assert.ok(Number.isInteger(manifest.storage[kind]) && manifest.storage[kind] > 0, kind);
    const worker = readFileSync(join(output, "worker", manifest.main_module), "utf8");
    assert.ok(worker.includes(JSON.stringify(version)), "the Worker does not contain the release version");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function packWhile(during) {
  const dir = mkdtempSync(join(tmpdir(), "pack-bundle-test-"));
  const output = join(dir, "out");
  mkdirSync(output);
  const child = spawn(process.execPath, [packer, output], { stdio: ["ignore", "pipe", "inherit"] });
  let started = false;
  child.stdout.on("data", (chunk) => {
    if (started || !chunk.toString().includes(DRY_RUN)) return;
    started = true;
    during(dir, output);
  });
  const code = await new Promise((done, failed) => child.on("error", failed).on("exit", done));
  return { dir, output, started, code };
}

test("a file created in the output during the dry run survives", async () => {
  const { dir, output, started, code } = await packWhile((_, output) => writeFileSync(join(output, "manifest.json"), "unrelated data"));
  try {
    assert.ok(started, "the packer never started its dry run");
    assert.notEqual(code, 0);
    assert.equal(readFileSync(join(output, "manifest.json"), "utf8"), "unrelated data");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a symlink created in the output during the dry run is not followed", async () => {
  const { dir, output, started, code } = await packWhile((dir, output) => {
    writeFileSync(join(dir, "target"), "unrelated data");
    symlinkSync(join(dir, "target"), join(output, "manifest.json"));
  });
  try {
    assert.ok(started, "the packer never started its dry run");
    assert.notEqual(code, 0);
    assert.equal(readlinkSync(join(output, "manifest.json")), join(dir, "target"));
    assert.equal(readFileSync(join(dir, "target"), "utf8"), "unrelated data");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an output inside the assets directory is refused before the dry run, also through a symlink", () => {
  mkdirSync(assets, { recursive: true });
  const dir = mkdtempSync(join(tmpdir(), "pack-bundle-test-"));
  try {
    symlinkSync(assets, join(dir, "link"));
    for (const [output, created] of [
      [join(assets, "bundle"), join(assets, "bundle")],
      [join(dir, "link", "bundle"), join(assets, "bundle")],
      [join(assets, "..bundle"), join(assets, "..bundle")],
    ]) {
      const run = spawnSync(process.execPath, [packer, output], { encoding: "utf8" });
      assert.notEqual(run.status, 0, output);
      assert.ok(!run.stdout.includes(DRY_RUN), output);
      assert.ok(!existsSync(created), output);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
