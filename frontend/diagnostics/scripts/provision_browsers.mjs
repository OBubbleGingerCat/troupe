#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createWriteStream } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rename,
  rm,
  rmdir,
  writeFile,
} from "node:fs/promises";
import { get as httpsGet } from "node:https";
import { arch, homedir, platform as osPlatform } from "node:os";
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
import { inflateRawSync } from "node:zlib";


const frontendRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(frontendRoot, "../..");
const lockPath = join(frontendRoot, "package-lock.json");
const defaultManifestPath = join(frontendRoot, "tests", "tooling", "playwright-browsers.json");
const identityName = ".troupe-playwright-cache.json";
const markerName = "INSTALLATION_COMPLETE";
const sha256Pattern = /^[0-9a-f]{64}$/;
const requiredNames = [
  "chromium",
  "chromium-headless-shell",
  "firefox",
  "webkit",
  "ffmpeg",
];
const pinnedRegistry = {
  chromium: {
    revision: "1234",
    browserVersion: "151.0.7922.34",
    url: "https://cdn.playwright.dev/builds/cft/151.0.7922.34/linux64/chrome-linux64.zip",
  },
  "chromium-headless-shell": {
    revision: "1234",
    browserVersion: "151.0.7922.34",
    url: "https://cdn.playwright.dev/builds/cft/151.0.7922.34/linux64/chrome-headless-shell-linux64.zip",
  },
  firefox: {
    revision: "1538",
    browserVersion: "153.0",
    url: "https://cdn.playwright.dev/dbazure/download/playwright/builds/firefox/1538/firefox-ubuntu-22.04.zip",
  },
  webkit: {
    revision: "2336",
    browserVersion: "26.5",
    url: "https://cdn.playwright.dev/dbazure/download/playwright/builds/webkit/2336/webkit-ubuntu-22.04.zip",
  },
  ffmpeg: {
    revision: "1011",
    browserVersion: null,
    url: "https://cdn.playwright.dev/dbazure/download/playwright/builds/ffmpeg/1011/ffmpeg-linux.zip",
  },
};


class ProvisionError extends Error {}


function fail(message) {
  throw new ProvisionError(message);
}


function parseArguments(argv) {
  const options = { browserCache: null, manifest: defaultManifestPath, transport: null };
  const value = (argument, position) => {
    const candidate = argv[position + 1];
    if (candidate === undefined || candidate.startsWith("--")) {
      fail(`${argument} requires a value`);
    }
    return candidate;
  };
  for (let position = 0; position < argv.length; position += 1) {
    const argument = argv[position];
    switch (argument) {
      case "--browser-cache":
        options.browserCache = value(argument, position);
        position += 1;
        break;
      case "--manifest":
        options.manifest = value(argument, position);
        position += 1;
        break;
      case "--transport":
        options.transport = value(argument, position);
        position += 1;
        break;
      default:
        fail(`unknown argument: ${argument}`);
    }
  }
  if (options.browserCache === null) {
    fail("--browser-cache is required");
  }
  return options;
}


function isWithin(candidate, parent) {
  const remainder = relative(parent, candidate);
  return remainder === "" || (remainder !== ".." && !remainder.startsWith(`..${sep}`));
}


function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}


function exactFields(value, fields, label) {
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    fail(`${label} must be an object`);
  }
  const actual = Object.keys(value).sort();
  const expected = [...fields].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail(`${label} fields are not exact`);
  }
}


function equalJson(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}


function canonicalRelativePath(candidate, label) {
  if (
    typeof candidate !== "string"
    || candidate.length === 0
    || isAbsolute(candidate)
    || candidate.includes("\\")
    || candidate.includes("\0")
    || candidate.split("/").some((part) => part === "" || part === "." || part === "..")
  ) {
    fail(`${label} path is not canonical repository-relative data: ${String(candidate)}`);
  }
  return candidate;
}


function currentPlatform() {
  if (osPlatform() === "linux" && arch() === "x64") {
    return { id: "linux-x64", playwright: "ubuntu22.04-x64" };
  }
  fail(`unsupported browser cache platform: ${osPlatform()}-${arch()}`);
}


