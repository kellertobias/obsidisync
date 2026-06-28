import { App, PluginSettingTab, Setting } from "obsidian";
import ObsyncPlugin from "./main";
import { ManifestEntry } from "./protocol";

export interface IosGitSyncSettings {
  serverUrl: string;
  oidcIssuer: string;
  oidcClientId: string;
  oidcScope: string;
  oidcAudience: string;
  oidcAccessToken: string;
  userSlug: string;
  vaultSlug: string;
  remoteUrl: string;
  branch: string;
  authorName: string;
  authorEmail: string;
  deviceName: string;
  syncOnStartup: boolean;
  syncIntervalMinutes: number;
  clientId: string;
  serverHead: string | null;
  lastSyncedAt: string | null;
  localManifest: ManifestEntry[];
  historySnapshots: HistorySnapshotEntry[];
  historyVersions: HistoryVersionEntry[];
}

export interface HistorySnapshotEntry {
  snapshotPath: string;
  sourcePath: string;
  hash: string;
}

export interface HistoryVersionEntry {
  sourcePath: string;
  hash: string;
  name?: string;
  squashedIntoHash?: string;
}

export const DEFAULT_SETTINGS: IosGitSyncSettings = {
  serverUrl: "",
  oidcIssuer: "",
  oidcClientId: "",
  oidcScope: "openid profile email",
  oidcAudience: "",
  oidcAccessToken: "",
  userSlug: "",
  vaultSlug: "",
  remoteUrl: "",
  branch: "main",
  authorName: "Obsidian Mobile",
  authorEmail: "obsidian-mobile@example.invalid",
  deviceName: "",
  syncOnStartup: true,
  syncIntervalMinutes: 10,
  clientId: "",
  serverHead: null,
  lastSyncedAt: null,
  localManifest: [],
  historySnapshots: [],
  historyVersions: []
};

export class IosGitSyncSettingTab extends PluginSettingTab {
  plugin: ObsyncPlugin;

  constructor(app: App, plugin: ObsyncPlugin) {
    super(app, plugin);
    this.plugin = plugin;
  }

  display(): void {
    const { containerEl } = this;
    containerEl.empty();

    new Setting(containerEl)
      .setName("Sync server URL")
      .setDesc("Your self-hosted server, for example https://sync.example.com.")
      .addText((text) =>
        text
          .setPlaceholder("https://sync.example.com")
          .setValue(this.plugin.settings.serverUrl)
          .onChange(async (value) => {
            this.plugin.settings.serverUrl = value.trim();
            await this.plugin.saveSettings();
          })
      );

    new Setting(containerEl)
      .setName("Login")
      .setDesc(this.plugin.settings.oidcAccessToken ? `Logged in as ${this.plugin.settings.userSlug || "configured user"}.` : "Fetch login settings from the sync server and store the access token automatically.")
      .addButton((button) =>
        button.setCta().setButtonText(this.plugin.settings.oidcAccessToken ? "Log in again" : "Log in").onClick(() => {
          this.plugin.openLoginModal();
        })
      );

    new Setting(containerEl)
      .setName("Vault name")
      .setDesc("Server vault name. Defaults to this Obsidian vault name.")
      .addText((text) =>
        text.setPlaceholder("personal").setValue(this.plugin.settings.vaultSlug).onChange(async (value) => {
          this.plugin.settings.vaultSlug = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Git remote URL")
      .setDesc("Optional. Leave blank to keep a server-local Git repository in the server data directory.")
      .addText((text) =>
        text
          .setPlaceholder("optional")
          .setValue(this.plugin.settings.remoteUrl)
          .onChange(async (value) => {
            this.plugin.settings.remoteUrl = value.trim();
            await this.plugin.saveSettings();
          })
      );

    new Setting(containerEl)
      .setName("Branch")
      .addText((text) =>
        text.setValue(this.plugin.settings.branch).onChange(async (value) => {
          this.plugin.settings.branch = value.trim() || "main";
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Author name")
      .addText((text) =>
        text.setValue(this.plugin.settings.authorName).onChange(async (value) => {
          this.plugin.settings.authorName = value.trim() || DEFAULT_SETTINGS.authorName;
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Author email")
      .addText((text) =>
        text.setValue(this.plugin.settings.authorEmail).onChange(async (value) => {
          this.plugin.settings.authorEmail = value.trim() || DEFAULT_SETTINGS.authorEmail;
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Computer name")
      .setDesc("Shown as the source computer in sync history. Set a different name on each computer.")
      .addText((text) =>
        text.setPlaceholder("Keller MacBook").setValue(this.plugin.settings.deviceName).onChange(async (value) => {
          this.plugin.settings.deviceName = value.trim();
          await this.plugin.saveSettings();
        })
      );

    containerEl.createEl("h3", { text: "Sync" });

    new Setting(containerEl)
      .setName("Sync on startup")
      .addToggle((toggle) =>
        toggle.setValue(this.plugin.settings.syncOnStartup).onChange(async (value) => {
          this.plugin.settings.syncOnStartup = value;
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Sync interval")
      .setDesc("Minutes. Set to 0 to disable scheduled sync.")
      .addText((text) =>
        text.setValue(String(this.plugin.settings.syncIntervalMinutes)).onChange(async (value) => {
          const parsed = Number.parseInt(value, 10);
          this.plugin.settings.syncIntervalMinutes = Number.isFinite(parsed) && parsed > 0 ? parsed : 0;
          await this.plugin.saveSettings();
          this.plugin.resetSyncTimer();
        })
      );

    containerEl.createEl("h3", { text: "Advanced" });

    new Setting(containerEl)
      .setName("Access token")
      .setDesc("Manual fallback for static-token development servers or recovery.")
      .addText((text) => {
        text.inputEl.type = "password";
        text.setValue(this.plugin.settings.oidcAccessToken).onChange(async (value) => {
          this.plugin.settings.oidcAccessToken = value;
          await this.plugin.saveSettings();
        });
      });

    new Setting(containerEl)
      .setName("User namespace")
      .setDesc("Normally set by login. Must match the authenticated server user.")
      .addText((text) =>
        text.setPlaceholder("alice").setValue(this.plugin.settings.userSlug).onChange(async (value) => {
          this.plugin.settings.userSlug = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Reset registration")
      .setDesc("Forget the last synced commit and manifest. Local files are not changed.")
      .addButton((button) =>
        button.setButtonText("Reset").onClick(async () => {
          this.plugin.settings.serverHead = null;
          this.plugin.settings.localManifest = [];
          await this.plugin.saveSettings();
        })
      );
  }
}
