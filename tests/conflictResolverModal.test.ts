import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("conflict resolver scans the vault using the shared conflict-marker detector", () => {
  const source = readFileSync(join(root, "src", "conflictResolverModal.ts"), "utf8");

  assert.match(source, /if \(!hasConflictMarkers\(content\)\) return;/);
});

test("merge conflict editor puts version buttons under their text previews", () => {
  const source = readFileSync(join(root, "src", "conflictResolverModal.ts"), "utf8");

  assert.match(source, /const server = this\.renderPreview\(grid, "Server", hunk\.server\)/);
  assert.match(source, /this\.createStackButton\(server, "Use server version", useServer\)/);
  assert.match(source, /const local = this\.renderPreview\(grid, "Local", hunk\.local\)/);
  assert.match(source, /this\.createStackButton\(local, "Use local version", useLocal\)/);
  assert.doesNotMatch(source, /this\.createStackButton\(controls, "Use server version"/);
  assert.doesNotMatch(source, /this\.createStackButton\(controls, "Use local version"/);
});

test("merge conflict editor directly applies selected or edited versions", () => {
  const source = readFileSync(join(root, "src", "conflictResolverModal.ts"), "utf8");

  assert.match(source, /this\.createStackButton\(editedActions, "Use edited version"/);
  assert.match(source, /void this\.resolveCustom\(path, parsed, resolutions\)/);
  assert.match(source, /setChoice\("server", hunk\.server\);\s*void this\.resolveCustom\(path, parsed, resolutions\)/);
  assert.match(source, /setChoice\("local", hunk\.local\);\s*void this\.resolveCustom\(path, parsed, resolutions\)/);
  assert.match(source, /editedActions\.style\.display = "none"/);
  assert.match(source, /textarea\.oninput = \(\) => \{/);
  assert.match(source, /editedActions\.style\.display = "block"/);
});

test("conflict resolver shows progress while pushing a resolution", () => {
  const source = readFileSync(join(root, "src", "conflictResolverModal.ts"), "utf8");

  assert.match(source, /this\.renderProgress\(path, "Pushing resolution\.\.\."\)/);
  assert.match(source, /private renderProgress\(path: string, message: string\): void/);
  assert.match(source, /contentEl\.empty\(\)/);
  assert.match(source, /this\.statusEl = contentEl\.createEl\("p", \{ text: message \}\)/);
});