async function readRegularFile(path, label) {
  let metadata;
  try {
    metadata = await lstat(path);
  } catch (error) {
    fail(`${label} is missing: ${error.message}`);
  }
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    fail(`${label} must be a regular file without symlink indirection`);
  }
  const resolved = await realpath(path);
  if (resolved !== path) {
    fail(`${label} must use its exact real path`);
  }
  return readFile(path);
}


async function readJson(path, label) {
  const bytes = await readRegularFile(path, label);
  let value;
  try {
    value = JSON.parse(bytes.toString("utf8"));
  } catch (error) {
    fail(`${label} is not valid JSON: ${error.message}`);
  }
  return { bytes, value };
}


function validateTestAuthority(options, target) {
  const customManifest = resolve(options.manifest) !== defaultManifestPath;
  const testMode = customManifest || options.transport !== null;
  if (!testMode) {
    return { testMode: false, manifestPath: defaultManifestPath, transport: null };
  }
  if (
    !customManifest
    || options.transport === null
    || process.env.TROUPE_PLAYWRIGHT_TEST_TRANSPORT !== "1"
  ) {
    fail("custom manifest and fake transport must be explicitly authorized together");
  }
  const rawGateRoot = process.env.TROUPE_GATE_TMP;
  if (rawGateRoot === undefined || !isAbsolute(rawGateRoot)) {
    fail("fake transport requires an absolute TROUPE_GATE_TMP");
  }
  const gateRoot = resolve(rawGateRoot);
  const manifestPath = resolve(options.manifest);
  const transport = resolve(options.transport);
  if (![manifestPath, transport, target].every((path) => isWithin(path, gateRoot))) {
    fail("fake transport, manifest, and target must remain inside TROUPE_GATE_TMP");
  }
  return { testMode: true, manifestPath, transport };
}


function validateArchive(record, expectedName, testMode) {
  exactFields(
    record,
    [
      "name",
      "revision",
      "browserVersion",
      "cacheDirectory",
      "url",
      "archiveSha256",
      "treeSha256",
      "memberCount",
      "executable",
      "executableSha256",
      "materializedLinks",
    ],
    `browser manifest archive ${expectedName}`,
  );
  const pinned = pinnedRegistry[expectedName];
  if (record.name !== expectedName) {
    fail(`browser manifest name mismatch: expected ${expectedName}`);
  }
  if (record.revision !== pinned.revision) {
    fail(`browser manifest revision mismatch for ${expectedName}`);
  }
  if (record.browserVersion !== pinned.browserVersion) {
    fail(`browser manifest browserVersion mismatch for ${expectedName}`);
  }
  const cacheDirectory = `${expectedName.replace(/-/g, "_")}-${pinned.revision}`;
  if (record.cacheDirectory !== cacheDirectory) {
    fail(`browser manifest cache directory mismatch for ${expectedName}`);
  }
  let url;
  try {
    url = new URL(record.url);
  } catch {
    fail(`browser manifest URL is invalid for ${expectedName}`);
  }
  if (url.protocol !== "https:" || url.username || url.password || url.hash) {
    fail(`browser manifest URL must be an uncredentialed HTTPS URL for ${expectedName}`);
  }
  if (!testMode && record.url !== pinned.url) {
    fail(`browser manifest URL mismatch for ${expectedName}`);
  }
  for (const field of ["archiveSha256", "treeSha256", "executableSha256"]) {
    if (typeof record[field] !== "string" || !sha256Pattern.test(record[field])) {
      fail(`browser manifest ${field} is invalid for ${expectedName}`);
    }
  }
  if (!Number.isSafeInteger(record.memberCount) || record.memberCount <= 0) {
    fail(`browser manifest memberCount is invalid for ${expectedName}`);
  }
  canonicalRelativePath(record.executable, `${expectedName} executable`);
  if (!Array.isArray(record.materializedLinks)) {
    fail(`browser manifest materializedLinks is invalid for ${expectedName}`);
  }
  const linkPaths = new Set();
  let previousLinkPath = null;
  for (const link of record.materializedLinks) {
    exactFields(link, ["path", "target"], `${expectedName} materialized link`);
    canonicalRelativePath(link.path, `${expectedName} materialized link`);
    if (
      typeof link.target !== "string"
      || link.target.length === 0
      || isAbsolute(link.target)
      || link.target.includes("\\")
      || link.target.includes("\0")
    ) {
      fail(`${expectedName} materialized link target is invalid: ${String(link.target)}`);
    }
    const syntheticRoot = "/troupe-browser-cache";
    const resolvedTarget = resolve(syntheticRoot, dirname(link.path), link.target);
    if (!isWithin(resolvedTarget, syntheticRoot) || resolvedTarget === syntheticRoot) {
      fail(`${expectedName} materialized link target escapes its cache directory`);
    }
    if (linkPaths.has(link.path) || (previousLinkPath !== null && link.path <= previousLinkPath)) {
      fail(`${expectedName} materialized links must have unique sorted paths`);
    }
    linkPaths.add(link.path);
    previousLinkPath = link.path;
  }
  if (linkPaths.has(record.executable)) {
    fail(`${expectedName} executable must not be a materialized link`);
  }
  return record;
}


