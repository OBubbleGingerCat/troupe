#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  realpath,
  rename,
  rm,
  rmdir,
  stat,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath } from "node:url";


const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const packagePath = join(projectRoot, "package.json");
const lockPath = join(projectRoot, "package-lock.json");
const cacheManifestName = ".troupe-npm-cache.json";
const registryOrigin = "https://registry.npmjs.org";


class MaintainerError extends Error {
  constructor(message, exitCode = 1) {
    super(message);
    this.exitCode = exitCode;
  }
}


function fail(message, exitCode = 1) {
  throw new MaintainerError(message, exitCode);
}


function parseArguments(argv) {
  const options = {
    npmCache: null,
    allowRegistry: false,
    provisionPackageCache: false,
    checkToolchain: false,
    verifyOfflineCacheReplay: false,
    typecheck: false,
    unit: null,
    component: null,
    browser: null,
    browserCache: null,
    project: null,
    auditFixtures: false,
    buildRaw: false,
    verifyReproducible: false,
    generateAssets: false,
    check: false,
    repeat: 1,
  };

  const value = (name, position) => {
    const candidate = argv[position + 1];
    if (candidate === undefined || candidate.startsWith("--")) {
      fail(`${name} requires a value`, 2);
    }
    return candidate;
  };

  for (let position = 0; position < argv.length; position += 1) {
    const argument = argv[position];
    switch (argument) {
      case "--npm-cache":
        options.npmCache = value(argument, position);
        position += 1;
        break;
      case "--allow-registry":
        options.allowRegistry = true;
        break;
      case "--provision-package-cache":
        options.provisionPackageCache = true;
        break;
      case "--check-toolchain":
        options.checkToolchain = true;
        break;
      case "--verify-offline-cache-replay":
        options.verifyOfflineCacheReplay = true;
        break;
      case "--typecheck":
        options.typecheck = true;
        break;
      case "--unit":
        options.unit = [];
        if (argv[position + 1] !== undefined && !argv[position + 1].startsWith("--")) {
          options.unit = splitTestPaths(value(argument, position));
          position += 1;
        }
        break;
      case "--component":
        options.component = splitTestPaths(value(argument, position));
        position += 1;
        break;
      case "--browser":
        options.browser = splitTestPaths(value(argument, position));
        position += 1;
        break;
      case "--browser-cache":
        options.browserCache = value(argument, position);
        position += 1;
        break;
      case "--project":
        options.project = value(argument, position);
        position += 1;
        break;
      case "--audit-fixtures":
        options.auditFixtures = true;
        break;
      case "--build-raw":
        options.buildRaw = true;
        break;
      case "--verify-reproducible":
        options.verifyReproducible = true;
        break;
      case "--generate-assets":
        options.generateAssets = true;
        break;
      case "--check":
        options.check = true;
        break;
      case "--repeat": {
        const raw = value(argument, position);
        if (!/^[1-9][0-9]*$/.test(raw)) {
          fail("--repeat must be a positive integer", 2);
        }
        options.repeat = Number(raw);
        position += 1;
        break;
      }
      default:
        fail(`unknown argument: ${argument}`, 2);
    }
  }
  return options;
}


function splitTestPaths(value) {
  const paths = value.split(",");
  if (paths.some((item) => item.length === 0) || new Set(paths).size !== paths.length) {
    fail("test path list must be non-empty and unique", 2);
  }
  return paths;
}


function isWithin(path, parent) {
  const remainder = relative(parent, path);
  return remainder === "" || (remainder !== ".." && !remainder.startsWith(`..${sep}`));
}


async function repositoryRoot() {
  let current = projectRoot;
  while (true) {
    try {
      await lstat(join(current, ".git"));
      return current;
    } catch (error) {
      if (error.code !== "ENOENT") {
        throw error;
      }
    }
    const parent = dirname(current);
    if (parent === current) {
      return projectRoot;
    }
    current = parent;
  }
}


async function readJson(path, label) {
  let value;
  try {
    value = JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    fail(`${label} is not readable JSON: ${error.message}`);
  }
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    fail(`${label} must contain an object`);
  }
  return value;
}


