import { App, PluginSettingTab, Setting } from "obsidian";
import IosGitSyncPlugin from "./main";
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
  localManifest: ManifestEntry[];
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
  localManifest: []
};

export class IosGitSyncSettingTab extends PluginSettingTab {
  plugin: IosGitSyncPlugin;

  constructor(app: App, plugin: IosGitSyncPlugin) {
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
      .setName("OIDC access token")
      .setDesc("Bearer access token issued by your OIDC provider. You can paste one or use the device login command.")
      .addText((text) => {
        text.inputEl.type = "password";
        text.setValue(this.plugin.settings.oidcAccessToken).onChange(async (value) => {
          this.plugin.settings.oidcAccessToken = value;
          await this.plugin.saveSettings();
        });
      });

    new Setting(containerEl)
      .setName("OIDC issuer")
      .setDesc("Issuer base URL for device login, for example https://auth.example.com/realms/obsidian.")
      .addText((text) =>
        text.setPlaceholder("https://issuer.example.com").setValue(this.plugin.settings.oidcIssuer).onChange(async (value) => {
          this.plugin.settings.oidcIssuer = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("OIDC client ID")
      .setDesc("Public client configured for device authorization.")
      .addText((text) =>
        text.setValue(this.plugin.settings.oidcClientId).onChange(async (value) => {
          this.plugin.settings.oidcClientId = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("OIDC scope")
      .addText((text) =>
        text.setValue(this.plugin.settings.oidcScope).onChange(async (value) => {
          this.plugin.settings.oidcScope = value.trim() || DEFAULT_SETTINGS.oidcScope;
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("OIDC audience")
      .setDesc("Optional audience/resource parameter if your provider requires it.")
      .addText((text) =>
        text.setValue(this.plugin.settings.oidcAudience).onChange(async (value) => {
          this.plugin.settings.oidcAudience = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("User namespace")
      .setDesc("Must match the normalized OIDC user claim on the server.")
      .addText((text) =>
        text.setPlaceholder("alice").setValue(this.plugin.settings.userSlug).onChange(async (value) => {
          this.plugin.settings.userSlug = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Vault namespace")
      .setDesc("Server path segment for this vault, for example personal or work.")
      .addText((text) =>
        text.setPlaceholder("personal").setValue(this.plugin.settings.vaultSlug).onChange(async (value) => {
          this.plugin.settings.vaultSlug = value.trim();
          await this.plugin.saveSettings();
        })
      );

    new Setting(containerEl)
      .setName("Git remote URL")
      .setDesc("Remote used by the server. SSH is supported only on the server if its Git environment is configured for it.")
      .addText((text) =>
        text
          .setPlaceholder("git@github.com:user/private-vault.git")
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
      .setName("Device name")
      .setDesc("Used in Git commit messages created by the server.")
      .addText((text) =>
        text.setPlaceholder("Keller iPhone").setValue(this.plugin.settings.deviceName).onChange(async (value) => {
          this.plugin.settings.deviceName = value.trim();
          await this.plugin.saveSettings();
        })
      );

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
