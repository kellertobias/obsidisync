import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();
const source = readFileSync(join(root, "src", "conflictResolverModal.ts"), "utf8");
const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");
const gitSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");

test("conflict resolver scans the vault using the shared conflict-marker detector", () => {
  assert.match(source, /if \(!hasConflictMarkers\(content\)\) return;/);
});

test("conflict resolver drops server-reported paths once their resolution was accepted", () => {
  // Previously the server's initial list was re-merged on every reload, so a resolved file was
  // listed forever and the dialog could never close on its own.
  assert.match(source, /for \(const path of paths\) this\.reported\.delete\(path\);/);
  assert.match(source, /for \(const conflict of remaining\) this\.reported\.set\(conflict\.path, conflict\.reason\);/);
  assert.match(source, /if \(this\.conflicts\.length === 0\) \{\s*new Notice\("All sync conflicts resolved"\);\s*this\.close\(\);/);
});

test("conflict resolver parses server-reported files with git-style markers", () => {
  assert.match(source, /parseConflictDocument\(await this\.app\.vault\.cachedRead\(file\), \{ allowGenericMarkers: true \}\)/);
});

test("every conflict resolver screen offers a way out", () => {
  assert.match(source, /private renderError\(paths: string\[\], error: unknown, retry: \(\) => void\): void/);
  assert.match(source, /this\.createButton\(actions, "Retry", retry, \{ primary: true \}\)/);
  assert.match(source, /this\.createButton\(actions, "Back to list", \(\) => void this\.reloadAndList\(\), \{ plain: true \}\)/);
  assert.match(source, /this\.createButton\(footer, "Close", \(\) => this\.close\(\), \{ plain: true \}\)/);
});

test("conflict resolver offers bulk and per-file server/local choices for multiple conflicts", () => {
  assert.match(source, /"Use server version for all"/);
  assert.match(source, /"Use local version for all"/);
  assert.match(source, /this\.createButton\(actions, "Server", \(\) => void this\.resolveWholeFile\(conflict, "server", \{ returnTo: "list" \}\)\)/);
  assert.match(source, /this\.createButton\(actions, "Local", \(\) => void this\.resolveWholeFile\(conflict, "local", \{ returnTo: "list" \}\)\)/);
  assert.match(source, /this\.createButton\(actions, "Merge…", \(\) => this\.renderFile\(index\), \{ primary: true \}\)/);
  assert.match(source, /File \$\{index \+ 1\} of \$\{total\}/);
});

test("conflict resolver can resolve a locally deleted file by deleting it on the server", () => {
  assert.match(source, /this\.createButton\(actions, "Delete on server", \(\) => void this\.apply\(\[\{ path: conflict\.path, kind: "delete" \}\], \{ returnTo: "list" \}\)\)/);
  assert.match(gitSource, /files\.push\(\{ path: resolution\.path, delete: true \}\);/);
});

test("resolving refuses to silently no-op while a sync is running", () => {
  assert.match(gitSource, /throw new Error\("A sync is running\. Wait for it to finish, then try again\."\);/);
});

test("merge editor keeps per-change selections until the result is applied", () => {
  assert.match(source, /const setChoice = \(choice: Side\) => \{/);
  assert.match(source, /textarea\.oninput = \(\) => \{\s*resolution\.choice = "custom";/);
  assert.match(source, /parsed\.hunks\.length === 1 \? "Apply selection" : "Apply merged result"/);
});

test("automatic sync does not force a dismissed resolver back open", () => {
  assert.match(mainSource, /this\.openConflictResolver\(conflicts, \{ explicit: false \}\)/);
  assert.match(mainSource, /if \(!options\.explicit && key !== null && key === this\.dismissedConflictKey\) \{\s*return;/);
  assert.match(mainSource, /this\.dismissedConflictKey = conflictKey\(remainingPaths\);/);
});
