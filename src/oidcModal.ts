import { App, Modal, Notice, Setting } from "obsidian";
import { GitService, OidcDeviceAuthorization } from "./gitService";

export class OidcDeviceLoginModal extends Modal {
  private authorization: OidcDeviceAuthorization | null = null;
  private statusEl: HTMLElement;

  constructor(
    app: App,
    private readonly gitService: GitService
  ) {
    super(app);
  }

  async onOpen(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "OIDC device login" });
    this.statusEl = contentEl.createEl("p", { text: "Starting device login..." });

    try {
      this.authorization = await this.gitService.beginOidcDeviceLogin();
      this.renderAuthorization();
      await this.gitService.pollOidcDeviceLogin(
        this.authorization.device_code,
        this.authorization.interval ?? 5,
        this.authorization.expires_in
      );
      this.statusEl.setText("Login complete.");
      this.close();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.statusEl.setText(`Login failed: ${message}`);
    }
  }

  private renderAuthorization(): void {
    if (!this.authorization) return;
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "OIDC device login" });
    contentEl.createEl("p", { text: "Open the verification URL and enter the code." });
    contentEl.createEl("p", { text: this.authorization.verification_uri_complete || this.authorization.verification_uri });
    contentEl.createEl("h3", { text: this.authorization.user_code });

    new Setting(contentEl)
      .addButton((button) =>
        button.setButtonText("Copy code").onClick(async () => {
          await navigator.clipboard.writeText(this.authorization?.user_code ?? "");
          new Notice("OIDC user code copied");
        })
      )
      .addButton((button) =>
        button.setButtonText("Copy URL").onClick(async () => {
          await navigator.clipboard.writeText(this.authorization?.verification_uri_complete || this.authorization?.verification_uri || "");
          new Notice("OIDC verification URL copied");
        })
      );

    this.statusEl = contentEl.createEl("p", { text: "Waiting for authorization..." });
  }
}
