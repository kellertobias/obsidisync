import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin exposes a dedicated computer name command and setting", () => {
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");
  const modalSource = readFileSync(join(root, "src", "computerNameModal.ts"), "utf8");
  const settingsSource = readFileSync(join(root, "src", "settings.ts"), "utf8");

  assert.match(mainSource, /ComputerNameModal/);
  assert.match(mainSource, /id: "set-computer-name"/);
  assert.match(mainSource, /name: "Set computer name"/);
  assert.match(mainSource, /this\.settings\.deviceName = name/);
  assert.match(settingsSource, /setName\("Computer name"\)/);
  assert.match(settingsSource, /source computer in sync history/);
  assert.match(modalSource, /class ComputerNameModal extends Modal/);
  assert.match(modalSource, /setPlaceholder\("Keller MacBook"\)/);
  assert.match(modalSource, /Computer name set to/);
});

test("sync uses the configured computer name as the device name", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const runtimeSource = readFileSync(join(root, "src", "runtime.ts"), "utf8");

  assert.match(serviceSource, /deviceName: this\.deviceName\(\)/);
  assert.match(serviceSource, /return getDeviceName\(this\.settings\.deviceName\)/);
  assert.match(runtimeSource, /const configured = configuredDeviceName\?\.trim\(\)/);
  assert.match(runtimeSource, /if \(configured\) return configured/);
});

test("settings persist local history snapshot references", () => {
  const settingsSource = readFileSync(join(root, "src", "settings.ts"), "utf8");

  assert.match(settingsSource, /historySnapshots: HistorySnapshotEntry\[\]/);
  assert.match(settingsSource, /snapshotPath: string/);
  assert.match(settingsSource, /sourcePath: string/);
  assert.match(settingsSource, /historySnapshots: \[\]/);
});
