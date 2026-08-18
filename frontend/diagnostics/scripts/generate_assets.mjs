#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  brotliCompressSync,
  constants as zlibConstants,
  gzipSync,
} from "node:zlib";

import { JSDOM } from "jsdom";


const projectRoot = resolve(fileURLToPath(new URL("..", import.meta.url)));
const repositoryRoot = resolve(projectRoot, "..", "..");
const generatedRoot = resolve(
  repositoryRoot,
  "rust/crates/troupe-diagnostics-runtime/assets/generated",
);
const buildRunner = join(projectRoot, "scripts", "build.mjs");
const indexName = "index.html";
const rustTableName = "assets.rs";
const noticesName = "third-party-notices.txt";
const generatedMemberPattern = /^diagnostics-([0-9a-f]{64})\.(js|css)\.(raw|gz|br)$/;
const immutableCacheControl = "public, max-age=31536000, immutable";
const maximumLogicalBytes = 512 * 1024;
const maximumFirstLoadBrotliBytes = 160 * 1024;
const maximumEmbeddedBytes = 768 * 1024;


class AssetGenerationError extends Error {}


function fail(message) {
  throw new AssetGenerationError(message);
}


function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}


function options() {
  const arguments_ = process.argv.slice(2);
  if (arguments_.length === 0) {
    return { check: false };
  }
  if (arguments_.length === 1 && arguments_[0] === "--check") {
    return { check: true };
  }
  fail("usage: generate_assets.mjs [--check]");
}


async function readJson(path, label) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    fail(`${label} is not readable JSON: ${error.message}`);
  }
}


async function treeFiles(root, prefix = "") {
  const files = [];
  for (const entry of await readdir(join(root, prefix), { withFileTypes: true })) {
    const member = prefix ? join(prefix, entry.name) : entry.name;
    if (entry.isDirectory()) {
      files.push(...await treeFiles(root, member));
    } else if (entry.isFile()) {
      files.push(member.split(sep).join("/"));
    } else {
      fail(`asset tree contains a non-regular member: ${member}`);
    }
  }
  return files.sort();
}


function buildIdentity(files) {
  const digest = createHash("sha256");
  for (const file of files) {
    digest.update(file.path, "utf8");
    digest.update("\0", "utf8");
    digest.update(String(file.bytes.length), "utf8");
    digest.update("\0", "utf8");
    digest.update(file.bytes);
  }
  return digest.digest("hex");
}


function runRawBuild(output) {
  const completed = spawnSync(process.execPath, [buildRunner, "--out-dir", output], {
    cwd: projectRoot,
    env: process.env,
    stdio: "inherit",
  });
  if (completed.error !== undefined) {
    fail(`raw build could not start: ${completed.error.message}`);
  }
  if (completed.status !== 0) {
    fail(`raw build failed with exit ${completed.status}`);
  }
}


async function rawBundle(root) {
  const members = await treeFiles(root);
  const scripts = members.filter((member) => member.endsWith(".js"));
  const styles = members.filter((member) => member.endsWith(".css"));
  if (
    members.length !== 3
    || !members.includes(indexName)
    || scripts.length !== 1
    || styles.length !== 1
  ) {
    fail("raw bundle must contain exactly one HTML, one JavaScript, and one CSS file");
  }
  const files = await Promise.all(
    [indexName, scripts[0], styles[0]].map(async (path) => ({
      path,
      bytes: await readFile(join(root, path)),
    })),
  );
  return {
    buildHash: buildIdentity(files),
    html: files[0].bytes,
    javascript: files[1].bytes,
    stylesheet: files[2].bytes,
  };
}


function releaseUrl(buildHash, kind) {
  return `./assets/diagnostics-${buildHash}.${kind}`;
}