function exactFields(value, fields, label) {
  const actual = Object.keys(value).sort();
  const expected = [...fields].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail(`${label} fields are not exact`);
  }
}


function equalJson(left, right) {
  return JSON.stringify(left ?? {}) === JSON.stringify(right ?? {});
}


function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}


function decodedIntegrity(integrity) {
  const match = /^sha512-([A-Za-z0-9+/]+={0,2})$/.exec(integrity);
  if (match === null) {
    fail(`package-lock contains unsupported integrity: ${integrity}`);
  }
  const digest = Buffer.from(match[1], "base64");
  if (digest.length !== 64 || digest.toString("base64") !== match[1]) {
    fail(`package-lock contains malformed integrity: ${integrity}`);
  }
  return digest;
}


function cacheMemberPath(cache, integrity) {
  const hexadecimal = decodedIntegrity(integrity).toString("hex");
  return join(
    cache,
    "_cacache",
    "content-v2",
    "sha512",
    hexadecimal.slice(0, 2),
    hexadecimal.slice(2, 4),
    hexadecimal.slice(4),
  );
}


function collectLockMembers(lock) {
  if (lock.lockfileVersion !== 3 || lock.packages === null || typeof lock.packages !== "object") {
    fail("package-lock must use lockfileVersion 3 with a packages object");
  }
  const members = new Map();
  for (const [path, entry] of Object.entries(lock.packages).sort(([left], [right]) => left.localeCompare(right))) {
    if (path === "" || entry.link === true) {
      continue;
    }
    if (typeof entry.integrity !== "string" || typeof entry.resolved !== "string") {
      fail(`package-lock entry lacks exact resolved/integrity fields: ${path}`);
    }
    decodedIntegrity(entry.integrity);
    let resolved;
    try {
      resolved = new URL(entry.resolved);
    } catch {
      fail(`package-lock entry has an invalid resolved URL: ${path}`);
    }
    if (resolved.origin !== registryOrigin || resolved.username || resolved.password) {
      fail(`package-lock entry uses an undeclared registry: ${path}`);
    }
    const existing = members.get(entry.integrity);
    if (existing !== undefined && existing.resolved !== entry.resolved) {
      fail(`one package integrity maps to multiple registry URLs: ${entry.integrity}`);
    }
    members.set(entry.integrity, { integrity: entry.integrity, resolved: entry.resolved });
  }
  return [...members.values()].sort((left, right) => left.integrity.localeCompare(right.integrity));
}


async function validateToolchainFiles() {
  const packageBytes = await readFile(packagePath);
  const lockBytes = await readFile(lockPath);
  const packageJson = await readJson(packagePath, "package.json");
  const lock = await readJson(lockPath, "package-lock.json");
  const rawMajor = (await readFile(join(projectRoot, ".node-version"), "utf8")).trim();
  if (!/^[1-9][0-9]*$/.test(rawMajor)) {
    fail(".node-version must contain one Node major");
  }
  const nodeMajor = Number(rawMajor);
  const actualNodeMajor = Number(process.versions.node.split(".", 1)[0]);
  if (actualNodeMajor !== nodeMajor) {
    fail(`Node major mismatch: expected ${nodeMajor}, got ${actualNodeMajor}`);
  }
  if (packageJson.packageManager === undefined || !/^npm@[0-9]+\.[0-9]+\.[0-9]+$/.test(packageJson.packageManager)) {
    fail("package.json must pin an exact npm version");
  }
  if (!equalJson(packageJson.engines, { node: `>=${nodeMajor} <${nodeMajor + 1}` })) {
    fail("package.json Node engine does not match .node-version");
  }
  const root = lock.packages?.[""];
  if (
    root === undefined
    || root.name !== packageJson.name
    || root.version !== packageJson.version
    || !equalJson(root.dependencies, packageJson.dependencies)
    || !equalJson(root.devDependencies, packageJson.devDependencies)
    || !equalJson(root.engines, packageJson.engines)
  ) {
    fail("package-lock root does not match package.json");
  }
  if (packageBytes.length === 0 || lockBytes.length === 0) {
    fail("frontend package files must not be empty");
  }
  return {
    packageJson,
    lock,
    lockSha256: sha256(lockBytes),
    nodeMajor,
    nodeVersion: process.versions.node,
    npmVersion: packageJson.packageManager.slice("npm@".length),
    members: collectLockMembers(lock),
  };
}


