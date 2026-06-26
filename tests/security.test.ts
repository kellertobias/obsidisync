import test from "node:test";
import assert from "node:assert/strict";
import { assertGitBranch, assertNamespaceSlug, assertSafeVaultPath, assertSecureHttpUrl } from "../src/security";

test("assertSafeVaultPath accepts normal vault-relative paths", () => {
  assert.equal(assertSafeVaultPath("Folder/Note.md"), "Folder/Note.md");
});

test("assertSafeVaultPath rejects traversal, absolute, git, and backslash paths", () => {
  for (const path of ["../Note.md", "/Note.md", "Folder/../Note.md", ".git/config", "Folder\\Note.md", ""]) {
    assert.throws(() => assertSafeVaultPath(path), /Unsafe vault path/);
  }
});

test("assertSecureHttpUrl permits HTTPS and local HTTP only", () => {
  assert.doesNotThrow(() => assertSecureHttpUrl("https://sync.example.com", "Sync server URL"));
  assert.doesNotThrow(() => assertSecureHttpUrl("http://localhost:3000", "Sync server URL"));
  assert.doesNotThrow(() => assertSecureHttpUrl("http://127.0.0.1:3000", "Sync server URL"));
  assert.throws(() => assertSecureHttpUrl("http://sync.example.com", "Sync server URL"), /must use HTTPS/);
});

test("namespace and branch validation rejects unsafe values", () => {
  assert.doesNotThrow(() => assertNamespaceSlug("alice.work-1", "User namespace"));
  assert.throws(() => assertNamespaceSlug("../alice", "User namespace"), /must contain only/);

  assert.doesNotThrow(() => assertGitBranch("main"));
  assert.doesNotThrow(() => assertGitBranch("feature/mobile-sync"));
  assert.throws(() => assertGitBranch("feature/../main"), /Branch name is invalid/);
  assert.throws(() => assertGitBranch("main lock"), /Branch name is invalid/);
});
