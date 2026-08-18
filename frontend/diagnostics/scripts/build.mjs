#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  lstat,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
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

import { JSDOM } from "jsdom";
import { build } from "vite";


const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(projectRoot, "..", "..");
const configPath = join(projectRoot, "vite.config.ts");
const manifestName = "raw-dist-manifest.json";
const maximumLogicalBytes = 512 * 1024;
const assetReferencePattern = /^\.\/assets\/diagnostics-([A-Za-z0-9_-]{8,64})\.(js|css)$/;
const allowedNamespaceUrls = new Set([
  "http://www.w3.org/1998/Math/MathML",
  "http://www.w3.org/1999/xhtml",
  "http://www.w3.org/1999/xlink",
  "http://www.w3.org/2000/svg",
  "http://www.w3.org/2000/xmlns/",
  "http://www.w3.org/XML/1998/namespace",
]);


class BuildContractError extends Error {}


function fail(message) {
  throw new BuildContractError(message);
}


function parseArguments(argv) {
  if (argv.length !== 2 || argv[0] !== "--out-dir") {
    fail("usage: build.mjs --out-dir <absolute-path>");
  }
  return argv[1];
}


function isWithin(path, parent) {
  const remainder = relative(parent, path);
  return remainder === "" || (remainder !== ".." && !remainder.startsWith(`..${sep}`));
}


