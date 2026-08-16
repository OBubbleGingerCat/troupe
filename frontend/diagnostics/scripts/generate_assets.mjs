#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  brotliCompressSync,
  brotliDecompressSync,
  constants as zlibConstants,
  gunzipSync,
  gzipSync,
} from "node:zlib";

import { JSDOM } from "jsdom";


const projectRoot = resolve(fileURLToPath(new URL("..", import.meta.url)));
const repositoryRoot = resolve(projectRoot, "..", "..");
const generatedRelative = "rust/crates/troupe-diagnostics-runtime/assets/generated/";
const generatedRoot = resolve(repositoryRoot, generatedRelative);
const buildRunner = join(projectRoot, "scripts", "build.mjs");
const rawManifestName = "raw-dist-manifest.json";
const manifestName = "manifest.json";
const rustTableName = "assets.rs";
const noticesName = "third-party-notices.txt";
const fullHashPattern = /^[0-9a-f]{64}$/;
const generatedMemberPattern = /^diagnostics-([0-9a-f]{64})\.(js|css)\.(raw|gz|br)$/;
const maximumLogicalBytes = 512 * 1024;
const maximumFirstLoadBrotliBytes = 160 * 1024;
const maximumEmbeddedBytes = 768 * 1024;
const immutableCacheControl = "public, max-age=31536000, immutable";


class AssetGenerationError extends Error {}


function fail(message) {
  throw new AssetGenerationError(message);
}


function objectValue(value, label) {
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    fail(`${label} must be an object`);
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


function stringValue(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    fail(`${label} must be a nonempty string`);
  }
  return value;
}


function byteCount(value, label) {
  if (!Number.isSafeInteger(value) || value < 0) {
    fail(`${label} must be a nonnegative safe integer`);
  }
  return value;
}


function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}


function compareText(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}


async function readJson(path, label) {
  let value;
  try {
    value = JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    fail(`${label} is not readable JSON: ${error.message}`);
  }
  return objectValue(value, label);
}


function isWithin(path, parent) {
  const remainder = relative(parent, path);
  return remainder === "" || (remainder !== ".." && !remainder.startsWith(`..${sep}`));
}


async function requireGateRoot() {
  const raw = process.env.TROUPE_GATE_TMP;
  if (raw === undefined || !isAbsolute(raw) || resolve(raw) !== raw) {
    fail("TROUPE_GATE_TMP must be a canonical absolute path");
  }
  let metadata;
  let canonical;
  try {
    metadata = await lstat(raw);
    canonical = await realpath(raw);
  } catch (error) {
    fail(`TROUPE_GATE_TMP must be an existing directory: ${error.message}`);
  }
  if (metadata.isSymbolicLink() || !metadata.isDirectory() || canonical !== raw) {
    fail("TROUPE_GATE_TMP must be a real directory without symlink indirection");
  }
  if (isWithin(canonical, repositoryRoot)) {
    fail("TROUPE_GATE_TMP must be outside the repository");
  }
  return canonical;
}


function parseArguments(argv) {
  if (argv.length === 0) {
    return { check: false };
  }
  if (argv.length === 1 && argv[0] === "--check") {
    return { check: true };
  }
  fail("usage: generate_assets.mjs [--check]");
}


