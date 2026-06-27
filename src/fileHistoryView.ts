import { ItemView, MarkdownRenderer, MarkdownView, Notice, setIcon, WorkspaceLeaf } from "obsidian";
import { base64ToArrayBuffer } from "./base64";
import { GitService } from "./gitService";
import { HistoryEntry, VersionFileResponse } from "./protocol";

export const FILE_HISTORY_VIEW_TYPE = "obsync-file-history";

type PreviewMode = "rendered" | "markdown";

interface LoadedVersion {
  entry: HistoryEntry;
  version: VersionFileResponse;
  text: string | null;
}

export class FileHistoryView extends ItemView {
  private filePath: string | null = null;
  private history: HistoryEntry[] = [];
  private loadedVersion: LoadedVersion | null = null;
  private previewMode: PreviewMode = "rendered";
  private requestId = 0;

  constructor(
    leaf: WorkspaceLeaf,
    private readonly gitService: GitService
  ) {
    super(leaf);
  }

  getViewType(): string {
    return FILE_HISTORY_VIEW_TYPE;
  }

  getDisplayText(): string {
    return "Obsync file history";
  }

  getIcon(): string {
    return "history";
  }

  protected async onOpen(): Promise<void> {
    this.addAction("refresh-cw", "Refresh history", () => this.refresh());
    this.registerEvent(this.app.workspace.on("file-open", () => this.refreshCurrentFile()));
    this.registerEvent(this.app.workspace.on("active-leaf-change", () => this.refreshCurrentFile()));
    await this.refreshCurrentFile();
  }

  async refreshCurrentFile(): Promise<void> {
    const file = this.app.workspace.getActiveViewOfType(MarkdownView)?.file;
    if (!file) {
      if (!this.filePath) await this.refresh();
      return;
    }
    const nextPath = file?.path ?? null;
    if (nextPath === this.filePath) return;
    this.filePath = nextPath;
    this.history = [];
    this.loadedVersion = null;
    await this.refresh();
  }

  async refresh(): Promise<void> {
    const currentRequest = ++this.requestId;
    const { contentEl } = this;
    contentEl.empty();
    this.applyLayoutStyles(contentEl);
    this.renderHeader(contentEl);

    if (!this.filePath) {
      contentEl.createEl("p", { text: "Open a Markdown file to view its history." });
      return;
    }

    const body = contentEl.createDiv();
    body.style.display = "grid";
    body.style.gridTemplateColumns = "repeat(auto-fit, minmax(220px, 1fr))";
    body.style.gap = "12px";
    body.style.minHeight = "0";

    const listEl = body.createDiv();
    listEl.style.minWidth = "0";
    const previewEl = body.createDiv();
    previewEl.style.minWidth = "0";

    listEl.createEl("p", { text: "Loading history..." });
    try {
      this.history = await this.gitService.history(this.filePath);
      if (currentRequest !== this.requestId) return;
      this.renderHistoryList(listEl);
      await this.renderPreview(previewEl);
    } catch (error) {
      if (currentRequest !== this.requestId) return;
      const message = error instanceof Error ? error.message : String(error);
      listEl.empty();
      listEl.createEl("p", { text: `Could not load history: ${message}` });
      previewEl.empty();
    }
  }

  private renderHeader(container: HTMLElement): void {
    const header = container.createDiv();
    header.style.display = "flex";
    header.style.alignItems = "center";
    header.style.justifyContent = "space-between";
    header.style.gap = "8px";
    header.style.marginBottom = "12px";

    const titleWrap = header.createDiv();
    titleWrap.style.minWidth = "0";
    titleWrap.createEl("h2", { text: "File history" }).style.margin = "0";
    const pathEl = titleWrap.createEl("div", { text: this.filePath ?? "No active file" });
    pathEl.style.fontSize = "12px";
    pathEl.style.opacity = "0.75";
    pathEl.style.overflow = "hidden";
    pathEl.style.textOverflow = "ellipsis";
    pathEl.style.whiteSpace = "nowrap";

    const refreshButton = header.createEl("button", { attr: { type: "button", "aria-label": "Refresh history" } });
    refreshButton.style.width = "32px";
    refreshButton.style.height = "32px";
    refreshButton.style.display = "grid";
    refreshButton.style.placeItems = "center";
    setIcon(refreshButton, "refresh-cw");
    refreshButton.onclick = () => this.refresh();
  }

