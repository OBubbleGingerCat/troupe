// @vitest-environment node

import { createHash } from "node:crypto";
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import {
  afterAll,
  describe,
  expect,
  it,
} from "vitest";


interface GeneratedManifest {
  readonly schema_version: number;
  readonly build_sha256: string;
  readonly html: {
    readonly content: string;
    readonly bytes: number;
  };
  readonly files: Array<Record<string, unknown>>;
  readonly notices: {
    readonly bytes: number;
  };
  readonly budgets: {
    readonly logical_uncompressed_bytes: number;
    readonly first_load_brotli_bytes: number;
    readonly all_embedded_bytes: number;
  };
}

interface GeneratorModule {
  readonly validateGeneratedTree: (root?: string) => Promise<unknown>;
}


const frontendRoot = resolve(process.cwd());
const repositoryRoot = resolve(frontendRoot, "..", "..");
const generatedRoot = resolve(
  repositoryRoot,
  "rust/crates/troupe-diagnostics-runtime/assets/generated",
);
const generatorPath = resolve(frontendRoot, "scripts/generate_assets.mjs");
const temporaryRoots: string[] = [];


function sha256(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}


function gateRoot(): string {
  const value = process.env.TROUPE_GATE_TMP;
  if (value === undefined || value.length === 0) {
    throw new Error("generated asset tests require TROUPE_GATE_TMP");
  }
  return value;
}


async function generator(): Promise<GeneratorModule> {
  return await import(/* @vite-ignore */ pathToFileURL(generatorPath).href) as GeneratorModule;
}


function manifestAt(root: string): GeneratedManifest {
  return JSON.parse(readFileSync(join(root, "manifest.json"), "utf8")) as GeneratedManifest;
}


function writeManifest(root: string, manifest: GeneratedManifest): void {
  writeFileSync(join(root, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
}


function copiedTree(): string {
  const parent = mkdtempSync(join(gateRoot(), "generated-assets-negative-"));
  temporaryRoots.push(parent);
  const target = join(parent, "generated");
  cpSync(generatedRoot, target, { recursive: true, errorOnExist: true });
  return target;
}


function firstFile(manifest: GeneratedManifest): Record<string, unknown> {
  const file = manifest.files[0];
  if (file === undefined) {
    throw new Error("generated manifest has no first file");
  }
  return file;
}


afterAll(() => {
  for (const root of temporaryRoots) {
    rmSync(root, { recursive: true, force: true });
  }
});


describe("checked-in generated diagnostics assets", () => {
  it("binds the exact full-hash representations, notices, and release budgets", async () => {
    const module = await generator();
    await module.validateGeneratedTree(generatedRoot);
    const manifest = manifestAt(generatedRoot);

    expect(manifest.schema_version).toBe(1);
    expect(manifest.build_sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(manifest.files).toHaveLength(6);
    const combinations = manifest.files.map((file) => `${file.kind}:${file.encoding}`);
    expect(combinations).toEqual([
      "js:raw",
      "js:gzip",
      "js:br",
      "css:raw",
      "css:gzip",
      "css:br",
    ]);
    for (const file of manifest.files) {
      expect(file.path).toBe(
        `rust/crates/troupe-diagnostics-runtime/assets/generated/diagnostics-${manifest.build_sha256}.${file.kind}.${file.encoding === "gzip" ? "gz" : file.encoding}`,
      );
      expect(file.url).toBe(`./assets/diagnostics-${manifest.build_sha256}.${file.kind}`);
      const bytes = readFileSync(resolve(repositoryRoot, String(file.path)));
      expect(bytes).toHaveLength(Number(file.bytes));
      expect(sha256(bytes)).toBe(file.sha256);
    }

    expect(manifest.html.content).toContain(
      `src="./assets/diagnostics-${manifest.build_sha256}.js"`,
    );
    expect(manifest.html.content).toContain(
      `href="./assets/diagnostics-${manifest.build_sha256}.css"`,
    );
    expect(manifest.html.content).toContain('rel="icon" href="data:,"');
    expect(manifest.html.content).not.toMatch(/<(?:style|base)(?:\s|>)/i);
    expect(Buffer.byteLength(manifest.html.content)).toBe(manifest.html.bytes);
    expect(manifest.budgets.logical_uncompressed_bytes).toBeLessThanOrEqual(512 * 1024);
    expect(manifest.budgets.first_load_brotli_bytes).toBeLessThanOrEqual(160 * 1024);
    expect(manifest.budgets.all_embedded_bytes).toBeLessThanOrEqual(768 * 1024);

    const notices = readFileSync(join(generatedRoot, "third-party-notices.txt"), "utf8");
    for (const dependency of [
      "@preact/signals 2.11.1",
      "@preact/signals-core 1.14.4",
      "lucide-preact 1.31.0",
      "preact 10.29.8",
      "uplot 1.6.32",
    ]) {
      expect(notices).toContain(dependency);
    }
    expect(notices).not.toMatch(/\n(?:vite|typescript|vitest|@playwright\/test) [0-9]/i);
    expect(Buffer.byteLength(notices)).toBe(manifest.notices.bytes);

    const rustTable = readFileSync(join(generatedRoot, "assets.rs"), "utf8");
    expect(rustTable.match(/include_bytes!/g)).toHaveLength(7);
    expect(rustTable).not.toMatch(/std::fs|Command::new|flate2|brotli::/i);
    expect(existsSync(resolve(frontendRoot, "dist"))).toBe(false);
  });

  it.each([
    "extra",
    "missing",
    "traversal",
    "symlink",
    "field",
    "cardinality",
    "hash",
  ] as const)("rejects %s generated-tree drift", async (change) => {
    const root = copiedTree();
    const manifest = manifestAt(root);
    const first = firstFile(manifest);
    const firstPath = join(root, basename(String(first.path)));
    switch (change) {
      case "extra":
        copyFileSync(firstPath, join(root, "unexpected.bin"));
        break;
      case "missing":
        unlinkSync(firstPath);
        break;
      case "traversal":
        first.path = "../outside.js.raw";
        writeManifest(root, manifest);
        break;
      case "symlink": {
        const second = manifest.files[1];
        if (second === undefined) {
          throw new Error("generated manifest has no second file");
        }
        unlinkSync(firstPath);
        symlinkSync(basename(String(second.path)), firstPath);
        break;
      }
      case "field":
        first.unexpected = true;
        writeManifest(root, manifest);
        break;
      case "cardinality":
        manifest.files.pop();
        writeManifest(root, manifest);
        break;
      case "hash":
        first.sha256 = "0".repeat(64);
        writeManifest(root, manifest);
        break;
    }

    const module = await generator();
    await expect(module.validateGeneratedTree(root)).rejects.toThrow();
  });
});