function generationAttempt() {
  const raw = process.env.TROUPE_FRONTEND_GENERATION_ATTEMPT ?? "1";
  if (!/^[1-9][0-9]*$/.test(raw)) {
    fail("TROUPE_FRONTEND_GENERATION_ATTEMPT must be a positive integer");
  }
  return raw;
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


async function treeFiles(root, prefix = "") {
  const members = [];
  for (const entry of await readdir(join(root, prefix), { withFileTypes: true })) {
    const member = prefix ? join(prefix, entry.name) : entry.name;
    if (entry.isDirectory()) {
      members.push(...await treeFiles(root, member));
    } else if (entry.isFile()) {
      members.push(member.split(sep).join("/"));
    } else {
      fail(`asset tree contains a non-regular member: ${member}`);
    }
  }
  return members.sort();
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


async function loadRawBundle(rawRoot) {
  const manifest = await readJson(join(rawRoot, rawManifestName), "raw dist manifest");
  exactFields(
    manifest,
    ["schema_version", "build_sha256", "target", "base", "logical_bytes", "files"],
    "raw dist manifest",
  );
  if (
    manifest.schema_version !== 1
    || manifest.target !== "es2020"
    || manifest.base !== "./"
    || typeof manifest.build_sha256 !== "string"
    || !fullHashPattern.test(manifest.build_sha256)
    || !Array.isArray(manifest.files)
    || manifest.files.length !== 3
  ) {
    fail("raw dist manifest identity is invalid");
  }
  const roles = ["html", "javascript", "stylesheet"];
  const files = [];
  for (let index = 0; index < roles.length; index += 1) {
    const entry = objectValue(manifest.files[index], `raw dist files[${index}]`);
    exactFields(entry, ["role", "path", "sha256", "bytes"], `raw dist files[${index}]`);
    if (
      entry.role !== roles[index]
      || typeof entry.path !== "string"
      || entry.path.length === 0
      || entry.path.startsWith("/")
      || entry.path.split("/").some((part) => part === "" || part === "." || part === "..")
      || typeof entry.sha256 !== "string"
      || !fullHashPattern.test(entry.sha256)
    ) {
      fail(`raw dist files[${index}] is invalid`);
    }
    const bytes = await readFile(join(rawRoot, entry.path));
    if (bytes.length !== byteCount(entry.bytes, `raw dist files[${index}].bytes`)) {
      fail(`raw dist files[${index}] byte length differs`);
    }
    if (sha256(bytes) !== entry.sha256) {
      fail(`raw dist files[${index}] SHA-256 differs`);
    }
    files.push({ role: entry.role, path: entry.path, bytes });
  }
  const expectedMembers = [...files.map((file) => file.path), rawManifestName].sort();
  if (JSON.stringify(await treeFiles(rawRoot)) !== JSON.stringify(expectedMembers)) {
    fail("raw dist contains an undeclared member");
  }
  const logicalBytes = files.reduce((total, file) => total + file.bytes.length, 0);
  if (
    logicalBytes !== byteCount(manifest.logical_bytes, "raw dist logical_bytes")
    || buildIdentity(files) !== manifest.build_sha256
  ) {
    fail("raw dist manifest does not bind its exact content");
  }
  return {
    buildSha256: manifest.build_sha256,
    html: files[0].bytes,
    javascript: files[1].bytes,
    stylesheet: files[2].bytes,
  };
}


function releaseUrl(buildHash, kind) {
  return `./assets/diagnostics-${buildHash}.${kind}`;
}


function releaseHtml(rawHtml, buildHash) {
  const dom = new JSDOM(rawHtml.toString("utf8"));
  const document = dom.window.document;
  const scripts = [...document.querySelectorAll("script")];
  const styles = [...document.querySelectorAll('link[rel="stylesheet"]')];
  if (
    scripts.length !== 1
    || styles.length !== 1
    || scripts[0].textContent.trim() !== ""
    || document.querySelector("style, base") !== null
  ) {
    fail("raw HTML does not have the closed external asset shape");
  }
  scripts[0].setAttribute("src", releaseUrl(buildHash, "js"));
  styles[0].setAttribute("href", releaseUrl(buildHash, "css"));
  for (const element of document.querySelectorAll("[src], [href]")) {
    if (element !== scripts[0] && element !== styles[0]) {
      fail("raw HTML contains an undeclared resource reference");
    }
  }
  return Buffer.from(`${dom.serialize()}\n`, "utf8");
}


function compressedRepresentations(buildHash, kind, raw) {
  const gzip = gzipSync(raw, { level: 9, mtime: 0 });
  const brotli = brotliCompressSync(raw, {
    params: {
      [zlibConstants.BROTLI_PARAM_MODE]: zlibConstants.BROTLI_MODE_TEXT,
      [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
      [zlibConstants.BROTLI_PARAM_SIZE_HINT]: raw.length,
    },
  });
  const mime = kind === "js" ? "text/javascript; charset=utf-8" : "text/css; charset=utf-8";
  return [
    { suffix: "raw", encoding: "raw", contentEncoding: null, bytes: raw },
    { suffix: "gz", encoding: "gzip", contentEncoding: "gzip", bytes: gzip },
    { suffix: "br", encoding: "br", contentEncoding: "br", bytes: brotli },
  ].map((representation) => {
    const fileName = `diagnostics-${buildHash}.${kind}.${representation.suffix}`;
    return {
      fileName,
      path: `${generatedRelative}${fileName}`,
      url: releaseUrl(buildHash, kind),
      kind,
      encoding: representation.encoding,
      contentEncoding: representation.contentEncoding,
      mime,
      cacheControl: immutableCacheControl,
      bytes: representation.bytes,
      sha256: sha256(representation.bytes),
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
  const nodeModules = parts
    .map((part, index) => part === "node_modules" ? index : -1)
    .filter((index) => index >= 0)
    .reverse();
  for (const index of nodeModules) {
    const prefix = parts.slice(0, index).join("/");
    const candidate = `${prefix ? `${prefix}/` : ""}node_modules/${dependency}`;
    if (packages[candidate] !== undefined) {
      return candidate;
    }
  }
  return null;
}


function runtimeLockPaths(packageJson, lock) {
  const packages = objectValue(lock.packages, "package-lock packages");
  const root = objectValue(packages[""], "package-lock root");
  const dependencies = objectValue(packageJson.dependencies, "package.json dependencies");
  if (JSON.stringify(root.dependencies) !== JSON.stringify(dependencies)) {
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
    const entry = objectValue(packages[path], `package-lock ${path}`);
    if (entry.dev === true) {
      fail(`runtime dependency is marked dev-only: ${path}`);
    }
    const dependencyGroups = [
      [entry.dependencies, true],
      [entry.optionalDependencies, false],
      [entry.peerDependencies, true],
    ];
    for (const [rawGroup, required] of dependencyGroups) {
      if (rawGroup === undefined) {
        continue;
      }
      const group = objectValue(rawGroup, `package-lock ${path} dependency group`);
      for (const dependency of Object.keys(group).sort()) {
        const resolved = resolveLockedDependency(packages, path, dependency);
        if (resolved === null) {
          const optionalPeer = entry.peerDependenciesMeta?.[dependency]?.optional === true;
          if (required && !optionalPeer) {
            fail(`package-lock does not resolve ${path} dependency ${dependency}`);
          }
          continue;
        }
        pending.push(resolved);
      }
    }
    pending.sort();
  }
  return [...selected].sort();
}


function normalizedLicense(bytes, label) {
  const text = bytes.toString("utf8");
  if (!Buffer.from(text, "utf8").equals(bytes) || text.includes("\0")) {
    fail(`${label} must be UTF-8 text`);
  }
  return `${text.replace(/\r\n?/g, "\n").trimEnd()}\n`;
}


async function thirdPartyNotices() {
  const packageJson = await readJson(join(projectRoot, "package.json"), "package.json");
  const lock = await readJson(join(projectRoot, "package-lock.json"), "package-lock.json");
  const packages = objectValue(lock.packages, "package-lock packages");
  const notices = [];
  for (const lockedPath of runtimeLockPaths(packageJson, lock)) {
    const directory = join(projectRoot, lockedPath);
    const metadata = await lstat(directory);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
      fail(`installed runtime package is not a regular directory: ${lockedPath}`);
    }
    const installed = await readJson(join(directory, "package.json"), `${lockedPath} package.json`);
    const locked = objectValue(packages[lockedPath], `package-lock ${lockedPath}`);
    const name = stringValue(installed.name, `${lockedPath} name`);
    const version = stringValue(installed.version, `${lockedPath} version`);
    const license = stringValue(installed.license, `${lockedPath} license`);
    if (version !== locked.version) {
      fail(`installed runtime package version differs from lock: ${lockedPath}`);
    }
    const licenseFiles = (await readdir(directory, { withFileTypes: true }))
      .filter((entry) => entry.isFile() && /^(licen[cs]e|copying|notice)(\..*)?$/i.test(entry.name))
      .map((entry) => entry.name)
      .sort(compareText);
    if (licenseFiles.length === 0) {
      fail(`installed runtime package has no license text: ${lockedPath}`);
    }
    const texts = [];
    for (const file of licenseFiles) {
      texts.push({
        file,
        text: normalizedLicense(await readFile(join(directory, file)), `${lockedPath}/${file}`),
      });
    }
    notices.push({ name, version, license, lockedPath, texts });
  }
  notices.sort((left, right) => (
    compareText(left.name, right.name)
    || compareText(left.version, right.version)
    || compareText(left.lockedPath, right.lockedPath)
  ));
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
      lines.push(text.text.trimEnd());
    }
    lines.push("");
  }
  return Buffer.from(`${lines.join("\n").trimEnd()}\n`, "utf8");
}


function manifestFor(buildHash, html, representations, notices) {
  const raw = Object.fromEntries(
    representations.filter((item) => item.encoding === "raw").map((item) => [item.kind, item]),
  );
  const brotli = Object.fromEntries(
    representations.filter((item) => item.encoding === "br").map((item) => [item.kind, item]),
  );
  const logicalBytes = html.length + raw.js.bytes.length + raw.css.bytes.length;
  const firstLoadBrotliBytes = html.length + brotli.js.bytes.length + brotli.css.bytes.length;
  const embeddedBytes = html.length
    + representations.reduce((total, item) => total + item.bytes.length, 0)
    + notices.length;
  if (logicalBytes > maximumLogicalBytes) {
    fail(`release bundle exceeds the ${maximumLogicalBytes}-byte logical budget`);
  }
  if (firstLoadBrotliBytes > maximumFirstLoadBrotliBytes) {
    fail(`release bundle exceeds the ${maximumFirstLoadBrotliBytes}-byte Brotli budget`);
  }
  if (embeddedBytes > maximumEmbeddedBytes) {
    fail(`release bundle exceeds the ${maximumEmbeddedBytes}-byte embedded budget`);
  }
  const htmlText = html.toString("utf8");
  if (!Buffer.from(htmlText, "utf8").equals(html)) {
    fail("release HTML must be UTF-8");
  }
  return {
    schema_version: 1,
    build_sha256: buildHash,
    html: {
      url: "./",
      mime: "text/html; charset=utf-8",
      cache_control: "no-cache",
      sha256: sha256(html),
      bytes: html.length,
      content: htmlText,
    },
    files: representations.map((item) => ({
      path: item.path,
      url: item.url,
      kind: item.kind,
      encoding: item.encoding,
      content_encoding: item.contentEncoding,
      mime: item.mime,
      cache_control: item.cacheControl,
      sha256: item.sha256,
      bytes: item.bytes.length,
    })),
    notices: {
      path: `${generatedRelative}${noticesName}`,
      sha256: sha256(notices),
      bytes: notices.length,
    },
    budgets: {
      logical_uncompressed_bytes: logicalBytes,
      first_load_brotli_bytes: firstLoadBrotliBytes,
      all_embedded_bytes: embeddedBytes,
    },
  };
}


function rustString(value) {
  if (!/^[\x20-\x7e]*$/.test(value)) {
    fail("generated Rust metadata must remain printable ASCII");
  }
  return JSON.stringify(value);
}


function rustByteSlice(bytes) {
  const lines = [];
  for (let offset = 0; offset < bytes.length; offset += 20) {
    lines.push(`    ${[...bytes.subarray(offset, offset + 20)].join(", ")},`);
  }
  return `&[\n${lines.join("\n")}\n]`;
}


function renderRustTable(manifest, html, representations) {
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
    `pub const BUILD_SHA256: &str = ${rustString(manifest.build_sha256)};`,
    `pub const INDEX_HTML_SHA256: &str = ${rustString(manifest.html.sha256)};`,
    `pub const INDEX_HTML_MIME: &str = ${rustString(manifest.html.mime)};`,
    `pub const INDEX_HTML_CACHE_CONTROL: &str = ${rustString(manifest.html.cache_control)};`,
    `pub static INDEX_HTML: &[u8] = ${rustByteSlice(html)};`,
    "",
    `pub const THIRD_PARTY_NOTICES_SHA256: &str = ${rustString(manifest.notices.sha256)};`,
    `pub static THIRD_PARTY_NOTICES: &[u8] = include_bytes!(${rustString(noticesName)});`,
    "",
    "pub static REPRESENTATIONS: &[GeneratedRepresentation] = &[",
  ];
  for (const item of representations) {
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


function canonicalGeneratedPath(value, buildHash, kind, suffix, label) {
  const expected = `${generatedRelative}diagnostics-${buildHash}.${kind}.${suffix}`;
  if (
    value !== expected
    || value.startsWith("/")
    || value.includes("\\")
    || value.split("/").some((part) => part === "" || part === "." || part === "..")
  ) {
    fail(`${label} is not the exact generated repository path`);
  }
  return value.slice(generatedRelative.length);
}


function validateReleaseHtml(html, buildHash) {
  const dom = new JSDOM(html.toString("utf8"));
  const document = dom.window.document;
  const scripts = [...document.querySelectorAll("script")];
  const styles = [...document.querySelectorAll('link[rel="stylesheet"]')];
  if (
    scripts.length !== 1
    || styles.length !== 1
    || scripts[0].getAttribute("src") !== releaseUrl(buildHash, "js")
    || styles[0].getAttribute("href") !== releaseUrl(buildHash, "css")
    || scripts[0].textContent.trim() !== ""
    || document.querySelector("style, base") !== null
  ) {
    fail("release HTML does not use the exact full-hash external resources");
  }
  for (const element of document.querySelectorAll("[src], [href]")) {
    if (element !== scripts[0] && element !== styles[0]) {
      fail("release HTML contains an undeclared resource reference");
    }
  }
}


export async function validateGeneratedTree(root = generatedRoot) {
  const metadata = await lstat(root);
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    fail("generated asset root must be a regular directory");
  }
  const entries = await readdir(root, { withFileTypes: true });
  if (entries.some((entry) => !entry.isFile())) {
    fail("generated asset root contains a non-regular member");
  }
  const manifest = await readJson(join(root, manifestName), "generated manifest");
  exactFields(
    manifest,
    ["schema_version", "build_sha256", "html", "files", "notices", "budgets"],
    "generated manifest",
  );
  if (
    manifest.schema_version !== 1
    || typeof manifest.build_sha256 !== "string"
    || !fullHashPattern.test(manifest.build_sha256)
    || !Array.isArray(manifest.files)
    || manifest.files.length !== 6
  ) {
    fail("generated manifest identity or cardinality is invalid");
  }
  const buildHash = manifest.build_sha256;
  const htmlEntry = objectValue(manifest.html, "generated manifest html");
  exactFields(
    htmlEntry,
    ["url", "mime", "cache_control", "sha256", "bytes", "content"],
    "generated manifest html",
  );
  const html = Buffer.from(stringValue(htmlEntry.content, "generated manifest html.content"), "utf8");
  if (
    htmlEntry.url !== "./"
    || htmlEntry.mime !== "text/html; charset=utf-8"
    || htmlEntry.cache_control !== "no-cache"
    || typeof htmlEntry.sha256 !== "string"
    || !fullHashPattern.test(htmlEntry.sha256)
    || html.length !== byteCount(htmlEntry.bytes, "generated manifest html.bytes")
    || sha256(html) !== htmlEntry.sha256
  ) {
    fail("generated manifest HTML metadata differs from its content");
  }
  validateReleaseHtml(html, buildHash);

  const combinations = [
    ["js", "raw", "raw", null],
    ["js", "gzip", "gz", "gzip"],
    ["js", "br", "br", "br"],
    ["css", "raw", "raw", null],
    ["css", "gzip", "gz", "gzip"],
    ["css", "br", "br", "br"],
  ];
  const representations = [];
  for (let index = 0; index < combinations.length; index += 1) {
    const [kind, encoding, suffix, contentEncoding] = combinations[index];
    const entry = objectValue(manifest.files[index], `generated manifest files[${index}]`);
    exactFields(
      entry,
      [
        "path",
        "url",
        "kind",
        "encoding",
        "content_encoding",
        "mime",
        "cache_control",
        "sha256",
        "bytes",
      ],
      `generated manifest files[${index}]`,
    );
    const fileName = canonicalGeneratedPath(
      entry.path,
      buildHash,
      kind,
      suffix,
      `generated manifest files[${index}].path`,
    );
    const mime = kind === "js" ? "text/javascript; charset=utf-8" : "text/css; charset=utf-8";
    if (
      entry.url !== releaseUrl(buildHash, kind)
      || entry.kind !== kind
      || entry.encoding !== encoding
      || entry.content_encoding !== contentEncoding
      || entry.mime !== mime
      || entry.cache_control !== immutableCacheControl
      || typeof entry.sha256 !== "string"
      || !fullHashPattern.test(entry.sha256)
    ) {
      fail(`generated manifest files[${index}] metadata is invalid`);
    }
    const bytes = await readFile(join(root, fileName));
    if (
      bytes.length !== byteCount(entry.bytes, `generated manifest files[${index}].bytes`)
      || sha256(bytes) !== entry.sha256
    ) {
      fail(`generated manifest files[${index}] content differs`);
    }
    representations.push({
      fileName,
      path: entry.path,
      url: entry.url,
      kind,
      encoding,
      contentEncoding,
      mime,
      cacheControl: immutableCacheControl,
      sha256: entry.sha256,
      bytes,
    });
  }
  const names = [manifestName, rustTableName, noticesName, ...representations.map((item) => item.fileName)].sort();
  if (JSON.stringify(entries.map((entry) => entry.name).sort()) !== JSON.stringify(names)) {
    fail("generated asset root has extra or missing members");
  }
  for (const kind of ["js", "css"]) {
    const raw = representations.find((item) => item.kind === kind && item.encoding === "raw").bytes;
    const gzip = representations.find((item) => item.kind === kind && item.encoding === "gzip").bytes;
    const brotli = representations.find((item) => item.kind === kind && item.encoding === "br").bytes;
    if (!gunzipSync(gzip).equals(raw) || !brotliDecompressSync(brotli).equals(raw)) {
      fail(`generated ${kind} compression does not round-trip`);
    }
    const text = raw.toString("utf8");
    if (/sourceMappingURL|node_modules/i.test(text)) {
      fail(`generated ${kind} contains source-map or toolchain material`);
    }
  }

  const noticesEntry = objectValue(manifest.notices, "generated manifest notices");
  exactFields(noticesEntry, ["path", "sha256", "bytes"], "generated manifest notices");
  const notices = await readFile(join(root, noticesName));
  if (
    noticesEntry.path !== `${generatedRelative}${noticesName}`
    || typeof noticesEntry.sha256 !== "string"
    || !fullHashPattern.test(noticesEntry.sha256)
    || notices.length !== byteCount(noticesEntry.bytes, "generated manifest notices.bytes")
    || sha256(notices) !== noticesEntry.sha256
  ) {
    fail("generated notices metadata differs from its content");
  }
  const budgets = objectValue(manifest.budgets, "generated manifest budgets");
  exactFields(
    budgets,
    ["logical_uncompressed_bytes", "first_load_brotli_bytes", "all_embedded_bytes"],
    "generated manifest budgets",
  );
  const rawBytes = representations
    .filter((item) => item.encoding === "raw")
    .reduce((total, item) => total + item.bytes.length, 0);
  const brotliBytes = representations
    .filter((item) => item.encoding === "br")
    .reduce((total, item) => total + item.bytes.length, 0);
  const allRepresentationBytes = representations.reduce((total, item) => total + item.bytes.length, 0);
  const expectedBudgets = {
    logical_uncompressed_bytes: html.length + rawBytes,
    first_load_brotli_bytes: html.length + brotliBytes,
    all_embedded_bytes: html.length + allRepresentationBytes + notices.length,
  };
  if (JSON.stringify(budgets) !== JSON.stringify(expectedBudgets)) {
    fail("generated manifest budgets differ from the exact members");
  }
  if (
    expectedBudgets.logical_uncompressed_bytes > maximumLogicalBytes
    || expectedBudgets.first_load_brotli_bytes > maximumFirstLoadBrotliBytes
    || expectedBudgets.all_embedded_bytes > maximumEmbeddedBytes
  ) {
    fail("generated asset release budget is exceeded");
  }

  const canonicalManifest = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  if (!(await readFile(join(root, manifestName))).equals(canonicalManifest)) {
    fail("generated manifest JSON is not canonical");
  }
  const rustTable = renderRustTable(manifest, html, representations);
  if (!(await readFile(join(root, rustTableName))).equals(rustTable)) {
    fail("generated Rust include table differs from the manifest");
  }
  return manifest;
}


async function expectedAssets(rawRoot) {
  const raw = await loadRawBundle(rawRoot);
  const html = releaseHtml(raw.html, raw.buildSha256);
  const representations = [
    ...compressedRepresentations(raw.buildSha256, "js", raw.javascript),
    ...compressedRepresentations(raw.buildSha256, "css", raw.stylesheet),
  ];
  const notices = await thirdPartyNotices();
  const manifest = manifestFor(raw.buildSha256, html, representations, notices);
  const assets = new Map();
  for (const item of representations) {
    assets.set(item.fileName, item.bytes);
  }
  assets.set(noticesName, notices);
  assets.set(rustTableName, renderRustTable(manifest, html, representations));
  assets.set(manifestName, Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, "utf8"));
  return { assets, manifest };
}


async function ensureGeneratedRoot() {
  const runtimeRoot = resolve(repositoryRoot, "rust/crates/troupe-diagnostics-runtime");
  if (await realpath(runtimeRoot) !== runtimeRoot) {
    fail("runtime crate root must not use symlink indirection");
  }
  const assetsRoot = join(runtimeRoot, "assets");
  try {
    const metadata = await lstat(assetsRoot);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
      fail("runtime assets root must be a regular directory");
    }
  } catch (error) {
    if (error instanceof AssetGenerationError) {
      throw error;
    }
    if (error.code !== "ENOENT") {
      throw error;
    }
    await mkdir(assetsRoot);
  }
  try {
    const metadata = await lstat(generatedRoot);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
      fail("generated asset root must be a regular directory");
    }
  } catch (error) {
    if (error instanceof AssetGenerationError) {
      throw error;
    }
    if (error.code !== "ENOENT") {
      throw error;
    }
    await mkdir(generatedRoot);
  }
}


