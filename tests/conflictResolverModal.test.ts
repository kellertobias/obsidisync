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

test("conflict resolver asks the server for conflicts it still expects this device to resolve", () => {
  assert.match(source, /await this\.loadPendingFromServer\(\);/);
  assert.match(source, /await this\.gitService\.pendingConflicts\(\)/);
  assert.match(gitSource, /async pendingConflicts\(\): Promise<SyncConflict\[\]>/);
  assert.match(gitSource, /if \(error instanceof HttpStatusError && error\.status === 404\) return \[\];/);
});

test("conflict resolver collects one choice per file and pushes them with a single Resolve button", () => {
  assert.match(source, /private choices = new Map<string, FileChoice>\(\);/);
  assert.match(source, /toggle\(`Keep \$\{sideLabel\(conflict\.parsed, "server"\)\}`, \{ kind: "server" \}\);/);
  assert.match(source, /toggle\(`Keep \$\{sideLabel\(conflict\.parsed, "local"\)\}`, \{ kind: "local" \}\);/);
  assert.match(source, /toggle\("Use current content", \{ kind: "current" \}\);/);
  assert.match(source, /toggle\("Delete on server", \{ kind: "delete" \}\);/);
  assert.match(source, /toggle\("Restore server version", \{ kind: "restore" \}\);/);
  assert.match(source, /`Resolve \$\{selected\} file\$\{selected === 1 \? "" : "s"\}`/);
  assert.match(source, /const remaining = await this\.gitService\.resolveConflicts\(resolutions\);/);
});

test("merge editor stores the merged text as the file's choice instead of pushing immediately", () => {
  assert.match(source, /"Use this merge"/);
  assert.match(source, /this\.setChoice\(conflict, allServer \? \{ kind: "server" \} : allLocal \? \{ kind: "local" \} : \{ kind: "custom", content \}\);/);
  assert.doesNotMatch(source, /void this\.resolveCustom\(/);
});

test("conflict resolver drops server-reported paths once their resolution was accepted", () => {
  assert.match(source, /this\.reported\.delete\(path\);/);
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

test("sync does not record conflict-marker files as synced", () => {
  assert.match(gitSource, /persistManifest: response\.status !== "conflict"/);
  assert.match(gitSource, /onApplied: !persistManifest \? undefined : async/);
});

test("resolving only marks the pushed files as synced", () => {
  assert.match(gitSource, /const entry = resolution\.kind === "delete" \? null : await vaultState\.manifestEntryFor\(resolution\.path\);/);
  assert.doesNotMatch(gitSource.slice(gitSource.indexOf("async resolveConflicts"), gitSource.indexOf("async pendingConflicts")), /computeManifest\(\)/);
  assert.match(gitSource, /files\.push\(\{ path: resolution\.path, delete: true \}\);/);
});

test("resolving refuses to silently no-op while a sync is running", () => {
  assert.match(gitSource, /throw new Error\("A sync is running\. Wait for it to finish, then try again\."\);/);
});

test("automatic sync does not force a dismissed resolver back open", () => {
  assert.match(mainSource, /this\.openConflictResolver\(conflicts, \{ explicit: false \}\)/);
  assert.match(mainSource, /if \(!options\.explicit && key !== null && key === this\.dismissedConflictKey\) \{\s*return;/);
  assert.match(mainSource, /this\.dismissedConflictKey = conflictKey\(remainingPaths\);/);
});
