import { App, ItemView, MarkdownView, Modal, Notice, setIcon, Setting, TFile, WorkspaceLeaf } from "obsidian";
import { base64ToArrayBuffer } from "./base64";
import { GitService } from "./gitService";
import { HISTORY_SNAPSHOT_DIR } from "./ignore";
import { HistoryEntry } from "./protocol";
import { sha256Hex } from "./vaultState";

export const FILE_HISTORY_VIEW_TYPE = "obsidisync-file-history";

type SyncState = "up-to-date" | "local-changes" | "server-newer" | "not-synced" | "unknown";

interface FileSyncStatus {
  state: SyncState;
  title: string;
  detail: string;
  lastSaved: string;
  source: string;
  hasConflict: boolean;
}

interface VersionSource {
  device: string;
  label: string;
  relation: "this" | "other" | "server" | "unknown";
}

export interface HistorySnapshotReference {
  snapshotPath: string;
  sourcePath: string;
  hash: string;
}

export interface HistorySnapshotStore {
  get(path: string): HistorySnapshotReference | null;
  save(reference: HistorySnapshotReference): Promise<void>;
  getVersionName(sourcePath: string, hash: string): string | null;
  saveVersionName(sourcePath: string, hash: string, name: string | null): Promise<void>;
  isVersionSquashed(sourcePath: string, hash: string): boolean;
  squashVersion(sourcePath: string, hash: string, intoHash: string): Promise<void>;
  lastSyncedAt(): string | null;
  openConflictResolver(): void;
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
    private readonly gitService: GitService,
    private readonly snapshots: HistorySnapshotStore
  ) {
    super(leaf);
  }

  getViewType(): string {
    return FILE_HISTORY_VIEW_TYPE;
  }

  getDisplayText(): string {
    return "ObsidiSync file history";
  }

  getIcon(): string {
    return "history";
  }

  protected async onOpen(): Promise<void> {
    this.register(
      this.gitService.onSyncStateChange((running) => {
        if (this.syncing === running) return;
        this.syncing = running;
        void this.refresh();
      })
    );
    this.registerEvent(
      this.app.workspace.on("file-open", (file) => {
        void this.showFile(file?.path ?? null);
      })
    );
    this.registerEvent(
      this.app.workspace.on("active-leaf-change", (leaf) => {
        const view = leaf?.view;
        if (view instanceof MarkdownView && view.file) {
          void this.showFile(view.file.path);
        }
      })
    );
    this.registerEvent(
      this.app.vault.on("modify", (file) => {
        if (!(file instanceof TFile) || file.path !== this.filePath) return;
        void this.refresh();
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
    await this.showFile(null);
  }

  async showFile(path: string | null): Promise<void> {
    const resolved = this.resolveHistorySnapshot(path);
    if (path && !resolved.path && this.filePath) {
      await this.refresh();
      return;
    }

    if (resolved.path === this.filePath) {
      this.selectedHash = resolved.hash;
      await this.refresh();
      return;
    }

    this.filePath = resolved.path;
    this.history = [];
    this.selectedHash = resolved.hash;
    this.fileStatus = null;
    await this.refresh();
  }

  async refresh(): Promise<void> {
    const currentRequest = ++this.requestId;
    const { contentEl } = this;
    contentEl.empty();
    this.applyLayoutStyles(contentEl);

    if (!this.filePath) {
      this.renderNoActiveFile(contentEl);
      return;
    }

    const statusEl = contentEl.createDiv();
    statusEl.style.flex = "0 0 auto";
    statusEl.style.position = "sticky";
    statusEl.style.top = "0";
    statusEl.style.zIndex = "1";
    this.renderStatus(statusEl, null);

    const listEl = contentEl.createDiv();
    listEl.style.flex = "1 1 auto";
    listEl.style.minHeight = "0";
    listEl.style.minWidth = "0";
    listEl.style.overflow = "hidden";

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
        source: "Unknown",
        hasConflict: false
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
    if (this.syncing) {
      const running = textWrap.createEl("div", { text: "Sync is running..." });
      running.style.fontSize = "12px";
      running.style.marginTop = "2px";
      running.style.color = "var(--text-accent)";
      running.style.fontWeight = "600";
    }
    if (status?.detail) {
      const detail = textWrap.createEl("div", { text: status.detail });
      detail.style.fontSize = "12px";
      detail.style.marginTop = "2px";
      detail.style.color = "var(--text-muted)";
    }

    const actions = top.createDiv();
    actions.style.display = "inline-flex";
    actions.style.alignItems = "center";
    actions.style.gap = "6px";
    actions.style.flexShrink = "0";

    if (status?.hasConflict) {
      const resolveButton = actions.createEl("button", { attr: { type: "button", "aria-label": "Resolve conflicts" } });
      resolveButton.style.display = "inline-flex";
      resolveButton.style.alignItems = "center";
      resolveButton.style.gap = "6px";
      setIcon(resolveButton.createEl("span"), "git-pull-request");
      resolveButton.createEl("span", { text: "Resolve" });
      resolveButton.onclick = () => this.snapshots.openConflictResolver();
    }

    const syncButton = actions.createEl("button", { attr: { type: "button", "aria-label": "Sync now" } });
    syncButton.disabled = this.syncing;
    syncButton.style.display = "inline-flex";
    syncButton.style.alignItems = "center";
    syncButton.style.gap = "6px";
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

  private renderNoActiveFile(container: HTMLElement): void {
    const empty = container.createDiv();
    empty.style.flex = "1 1 auto";
    empty.style.minHeight = "0";
    empty.style.display = "flex";
    empty.style.flexDirection = "column";
    empty.style.alignItems = "center";
    empty.style.justifyContent = "center";
    empty.style.gap = "10px";
    empty.style.textAlign = "center";

    const lastSynced = empty.createEl("div", { text: `Last Synced: ${formatLastSynced(this.snapshots.lastSyncedAt())}` });
    lastSynced.style.fontWeight = "700";

    const syncButton = empty.createEl("button", { text: this.syncing ? "Syncing" : "Sync Now", attr: { type: "button" } });
    syncButton.disabled = this.syncing;
    syncButton.onclick = () => this.syncNow();
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
    list.style.alignContent = "start";
    list.style.gap = "6px";
    list.style.height = "100%";
    list.style.minHeight = "0";
    list.style.overflow = "auto";

    const visibleHistory = this.history.filter((entry) => !this.isVersionSquashed(entry)).slice(0, 80);

    for (const [index, entry] of visibleHistory.entries()) {
      const versionNumber = visibleHistory.length - index;
      const source = this.describeSource(entry);
      const item = list.createDiv();
      item.style.display = "grid";
      item.style.gridTemplateColumns = "minmax(0, 1fr) auto";
      item.style.gap = "6px";
      item.style.alignItems = "stretch";

      const button = item.createEl("button", { attr: { type: "button" } });
      button.style.textAlign = "left";
      button.style.padding = "9px";
      button.style.border = "1px solid var(--background-modifier-border)";
      button.style.borderRadius = "6px";
      button.style.background = this.selectedHash === entry.hash ? "var(--background-modifier-hover)" : "transparent";
      button.style.cursor = "pointer";
      button.style.display = "flex";
      button.style.alignItems = "center";
      button.style.justifyContent = "stretch";
      button.style.minHeight = "38px";
      button.style.width = "100%";

      const row = button.createDiv();
      row.style.display = "grid";
      row.style.gridTemplateColumns = "minmax(0, 1fr) minmax(80px, auto)";
      row.style.alignItems = "center";
      row.style.gap = "12px";
      row.style.flex = "1 1 auto";
      row.style.width = "100%";

      const titleWrap = row.createDiv();
      titleWrap.style.minWidth = "0";
      const name = this.versionName(entry);
      const versionEl = titleWrap.createEl("div", { text: name ? `Version ${versionNumber}: ${name}` : `Version ${versionNumber}` });
      versionEl.style.fontWeight = "700";
      versionEl.style.color = this.selectedHash === entry.hash ? "var(--text-accent)" : "var(--text-normal)";
      versionEl.style.overflow = "hidden";
      versionEl.style.textOverflow = "ellipsis";
      versionEl.style.whiteSpace = "nowrap";
      versionEl.title = name ? `Version ${versionNumber}: ${name}` : `Version ${versionNumber}`;
      const dateEl = titleWrap.createEl("div", { text: formatDate(entry.date) });
      dateEl.style.display = "flex";
      dateEl.style.alignItems = "center";
      dateEl.style.justifyContent = "flex-start";
      dateEl.style.fontSize = "12px";
      dateEl.style.color = "var(--text-muted)";
      dateEl.style.textAlign = "left";
      dateEl.style.overflow = "hidden";
      dateEl.style.textOverflow = "ellipsis";
      dateEl.style.whiteSpace = "nowrap";
      const deviceEl = row.createEl("div", { text: source.device });
      deviceEl.style.display = "flex";
      deviceEl.style.alignItems = "center";
      deviceEl.style.justifyContent = "flex-end";
      deviceEl.style.justifySelf = "end";
      deviceEl.style.fontSize = "12px";
      deviceEl.style.color = "var(--text-muted)";
      deviceEl.style.textAlign = "right";
      deviceEl.style.overflow = "hidden";
      deviceEl.style.textOverflow = "ellipsis";
      deviceEl.style.whiteSpace = "nowrap";
      deviceEl.title = source.label;

      button.onclick = () => this.openVersion(entry, versionNumber);

      const actions = item.createDiv();
      actions.style.display = "flex";
      actions.style.alignItems = "center";
      actions.style.gap = "4px";

      const nameButton = this.createIconButton(actions, "pencil", "Name version");
      nameButton.onclick = () => this.nameVersion(entry);

      if (index > 0) {
        const intoVersionNumber = visibleHistory.length - index + 1;
        const squashButton = this.createIconButton(actions, "combine", `Squash Version ${versionNumber} into Version ${intoVersionNumber}`);
        squashButton.onclick = () => this.squashVersion(entry, visibleHistory[index - 1], versionNumber);
      }
    }
  }

  private createIconButton(container: HTMLElement, icon: string, label: string): HTMLButtonElement {
    const button = container.createEl("button", { attr: { type: "button", "aria-label": label, title: label } });
    button.style.display = "inline-flex";
    button.style.alignItems = "center";
    button.style.justifyContent = "center";
    button.style.width = "32px";
    button.style.height = "32px";
    button.style.padding = "0";
    setIcon(button, icon);
    return button;
  }

  private versionName(entry: HistoryEntry): string | null {
    if (!this.filePath) return null;
    return this.snapshots.getVersionName(this.filePath, entry.hash);
  }

  private isVersionSquashed(entry: HistoryEntry): boolean {
    if (!this.filePath) return false;
    return this.snapshots.isVersionSquashed(this.filePath, entry.hash);
  }

  private nameVersion(entry: HistoryEntry): void {
    if (!this.filePath) return;
    const sourcePath = this.filePath;
    const hash = entry.hash;
    const current = this.versionName(entry) ?? "";
    new VersionNameModal(this.app, current, async (name) => {
      const trimmed = name.trim();
      await this.snapshots.saveVersionName(sourcePath, hash, trimmed || null);
      await this.refresh();
    }).open();
  }

  private async squashVersion(entry: HistoryEntry, intoEntry: HistoryEntry, versionNumber: number): Promise<void> {
    if (!this.filePath) return;
    const ok = window.confirm(`Squash Version ${versionNumber} into a newer version? This only collapses the local history list.`);
    if (!ok) return;
    await this.snapshots.squashVersion(this.filePath, entry.hash, intoEntry.hash);
    if (this.selectedHash === entry.hash) this.selectedHash = intoEntry.hash;
    await this.refresh();
  }

  private async openVersion(entry: HistoryEntry, versionNumber: number): Promise<void> {
    if (!this.filePath) return;
    const currentRequest = ++this.requestId;
    try {
      const version = await this.gitService.fileAtVersion(this.filePath, entry.hash);
      if (currentRequest !== this.requestId) return;
      const content = base64ToArrayBuffer(version.contentBase64);
      this.selectedHash = entry.hash;
      const snapshot = await this.writeVersionSnapshot(entry, content, versionNumber);
      const leaf = this.app.workspace.getLeaf("tab");
      await leaf.openFile(snapshot, { active: true, state: { mode: "preview" } });
      await this.refresh();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      new Notice(`Could not open version: ${message}`);
    }
  }

  private async writeVersionSnapshot(entry: HistoryEntry, content: ArrayBuffer, versionNumber: number): Promise<TFile> {
    await this.ensureSnapshotFolder();
    const source = this.describeSource(entry);

    for (let attempt = 0; attempt < 100; attempt += 1) {
      const path = snapshotPath(this.filePath ?? "version", entry, source.device, versionNumber, attempt);
      const existing = this.app.vault.getAbstractFileByPath(path);
      if (existing instanceof TFile) {
        await this.app.vault.modifyBinary(existing, content);
        await this.saveSnapshotReference(existing.path, entry.hash);
        return existing;
      }

      if (await this.app.vault.adapter.exists(path, true)) continue;
      const created = await this.app.vault.createBinary(path, content);
      await this.saveSnapshotReference(created.path, entry.hash);
      return created;
    }

    throw new Error("Could not create a unique history snapshot file");
  }

  private async saveSnapshotReference(snapshotPath: string, hash: string): Promise<void> {
    if (!this.filePath) return;
    await this.snapshots.save({ snapshotPath, sourcePath: this.filePath, hash });
  }

  private resolveHistorySnapshot(path: string | null): { path: string | null; hash: string | null } {
    if (!path || !isHistorySnapshotPath(path)) return { path, hash: null };

    const reference = this.snapshots.get(path);
    if (!reference) return { path: null, hash: null };

    return { path: reference.sourcePath, hash: reference.hash };
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
    const hasConflict = await this.fileHasConflictMarkers(file);

    if (!this.filePath || this.history.length === 0) {
      return {
        state: "not-synced",
        title: "Not synced yet",
        detail: "This file has no saved server version.",
        lastSaved: localSaved,
        source: currentSource,
        hasConflict
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
          source: latestSource.label,
          hasConflict
        };
      }

      const latestTime = Date.parse(latest.date);
      const localIsNewer = file && !Number.isNaN(latestTime) && file.stat.mtime > latestTime + 1000;
      if (localIsNewer) {
        return {
          state: "local-changes",
          title: "File changed",
          detail: `Latest synced version: ${latestSaved} from ${latestSource.label}.`,
          lastSaved: localSaved,
          source: currentSource,
          hasConflict
        };
      }

      return {
        state: "server-newer",
        title: "Server version differs",
        detail: "Sync to update this device or resolve conflicts if both sides changed.",
        lastSaved: latestSaved,
        source: latestSource.label,
        hasConflict
      };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        state: "unknown",
        title: "Status unavailable",
        detail: message,
        lastSaved: latestSaved,
        source: latestSource.label,
        hasConflict
      };
    }
  }

  private async fileHasConflictMarkers(file: TFile | null): Promise<boolean> {
    if (!file) return false;
    try {
      return hasConflictMarkers(await this.app.vault.cachedRead(file));
    } catch {
      return false;
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
    if (this.gitService.isSyncRunning()) return;
    try {
      await this.gitService.sync();
    } catch {
      // GitService already shows a Notice with the failure reason.
    }
  }

  private applyLayoutStyles(container: HTMLElement): void {
    container.style.height = "100%";
    container.style.display = "flex";
    container.style.flexDirection = "column";
    container.style.minHeight = "0";
    container.style.overflow = "hidden";
  }
}

class VersionNameModal extends Modal {
  private value: string;

  constructor(
    app: App,
    currentName: string,
    private readonly saveName: (name: string) => Promise<void>
  ) {
    super(app);
    this.value = currentName;
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Name version" });

    let input: HTMLInputElement | null = null;
    new Setting(contentEl).setName("Name").addText((text) => {
      input = text.inputEl;
      text.setValue(this.value).onChange((value) => {
        this.value = value;
      });
      text.inputEl.addEventListener("keydown", (event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          void this.save();
        }
      });
    });

    new Setting(contentEl).addButton((button) =>
      button
        .setCta()
        .setButtonText("Save")
        .onClick(() => {
          void this.save();
        })
    );

    window.setTimeout(() => {
      input?.focus();
      input?.select();
    }, 0);
  }

  private async save(): Promise<void> {
    await this.saveName(this.value);
    this.close();
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

function hasConflictMarkers(content: string): boolean {
  return content.includes("<<<<<<<") && content.includes("=======") && content.includes(">>>>>>>");
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

function formatLastSynced(date: string | null): string {
  if (!date) return "Never";
  return formatDate(date);
}

function snapshotPath(originalPath: string, entry: HistoryEntry, computerName: string, versionNumber: number, attempt = 0): string {
  const title = snapshotTitle(originalPath);
  const extension = snapshotExtension(originalPath);
  const version = String(Math.max(1, versionNumber)).padStart(2, "0");
  const date = formatSnapshotDate(entry.date);
  const computer = sanitizeSnapshotComponent(computerName || "Unknown computer");
  const suffix = attempt === 0 ? "" : `-${attempt + 1}`;
  return `${HISTORY_SNAPSHOT_DIR}/Version ${version} - ${date} - ${computer} - ${title}${suffix}${extension}`;
}

function snapshotTitle(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  const name = normalized.split("/").filter(Boolean).pop() || "Untitled.md";
  const dotIndex = name.lastIndexOf(".");
  const title = dotIndex > 0 ? name.slice(0, dotIndex) : name;
  return sanitizeSnapshotComponent(title || "Untitled");
}

function snapshotExtension(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  const name = normalized.split("/").filter(Boolean).pop() || "Untitled.md";
  const dotIndex = name.lastIndexOf(".");
  if (dotIndex <= 0 || dotIndex === name.length - 1) return ".md";
  return sanitizeSnapshotComponent(name.slice(dotIndex)).replace(/\s/g, "") || ".md";
}

function sanitizeSnapshotComponent(value: string): string {
  return value.replace(/[\\/\0:*?"<>|]/g, "-").replace(/\s+/g, " ").trim().slice(0, 80) || "Untitled";
}

function formatSnapshotDate(date: string): string {
  const parsed = new Date(date);
  if (Number.isNaN(parsed.valueOf())) return sanitizeSnapshotComponent(date);
  const year = parsed.getFullYear();
  const month = String(parsed.getMonth() + 1).padStart(2, "0");
  const day = String(parsed.getDate()).padStart(2, "0");
  const hour = String(parsed.getHours()).padStart(2, "0");
  const minute = String(parsed.getMinutes()).padStart(2, "0");
  return `${year}-${month}-${day} ${hour}-${minute}`;
}

function isHistorySnapshotPath(path: string): boolean {
  const normalized = path.replace(/\\/g, "/").replace(/^\/+/, "");
  return normalized === HISTORY_SNAPSHOT_DIR || normalized.startsWith(`${HISTORY_SNAPSHOT_DIR}/`);
}
