import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin stages file upserts through chunked upload endpoints", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const vaultStateSource = readFileSync(join(root, "src", "vaultState.ts"), "utf8");

  assert.match(serviceSource, /\/uploads`/);
  assert.match(serviceSource, /\/uploads\/\$\{encodeURIComponent\(init\.uploadId\)\}\/chunk/);
  assert.match(serviceSource, /\/uploads\/\$\{encodeURIComponent\(init\.uploadId\)\}\/complete/);
  assert.match(vaultStateSource, /stageUpload/);
  assert.match(vaultStateSource, /uploadId/);
});
