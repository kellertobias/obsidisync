import test from "node:test";
import assert from "node:assert/strict";
import { diffManifests } from "../src/manifest";
import { ManifestEntry } from "../src/protocol";

function entry(path: string, sha256: string, size = 1): ManifestEntry {
  return { path, sha256, size, mtime: 1000 };
}

test("diffManifests detects sorted upserts and deletes", () => {
  const previous = [entry("same.md", "aaa"), entry("deleted.md", "bbb"), entry("changed.md", "old")];
  const current = [entry("changed.md", "new"), entry("added.md", "ccc"), entry("same.md", "aaa")];

  assert.deepEqual(diffManifests(current, previous), {
    upsertPaths: ["added.md", "changed.md"],
    deletePaths: ["deleted.md"]
  });
});

test("diffManifests treats size changes as upserts even when hashes match", () => {
  assert.deepEqual(diffManifests([entry("note.md", "aaa", 2)], [entry("note.md", "aaa", 1)]), {
    upsertPaths: ["note.md"],
    deletePaths: []
  });
});