async function publishAssets(expected) {
  await ensureGeneratedRoot();
  const existing = await readdir(generatedRoot, { withFileTypes: true });
  for (const entry of existing) {
    if (
      !entry.isFile()
      || (![manifestName, rustTableName, noticesName].includes(entry.name)
        && !generatedMemberPattern.test(entry.name))
    ) {
      fail(`refusing to replace unknown generated asset member: ${entry.name}`);
    }
  }
  for (const [name, bytes] of expected) {
    if (name !== manifestName) {
      await writeFile(join(generatedRoot, name), bytes);
    }
  }
  await writeFile(join(generatedRoot, manifestName), expected.get(manifestName));
  for (const entry of existing) {
    if (generatedMemberPattern.test(entry.name) && !expected.has(entry.name)) {
      await rm(join(generatedRoot, entry.name));
    }
  }
  await validateGeneratedTree(generatedRoot);
}


async function checkAssets(expected) {
  await validateGeneratedTree(generatedRoot);
  for (const [name, bytes] of expected) {
    if (!(await readFile(join(generatedRoot, name))).equals(bytes)) {
      fail(`checked-in generated asset differs: ${name}`);
    }
  }
}


async function main() {
  const options = parseArguments(process.argv.slice(2));
  const gateRoot = await requireGateRoot();
  const rawRoot = join(gateRoot, `generated-assets-raw-${generationAttempt()}`);
  runRawBuild(rawRoot);
  const { assets, manifest } = await expectedAssets(rawRoot);
  if (options.check) {
    await checkAssets(assets);
  } else {
    await publishAssets(assets);
  }
  process.stdout.write(
    `generated diagnostics assets ${manifest.build_sha256} `
    + `(${manifest.budgets.all_embedded_bytes} embedded bytes)\n`,
  );
}


const directInvocation = process.argv[1] !== undefined
  && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (directInvocation) {
  try {
    await main();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`diagnostics asset generation: ${message}\n`);
    process.exitCode = 1;
  }
}
