import { normalizePath, TFile, Vault } from "obsidian";
import { arrayBufferToBase64, base64ToArrayBuffer } from "./base64";
import { shouldIgnoreVaultPath } from "./ignore";
import { diffManifests } from "./manifest";
import { ClientChange, ManifestEntry, ServerFileChange } from "./protocol";
import { assertSafeVaultPath } from "./security";
import { serverFileAlreadyLocal } from "./serverFiles";

export interface CollectedVaultChanges {
  manifest: ManifestEntry[];
  changes: ClientChange[];
}

export interface CollectChangesOptions {
  stageUpload?: (path: string, buffer: ArrayBuffer, entry: ManifestEntry) => Promise<string>;
}

export type ServerUpsert = Extract<ServerFileChange, { op: "upsert" }>;

export interface ApplyServerFilesOptions {
  /** Fetches the bytes of a file the server sent without inline content. */
  download?: (file: ServerUpsert) => Promise<ArrayBuffer>;
  /** sha256 of files already on disk; matching upserts are skipped without touching the file. */
  localHashes?: Map<string, string>;
  onProgress?: (done: number, total: number, path: string) => void;
  /** Called after every file that was written or deleted, so progress can be persisted. */
  onApplied?: (change: { path: string; entry: ManifestEntry | null }) => Promise<void>;
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

  /** Manifest entry for one file as it is on disk right now, or null when it does not exist. */
  async manifestEntryFor(path: string): Promise<ManifestEntry | null> {
    const safePath = assertSafeVaultPath(path);
    const stat = await this.vault.adapter.stat(normalizePath(safePath));
    if (!stat || stat.type !== "file") return null;
    const buffer = await this.vault.adapter.readBinary(normalizePath(safePath));
    return { path: safePath, sha256: await sha256Hex(buffer), mtime: stat.mtime, size: buffer.byteLength };
  }

  async backupTo(folder: string): Promise<number> {
    const target = normalizePath(folder).replace(/\/+$/, "");
    if (!target) throw new Error("Backup folder must not be empty");

    const manifest = await this.computeManifest();
    for (const entry of manifest) {
      const buffer = await this.vault.adapter.readBinary(entry.path);
      const destination = `${target}/${entry.path}`;
      await this.ensureParentFolder(destination);
      await this.vault.adapter.writeBinary(destination, buffer);
    }
    return manifest.length;
  }

  async deletePaths(paths: string[]): Promise<void> {
    for (const path of paths) {
      const safePath = assertSafeVaultPath(path);
      if (shouldIgnoreVaultPath(safePath)) continue;
      const normalizedPath = normalizePath(safePath);
      if (await this.vault.adapter.exists(normalizedPath, true)) {
        await this.vault.adapter.remove(normalizedPath);
      }
    }
  }

  /**
   * Writes server changes to disk one file at a time. Files arrive either inline (base64) or as
   * references that are downloaded on demand, so memory use stays bounded by the largest file
   * rather than by the vault. Returns the number of files written or deleted.
   */
  async applyServerFiles(files: ServerFileChange[], options: ApplyServerFilesOptions = {}): Promise<number> {
    let applied = 0;
    for (const [index, file] of files.entries()) {
      const safePath = assertSafeVaultPath(file.path);
      if (shouldIgnoreVaultPath(safePath)) continue;
      const normalizedPath = normalizePath(safePath);
      options.onProgress?.(index + 1, files.length, safePath);

      if (file.op === "delete") {
        if (await this.vault.adapter.exists(normalizedPath, true)) {
          await this.vault.adapter.remove(normalizedPath);
        }
        applied += 1;
        await options.onApplied?.({ path: safePath, entry: null });
        continue;
      }

      if (serverFileAlreadyLocal(file, options.localHashes) && (await this.vault.adapter.exists(normalizedPath, true))) {
        continue;
      }

      let buffer: ArrayBuffer;
      if (typeof file.contentBase64 === "string") {
        buffer = base64ToArrayBuffer(file.contentBase64);
      } else if (options.download) {
        buffer = await options.download(file);
      } else {
        throw new Error(`Server sent no content for ${safePath}`);
      }

      await this.ensureParentFolder(normalizedPath);
      await this.vault.adapter.writeBinary(normalizedPath, buffer);
      applied += 1;
      if (options.onApplied) {
        const stat = await this.vault.adapter.stat(normalizedPath);
        await options.onApplied({
          path: safePath,
          entry: { path: safePath, sha256: file.sha256, mtime: stat?.mtime ?? Date.now(), size: buffer.byteLength }
        });
      }
    }
    return applied;
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
