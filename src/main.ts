import { MarkdownView, Notice, Plugin } from "obsidian";
import { ComputerNameModal } from "./computerNameModal";
import { FILE_HISTORY_VIEW_TYPE, FileHistoryView, HistorySnapshotReference } from "./fileHistoryView";
import { GitService } from "./gitService";
import { OidcDeviceLoginModal } from "./oidcModal";
import { createClientId } from "./runtime";
import { DEFAULT_SETTINGS, IosGitSyncSettings, IosGitSyncSettingTab } from "./settings";

export default class ObsyncPlugin extends Plugin {
  settings: IosGitSyncSettings;
  private gitService: GitService;
  private timer: number | null = null;

  async onload(): Promise<void> {
    await this.loadSettings();
    if (!this.settings.clientId) {
      this.settings.clientId = createClientId();
      await this.saveSettings();
    }

    this.gitService = new GitService(this.app.vault, this.settings, () => this.saveSettings());

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
    this.addRibbonIcon("refresh-cw", "Sync vault", () => this.runCommand(() => this.gitService.sync()));
    this.addRibbonIcon("history", "Open file history", () => this.openFileHistoryView());

    this.addCommand({
      id: "sync-now",
      name: "Sync now",
      callback: () => this.runCommand(() => this.gitService.sync())
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
      name: "Resolve current conflict file",
      callback: () => this.resolveCurrentFile()
    });

    this.resetSyncTimer();

    if (this.settings.syncOnStartup) {
      this.app.workspace.onLayoutReady(() => {
        window.setTimeout(() => {
          this.runCommand(() => this.gitService.sync());
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
    if (!this.settings.oidcAccessToken && loaded.authToken) {
      this.settings.oidcAccessToken = loaded.authToken;
    }
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.settings);
    if (this.gitService) this.gitService.updateSettings(this.settings);
  }

  resetSyncTimer(): void {
    if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }

    if (!this.settings.syncIntervalMinutes || this.settings.syncIntervalMinutes < 1) return;

    this.timer = window.setInterval(() => {
      this.runCommand(() => this.gitService.sync());
    }, this.settings.syncIntervalMinutes * 60 * 1000);
    this.registerInterval(this.timer);
  }

  private async resetLocalSyncState(): Promise<void> {
    this.settings.serverHead = null;
    this.settings.localManifest = [];
    await this.saveSettings();
    new Notice("Local sync state reset");
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

  private async resolveCurrentFile(): Promise<void> {
    const file = this.app.workspace.getActiveViewOfType(MarkdownView)?.file;
    if (!file) {
      new Notice("Open a conflict file before resolving");
      return;
    }
    await this.gitService.resolveFile(file.path);
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