async function validateManifest(manifestPath, testMode) {
  const { bytes, value: manifest } = await readJson(manifestPath, "browser manifest");
  exactFields(
    manifest,
    ["schemaVersion", "lockSha256", "playwrightCore", "platforms"],
    "browser manifest",
  );
  if (manifest.schemaVersion !== 1) {
    fail("browser manifest schema version is unsupported");
  }
  const lockBytes = await readRegularFile(lockPath, "package lock");
  const lockSha256 = sha256(lockBytes);
  if (manifest.lockSha256 !== lockSha256) {
    fail("browser manifest lock SHA-256 mismatch");
  }
  let lock;
  try {
    lock = JSON.parse(lockBytes.toString("utf8"));
  } catch (error) {
    fail(`package lock is not valid JSON: ${error.message}`);
  }
  const core = lock.packages?.["node_modules/playwright-core"];
  if (core === undefined) {
    fail("package lock does not contain playwright-core");
  }
  exactFields(
    manifest.playwrightCore,
    ["version", "integrity", "browsersSha256"],
    "browser manifest playwrightCore",
  );
  if (
    manifest.playwrightCore.version !== core.version
    || manifest.playwrightCore.integrity !== core.integrity
  ) {
    fail("browser manifest playwright-core identity does not match the lock");
  }
  if (!sha256Pattern.test(manifest.playwrightCore.browsersSha256)) {
    fail("browser manifest playwright-core registry SHA-256 is invalid");
  }
  const platform = currentPlatform();
  exactFields(manifest.platforms, [platform.id], "browser manifest platforms");
  const selected = manifest.platforms[platform.id];
  exactFields(selected, ["playwrightPlatform", "archives"], "browser manifest platform");
  if (selected.playwrightPlatform !== platform.playwright) {
    fail("browser manifest Playwright platform mismatch");
  }
  if (!Array.isArray(selected.archives) || selected.archives.length !== requiredNames.length) {
    fail("browser manifest must contain the exact default archive set");
  }
  const archives = selected.archives.map((record, index) => (
    validateArchive(record, requiredNames[index], testMode)
  ));
  return {
    manifestSha256: sha256(bytes),
    lockSha256,
    playwrightCore: manifest.playwrightCore,
    platform,
    archives,
  };
}


async function validateTarget(rawTarget, lockSha256, platform, testAuthority) {
  if (!isAbsolute(rawTarget)) {
    fail("--browser-cache must be an absolute path");
  }
  const target = resolve(rawTarget);
  if (target !== rawTarget) {
    fail("--browser-cache must be a canonical absolute path");
  }
  if (basename(target) !== platform || basename(dirname(target)) !== lockSha256) {
    fail("browser cache path must end in the exact lock SHA-256 and platform");
  }
  const parent = dirname(target);
  let parentReal;
  try {
    parentReal = await realpath(parent);
  } catch (error) {
    fail(`browser cache parent must already exist: ${error.message}`);
  }
  if (parentReal !== parent) {
    fail("browser cache parent must be an exact real path without symlink indirection");
  }
  if (isWithin(target, repositoryRoot)) {
    fail("browser cache must remain outside the repository");
  }
  const homeCache = resolve(homedir(), ".cache", "ms-playwright");
  if (isWithin(target, homeCache) || isWithin(homeCache, target)) {
    fail("browser cache must not use the home Playwright cache");
  }
  if (testAuthority.testMode && !isWithin(target, resolve(process.env.TROUPE_GATE_TMP))) {
    fail("test browser cache must remain inside TROUPE_GATE_TMP");
  }
  return target;
}


