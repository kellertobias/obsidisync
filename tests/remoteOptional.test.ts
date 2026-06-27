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
  assert.match(settingsSource, /Leave blank to keep a server-local Git repository/);
});
