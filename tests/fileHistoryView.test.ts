import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin registers a dockable Obsync file history view", () => {
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /extends ItemView/);
  assert.match(viewSource, /FILE_HISTORY_VIEW_TYPE = "obsync-file-history"/);
  assert.match(viewSource, /getDisplayText\(\): string \{\s*return "Obsync file history";/);
  assert.match(mainSource, /registerView\(FILE_HISTORY_VIEW_TYPE/);
  assert.match(mainSource, /getLeavesOfType\(FILE_HISTORY_VIEW_TYPE\)/);
  assert.match(mainSource, /addRibbonIcon\("history", "Open file history"/);
  assert.doesNotMatch(mainSource, /FileVersionsModal/);
});

test("file history view opens versions read-only in rendered or markdown mode", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /gitService\.history\(this\.filePath\)/);
  assert.match(viewSource, /gitService\.fileAtVersion\(this\.filePath, entry\.hash\)/);
  assert.match(viewSource, /MarkdownRenderer\.render/);
  assert.match(viewSource, /"Rendered"/);
  assert.match(viewSource, /"Markdown"/);
  assert.match(viewSource, /textarea\.readOnly = true/);
  assert.match(viewSource, /navigator\.clipboard\.writeText/);
  assert.doesNotMatch(viewSource, /Replace current file/);
  assert.doesNotMatch(viewSource, /Restore binary file/);
  assert.doesNotMatch(viewSource, /vault\.adapter\.write/);
});
