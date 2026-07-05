import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.cwd();

test("plugin registers a dockable ObsidiSync file history view", () => {
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /extends ItemView/);
  assert.match(viewSource, /FILE_HISTORY_VIEW_TYPE = "obsidisync-file-history"/);
  assert.match(viewSource, /getDisplayText\(\): string \{\s*return "ObsidiSync file history";/);
  assert.match(mainSource, /registerView\(\s*FILE_HISTORY_VIEW_TYPE/);
  assert.match(mainSource, /getLeavesOfType\(FILE_HISTORY_VIEW_TYPE\)/);
  assert.match(mainSource, /activeFilePath/);
  assert.match(mainSource, /view\.showFile\(activeFilePath\)/);
  assert.match(mainSource, /historySnapshots/);
  assert.match(mainSource, /snapshotReference\(path\)/);
  assert.match(mainSource, /addRibbonIcon\("history", "Open file history"/);
  assert.match(viewSource, /this\.showFile\(file\?\.path \?\? null\)/);
  assert.match(viewSource, /this\.showFile\(null\)/);
  assert.match(viewSource, /if \(path && !resolved\.path && this\.filePath\)/);
  assert.doesNotMatch(viewSource, /else if \(!this\.app\.workspace\.getActiveViewOfType\(MarkdownView\)\?\.file\)/);
  assert.doesNotMatch(mainSource, /FileVersionsModal/);
});

test("file history view shows sync status, last save, source device, and sync action", () => {
  const serviceSource = readFileSync(join(root, "src", "gitService.ts"), "utf8");
  const mainSource = readFileSync(join(root, "src", "main.ts"), "utf8");
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(serviceSource, /currentDeviceName\(\): string/);
  assert.match(serviceSource, /this\.settings\.lastSyncedAt = new Date\(\)\.toISOString\(\)/);
  assert.match(mainSource, /lastSyncedAt: \(\) => this\.settings\.lastSyncedAt/);
  assert.match(viewSource, /sha256Hex/);
  assert.match(viewSource, /`Up to date: version \$\{latest\.versionNumber\}`/);
  assert.match(viewSource, /"File changed"/);
  assert.match(viewSource, /this\.app\.vault\.on\("modify"/);
  assert.match(viewSource, /file\.path !== this\.filePath/);
  assert.match(viewSource, /if \(localIsNewer\)/);
  assert.match(viewSource, /if \(localSha === latestVersion\.sha256\)[\s\S]*?title: `Up to date: version \$\{latest\.versionNumber\}`[\s\S]*?const latestTime = Date\.parse\(latest\.date\)/);
  assert.match(viewSource, /"Last saved"/);
  assert.match(viewSource, /"Source"/);
  assert.doesNotMatch(viewSource, /"Login"/);
  assert.doesNotMatch(viewSource, /onLoginStatusChange/);
  assert.match(serviceSource, /state: "not-logged-in"/);
  assert.match(serviceSource, /state: "failed"/);
  assert.match(serviceSource, /Re-login failed/);
  assert.match(viewSource, /gitService\.sync\(\)/);
  assert.match(viewSource, /extractSyncDevice/);
  assert.doesNotMatch(viewSource, /"Refresh history"/);
  assert.doesNotMatch(viewSource, /renderHeader/);
  assert.doesNotMatch(viewSource, /"Synced versions"/);
});

test("file history view shows centered last sync status and sync action when no file is active", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /this\.renderNoActiveFile\(contentEl\)/);
  assert.match(viewSource, /text: `Last Synced: \$\{formatLastSynced\(this\.snapshots\.lastSyncedAt\(\)\)\}`/);
  assert.match(viewSource, /text: this\.syncing \? "Syncing" : "Sync Now"/);
  assert.match(viewSource, /empty\.style\.alignItems = "center"/);
  assert.match(viewSource, /empty\.style\.justifyContent = "center"/);
  assert.match(viewSource, /syncButton\.onclick = \(\) => this\.syncNow\(\)/);
  assert.match(viewSource, /function formatLastSynced\(date: string \| null\): string/);
  assert.doesNotMatch(viewSource, /Open a Markdown file to view its history\./);
});

test("file history view opens selected versions with the regular Obsidian file UI", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /gitService\.history\(this\.filePath\)/);
  assert.match(viewSource, /gitService\.fileAtVersion\(this\.filePath, entry\.hash\)/);
  assert.match(viewSource, /HISTORY_SNAPSHOT_DIR/);
  assert.match(viewSource, /createBinary\(path, content\)/);
  assert.match(viewSource, /modifyBinary\(existing, content\)/);
  assert.match(viewSource, /adapter\.exists\(path, true\)/);
  assert.match(viewSource, /openVersion\(entry, versionNumber\)/);
  assert.match(viewSource, /snapshotPath\(this\.filePath \?\? "version", entry, source\.device, versionNumber, attempt\)/);
  assert.match(viewSource, /`\$\{HISTORY_SNAPSHOT_DIR\}\/Version \$\{version\} - \$\{date\} - \$\{computer\} - \$\{title\}\$\{suffix\}\$\{extension\}`/);
  assert.match(viewSource, /formatSnapshotDate/);
  assert.match(viewSource, /snapshotTitle/);
  assert.match(viewSource, /snapshotExtension/);
  assert.match(viewSource, /saveSnapshotReference\(created\.path, entry\.hash\)/);
  assert.match(viewSource, /resolveHistorySnapshot/);
  assert.match(viewSource, /this\.snapshots\.get\(path\)/);
  assert.match(viewSource, /return \{ path: reference\.sourcePath, hash: reference\.hash \}/);
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

test("file history list shows version numbers and keeps device status compact", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /const versionNumber = entry\.versionNumber/);
  assert.match(viewSource, /`Version \$\{versionNumber\}: \$\{name\}`/);
  assert.match(viewSource, /versionEl\.style\.color = this\.selectedHash === entry\.hash \? "var\(--text-accent\)" : "var\(--text-normal\)"/);
  assert.match(viewSource, /private versionName\(entry: HistoryEntry\): string \| null/);
  assert.match(viewSource, /await this\.gitService\.saveVersionMetadata\(\{ path: sourcePath, hash, name: trimmed \|\| null, clearName: !trimmed \}\)/);
  assert.match(viewSource, /entry\.squashedIntoHash == null/);
  assert.match(viewSource, /private visibleHistoryEntries\(\): HistoryEntry\[\]/);
  assert.match(viewSource, /this\.fileStatus\?\.state === "up-to-date" \? this\.history\[0\]\?\.hash : null/);
  assert.match(viewSource, /entry\.hash !== latestHash/);
  assert.match(viewSource, /"No earlier versions for this file yet\."/);
  assert.match(viewSource, /private async squashVersion\(entry: HistoryEntry, intoEntry: HistoryEntry\): Promise<void>/);
  assert.match(viewSource, /class VersionNameModal extends Modal/);
  assert.match(viewSource, /contentEl\.createEl\("h2", \{ text: "Name version" \}\)/);
  assert.match(viewSource, /setButtonText\("Save"\)/);
  assert.match(viewSource, /const intoEntry = visibleHistory\[index - 1\]/);
  assert.match(viewSource, /window\.confirm\(`Squash Version \$\{entry\.versionNumber\} into Version \$\{intoEntry\.versionNumber\}\?/);
  assert.match(viewSource, /button\.style\.display = "flex"/);
  assert.match(viewSource, /button\.style\.alignItems = "center"/);
  assert.match(viewSource, /button\.style\.justifyContent = "stretch"/);
  assert.match(viewSource, /row\.style\.gridTemplateColumns = "minmax\(0, 1fr\) minmax\(80px, auto\)"/);
  assert.match(viewSource, /row\.style\.flex = "1 1 auto"/);
  assert.match(viewSource, /row\.style\.width = "100%"/);
  assert.match(viewSource, /dateEl\.style\.alignItems = "center"/);
  assert.match(viewSource, /dateEl\.style\.justifyContent = "flex-start"/);
  assert.match(viewSource, /dateEl\.style\.textAlign = "left"/);
  assert.match(viewSource, /deviceEl\.style\.alignItems = "center"/);
  assert.match(viewSource, /deviceEl\.style\.justifyContent = "flex-end"/);
  assert.match(viewSource, /deviceEl\.style\.justifySelf = "end"/);
  assert.match(viewSource, /deviceEl\.style\.textAlign = "right"/);
  assert.match(viewSource, /const deviceNameEl = deviceEl\.createEl\("span", \{ text: source\.device \}\)/);
  assert.match(viewSource, /setIcon\(pinEl, "monitor-check"\)/);
  assert.match(viewSource, /title: `Currently on this version: \$\{pinnedDevices\.join\(", "\)\}`/);
  assert.doesNotMatch(viewSource, /text: `📍 \$\{pinnedDevices\.join\(", "\)\}`/);
});

test("file history keeps the status header sticky and lets the version list fill available height", () => {
  const viewSource = readFileSync(join(root, "src", "fileHistoryView.ts"), "utf8");

  assert.match(viewSource, /container\.style\.display = "flex"/);
  assert.match(viewSource, /container\.style\.flexDirection = "column"/);
  assert.match(viewSource, /container\.style\.overflow = "hidden"/);
  assert.match(viewSource, /statusEl\.style\.position = "sticky"/);
  assert.match(viewSource, /statusEl\.style\.top = "0"/);
  assert.match(viewSource, /statusEl\.style\.zIndex = "1"/);
  assert.match(viewSource, /listEl\.style\.flex = "1 1 auto"/);
  assert.match(viewSource, /listEl\.style\.minHeight = "0"/);
  assert.match(viewSource, /listEl\.style\.overflow = "hidden"/);
  assert.match(viewSource, /list\.style\.height = "100%"/);
  assert.match(viewSource, /list\.style\.overflow = "auto"/);
  assert.doesNotMatch(viewSource, /list\.style\.maxHeight = "62vh"/);
});
