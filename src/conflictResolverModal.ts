import { App, Modal, Notice, TFile } from "obsidian";
import {
  buildResolvedText,
  ConflictHunk,
  hasConflictMarkers,
  ParsedConflictDocument,
  parseConflictDocument
} from "./conflictParser";
import { ConflictResolution, GitService } from "./gitService";
import { SyncConflict } from "./protocol";

type Side = "server" | "local";
type ConflictChoice = Side | "custom";

interface ConflictFile {
  path: string;
  reason: string;
  /** The server reported this path in its last sync or resolve response. */
  reportedByServer: boolean;
  /** The file currently exists in the local vault. */
  exists: boolean;
  /** Parsed conflict hunks, or null when the file has no usable text markers. */
  parsed: ParsedConflictDocument | null;
}

interface HunkResolution {
  choice: ConflictChoice;
  custom: string;
}

const SCANNED_REASON = "Conflict markers found in this file";
const PENDING_REASON = "file is already awaiting conflict resolution";

export type ConflictResolverClosedHandler = (remainingPaths: string[]) => void;

export class ConflictResolverModal extends Modal {
  /** Paths the server told us are conflicted, with the server's reason. */
  private reported = new Map<string, string>();
  private conflicts: ConflictFile[] = [];
  private syncStateEl: HTMLElement | null = null;
  private unsubscribeSyncState: (() => void) | null = null;
  private syncRunning = false;
  private busy = false;
  private actionButtons: HTMLButtonElement[] = [];
  private pendingBulk: Side | null = null;

  constructor(
    app: App,
    private readonly gitService: GitService,
    initialConflicts: SyncConflict[] = [],
    private readonly onClosed?: ConflictResolverClosedHandler
  ) {
    super(app);
    for (const conflict of initialConflicts) {
      this.reported.set(conflict.path, conflict.reason);
    }
  }

  async onOpen(): Promise<void> {
    this.modalEl.style.width = "min(900px, 96vw)";
    this.unsubscribeSyncState = this.gitService.onSyncStateChange((running) => {
      this.syncRunning = running;
      this.updateSyncStatus();
    });
    this.renderProgress("Resolve sync conflicts", "Looking for conflicted files...");
    await this.loadConflicts();
    this.renderFileList();
  }

  onClose(): void {
    this.unsubscribeSyncState?.();
    this.unsubscribeSyncState = null;
    this.contentEl.empty();
    this.onClosed?.(this.conflicts.map((conflict) => conflict.path));
  }

  // ---------------------------------------------------------------------------------------------
  // Data

  private async loadConflicts(): Promise<void> {
    const byPath = new Map<string, ConflictFile>();

    for (const [path, reason] of this.reported) {
      const file = this.app.vault.getAbstractFileByPath(path);
      const exists = file instanceof TFile;
      let parsed: ParsedConflictDocument | null = null;
      if (exists) {
        try {
          // Server-reported files may carry git's own markers after a failed server-side rebase.
          parsed = parseConflictDocument(await this.app.vault.cachedRead(file), { allowGenericMarkers: true });
        } catch {
          parsed = null;
        }
      }
      byPath.set(path, { path, reason: friendlyReason(reason), reportedByServer: true, exists, parsed });
    }

    const files = this.app.vault.getFiles();
    await Promise.all(
      files.map(async (file) => {
        if (byPath.has(file.path)) return;
        try {
          const content = await this.app.vault.cachedRead(file);
          if (!hasConflictMarkers(content)) return;
          byPath.set(file.path, {
            path: file.path,
            reason: SCANNED_REASON,
            reportedByServer: false,
            exists: true,
            parsed: parseConflictDocument(content)
          });
        } catch {
          // Binary or unreadable files cannot be resolved in the text hunk editor.
        }
      })
    );

    this.conflicts = Array.from(byPath.values()).sort((left, right) => left.path.localeCompare(right.path));
  }

