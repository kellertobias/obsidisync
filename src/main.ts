import { MarkdownView, Notice, Plugin } from "obsidian";
import { GitService } from "./gitService";
import { OidcDeviceLoginModal } from "./oidcModal";
import { DEFAULT_SETTINGS, IosGitSyncSettings, IosGitSyncSettingTab } from "./settings";
import { FileVersionsModal } from "./versionModal";

export default class IosGitSyncPlugin extends Plugin {
  settings: IosGitSyncSettings;
  private gitService: GitService;
  private timer: number | null = null;

  async onload(): Promise<void> {
    await this.loadSettings();
    if (!this.settings.clientId) {
      this.settings.clientId = crypto.randomUUID();
      await this.saveSettings();
    }

    this.gitService = new GitService(this.app.vault, this.settings, () => this.saveSettings());

    this.addSettingTab(new IosGitSyncSettingTab(this.app, this));
    this.addRibbonIcon("refresh-cw", "Sync vault", () => this.runCommand(() => this.gitService.sync()));

    this.addCommand({
      id: "sync-now",
      name: "Sync now",
      callback: () => this.runCommand(() => this.gitService.sync())
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
      name: "Show current file versions",
      callback: () => this.showCurrentFileVersions()
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
    const loaded = (await this.loadData()) as Partial<IosGitSyncSettings> & { authToken?: string; vaultId?: string };
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

  private async showCurrentFileVersions(): Promise<void> {
    const file = this.app.workspace.getActiveViewOfType(MarkdownView)?.file;
    if (!file) {
      new Notice("Open a file before showing versions");
      return;
    }
    new FileVersionsModal(this.app, this.gitService, file.path).open();
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