function registryEnvironment() {
  return Object.entries(process.env).find(
    ([name, value]) => name.toLowerCase() === "npm_config_registry" && value,
  );
}


function validateRegistryAuthority(options) {
  const configured = registryEnvironment();
  if (configured !== undefined && !options.allowRegistry) {
    fail("registry authority requires --allow-registry");
  }
  if (configured !== undefined) {
    let origin;
    try {
      origin = new URL(configured[1]).origin;
    } catch {
      fail("configured npm registry URL is invalid");
    }
    if (origin !== registryOrigin) {
      fail("only the exact npmjs registry is authorized");
    }
  }
  if (Object.entries(process.env).some(
    ([name, value]) => name.toLowerCase() === "npm_config_cache" && value,
  )) {
    fail("implicit npm cache environment is forbidden; use --npm-cache");
  }
}


async function validateExternalDirectory(raw, label, repository, forbiddenHomePath) {
  if (!isAbsolute(raw)) {
    fail(`${label} must be an absolute path`);
  }
  const normalized = resolve(raw);
  let resolved;
  let metadata;
  try {
    resolved = await realpath(normalized);
    metadata = await lstat(normalized);
  } catch (error) {
    fail(`${label} must be an existing directory: ${error.message}`);
  }
  if (resolved !== normalized || metadata.isSymbolicLink() || !metadata.isDirectory()) {
    fail(`${label} must be a real directory without symlink indirection`);
  }
  if (isWithin(resolved, repository)) {
    fail(`${label} must be outside the repository`);
  }
  if (isWithin(resolved, forbiddenHomePath)) {
    fail(`${label} must not use the home npm cache`);
  }
  return resolved;
}


async function validateNpmCache(raw, repository) {
  if (raw === null) {
    fail("--npm-cache is required");
  }
  return validateExternalDirectory(raw, "npm cache", repository, resolve(homedir(), ".npm"));
}


async function validateBrowserCache(raw, repository) {
  if (raw === null) {
    fail("--browser-cache is required for browser mode");
  }
  const cache = await validateExternalDirectory(
    raw,
    "browser cache",
    repository,
    resolve(homedir(), ".cache", "ms-playwright"),
  );
  if (((await stat(cache)).mode & 0o222) !== 0) {
    fail("browser cache must be read-only");
  }
  return cache;
}


function canonicalTestPath(value, label) {
  if (
    value.length === 0
    || isAbsolute(value)
    || value.includes("\\")
    || value.split("/").some((part) => part === "" || part === "." || part === "..")
  ) {
    fail(`${label} path is not repository-relative: ${value}`);
  }
  const absolute = resolve(projectRoot, value);
  if (!isWithin(absolute, projectRoot)) {
    fail(`${label} path escapes the frontend root: ${value}`);
  }
  return absolute;
}


async function requireFile(path, label) {
  try {
    const metadata = await lstat(path);
    if (!metadata.isFile() || metadata.isSymbolicLink()) {
      fail(`${label} is not a regular file: ${path}`);
    }
  } catch (error) {
    if (error instanceof MaintainerError) {
      throw error;
    }
    fail(`${label} is missing: ${path}`);
  }
}


