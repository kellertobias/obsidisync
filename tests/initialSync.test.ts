import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("first sync prompts for a direction instead of merging", () => {
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");

  assert.match(mainSource, /InitialSyncModal/);
  assert.match(mainSource, /needsInitialSyncSetup\(\)/);
  // The gate only fires before the vault has ever synced and once credentials exist.
  assert.match(mainSource, /this\.settings\.serverHead === null/);
  assert.match(mainSource, /!this\.settings\.initialSyncDone/);
  // Sync defers to the modal when setup is still pending.
  assert.match(mainSource, /if \(this\.needsInitialSyncSetup\(\)\) \{\s*this\.openInitialSyncModal\(\);/);
});

test("the setup modal offers force push and overwrite with a double confirmation", () => {
  const modalSource = readFileSync(join(root, "src", "initialSyncModal.ts"), "utf8");

  assert.match(modalSource, /class InitialSyncModal extends Modal/);
  assert.match(modalSource, /Force push/);
  assert.match(modalSource, /Overwrite local/);
  // Force push must ask a second time before destroying server data.
  assert.match(modalSource, /renderForcePushConfirm/);
  assert.match(modalSource, /Yes, overwrite the server/);
  // Overwrite local must ask where the backup goes.
  assert.match(modalSource, /renderBackupPrompt/);
  assert.match(modalSource, /setName\("Backup folder"\)/);
});

test("the service implements force push and server overwrite with a local backup", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const vaultStateSource = readFileSync(join(root, "src", "vaultState.ts"), "utf8");

  assert.match(serviceSource, /async forcePushLocal\(\)/);
  assert.match(serviceSource, /async overwriteLocalFromServer\(/);
  assert.match(serviceSource, /private async probeServerState\(\)/);
  // The probe must not mutate the server: empty changes and an empty client manifest.
  assert.match(serviceSource, /changes: \[\],\s*clientManifest: \[\]/);
  // Force push passes the real server head as the base so the server takes client content.
  assert.match(serviceSource, /baseHead: probe\.serverHead/);
  assert.match(serviceSource, /this\.settings\.initialSyncDone = true/);
  // Overwrite backs up before replacing local files.
  assert.match(vaultStateSource, /async backupTo\(/);
  assert.match(vaultStateSource, /async deletePaths\(/);
});
