import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("Obsidian manifest enables iOS, iPadOS, and macOS plugin loading", () => {
  const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));

  assert.equal(manifest.isDesktopOnly, false);
  assert.equal(typeof manifest.minAppVersion, "string");
  assert.equal(manifest.name, "ObsidiSync");
  assert.match(manifest.description, /mobile/i);
  assert.match(manifest.description, /desktop/i);
});

test("plugin source does not import desktop-only Node or Electron APIs", () => {
  const forbidden = [
    /from ["']electron["']/,
    /from ["'](?:fs|path|os|child_process|worker_threads)["']/,
    /require\([\"'](?:electron|fs|path|os|child_process|worker_threads)[\"']\)/,
    /\bBuffer\b/
  ];

  for (const file of sourceFiles(join(root, "src"))) {
    const source = readFileSync(file, "utf8");
    for (const pattern of forbidden) {
      assert.doesNotMatch(source, pattern, file + " matches " + pattern);
    }
  }
});

test("bundler keeps Obsidian desktop modules external instead of bundling them into mobile runtime", () => {
  const config = readFileSync(join(root, "esbuild.config.mjs"), "utf8");

  assert.match(config, /entryPoints:\s*\[\"src\/main\.ts\"\]/);
  assert.match(config, /format:\s*\"cjs\"/);
  assert.match(config, /target:\s*\"es2018\"/);
  assert.match(config, /"obsidian"/);
  assert.match(config, /"electron"/);
});

test("package entry points at the generated Obsidian plugin bundle", () => {
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));

  assert.equal(pkg.main, "main.js");
  assert.equal(existsSync(join(root, "src", "main.ts")), true);
});

function sourceFiles(directory: string): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(directory)) {
    const fullPath = join(directory, entry);
    if (statSync(fullPath).isDirectory()) {
      files.push(...sourceFiles(fullPath));
    } else if (fullPath.endsWith(".ts")) {
      files.push(fullPath);
    }
  }
  return files;
}