async function validateModePrerequisites(options) {
  if (options.verifyOfflineCacheReplay && !options.allowRegistry) {
    fail("--verify-offline-cache-replay requires --allow-registry");
  }
  if (options.allowRegistry && !options.verifyOfflineCacheReplay && !options.provisionPackageCache) {
    fail("--allow-registry is only valid for cache replay verification or provisioning");
  }
  if (options.provisionPackageCache) {
    if (!options.allowRegistry) {
      fail("--provision-package-cache requires --allow-registry");
    }
    const otherAction = options.checkToolchain
      || options.verifyOfflineCacheReplay
      || options.typecheck
      || options.unit !== null
      || options.component !== null
      || options.browser !== null
      || options.auditFixtures
      || options.buildRaw
      || options.generateAssets;
    if (otherAction) {
      fail("--provision-package-cache must be the only action");
    }
  }
  if (options.verifyReproducible && !options.buildRaw) {
    fail("--verify-reproducible requires --build-raw");
  }
  if ((options.check || options.repeat !== 1) && !options.generateAssets) {
    fail("--check and --repeat require --generate-assets");
  }
  if (options.project !== null && options.browser === null) {
    fail("--project requires --browser");
  }
  if (options.browser !== null && options.browserCache === null) {
    fail("--browser-cache is required for browser mode");
  }
  for (const [paths, label] of [
    [options.unit ?? [], "unit test"],
    [options.component ?? [], "component test"],
    [options.browser ?? [], "browser test"],
  ]) {
    for (const value of paths) {
      await requireFile(canonicalTestPath(value, label), label);
    }
  }
  if (options.buildRaw) {
    await requireFile(join(projectRoot, "scripts", "build.mjs"), "build runner");
  }
  if (options.generateAssets) {
    await requireFile(join(projectRoot, "scripts", "generate_assets.mjs"), "generate runner");
  }
  const action = options.provisionPackageCache
    || options.checkToolchain
    || options.verifyOfflineCacheReplay
    || options.typecheck
    || options.unit !== null
    || options.component !== null
    || options.browser !== null
    || options.buildRaw
    || options.generateAssets;
  if (!action) {
    fail("no maintainer action was selected", 2);
  }
}


function childEnvironment(ownedRoot, cache, offline) {
  const environment = { ...process.env };
  for (const name of Object.keys(environment)) {
    const lower = name.toLowerCase();
    if (lower === "npm_config_cache" || lower === "npm_config_registry") {
      delete environment[name];
    }
  }
  environment.HOME = join(ownedRoot, "home");
  environment.npm_config_cache = cache;
  environment.npm_config_userconfig = join(ownedRoot, "npmrc");
  environment.npm_config_globalconfig = join(ownedRoot, "global-npmrc");
  environment.npm_config_logs_dir = join(ownedRoot, "npm-logs");
  environment.npm_config_audit = "false";
  environment.npm_config_fund = "false";
  environment.npm_config_update_notifier = "false";
  environment.npm_config_logs_max = "0";
  environment.PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
  delete environment.PLAYWRIGHT_BROWSERS_PATH;
  if (offline) {
    environment.npm_config_offline = "true";
    environment.npm_config_registry = "http://127.0.0.1:9/";
    environment.HTTP_PROXY = "http://127.0.0.1:9/";
    environment.HTTPS_PROXY = "http://127.0.0.1:9/";
    environment.ALL_PROXY = "http://127.0.0.1:9/";
    environment.NO_PROXY = "";
  } else {
    delete environment.npm_config_offline;
    environment.npm_config_registry = `${registryOrigin}/`;
  }
  return environment;
}


async function prepareChildEnvironment(ownedRoot, cache, offline) {
  await mkdir(join(ownedRoot, "home"), { recursive: true });
  await mkdir(join(ownedRoot, "npm-logs"), { recursive: true });
  await writeFile(join(ownedRoot, "npmrc"), "", { encoding: "utf8", flag: "wx" });
  await writeFile(join(ownedRoot, "global-npmrc"), "", { encoding: "utf8", flag: "wx" });
  return childEnvironment(ownedRoot, cache, offline);
}


