// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { isIP } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { stripVTControlCharacters } from "node:util";
import { releaseVersion } from "./release-version.mjs";

const MIGRATION_TAGS = ["v1"];
const DURABLE_OBJECTS = { ACCOUNT: "AccountDO", SPACE: "SpaceDO" };
const INSTALL_MS = 5 * 60_000;
const DRY_RUN_MS = 2 * 60_000;
const STARTUP_MS = 60_000;
const REQUEST_MS = 5_000;
const WHOLE_CHECK_MS = 9 * 60_000;

const server = join(dirname(fileURLToPath(import.meta.url)), "..");
const version = releaseVersion(server);
const workspace = mkdtempSync(join(tmpdir(), "koidrive-deploy-"));
const root = join(workspace, "server");
const env = { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "true" };
let dev;
let devClosed;
let devError;
let devOutput = "";

class CheckFailed extends Error {}

function fail(message) {
  throw new CheckFailed(message);
}

function sanitized(text) {
  return text.replace(/\b[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}\b/g, "<claim code>").replace(/[A-Za-z0-9_\-.]{40,}/g, "<redacted>");
}

function run(command, args, timeout) {
  console.log("$ " + [command, ...args].join(" "));
  try {
    execFileSync(command, args, { cwd: root, env, stdio: "inherit", timeout, killSignal: "SIGKILL" });
  } catch (error) {
    const reason = error.code === "ETIMEDOUT" || error.signal === "SIGKILL" ? "was killed after " + timeout / 1000 + " s" : "failed with " + (error.status ?? error.message);
    fail(command + " " + args.join(" ") + " " + reason);
  }
}

function request(path, init = {}) {
  return fetch(path, { ...init, signal: AbortSignal.timeout(REQUEST_MS) });
}

function copyTrackedFiles() {
  const files = execFileSync("git", ["ls-files", "-z"], { cwd: server, encoding: "utf8", timeout: 30_000 }).split("\0").filter(Boolean);
  for (const file of files) {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    cpSync(join(server, file), join(root, file));
  }
  console.log("Copied " + files.length + " tracked files of server/ to " + root);
}

async function checkConfig() {
  for (const file of [".dev.vars.example", ".env.example"]) {
    if (existsSync(join(root, file))) fail(file + " exists, so the deploy button would ask owners for secrets; Koidrive needs none");
  }
  const require = createRequire(join(root, "package.json"));
  const pkg = require("./package.json");
  if (pkg.scripts?.deploy !== "wrangler deploy") fail("package.json needs the script \"deploy\": \"wrangler deploy\" for the deploy button");
  const { unstable_readConfig } = await import(pathToFileURL(require.resolve("wrangler")).href);
  const config = unstable_readConfig({ config: join(root, "wrangler.jsonc") });
  if (!config.observability?.enabled) fail("observability.enabled must be true, or the claim code is lost unless someone watched live logs");
  if (config.observability.redact_query_string !== true) {
    fail("observability.redact_query_string must be true, or persisted invocation logs keep the chunk tickets in relay URLs");
  }
  const tags = config.migrations.map((migration) => migration.tag);
  if (MIGRATION_TAGS.some((tag, i) => tags[i] !== tag)) {
    fail("Durable Object migration tags must start with " + JSON.stringify(MIGRATION_TAGS) + " and only grow, but are " + JSON.stringify(tags));
  }
  const sqliteClasses = config.migrations.flatMap((migration) => migration.new_sqlite_classes ?? []);
  for (const [name, className] of Object.entries(DURABLE_OBJECTS)) {
    const binding = config.durable_objects.bindings.find((entry) => entry.name === name);
    if (binding?.class_name !== className) fail("binding " + name + " must name the Durable Object class " + className);
    if (!sqliteClasses.includes(className)) fail("no migration creates " + className + " as a SQLite class");
  }
  const bucket = config.r2_buckets.find((entry) => entry.binding === "BUCKET");
  if (!bucket?.bucket_name) fail("the R2 binding BUCKET needs a default bucket_name for the deploy button to provision");
  for (const name of [...Object.keys(DURABLE_OBJECTS), "BUCKET"]) {
    if (!pkg.cloudflare?.bindings?.[name]?.description) fail("package.json cloudflare.bindings." + name + ".description is missing");
  }
  console.log("Wrangler configuration: migration tags " + JSON.stringify(tags) + ", bindings ACCOUNT, SPACE and BUCKET, logs persisted with query strings redacted");
}

async function waitFor(check, what, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (devError) fail("could not start wrangler dev: " + devError.message);
    if (dev && (dev.exitCode !== null || dev.signalCode !== null)) fail("wrangler dev exited with " + (dev.exitCode ?? dev.signalCode) + " while waiting for " + what);
    const value = await check().catch(() => undefined);
    if (value) return value;
    if (Date.now() > deadline) fail("timed out after " + timeoutMs / 1000 + " s waiting for " + what);
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
}

async function stopDev() {
  if (!dev) return;
  if (dev.pid) {
    try {
      process.kill(-dev.pid, "SIGKILL");
    } catch {
      dev.kill("SIGKILL");
    }
  }
  await devClosed;
}