async function requireGateRoot() {
  const raw = process.env.TROUPE_GATE_TMP;
  if (raw === undefined || raw.length === 0 || !isAbsolute(raw) || resolve(raw) !== raw) {
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


async function requireOutputPath(raw, gateRoot) {
  if (!isAbsolute(raw) || resolve(raw) !== raw) {
    fail("--out-dir must be a canonical absolute path");
  }
  if (dirname(raw) !== gateRoot || basename(raw).length === 0) {
    fail("--out-dir must be a direct child of TROUPE_GATE_TMP");
  }
  try {
    await lstat(raw);
    fail("--out-dir must not already exist");
  } catch (error) {
    if (error instanceof BuildContractError) {
      throw error;
    }
    if (error.code !== "ENOENT") {
      throw error;
    }
  }
  return raw;
}


function exactTarget(target) {
  return target === "es2020"
    || (Array.isArray(target) && target.length === 1 && target[0] === "es2020");
}


function validateResolvedConfig(config, output) {
  const buildConfig = config.build;
  const rollupOutput = buildConfig.rollupOptions.output;
  if (config.base !== "./") {
    fail("Vite base must be exactly ./");
  }
  if (!exactTarget(buildConfig.target)) {
    fail("Vite target must be exactly es2020");
  }
  if (
    buildConfig.cssCodeSplit !== false
    || buildConfig.sourcemap !== false
    || buildConfig.modulePreload !== false
    || buildConfig.manifest !== false
    || buildConfig.ssrManifest !== false
  ) {
    fail("Vite production output flags violate the raw bundle contract");
  }
  if (
    rollupOutput === undefined
    || Array.isArray(rollupOutput)
    || rollupOutput.inlineDynamicImports !== true
    || rollupOutput.entryFileNames !== "assets/diagnostics-[hash].js"
    || rollupOutput.assetFileNames !== "assets/diagnostics-[hash][extname]"
  ) {
    fail("Vite Rollup output must remain one content-hashed entry");
  }
  if (resolve(buildConfig.outDir) !== output || buildConfig.emptyOutDir !== false) {
    fail("Vite output directory is not the validated invocation directory");
  }
}


async function treeFiles(root, prefix = "") {
  const result = [];
  for (const entry of await readdir(join(root, prefix), { withFileTypes: true })) {
    const member = prefix ? join(prefix, entry.name) : entry.name;
    if (entry.isDirectory()) {
      result.push(...await treeFiles(root, member));
    } else if (entry.isFile()) {
      result.push(member.split(sep).join("/"));
    } else {
      fail(`raw bundle contains a non-regular member: ${member}`);
    }
  }
  return result.sort();
}


function requireAssetReference(value, extension) {
  const match = assetReferencePattern.exec(value);
  if (match === null || match[2] !== extension || value.includes("?") || value.includes("#")) {
    fail(`HTML contains a non-relative or non-hashed ${extension} reference`);
  }
  return value.slice(2);
}


function requireEmptyFavicon(document) {
  const favicons = [...document.querySelectorAll('link[rel="icon"]')];
  if (favicons.length !== 1 || favicons[0].getAttribute("href") !== "data:,") {
    fail("HTML must contain exactly one inert data favicon");
  }
  return favicons[0];
}


function validateHtml(html, expectedScript, expectedStyle) {
  const dom = new JSDOM(html);
  const document = dom.window.document;
  const scripts = [...document.querySelectorAll("script")];
  const stylesheets = [...document.querySelectorAll('link[rel="stylesheet"]')];
  const favicon = requireEmptyFavicon(document);
  if (scripts.length !== 1 || stylesheets.length !== 1) {
    fail("HTML must contain exactly one script and one stylesheet");
  }
  const script = scripts[0];
  const stylesheet = stylesheets[0];
  if (
    script.getAttribute("type") !== "module"
    || script.textContent.trim() !== ""
    || document.querySelector("style, base") !== null
  ) {
    fail("HTML contains inline executable/style content or a base override");
  }
  const scriptPath = requireAssetReference(script.getAttribute("src") ?? "", "js");
  const stylePath = requireAssetReference(stylesheet.getAttribute("href") ?? "", "css");
  if (scriptPath !== expectedScript || stylePath !== expectedStyle) {
    fail("HTML asset references do not match the emitted bundle members");
  }
  for (const element of document.querySelectorAll("*")) {
    for (const attribute of element.getAttributeNames()) {
      if (attribute.toLowerCase().startsWith("on")) {
        fail("HTML contains an inline event handler");
      }
    }
  }
  for (const element of document.querySelectorAll("[src], [href]")) {
    if (element !== script && element !== stylesheet && element !== favicon) {
      fail("HTML contains an undeclared resource reference");
    }
  }
}


function validateCss(css) {
  if (/@import\b|@font-face\b/i.test(css)) {
    fail("CSS contains an imported stylesheet or external font declaration");
  }
  for (const match of css.matchAll(/url\(\s*([^)]+?)\s*\)/gi)) {
    const value = match[1].replace(/^["']|["']$/g, "").trim();
    if (!value.startsWith("data:")) {
      fail("CSS contains a non-embedded resource URL");
    }
  }
}


function validateJavaScript(javascript) {
  if (/\bimport\s*\(|sourceMappingURL/i.test(javascript)) {
    fail("JavaScript contains a dynamic import or source-map reference");
  }
  for (const match of javascript.matchAll(/https?:\/\/[^\s"'`\\)]+/g)) {
    if (!allowedNamespaceUrls.has(match[0])) {
      fail(`JavaScript contains an external URL: ${match[0]}`);
    }
  }
}


function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
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


async function auditOutput(output) {
  const members = await treeFiles(output);
  const scripts = members.filter((member) => member.endsWith(".js"));
  const styles = members.filter((member) => member.endsWith(".css"));
  if (
    members.length !== 3
    || !members.includes("index.html")
    || scripts.length !== 1
    || styles.length !== 1
  ) {
    fail("raw bundle must contain exactly one HTML, one JavaScript, and one CSS file");
  }
  if (members.some((member) => member.endsWith(".map"))) {
    fail("raw bundle contains a source map");
  }

  const scriptPath = scripts[0];
  const stylePath = styles[0];
  const ordered = ["index.html", scriptPath, stylePath];
  const files = [];
  for (const path of ordered) {
    const bytes = await readFile(join(output, path));
    files.push({ path, bytes });
  }
  validateHtml(files[0].bytes.toString("utf8"), scriptPath, stylePath);
  validateJavaScript(files[1].bytes.toString("utf8"));
  validateCss(files[2].bytes.toString("utf8"));

  const logicalBytes = files.reduce((total, file) => total + file.bytes.length, 0);
  if (logicalBytes > maximumLogicalBytes) {
    fail(`raw bundle exceeds the ${maximumLogicalBytes}-byte logical budget`);
  }
  return {
    schema_version: 1,
    build_sha256: buildIdentity(files),
    target: "es2020",
    base: "./",
    logical_bytes: logicalBytes,
    files: files.map((file, index) => ({
      role: ["html", "javascript", "stylesheet"][index],
      path: file.path,
      sha256: sha256(file.bytes),
      bytes: file.bytes.length,
    })),
  };
}


async function main() {
  const gateRoot = await requireGateRoot();
  const output = await requireOutputPath(parseArguments(process.argv.slice(2)), gateRoot);
  let succeeded = false;
  try {
    await build({
      root: projectRoot,
      configFile: configPath,
      mode: "production",
      build: {
        outDir: output,
        emptyOutDir: false,
      },
      plugins: [{
        name: "troupe-raw-bundle-contract",
        enforce: "post",
        configResolved: (config) => validateResolvedConfig(config, output),
      }],
    });
    const manifest = await auditOutput(output);
    await writeFile(join(output, manifestName), `${JSON.stringify(manifest, null, 2)}\n`, {
      encoding: "utf8",
      flag: "wx",
    });
    process.stdout.write(
      `raw diagnostics bundle ${manifest.build_sha256} (${manifest.logical_bytes} bytes)\n`,
    );
    succeeded = true;
  } finally {
    if (!succeeded) {
      await rm(output, { recursive: true, force: true });
    }
  }
}


try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`diagnostics frontend build: ${message}\n`);
  process.exitCode = 1;
}
