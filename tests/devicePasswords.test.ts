import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describeDevicePassword, normalizeDeviceFolder, webdavUrl } from "../src/devicePasswords";

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
