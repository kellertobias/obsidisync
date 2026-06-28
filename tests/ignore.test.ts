import test from "node:test";
import assert from "node:assert/strict";
import { shouldIgnoreVaultPath } from "../src/ignore";

test("ignores git internals and plugin-owned state", () => {
  for (const path of [
    ".git/config",
    ".obsidian-git-sync/state.json",
    "ObsidiSync History/abc123-Note.md",
    ".trash/deleted.md",
    ".obsidian/cache/index.json",
    ".obsidian/plugins/ios-git-sync/main.js"
  ]) {
    assert.equal(shouldIgnoreVaultPath(path), true, path);
  }
});

test("ignores desktop and mobile workspace files while keeping regular notes", () => {
  assert.equal(shouldIgnoreVaultPath(".obsidian/workspace.json"), true);
  assert.equal(shouldIgnoreVaultPath(".obsidian/workspace-mobile.json"), true);
  assert.equal(shouldIgnoreVaultPath("Notes/workspace-mobile.json"), false);
  assert.equal(shouldIgnoreVaultPath("Notes/today.md"), false);
});