function releaseHtml(raw, buildHash) {
  const dom = new JSDOM(raw.toString("utf8"));
  const document = dom.window.document;
  const scripts = [...document.querySelectorAll("script")];
  const styles = [...document.querySelectorAll('link[rel="stylesheet"]')];
  const favicons = [...document.querySelectorAll('link[rel="icon"]')];
  if (
    scripts.length !== 1
    || styles.length !== 1
    || favicons.length !== 1
    || favicons[0].getAttribute("href") !== "data:,"
    || scripts[0].textContent.trim() !== ""
    || document.querySelector("style, base") !== null
  ) {
    fail("raw HTML does not have the expected external asset shape");
  }
  scripts[0].setAttribute("src", releaseUrl(buildHash, "js"));
  styles[0].setAttribute("href", releaseUrl(buildHash, "css"));
  for (const element of document.querySelectorAll("[src], [href]")) {
    if (element !== scripts[0] && element !== styles[0] && element !== favicons[0]) {
      fail("raw HTML contains an undeclared resource reference");
    }
  }
  for (const node of [...document.body.childNodes]) {
    if (node.nodeType === dom.window.Node.TEXT_NODE && node.textContent.trim() === "") {
      node.remove();
    }
  }
  return Buffer.from(`${dom.serialize()}\n`, "utf8");
}