function run(command, commandArguments, environment, cwd, label, capture = false) {
  const completed = spawnSync(command, commandArguments, {
    cwd,
    env: environment,
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (completed.error !== undefined) {
    fail(`${label} could not start: ${completed.error.message}`);
  }
  if (completed.status !== 0) {
    const detail = capture ? (completed.stderr.trim() || completed.stdout.trim()) : "";
    fail(`${label} failed with exit ${completed.status}${detail ? `: ${detail}` : ""}`);
  }
  return capture ? completed.stdout.trim() : "";
}


function verifyNpmVersion(toolchain, environment) {
  const actual = run("npm", ["--version"], environment, projectRoot, "npm version check", true);
  if (actual !== toolchain.npmVersion) {
    fail(`npm version mismatch: expected ${toolchain.npmVersion}, got ${actual}`);
  }
}


async function installDependencies(ownedRoot, cache, offline, environment) {
  const installation = join(ownedRoot, offline ? "offline-install" : "registry-install");
  await mkdir(installation);
  await copyFile(packagePath, join(installation, "package.json"));
  await copyFile(lockPath, join(installation, "package-lock.json"));
  const commandArguments = [
    "ci",
    "--ignore-scripts",
    "--cache",
    cache,
    "--prefix",
    installation,
    "--no-audit",
    "--no-fund",
  ];
  if (offline) {
    commandArguments.push("--offline");
  } else {
    commandArguments.push(`--registry=${registryOrigin}/`);
  }
  run(
    "npm",
    commandArguments,
    environment,
    projectRoot,
    offline ? "offline npm ci" : "registry npm ci",
  );
  return installation;
}


async function verifyTarball(cache, member) {
  const path = cacheMemberPath(cache, member.integrity);
  let metadata;
  try {
    metadata = await lstat(path);
  } catch (error) {
    if (error.code === "ENOENT") {
      return null;
    }
    throw error;
  }
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    fail(`cached tarball is not a regular file: ${member.integrity}`);
  }
  const resolved = await realpath(path);
  if (resolved !== path || !isWithin(resolved, cache)) {
    fail(`cached tarball escapes the npm cache: ${member.integrity}`);
  }
  const bytes = await readFile(path);
  const digest = createHash("sha512").update(bytes).digest();
  if (!digest.equals(decodedIntegrity(member.integrity))) {
    fail(`cached tarball integrity mismatch: ${member.integrity}`);
  }
  return {
    integrity: member.integrity,
    path: relative(cache, path).split(sep).join("/"),
    resolved: member.resolved,
    sha512: digest.toString("hex"),
    size: bytes.length,
  };
}


async function populateMissingTarballs(cache, toolchain, environment) {
  for (const member of toolchain.members) {
    if (await verifyTarball(cache, member) !== null) {
      continue;
    }
    run(
      "npm",
      [
        "cache",
        "add",
        member.resolved,
        "--cache",
        cache,
        `--registry=${registryOrigin}/`,
        "--no-audit",
        "--no-fund",
      ],
      environment,
      projectRoot,
      `npm cache add ${member.resolved}`,
    );
    if (await verifyTarball(cache, member) === null) {
      fail(`npm did not populate the expected cache member: ${member.integrity}`);
    }
  }
}


async function expectedCacheManifest(cache, toolchain) {
  const members = [];
  for (const member of toolchain.members) {
    const verified = await verifyTarball(cache, member);
    if (verified === null) {
      fail(`npm cache is missing locked tarball: ${member.integrity}`);
    }
    members.push(verified);
  }
  return {
    schemaVersion: 1,
    lockSha256: toolchain.lockSha256,
    nodeMajor: toolchain.nodeMajor,
    nodeVersion: toolchain.nodeVersion,
    npmVersion: toolchain.npmVersion,
    members,
  };
}


async function writeCacheManifest(cache, toolchain) {
  const manifest = await expectedCacheManifest(cache, toolchain);
  const temporary = join(cache, `${cacheManifestName}.tmp-${process.pid}`);
  await writeFile(temporary, `${JSON.stringify(manifest, null, 2)}\n`, {
    encoding: "utf8",
    flag: "wx",
  });
  await rename(temporary, join(cache, cacheManifestName));
  return manifest;
}


async function verifyCacheManifest(cache, toolchain, requireReadonly) {
  const manifestPath = join(cache, cacheManifestName);
  const manifest = await readJson(manifestPath, "npm cache identity");
  exactFields(
    manifest,
    ["schemaVersion", "lockSha256", "nodeMajor", "nodeVersion", "npmVersion", "members"],
    "npm cache identity",
  );
  const expected = await expectedCacheManifest(cache, toolchain);
  if (!equalJson(manifest, expected)) {
    fail("npm cache identity does not match the current lock and toolchain");
  }
  if (requireReadonly && ((await stat(cache)).mode & 0o222) !== 0) {
    fail("npm cache must be read-only");
  }
  return manifest;
}


async function makeReadonly(path) {
  const metadata = await lstat(path);
  if (metadata.isSymbolicLink()) {
    fail(`refusing to publish a cache containing a symlink: ${path}`);
  }
  if (metadata.isDirectory()) {
    for (const entry of await readdir(path)) {
      await makeReadonly(join(path, entry));
    }
    await chmod(path, 0o555);
  } else if (metadata.isFile()) {
    await chmod(path, 0o444);
  } else {
    fail(`refusing to publish a cache containing a non-regular member: ${path}`);
  }
}


async function createOwnedRoot(repository) {
  let parent;
  if (process.env.TROUPE_GATE_TMP) {
    parent = await realpath(process.env.TROUPE_GATE_TMP);
    if (isWithin(parent, repository) || !(await stat(parent)).isDirectory()) {
      fail("TROUPE_GATE_TMP must be an external directory");
    }
  } else {
    parent = await realpath(tmpdir());
  }
  return mkdtemp(join(parent, "troupe-diagnostics-frontend-"));
}


async function withTemporaryNodeModules(installation, callback) {
  const link = join(projectRoot, "node_modules");
  const target = join(installation, "node_modules");
  try {
    await lstat(link);
    fail("frontend source node_modules must be absent before maintainer execution");
  } catch (error) {
    if (error instanceof MaintainerError) {
      throw error;
    }
    if (error.code !== "ENOENT") {
      throw error;
    }
  }
  await symlink(target, link, "dir");
  try {
    return await callback(join(target, ".bin"));
  } finally {
    const metadata = await lstat(link);
    if (!metadata.isSymbolicLink() || resolve(projectRoot, await readlink(link)) !== target) {
      fail("temporary node_modules link changed during maintainer execution");
    }
    await unlink(link);
  }
}


async function treeFiles(root, prefix = "") {
  const files = [];
  for (const entry of await readdir(join(root, prefix), { withFileTypes: true })) {
    const path = prefix ? join(prefix, entry.name) : entry.name;
    if (entry.isDirectory()) {
      files.push(...await treeFiles(root, path));
    } else if (entry.isFile()) {
      files.push(path.split(sep).join("/"));
    } else {
      fail(`build output contains a non-regular member: ${path}`);
    }
  }
  return files.sort();
}


async function validateBuildOutput(output) {
  const files = await treeFiles(output);
  const scripts = files.filter((path) => path.endsWith(".js"));
  const styles = files.filter((path) => path.endsWith(".css"));
  if (!files.includes("index.html") || scripts.length !== 1 || styles.length !== 1) {
    fail("build smoke must contain one HTML, one JavaScript, and one CSS file");
  }
  if (files.some((path) => path.endsWith(".map"))) {
    fail("build smoke produced a shipped source map");
  }
  const html = await readFile(join(output, "index.html"), "utf8");
  for (const match of html.matchAll(/(?:src|href)="([^"]+)"/g)) {
    if (match[1].startsWith("/") || /^[a-z]+:/i.test(match[1])) {
      fail("build smoke produced a non-relative asset URL");
    }
  }
}


