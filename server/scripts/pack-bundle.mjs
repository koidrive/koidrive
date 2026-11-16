// SPDX-License-Identifier: AGPL-3.0-or-later

import { execFileSync } from "node:child_process";
import {
  constants,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createRequire, registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { basename, dirname, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { releaseVersion } from "./release-version.mjs";

const DRY_RUN_MS = 2 * 60_000;

const server = join(dirname(fileURLToPath(import.meta.url)), "..");
const output = process.argv[2] && resolve(process.argv[2]);
if (!output || process.argv.length !== 3) {
  console.error("usage: node scripts/pack-bundle.mjs <output directory>");
  process.exit(2);
}

function fail(message) {
  console.error("pack-bundle: " + message);
  process.exit(1);
}

function copyNew(from, to) {
  if (!statSync(from).isDirectory()) return copyFileSync(from, to, constants.COPYFILE_EXCL);
  mkdirSync(to);
  for (const entry of readdirSync(from)) copyNew(join(from, entry), join(to, entry));
}

function realPath(path) {
  try {
    return realpathSync(path);
  } catch {
    return join(realPath(dirname(path)), basename(path));
  }
}

/** Drops undefined fields, which Wrangler's normalized config keeps for options that are not set. */
function defined(object) {
  return Object.fromEntries(Object.entries(object).filter(([, value]) => value !== undefined));
}

if (existsSync(output) && (!statSync(output).isDirectory() || readdirSync(output).length > 0)) {
  fail(output + " exists and is not an empty directory; name a new or empty one");
}

const require = createRequire(join(server, "package.json"));
const { unstable_readConfig } = await import(pathToFileURL(require.resolve("wrangler")).href);
const config = unstable_readConfig({ config: join(server, "wrangler.jsonc") });
const version = releaseVersion(server);
registerHooks({
  resolve: (specifier, context, next) => next(specifier.startsWith(".") && !extname(specifier) ? specifier + ".ts" : specifier, context),
});
const { STORAGE: storage } = await import(pathToFileURL(join(server, "src", "storage.ts")).href);
if (config.r2_buckets.length !== 1) fail("wrangler.jsonc must bind exactly one R2 bucket, found " + config.r2_buckets.length);
if (!config.assets?.directory || !config.assets.binding) fail("wrangler.jsonc must set assets.directory and assets.binding");
const fromAssets = relative(realPath(resolve(server, config.assets.directory)), realPath(output));
if (fromAssets === "" || (fromAssets.split(sep)[0] !== ".." && !isAbsolute(fromAssets))) {
  fail(output + " is inside " + config.assets.directory + ", which the packer copies into it; name a directory outside it");
}

const built = mkdtempSync(join(tmpdir(), "koidrive-bundle-"));
try {
  console.log("$ wrangler deploy --dry-run --outdir " + built);
  execFileSync("pnpm", ["exec", "wrangler", "deploy", "--dry-run", "--outdir", built], {
    cwd: server,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false" },
    stdio: "inherit",
    timeout: DRY_RUN_MS,
    killSignal: "SIGKILL",
  });
  const mainModule = basename(config.main, extname(config.main)) + ".js";
  if (!existsSync(join(built, mainModule))) fail("the dry run wrote no " + mainModule + "; it wrote " + readdirSync(built).join(", "));
  const assets = resolve(server, config.assets.directory);
  if (!existsSync(assets) || readdirSync(assets).length === 0) fail(config.assets.directory + " is empty after the build");

  const r2 = config.r2_buckets[0];
  const manifest = defined({
    version,
    storage,
    main_module: mainModule,
    compatibility_date: config.compatibility_date,
    durable_objects: config.durable_objects.bindings.map(({ name, class_name }) => ({ name, class_name })),
    r2_binding: r2.binding,
    default_bucket: r2.bucket_name,
    migrations: config.migrations.map(defined),
    assets: { binding: config.assets.binding },
    observability: config.observability,
    code_update_strategy: config.durable_objects.code_update_strategy,
  });
  try {
    mkdirSync(output, { recursive: true });
    mkdirSync(join(output, "worker"));
    copyFileSync(join(built, mainModule), join(output, "worker", mainModule), constants.COPYFILE_EXCL);
    copyNew(assets, join(output, "assets"));
    writeFileSync(join(output, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n", { flag: "wx" });
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
    fail(error.path + " appeared in the output during the build; nothing was replaced");
  }
  console.log(JSON.stringify(manifest));
} finally {
  rmSync(built, { recursive: true, force: true });
}
