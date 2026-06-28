import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("OIDC device polling preserves the requested audience resource", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const pollingSection = serviceSource.slice(
    serviceSource.indexOf("async pollOidcDeviceLogin"),
    serviceSource.indexOf("  private async register")
  );

  assert.match(pollingSection, /const config = await this\.serverOidcLoginConfig\(\)/);
  assert.match(pollingSection, /params\.set\("client_id", config\.clientId\)/);
  assert.match(pollingSection, /params\.set\("audience", config\.audience\)/);
  assert.match(pollingSection, /params\.set\("resource", config\.audience\)/);
});

test("OIDC login modals can open the verification URL", () => {
  const authSource = readFileSync(join(root, "src", "authLoginModal.ts"), "utf8");
  const oidcSource = readFileSync(join(root, "src", "oidcModal.ts"), "utf8");

  assert.match(authSource, /setButtonText\("Open URL"\)/);
  assert.match(authSource, /window\.open\(url, "_blank"\)/);
  assert.match(oidcSource, /setButtonText\("Open URL"\)/);
  assert.match(oidcSource, /window\.open\(url, "_blank"\)/);
});