function runFakeTransport(transport, url, destination, gateRoot) {
  const completed = spawnSync(transport, [url, destination], {
    cwd: gateRoot,
    env: { ...process.env },
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (completed.error !== undefined) {
    fail(`fake transport could not start: ${completed.error.message}`);
  }
  if (completed.status !== 0) {
    const detail = completed.stderr.trim() || completed.stdout.trim();
    fail(`fake transport failed with exit ${completed.status}${detail ? `: ${detail}` : ""}`);
  }
}


function downloadHttps(url, destination, redirects = 0) {
  if (redirects > 5) {
    return Promise.reject(new ProvisionError("browser download exceeded the redirect limit"));
  }
  return new Promise((accept, reject) => {
    const request = httpsGet(url, { headers: { "User-Agent": "troupe-browser-provisioner/1" } }, (response) => {
      if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
        response.resume();
        const redirected = new URL(response.headers.location, url);
        if (redirected.protocol !== "https:") {
          reject(new ProvisionError("browser download redirect is not HTTPS"));
          return;
        }
        downloadHttps(redirected.href, destination, redirects + 1).then(accept, reject);
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new ProvisionError(`browser download failed with HTTP ${response.statusCode}`));
        return;
      }
      const output = createWriteStream(destination, { flags: "wx", mode: 0o600 });
      let received = 0;
      response.on("data", (chunk) => {
        received += chunk.length;
        if (received > 1024 * 1024 * 1024) {
          request.destroy(new ProvisionError("browser archive exceeds the 1 GiB limit"));
        }
      });
      response.on("error", reject);
      output.on("error", reject);
      output.on("close", accept);
      response.pipe(output);
    });
    request.setTimeout(120_000, () => request.destroy(new ProvisionError("browser download timed out")));
    request.on("error", reject);
  });
}


async function downloadArchive(archive, destination, authority) {
  if (authority.testMode) {
    runFakeTransport(authority.transport, archive.url, destination, resolve(process.env.TROUPE_GATE_TMP));
  } else {
    await downloadHttps(archive.url, destination);
  }
  const bytes = await readRegularFile(destination, `${archive.name} downloaded archive`);
  if (sha256(bytes) !== archive.archiveSha256) {
    fail(`${archive.name} archive hash mismatch`);
  }
}


let crcTable;


function crc32(bytes) {
  if (crcTable === undefined) {
    crcTable = Array.from({ length: 256 }, (_, value) => {
      let current = value;
      for (let bit = 0; bit < 8; bit += 1) {
        current = current & 1 ? 0xedb88320 ^ (current >>> 1) : current >>> 1;
      }
      return current >>> 0;
    });
  }
  let result = 0xffffffff;
  for (const byte of bytes) {
    result = crcTable[(result ^ byte) & 0xff] ^ (result >>> 8);
  }
  return (result ^ 0xffffffff) >>> 0;
}


function findEndOfCentralDirectory(bytes) {
  const lower = Math.max(0, bytes.length - 65_557);
  for (let offset = bytes.length - 22; offset >= lower; offset -= 1) {
    if (bytes.readUInt32LE(offset) === 0x06054b50) {
      const commentLength = bytes.readUInt16LE(offset + 20);
      if (offset + 22 + commentLength === bytes.length) {
        return offset;
      }
    }
  }
  fail("browser archive has no valid ZIP central directory");
}


function decodeZipName(bytes) {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    fail("browser archive contains a non-UTF-8 member path");
  }
}


