import { ItemView, MarkdownView, Notice, setIcon, TFile, WorkspaceLeaf } from "obsidian";
import { base64ToArrayBuffer } from "./base64";
import { GitService } from "./gitService";
import { HistoryEntry } from "./protocol";
import { sha256Hex } from "./vaultState";

export const FILE_HISTORY_VIEW_TYPE = "obsync-file-history";
const HISTORY_SNAPSHOT_DIR = ".obsidian-git-sync/history";

type SyncState = "up-to-date" | "local-changes" | "server-newer" | "not-synced" | "unknown";

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
  private fileStatus: FileSyncStatus | null = null;
  private selectedHash: string | null = null;
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
    this.registerEvent(
      this.app.workspace.on("file-open", (file) => {
        if (file && !isHistorySnapshotPath(file.path)) void this.showFile(file.path);
      })
    );
    this.registerEvent(
      this.app.workspace.on("active-leaf-change", (leaf) => {
        const view = leaf?.view;
        if (view instanceof MarkdownView && view.file && !isHistorySnapshotPath(view.file.path)) void this.showFile(view.file.path);
      })
    );
    await this.refreshCurrentFile();
  }

  async refreshCurrentFile(): Promise<void> {
    const file = this.app.workspace.getActiveViewOfType(MarkdownView)?.file;
    if (file && !isHistorySnapshotPath(file.path)) {
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
    this.selectedHash = null;
    this.fileStatus = null;
    await this.refresh();
  }

  async refresh(): Promise<void> {
    const currentRequest = ++this.requestId;
    const { contentEl } = this;
    contentEl.empty();
    this.applyLayoutStyles(contentEl);

    if (!this.filePath) {
      contentEl.createEl("p", { text: "Open a Markdown file to view its history." });
      return;
    }

    const statusEl = contentEl.createDiv();
    this.renderStatus(statusEl, null);

    const listEl = contentEl.createDiv();
    listEl.style.minWidth = "0";

    listEl.createEl("p", { text: "Loading history..." });
    try {
      this.history = await this.gitService.history(this.filePath);
      if (currentRequest !== this.requestId) return;
      this.fileStatus = await this.computeFileStatus();
      if (currentRequest !== this.requestId) return;
      this.renderStatus(statusEl, this.fileStatus);
      this.renderHistoryList(listEl);
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
    }
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

    if (this.history.length === 0) {
      container.createEl("p", { text: "No synced versions for this file yet." });
      return;
    }

    const list = container.createEl("div");
    list.style.display = "grid";
    list.style.gap = "6px";
    list.style.maxHeight = "62vh";
    list.style.overflow = "auto";

    for (const entry of this.history.slice(0, 80)) {
      const source = this.describeSource(entry);
      const button = list.createEl("button", { attr: { type: "button" } });
      button.style.textAlign = "left";
      button.style.padding = "9px";
      button.style.border = "1px solid var(--background-modifier-border)";
      button.style.borderRadius = "6px";
      button.style.background = this.selectedHash === entry.hash ? "var(--background-modifier-hover)" : "transparent";
      button.style.cursor = "pointer";
      button.style.width = "100%";

      const row = button.createDiv();
      row.style.display = "grid";
      row.style.gridTemplateColumns = "minmax(0, 1fr) minmax(80px, auto)";
      row.style.alignItems = "center";
      row.style.gap = "12px";
      const dateEl = row.createEl("div", { text: formatDate(entry.date) });
      dateEl.style.fontWeight = "700";
      dateEl.style.overflow = "hidden";
      dateEl.style.textOverflow = "ellipsis";
      dateEl.style.whiteSpace = "nowrap";
      const deviceEl = row.createEl("div", { text: source.device });
      deviceEl.style.fontSize = "12px";
      deviceEl.style.color = "var(--text-muted)";
      deviceEl.style.textAlign = "right";
      deviceEl.style.overflow = "hidden";
      deviceEl.style.textOverflow = "ellipsis";
      deviceEl.style.whiteSpace = "nowrap";
      deviceEl.title = source.label;

      button.onclick = () => this.openVersion(entry);
    }
  }

  private async openVersion(entry: HistoryEntry): Promise<void> {
    if (!this.filePath) return;
    const currentRequest = ++this.requestId;
    try {
      const version = await this.gitService.fileAtVersion(this.filePath, entry.hash);
      if (currentRequest !== this.requestId) return;
      const content = base64ToArrayBuffer(version.contentBase64);
      this.selectedHash = entry.hash;
      const snapshot = await this.writeVersionSnapshot(entry, content);
      const leaf = this.app.workspace.getLeaf("tab");
      await leaf.openFile(snapshot, { active: true, state: { mode: "preview" } });
      await this.refresh();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      new Notice(`Could not open version: ${message}`);
    }
  }

  private async writeVersionSnapshot(entry: HistoryEntry, content: ArrayBuffer): Promise<TFile> {
    await this.ensureSnapshotFolder();
    const path = snapshotPath(this.filePath ?? "version", entry);
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      await this.app.vault.modifyBinary(existing, content);
      return existing;
    }
    return this.app.vault.createBinary(path, content);
  }

  private async ensureSnapshotFolder(): Promise<void> {
    let current = "";
    for (const part of HISTORY_SNAPSHOT_DIR.split("/")) {
      current = current ? `${current}/${part}` : part;
      if (!(await this.app.vault.adapter.exists(current, true))) {
        await this.app.vault.createFolder(current);
      }
    }
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

function snapshotPath(originalPath: string, entry: HistoryEntry): string {
  const name = sanitizeSnapshotName(originalPath);
  return `${HISTORY_SNAPSHOT_DIR}/${entry.hash.slice(0, 12)}-${name}`;
}

function sanitizeSnapshotName(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  const fallback = normalized || "version.md";
  return fallback
    .split("/")
    .filter(Boolean)
    .join(" - ")
    .replace(/[\0:*?"<>|]/g, "-")
    .replace(/\s+/g, " ")
    .slice(0, 120);
}

function isHistorySnapshotPath(path: string): boolean {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  return normalized === HISTORY_SNAPSHOT_DIR || normalized.startsWith(`${HISTORY_SNAPSHOT_DIR}/`);
}
