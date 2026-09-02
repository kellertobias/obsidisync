import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  describeDownloadProgress,
  hashesByPath,
  removeManifestEntry,
  serverFileAlreadyLocal,
  serverSupportsFileReferences,
  upsertManifestEntry
} from "../src/serverFiles";

const root = process.cwd();

test("file references are used only when the server advertises them", () => {
  assert.equal(serverSupportsFileReferences(["syncFileReferences"]), true);
  assert.equal(serverSupportsFileReferences([]), false);
  assert.equal(serverSupportsFileReferences(undefined), false);
});

test("files already on disk with the same hash are skipped", () => {
  const local = hashesByPath([{ path: "Tablet/a.pdf", sha256: "aaa", mtime: 1, size: 3 }]);
  assert.equal(serverFileAlreadyLocal({ op: "upsert", path: "Tablet/a.pdf", sha256: "aaa" }, local), true);
  assert.equal(serverFileAlreadyLocal({ op: "upsert", path: "Tablet/a.pdf", sha256: "bbb" }, local), false);
  assert.equal(serverFileAlreadyLocal({ op: "upsert", path: "Tablet/new.pdf", sha256: "aaa" }, local), false);
  assert.equal(serverFileAlreadyLocal({ op: "delete", path: "Tablet/a.pdf" }, local), false);
  assert.equal(serverFileAlreadyLocal({ op: "upsert", path: "Tablet/a.pdf", sha256: "aaa" }, undefined), false);
});

test("manifest progress entries are replaced or removed in place", () => {
  const manifest = [{ path: "a.md", sha256: "1", mtime: 1, size: 1 }];
  const added = upsertManifestEntry(manifest, { path: "b.md", sha256: "2", mtime: 2, size: 2 });
  assert.equal(added.length, 2);
  const replaced = upsertManifestEntry(added, { path: "a.md", sha256: "9", mtime: 9, size: 9 });
  assert.equal(replaced.length, 2);
  assert.equal(replaced.find((entry) => entry.path === "a.md")?.sha256, "9");
  assert.deepEqual(removeManifestEntry(replaced, "a.md").map((entry) => entry.path), ["b.md"]);
  assert.equal(manifest.length, 1, "inputs are not mutated");
});

test("download progress names the file", () => {
  assert.equal(describeDownloadProgress(3, 40, "Tablet/Notes/Meeting.pdf"), "ObsidiSync: downloading 3/40 - Meeting.pdf");
});

test("sync paths request references, download per file, and persist progress", () => {
  const service = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  assert.equal((service.match(/fileContent: this\.fileContentMode\(\)/g) ?? []).length, 4, "sync, force push, probe, resolve");
  assert.match(service, /download: \(file\) => this\.downloadServerFile\(file, serverHead\)/);
  assert.match(service, /if \(actual !== file\.sha256\)/);
  assert.match(service, /appliedSinceSave >= DOWNLOAD_PROGRESS_SAVE_EVERY/);
  assert.doesNotMatch(service, /vaultState\.applyServerFiles\(response\.files\)/);

  const vaultState = readFileSync(join(root, "src", "vaultState.ts"), "utf8");
  assert.match(vaultState, /serverFileAlreadyLocal\(file, options\.localHashes\)/);
  assert.match(vaultState, /throw new Error\(`Server sent no content for \$\{safePath\}`\)/);
});