function zipEntries(bytes) {
  const end = findEndOfCentralDirectory(bytes);
  if (bytes.readUInt16LE(end + 4) !== 0 || bytes.readUInt16LE(end + 6) !== 0) {
    fail("multi-disk browser ZIP archives are unsupported");
  }
  const count = bytes.readUInt16LE(end + 10);
  const centralSize = bytes.readUInt32LE(end + 12);
  const centralOffset = bytes.readUInt32LE(end + 16);
  if (count === 0xffff || centralSize === 0xffffffff || centralOffset === 0xffffffff) {
    fail("ZIP64 browser archives are unsupported");
  }
  if (centralOffset + centralSize !== end) {
    fail("browser archive central directory bounds are invalid");
  }
  const entries = [];
  let offset = centralOffset;
  for (let index = 0; index < count; index += 1) {
    if (offset + 46 > end || bytes.readUInt32LE(offset) !== 0x02014b50) {
      fail("browser archive central directory entry is invalid");
    }
    const madeBy = bytes.readUInt16LE(offset + 4);
    const flags = bytes.readUInt16LE(offset + 8);
    const method = bytes.readUInt16LE(offset + 10);
    const expectedCrc32 = bytes.readUInt32LE(offset + 16);
    const compressedSize = bytes.readUInt32LE(offset + 20);
    const uncompressedSize = bytes.readUInt32LE(offset + 24);
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    const disk = bytes.readUInt16LE(offset + 34);
    const externalAttributes = bytes.readUInt32LE(offset + 38);
    const localOffset = bytes.readUInt32LE(offset + 42);
    const next = offset + 46 + nameLength + extraLength + commentLength;
    if (next > end || disk !== 0 || compressedSize === 0xffffffff || uncompressedSize === 0xffffffff) {
      fail("browser archive central directory metadata is unsupported");
    }
    if ((flags & 0x1) !== 0 || ![0, 8].includes(method)) {
      fail("browser archive contains an encrypted or unsupported member");
    }
    const rawName = bytes.subarray(offset + 46, offset + 46 + nameLength);
    const name = decodeZipName(rawName);
    const trimmedName = name.endsWith("/") ? name.slice(0, -1) : name;
    canonicalRelativePath(trimmedName, "browser archive member");
    const creator = madeBy >>> 8;
    const declaredMode = creator === 3 ? externalAttributes >>> 16 : 0;
    const type = declaredMode & 0o170000;
    const directory = name.endsWith("/") || type === 0o040000;
    const symlink = type === 0o120000;
    if (![0, 0o040000, 0o100000, 0o120000].includes(type)) {
      fail(`browser archive special member is forbidden: ${name}`);
    }
    entries.push({
      name: trimmedName,
      rawName,
      directory,
      symlink,
      mode: declaredMode,
      method,
      expectedCrc32,
      compressedSize,
      uncompressedSize,
      localOffset,
    });
    offset = next;
  }
  if (offset !== end) {
    fail("browser archive central directory cardinality is invalid");
  }
  return entries;
}


function entryBytes(archiveBytes, entry) {
  const offset = entry.localOffset;
  if (offset + 30 > archiveBytes.length || archiveBytes.readUInt32LE(offset) !== 0x04034b50) {
    fail(`browser archive local header is invalid: ${entry.name}`);
  }
  const nameLength = archiveBytes.readUInt16LE(offset + 26);
  const extraLength = archiveBytes.readUInt16LE(offset + 28);
  const rawName = archiveBytes.subarray(offset + 30, offset + 30 + nameLength);
  if (!rawName.equals(entry.rawName)) {
    fail(`browser archive local path differs from central path: ${entry.name}`);
  }
  const dataOffset = offset + 30 + nameLength + extraLength;
  const dataEnd = dataOffset + entry.compressedSize;
  if (dataEnd > archiveBytes.length) {
    fail(`browser archive member exceeds archive bounds: ${entry.name}`);
  }
  const compressed = archiveBytes.subarray(dataOffset, dataEnd);
  let uncompressed;
  try {
    uncompressed = entry.method === 0 ? Buffer.from(compressed) : inflateRawSync(compressed);
  } catch (error) {
    fail(`browser archive member cannot be decompressed (${entry.name}): ${error.message}`);
  }
  if (uncompressed.length !== entry.uncompressedSize || crc32(uncompressed) !== entry.expectedCrc32) {
    fail(`browser archive member checksum mismatch: ${entry.name}`);
  }
  return uncompressed;
}


function decodedLinkTarget(bytes, entry) {
  const target = decodeZipName(bytes);
  if (
    target.length === 0
    || isAbsolute(target)
    || target.includes("\\")
    || target.includes("\0")
  ) {
    fail(`browser archive symlink target is invalid: ${entry.name}`);
  }
  return target;
}


