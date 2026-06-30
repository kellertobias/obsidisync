import { MarkdownView, Menu, Notice, Platform, Plugin } from "obsidian";
import { AuthLoginModal } from "./authLoginModal";
import { ComputerNameModal } from "./computerNameModal";
import { ConflictResolverModal } from "./conflictResolverModal";
import { FILE_HISTORY_VIEW_TYPE, FileHistoryView, HistorySnapshotReference } from "./fileHistoryView";
import { GitService } from "./gitService";
import { InitialSyncModal } from "./initialSyncModal";
import { OidcDeviceLoginModal } from "./oidcModal";
import { ServerInfoResponse, SyncConflict } from "./protocol";
import { createClientId, generateComputerName, slugFromName } from "./runtime";
import { DEFAULT_SETTINGS, IosGitSyncSettings, IosGitSyncSettingTab } from "./settings";
import { sha256Hex } from "./vaultState";

export default class ObsidiSyncPlugin extends Plugin {
  settings: IosGitSyncSettings;
  private gitService: GitService;
  private timer: number | null = null;
  private conflictResolverOpen = false;
  private initialSyncModalOpen = false;
  private mobileSyncIndicatorEl: HTMLElement | null = null;
  private mobileSyncIndicatorRequestId = 0;
  private lastActiveFilePath: string | null = null;
  private pendingCloseSyncPaths = new Set<string>();
  private appCloseSyncStarted = false;

  async onload(): Promise<void> {
    await this.loadSettings();
    let shouldSaveSettings = false;
    if (!this.settings.clientId) {
      this.settings.clientId = createClientId();
      shouldSaveSettings = true;
    }
    if (!this.settings.vaultSlug) {
      this.settings.vaultSlug = slugFromName(this.app.vault.getName(), "personal");
      shouldSaveSettings = true;
    }
    if (!this.settings.deviceName) {
      this.settings.deviceName = generateComputerName();
      shouldSaveSettings = true;
    }
    if (this.settings.branch !== DEFAULT_SETTINGS.branch) {
      this.settings.branch = DEFAULT_SETTINGS.branch;
      shouldSaveSettings = true;
    }
    if (shouldSaveSettings) await this.saveSettings();

    this.gitService = new GitService(
      this.app.vault,
      this.settings,
      () => this.saveSettings(),
      (conflicts) => this.openConflictResolver(conflicts),
      () => (this.conflictResolverOpen ? "Finish conflict resolution before starting another sync." : null)
    );

    this.registerView(
      FILE_HISTORY_VIEW_TYPE,
      (leaf) =>
        new FileHistoryView(leaf, this.gitService, {
          get: (path) => this.snapshotReference(path),
          save: (reference) => this.saveSnapshotReference(reference),
          getVersionName: (sourcePath, hash) => this.versionMetadata(sourcePath, hash)?.name?.trim() || null,
          saveVersionName: (sourcePath, hash, name) => this.saveVersionName(sourcePath, hash, name),
          isVersionSquashed: (sourcePath, hash) => Boolean(this.versionMetadata(sourcePath, hash)?.squashedIntoHash),
          squashVersion: (sourcePath, hash, intoHash) => this.squashVersion(sourcePath, hash, intoHash),
          lastSyncedAt: () => this.settings.lastSyncedAt,
          openConflictResolver: () => this.openConflictResolver()
        })
    );

    this.addSettingTab(new IosGitSyncSettingTab(this.app, this));
    this.addRibbonIcon("refresh-cw", "Sync vault", () => this.runCommand(() => this.syncNow()));
    this.addRibbonIcon("history", "Open file history", () => this.openFileHistoryView());
    this.setupMobileSyncIndicator();
    this.setupSyncOnClose();

    this.addCommand({
      id: "sync-now",
      name: "Sync now",
      callback: () => this.runCommand(() => this.syncNow())
    });

    this.addCommand({
      id: "set-computer-name",
      name: "Set computer name",
      callback: () =>
        new ComputerNameModal(this.app, this.settings.deviceName, async (name) => {
          this.settings.deviceName = name;
          await this.saveSettings();
        }).open()
    });

    this.addCommand({
      id: "login",
      name: "Log in to ObsidiSync",
      callback: () => this.openLoginModal()
    });

    this.addCommand({
      id: "oidc-device-login",
      name: "Start OIDC device login",
      callback: () => new OidcDeviceLoginModal(this.app, this.gitService).open()
    });

    this.addCommand({
      id: "reset-sync-state",
      name: "Reset local sync state",
      callback: () => this.runCommand(() => this.resetLocalSyncState())
    });

    this.addCommand({
      id: "show-current-file-versions",
      name: "Open file history view",
      callback: () => this.openFileHistoryView()
    });

    this.addCommand({
      id: "resolve-current-conflict-file",
      name: "Open conflict resolver",
      callback: () => this.openConflictResolver()
    });

    this.resetSyncTimer();

    if (this.settings.syncOnStartup) {
      this.app.workspace.onLayoutReady(() => {
        window.setTimeout(() => {
          this.runCommand(() => this.syncNow());
        }, 1500);
      });
    }
  }

