import { DevicePasswordEntry } from "./protocol";
import { shouldIgnoreVaultPath } from "./ignore";
import { assertSafeVaultPath } from "./security";

export const DEFAULT_DEVICE_FOLDER = "Tablet";

/**
 * Turns user input into the vault-relative folder a device password grants access to.
 * Mirrors the server's `validate_device_folder`: no vault root, no traversal, no server metadata.
 */
export function normalizeDeviceFolder(input: string): string {
  const folder = input.trim().replace(/\\/g, "/").replace(/^\/+|\/+$/g, "");
  if (!folder) throw new Error("Choose a folder inside the vault, for example Tablet/Notes");
  const safe = assertSafeVaultPath(folder);
  if (safe === ".obsidian-git-sync" || safe.startsWith(".obsidian-git-sync/") || shouldIgnoreVaultPath(safe)) {
    throw new Error("That folder is not synced by ObsidiSync; choose a regular vault folder");
  }
  return safe;
}

export function webdavUrl(serverUrl: string, webdavPath: string): string {
  const base = serverUrl.trim().replace(/\/+$/, "");
  const path = webdavPath.startsWith("/") ? webdavPath : `/${webdavPath}`;
  return `${base}${path}`;
}

export function describeDevicePassword(entry: DevicePasswordEntry, serverUrl: string): string {
  const created = formatDate(entry.createdAt);
  const lastUsed = entry.lastUsedAt ? formatDate(entry.lastUsedAt) : "never";
  return `${webdavUrl(serverUrl, entry.webdavPath)} · created ${created} · last used ${lastUsed}`;
}

function formatDate(value: string): string {
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? value : new Date(parsed).toLocaleString();
}
