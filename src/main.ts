import { MarkdownView, Notice, Platform, Plugin, setIcon } from "obsidian";
import { AuthLoginModal } from "./authLoginModal";
import { ComputerNameModal } from "./computerNameModal";
import { ConflictResolverModal } from "./conflictResolverModal";
import { FILE_HISTORY_VIEW_TYPE, FileHistoryView, HistorySnapshotReference } from "./fileHistoryView";
import { GitService } from "./gitService";
import { OidcDeviceLoginModal } from "./oidcModal";
import { SyncConflict } from "./protocol";
import { createClientId, generateComputerName, slugFromName } from "./runtime";
import { DEFAULT_SETTINGS, IosGitSyncSettings, IosGitSyncSettingTab } from "./settings";

export default class ObsyncPlugin extends Plugin {
  settings: IosGitSyncSettings;
  private gitService: GitService;
  private timer: number | null = null;
  private conflictResolverOpen = false;
  private mobileSyncIndicatorEl: HTMLElement | null = null;
  private mobileSyncIndicatorRequestId = 0;

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
          lastSyncedAt: () => this.settings.lastSyncedAt
        })
    );

    this.addSettingTab(new IosGitSyncSettingTab(this.app, this));
    this.addRibbonIcon("refresh-cw", "Sync vault", () => this.runCommand(() => this.syncNow()));
    this.addRibbonIcon("history", "Open file history", () => this.openFileHistoryView());
    this.setupMobileSyncIndicator();

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
      name: "Log in to Obsync",
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
      @keyframes obsync-mobile-sync-spin {
        from { transform: rotate(0deg); }
        to { transform: rotate(360deg); }
      }
      .obsync-mobile-sync-indicator {
        align-items: center;
        color: var(--text-muted);
        display: none;
        flex: 0 0 auto;
        height: var(--clickable-icon-size, 32px);
        justify-content: center;
        margin-inline: 2px;
        max-width: 92px;
        min-width: var(--clickable-icon-size, 32px);
        overflow: hidden;
        padding-inline: 4px;
        white-space: nowrap;
      }
      .obsync-mobile-sync-indicator svg {
        animation: obsync-mobile-sync-spin 1s linear infinite;
        height: 18px;
        width: 18px;
      }
      .obsync-mobile-sync-indicator:not(.is-syncing) {
        font-size: 11px;
        font-weight: 600;
        justify-content: flex-end;
        line-height: 1;
        text-overflow: ellipsis;
      }
    `;
    document.head.appendChild(styleEl);
    this.register(() => styleEl.remove());

    const update = (running = this.gitService.isSyncRunning()) => this.updateMobileSyncIndicator(running);
    this.register(this.gitService.onSyncStateChange(update));
    this.registerEvent(this.app.workspace.on("layout-change", () => update()));
    this.registerEvent(this.app.workspace.on("active-leaf-change", () => update()));
    this.registerEvent(this.app.workspace.on("file-open", () => update()));
    this.app.workspace.onLayoutReady(() => update());
    window.setTimeout(() => update(), 0);
  }

  private updateMobileSyncIndicator(running: boolean): void {
    if (!Platform.isMobile) return;

    const indicator = this.ensureMobileSyncIndicator();
    if (!indicator) return;

    if (running) {
      this.mobileSyncIndicatorRequestId += 1;
      this.clearMobileSyncIndicator(indicator);
      indicator.addClass("is-syncing");
      indicator.style.display = "inline-flex";
      indicator.setAttribute("aria-hidden", "false");
      indicator.title = "Obsync is syncing";
      setIcon(indicator, "refresh-cw");
      return;
    }

    void this.refreshMobileFileSyncIndicator(indicator);
  }

  private async refreshMobileFileSyncIndicator(indicator: HTMLElement): Promise<void> {
    const requestId = ++this.mobileSyncIndicatorRequestId;
    const path = this.app.workspace.getActiveViewOfType(MarkdownView)?.file?.path ?? null;
    if (!path) {
      this.hideMobileSyncIndicator(indicator);
      return;
    }

    try {
      const history = await this.gitService.history(path);
      if (requestId !== this.mobileSyncIndicatorRequestId || this.gitService.isSyncRunning()) return;
      const latest = history[0];
      if (!latest) {
        this.hideMobileSyncIndicator(indicator);
        return;
      }
      const label = formatMobileSyncDate(latest.date);
      this.clearMobileSyncIndicator(indicator);
      indicator.removeClass("is-syncing");
      indicator.createSpan({ text: label });
      indicator.style.display = "inline-flex";
      indicator.setAttribute("aria-hidden", "false");
      indicator.title = `Last synced: ${formatFullDate(latest.date)}`;
    } catch {
      if (requestId === this.mobileSyncIndicatorRequestId) {
        this.hideMobileSyncIndicator(indicator);
      }
    }
  }

  private hideMobileSyncIndicator(indicator: HTMLElement): void {
    this.clearMobileSyncIndicator(indicator);
    indicator.removeClass("is-syncing");
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
    indicator.className = "obsync-mobile-sync-indicator";
    indicator.setAttribute("aria-label", "Obsync is syncing");

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

  private mobileTitleActionsContainer(): HTMLElement | null {
    return (
      document.querySelector<HTMLElement>(".mobile-navbar-actions") ??
      document.querySelector<HTMLElement>(".workspace-leaf.mod-active .view-header .view-actions") ??
      document.querySelector<HTMLElement>(".view-header .view-actions")
    );
  }

  private async syncNow(): Promise<void> {
    const conflicts = await this.gitService.sync();
    if (conflicts.length > 0) {
      this.openConflictResolver(conflicts);
    }
  }

  private openConflictResolver(conflicts: SyncConflict[] = []): void {
    if (this.conflictResolverOpen) {
      new Notice("Conflict resolver is already open");
      return;
    }
    this.conflictResolverOpen = true;
    new ConflictResolverModal(this.app, this.gitService, conflicts, () => {
      this.conflictResolverOpen = false;
    }).open();
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
      console.error("iOS Git Sync error", error);
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("CONFLICT") || message.toLowerCase().includes("conflict")) {
        new Notice("Git conflict detected. Resolve conflict markers, then run sync again.", 15000);
      }
    }
  }
}

function formatMobileSyncDate(date: string): string {
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

function formatFullDate(date: string): string {
  const timestamp = Date.parse(date);
  if (Number.isNaN(timestamp)) return date;
  return new Date(timestamp).toLocaleString();
}