  private conflictAt(path: string): ConflictFile | undefined {
    return this.conflicts.find((conflict) => conflict.path === path);
  }

  // ---------------------------------------------------------------------------------------------
  // File list

  private renderFileList(): void {
    const { contentEl } = this;
    contentEl.empty();
    this.actionButtons = [];
    contentEl.createEl("h2", { text: "Resolve sync conflicts" });
    this.renderSyncStatus(contentEl);

    if (this.conflicts.length === 0) {
      contentEl.createEl("p", { text: "No conflicts are left in this vault." });
      const actions = this.createButtonRow(contentEl);
      this.createButton(actions, "Close", () => this.close(), { primary: true });
      return;
    }

    const count = this.conflicts.length;
    contentEl.createEl("p", { text: `${count} conflicted file${count === 1 ? "" : "s"}. Pick a version per file, or resolve all of them at once.` });

    this.renderBulkActions(contentEl);

    const list = contentEl.createDiv();
    list.style.display = "flex";
    list.style.flexDirection = "column";
    list.style.gap = "8px";
    list.style.maxHeight = "55vh";
    list.style.overflow = "auto";
    list.style.border = "1px solid var(--background-modifier-border)";
    list.style.borderRadius = "8px";
    list.style.padding = "8px";

    this.conflicts.forEach((conflict, index) => this.renderFileRow(list, conflict, index));

    const footer = this.createButtonRow(contentEl);
    this.createButton(footer, "Close", () => this.close(), { plain: true });
    contentEl.createEl("p", {
      text: "Closing keeps the conflicts. Reopen this dialog any time from the sync menu or the \"Open conflict resolver\" command.",
      cls: "setting-item-description"
    });
  }

  private renderBulkActions(container: HTMLElement): void {
    const resolvable = this.conflicts.filter((conflict) => conflict.parsed);
    if (this.conflicts.length < 2 || resolvable.length === 0) return;

    const box = container.createDiv();
    box.style.border = "1px solid var(--background-modifier-border)";
    box.style.borderRadius = "8px";
    box.style.padding = "10px";
    box.style.marginBottom = "10px";
    box.style.background = "var(--background-secondary)";

    const skipped = this.conflicts.length - resolvable.length;
    if (this.pendingBulk) {
      const side = this.pendingBulk;
      box.createEl("div", {
        text: `Use the ${side} version for all ${resolvable.length} file${resolvable.length === 1 ? "" : "s"} with conflict markers?`,
        attr: { style: "font-weight:600;margin-bottom:6px" }
      });
      if (skipped > 0) {
        box.createEl("div", {
          text: `${skipped} file${skipped === 1 ? "" : "s"} without text markers will stay in the list.`,
          cls: "setting-item-description"
        });
      }
      const actions = this.createButtonRow(box);
      this.createButton(actions, `Yes, use ${side} for all`, () => {
        this.pendingBulk = null;
        void this.apply(
          resolvable.map((conflict) => ({
            path: conflict.path,
            kind: "text",
            content: buildResolvedText(conflict.parsed as ParsedConflictDocument, () => ({ side }))
          })),
          { returnTo: "list" }
        );
      }, { primary: true });
      this.createButton(actions, "Cancel", () => {
        this.pendingBulk = null;
        this.renderFileList();
      }, { plain: true });
      return;
    }

    box.createEl("div", { text: "Resolve all at once", attr: { style: "font-weight:600;margin-bottom:6px" } });
    const actions = this.createButtonRow(box);
    this.createButton(actions, "Use server version for all", () => {
      this.pendingBulk = "server";
      this.renderFileList();
    });
    this.createButton(actions, "Use local version for all", () => {
      this.pendingBulk = "local";
      this.renderFileList();
    });
  }