function binary(binDirectory, name) {
  return join(binDirectory, process.platform === "win32" ? `${name}.cmd` : name);
}


async function buildSmoke(binDirectory, environment, ownedRoot) {
  const output = join(ownedRoot, "build-smoke");
  run(
    binary(binDirectory, "vite"),
    [
      "build",
      "--config",
      join(projectRoot, "vite.config.ts"),
      "--outDir",
      output,
      "--emptyOutDir",
    ],
    environment,
    projectRoot,
    "Vite build smoke",
  );
  await validateBuildOutput(output);
}


function selectedTestPaths(values, fallback) {
  const selected = values.length === 0 ? [fallback] : values;
  return selected.map((path) => canonicalTestPath(path, "test"));
}


async function fingerprintTree(root) {
  const result = [];
  for (const path of await treeFiles(root)) {
    const bytes = await readFile(join(root, path));
    result.push({ path, sha256: sha256(bytes), size: bytes.length });
  }
  return result;
}


async function runRawBuild(environment, ownedRoot, verifyReproducible) {
  const runner = join(projectRoot, "scripts", "build.mjs");
  const first = join(ownedRoot, "raw-build-1");
  run(process.execPath, [runner, "--out-dir", first], environment, projectRoot, "raw build");
  if (!verifyReproducible) {
    return;
  }
  const second = join(ownedRoot, "raw-build-2");
  run(process.execPath, [runner, "--out-dir", second], environment, projectRoot, "raw build replay");
  if (!equalJson(await fingerprintTree(first), await fingerprintTree(second))) {
    fail("raw build replay is not byte-for-byte reproducible");
  }
}


