import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  describeDevicePassword,
  deviceUrl,
  devicePasswordsAvailabilityMessage,
  normalizeDeviceFolder,
  serverSupportsDevicePasswords,
  serverSupportsSaber,
  webdavUrl
} from "../src/devicePasswords";

const root = process.cwd();

test("device folders are normalized to safe vault-relative paths", () => {
  assert.equal(normalizeDeviceFolder(" /Tablet/Notes/ "), "Tablet/Notes");
  assert.equal(normalizeDeviceFolder("Tablet\\Notes"), "Tablet/Notes");
  assert.throws(() => normalizeDeviceFolder(""), /Choose a folder/);
  assert.throws(() => normalizeDeviceFolder("/"), /Choose a folder/);
  assert.throws(() => normalizeDeviceFolder("../outside"), /Unsafe vault path/);
  assert.throws(() => normalizeDeviceFolder(".git/hooks"), /Unsafe vault path/);
  assert.throws(() => normalizeDeviceFolder(".obsidian-git-sync/x"), /not synced/);
  assert.throws(() => normalizeDeviceFolder(".trash/notes"), /not synced/);
  assert.throws(() => normalizeDeviceFolder("ObsidiSync History"), /not synced/);
});

test("webdav urls combine the server url with the server-provided path", () => {
  assert.equal(webdavUrl("https://sync.example.com/", "/dav/notes/Tablet/"), "https://sync.example.com/dav/notes/Tablet/");
  assert.equal(webdavUrl("https://sync.example.com", "dav/notes/Tablet/"), "https://sync.example.com/dav/notes/Tablet/");
});

test("device password descriptions include url and usage", () => {
  const description = describeDevicePassword(
    {
      id: "abc",
      label: "Boox",
      vault: "notes",
      folder: "Tablet",
      username: "alice",
      webdavPath: "/dav/notes/Tablet/",
      createdAt: "2026-09-02T10:00:00Z",
      lastUsedAt: null
    },
    "https://sync.example.com"
  );
  assert.match(description, /^https:\/\/sync\.example\.com\/dav\/notes\/Tablet\/ · created /);
  assert.match(description, /last used never$/);
});

test("saber devices are described by their nextcloud login and pdf folder", () => {
  const entry = {
    id: "abc",
    label: "Saber on iPad",
    vault: "notes",
    folder: "Saber/Sync",
    username: "alice",
    webdavPath: "/dav/notes/Saber/Sync/",
    createdAt: "2026-09-02T10:00:00Z",
    lastUsedAt: null,
    kind: "saber" as const,
    pdfFolder: "Saber"
  };
  assert.equal(deviceUrl(entry, "https://sync.example.com/"), "https://sync.example.com");
  const description = describeDevicePassword(entry, "https://sync.example.com");
  assert.match(description, /^Saber app \(Nextcloud login at https:\/\/sync\.example\.com\) · syncs to Saber\/Sync · PDFs in Saber · /);
  assert.match(describeDevicePassword({ ...entry, pdfFolder: undefined }, "https://sync.example.com"), /encrypted files only/);
  assert.equal(serverSupportsSaber({ features: ["webdavDevicePasswords", "saberNextcloud"] }), true);
  assert.equal(serverSupportsSaber({ features: ["webdavDevicePasswords"] }), false);
});

test("device passwords are managed from settings and shown once after creation", () => {
  const settings = readFileSync(join(root, "src", "settings.ts"), "utf8");
  assert.match(settings, /Device passwords \(WebDAV\)/);
  assert.match(settings, /this\.plugin\.openDevicePasswordsModal\(\)/);

  const modal = readFileSync(join(root, "src", "devicePasswordsModal.ts"), "utf8");
  assert.match(modal, /The password is shown only once/);
  assert.match(modal, /this\.renderCopyRow\(container, "Password", created\.password\)/);
  assert.match(modal, /setButtonText\("Revoke"\)/);

  const service = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  assert.match(service, /async createDevicePassword\(label: string, folder: string\): Promise<CreatedDevicePassword>/);
  assert.match(service, /device-passwords\/\$\{encodeURIComponent\(id\)\}/);
});

test("device passwords are only offered when the server advertises the feature", () => {
  assert.equal(serverSupportsDevicePasswords({ features: ["webdavDevicePasswords"] }), true);
  assert.equal(serverSupportsDevicePasswords({ features: [] }), false);
  assert.equal(serverSupportsDevicePasswords({}), false);

  // Unknown until the first server check: do not claim it is missing.
  assert.equal(devicePasswordsAvailabilityMessage({ lastServerCheckAt: null, serverVersion: null, serverFeatures: [] }), null);
  assert.equal(
    devicePasswordsAvailabilityMessage({ lastServerCheckAt: "2026-09-02T10:00:00Z", serverVersion: "0.5.0", serverFeatures: ["webdavDevicePasswords"] }),
    null
  );
  const message = devicePasswordsAvailabilityMessage({ lastServerCheckAt: "2026-09-02T10:00:00Z", serverVersion: "0.4.0", serverFeatures: [] });
  assert.match(message ?? "", /too old for device passwords/);
  assert.match(message ?? "", /version 0\.4\.0/);
});

test("the modal checks server support before offering device passwords", () => {
  const modal = readFileSync(join(root, "src", "devicePasswordsModal.ts"), "utf8");
  assert.match(modal, /await this\.gitService\.devicePasswordsUnavailableReason\(\)/);

  const service = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  assert.match(service, /this\.settings\.serverFeatures = Array\.isArray\(info\.features\)/);
  assert.match(service, /error\.status === 404 && !this\.settings\.serverFeatures\.includes\("webdavDevicePasswords"\)/);
});
