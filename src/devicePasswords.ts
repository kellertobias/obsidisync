import { DevicePasswordEntry, ServerInfoResponse } from "./protocol";
import { shouldIgnoreVaultPath } from "./ignore";
import { assertSafeVaultPath } from "./security";

export const DEFAULT_DEVICE_FOLDER = "Tablet";
export const DEVICE_PASSWORDS_FEATURE = "webdavDevicePasswords";
/** Servers that emulate Nextcloud for the Saber handwriting app. */
export const SABER_NEXTCLOUD_FEATURE = "saberNextcloud";

export function serverSupportsDevicePasswords(info: Pick<ServerInfoResponse, "features">): boolean {
  return Array.isArray(info.features) && info.features.includes(DEVICE_PASSWORDS_FEATURE);
}

export function serverSupportsSaber(info: Pick<ServerInfoResponse, "features">): boolean {
  return Array.isArray(info.features) && info.features.includes(SABER_NEXTCLOUD_FEATURE);
}

/** Saber devices log in through the server's Nextcloud emulation, not the `/dav/` tree. */
export function deviceUrl(entry: DevicePasswordEntry, serverUrl: string): string {
  if (entry.kind === "saber") return serverUrl.trim().replace(/\/+$/, "");
  return webdavUrl(serverUrl, entry.webdavPath);
}

/**
 * Explains why device passwords cannot be used yet, or `null` when they can.
 * Before the first server check nothing is known, so the feature is offered normally.
 */
export function devicePasswordsAvailabilityMessage(settings: {
  lastServerCheckAt: string | null;
  serverVersion: string | null;
  serverFeatures: string[];
}): string | null {
  if (!settings.lastServerCheckAt) return null;
  if (serverSupportsDevicePasswords({ features: settings.serverFeatures })) return null;
  const version = settings.serverVersion ? ` (it reports version ${settings.serverVersion})` : "";
  return `Not available: the sync server${version} is too old for device passwords. Update the server, then check the connection again.`;
}

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
  if (entry.kind === "saber") {
    const pdfs = entry.pdfFolder ? `PDFs in ${entry.pdfFolder}` : "encrypted files only, no PDFs";
    return `Saber app (Nextcloud login at ${deviceUrl(entry, serverUrl)}) · syncs to ${entry.folder} · ${pdfs} · created ${created} · last used ${lastUsed}`;
  }
  return `${webdavUrl(serverUrl, entry.webdavPath)} · created ${created} · last used ${lastUsed}`;
}

function formatDate(value: string): string {
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? value : new Date(parsed).toLocaleString();
}
