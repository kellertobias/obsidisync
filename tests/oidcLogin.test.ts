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
