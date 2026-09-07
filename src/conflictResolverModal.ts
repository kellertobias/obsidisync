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

/** What the user picked for one file. Nothing is sent until "Resolve" is pressed. */
export type FileChoice =
  | { kind: "server" }
  | { kind: "local" }
  | { kind: "custom"; content: string }
  | { kind: "current" }
  | { kind: "delete" }
  | { kind: "restore" };

interface ConflictFile {
  path: string;
  reason: string;
  /** The server reported this path in a sync, resolve, or pending-conflicts response. */
  reportedByServer: boolean;
  /** The file currently exists in the local vault. */
  exists: boolean;
  /** Parsed conflict hunks, or null when the file has no usable text markers. */
  parsed: ParsedConflictDocument | null;
  choice: FileChoice | null;
}

interface HunkResolution {
  choice: Side | "custom";
  custom: string;
}

const SCANNED_REASON = "Conflict markers found in this file";
const PENDING_REASON = "file is already awaiting conflict resolution";

export type ConflictResolverClosedHandler = (remainingPaths: string[]) => void;

export class ConflictResolverModal extends Modal {
  /** Paths the server told us are conflicted, with the server's reason. */
  private reported = new Map<string, string>();
  private conflicts: ConflictFile[] = [];
  private choices = new Map<string, FileChoice>();
  private syncStateEl: HTMLElement | null = null;
  private unsubscribeSyncState: (() => void) | null = null;
  private syncRunning = false;
  private busy = false;
  private actionButtons: HTMLButtonElement[] = [];

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
    await this.loadPendingFromServer();
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