async function extractArchive(archivePath, destination, archive) {
  const bytes = await readFile(archivePath);
  const entries = zipEntries(bytes);
  const seen = new Set();
  const expectedLinks = new Map(archive.materializedLinks.map((link) => [link.path, link.target]));
  const archiveLinks = new Map();
  for (const entry of entries) {
    if (!entry.symlink) {
      continue;
    }
    const expectedTarget = expectedLinks.get(entry.name);
    if (expectedTarget === undefined) {
      fail(`browser archive symlink is not pinned for materialization: ${entry.name}`);
    }
    const target = decodedLinkTarget(entryBytes(bytes, entry), entry);
    if (target !== expectedTarget) {
      fail(`browser archive symlink target mismatch: ${entry.name}`);
    }
    if (entries.some((candidate) => candidate.name.startsWith(`${entry.name}/`))) {
      fail(`browser archive symlink cannot be a parent path: ${entry.name}`);
    }
    archiveLinks.set(entry.name, target);
  }
  if (
    archiveLinks.size !== expectedLinks.size
    || [...expectedLinks].some(([path, target]) => archiveLinks.get(path) !== target)
  ) {
    fail(`${archive.name} browser archive materialized link set is incomplete`);
  }
  await mkdir(destination, { mode: 0o700 });
  for (const entry of entries) {
    if (seen.has(entry.name)) {
      fail(`browser archive contains duplicate path: ${entry.name}`);
    }
    seen.add(entry.name);
    const output = join(destination, ...entry.name.split("/"));
    if (!isWithin(output, destination)) {
      fail(`browser archive member path escapes staging: ${entry.name}`);
    }
    if (entry.directory) {
      await mkdir(output, { recursive: true, mode: 0o700 });
      continue;
    }
    if (entry.symlink) {
      continue;
    }
    await mkdir(dirname(output), { recursive: true, mode: 0o700 });
    const data = entryBytes(bytes, entry);
    const permissions = entry.mode & 0o777;
    await writeFile(output, data, { flag: "wx", mode: permissions || 0o644 });
    await chmod(output, permissions || 0o644);
  }
  const materialized = new Set();
  const visiting = new Set();
  const materialize = async (memberPath) => {
    if (materialized.has(memberPath)) {
      return;
    }
    if (visiting.has(memberPath)) {
      fail(`browser archive materialized link cycle: ${memberPath}`);
    }
    visiting.add(memberPath);
    const target = archiveLinks.get(memberPath);
    const output = join(destination, ...memberPath.split("/"));
    const resolvedTarget = resolve(dirname(output), target);
    if (!isWithin(resolvedTarget, destination) || resolvedTarget === destination) {
      fail(`browser archive materialized link escapes staging: ${memberPath}`);
    }
    const targetPath = relative(destination, resolvedTarget).split(sep).join("/");
    if (archiveLinks.has(targetPath)) {
      await materialize(targetPath);
    }
    const targetMetadata = await lstat(resolvedTarget).catch(() => null);
    if (
      targetMetadata === null
      || !targetMetadata.isFile()
      || targetMetadata.isSymbolicLink()
    ) {
      fail(`browser archive materialized link target is not a regular file: ${memberPath}`);
    }
    await mkdir(dirname(output), { recursive: true, mode: 0o700 });
    await writeFile(output, await readFile(resolvedTarget), {
      flag: "wx",
      mode: targetMetadata.mode & 0o777,
    });
    await chmod(output, targetMetadata.mode & 0o777);
    visiting.delete(memberPath);
    materialized.add(memberPath);
  };
  for (const memberPath of archiveLinks.keys()) {
    await materialize(memberPath);
  }
  const executablePath = join(destination, ...archive.executable.split("/"));
  if (!isWithin(executablePath, destination)) {
    fail("browser executable path escapes its cache directory");
  }
  let executableMetadata;
  try {
    executableMetadata = await lstat(executablePath);
  } catch (error) {
    fail(`browser executable is missing after extraction: ${error.message}`);
  }
  if (!executableMetadata.isFile() || executableMetadata.isSymbolicLink()) {
    fail("browser executable is not a regular file");
  }
  await chmod(executablePath, 0o755);
}