  onunload(): void {
    if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }
  }

  async loadSettings(): Promise<void> {
    const loaded = ((await this.loadData()) ?? {}) as Partial<IosGitSyncSettings> & { authToken?: string; vaultId?: string };
    this.settings = Object.assign({}, DEFAULT_SETTINGS, loaded);
    this.settings.branch = DEFAULT_SETTINGS.branch;
    if (!this.settings.oidcAccessToken && loaded.authToken) {
      this.settings.oidcAccessToken = loaded.authToken;
    }
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.settings);
    if (this.gitService) this.gitService.updateSettings(this.settings);
  }

  openLoginModal(): void {
    new AuthLoginModal(this.app, this.gitService, async () => {
      await this.saveSettings();
    }).open();
  }

  isSyncRunning(): boolean {
    return this.gitService?.isSyncRunning() ?? false;
  }

  checkConnection(): void {
    this.runCommand(async () => {
      const info = await this.refreshConnectionStatus();
      new Notice(`ObsidiSync server ${info.version} is reachable`);
    });
  }

  async refreshConnectionStatus(): Promise<ServerInfoResponse> {
    const info = await this.gitService.checkServerCompatibility();
    if (this.settings.oidcAccessToken) {
      await this.gitService.loadAuthenticatedUser();
    }
    return info;
  }

  resetSyncTimer(): void {
    if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }

    if (!this.settings.syncIntervalMinutes || this.settings.syncIntervalMinutes < 1) return;

    this.timer = window.setInterval(() => {
      this.runCommand(() => this.syncNow());
    }, this.settings.syncIntervalMinutes * 60 * 1000);
    this.registerInterval(this.timer);
  }

  private async resetLocalSyncState(): Promise<void> {
    this.settings.serverHead = null;
    this.settings.localManifest = [];
    await this.saveSettings();
    new Notice("Local sync state reset");
  }

  private setupMobileSyncIndicator(): void {
    if (!Platform.isMobile) return;

    const styleEl = document.createElement("style");
    styleEl.textContent = `
      .obsidisync-mobile-sync-indicator {
        align-items: center;
        color: var(--text-muted);
        cursor: pointer;
        display: none;
        flex-direction: column;
        flex: 0 0 auto;
        height: var(--clickable-icon-size, 32px);
        margin-inline: 2px;
        max-width: 92px;
        min-width: var(--clickable-icon-size, 32px);
        overflow: hidden;
        padding-inline: 4px;
        font-size: 11px;
        font-weight: 600;
        justify-content: flex-end;
        line-height: 1;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .obsidisync-mobile-sync-indicator-row {
        display: block;
        max-width: 100%;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      .obsidisync-mobile-sync-indicator-state {
        color: var(--text-faint);
        font-size: 10px;
        text-transform: lowercase;
      }
    `;
    document.head.appendChild(styleEl);
    this.register(() => styleEl.remove());

    const update = () => this.updateMobileSyncIndicator();
    this.register(this.gitService.onSyncStateChange(update));
    this.registerEvent(this.app.workspace.on("layout-change", () => update()));
    this.registerEvent(this.app.workspace.on("active-leaf-change", () => update()));
    this.registerEvent(this.app.workspace.on("file-open", () => update()));
    this.registerEvent(this.app.vault.on("create", (file) => this.updateMobileSyncIndicatorForPath(file.path)));
    this.registerEvent(this.app.vault.on("modify", (file) => this.updateMobileSyncIndicatorForPath(file.path)));
    this.registerEvent(this.app.vault.on("delete", (file) => this.updateMobileSyncIndicatorForPath(file.path)));
    this.registerEvent(this.app.vault.on("rename", (file, oldPath) => {
      this.updateMobileSyncIndicatorForPath(file.path);
      this.updateMobileSyncIndicatorForPath(oldPath);
    }));
    this.app.workspace.onLayoutReady(() => update());
    window.setTimeout(() => update(), 0);
  }

  private setupSyncOnClose(): void {
    this.app.workspace.onLayoutReady(() => {
      this.lastActiveFilePath = this.currentMarkdownFilePath();
    });

    this.registerEvent(
      this.app.workspace.on("file-open", (file) => {
        this.handleActiveFilePathChange(file?.path ?? null);
      })
    );
    this.registerEvent(
      this.app.workspace.on("active-leaf-change", () => {
        window.setTimeout(() => this.handleActiveFilePathChange(this.currentMarkdownFilePath()), 0);
      })
    );

    const syncCurrentFile = () => this.syncCurrentFileBeforeAppClose();
    const syncWhenHidden = () => {
      if (document.visibilityState === "hidden") syncCurrentFile();
    };

    window.addEventListener("pagehide", syncCurrentFile);
    document.addEventListener("visibilitychange", syncWhenHidden);
    this.register(() => {
      window.removeEventListener("pagehide", syncCurrentFile);
      document.removeEventListener("visibilitychange", syncWhenHidden);
    });
  }

  private currentMarkdownFilePath(): string | null {
    return this.app.workspace.getActiveViewOfType(MarkdownView)?.file?.path ?? null;
  }

  private handleActiveFilePathChange(nextPath: string | null): void {
    const previousPath = this.lastActiveFilePath;
    this.lastActiveFilePath = nextPath;
    if (previousPath && previousPath !== nextPath) {
      this.scheduleSyncClosedFile(previousPath);
    }
  }

  private scheduleSyncClosedFile(path: string): void {
    if (this.pendingCloseSyncPaths.has(path)) return;
    this.pendingCloseSyncPaths.add(path);
    window.setTimeout(() => {
      this.pendingCloseSyncPaths.delete(path);
      void this.syncPathIfChanged(path);
    }, 300);
  }

  private syncCurrentFileBeforeAppClose(): void {
    if (this.appCloseSyncStarted) return;
    const path = this.lastActiveFilePath ?? this.currentMarkdownFilePath();
    if (!path) return;

    this.appCloseSyncStarted = true;
    void this.syncPathIfChanged(path).finally(() => {
      this.appCloseSyncStarted = false;
    });
  }

  private async syncPathIfChanged(path: string): Promise<void> {
    try {
      if (!(await this.fileHasLocalChanges(path))) return;
      await this.runCommand(() => this.syncNow());
    } catch (error) {
      console.error("ObsidiSync close sync failed", error);
    }
  }

  private updateMobileSyncIndicator(): void {
    if (!Platform.isMobile) return;

    const indicator = this.ensureMobileSyncIndicator();
    if (!indicator) return;

    void this.refreshMobileFileSyncIndicator(indicator);
  }

  private updateMobileSyncIndicatorForPath(path: string): void {
    if (path !== this.currentMarkdownFilePath()) return;
    window.setTimeout(() => this.updateMobileSyncIndicator(), 50);
  }

  private async refreshMobileFileSyncIndicator(indicator: HTMLElement): Promise<void> {
    const requestId = ++this.mobileSyncIndicatorRequestId;
    const path = this.app.workspace.getActiveViewOfType(MarkdownView)?.file?.path ?? null;
    if (!path) {
      this.hideMobileSyncIndicator(indicator);
      return;
    }

    try {
      const savedState = await this.mobileFileSavedState(path);
      if (requestId !== this.mobileSyncIndicatorRequestId) return;
      const label = savedState === "changes" ? "Changed" : formatMobileSyncDate(this.settings.lastSyncedAt);
      this.clearMobileSyncIndicator(indicator);
      indicator.createSpan({ cls: "obsidisync-mobile-sync-indicator-row", text: label });
      indicator.createSpan({ cls: "obsidisync-mobile-sync-indicator-row obsidisync-mobile-sync-indicator-state", text: savedState });
      indicator.style.display = "inline-flex";
      indicator.setAttribute("aria-hidden", "false");
      indicator.setAttribute("aria-label", `Last synced ${label}; ${savedState}`);
      indicator.title = `Last synced: ${formatFullDate(this.settings.lastSyncedAt)}; ${savedState}`;
    } catch {
      if (requestId === this.mobileSyncIndicatorRequestId) {
        this.hideMobileSyncIndicator(indicator);
      }
    }
  }

  private async mobileFileSavedState(path: string): Promise<"saved" | "changes"> {
    return (await this.fileHasLocalChanges(path)) ? "changes" : "saved";
  }

  private async fileHasLocalChanges(path: string): Promise<boolean> {
    const syncedEntry = this.settings.localManifest.find((entry) => entry.path === path);
    const exists = await this.app.vault.adapter.exists(path, true);
    if (!exists) return Boolean(syncedEntry);
    if (!syncedEntry) return true;

    const buffer = await this.app.vault.adapter.readBinary(path);
    const currentSha = await sha256Hex(buffer);
    return currentSha !== syncedEntry.sha256;
  }

  private hideMobileSyncIndicator(indicator: HTMLElement): void {
    this.clearMobileSyncIndicator(indicator);
    indicator.style.display = "none";
    indicator.setAttribute("aria-hidden", "true");
    indicator.title = "";
  }

  private clearMobileSyncIndicator(indicator: HTMLElement): void {
    while (indicator.firstChild) {
      indicator.removeChild(indicator.firstChild);
    }
  }

  private ensureMobileSyncIndicator(): HTMLElement | null {
    const container = this.mobileTitleActionsContainer();
    if (!container) return null;

    if (this.mobileSyncIndicatorEl?.parentElement === container) {
      return this.mobileSyncIndicatorEl;
    }

    this.mobileSyncIndicatorEl?.remove();
    const indicator = document.createElement("div");
    indicator.className = "obsidisync-mobile-sync-indicator";
    indicator.tabIndex = 0;
    indicator.setAttribute("role", "button");
    indicator.setAttribute("aria-label", "ObsidiSync sync status");
    indicator.onclick = (event) => {
      void this.openSyncMenu(event);
    };
    indicator.onkeydown = (event) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      void this.openSyncMenu(event);
    };

    const menuButton = container.querySelector(
      '.view-action[aria-label*="More"], .view-action.mod-more-options, .mobile-navbar-action[aria-label*="More"], [aria-label*="More options"]'
    );
    if (menuButton?.parentElement === container) {
      container.insertBefore(indicator, menuButton);
    } else {
      container.appendChild(indicator);
    }

    this.mobileSyncIndicatorEl = indicator;
    this.register(() => indicator.remove());
    return indicator;
  }

  private async openSyncMenu(event: MouseEvent | KeyboardEvent): Promise<void> {
    event.preventDefault();
    event.stopPropagation();

    const menu = new Menu();
    try {
      const summary = await this.gitService.localChangeSummary();
      menu.addItem((item) =>
        item
          .setTitle(`${summary.changed} changed file${summary.changed === 1 ? "" : "s"}`)
          .setIcon(summary.changed > 0 ? "circle-alert" : "check")
          .setDisabled(true)
      );
      menu.addSeparator();
    } catch {
      // The action menu should still open even if the local summary cannot be computed.
    }

    menu.addItem((item) =>
      item
        .setTitle("Sync now")
        .setIcon("refresh-cw")
        .setDisabled(this.isSyncRunning())
        .onClick(() => this.runCommand(() => this.syncNow()))
    );
    menu.addItem((item) =>
      item
        .setTitle("Open file history")
        .setIcon("history")
        .onClick(() => {
          void this.openFileHistoryView();
        })
    );
    menu.addItem((item) =>
      item
        .setTitle("Resolve conflicts")
        .setIcon("git-pull-request")
        .onClick(() => this.openConflictResolver())
    );
    menu.addSeparator();
    menu.addItem((item) =>
      item
        .setTitle("Log in to ObsidiSync")
        .setIcon("log-in")
        .onClick(() => this.openLoginModal())
    );

    if (event instanceof MouseEvent) {
      menu.showAtMouseEvent(event);
      return;
    }

    const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
    menu.showAtPosition({ x: rect.left, y: rect.bottom, width: rect.width });
  }

  private mobileTitleActionsContainer(): HTMLElement | null {
    return (
      document.querySelector<HTMLElement>(".mobile-navbar-actions") ??
      document.querySelector<HTMLElement>(".workspace-leaf.mod-active .view-header .view-actions") ??
      document.querySelector<HTMLElement>(".view-header .view-actions")
    );
  }

  private async syncNow(): Promise<void> {
    if (this.needsInitialSyncSetup()) {
      this.openInitialSyncModal();
      return;
    }
    const conflicts = await this.gitService.sync();
    if (conflicts.length > 0) {
      this.openConflictResolver(conflicts);
    }
  }

  private needsInitialSyncSetup(): boolean {
    return (
      !this.settings.initialSyncDone &&
      this.settings.serverHead === null &&
      Boolean(this.settings.serverUrl) &&
      Boolean(this.settings.oidcAccessToken) &&
      Boolean(this.settings.userSlug) &&
      Boolean(this.settings.vaultSlug)
    );
  }

  private openInitialSyncModal(): void {
    if (this.initialSyncModalOpen) return;
    this.initialSyncModalOpen = true;
    const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
    const defaultBackupFolder = `.obsidian-git-sync/backups/${timestamp}`;
    new InitialSyncModal(
      this.app,
      defaultBackupFolder,
      {
        forcePush: () => this.runCommand(() => this.gitService.forcePushLocal()),
        overwriteLocal: (backupFolder) =>
          this.runCommand(() => this.gitService.overwriteLocalFromServer(backupFolder))
      },
      () => {
        this.initialSyncModalOpen = false;
        this.updateMobileSyncIndicator();
      }
    ).open();
  }

  private openConflictResolver(conflicts: SyncConflict[] = []): void {
    if (this.conflictResolverOpen) {
      new Notice("Conflict resolver is already open");
      return;
    }
    this.conflictResolverOpen = true;
    new ConflictResolverModal(this.app, this.gitService, conflicts, () => {
      this.conflictResolverOpen = false;
      void this.refreshFileHistoryViews();
      this.updateMobileSyncIndicator();
    }).open();
  }

  private async refreshFileHistoryViews(): Promise<void> {
    for (const leaf of this.app.workspace.getLeavesOfType(FILE_HISTORY_VIEW_TYPE)) {
      const view = leaf.view;
      if (view instanceof FileHistoryView) {
        await view.refresh();
      }
    }
  }

  private snapshotReference(path: string): HistorySnapshotReference | null {
    return this.settings.historySnapshots.find((snapshot) => snapshot.snapshotPath === path) ?? null;
  }

  private async saveSnapshotReference(reference: HistorySnapshotReference): Promise<void> {
    const withoutExisting = this.settings.historySnapshots.filter((snapshot) => snapshot.snapshotPath !== reference.snapshotPath);
    this.settings.historySnapshots = [...withoutExisting, reference].slice(-200);
    await this.saveSettings();
  }

  private versionMetadata(sourcePath: string, hash: string): { sourcePath: string; hash: string; name?: string; squashedIntoHash?: string } | null {
    return this.settings.historyVersions.find((version) => version.sourcePath === sourcePath && version.hash === hash) ?? null;
  }

  private async saveVersionName(sourcePath: string, hash: string, name: string | null): Promise<void> {
    const current = this.versionMetadata(sourcePath, hash);
    await this.saveVersionMetadata({
      sourcePath,
      hash,
      name: name ?? undefined,
      squashedIntoHash: current?.squashedIntoHash
    });
  }

  private async squashVersion(sourcePath: string, hash: string, intoHash: string): Promise<void> {
    const current = this.versionMetadata(sourcePath, hash);
    await this.saveVersionMetadata({
      sourcePath,
      hash,
      name: current?.name,
      squashedIntoHash: intoHash
    });
  }

  private async saveVersionMetadata(entry: { sourcePath: string; hash: string; name?: string; squashedIntoHash?: string }): Promise<void> {
    const withoutExisting = this.settings.historyVersions.filter(
      (version) => version.sourcePath !== entry.sourcePath || version.hash !== entry.hash
    );
    if (!entry.name && !entry.squashedIntoHash) {
      this.settings.historyVersions = withoutExisting;
    } else {
      this.settings.historyVersions = [...withoutExisting, entry].slice(-500);
    }
    await this.saveSettings();
  }

  private async openFileHistoryView(): Promise<void> {
    const activeFilePath = this.app.workspace.getActiveViewOfType(MarkdownView)?.file?.path ?? null;
    let leaf = this.app.workspace.getLeavesOfType(FILE_HISTORY_VIEW_TYPE)[0];
    if (!leaf) {
      leaf = this.app.workspace.getRightLeaf(false) ?? this.app.workspace.getLeaf(true);
      await leaf.setViewState({ type: FILE_HISTORY_VIEW_TYPE, active: true });
    }
    await this.app.workspace.revealLeaf(leaf);
    const view = leaf.view;
    if (view instanceof FileHistoryView) {
      if (activeFilePath) {
        await view.showFile(activeFilePath);
      } else {
        await view.refresh();
      }
    }
  }

  private async runCommand(operation: () => Promise<void>): Promise<void> {
    try {
      await operation();
    } catch (error) {
      console.error("ObsidiSync error", error);
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("CONFLICT") || message.toLowerCase().includes("conflict")) {
        new Notice("Git conflict detected. Resolve conflict markers, then run sync again.", 15000);
      } else {
        new Notice(`ObsidiSync error: ${message}`, 10000);
      }
    }
  }
}

function formatMobileSyncDate(date: string | null): string {
  if (!date) return "Never";
  const timestamp = Date.parse(date);
  if (Number.isNaN(timestamp)) return "Synced";

  const elapsedMs = Date.now() - timestamp;
  const elapsedSeconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (elapsedSeconds < 60) return "Synced now";

  const elapsedMinutes = Math.floor(elapsedSeconds / 60);
  if (elapsedMinutes < 60) return `${elapsedMinutes}m ago`;

  const elapsedHours = Math.floor(elapsedMinutes / 60);
  if (elapsedHours < 24) return `${elapsedHours}h ago`;

  const elapsedDays = Math.floor(elapsedHours / 24);
  if (elapsedDays < 7) return `${elapsedDays}d ago`;

  return new Date(timestamp).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function formatFullDate(date: string | null): string {
  if (!date) return "Never";
  const timestamp = Date.parse(date);
  if (Number.isNaN(timestamp)) return date;
  return new Date(timestamp).toLocaleString();
}
