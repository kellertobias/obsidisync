import { ManifestEntry } from "./protocol";

export interface ManifestDiff {
  upsertPaths: string[];
  deletePaths: string[];
}

export function diffManifests(current: ManifestEntry[], previous: ManifestEntry[]): ManifestDiff {
  const currentByPath = new Map(current.map((entry) => [entry.path, entry]));
  const previousByPath = new Map(previous.map((entry) => [entry.path, entry]));
  const upsertPaths: string[] = [];
  const deletePaths: string[] = [];

  for (const entry of current) {
    const previousEntry = previousByPath.get(entry.path);
    if (!previousEntry || previousEntry.sha256 !== entry.sha256 || previousEntry.size !== entry.size) {
      upsertPaths.push(entry.path);
    }
  }

  for (const entry of previous) {
    if (!currentByPath.has(entry.path)) {
      deletePaths.push(entry.path);
    }
  }

  return {
    upsertPaths: upsertPaths.sort(),
    deletePaths: deletePaths.sort()
  };
}