async function fingerprintTree(root, prefix = "") {
  const members = [];
  const names = (await readdir(root)).sort();
  for (const name of names) {
    if (prefix === "" && name === markerName) {
      continue;
    }
    const path = join(root, name);
    const memberPath = prefix === "" ? name : `${prefix}/${name}`;
    const metadata = await lstat(path);
    if (metadata.isSymbolicLink()) {
      fail(`browser cache contains a symlink: ${memberPath}`);
    }
    if (metadata.isDirectory()) {
      members.push(...await fingerprintTree(path, memberPath));
    } else if (metadata.isFile()) {
      const bytes = await readFile(path);
      members.push({
        path: memberPath,
        mode: (metadata.mode & 0o170000) | (metadata.mode & 0o111 ? 0o555 : 0o444),
        size: bytes.length,
        sha256: sha256(bytes),
      });
    } else {
      fail(`browser cache contains a special member: ${memberPath}`);
    }
  }
  return members;
}


function treeSha256(members) {
  const digest = createHash("sha256");
  const ordered = [...members].sort((left, right) => (
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0
  ));
  for (const member of ordered) {
    digest.update(`${member.path}\0${member.mode.toString(8)}\0${member.size}\0${member.sha256}\n`);
  }
  return digest.digest("hex");
}


async function verifyArchiveDirectory(cache, archive) {
  const directory = join(cache, archive.cacheDirectory);
  let metadata;
  try {
    metadata = await lstat(directory);
  } catch (error) {
    fail(`${archive.name} cache directory is missing: ${error.message}`);
  }
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    fail(`${archive.name} cache directory is not a real directory`);
  }
  const marker = join(directory, markerName);
  const markerMetadata = await lstat(marker).catch(() => null);
  if (
    markerMetadata === null
    || !markerMetadata.isFile()
    || markerMetadata.isSymbolicLink()
    || markerMetadata.size !== 0
  ) {
    fail(`${archive.name} cache is partial: installation marker is missing or invalid`);
  }
  const members = await fingerprintTree(directory);
  const actualTreeSha256 = treeSha256(members);
  const executable = members.find((member) => member.path === archive.executable);
  const executableSha256 = executable?.sha256 ?? "missing";
  if (members.length !== archive.memberCount || actualTreeSha256 !== archive.treeSha256) {
    fail(
      `${archive.name} cache member tree hash mismatch or partial cache: `
      + `expected ${archive.memberCount}/${archive.treeSha256}, `
      + `got ${members.length}/${actualTreeSha256}; executable ${executableSha256}`,
    );
  }
  if (executable === undefined || executable.sha256 !== archive.executableSha256) {
    fail(`${archive.name} executable hash mismatch: got ${executableSha256}`);
  }
}


function expectedIdentity(validated) {
  return {
    schemaVersion: 1,
    manifestSha256: validated.manifestSha256,
    lockSha256: validated.lockSha256,
    platform: validated.platform.id,
    playwrightPlatform: validated.platform.playwright,
    playwrightCore: validated.playwrightCore,
    archives: validated.archives.map((archive) => ({
      name: archive.name,
      revision: archive.revision,
      browserVersion: archive.browserVersion,
      cacheDirectory: archive.cacheDirectory,
      archiveSha256: archive.archiveSha256,
      treeSha256: archive.treeSha256,
      memberCount: archive.memberCount,
      executable: archive.executable,
      executableSha256: archive.executableSha256,
      materializedLinks: archive.materializedLinks,
    })),
  };
}


async function verifyReadonlyTree(root) {
  const metadata = await lstat(root);
  if (metadata.isSymbolicLink() || (!metadata.isDirectory() && !metadata.isFile())) {
    fail("browser cache contains a symlink or special member");
  }
  if ((metadata.mode & 0o222) !== 0) {
    fail("browser cache must be read-only");
  }
  if (metadata.isDirectory()) {
    for (const name of await readdir(root)) {
      await verifyReadonlyTree(join(root, name));
    }
  }
}


async function verifyCache(cache, validated, requireReadonly) {
  const metadata = await lstat(cache);
  if (!metadata.isDirectory() || metadata.isSymbolicLink() || await realpath(cache) !== cache) {
    fail("browser cache must be a real directory without symlink indirection");
  }
  const expectedEntries = [identityName, ...validated.archives.map((item) => item.cacheDirectory)].sort();
  if (!equalJson((await readdir(cache)).sort(), expectedEntries)) {
    fail("browser cache has missing or unexpected top-level members");
  }
  const { value: identity } = await readJson(join(cache, identityName), "browser cache identity");
  if (!equalJson(identity, expectedIdentity(validated))) {
    fail("browser cache identity does not match the pinned manifest");
  }
  for (const archive of validated.archives) {
    await verifyArchiveDirectory(cache, archive);
  }
  if (requireReadonly) {
    await verifyReadonlyTree(cache);
  }
}


