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
  assert.match(mainSource, /activeFilePath/);
  assert.match(mainSource, /view\.showFile\(activeFilePath\)/);
  assert.match(mainSource, /addRibbonIcon\("history", "Open file history"/);
  assert.doesNotMatch(mainSource, /FileVersionsModal/);
});

test("file history view shows sync status, last save, source device, and sync action", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(serviceSource, /currentDeviceName\(\): string/);
  assert.match(viewSource, /sha256Hex/);
  assert.match(viewSource, /"Up to date"/);
  assert.match(viewSource, /"Local changes not synced"/);
  assert.match(viewSource, /"Last saved"/);
  assert.match(viewSource, /"Source"/);
  assert.match(viewSource, /gitService\.sync\(\)/);
  assert.match(viewSource, /extractSyncDevice/);
  assert.doesNotMatch(viewSource, /"Refresh history"/);
  assert.doesNotMatch(viewSource, /renderHeader/);
  assert.doesNotMatch(viewSource, /"Synced versions"/);
});

test("file history view opens selected versions with the regular Obsidian file UI", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /gitService\.history\(this\.filePath\)/);
  assert.match(viewSource, /gitService\.fileAtVersion\(this\.filePath, entry\.hash\)/);
  assert.match(viewSource, /HISTORY_SNAPSHOT_DIR/);
  assert.match(viewSource, /createBinary\(path, content\)/);
  assert.match(viewSource, /modifyBinary\(existing, content\)/);
  assert.match(viewSource, /adapter\.exists\(path, true\)/);
  assert.match(viewSource, /openVersion\(entry, index \+ 1\)/);
  assert.match(viewSource, /snapshotPath\(this\.filePath \?\? "version", entry, source\.device, versionNumber, attempt\)/);
  assert.match(viewSource, /`\$\{HISTORY_SNAPSHOT_DIR\}\/Version \$\{version\} - \$\{date\} - \$\{computer\} - \$\{title\}\$\{suffix\}\$\{extension\}`/);
  assert.match(viewSource, /formatSnapshotDate/);
  assert.match(viewSource, /snapshotTitle/);
  assert.match(viewSource, /snapshotExtension/);
  assert.match(viewSource, /openFile\(snapshot/);
  assert.match(viewSource, /isHistorySnapshotPath/);
  assert.doesNotMatch(viewSource, /MarkdownRenderer\.render/);
  assert.doesNotMatch(viewSource, /"Rendered"/);
  assert.doesNotMatch(viewSource, /"Markdown"/);
  assert.doesNotMatch(viewSource, /textarea\.readOnly = true/);
  assert.doesNotMatch(viewSource, /navigator\.clipboard\.writeText/);
  assert.doesNotMatch(viewSource, /Replace current file/);
  assert.doesNotMatch(viewSource, /Restore binary file/);
});

test("file history list aligns date left and source device right", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /button\.style\.display = "flex"/);
  assert.match(viewSource, /button\.style\.alignItems = "center"/);
  assert.match(viewSource, /button\.style\.justifyContent = "stretch"/);
  assert.match(viewSource, /row\.style\.gridTemplateColumns = "minmax\(0, 1fr\) minmax\(80px, auto\)"/);
  assert.match(viewSource, /row\.style\.flex = "1 1 auto"/);
  assert.match(viewSource, /row\.style\.width = "100%"/);
  assert.match(viewSource, /dateEl\.style\.alignItems = "center"/);
  assert.match(viewSource, /dateEl\.style\.justifyContent = "flex-start"/);
  assert.match(viewSource, /dateEl\.style\.fontWeight = "700"/);
  assert.match(viewSource, /dateEl\.style\.textAlign = "left"/);
  assert.match(viewSource, /deviceEl\.style\.alignItems = "center"/);
  assert.match(viewSource, /deviceEl\.style\.justifyContent = "flex-end"/);
  assert.match(viewSource, /deviceEl\.style\.justifySelf = "end"/);
  assert.match(viewSource, /deviceEl\.style\.textAlign = "right"/);
});
