import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin treats Git remote URL as optional", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const settingsSource = readFileSync(join(root, "src", "settings.ts"), "utf8");

  assert.doesNotMatch(serviceSource, /Set a Git remote URL before registering this vault/);
  assert.match(serviceSource, /this\.settings\.remoteUrl &&/);
  assert.doesNotMatch(settingsSource, /setName\("Git remote URL"\)/);
});

test("plugin hides the branch setting and registers main", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const settingsSource = readFileSync(join(root, "src", "settings.ts"), "utf8");
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");

  assert.doesNotMatch(settingsSource, /setName\("Branch"\)/);
  assert.match(serviceSource, /const MAIN_BRANCH = "main"/);
  assert.match(serviceSource, /branch: MAIN_BRANCH/);
  assert.match(mainSource, /this\.settings\.branch = DEFAULT_SETTINGS\.branch/);
});
