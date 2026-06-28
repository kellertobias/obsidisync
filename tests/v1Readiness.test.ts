import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin records persistent sync health and compatibility state", () => {
  const settingsSource = readFileSync(join(root, "src", "settings.ts"), "utf8");
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const protocolSource = readFileSync(join(root, "src", "protocol.ts"), "utf8");

  assert.match(protocolSource, /interface ServerInfoResponse/);
  assert.match(settingsSource, /lastSyncAttemptAt: string \| null/);
  assert.match(settingsSource, /lastSyncCompletedAt: string \| null/);
  assert.match(settingsSource, /lastSyncError: string \| null/);
  assert.match(settingsSource, /syncStatus: "idle" \| "running" \| "queued" \| "error"/);
  assert.match(settingsSource, /serverVersion: string \| null/);
  assert.match(settingsSource, /oidcRefreshToken: string/);
  assert.match(settingsSource, /oidcAccessTokenExpiresAt: string \| null/);
  assert.match(serviceSource, /CLIENT_API_VERSION = 1/);
  assert.match(serviceSource, /checkServerCompatibility\(\): Promise<ServerInfoResponse>/);
  assert.match(serviceSource, /refreshOidcAccessToken\(\): Promise<boolean>/);
  assert.match(serviceSource, /grant_type", "refresh_token"/);
  assert.match(serviceSource, /\/v1\/server\/info/);
  assert.match(serviceSource, /Login expired or unauthorized\. Log in to Obsync again\./);
  assert.match(settingsSource, /setButtonText\("Check"\)/);
});

test("plugin queues overlapping sync requests and summarizes local changes", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");

  assert.match(serviceSource, /private syncQueued = false/);
  assert.match(serviceSource, /Git sync queued/);
  assert.match(serviceSource, /localChangeSummary\(\): Promise<\{ changed: number; upserts: number; deletes: number \}>/);
  assert.match(serviceSource, /diffManifests\(manifest, this\.settings\.localManifest\)/);
  assert.match(mainSource, /changed file/);
});

test("sidebar exposes conflict resolution when current file has conflict markers", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");

  assert.match(viewSource, /hasConflict: boolean/);
  assert.match(viewSource, /openConflictResolver\(\): void/);
  assert.match(viewSource, /aria-label": "Resolve conflicts"/);
  assert.match(viewSource, /hasConflictMarkers\(await this\.app\.vault\.cachedRead\(file\)\)/);
  assert.match(mainSource, /openConflictResolver: \(\) => this\.openConflictResolver\(\)/);
  assert.match(mainSource, /refreshFileHistoryViews/);
});

test("readme documents v1 operations", () => {
  const readme = readFileSync(join(root, "README.md"), "utf8");

  assert.match(readme, /Release checklist/);
  assert.match(readme, /Backup and restore/);
  assert.match(readme, /Server maintenance/);
  assert.match(readme, /Sync status and recovery/);
  assert.match(readme, /\/v1\/server\/info/);
});