  private renderHistoryList(container: HTMLElement): void {
    container.empty();
    const listTitle = container.createEl("h3", { text: "Versions" });
    listTitle.style.marginTop = "0";

    if (this.history.length === 0) {
      container.createEl("p", { text: "No versions found for this file." });
      return;
    }

    const list = container.createEl("div");
    list.style.display = "grid";
    list.style.gap = "6px";
    list.style.maxHeight = "70vh";
    list.style.overflow = "auto";

    for (const entry of this.history.slice(0, 80)) {
      const button = list.createEl("button", { attr: { type: "button" } });
      button.style.textAlign = "left";
      button.style.padding = "8px";
      button.style.border = "1px solid var(--background-modifier-border)";
      button.style.borderRadius = "6px";
      button.style.background = this.loadedVersion?.entry.hash === entry.hash ? "var(--background-modifier-hover)" : "transparent";
      button.style.cursor = "pointer";
      button.createEl("div", { text: entry.subject || entry.hash.slice(0, 12) }).style.fontWeight = "600";
      const meta = button.createEl("div", { text: `${formatDate(entry.date)} · ${entry.hash.slice(0, 12)}` });
      meta.style.fontSize = "12px";
      meta.style.opacity = "0.75";
      button.onclick = () => this.loadVersion(entry);
    }
  }

  private async loadVersion(entry: HistoryEntry): Promise<void> {
    if (!this.filePath) return;
    const currentRequest = ++this.requestId;
    try {
      const version = await this.gitService.fileAtVersion(this.filePath, entry.hash);
      if (currentRequest !== this.requestId) return;
      const content = base64ToArrayBuffer(version.contentBase64);
      this.loadedVersion = { entry, version, text: decodeTextIfPossible(content) };
      await this.refresh();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      new Notice(`Could not open version: ${message}`);
    }
  }

  private async renderPreview(container: HTMLElement): Promise<void> {
    container.empty();
    const title = container.createEl("h3", { text: "Preview" });
    title.style.marginTop = "0";

    if (!this.loadedVersion) {
      container.createEl("p", { text: "Select a version to open it read-only." });
      return;
    }

    const { entry, version, text } = this.loadedVersion;
    const toolbar = container.createDiv();
    toolbar.style.display = "flex";
    toolbar.style.flexWrap = "wrap";
    toolbar.style.gap = "6px";
    toolbar.style.alignItems = "center";
    toolbar.style.marginBottom = "8px";

    toolbar.createEl("span", { text: `${formatDate(entry.date)} · ${version.hash.slice(0, 12)}` }).style.fontSize = "12px";
    this.renderModeButton(toolbar, "rendered", "Rendered");
    this.renderModeButton(toolbar, "markdown", "Markdown");

    const copyButton = toolbar.createEl("button", { text: text === null ? "Copy base64" : "Copy", attr: { type: "button" } });
    copyButton.onclick = async () => {
      await navigator.clipboard.writeText(text ?? version.contentBase64);
      new Notice(text === null ? "Version content copied as base64" : "Version text copied");
    };

    const preview = container.createDiv();
    preview.style.border = "1px solid var(--background-modifier-border)";
    preview.style.borderRadius = "6px";
    preview.style.padding = "12px";
    preview.style.maxHeight = "70vh";
    preview.style.overflow = "auto";

    if (text === null) {
      preview.createEl("p", { text: "This version is binary and cannot be rendered as Markdown." });
      preview.createEl("p", { text: `${version.sha256} · ${version.contentBase64.length} base64 characters` });
      return;
    }

    if (this.previewMode === "markdown") {
      const textarea = preview.createEl("textarea");
      textarea.value = text;
      textarea.readOnly = true;
      textarea.style.width = "100%";
      textarea.style.minHeight = "60vh";
      textarea.style.resize = "vertical";
      textarea.style.fontFamily = "var(--font-monospace)";
      return;
    }

    await MarkdownRenderer.render(this.app, text, preview, this.filePath ?? version.path, this);
  }

  private renderModeButton(container: HTMLElement, mode: PreviewMode, label: string): void {
    const button = container.createEl("button", { text: label, attr: { type: "button" } });
    button.style.fontWeight = this.previewMode === mode ? "700" : "400";
    button.onclick = async () => {
      this.previewMode = mode;
      await this.refresh();
    };
  }

  private applyLayoutStyles(container: HTMLElement): void {
    container.style.height = "100%";
    container.style.overflow = "auto";
  }
}

function decodeTextIfPossible(buffer: ArrayBuffer): string | null {
  const bytes = new Uint8Array(buffer);
  if (bytes.includes(0)) return null;
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}

function formatDate(date: string): string {
  const parsed = new Date(date);
  if (Number.isNaN(parsed.valueOf())) return date;
  return parsed.toLocaleString();
}