function runAssetGeneration(options, environment) {
  const runner = join(projectRoot, "scripts", "generate_assets.mjs");
  for (let attempt = 1; attempt <= options.repeat; attempt += 1) {
    const child = { ...environment, TROUPE_FRONTEND_GENERATION_ATTEMPT: String(attempt) };
    const commandArguments = [runner];
    if (options.check) {
      commandArguments.push("--check");
    }
    run(process.execPath, commandArguments, child, projectRoot, `asset generation attempt ${attempt}`);
  }
}


async function runSelectedActions(options, installation, environment, ownedRoot, browserCache) {
  await withTemporaryNodeModules(installation, async (binDirectory) => {
    const taskEnvironment = {
      ...environment,
      NODE_PATH: join(installation, "node_modules"),
      PWTEST_CACHE_DIR: join(ownedRoot, "playwright-transform-cache"),
      TROUPE_GATE_TMP: ownedRoot,
    };
    if (options.auditFixtures) {
      taskEnvironment.TROUPE_DIAGNOSTIC_AUDIT_FIXTURES = "1";
    }
    if (options.typecheck || options.verifyOfflineCacheReplay) {
      run(
        binary(binDirectory, "tsc"),
        ["--project", join(projectRoot, "tsconfig.json"), "--noEmit"],
        taskEnvironment,
        projectRoot,
        "strict TypeScript",
      );
    }
    if (options.unit !== null || options.verifyOfflineCacheReplay) {
      const paths = selectedTestPaths(options.unit ?? [], "tests/unit/toolchain.test.ts");
      run(
        binary(binDirectory, "vitest"),
        ["run", "--config", join(projectRoot, "vitest.config.ts"), ...paths],
        taskEnvironment,
        projectRoot,
        "unit tests",
      );
    }
    if (options.component !== null) {
      run(
        binary(binDirectory, "vitest"),
        [
          "run",
          "--config",
          join(projectRoot, "vitest.config.ts"),
          ...selectedTestPaths(options.component, "tests/component"),
        ],
        taskEnvironment,
        projectRoot,
        "component tests",
      );
    }
    if (options.browser !== null) {
      taskEnvironment.PLAYWRIGHT_BROWSERS_PATH = browserCache;
      taskEnvironment.TROUPE_FRONTEND_TEST_OUTPUT = join(ownedRoot, "browser-output");
      const commandArguments = [
        "test",
        "--config",
        join(projectRoot, "playwright.config.ts"),
        ...selectedTestPaths(options.browser, "tests/e2e"),
      ];
      if (options.project !== null) {
        commandArguments.push("--project", options.project);
      }
      run(
        binary(binDirectory, "playwright"),
        commandArguments,
        taskEnvironment,
        projectRoot,
        "browser tests",
      );
    }
    if (options.verifyOfflineCacheReplay) {
      await buildSmoke(binDirectory, taskEnvironment, ownedRoot);
    }
    if (options.buildRaw) {
      await runRawBuild(taskEnvironment, ownedRoot, options.verifyReproducible);
    }
    if (options.generateAssets) {
      runAssetGeneration(options, taskEnvironment);
    }
  });
}