  private renderFileRow(list: HTMLElement, conflict: ConflictFile, index: number): void {
    const row = list.createDiv();
    row.style.display = "flex";
    row.style.flexDirection = "column";
    row.style.gap = "6px";
    row.style.padding = "8px";
    row.style.borderRadius = "6px";
    row.style.background = "var(--background-secondary)";

    const name = row.createEl("div", { text: conflict.path });
    name.style.fontWeight = "700";
    name.style.overflow = "hidden";
    name.style.textOverflow = "ellipsis";
    name.style.whiteSpace = "nowrap";
    name.title = conflict.path;

    const detail = row.createEl("div", { text: this.describeConflict(conflict) });
    detail.style.color = "var(--text-muted)";
    detail.style.fontSize = "12px";

    const actions = this.createButtonRow(row, { compact: true });
    if (!conflict.exists) {
      this.createButton(actions, "Delete on server", () => void this.apply([{ path: conflict.path, kind: "delete" }], { returnTo: "list" }));
      this.createButton(actions, "Restore server version", () => void this.restoreServerVersion(conflict.path));
      return;
    }
    if (conflict.parsed) {
      this.createButton(actions, "Server", () => void this.resolveWholeFile(conflict, "server", { returnTo: "list" }));
      this.createButton(actions, "Local", () => void this.resolveWholeFile(conflict, "local", { returnTo: "list" }));
      this.createButton(actions, "Merge…", () => this.renderFile(index), { primary: true });
      return;
    }
    this.createButton(actions, "Use current content", () => void this.apply([{ path: conflict.path, kind: "current" }], { returnTo: "list" }), { primary: true });
    this.createButton(actions, "Open file", () => void this.openInEditor(conflict.path));
  }

  private describeConflict(conflict: ConflictFile): string {
    if (!conflict.exists) return `${conflict.reason}. The file no longer exists in this vault.`;
    if (!conflict.parsed) return `${conflict.reason}. No text markers found; the current file content will be used as the resolution.`;
    const hunks = conflict.parsed.hunks.length;
    const kind = conflict.parsed.generic ? "git-style change" : "change";
    return `${conflict.reason}. ${hunks} conflicted ${kind}${hunks === 1 ? "" : "s"}.`;
  }

  // ---------------------------------------------------------------------------------------------
  // Single file view (merge editor)

  private renderFile(index: number): void {
    const conflict = this.conflicts[index];
    if (!conflict) {
      this.renderFileList();
      return;
    }
    if (!conflict.parsed) {
      this.renderFileList();
      return;
    }
    const parsed = conflict.parsed;
    const { contentEl } = this;
    contentEl.empty();
    this.actionButtons = [];

    contentEl.createEl("h2", { text: conflict.path });
    this.renderSyncStatus(contentEl);
    const total = this.conflicts.length;
    contentEl.createEl("p", {
      text: `File ${index + 1} of ${total} · ${this.describeConflict(conflict)}`,
      cls: "setting-item-description"
    });
    if (parsed.generic) {
      contentEl.createEl("p", {
        text: "These markers were produced by git on the server while integrating a remote branch. The first side is the remote branch (HEAD), the second side is the pending sync commit.",
        cls: "setting-item-description"
      });
    }

    const quick = this.createButtonRow(contentEl);
    this.createButton(quick, `Use ${sideLabel(parsed, "server")} version`, () => void this.resolveWholeFile(conflict, "server", { returnTo: "next", index }));
    this.createButton(quick, `Use ${sideLabel(parsed, "local")} version`, () => void this.resolveWholeFile(conflict, "local", { returnTo: "next", index }));

    const resolutions: HunkResolution[] = parsed.hunks.map((hunk) => ({ choice: "server", custom: hunk.server }));

    const hunkList = contentEl.createDiv();
    hunkList.style.display = "flex";
    hunkList.style.flexDirection = "column";
    hunkList.style.gap = "12px";
    hunkList.style.maxHeight = "55vh";
    hunkList.style.overflow = "auto";
    parsed.hunks.forEach((hunk, hunkIndex) => {
      this.renderHunkEditor(hunkList, parsed, hunk, hunkIndex, resolutions[hunkIndex]);
    });

    const footer = this.createButtonRow(contentEl);
    this.createButton(footer, parsed.hunks.length === 1 ? "Apply selection" : "Apply merged result", () => {
      let hunkIndex = 0;
      const content = buildResolvedText(parsed, () => {
        const resolution = resolutions[hunkIndex++];
        if (resolution.choice === "custom") return { content: resolution.custom };
        return { side: resolution.choice };
      });
      void this.apply([{ path: conflict.path, kind: "text", content }], { returnTo: "next", index });
    }, { primary: true });
    this.createButton(footer, "Back to list", () => this.renderFileList(), { plain: true });
    this.createButton(footer, "Close", () => this.close(), { plain: true });
  }

