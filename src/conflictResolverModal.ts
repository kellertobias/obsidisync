import { App, Modal, Notice, TFile } from "obsidian";
import { buildResolvedText, ConflictHunk, ParsedConflictDocument, parseConflictDocument } from "./conflictParser";
import { GitService } from "./gitService";
import { SyncConflict } from "./protocol";

type ConflictChoice = "server" | "local" | "custom";

interface ConflictFile {
  path: string;
  reason: string;
}

interface HunkResolution {
  choice: ConflictChoice;
  custom: string;
}

export class ConflictResolverModal extends Modal {
  private conflicts: ConflictFile[] = [];
  private statusEl: HTMLElement | null = null;
  private syncStateEl: HTMLElement | null = null;
  private unsubscribeSyncState: (() => void) | null = null;
  private syncRunning = false;
  private resolving = false;

  constructor(
    app: App,
    private readonly gitService: GitService,
    initialConflicts: SyncConflict[] = [],
    private readonly onClosed?: () => void
  ) {
    super(app);
    this.conflicts = initialConflicts.map((conflict) => ({
      path: conflict.path,
      reason: conflict.reason
    }));
  }

  async onOpen(): Promise<void> {
    this.modalEl.style.width = "min(900px, 96vw)";
    this.unsubscribeSyncState = this.gitService.onSyncStateChange((running) => {
      this.syncRunning = running;
      this.updateSyncStatus();
    });
    await this.loadConflicts();
    this.renderFileList();
  }

  onClose(): void {
    this.unsubscribeSyncState?.();
    this.unsubscribeSyncState = null;
    this.onClosed?.();
  }

  private async loadConflicts(): Promise<void> {
    const byPath = new Map<string, ConflictFile>();
    for (const conflict of this.conflicts) {
      byPath.set(conflict.path, conflict);
    }

    const files = this.app.vault.getFiles();
    await Promise.all(
      files.map(async (file) => {
        try {
          const content = await this.app.vault.cachedRead(file);
          if (!hasConflictMarkers(content)) return;
          const existing = byPath.get(file.path);
          byPath.set(file.path, {
            path: file.path,
            reason: existing?.reason ?? "Conflict markers found in this file"
          });
        } catch {
          // Binary or unreadable files cannot be resolved in the text hunk editor.
        }
      })
    );

    this.conflicts = Array.from(byPath.values()).sort((left, right) => left.path.localeCompare(right.path));
  }

  private renderFileList(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Resolve sync conflicts" });
    this.renderSyncStatus(contentEl);

    if (this.conflicts.length === 0) {
      contentEl.createEl("p", { text: "No conflict markers were found in this vault." });
      const actions = this.createButtonStack(contentEl);
      this.createStackButton(actions, "Close", () => this.close());
      return;
    }

    contentEl.createEl("p", {
      text: `${this.conflicts.length} conflicted file${this.conflicts.length === 1 ? "" : "s"}`
    });

    const list = contentEl.createDiv();
    list.style.display = "flex";
    list.style.flexDirection = "column";
    list.style.gap = "8px";
    list.style.maxHeight = "55vh";
    list.style.overflow = "auto";
    list.style.border = "1px solid var(--background-modifier-border)";
    list.style.borderRadius = "8px";
    list.style.padding = "8px";

    for (const conflict of this.conflicts) {
      const row = list.createDiv();
      row.style.display = "grid";
      row.style.gridTemplateColumns = "minmax(0, 1fr) auto";
      row.style.gap = "12px";
      row.style.alignItems = "center";
      row.style.padding = "8px";
      row.style.borderRadius = "6px";
      row.style.background = "var(--background-secondary)";

      const text = row.createDiv();
      text.style.minWidth = "0";
      const name = text.createEl("div", { text: conflict.path });
      name.style.fontWeight = "700";
      name.style.overflow = "hidden";
      name.style.textOverflow = "ellipsis";
      name.style.whiteSpace = "nowrap";
      name.title = conflict.path;
      const reason = text.createEl("div", { text: conflict.reason });
      reason.style.color = "var(--text-muted)";
      reason.style.fontSize = "12px";

      const button = row.createEl("button", { text: "Resolve", attr: { type: "button" } });
      button.onclick = () => void this.openFile(conflict.path);
    }

    this.statusEl = contentEl.createEl("p", { text: "" });
  }