function representations(buildHash, kind, raw) {
  const compressed = [
    { suffix: "raw", encoding: "raw", contentEncoding: null, bytes: raw },
    { suffix: "gz", encoding: "gzip", contentEncoding: "gzip", bytes: gzipSync(raw, { level: 9, mtime: 0 }) },
    {
      suffix: "br",
      encoding: "br",
      contentEncoding: "br",
      bytes: brotliCompressSync(raw, {
        params: {
          [zlibConstants.BROTLI_PARAM_MODE]: zlibConstants.BROTLI_MODE_TEXT,
          [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
          [zlibConstants.BROTLI_PARAM_SIZE_HINT]: raw.length,
        },
      }),
    },
  ];
  return compressed.map((item) => {
    const fileName = `diagnostics-${buildHash}.${kind}.${item.suffix}`;
    return {
      fileName,
      url: releaseUrl(buildHash, kind),
      kind,
      encoding: item.encoding,
      contentEncoding: item.contentEncoding,
      mime: kind === "js" ? "text/javascript; charset=utf-8" : "text/css; charset=utf-8",
      cacheControl: immutableCacheControl,
      sha256: sha256(item.bytes),
      bytes: item.bytes,
    };
  });
}


function resolveLockedDependency(packages, parentPath, dependency) {
  if (parentPath === "") {
    return packages[`node_modules/${dependency}`] === undefined
      ? null
      : `node_modules/${dependency}`;
  }
  const parts = parentPath.split("/");
  const moduleIndexes = parts
    .map((part, index) => part === "node_modules" ? index : -1)
    .filter((index) => index >= 0)
    .reverse();
  for (const index of moduleIndexes) {
    const prefix = parts.slice(0, index).join("/");
    const candidate = `${prefix ? `${prefix}/` : ""}node_modules/${dependency}`;
    if (packages[candidate] !== undefined) {
      return candidate;
    }
  }
  return null;
}


function runtimeLockPaths(packageJson, lock) {
  const packages = lock.packages;
  const dependencies = packageJson.dependencies;
  if (JSON.stringify(packages[""].dependencies) !== JSON.stringify(dependencies)) {
    fail("package-lock runtime dependencies differ from package.json");
  }
  const pending = Object.keys(dependencies).sort().map((dependency) => {
    const path = resolveLockedDependency(packages, "", dependency);
    if (path === null) {
      fail(`package-lock does not resolve runtime dependency ${dependency}`);
    }
    return path;
  });
  const selected = new Set();
  while (pending.length > 0) {
    const path = pending.shift();
    if (selected.has(path)) {
      continue;
    }
    selected.add(path);
    const entry = packages[path];
    for (const group of [entry.dependencies, entry.optionalDependencies, entry.peerDependencies]) {
      if (group === undefined) {
        continue;
      }
      for (const dependency of Object.keys(group).sort()) {
        const resolved = resolveLockedDependency(packages, path, dependency);
        if (resolved !== null) {
          pending.push(resolved);
        }
      }
    }
    pending.sort();
  }
  return [...selected].sort();
}


async function thirdPartyNotices() {
  const packageJson = await readJson(join(projectRoot, "package.json"), "package.json");
  const lock = await readJson(join(projectRoot, "package-lock.json"), "package-lock.json");
  const notices = [];
  for (const lockedPath of runtimeLockPaths(packageJson, lock)) {
    const directory = join(projectRoot, lockedPath);
    const installed = await readJson(join(directory, "package.json"), `${lockedPath} package.json`);
    const licenseFiles = (await readdir(directory, { withFileTypes: true }))
      .filter((entry) => entry.isFile() && /^(licen[cs]e|copying|notice)(\..*)?$/i.test(entry.name))
      .map((entry) => entry.name)
      .sort();
    if (licenseFiles.length === 0 || installed.version !== lock.packages[lockedPath].version) {
      fail(`installed runtime package metadata is incomplete: ${lockedPath}`);
    }
    notices.push({
      name: installed.name,
      version: installed.version,
      license: installed.license,
      lockedPath,
      texts: await Promise.all(licenseFiles.map(async (file) => ({
        file,
        text: (await readFile(join(directory, file), "utf8")).replace(/\r\n?/g, "\n").trimEnd(),
      }))),
    });
  }
  notices.sort((left, right) => left.name.localeCompare(right.name) || left.version.localeCompare(right.version));
  const lines = [
    "Troupe Diagnostics Web UI - Third-Party Notices",
    "",
    "This file covers the package-lock production dependency closure declared for the UI bundle.",
    "",
  ];
  for (const notice of notices) {
    lines.push("=".repeat(79));
    lines.push(`${notice.name} ${notice.version}`);
    lines.push(`Declared license: ${notice.license}`);
    lines.push(`Locked package: ${notice.lockedPath}`);
    for (const text of notice.texts) {
      lines.push(`License file: ${text.file}`);
      lines.push("-".repeat(79));
      lines.push(text.text);
    }
    lines.push("");
  }
  return Buffer.from(`${lines.join("\n").trimEnd()}\n`, "utf8");
}


function rustString(value) {
  if (!/^[\x20-\x7e]*$/.test(value)) {
    fail("generated Rust metadata must remain printable ASCII");
  }
  return JSON.stringify(value);
}


function renderRustTable(buildHash, html, items, notices) {
  const lines = [
    "// @generated by frontend/diagnostics/scripts/generate_assets.mjs; do not edit.",
    "",
    "#[derive(Clone, Copy, Debug, Eq, PartialEq)]",
    "pub struct GeneratedRepresentation {",
    "    pub file_name: &'static str,",
    "    pub url: &'static str,",
    "    pub kind: &'static str,",
    "    pub encoding: &'static str,",
    "    pub content_encoding: Option<&'static str>,",
    "    pub mime: &'static str,",
    "    pub cache_control: &'static str,",
    "    pub sha256: &'static str,",
    "    pub bytes_len: usize,",
    "    pub bytes: &'static [u8],",
    "}",
    "",
    `pub const BUILD_SHA256: &str = ${rustString(buildHash)};`,
    `pub const INDEX_HTML_SHA256: &str = ${rustString(sha256(html))};`,
    'pub const INDEX_HTML_MIME: &str = "text/html; charset=utf-8";',
    'pub const INDEX_HTML_CACHE_CONTROL: &str = "no-cache";',
    `pub static INDEX_HTML: &[u8] = include_bytes!(${rustString(indexName)});`,
    "",
    `pub const THIRD_PARTY_NOTICES_SHA256: &str = ${rustString(sha256(notices))};`,
    `pub static THIRD_PARTY_NOTICES: &[u8] = include_bytes!(${rustString(noticesName)});`,
    "",
    "pub static REPRESENTATIONS: &[GeneratedRepresentation] = &[",
  ];
  for (const item of items) {
    lines.push("    GeneratedRepresentation {");
    lines.push(`        file_name: ${rustString(item.fileName)},`);
    lines.push(`        url: ${rustString(item.url)},`);
    lines.push(`        kind: ${rustString(item.kind)},`);
    lines.push(`        encoding: ${rustString(item.encoding)},`);
    lines.push(`        content_encoding: ${item.contentEncoding === null ? "None" : `Some(${rustString(item.contentEncoding)})`},`);
    lines.push(`        mime: ${rustString(item.mime)},`);
    lines.push(`        cache_control: ${rustString(item.cacheControl)},`);
    lines.push(`        sha256: ${rustString(item.sha256)},`);
    lines.push(`        bytes_len: ${item.bytes.length},`);
    lines.push(`        bytes: include_bytes!(${rustString(item.fileName)}),`);
    lines.push("    },");
  }
  lines.push("];", "");
  return Buffer.from(lines.join("\n"), "ascii");
}


async function expectedAssets(rawRoot) {
  const raw = await rawBundle(rawRoot);
  const html = releaseHtml(raw.html, raw.buildHash);
  const items = [
    ...representations(raw.buildHash, "js", raw.javascript),
    ...representations(raw.buildHash, "css", raw.stylesheet),
  ];
  const notices = await thirdPartyNotices();
  const rawBytes = items
    .filter((item) => item.encoding === "raw")
    .reduce((total, item) => total + item.bytes.length, 0);
  const brotliBytes = items
    .filter((item) => item.encoding === "br")
    .reduce((total, item) => total + item.bytes.length, 0);
  const embeddedBytes = html.length
    + notices.length
    + items.reduce((total, item) => total + item.bytes.length, 0);
  if (
    html.length + rawBytes > maximumLogicalBytes
    || html.length + brotliBytes > maximumFirstLoadBrotliBytes
    || embeddedBytes > maximumEmbeddedBytes
  ) {
    fail("generated asset release budget is exceeded");
  }
  const assets = new Map([
    [indexName, html],
    [noticesName, notices],
    [rustTableName, renderRustTable(raw.buildHash, html, items, notices)],
  ]);
  for (const item of items) {
    assets.set(item.fileName, item.bytes);
  }
  return { assets, buildHash: raw.buildHash, embeddedBytes };
}


async function ensureGeneratedRoot() {
  await mkdir(generatedRoot, { recursive: true });
  if (await realpath(generatedRoot) !== generatedRoot) {
    fail("generated asset root must not use symlink indirection");
  }
}


async function validateTree(expected) {
  const entries = await readdir(generatedRoot, { withFileTypes: true });
  const actualNames = entries.map((entry) => entry.name).sort();
  const expectedNames = [...expected.keys()].sort();
  if (JSON.stringify(actualNames) !== JSON.stringify(expectedNames)) {
    fail("generated asset tree has extra or missing members");
  }
  for (const entry of entries) {
    if (!entry.isFile() || entry.isSymbolicLink()) {
      fail(`generated asset is not a regular file: ${entry.name}`);
    }
    if (!(await readFile(join(generatedRoot, entry.name))).equals(expected.get(entry.name))) {
      fail(`checked-in generated asset differs: ${entry.name}`);
    }
  }
}


async function publishAssets(expected) {
  await ensureGeneratedRoot();
  const existing = await readdir(generatedRoot, { withFileTypes: true });
  for (const entry of existing) {
    if (
      !entry.isFile()
      || (![indexName, rustTableName, noticesName].includes(entry.name)
        && !generatedMemberPattern.test(entry.name))
    ) {
      fail(`refusing to replace unknown generated asset member: ${entry.name}`);
    }
  }
  for (const [name, bytes] of expected) {
    await writeFile(join(generatedRoot, name), bytes);
  }
  for (const entry of existing) {
    if (!expected.has(entry.name)) {
      await rm(join(generatedRoot, entry.name));
    }
  }
  await validateTree(expected);
}


async function main() {
  const requested = options();
  const temporaryRoot = await mkdtemp(join(tmpdir(), "troupe-diagnostics-assets-"));
  try {
    const rawRoot = join(temporaryRoot, "raw");
    runRawBuild(rawRoot);
    const expected = await expectedAssets(rawRoot);
    if (requested.check) {
      await validateTree(expected.assets);
    } else {
      await publishAssets(expected.assets);
    }
    process.stdout.write(
      `generated diagnostics assets ${expected.buildHash} (${expected.embeddedBytes} embedded bytes)\n`,
    );
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
}


try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`diagnostics asset generation: ${message}\n`);
  process.exitCode = 1;
}
