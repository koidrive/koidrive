// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync, spawn } from "node:child_process";
import { constants, createHash, generateKeyPairSync, privateDecrypt, publicEncrypt } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, openSync, readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { uuidv7 } from "../../src/ids.ts";
import { addStats, readState } from "./state.ts";

const SPACE = "e2e";
const SYNC_MS = 5 * 60_000;
const PEER_MS = 45 * 60_000;
const DECISION_MS = 10 * 60_000;
const REWIND_MS = 5 * 60_000;
const RESTORE_POINT_AGE_MS = 70_000;
const RESTORE_POINT_GAP_MS = 10_000;
const API_POLL_MS = 10_000;
const PEER_CHECK_MS = 30_000;

class Failed extends Error {}

function fail(message: string): never {
  throw new Failed(message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function sha256(bytes: Uint8Array | string): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function env(name: string): string {
  return process.env[name] || fail(name + " is not set");
}

function temp(...path: string[]): string {
  return join(process.env.RUNNER_TEMP || tmpdir(), "koidrive-e2e", ...path);
}

function generated(label: string, size: number): Buffer {
  const out = Buffer.alloc(size);
  for (let block = 0, filled = 0; filled < size; block++) filled += createHash("sha256").update(label + ":" + block).digest().copy(out, filled);
  return out;
}

export const contents = {
  fromMac: Buffer.from("written on macOS\n"),
  fromMacEdited: Buffer.from("written on macOS\na second line from macOS\n"),
  randomMac: generated("random-from-mac", 3_000_000),
  shared: Buffer.from("the shared note, first version\n"),
  fromLinux: Buffer.from("written on Linux\n"),
  fromLinuxEdited: Buffer.from("rewritten on Linux\n"),
  randomLinux: generated("random-from-linux", 2_000_000),
  sharedMac: Buffer.from("shared note, edited on macOS\n"),
  sharedLinux: Buffer.from("shared note, edited on Linux while offline\n"),
  afterMac: generated("after-point-mac", 2_500_000),
  afterLinux: Buffer.from("written on Linux after the restore point\n"),
};
export type Content = keyof typeof contents;
const hashes = Object.fromEntries(Object.entries(contents).map(([name, bytes]) => [name, sha256(bytes)])) as Record<Content, string>;

export type Entry = [path: string | RegExp, content: Content];

const SHARED = /^shared[^/]*\.txt$/;
const kept: Entry[] = [
  ["docs/from-mac-renamed.txt", "fromMacEdited"],
  ["notes/random-moved.bin", "randomMac"],
  ["notes/random-from-linux.bin", "randomLinux"],
];
const conflicted: Entry[] = [...kept, [SHARED, "sharedMac"], [SHARED, "sharedLinux"]];

export const trees = {
  created: [
    ["from-mac.txt", "fromMac"],
    ["random-from-mac.bin", "randomMac"],
    ["shared.txt", "shared"],
    ["notes/from-linux.txt", "fromLinux"],
    ["notes/random-from-linux.bin", "randomLinux"],
  ],
  edited: [
    ["from-mac.txt", "fromMacEdited"],
    ["random-from-mac.bin", "randomMac"],
    ["shared.txt", "shared"],
    ["notes/from-linux.txt", "fromLinuxEdited"],
    ["notes/random-from-linux.bin", "randomLinux"],
  ],
  moved: [...kept, ["shared.txt", "shared"], ["notes/from-linux.txt", "fromLinuxEdited"]],
  deleted: [...kept, ["shared.txt", "shared"]],
  conflicted,
  afterPoint: [...conflicted, ["after-point-mac.bin", "afterMac"], ["docs/after-point-linux.txt", "afterLinux"]],
} satisfies Record<string, Entry[]>;
export type Tree = keyof typeof trees;

export function listing(root: string): Map<string, string> {
  const files: [string, string][] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (dir === root && entry.name === ".koidrive") continue;
      const path = join(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (entry.isFile()) files.push([relative(root, path).split(sep).join("/"), sha256(readFileSync(path))]);
    }
  };
  walk(root);
  return new Map(files.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}

export function matches(files: Map<string, string>, entries: Entry[]): boolean {
  if (files.size !== entries.length) return false;
  const left = [...entries];
  for (const [path, hash] of files) {
    const at = left.findIndex(([want, content]) => hashes[content] === hash && (typeof want === "string" ? want === path : want.test(path)));
    if (at < 0) return false;
    left.splice(at, 1);
  }
  return true;
}

export function format(files: Map<string, string>): string {
  return [...files].map(([path, hash]) => hash + "  " + path + "\n").join("");
}

export function keypair(): { publicKey: string; privateKey: string } {
  return generateKeyPairSync("rsa", {
    modulusLength: 3072,
    publicKeyEncoding: { type: "spki", format: "pem" },
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
  });
}

export function seal(publicKey: string, text: string): Buffer {
  return publicEncrypt({ key: publicKey, padding: constants.RSA_PKCS1_OAEP_PADDING, oaepHash: "sha256" }, Buffer.from(text));
}

export function unseal(privateKey: string, sealed: Buffer): string {
  return privateDecrypt({ key: privateKey, padding: constants.RSA_PKCS1_OAEP_PADDING, oaepHash: "sha256" }, sealed).toString();
}

interface Job {
  name: string;
  status: string;
  conclusion: string | null;
  steps?: { name: string; status: string; conclusion: string | null }[];
}

async function github(path: string, init: RequestInit = {}): Promise<Response> {
  const response = await fetch("https://api.github.com/repos/" + env("GITHUB_REPOSITORY") + path, {
    ...init,
    headers: { Authorization: "Bearer " + env("GH_TOKEN"), Accept: "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28" },
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok && response.status !== 302) fail("GET " + path.split("?")[0] + " returned HTTP " + response.status);
  return response;
}

async function peer(): Promise<Job | undefined> {
  const response = await github("/actions/runs/" + env("GITHUB_RUN_ID") + "/attempts/" + env("GITHUB_RUN_ATTEMPT") + "/jobs?per_page=100");
  return ((await response.json()) as { jobs: Job[] }).jobs.find((job) => job.name === env("E2E_PEER_JOB"));
}

let peerCheckedAt = 0;

async function checkPeer(): Promise<void> {
  if (!process.env.GITHUB_RUN_ID || Date.now() - peerCheckedAt < PEER_CHECK_MS) return;
  peerCheckedAt = Date.now();
  const job = await peer();
  if (job?.status === "completed" && job.conclusion !== "success") fail("the job " + job.name + " ended (" + job.conclusion + ") while this one still needed it");
}

async function until<T>(what: string, ms: number, check: () => T | undefined | Promise<T | undefined>, everyMs = 1000): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await check();
    if (value !== undefined) return value;
    if (Date.now() > deadline) fail("timed out after " + ms / 1000 + " s waiting for " + what);
    await checkPeer();
    await sleep(everyMs);
  }
}

async function afterPeerStep(step: string): Promise<void> {
  const name = env("E2E_PEER_JOB");
  if (!step) fail("name the step of " + name + " to wait for");
  await until(
    name + " to finish its step \"" + step + "\"",
    PEER_MS,
    async () => {
      const job = await peer();
      const found = job?.steps?.find((candidate) => candidate.name === step);
      if (job?.steps?.length && !found) fail("the job " + name + " has no step named \"" + step + "\"");
      if (found?.status !== "completed") return undefined;
      return found.conclusion === "success" ? true : fail("the step \"" + step + "\" of " + name + " ended " + found.conclusion);
    },
    API_POLL_MS,
  );
  console.log("The job " + name + " finished its step \"" + step + "\"");
}

async function download(name: string, dir: string): Promise<void> {
  const artifact = name + "-" + env("GITHUB_RUN_ATTEMPT");
  const id = await until(
    "the artifact " + artifact,
    PEER_MS,
    async () => {
      const response = await github("/actions/runs/" + env("GITHUB_RUN_ID") + "/artifacts?name=" + encodeURIComponent(artifact));
      return ((await response.json()) as { artifacts: { id: number }[] }).artifacts[0]?.id;
    },
    API_POLL_MS,
  );
  const redirect = await github("/actions/artifacts/" + id + "/zip", { redirect: "manual" });
  const zip = await fetch(redirect.headers.get("location") ?? fail("the artifact " + artifact + " has no download address"), { signal: AbortSignal.timeout(60_000) });
  if (!zip.ok) fail("downloading the artifact " + artifact + " returned HTTP " + zip.status);
  mkdirSync(dir, { recursive: true });
  const file = join(dir, "artifact.zip");
  writeFileSync(file, Buffer.from(await zip.arrayBuffer()));
  execFileSync("unzip", ["-o", "-q", file, "-d", dir]);
  rmSync(file);
  console.log("Downloaded the artifact " + artifact);
}

let ownerRequests = 0;

async function asOwner(method: string, path: string, body?: unknown): Promise<Response> {
  const { url, owner } = readState();
  if (!url || !owner) fail("run.ts deploy and run.ts claim must run first: the state file has no server or owner");
  const send = (path: string, init: RequestInit) => {
    ownerRequests++;
    return fetch(url + path, { ...init, signal: AbortSignal.timeout(60_000) });
  };
  const json = { "Content-Type": "application/json", Accept: "application/json" };
  const refreshed = await send("/v1/token", { method: "POST", headers: json, body: JSON.stringify({ deviceId: owner.deviceId, refreshCredential: owner.refreshCredential }) });
  if (!refreshed.ok) fail("POST /v1/token for the owner returned HTTP " + refreshed.status);
  const { token } = (await refreshed.json()) as { token?: string };
  return send(path, { method, headers: { ...json, Authorization: "Bearer " + (token ?? fail("POST /v1/token returned no token")) }, body: body === undefined ? undefined : JSON.stringify(body) });
}

async function invitation(): Promise<string> {
  const response = await asOwner("POST", "/v1/pair/start");
  if (!response.ok) fail("POST /v1/pair/start returned HTTP " + response.status);
  const { code } = (await response.json()) as { code?: string };
  return (code ?? fail("POST /v1/pair/start returned no code")) + "@" + new URL(readState().url!).host;
}

interface Folder {
  path: string;
  space_id: string;
  state: string;
  history_id: string | null;
  head_seq: number | null;
  questions: number;
  problem: string | null;
}

export interface Device {
  name: string;
  dir: string;
  folder: string;
  env: Record<string, string | undefined>;
}

function device(name: string): Device {
  const dir = temp(name);
  const xdg = { XDG_CONFIG_HOME: join(dir, "config"), XDG_STATE_HOME: join(dir, "state"), XDG_RUNTIME_DIR: join(dir, "run") };
  for (const path of [...Object.values(xdg), join(dir, "folder")]) mkdirSync(path, { recursive: true, mode: 0o700 });
  mkdirSync(join(xdg.XDG_CONFIG_HOME, "koidrive"), { recursive: true, mode: 0o700 });
  writeFileSync(join(xdg.XDG_CONFIG_HOME, "koidrive", "config.toml"), "[service]\nbackground = false\n");
  return { name, dir, folder: join(dir, "folder"), env: { ...process.env, ...xdg } };
}

function koi(on: Device, args: string[], shown: string[] | null = args): string {
  if (shown) console.log("\n$ koi " + shown.join(" ") + "   # " + on.name);
  try {
    const out = execFileSync(env("KOI"), args, { env: on.env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
    if (shown) process.stdout.write(out);
    return out;
  } catch (error) {
    fail("koi " + (shown ?? args).join(" ") + " failed on " + on.name + ": " + String((error as { stderr?: unknown }).stderr ?? error).trim());
  }
}

function join_(on: Device, code: string): void {
  koi(on, ["join", code, "--name", on.name], ["join", "<pairing code>", "--name", on.name]);
}

function folder(on: Device): Folder {
  const status = JSON.parse(koi(on, ["status", "--json"], null)) as { data: { folders: Folder[] } };
  return status.data.folders[0] ?? fail(on.name + " has no linked folder");
}

export async function startDaemon(on: Device): Promise<void> {
  if (answers(on)) fail("a daemon already answers for " + on.name + "'s profile, so the one started here could not take it");
  console.log("\n$ koi _daemon >> daemon.log &   # " + on.name);
  const log = openSync(join(on.dir, "daemon.log"), "a");
  const daemon = spawn(env("KOI"), ["_daemon"], { env: { ...on.env, KOIDRIVE_LOG: "info,koidrive_driver=debug,koidrive_engine=debug" }, detached: true, stdio: ["ignore", log, log] });
  daemon.unref();
  writeFileSync(join(on.dir, "daemon.pid"), String(daemon.pid));
  const runtime = join(on.env.XDG_RUNTIME_DIR!, "koidrive");
  await until(on.name + "'s daemon to answer", 30_000, () => {
    if (daemon.exitCode !== null || daemon.signalCode !== null) fail(on.name + "'s daemon exited with " + (daemon.exitCode ?? daemon.signalCode) + ": " + readFileSync(join(on.dir, "daemon.log"), "utf8").slice(-2000));
    if (!existsSync(runtime) || !readdirSync(runtime).some((name) => /^daemon-[0-9a-f]{8}\.sock$/.test(name))) return undefined;
    return answers(on) ? true : undefined;
  });
}

function answers(on: Device): boolean {
  try {
    return (JSON.parse(koi(on, ["status", "--json"], null)) as { data: { running: boolean } }).data.running;
  } catch {
    return false;
  }
}

async function stopDaemon(on: Device): Promise<void> {
  const file = join(on.dir, "daemon.pid");
  const pid = Number(readFileSync(file, "utf8"));
  console.log("\n$ kill -TERM <daemon>   # " + on.name);
  try {
    process.kill(pid, "SIGTERM");
  } catch (error) {
    fail(on.name + "'s daemon was gone before it was stopped (" + String(error) + "): " + readFileSync(join(on.dir, "daemon.log"), "utf8").slice(-2000));
  }
  await until(on.name + "'s daemon to exit", 30_000, () => {
    try {
      process.kill(pid, 0);
      return undefined;
    } catch {
      return true;
    }
  });
  rmSync(file);
}

function put(on: Device, path: string, content: Content): void {
  console.log("\n$ write " + path + " (" + contents[content].length + " bytes, " + content + ")   # " + on.name);
  mkdirSync(dirname(join(on.folder, path)), { recursive: true });
  writeFileSync(join(on.folder, path), contents[content]);
}

function move(on: Device, from: string, to: string): void {
  console.log("\n$ mv " + from + " " + to + "   # " + on.name);
  mkdirSync(dirname(join(on.folder, to)), { recursive: true });
  renameSync(join(on.folder, from), join(on.folder, to));
}

function remove(on: Device, path: string): void {
  console.log("\n$ rm " + path + "   # " + on.name);
  rmSync(join(on.folder, path));
}

function explain(on: Device, files: Map<string, string>): void {
  console.log("\nThe last listing on " + on.name + ":\n" + format(files));
  for (const args of [["status"], ["resolve", "--json"]]) {
    try {
      koi(on, args);
    } catch (error) {
      console.log(String(error));
    }
  }
  try {
    console.log("\nThe end of " + on.name + "'s daemon log:\n" + readFileSync(join(on.dir, "daemon.log"), "utf8").split("\n").slice(-80).join("\n"));
  } catch {}
}

async function reach(on: Device, ...wanted: Tree[]): Promise<[Tree, Map<string, string>]> {
  let files = new Map<string, string>();
  try {
    return await until("the " + wanted.join(" or ") + " tree on " + on.name, SYNC_MS, () => {
      try {
        files = listing(on.folder);
      } catch {
        return undefined;
      }
      const tree = wanted.find((name) => matches(files, trees[name]));
      return tree && folder(on).state === "up_to_date" ? ([tree, files] as [Tree, Map<string, string>]) : undefined;
    });
  } catch (error) {
    explain(on, files);
    throw error;
  }
}

function heading(): void {
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, "### End-to-end scenarios\n\n| Scenario | Device | Files | Tree hash |\n|---|---|---|---|\n");
}

function passed(scenario: string, on: Device, files: Map<string, string>): void {
  const names = new Map(Object.entries(hashes).map(([name, hash]) => [hash, name]));
  const hash = sha256(format(files));
  console.log("\n" + [...files].map(([path, sha]) => sha + "  " + path + "  (" + names.get(sha) + ")").join("\n"));
  console.log(scenario + ": passed on " + on.name + ", " + files.size + " files, tree " + hash);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, "| " + scenario + " | " + on.name + " | " + files.size + " | " + hash + " |\n");
}

interface Decision {
  kind: string;
  group?: string;
  items: { path: string }[];
  answers: { token: string; argv: string[] }[];
}

const AFTER_POINT = ["after-point-mac.bin", "docs/after-point-linux.txt"];

async function answerRewind(on: Device, token: "keep" | "discard"): Promise<void> {
  let decision: Decision;
  try {
    decision = await until("a decision on " + on.name + " about the files written after the restore point", DECISION_MS, () =>
      (JSON.parse(koi(on, ["resolve", "--json"], null)) as { decisions: Decision[] }).decisions.find(
        (candidate) => candidate.kind === "recovery" && candidate.group === "created_after_restore_point",
      ),
    );
  } catch (error) {
    explain(on, listing(on.folder));
    throw error;
  }
  const held = decision.items.map((item) => item.path);
  if (held.length !== AFTER_POINT.length || !AFTER_POINT.every((path) => held.some((item) => item === path || item.endsWith("/" + path)))) {
    fail("the decision on " + on.name + " holds " + JSON.stringify(held) + ", not the files written after the restore point " + JSON.stringify(AFTER_POINT));
  }
  const answer = decision.answers.find((candidate) => candidate.token === token) ?? fail("the decision on " + on.name + " offers no " + token + " answer");
  koi(on, answer.argv.slice(1));
}

/**
 * Runs rewind-check.ts, which reads the server through the generated protobuf code and so runs under Vitest, not plain
 * Node.
 */
function checkRewind(vars: Record<string, string>): void {
  const server = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
  execFileSync("pnpm", ["exec", "vitest", "run", "--config", "scripts/deploy-test/vitest.config.ts", "rewind-check"], { cwd: server, env: { ...process.env, ...vars, E2E_TEMP: temp() }, stdio: "inherit" });
}

const restorePointFile = () => temp("restore-point");

const phases: Record<string, (step: string) => Promise<void>> = {
  async "mac-keygen"() {
    const { publicKey, privateKey } = keypair();
    mkdirSync(temp("mac-key"), { recursive: true });
    writeFileSync(temp("mac-private.pem"), privateKey, { mode: 0o600 });
    writeFileSync(temp("mac-key", "public.pem"), publicKey);
    console.log("Made the key the Linux job seals the macOS pairing code to; only its public half is uploaded");
  },

  async "linux-setup"() {
    heading();
    const linux = device("linux");
    join_(linux, await invitation());
    koi(linux, ["space", "create", SPACE]);
    koi(linux, ["folder", "link", linux.folder, "--space", SPACE]);
    await startDaemon(linux);
    await download("e2e-macos-key", temp("mac-key"));
    mkdirSync(temp("mac-join"), { recursive: true });
    writeFileSync(temp("mac-join", "join.bin"), seal(readFileSync(temp("mac-key", "public.pem"), "utf8"), await invitation()));
    console.log("Sealed a pairing code for the macOS device to its public key");
  },

  async "mac-setup"() {
    heading();
    const mac = device("mac");
    await download("e2e-macos-join", temp("mac-join"));
    const code = unseal(readFileSync(temp("mac-private.pem"), "utf8"), readFileSync(temp("mac-join", "join.bin")));
    rmSync(temp("mac-join"), { recursive: true });
    join_(mac, code);
    koi(mac, ["folder", "link", mac.folder, "--space", SPACE]);
    await startDaemon(mac);
  },

  async "linux-1"() {
    const linux = device("linux");
    put(linux, "notes/from-linux.txt", "fromLinux");
    put(linux, "notes/random-from-linux.bin", "randomLinux");
    passed("1. Create", linux, (await reach(linux, "created"))[1]);
  },

  async "mac-1"() {
    const mac = device("mac");
    put(mac, "from-mac.txt", "fromMac");
    put(mac, "random-from-mac.bin", "randomMac");
    put(mac, "shared.txt", "shared");
    passed("1. Create", mac, (await reach(mac, "created"))[1]);
  },

  async "linux-2"(step) {
    const linux = device("linux");
    await afterPeerStep(step);
    put(linux, "notes/from-linux.txt", "fromLinuxEdited");
    passed("2. Edit", linux, (await reach(linux, "edited"))[1]);
  },

  async "mac-2"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    put(mac, "from-mac.txt", "fromMacEdited");
    passed("2. Edit", mac, (await reach(mac, "edited"))[1]);
  },

  async "linux-3"(step) {
    const linux = device("linux");
    await afterPeerStep(step);
    move(linux, "from-mac.txt", "docs/from-mac-renamed.txt");
    passed("3. Rename and move", linux, (await reach(linux, "moved"))[1]);
  },

  async "mac-3"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    move(mac, "random-from-mac.bin", "notes/random-moved.bin");
    passed("3. Rename and move", mac, (await reach(mac, "moved"))[1]);
  },

  async "mac-4"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    remove(mac, "notes/from-linux.txt");
    passed("4. Delete", mac, (await reach(mac, "deleted"))[1]);
  },

  async "linux-4"() {
    const linux = device("linux");
    passed("4. Delete", linux, (await reach(linux, "deleted"))[1]);
    if (!koi(linux, ["history", "trash", linux.folder, "--json"]).includes("from-linux.txt")) fail("the trash does not list notes/from-linux.txt");
  },

  async "linux-5-offline"() {
    const linux = device("linux");
    await stopDaemon(linux);
    put(linux, "shared.txt", "sharedLinux");
  },

  async "mac-5"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    const before = folder(mac).head_seq ?? 0;
    put(mac, "shared.txt", "sharedMac");
    await until("the macOS edit of shared.txt to reach the server", SYNC_MS, () => {
      const status = folder(mac);
      return status.state === "up_to_date" && (status.head_seq ?? 0) > before ? true : undefined;
    });
    console.log("The server took the macOS edit of shared.txt while the Linux daemon was stopped");
  },

  async "linux-5-online"(step) {
    const linux = device("linux");
    await afterPeerStep(step);
    await startDaemon(linux);
    passed("5. Conflict", linux, (await reach(linux, "conflicted"))[1]);
  },

  async "mac-5-check"() {
    const mac = device("mac");
    passed("5. Conflict", mac, (await reach(mac, "conflicted"))[1]);
  },

  async "linux-6"() {
    const third = device("third");
    join_(third, await invitation());
    koi(third, ["folder", "link", third.folder, "--space", SPACE]);
    await startDaemon(third);
    passed("6. A third device reads the space", third, (await reach(third, "conflicted"))[1]);
    koi(third, ["history", "versions", join(third.folder, "docs", "from-mac-renamed.txt")]);
    koi(third, ["history", "trash", third.folder]);
    await stopDaemon(third);
  },

  async "linux-7-point"(step) {
    const linux = device("linux");
    await afterPeerStep(step);
    await reach(linux, "conflicted");
    await sleep(RESTORE_POINT_GAP_MS);
    const point = Date.now();
    writeFileSync(restorePointFile(), String(point));
    console.log("Restore point: " + new Date(point).toISOString());
    await sleep(RESTORE_POINT_GAP_MS);
  },

  async "mac-7"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    put(mac, "after-point-mac.bin", "afterMac");
    passed("7. Writes after the restore point", mac, (await reach(mac, "afterPoint"))[1]);
  },

  async "linux-7"(step) {
    const linux = device("linux");
    const point = Number(readFileSync(restorePointFile(), "utf8"));
    put(linux, "docs/after-point-linux.txt", "afterLinux");
    passed("7. Writes after the restore point", linux, (await reach(linux, "afterPoint"))[1]);
    await afterPeerStep(step);
    await sleep(Math.max(0, point + RESTORE_POINT_AGE_MS - Date.now()));
    const { space_id, history_id } = folder(linux);
    checkRewind({ REWIND_CHECK: "before", SPACE_ID: space_id, HISTORY_ID: history_id ?? fail("the folder on linux has no history") });
    const request = { rewindId: uuidv7(), restorePointMs: String(point), expectedHistoryId: history_id };
    const result = await until(
      "the rewind to complete",
      REWIND_MS,
      async () => {
        const response = await asOwner("POST", "/v1/spaces/" + space_id + "/rewind", request);
        if (response.status === 503) return undefined;
        if (!response.ok) fail("POST /v1/spaces/<space>/rewind returned HTTP " + response.status + ": " + (await response.text()).slice(0, 300));
        return (await response.json()) as Record<string, unknown>;
      },
      API_POLL_MS,
    );
    console.log("Rewound the space to " + new Date(point).toISOString() + ": " + JSON.stringify(result));
    checkRewind({ REWIND_CHECK: "after", SPACE_ID: space_id, HISTORY_ID: String(result.historyId), RESTORE_SEQ: String(result.restoreSeq) });
    const expected = readFileSync(temp("rewound-server-listing.txt"), "utf8");
    await answerRewind(linux, "discard");
    let files = new Map<string, string>();
    try {
      await until("the folder on linux to follow the rewound space", SYNC_MS, () => {
        files = listing(linux.folder);
        const status = folder(linux);
        return status.state === "up_to_date" && status.questions === 0 && status.history_id === String(result.historyId) && format(files) === expected ? true : undefined;
      });
    } catch (error) {
      console.log("The server's tree after the rewind:\n" + expected);
      explain(linux, files);
      throw error;
    }
    passed("7. Discard what the rewind removed", linux, files);
  },

  async "mac-7-keep"(step) {
    const mac = device("mac");
    await afterPeerStep(step);
    await answerRewind(mac, "keep");
    passed("7. Keep what the rewind removed", mac, (await reach(mac, "afterPoint"))[1]);
  },

  async "mac-final"() {
    const mac = device("mac");
    const [, files] = await reach(mac, "afterPoint");
    mkdirSync(temp("mac-listing"), { recursive: true });
    writeFileSync(temp("mac-listing", "listing.txt"), format(files));
    await stopDaemon(mac);
  },

  async "linux-final"(step) {
    const linux = device("linux");
    await afterPeerStep(step);
    const [, files] = await reach(linux, "afterPoint");
    await download("e2e-macos-listing", temp("mac-listing"));
    const mac = readFileSync(temp("mac-listing", "listing.txt"), "utf8");
    if (mac !== format(files)) fail("macOS and Linux ended with different trees.\nmacOS:\n" + mac + "Linux:\n" + format(files));
    passed("Final: macOS and Linux hold the same tree", linux, files);
    await stopDaemon(linux);
  },
};

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [phase = "", step = ""] = process.argv.slice(2);
  try {
    await (phases[phase] ?? fail("usage: e2e.ts " + Object.keys(phases).join(" | ") + " [<step of the other job>]"))(step);
  } catch (error) {
    console.error("::error::e2e " + phase + ": " + (error instanceof Error ? error.message : String(error)));
    process.exitCode = 1;
  } finally {
    if (ownerRequests > 0) addStats({ requests: ownerRequests, bytesUp: 0, bytesDown: 0 });
  }
}
