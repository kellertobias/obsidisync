import { App, Modal, Notice, Setting } from "obsidian";
import { GitService, OidcDeviceAuthorization, ServerAuthConfig } from "./gitService";

export class AuthLoginModal extends Modal {
  private authorization: OidcDeviceAuthorization | null = null;
  private statusEl: HTMLElement;

  constructor(
    app: App,
    private readonly gitService: GitService,
    private readonly onComplete: () => Promise<void> | void
  ) {
    super(app);
  }

  async onOpen(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Log in to ObsidiSync" });
    this.statusEl = contentEl.createEl("p", { text: "Loading login configuration..." });

    try {
      const config = await this.gitService.authConfig();
      if (config.type === "password") {
        this.renderPassword(config);
      } else if (config.type === "oidc") {
        await this.startOidc();
      } else {
        this.renderManualToken();
      }
    } catch (error) {
      this.statusEl.setText(`Login failed: ${errorMessage(error)}`);
    }
  }

  private renderPassword(config: Extract<ServerAuthConfig, { type: "password" }>): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: config.passwordConfigured ? "Log in to ObsidiSync" : "Set ObsidiSync password" });

    let username = "";
    let password = "";
    let passwordConfirm = "";
    let setupToken = "";

    new Setting(contentEl).setName("Username").addText((text) =>
      text.setPlaceholder("alice").onChange((value) => {
        username = value.trim();
      })
    );

    new Setting(contentEl).setName("Password").addText((text) => {
      text.inputEl.type = "password";
      text.onChange((value) => {
        password = value;
      });
    });

    if (!config.passwordConfigured) {
      new Setting(contentEl).setName("Confirm password").addText((text) => {
        text.inputEl.type = "password";
        text.onChange((value) => {
          passwordConfirm = value;
        });
      });
      if (config.setupTokenRequired) {
        new Setting(contentEl).setName("Setup token").addText((text) => {
          text.inputEl.type = "password";
          text.onChange((value) => {
            setupToken = value.trim();
          });
        });
      }
    }

    new Setting(contentEl).addButton((button) =>
      button
        .setCta()
        .setButtonText(config.passwordConfigured ? "Log in" : "Set password")
        .onClick(async () => {
          try {
            if (!username) throw new Error("Enter a username");
            if (!password) throw new Error("Enter a password");
            if (!config.passwordConfigured && passwordConfirm !== password) {
              throw new Error("Password confirmation does not match");
            }
            if (!config.passwordConfigured && config.setupTokenRequired && !setupToken) {
              throw new Error("Enter the setup token");
            }
            button.setDisabled(true);
            await this.gitService.loginPassword(username, password, !config.passwordConfigured, setupToken);
            await this.complete("Login complete");
          } catch (error) {
            button.setDisabled(false);
            this.setStatus(`Login failed: ${errorMessage(error)}`);
          }
        })
    );

    this.statusEl = contentEl.createEl("p", { text: "" });
  }

  private async startOidc(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Log in to ObsidiSync" });
    this.statusEl = contentEl.createEl("p", { text: "Starting device login..." });

    this.authorization = await this.gitService.beginOidcDeviceLogin();
    this.renderOidcAuthorization();
    await this.gitService.pollOidcDeviceLogin(this.authorization.device_code, this.authorization.interval ?? 5, this.authorization.expires_in);
    await this.complete("Login complete");
  }

  private renderOidcAuthorization(): void {
    if (!this.authorization) return;
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Log in to ObsidiSync" });
    contentEl.createEl("p", { text: "Open the verification URL and enter the code." });
    contentEl.createEl("p", { text: this.authorization.verification_uri_complete || this.authorization.verification_uri });
    contentEl.createEl("h3", { text: this.authorization.user_code });

    new Setting(contentEl)
      .addButton((button) =>
        button.setButtonText("Copy code").onClick(async () => {
          await navigator.clipboard.writeText(this.authorization?.user_code ?? "");
          new Notice("Login code copied");
        })
      )
      .addButton((button) =>
        button.setButtonText("Copy URL").onClick(async () => {
          await navigator.clipboard.writeText(this.authorization?.verification_uri_complete || this.authorization?.verification_uri || "");
          new Notice("Login URL copied");
        })
      )
      .addButton((button) =>
        button.setButtonText("Open URL").onClick(() => {
          openAuthorizationUrl(this.authorization);
        })
      );

    this.statusEl = contentEl.createEl("p", { text: "Waiting for authorization..." });
  }

  private renderManualToken(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Manual token required" });
    this.statusEl = contentEl.createEl("p", { text: "This server uses static token authentication. Paste the token in Advanced settings." });
  }

  private async complete(message: string): Promise<void> {
    this.setStatus(message);
    await this.onComplete();
    new Notice(message);
    this.close();
  }

  private setStatus(message: string): void {
    if (this.statusEl) this.statusEl.setText(message);
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function openAuthorizationUrl(authorization: OidcDeviceAuthorization | null): void {
  const url = authorization?.verification_uri_complete || authorization?.verification_uri;
  if (!url) return;
  window.open(url, "_blank");
}
