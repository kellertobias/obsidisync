export function assertSafeVaultPath(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  if (!normalized || normalized.includes("\0") || path.includes("\\") || path.startsWith("/") || normalized !== path) {
    throw new Error(`Unsafe vault path: ${path}`);
  }

  const parts = normalized.split("/");
  for (const part of parts) {
    if (!part || part === "." || part === ".." || part === ".git") {
      throw new Error(`Unsafe vault path: ${path}`);
    }
  }
  return normalized;
}

export function assertNamespaceSlug(value: string, label: string): void {
  if (!value || value.length > 96 || value.startsWith(".") || !/^[A-Za-z0-9_.-]+$/.test(value)) {
    throw new Error(`${label} must contain only letters, numbers, dot, dash, or underscore`);
  }
}

export function assertSecureHttpUrl(rawUrl: string, label: string): void {
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    throw new Error(`${label} must be a valid URL`);
  }

  if (url.protocol === "https:") return;
  if (url.protocol === "http:" && isLocalhost(url.hostname)) return;
  throw new Error(`${label} must use HTTPS, except localhost during development`);
}

export function assertGitBranch(branch: string): void {
  if (
    !branch ||
    branch.length > 255 ||
    branch.startsWith("-") ||
    branch.startsWith("/") ||
    branch.endsWith("/") ||
    branch.endsWith(".") ||
    branch.endsWith(".lock") ||
    branch.includes("//") ||
    branch.includes("..") ||
    branch.includes("@{") ||
    branch.includes("\\") ||
    /\s|[\0-\x1f\x7f]/.test(branch) ||
    !/^[A-Za-z0-9._/-]+$/.test(branch) ||
    branch.split("/").some((part) => !part || part === "." || part === ".." || part.endsWith(".lock"))
  ) {
    throw new Error("Branch name is invalid");
  }
}

function isLocalhost(hostname: string): boolean {
  const host = hostname.toLowerCase().replace(/^\[/, "").replace(/\]$/, "");
  return host === "localhost" || host === "127.0.0.1" || host === "::1";
}
