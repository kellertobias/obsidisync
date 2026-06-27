import { ItemView, MarkdownRenderer, MarkdownView, Notice, setIcon, TFile, WorkspaceLeaf } from "obsidian";
import { base64ToArrayBuffer } from "./base64";
import { GitService } from "./gitService";
import { HistoryEntry, VersionFileResponse } from "./protocol";
import { sha256Hex } from "./vaultState";

export const FILE_HISTORY_VIEW_TYPE = "obsync-file-history";

type PreviewMode = "rendered" | "markdown";
type SyncState = "up-to-date" | "local-changes" | "server-newer" | "not-synced" | "unknown";

interface LoadedVersion {
  entry: HistoryEntry;
  version: VersionFileResponse;
  text: string | null;
}

interface FileSyncStatus {
  state: SyncState;
  title: string;
  detail: string;
  lastSaved: string;
  source: string;
}

interface VersionSource {
  device: string;
  label: string;
  relation: "this" | "other" | "server" | "unknown";
}

export class FileHistoryView extends ItemView {
  private filePath: string | null = null;
  private history: HistoryEntry[] = [];
  private loadedVersion: LoadedVersion | null = null;
  private fileStatus: FileSyncStatus | null = null;
  private previewMode: PreviewMode = "rendered";
  private requestId = 0;
  private syncing = false;

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
    this.registerEvent(
      this.app.workspace.on("file-open", (file) => {
        if (file) void this.showFile(file.path);
      })
    );
    this.registerEvent(
      this.app.workspace.on("active-leaf-change", (leaf) => {
        const view = leaf?.view;
        if (view instanceof MarkdownView && view.file) void this.showFile(view.file.path);
      })
    );
    await this.refreshCurrentFile();
  }

  async refreshCurrentFile(): Promise<void> {
    const file = this.app.workspace.getActiveViewOfType(MarkdownView)?.file;
    if (file) {
      await this.showFile(file.path);
      return;
    }
    if (!this.filePath) await this.refresh();
  }

  async showFile(path: string | null): Promise<void> {
    if (path === this.filePath) {
      await this.refresh();
      return;
    }
    this.filePath = path;
    this.history = [];
    this.loadedVersion = null;
    this.fileStatus = null;
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

    const statusEl = contentEl.createDiv();
    this.renderStatus(statusEl, null);

    const body = contentEl.createDiv();
    body.style.display = "grid";
    body.style.gridTemplateColumns = "repeat(auto-fit, minmax(240px, 1fr))";
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
      this.fileStatus = await this.computeFileStatus();
      if (currentRequest !== this.requestId) return;
      this.renderStatus(statusEl, this.fileStatus);
      this.renderHistoryList(listEl);
      await this.renderPreview(previewEl);
    } catch (error) {
      if (currentRequest !== this.requestId) return;
      const message = error instanceof Error ? error.message : String(error);
      this.renderStatus(statusEl, {
        state: "unknown",
        title: "Status unavailable",
        detail: message,
        lastSaved: "Unknown",
        source: "Unknown"
      });
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

  private renderStatus(container: HTMLElement, status: FileSyncStatus | null): void {
    container.empty();
    container.style.border = "1px solid var(--background-modifier-border)";
    container.style.borderLeft = `4px solid ${statusColor(status?.state ?? "unknown")}`;
    container.style.borderRadius = "8px";
    container.style.padding = "12px";
    container.style.marginBottom = "12px";
    container.style.background = "var(--background-secondary)";

    const top = container.createDiv();
    top.style.display = "flex";
    top.style.alignItems = "flex-start";
    top.style.justifyContent = "space-between";
    top.style.gap = "10px";

    const textWrap = top.createDiv();
    textWrap.style.minWidth = "0";
    const title = textWrap.createEl("div", { text: status?.title ?? "Checking sync status..." });
    title.style.fontWeight = "700";
    title.style.fontSize = "15px";
    if (status?.detail) {
      const detail = textWrap.createEl("div", { text: status.detail });
      detail.style.fontSize = "12px";
      detail.style.marginTop = "2px";
      detail.style.color = "var(--text-muted)";
    }

    const syncButton = top.createEl("button", { attr: { type: "button", "aria-label": "Sync now" } });
    syncButton.disabled = this.syncing;
    syncButton.style.display = "inline-flex";
    syncButton.style.alignItems = "center";
    syncButton.style.gap = "6px";
    syncButton.style.flexShrink = "0";
    setIcon(syncButton.createEl("span"), "refresh-cw");
    syncButton.createEl("span", { text: this.syncing ? "Syncing" : "Sync" });
    syncButton.onclick = () => this.syncNow();

    const meta = container.createDiv();
    meta.style.display = "grid";
    meta.style.gridTemplateColumns = "repeat(auto-fit, minmax(130px, 1fr))";
    meta.style.gap = "8px";
    meta.style.marginTop = "10px";

    this.renderMeta(meta, "Last saved", status?.lastSaved ?? "Checking...");
    this.renderMeta(meta, "Source", status?.source ?? "Checking...");
  }

  private renderMeta(container: HTMLElement, label: string, value: string): void {
    const item = container.createDiv();
    item.style.minWidth = "0";
    const labelEl = item.createEl("div", { text: label });
    labelEl.style.fontSize = "11px";
    labelEl.style.textTransform = "uppercase";
    labelEl.style.letterSpacing = "0";
    labelEl.style.color = "var(--text-muted)";
    const valueEl = item.createEl("div", { text: value });
    valueEl.style.fontSize = "13px";
    valueEl.style.fontWeight = "600";
    valueEl.style.overflow = "hidden";
    valueEl.style.textOverflow = "ellipsis";
    valueEl.style.whiteSpace = "nowrap";
    valueEl.title = value;
  }

  private renderHistoryList(container: HTMLElement): void {
    container.empty();
    const listTitle = container.createEl("h3", { text: "Synced versions" });
    listTitle.style.marginTop = "0";

    if (this.history.length === 0) {
      container.createEl("p", { text: "No synced versions for this file yet." });
      return;
    }

    const list = container.createEl("div");
    list.style.display = "grid";
    list.style.gap = "6px";
    list.style.maxHeight = "62vh";
    list.style.overflow = "auto";

    for (const [index, entry] of this.history.slice(0, 80).entries()) {
      const source = this.describeSource(entry);
      const button = list.createEl("button", { attr: { type: "button" } });
      button.style.textAlign = "left";
      button.style.padding = "9px";
      button.style.border = "1px solid var(--background-modifier-border)";
      button.style.borderRadius = "6px";
      button.style.background = this.loadedVersion?.entry.hash === entry.hash ? "var(--background-modifier-hover)" : "transparent";
      button.style.cursor = "pointer";
      button.style.width = "100%";

      const row = button.createDiv();
      row.style.display = "flex";
      row.style.alignItems = "center";
      row.style.justifyContent = "space-between";
      row.style.gap = "8px";
      row.createEl("div", { text: formatDate(entry.date) }).style.fontWeight = "700";
      if (index === 0) {
        const badge = row.createEl("span", { text: "Latest" });
        badge.style.fontSize = "11px";
        badge.style.padding = "1px 6px";
        badge.style.border = "1px solid var(--background-modifier-border)";
        badge.style.borderRadius = "999px";
        badge.style.color = "var(--text-muted)";
      }

      const sourceRow = button.createDiv();
      sourceRow.style.display = "flex";
      sourceRow.style.alignItems = "center";
      sourceRow.style.gap = "6px";
      sourceRow.style.marginTop = "4px";
      sourceRow.style.fontSize = "12px";
      sourceRow.style.color = "var(--text-muted)";
      setIcon(sourceRow.createEl("span"), sourceIcon(source.relation));
      sourceRow.createEl("span", { text: source.label });

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
      container.createEl("p", { text: "Select a synced version to preview it read-only." });
      return;
    }

    const { entry, version, text } = this.loadedVersion;
    const source = this.describeSource(entry);
    const toolbar = container.createDiv();
    toolbar.style.display = "flex";
    toolbar.style.flexWrap = "wrap";
    toolbar.style.gap = "6px";
    toolbar.style.alignItems = "center";
    toolbar.style.marginBottom = "8px";

    toolbar.createEl("span", { text: `${formatDate(entry.date)} - ${source.label}` }).style.fontSize = "12px";
    this.renderModeButton(toolbar, "rendered", "Rendered");
    this.renderModeButton(toolbar, "markdown", "Markdown");

    const copyButton = toolbar.createEl("button", { attr: { type: "button" } });
    copyButton.style.display = "inline-flex";
    copyButton.style.alignItems = "center";
    copyButton.style.gap = "6px";
    setIcon(copyButton.createEl("span"), "copy");
    copyButton.createEl("span", { text: text === null ? "Copy base64" : "Copy" });
    copyButton.onclick = async () => {
      await navigator.clipboard.writeText(text ?? version.contentBase64);
      new Notice(text === null ? "Version content copied as base64" : "Version text copied");
    };

    const preview = container.createDiv();
    preview.style.border = "1px solid var(--background-modifier-border)";
    preview.style.borderRadius = "6px";
    preview.style.padding = "12px";
    preview.style.maxHeight = "62vh";
    preview.style.overflow = "auto";

    if (text === null) {
      preview.createEl("p", { text: "This version is binary and cannot be rendered as Markdown." });
      preview.createEl("p", { text: `${version.sha256} - ${version.contentBase64.length} base64 characters` });
      return;
    }

    if (this.previewMode === "markdown") {
      const textarea = preview.createEl("textarea");
      textarea.value = text;
      textarea.readOnly = true;
      textarea.style.width = "100%";
      textarea.style.minHeight = "52vh";
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

  private async computeFileStatus(): Promise<FileSyncStatus> {
    const file = this.currentFile();
    const localSaved = file ? formatDateFromMs(file.stat.mtime) : "Unknown";
    const currentSource = this.currentDeviceSource();

    if (!this.filePath || this.history.length === 0) {
      return {
        state: "not-synced",
        title: "Not synced yet",
        detail: "This file has no saved server version.",
        lastSaved: localSaved,
        source: currentSource
      };
    }

    const latest = this.history[0];
    const latestSource = this.describeSource(latest);
    const latestSaved = formatDate(latest.date);

    try {
      const [localBuffer, latestVersion] = await Promise.all([
        this.app.vault.adapter.readBinary(this.filePath),
        this.gitService.fileAtVersion(this.filePath, latest.hash)
      ]);
      const localSha = await sha256Hex(localBuffer);
      if (localSha === latestVersion.sha256) {
        return {
          state: "up-to-date",
          title: "Up to date",
          detail: "The open file matches the latest synced version.",
          lastSaved: latestSaved,
          source: latestSource.label
        };
      }

      const latestTime = Date.parse(latest.date);
      const localIsNewer = file && !Number.isNaN(latestTime) && file.stat.mtime > latestTime + 1000;
      if (localIsNewer) {
        return {
          state: "local-changes",
          title: "Local changes not synced",
          detail: `Latest synced version: ${latestSaved} from ${latestSource.label}.`,
          lastSaved: localSaved,
          source: currentSource
        };
      }

      return {
        state: "server-newer",
        title: "Server version differs",
        detail: "Sync to update this device or resolve conflicts if both sides changed.",
        lastSaved: latestSaved,
        source: latestSource.label
      };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        state: "unknown",
        title: "Status unavailable",
        detail: message,
        lastSaved: latestSaved,
        source: latestSource.label
      };
    }
  }

  private currentFile(): TFile | null {
    if (!this.filePath) return null;
    const file = this.app.vault.getAbstractFileByPath(this.filePath);
    return file instanceof TFile ? file : null;
  }

  private currentDeviceSource(): string {
    return `This device - ${this.gitService.currentDeviceName()}`;
  }

  private describeSource(entry: HistoryEntry): VersionSource {
    const device = entry.deviceName?.trim() || extractSyncDevice(entry.subject) || entry.author.trim() || "Unknown device";
    if (isServerSource(device)) {
      return { device: "Server", label: "Server", relation: "server" };
    }

    const current = this.gitService.currentDeviceName();
    if (sameDevice(device, current)) {
      return { device, label: `This device - ${device}`, relation: "this" };
    }
    return { device, label: `Other device - ${device}`, relation: "other" };
  }

  private async syncNow(): Promise<void> {
    if (this.syncing) return;
    this.syncing = true;
    await this.refresh();
    try {
      await this.gitService.sync();
    } catch {
      // GitService already shows a Notice with the failure reason.
    } finally {
      this.syncing = false;
      await this.refresh();
    }
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

function extractSyncDevice(subject: string): string | null {
  const trimmed = subject.trim();
  if (trimmed === "sync: server pending") return "Server";

  let rest: string | null = null;
  if (trimmed.startsWith("sync: resolve ")) {
    rest = trimmed.slice("sync: resolve ".length);
  } else if (trimmed.startsWith("sync: ")) {
    rest = trimmed.slice("sync: ".length);
  }
  if (!rest) return null;

  const systemTimeIndex = rest.indexOf(" SystemTime ");
  const device = (systemTimeIndex >= 0 ? rest.slice(0, systemTimeIndex) : rest).trim();
  return device || null;
}

function sameDevice(left: string, right: string): boolean {
  return normalizeDevice(left) === normalizeDevice(right);
}

function normalizeDevice(device: string): string {
  return device.trim().replace(/\s+/g, " ").toLocaleLowerCase();
}

function isServerSource(device: string): boolean {
  const normalized = normalizeDevice(device);
  return normalized === "server" || normalized === "server pending";
}

function sourceIcon(relation: VersionSource["relation"]): string {
  if (relation === "server") return "server";
  if (relation === "this") return "monitor-check";
  if (relation === "other") return "monitor";
  return "circle-help";
}

function statusColor(state: SyncState): string {
  if (state === "up-to-date") return "var(--color-green)";
  if (state === "local-changes") return "var(--color-orange)";
  if (state === "server-newer") return "var(--color-yellow)";
  if (state === "not-synced") return "var(--text-accent)";
  return "var(--background-modifier-border-hover)";
}

function formatDate(date: string): string {
  const parsed = new Date(date);
  if (Number.isNaN(parsed.valueOf())) return date;
  return parsed.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

function formatDateFromMs(timestamp: number): string {
  if (!Number.isFinite(timestamp) || timestamp <= 0) return "Unknown";
  return new Date(timestamp).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}