async function populateCache(cache, toolchain, ownedRoot) {
  const environment = await prepareChildEnvironment(ownedRoot, cache, false);
  verifyNpmVersion(toolchain, environment);
  await installDependencies(ownedRoot, cache, false, environment);
  await populateMissingTarballs(cache, toolchain, environment);
  await writeCacheManifest(cache, toolchain);
  await verifyCacheManifest(cache, toolchain, false);
}


async function runOffline(options, cache, toolchain, ownedRoot, requireReadonly, browserCache) {
  await verifyCacheManifest(cache, toolchain, requireReadonly);
  const environment = await prepareChildEnvironment(join(ownedRoot, "offline-environment"), cache, true);
  verifyNpmVersion(toolchain, environment);
  const installation = await installDependencies(ownedRoot, cache, true, environment);
  await runSelectedActions(options, installation, environment, ownedRoot, browserCache);
}


async function provisionCache(options, repository, toolchain, ownedRoot) {
  if (options.npmCache === null || !isAbsolute(options.npmCache)) {
    fail("--npm-cache must be an absolute path");
  }
  const target = resolve(options.npmCache);
  const parent = await realpath(dirname(target));
  if (dirname(target) !== parent || isWithin(target, repository) || isWithin(target, resolve(homedir(), ".npm"))) {
    fail("npm cache publish target must be an external real path");
  }
  try {
    const metadata = await lstat(target);
    if (!metadata.isDirectory() || metadata.isSymbolicLink() || (await readdir(target)).length !== 0) {
      fail("npm cache publish target must be absent or an empty directory");
    }
    await rmdir(target);
  } catch (error) {
    if (error instanceof MaintainerError) {
      throw error;
    }
    if (error.code !== "ENOENT") {
      throw error;
    }
  }
  const staging = await mkdtemp(join(parent, `.${basename(target)}.staging-`));
  let published = false;
  try {
    await populateCache(staging, toolchain, ownedRoot);
    const replayOptions = { ...options, provisionPackageCache: false, verifyOfflineCacheReplay: false };
    await runOffline(replayOptions, staging, toolchain, ownedRoot, false, null);
    await makeReadonly(staging);
    await rename(staging, target);
    published = true;
  } finally {
    if (!published) {
      await rm(staging, { recursive: true, force: true });
    }
  }
}


async function main() {
  const options = parseArguments(process.argv.slice(2));
  await validateModePrerequisites(options);
  validateRegistryAuthority(options);
  const repository = await repositoryRoot();
  let cache = null;
  if (!options.provisionPackageCache) {
    cache = await validateNpmCache(options.npmCache, repository);
  }
  const toolchain = await validateToolchainFiles();
  const ownedRoot = await createOwnedRoot(repository);
  try {
    if (options.provisionPackageCache) {
      await provisionCache(options, repository, toolchain, ownedRoot);
      return;
    }
    let browserCache = null;
    if (options.browser !== null) {
      browserCache = await validateBrowserCache(options.browserCache, repository);
    }
    if (options.allowRegistry) {
      if ((await readdir(cache)).length !== 0) {
        fail("--allow-registry requires a fresh empty npm cache");
      }
      await populateCache(cache, toolchain, ownedRoot);
      await runOffline(options, cache, toolchain, ownedRoot, false, browserCache);
    } else {
      await runOffline(options, cache, toolchain, ownedRoot, true, browserCache);
    }
  } finally {
    await rm(ownedRoot, { recursive: true, force: false });
  }
}


try {
  await main();
} catch (error) {
  const exitCode = error instanceof MaintainerError ? error.exitCode : 1;
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`diagnostics frontend maintainer: ${message}\n`);
  process.exitCode = exitCode;
}