  private async openFile(path: string): Promise<void> {
    const file = this.app.vault.getAbstractFileByPath(path);
    if (!(file instanceof TFile)) {
      new Notice(`Could not find ${path}`);
      await this.loadConflicts();
      this.renderFileList();
      return;
    }

    let content = "";
    try {
      content = await this.app.vault.cachedRead(file);
    } catch (error) {
      this.renderUnsupportedFile(path, error);
      return;
    }

    const parsed = parseConflictDocument(content);
    if (!parsed || parsed.hunks.length === 0) {
      this.renderUnsupportedFile(path, new Error("No text conflict markers found"));
      return;
    }

    this.renderFileActions(path, parsed);
  }

  private renderUnsupportedFile(path: string, error: unknown): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: path });
    this.renderSyncStatus(contentEl);
    contentEl.createEl("p", { text: errorMessage(error) });
    contentEl.createEl("p", { text: "Edit the file manually, then push the current file content as the resolution." });
    const actions = this.createButtonStack(contentEl);
    this.createStackButton(actions, "Back", () => this.renderFileList());
    this.createStackButton(actions, "Use current file content", () => void this.resolveCurrentFile(path), { primary: true });
    this.createStackButton(actions, "Close", () => this.close());
    this.statusEl = contentEl.createEl("p", { text: "" });
  }

  private renderFileActions(path: string, parsed: ParsedConflictDocument): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: path });
    this.renderSyncStatus(contentEl);
    contentEl.createEl("p", {
      text: `${parsed.hunks.length} conflicted change${parsed.hunks.length === 1 ? "" : "s"}`
    });

    const actions = this.createButtonStack(contentEl);
    this.createStackButton(actions, "Use server version", () => void this.resolveParsed(path, parsed, "server"), { disabled: this.resolving });
    this.createStackButton(actions, "Use local version", () => void this.resolveParsed(path, parsed, "local"), { disabled: this.resolving });
    this.createStackButton(actions, "Merge", () => this.renderMerge(path, parsed), { primary: true });
    this.createStackButton(actions, "Back", () => this.renderFileList());

    this.renderPreview(contentEl, "Server version", buildResolvedText(parsed, () => ({ side: "server" })));
    this.renderPreview(contentEl, "Local version", buildResolvedText(parsed, () => ({ side: "local" })));
    this.statusEl = contentEl.createEl("p", { text: "" });
  }

  private renderMerge(path: string, parsed: ParsedConflictDocument): void {
    const { contentEl } = this;
    const resolutions: HunkResolution[] = parsed.hunks.map((hunk) => ({
      choice: "server",
      custom: hunk.server
    }));

    contentEl.empty();
    contentEl.createEl("h2", { text: path });
    this.renderSyncStatus(contentEl);

    const hunkList = contentEl.createDiv();
    hunkList.style.display = "flex";
    hunkList.style.flexDirection = "column";
    hunkList.style.gap = "12px";
    hunkList.style.maxHeight = "60vh";
    hunkList.style.overflow = "auto";

    parsed.hunks.forEach((hunk, index) => {
      this.renderHunkEditor(hunkList, hunk, index, resolutions[index]);
    });

    this.statusEl = contentEl.createEl("p", { text: "" });
    const actions = this.createButtonStack(contentEl);
    this.createStackButton(actions, "Back", () => this.renderFileActions(path, parsed));
    this.createStackButton(actions, "Apply merge", () => void this.resolveCustom(path, parsed, resolutions), {
      disabled: this.resolving,
      primary: true
    });
  }

  private renderHunkEditor(container: HTMLElement, hunk: ConflictHunk, index: number, resolution: HunkResolution): void {
    const item = container.createDiv();
    item.style.border = "1px solid var(--background-modifier-border)";
    item.style.borderRadius = "8px";
    item.style.padding = "10px";
    item.style.background = "var(--background-secondary)";

    const title = item.createEl("div", { text: `Change ${index + 1}` });
    title.style.fontWeight = "700";
    title.style.marginBottom = "8px";

    const controls = item.createDiv();
    controls.style.display = "flex";
    controls.style.flexDirection = "column";
    controls.style.gap = "8px";
    controls.style.marginBottom = "8px";

    const textarea = item.createEl("textarea");
    textarea.value = hunk.server;
    textarea.style.width = "100%";
    textarea.style.minHeight = "120px";
    textarea.style.resize = "vertical";
    textarea.style.fontFamily = "var(--font-monospace)";

    const setChoice = (choice: ConflictChoice, value: string) => {
      resolution.choice = choice;
      resolution.custom = value;
      textarea.value = value;
    };

    this.createStackButton(controls, "Use server version", () => setChoice("server", hunk.server));
    this.createStackButton(controls, "Use local version", () => setChoice("local", hunk.local));
    this.createStackButton(controls, "Edit text", () => {
      resolution.choice = "custom";
      textarea.focus();
    });
    textarea.oninput = () => {
      resolution.choice = "custom";
      resolution.custom = textarea.value;
    };

    this.renderSideBySide(item, hunk);
  }

  private renderSideBySide(container: HTMLElement, hunk: ConflictHunk): void {
    const grid = container.createDiv();
    grid.style.display = "grid";
    grid.style.gridTemplateColumns = "repeat(auto-fit, minmax(220px, 1fr))";
    grid.style.gap = "8px";
    grid.style.marginTop = "8px";
    this.renderPreview(grid, "Server", hunk.server);
    this.renderPreview(grid, "Local", hunk.local);
  }

  private renderPreview(container: HTMLElement, label: string, text: string): void {
    const wrap = container.createDiv();
    wrap.style.minWidth = "0";
    const title = wrap.createEl("div", { text: label });
    title.style.fontSize = "12px";
    title.style.fontWeight = "700";
    title.style.marginTop = "8px";
    const pre = wrap.createEl("pre", { text });
    pre.style.maxHeight = "180px";
    pre.style.overflow = "auto";
    pre.style.padding = "8px";
    pre.style.borderRadius = "6px";
    pre.style.background = "var(--background-primary)";
    pre.style.border = "1px solid var(--background-modifier-border)";
    pre.style.whiteSpace = "pre-wrap";
  }

  private async resolveParsed(path: string, parsed: ParsedConflictDocument, choice: Exclude<ConflictChoice, "custom">): Promise<void> {
    await this.submitResolution(path, buildResolvedText(parsed, () => ({ side: choice })));
  }

  private async resolveCustom(path: string, parsed: ParsedConflictDocument, resolutions: HunkResolution[]): Promise<void> {
    let hunkIndex = 0;
    const content = buildResolvedText(parsed, () => {
      const resolution = resolutions[hunkIndex++];
      if (resolution.choice === "local") return { side: "local" };
      if (resolution.choice === "server") return { side: "server" };
      return { content: resolution.custom };
    });
    await this.submitResolution(path, content);
  }

  private async submitResolution(path: string, content: string): Promise<void> {
    if (this.resolving) return;
    this.resolving = true;
    this.setStatus("Pushing resolution...");
    try {
      await this.gitService.resolveTextFile(path, content);
      new Notice(`Resolved ${path}`);
      await this.loadConflicts();
      if (this.conflicts.length === 0) {
        this.close();
        return;
      }
      this.renderFileList();
    } catch (error) {
      this.setStatus(`Resolve failed: ${errorMessage(error)}`);
    } finally {
      this.resolving = false;
    }
  }

  private async resolveCurrentFile(path: string): Promise<void> {
    if (this.resolving) return;
    this.resolving = true;
    this.setStatus("Pushing resolution...");
    try {
      await this.gitService.resolveFile(path);
      new Notice(`Resolved ${path}`);
      await this.loadConflicts();
      if (this.conflicts.length === 0) {
        this.close();
        return;
      }
      this.renderFileList();
    } catch (error) {
      this.setStatus(`Resolve failed: ${errorMessage(error)}`);
    } finally {
      this.resolving = false;
    }
  }

  private setStatus(message: string): void {
    if (this.statusEl) this.statusEl.setText(message);
  }

  private renderSyncStatus(container: HTMLElement): void {
    this.syncStateEl = container.createEl("p");
    this.syncStateEl.style.fontWeight = "600";
    this.updateSyncStatus();
  }

  private updateSyncStatus(): void {
    if (!this.syncStateEl) return;
    this.syncStateEl.setText(this.syncRunning ? "Sync is running..." : "No sync is running.");
    this.syncStateEl.style.color = this.syncRunning ? "var(--text-accent)" : "var(--text-muted)";
  }

  private createButtonStack(container: HTMLElement): HTMLElement {
    const actions = container.createDiv();
    actions.style.display = "flex";
    actions.style.flexDirection = "column";
    actions.style.gap = "8px";
    actions.style.margin = "12px 0";
    return actions;
  }

  private createStackButton(
    container: HTMLElement,
    text: string,
    onClick: () => void,
    options: { disabled?: boolean; primary?: boolean } = {}
  ): HTMLButtonElement {
    const button = container.createEl("button", { text, attr: { type: "button" } });
    button.disabled = Boolean(options.disabled);
    button.style.width = "100%";
    button.style.minHeight = "36px";
    button.style.textAlign = "center";
    if (options.primary) button.addClass("mod-cta");
    button.onclick = onClick;
    return button;
  }
}

function hasConflictMarkers(text: string): boolean {
  return text.includes("<<<<<<<") && text.includes("=======") && text.includes(">>>>>>>");
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