  private renderHunkEditor(
    container: HTMLElement,
    parsed: ParsedConflictDocument,
    hunk: ConflictHunk,
    index: number,
    resolution: HunkResolution
  ): void {
    const item = container.createDiv();
    item.style.border = "1px solid var(--background-modifier-border)";
    item.style.borderRadius = "8px";
    item.style.padding = "10px";
    item.style.background = "var(--background-secondary)";

    const header = item.createDiv();
    header.style.display = "flex";
    header.style.justifyContent = "space-between";
    header.style.alignItems = "center";
    header.style.marginBottom = "8px";
    const title = header.createEl("div", { text: `Change ${index + 1} of ${parsed.hunks.length}` });
    title.style.fontWeight = "700";
    const chosen = header.createEl("div", { text: "" });
    chosen.style.fontSize = "12px";
    chosen.style.color = "var(--text-accent)";

    const grid = item.createDiv();
    grid.style.display = "grid";
    grid.style.gridTemplateColumns = "repeat(auto-fit, minmax(220px, 1fr))";
    grid.style.gap = "8px";

    const serverWrap = this.renderPreview(grid, capitalize(sideLabel(parsed, "server")), hunk.server);
    const serverButton = this.createButton(serverWrap, `Use ${sideLabel(parsed, "server")}`, () => setChoice("server"));
    const localWrap = this.renderPreview(grid, capitalize(sideLabel(parsed, "local")), hunk.local);
    const localButton = this.createButton(localWrap, `Use ${sideLabel(parsed, "local")}`, () => setChoice("local"));

    const editLabel = item.createEl("div", { text: "Or edit the result by hand" });
    editLabel.style.fontSize = "12px";
    editLabel.style.fontWeight = "700";
    editLabel.style.marginTop = "8px";
    const textarea = item.createEl("textarea");
    textarea.style.width = "100%";
    textarea.style.minHeight = "100px";
    textarea.style.resize = "vertical";
    textarea.style.fontFamily = "var(--font-monospace)";

    const refresh = () => {
      serverButton.toggleClass("mod-cta", resolution.choice === "server");
      localButton.toggleClass("mod-cta", resolution.choice === "local");
      serverButton.setAttr("aria-pressed", String(resolution.choice === "server"));
      localButton.setAttr("aria-pressed", String(resolution.choice === "local"));
      chosen.setText(
        resolution.choice === "custom"
          ? "Selected: edited text"
          : `Selected: ${sideLabel(parsed, resolution.choice)} version`
      );
    };
    const setChoice = (choice: Side) => {
      resolution.choice = choice;
      resolution.custom = choice === "server" ? hunk.server : hunk.local;
      textarea.value = resolution.custom;
      refresh();
    };
    textarea.oninput = () => {
      resolution.choice = "custom";
      resolution.custom = textarea.value;
      refresh();
    };

    textarea.value = resolution.custom;
    refresh();
  }