async function makeReadonly(root) {
  const metadata = await lstat(root);
  if (metadata.isSymbolicLink()) {
    fail("refusing to chmod a symlink in browser cache staging");
  }
  if (metadata.isDirectory()) {
    for (const name of await readdir(root)) {
      await makeReadonly(join(root, name));
    }
    await chmod(root, 0o555);
  } else if (metadata.isFile()) {
    await chmod(root, metadata.mode & 0o111 ? 0o555 : 0o444);
  } else {
    fail("refusing to chmod a special member in browser cache staging");
  }
}


async function makeWritableForCleanup(root) {
  const metadata = await lstat(root).catch(() => null);
  if (metadata === null || metadata.isSymbolicLink()) {
    return;
  }
  if (metadata.isDirectory()) {
    await chmod(root, 0o700);
    for (const name of await readdir(root)) {
      await makeWritableForCleanup(join(root, name));
    }
  } else if (metadata.isFile()) {
    await chmod(root, 0o600);
  }
}


async function prepareAbsentTarget(target, validated) {
  const metadata = await lstat(target).catch((error) => {
    if (error.code === "ENOENT") {
      return null;
    }
    throw error;
  });
  if (metadata === null) {
    return false;
  }
  if (!metadata.isDirectory() || metadata.isSymbolicLink() || await realpath(target) !== target) {
    fail("existing browser cache target is not a real directory");
  }
  if ((await readdir(target)).length === 0) {
    await rmdir(target);
    return false;
  }
  await verifyCache(target, validated, true);
  return true;
}


async function provision(target, validated, authority) {
  if (await prepareAbsentTarget(target, validated)) {
    return;
  }
  const parent = dirname(target);
  const staging = await mkdtemp(join(parent, `.${basename(target)}.staging-`));
  let published = false;
  try {
    const downloads = join(staging, ".downloads");
    await mkdir(downloads, { mode: 0o700 });
    for (const archive of validated.archives) {
      const archivePath = join(downloads, `${archive.name}.zip`);
      await downloadArchive(archive, archivePath, authority);
      const destination = join(staging, archive.cacheDirectory);
      await extractArchive(archivePath, destination, archive);
      const members = await fingerprintTree(destination);
      const actualTreeSha256 = treeSha256(members);
      const executable = members.find((member) => member.path === archive.executable);
      const executableSha256 = executable?.sha256 ?? "missing";
      if (members.length !== archive.memberCount || actualTreeSha256 !== archive.treeSha256) {
        fail(
          `${archive.name} extracted member tree hash mismatch: `
          + `expected ${archive.memberCount}/${archive.treeSha256}, `
          + `got ${members.length}/${actualTreeSha256}; executable ${executableSha256}`,
        );
      }
      if (executable === undefined || executable.sha256 !== archive.executableSha256) {
        fail(`${archive.name} extracted executable hash mismatch: got ${executableSha256}`);
      }
      await writeFile(join(destination, markerName), "", { flag: "wx", mode: 0o600 });
    }
    await rm(downloads, { recursive: true, force: false });
    await writeFile(
      join(staging, identityName),
      `${JSON.stringify(expectedIdentity(validated), null, 2)}\n`,
      { encoding: "utf8", flag: "wx", mode: 0o600 },
    );
    await verifyCache(staging, validated, false);
    await makeReadonly(staging);
    await verifyCache(staging, validated, true);
    await rename(staging, target);
    published = true;
  } finally {
    if (!published) {
      await makeWritableForCleanup(staging);
      await rm(staging, { recursive: true, force: true });
    }
  }
}


async function main() {
  const options = parseArguments(process.argv.slice(2));
  if (!isAbsolute(options.browserCache)) {
    fail("--browser-cache must be an absolute path");
  }
  const preliminaryTarget = resolve(options.browserCache);
  const authority = validateTestAuthority(options, preliminaryTarget);
  const validated = await validateManifest(authority.manifestPath, authority.testMode);
  const target = await validateTarget(
    options.browserCache,
    validated.lockSha256,
    validated.platform.id,
    authority,
  );
  await provision(target, validated, authority);
}


try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`diagnostics browser provisioner: ${message}\n`);
  process.exitCode = 1;
}
