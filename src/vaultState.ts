import { normalizePath, TFile, Vault } from "obsidian";
import { arrayBufferToBase64, base64ToArrayBuffer } from "./base64";
import { shouldIgnoreVaultPath } from "./ignore";
import { diffManifests } from "./manifest";
import { ClientChange, ManifestEntry, ServerFileChange } from "./protocol";
import { assertSafeVaultPath } from "./security";

export interface CollectedVaultChanges {
  manifest: ManifestEntry[];
  changes: ClientChange[];
}

export interface CollectChangesOptions {
  stageUpload?: (path: string, buffer: ArrayBuffer, entry: ManifestEntry) => Promise<string>;
}

export class VaultState {
  constructor(private readonly vault: Vault) {}

  async collectChanges(previousManifest: ManifestEntry[], options: CollectChangesOptions = {}): Promise<CollectedVaultChanges> {
    const manifest = await this.computeManifest();
    const diff = diffManifests(manifest, previousManifest.filter((entry) => !shouldIgnoreVaultPath(entry.path)));
    const changes: ClientChange[] = [];

    for (const path of diff.upsertPaths) {
      const buffer = await this.vault.adapter.readBinary(path);
      const entry = manifest.find((manifestEntry) => manifestEntry.path === path);
      if (!entry) continue;
      if (options.stageUpload) {
        const uploadId = await options.stageUpload(path, buffer, entry);
        changes.push({
          path,
          op: "upsert",
          uploadId,
          sha256: entry.sha256,
          mtime: entry.mtime
        });
        continue;
      }
      changes.push({
        path,
        op: "upsert",
        contentBase64: arrayBufferToBase64(buffer),
        sha256: entry.sha256,
        mtime: entry.mtime
      });
    }

    for (const path of diff.deletePaths) {
      changes.push({ path, op: "delete" });
    }

    return { manifest, changes };
  }

  async computeManifest(): Promise<ManifestEntry[]> {
    const entries: ManifestEntry[] = [];
    const files = this.vault
      .getFiles()
      .filter((file) => !shouldIgnoreVaultPath(file.path))
      .sort((left, right) => left.path.localeCompare(right.path));

    for (const file of files) {
      const buffer = await this.vault.adapter.readBinary(file.path);
      entries.push({
        path: file.path,
        sha256: await sha256Hex(buffer),
        mtime: file.stat.mtime,
        size: file.stat.size
      });
    }

    return entries;
  }

  async applyServerFiles(files: ServerFileChange[]): Promise<void> {
    for (const file of files) {
      const safePath = assertSafeVaultPath(file.path);
      if (shouldIgnoreVaultPath(safePath)) continue;
      const normalizedPath = normalizePath(safePath);
      if (file.op === "delete") {
        if (await this.vault.adapter.exists(normalizedPath, true)) {
          await this.vault.adapter.remove(normalizedPath);
        }
        continue;
      }

      await this.ensureParentFolder(normalizedPath);
      await this.vault.adapter.writeBinary(normalizedPath, base64ToArrayBuffer(file.contentBase64));
    }
  }

  private async ensureParentFolder(path: string): Promise<void> {
    const index = path.lastIndexOf("/");
    if (index === -1) return;
    const parts = path.slice(0, index).split("/");
    let current = "";
    for (const part of parts) {
      current = current ? `${current}/${part}` : part;
      if (!(await this.vault.adapter.exists(current, true))) {
        await this.vault.adapter.mkdir(current);
      }
    }
  }
}

export async function sha256Hex(buffer: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", buffer);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