  private renderPreview(container: HTMLElement, label: string, text: string): HTMLElement {
    const wrap = container.createDiv();
    wrap.style.minWidth = "0";
    wrap.style.display = "flex";
    wrap.style.flexDirection = "column";
    wrap.style.gap = "6px";
    const title = wrap.createEl("div", { text: label });
    title.style.fontSize = "12px";
    title.style.fontWeight = "700";
    const pre = wrap.createEl("pre", { text: text || "(empty)" });
    pre.style.flex = "1";
    pre.style.margin = "0";
    pre.style.maxHeight = "180px";
    pre.style.overflow = "auto";
    pre.style.padding = "8px";
    pre.style.borderRadius = "6px";
    pre.style.background = "var(--background-primary)";
    pre.style.border = "1px solid var(--background-modifier-border)";
    pre.style.whiteSpace = "pre-wrap";
    if (!text) pre.style.color = "var(--text-faint)";
    return wrap;
  }

  // ---------------------------------------------------------------------------------------------
  // Resolution

  private async resolveWholeFile(conflict: ConflictFile, side: Side, navigation: Navigation): Promise<void> {
    if (!conflict.parsed) return;
    const content = buildResolvedText(conflict.parsed, () => ({ side }));
    await this.apply([{ path: conflict.path, kind: "text", content }], navigation);
  }