  private async loadPendingFromServer(): Promise<void> {
    try {
      for (const conflict of await this.gitService.pendingConflicts()) {
        if (!this.reported.has(conflict.path)) this.reported.set(conflict.path, conflict.reason);
      }
    } catch (error) {
      // Offline or not logged in: fall back to what the last sync reported and the vault scan.
      console.warn("ObsidiSync: could not load pending conflicts from the server", error);
    }
  }

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
      byPath.set(path, { path, reason: friendlyReason(reason), reportedByServer: true, exists, parsed, choice: null });
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
            parsed: parseConflictDocument(content),
            choice: null
          });
        } catch {
          // Binary or unreadable files cannot be resolved in the text hunk editor.
        }
      })
    );

    this.conflicts = Array.from(byPath.values()).sort((left, right) => left.path.localeCompare(right.path));
    for (const conflict of this.conflicts) {
      const choice = this.choices.get(conflict.path) ?? null;
      conflict.choice = choice && isChoiceAvailable(conflict, choice) ? choice : null;
      if (!conflict.choice) this.choices.delete(conflict.path);
    }
  }

  private setChoice(conflict: ConflictFile, choice: FileChoice | null): void {
    conflict.choice = choice;
    if (choice) this.choices.set(conflict.path, choice);
    else this.choices.delete(conflict.path);
  }

  private selectedConflicts(): ConflictFile[] {
    return this.conflicts.filter((conflict) => conflict.choice);
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
      this.createButton(actions, "Close", () => this.close(), { primary: true, plain: true });
      return;
    }

    const count = this.conflicts.length;
    contentEl.createEl("p", {
      text: `${count} conflicted file${count === 1 ? "" : "s"}. Choose what to keep for each file, then press Resolve.`
    });

    this.renderQuickSelect(contentEl);

    const list = contentEl.createDiv();
    list.style.display = "flex";
    list.style.flexDirection = "column";
    list.style.gap = "8px";
    list.style.maxHeight = "50vh";
    list.style.overflow = "auto";
    list.style.border = "1px solid var(--background-modifier-border)";
    list.style.borderRadius = "8px";
    list.style.padding = "8px";

    this.conflicts.forEach((conflict, index) => this.renderFileRow(list, conflict, index));

    const selected = this.selectedConflicts().length;
    const footer = this.createButtonRow(contentEl);
    const resolveButton = this.createButton(
      footer,
      selected === 0 ? "Resolve (nothing selected)" : `Resolve ${selected} file${selected === 1 ? "" : "s"}`,
      () => void this.resolveSelected(),
      { primary: true }
    );
    resolveButton.disabled = resolveButton.disabled || selected === 0;
    this.createButton(footer, "Close", () => this.close(), { plain: true });
    contentEl.createEl("p", {
      text: "Closing keeps the conflicts and your selections. Reopen this dialog any time from the sync menu or the \"Open conflict resolver\" command.",
      cls: "setting-item-description"
    });
  }

  private renderQuickSelect(container: HTMLElement): void {
    const resolvable = this.conflicts.filter((conflict) => conflict.parsed);
    if (this.conflicts.length < 2 || resolvable.length === 0) return;

    const row = container.createDiv();
    row.style.display = "flex";
    row.style.flexWrap = "wrap";
    row.style.alignItems = "center";
    row.style.gap = "8px";
    row.style.marginBottom = "10px";
    const label = row.createEl("span", { text: "Select for all files with markers:" });
    label.style.fontSize = "12px";
    label.style.color = "var(--text-muted)";
    const fill = (side: Side) => {
      for (const conflict of resolvable) this.setChoice(conflict, { kind: side });
      this.renderFileList();
    };
    this.createButton(row, "Server", () => fill("server"), { plain: true, compact: true });
    this.createButton(row, "Local", () => fill("local"), { plain: true, compact: true });
    this.createButton(
      row,
      "Clear",
      () => {
        for (const conflict of this.conflicts) this.setChoice(conflict, null);
        this.renderFileList();
      },
      { plain: true, compact: true }
    );
  }

  private renderFileRow(list: HTMLElement, conflict: ConflictFile, index: number): void {
    const row = list.createDiv();
    row.style.display = "flex";
    row.style.flexDirection = "column";
    row.style.gap = "6px";
    row.style.padding = "8px";
    row.style.borderRadius = "6px";
    row.style.background = "var(--background-secondary)";
    if (conflict.choice) row.style.outline = "1px solid var(--interactive-accent)";

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
    const toggle = (label: string, choice: FileChoice) => {
      const active = Boolean(conflict.choice && conflict.choice.kind === choice.kind);
      const button = this.createButton(
        actions,
        label,
        () => {
          this.setChoice(conflict, active ? null : choice);
          this.renderFileList();
        },
        { plain: true, compact: true }
      );
      button.toggleClass("mod-cta", active);
      button.setAttr("aria-pressed", String(active));
      return button;
    };

    if (!conflict.exists) {
      toggle("Delete on server", { kind: "delete" });
      toggle("Restore server version", { kind: "restore" });
      return;
    }
    if (conflict.parsed) {
      toggle(`Keep ${sideLabel(conflict.parsed, "server")}`, { kind: "server" });
      toggle(`Keep ${sideLabel(conflict.parsed, "local")}`, { kind: "local" });
      const merge = this.createButton(actions, conflict.choice?.kind === "custom" ? "Edit merge…" : "Merge…", () => this.renderFile(index), {
        plain: true,
        compact: true
      });
      merge.toggleClass("mod-cta", conflict.choice?.kind === "custom");
      merge.setAttr("aria-pressed", String(conflict.choice?.kind === "custom"));
      return;
    }
    toggle("Use current content", { kind: "current" });
    toggle("Delete on server", { kind: "delete" });
    this.createButton(actions, "Open file", () => void this.openInEditor(conflict.path), { plain: true, compact: true });
  }

  private describeConflict(conflict: ConflictFile): string {
    if (!conflict.exists) return `${conflict.reason}. The file no longer exists in this vault.`;
    if (!conflict.parsed) return `${conflict.reason}. No text markers found; the file can be pushed as it is now.`;
    const hunks = conflict.parsed.hunks.length;
    const kind = conflict.parsed.generic ? "git-style change" : "change";
    const chosen = conflict.choice?.kind === "custom" ? " Merged by hand." : "";
    return `${conflict.reason}. ${hunks} conflicted ${kind}${hunks === 1 ? "" : "s"}.${chosen}`;
  }

  // ---------------------------------------------------------------------------------------------
  // Single file merge editor

  private renderFile(index: number): void {
    const conflict = this.conflicts[index];
    if (!conflict?.parsed) {
      this.renderFileList();
      return;
    }
    const parsed = conflict.parsed;
    const { contentEl } = this;
    contentEl.empty();
    this.actionButtons = [];

    contentEl.createEl("h2", { text: conflict.path });
    this.renderSyncStatus(contentEl);
    contentEl.createEl("p", {
      text: `File ${index + 1} of ${this.conflicts.length} · ${this.describeConflict(conflict)}`,
      cls: "setting-item-description"
    });
    if (parsed.generic) {
      contentEl.createEl("p", {
        text: "These markers were produced by git on the server while integrating a remote branch. The first side is the remote branch (HEAD), the second side is the pending sync commit.",
        cls: "setting-item-description"
      });
    }

    // A hand merge stores whole-file content, which cannot be split back into hunks reliably,
    // so it is edited as one block. Otherwise start every hunk from the side chosen in the list.
    if (conflict.choice?.kind === "custom") {
      this.renderWholeFileEditor(index, conflict, conflict.choice.content);
      return;
    }
    const startSide: Side = conflict.choice?.kind === "local" ? "local" : "server";
    const resolutions: HunkResolution[] = parsed.hunks.map((hunk) => ({
      choice: startSide,
      custom: startSide === "server" ? hunk.server : hunk.local
    }));

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
    this.createButton(
      footer,
      "Use this merge",
      () => {
        let hunkIndex = 0;
        const content = buildResolvedText(parsed, () => {
          const resolution = resolutions[hunkIndex++];
          if (resolution.choice === "custom") return { content: resolution.custom };
          return { side: resolution.choice };
        });
        const allServer = resolutions.every((resolution) => resolution.choice === "server");
        const allLocal = resolutions.every((resolution) => resolution.choice === "local");
        this.setChoice(conflict, allServer ? { kind: "server" } : allLocal ? { kind: "local" } : { kind: "custom", content });
        this.renderFileList();
      },
      { primary: true, plain: true }
    );
    this.createButton(footer, "Back to list", () => this.renderFileList(), { plain: true });
    this.createButton(footer, "Close", () => this.close(), { plain: true });
  }

  private renderWholeFileEditor(index: number, conflict: ConflictFile, content: string): void {
    const { contentEl } = this;
    const textarea = contentEl.createEl("textarea");
    textarea.value = content;
    textarea.style.width = "100%";
    textarea.style.minHeight = "50vh";
    textarea.style.resize = "vertical";
    textarea.style.fontFamily = "var(--font-monospace)";
    const footer = this.createButtonRow(contentEl);
    this.createButton(
      footer,
      "Use this merge",
      () => {
        this.setChoice(conflict, { kind: "custom", content: textarea.value });
        this.renderFileList();
      },
      { primary: true, plain: true }
    );
    this.createButton(
      footer,
      "Start over from the conflict markers",
      () => {
        this.setChoice(conflict, null);
        this.renderFile(index);
      },
      { plain: true }
    );
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
    const serverButton = this.createButton(serverWrap, `Use ${sideLabel(parsed, "server")}`, () => setChoice("server"), { plain: true });
    const localWrap = this.renderPreview(grid, capitalize(sideLabel(parsed, "local")), hunk.local);
    const localButton = this.createButton(localWrap, `Use ${sideLabel(parsed, "local")}`, () => setChoice("local"), { plain: true });

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

  private async resolveSelected(): Promise<void> {
    if (this.busy) return;
    const selected = this.selectedConflicts();
    if (selected.length === 0) return;
    this.busy = true;
    const paths = selected.map((conflict) => conflict.path);
    this.renderProgress(
      paths.length === 1 ? paths[0] : `${paths.length} files`,
      `Pushing resolution${paths.length === 1 ? "" : "s"}...`
    );
    try {
      const resolutions: ConflictResolution[] = [];
      for (const conflict of selected) {
        resolutions.push(await this.toResolution(conflict));
      }
      const remaining = await this.gitService.resolveConflicts(resolutions);
      for (const path of paths) {
        this.reported.delete(path);
        this.choices.delete(path);
      }
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
      this.renderFileList();
    } catch (error) {
      this.renderError(paths, error, () => void this.resolveSelected());
    } finally {
      this.busy = false;
    }
  }

  /** Turns a choice into what the sync service needs, fetching the server version when asked. */
  private async toResolution(conflict: ConflictFile): Promise<ConflictResolution> {
    const choice = conflict.choice;
    if (!choice) throw new Error(`No choice for ${conflict.path}`);
    switch (choice.kind) {
      case "server":
      case "local":
        if (!conflict.parsed) throw new Error(`${conflict.path} has no conflict markers to pick a side from`);
        return { path: conflict.path, kind: "text", content: buildResolvedText(conflict.parsed, () => ({ side: choice.kind })) };
      case "custom":
        return { path: conflict.path, kind: "text", content: choice.content };
      case "current":
        return { path: conflict.path, kind: "current" };
      case "delete":
        return { path: conflict.path, kind: "delete" };
      case "restore": {
        const history = await this.gitService.history(conflict.path);
        const latest = history[0];
        if (!latest) throw new Error(`The server has no committed version of ${conflict.path}. Delete it on the server instead.`);
        const version = await this.gitService.fileAtVersion(conflict.path, latest.hash);
        const bytes = Uint8Array.from(atob(version.contentBase64), (char) => char.charCodeAt(0));
        await this.ensureParentFolder(conflict.path);
        await this.app.vault.adapter.writeBinary(conflict.path, bytes.buffer);
        return { path: conflict.path, kind: "current" };
      }
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
      this.syncStateEl.setText(this.syncRunning ? "Sync is running... resolving is available once it finishes." : "");
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
    options: { primary?: boolean; plain?: boolean; compact?: boolean } = {}
  ): HTMLButtonElement {
    const button = container.createEl("button", { text, attr: { type: "button" } });
    button.style.flex = options.compact ? "0 1 auto" : "1 1 auto";
    button.style.minHeight = options.compact ? "30px" : "36px";
    button.style.textAlign = "center";
    if (options.primary) button.addClass("mod-cta");
    button.onclick = onClick;
    if (!options.plain) {
      // Selection and navigation stay usable while a sync runs; talking to the server does not.
      this.actionButtons.push(button);
      button.disabled = this.syncRunning;
    }
    return button;
  }
}

function isChoiceAvailable(conflict: ConflictFile, choice: FileChoice): boolean {
  switch (choice.kind) {
    case "server":
    case "local":
    case "custom":
      return conflict.exists && Boolean(conflict.parsed);
    case "current":
      return conflict.exists && !conflict.parsed;
    case "delete":
      return !conflict.parsed;
    case "restore":
      return !conflict.exists;
  }
}

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
