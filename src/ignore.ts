export function shouldIgnoreVaultPath(path: string): boolean {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  return (
    normalized === ".git" ||
    normalized.startsWith(".git/") ||
    normalized === ".obsidian-git-sync" ||
    normalized.startsWith(".obsidian-git-sync/") ||
    normalized === ".trash" ||
    normalized.startsWith(".trash/") ||
    normalized === ".obsidian/workspace.json" ||
    normalized === ".obsidian/workspace-mobile.json" ||
    normalized.startsWith(".obsidian/cache/") ||
    normalized.startsWith(".obsidian/plugins/ios-git-sync/")
  );
}
