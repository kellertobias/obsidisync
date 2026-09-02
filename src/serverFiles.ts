import { ManifestEntry, ServerFileChange } from "./protocol";

export const FILE_REFERENCES_FEATURE = "syncFileReferences";

export function serverSupportsFileReferences(features: string[] | undefined): boolean {
  return Array.isArray(features) && features.includes(FILE_REFERENCES_FEATURE);
}

export function hashesByPath(manifest: ManifestEntry[]): Map<string, string> {
  return new Map(manifest.map((entry) => [entry.path, entry.sha256]));
}

/** A file whose bytes are already on disk with the same hash needs no download and no write. */
export function serverFileAlreadyLocal(file: ServerFileChange, localHashes: Map<string, string> | undefined): boolean {
  return file.op === "upsert" && localHashes?.get(file.path) === file.sha256;
}

/** Records one applied server file in a manifest so an interrupted sync resumes instead of restarting. */
export function upsertManifestEntry(manifest: ManifestEntry[], entry: ManifestEntry): ManifestEntry[] {
  const index = manifest.findIndex((existing) => existing.path === entry.path);
  if (index === -1) return [...manifest, entry];
  const next = manifest.slice();
  next[index] = entry;
  return next;
}

export function removeManifestEntry(manifest: ManifestEntry[], path: string): ManifestEntry[] {
  return manifest.filter((entry) => entry.path !== path);
}

export function describeDownloadProgress(done: number, total: number, path: string): string {
  const name = path.split("/").pop() ?? path;
  return `ObsidiSync: downloading ${done}/${total} - ${name}`;
}