function readyUrl() {
  for (const line of stripVTControlCharacters(devOutput).split(/(?<=\n)/)) {
    if (!line.endsWith("\n")) continue;
    const ready = /^\s*(?:\[wrangler:info\]\s*)?Ready on (\S+)\s*$/.exec(line);
    if (!ready) {
      if (/^\s*(?:\[wrangler:info\]\s*)?Ready on\b/.test(line)) fail("malformed Wrangler ready URL");
      continue;
    }
    const address = /^http:\/\/(\[[0-9a-fA-F:]+\]|[A-Za-z0-9.]+):([0-9]{1,5})$/.exec(ready[1]);
    if (!address || Number(address[2]) < 1 || Number(address[2]) > 65535) fail("malformed Wrangler ready URL");
    const host = address[1].replace(/^\[|\]$/g, "");
    if (host.toLowerCase() !== "localhost" && !(isIP(host) === 4 && host.startsWith("127.")) && !(isIP(host) === 6 && new URL(ready[1]).hostname === "[::1]")) {
      fail("Wrangler ready URL must use a loopback host");
    }
    return ready[1];
  }
}

async function claimLocally() {
  dev = spawn(join(root, "node_modules", ".bin", "wrangler"), ["dev", "--ip", "127.0.0.1", "--port", "0", "--inspector-port", "0", "--persist-to", join(root, ".state")], {
    cwd: root,
    env,
    detached: true,
  });
  devClosed = new Promise((resolve) => dev.once("close", resolve));
  dev.stdout.on("data", (chunk) => (devOutput += chunk));
  dev.stderr.on("data", (chunk) => (devOutput += chunk));
  dev.on("error", (error) => { devError = error; });
  const base = await waitFor(() => Promise.resolve(readyUrl()), "wrangler dev readiness", STARTUP_MS);
  const json = { "Content-Type": "application/json", Accept: "application/json" };
  const before = await waitFor(
    () => request(base + "/v1/server", { headers: json }).then((response) => (response.ok ? response.json() : undefined)),
    "wrangler dev to serve /v1/server",
    STARTUP_MS,
  );
  if (before.claimed) fail("a fresh deployment reports that it already has an owner");
  if (before.serverVersion !== version) fail("the standalone Worker reports " + before.serverVersion + " instead of " + version);
  const setup = await request(base + "/setup");
  const page = await setup.text();
  if (!setup.ok || !page.includes("claim code")) fail("/setup did not issue a claim code (HTTP " + setup.status + "): " + page.slice(0, 200));
  const viewer = await request(base + "/s/AAAAAAAAAAAAAAAAAAAAAA");
  if (!viewer.ok || !(await viewer.text()).includes('src="/viewer/viewer.js"')) fail("/s/{link_id} does not serve the built share viewer (HTTP " + viewer.status + ")");
  const script = await request(base + "/viewer/viewer.js");
  if (!script.ok || !script.headers.get("content-security-policy")?.includes("default-src 'none'")) {
    fail("/viewer/viewer.js is not served from the built assets with the viewer's headers (HTTP " + script.status + ")");
  }
  const code = await waitFor(async () => /Koidrive claim code[^:]*: ([A-Z0-9-]+)/.exec(devOutput)?.[1], "the claim code in the Worker log", 10_000);
  const response = await request(base + "/v1/claim", {
    method: "POST",
    headers: json,
    body: JSON.stringify({ code, userName: "Owner", deviceName: "check-deploy", platform: "linux" }),
  });
  if (!response.ok) {
    const error = await response.json().catch(() => ({}));
    fail("claiming the local deployment failed with HTTP " + response.status + ": " + (error.message ?? "no error message"));
  }
  const after = await request(base + "/v1/server", { headers: json }).then((r) => r.json());
  if (!after.claimed) fail("the deployment does not report an owner after the claim");
  console.log("Served the bundle with wrangler dev, read the claim code from its log and claimed it (server " + after.serverVersion + ")");
}

async function cleanUp() {
  await stopDev();
  rmSync(workspace, { recursive: true, force: true });
}

const watchdog = setTimeout(async () => {
  console.error("::error::check-deploy: the whole check took longer than " + WHOLE_CHECK_MS / 60_000 + " minutes");
  await cleanUp();
  process.exit(1);
}, WHOLE_CHECK_MS);
watchdog.unref();

try {
  copyTrackedFiles();
  run("pnpm", ["install", "--frozen-lockfile"], INSTALL_MS);
  await checkConfig();
  writeFileSync(join(workspace, "Cargo.toml"), '[workspace.package]\nversion = ' + JSON.stringify(version) + '\n');
  writeFileSync(join(root, "release.json"), JSON.stringify({ version: "0.0.0-stale" }) + '\n');
  run("pnpm", ["run", "deploy", "--dry-run", "--outdir", join(root, ".dist")], DRY_RUN_MS);
  const exported = JSON.parse(readFileSync(join(root, "release.json"), "utf8")).version;
  if (exported !== version) fail("direct Wrangler deploy kept stale release version " + exported + " instead of " + version);
  const worker = readFileSync(join(root, ".dist", "worker.js"), "utf8");
  if (!worker.includes(JSON.stringify(version))) fail("direct Wrangler deploy did not embed the workspace release version");
  console.log("Direct Wrangler deploy refreshed the stale export and embedded workspace version " + version);
  rmSync(join(workspace, "Cargo.toml"));
  if (!existsSync(join(root, "web", "dist", "viewer", "index.html"))) fail("the dry run did not build the web package: web/dist/viewer/index.html is missing");
  console.log("The dry run built the web package through wrangler.jsonc's build.command");
  await claimLocally();
  console.log("check-deploy: server/ deploys on its own");
} catch (error) {
  if (devOutput) console.error("wrangler dev output (claim codes and long tokens redacted):\n" + sanitized(devOutput));
  console.error("::error::check-deploy: " + (error instanceof CheckFailed ? error.message : error?.stack ?? String(error)));
  process.exitCode = 1;
} finally {
  clearTimeout(watchdog);
  await cleanUp();
}