  private async restoreServerVersion(path: string): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    this.renderProgress(path, "Fetching the server version...");
    try {
      const history = await this.gitService.history(path);
      const latest = history[0];
      if (!latest) throw new Error("The server has no committed version of this file. Delete it on the server instead.");
      const version = await this.gitService.fileAtVersion(path, latest.hash);
      const bytes = Uint8Array.from(atob(version.contentBase64), (char) => char.charCodeAt(0));
      await this.ensureParentFolder(path);
      await this.app.vault.adapter.writeBinary(path, bytes.buffer);
    } catch (error) {
      this.busy = false;
      this.renderError([path], error, () => void this.restoreServerVersion(path));
      return;
    }
    this.busy = false;
    await this.apply([{ path, kind: "current" }], { returnTo: "list" });
  }

  private async apply(resolutions: ConflictResolution[], navigation: Navigation): Promise<void> {
    if (this.busy || resolutions.length === 0) return;
    this.busy = true;
    const paths = resolutions.map((resolution) => resolution.path);
    const label = paths.length === 1 ? paths[0] : `${paths.length} files`;
    this.renderProgress(label, `Pushing resolution${paths.length === 1 ? "" : "s"}...`);
    try {
      const remaining = await this.gitService.resolveConflicts(resolutions);
      for (const path of paths) this.reported.delete(path);
      for (const conflict of remaining) this.reported.set(conflict.path, conflict.reason);
      await this.loadConflicts();

      if (remaining.length > 0) {
        new Notice(`The server still reports ${remaining.length} conflict${remaining.length === 1 ? "" : "s"}`, 8000);
      } else {
        new Notice(paths.length === 1 ? `Resolved ${paths[0]}` : `Resolved ${paths.length} files`);
      }

      if (this.conflicts.length === 0) {
        new Notice("All sync conflicts resolved");
        this.close();
        return;
      }
      if (navigation.returnTo === "next") {
        const nextIndex = Math.min(navigation.index, this.conflicts.length - 1);
        if (this.conflicts[nextIndex]?.parsed) {
          this.renderFile(nextIndex);
          return;
        }
      }
      this.renderFileList();
    } catch (error) {
      this.renderError(paths, error, () => void this.apply(resolutions, navigation));
    } finally {
      this.busy = false;
    }
  }

  private async openInEditor(path: string): Promise<void> {
    const file = this.app.vault.getAbstractFileByPath(path);
    if (!(file instanceof TFile)) {
      new Notice(`Could not find ${path}`);
      return;
    }
    await this.app.workspace.getLeaf(false).openFile(file);
    this.close();
  }

  private async ensureParentFolder(path: string): Promise<void> {
    const index = path.lastIndexOf("/");
    if (index === -1) return;
    const folder = path.slice(0, index);
    if (!(await this.app.vault.adapter.exists(folder, true))) {
      await this.app.vault.createFolder(folder);
    }
  }

  // ---------------------------------------------------------------------------------------------
  // Status screens

  private renderProgress(title: string, message: string): void {
    const { contentEl } = this;
    contentEl.empty();
    this.actionButtons = [];
    contentEl.createEl("h2", { text: title });
    this.renderSyncStatus(contentEl);
    contentEl.createEl("p", { text: message });
  }

  private renderError(paths: string[], error: unknown, retry: () => void): void {
    const { contentEl } = this;
    contentEl.empty();
    this.actionButtons = [];
    contentEl.createEl("h2", { text: "Resolve failed" });
    this.renderSyncStatus(contentEl);
    contentEl.createEl("p", { text: paths.join(", ") }).style.fontWeight = "600";
    const message = contentEl.createEl("pre", { text: errorMessage(error) });
    message.style.whiteSpace = "pre-wrap";
    message.style.padding = "8px";
    message.style.borderRadius = "6px";
    message.style.background = "var(--background-secondary)";
    const actions = this.createButtonRow(contentEl);
    this.createButton(actions, "Retry", retry, { primary: true });
    this.createButton(actions, "Back to list", () => void this.reloadAndList(), { plain: true });
    this.createButton(actions, "Close", () => this.close(), { plain: true });
  }

  private async reloadAndList(): Promise<void> {
    await this.loadConflicts();
    this.renderFileList();
  }

  private renderSyncStatus(container: HTMLElement): void {
    this.syncStateEl = container.createEl("p");
    this.syncStateEl.style.fontWeight = "600";
    this.syncStateEl.style.fontSize = "12px";
    this.updateSyncStatus();
  }

  private updateSyncStatus(): void {
    if (this.syncStateEl) {
      this.syncStateEl.setText(this.syncRunning ? "Sync is running... actions are available once it finishes." : "");
      this.syncStateEl.style.color = "var(--text-accent)";
      this.syncStateEl.style.display = this.syncRunning ? "" : "none";
    }
    for (const button of this.actionButtons) {
      button.disabled = this.syncRunning;
    }
  }

  // ---------------------------------------------------------------------------------------------
  // Widgets

  private createButtonRow(container: HTMLElement, options: { compact?: boolean } = {}): HTMLElement {
    const actions = container.createDiv();
    actions.style.display = "flex";
    actions.style.flexWrap = "wrap";
    actions.style.gap = options.compact ? "6px" : "8px";
    actions.style.margin = options.compact ? "0" : "12px 0";
    return actions;
  }

  private createButton(
    container: HTMLElement,
    text: string,
    onClick: () => void,
    options: { primary?: boolean; plain?: boolean } = {}
  ): HTMLButtonElement {
    const button = container.createEl("button", { text, attr: { type: "button" } });
    button.style.flex = "1 1 auto";
    button.style.minHeight = "36px";
    button.style.textAlign = "center";
    if (options.primary) button.addClass("mod-cta");
    button.onclick = onClick;
    if (!options.plain) {
      // Navigation buttons stay usable while a sync runs; buttons that talk to the server do not.
      this.actionButtons.push(button);
      button.disabled = this.syncRunning;
    }
    return button;
  }
}

type Navigation = { returnTo: "list" } | { returnTo: "next"; index: number };

function sideLabel(parsed: ParsedConflictDocument, side: Side): string {
  if (!parsed.generic) return side;
  const labels = new Set(parsed.hunks.map((hunk) => (side === "server" ? hunk.serverLabel : hunk.localLabel)));
  if (labels.size !== 1) return side === "server" ? "first" : "second";
  const [label] = Array.from(labels);
  if (!label) return side === "server" ? "first" : "second";
  return label.length > 24 ? `${label.slice(0, 24)}…` : label;
}

function capitalize(text: string): string {
  return text ? text[0].toUpperCase() + text.slice(1) : text;
}

function friendlyReason(reason: string): string {
  if (reason === PENDING_REASON) return "The server is waiting for this device to resolve the file";
  return reason ? capitalize(reason) : "Reported by the server";
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
