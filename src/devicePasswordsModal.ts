import { App, Modal, Notice, Setting } from "obsidian";
import type { ButtonComponent } from "obsidian";
import { DEFAULT_DEVICE_FOLDER, describeDevicePassword, deviceUrl, normalizeDeviceFolder, webdavUrl } from "./devicePasswords";
import { GitService } from "./gitService";
import { CreatedDevicePassword } from "./protocol";

/**
 * Creates, lists, and revokes per-device WebDAV passwords for the current vault.
 * The generated password is shown exactly once, right after creation.
 */
export class DevicePasswordsModal extends Modal {
  private label = "";
  private folder = DEFAULT_DEVICE_FOLDER;
  private createdEl: HTMLElement | null = null;
  private listEl: HTMLElement | null = null;
  private statusEl: HTMLElement | null = null;

  constructor(
    app: App,
    private readonly gitService: GitService,
    private readonly serverUrl: string
  ) {
    super(app);
  }

  async onOpen(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Device passwords" });
    contentEl.createEl("p", {
      text:
        "Give an e-ink tablet or another WebDAV client access to one folder of this vault. " +
        "Each device gets its own password that you can revoke at any time. " +
        "Files the device uploads appear in Obsidian after the next sync."
    });
    contentEl.createEl("p", {
      text:
        "Using the Saber handwriting app? Do not create a password here: in Saber choose \"Log in with Nextcloud\", " +
        "enter this sync server's URL, and finish the login in the browser. The device then appears in this list."
    });

    if (this.gitService.loginStatus().state !== "logged-in") {
      contentEl.createEl("p", { text: "Log in to ObsidiSync before managing device passwords." });
      return;
    }

    const checking = contentEl.createEl("p", { text: "Checking the sync server..." });
    try {
      const unavailable = await this.gitService.devicePasswordsUnavailableReason();
      if (unavailable) {
        checking.setText(unavailable);
        return;
      }
    } catch (error) {
      checking.setText(`Could not reach the sync server: ${errorMessage(error)}`);
      return;
    }
    checking.remove();

    this.createdEl = contentEl.createDiv();

    contentEl.createEl("h3", { text: "New device password" });
    new Setting(contentEl)
      .setName("Device name")
      .setDesc("Shown in sync history for files this device uploads.")
      .addText((text) =>
        text.setPlaceholder("Boox tablet").onChange((value) => {
          this.label = value.trim();
        })
      );

    new Setting(contentEl)
      .setName("Folder")
      .setDesc("Vault folder the device may read and write. It is created on the first upload.")
      .addText((text) =>
        text
          .setPlaceholder("Tablet/Notes")
          .setValue(this.folder)
          .onChange((value) => {
            this.folder = value;
          })
      );

    new Setting(contentEl).addButton((button) =>
      button
        .setCta()
        .setButtonText("Create password")
        .onClick(() => void this.create(button))
    );

    contentEl.createEl("h3", { text: "Existing device passwords" });
    this.listEl = contentEl.createDiv();
    this.statusEl = contentEl.createEl("p", { text: "" });
    await this.refreshList();
  }

  private async create(button: ButtonComponent): Promise<void> {
    try {
      if (!this.label) throw new Error("Enter a device name");
      const folder = normalizeDeviceFolder(this.folder);
      button.setDisabled(true);
      const created = await this.gitService.createDevicePassword(this.label, folder);
      this.renderCreated(created);
      await this.refreshList();
      this.setStatus("");
    } catch (error) {
      this.setStatus(`Could not create device password: ${errorMessage(error)}`);
    } finally {
      button.setDisabled(false);
    }
  }

  private renderCreated(created: CreatedDevicePassword): void {
    const container = this.createdEl;
    if (!container) return;
    container.empty();
    container.createEl("h3", { text: `Password for ${created.label}` });
    container.createEl("p", {
      text: "Enter these values in the device's WebDAV settings. The password is shown only once; create a new one if you lose it."
    });

    const url = webdavUrl(this.serverUrl, created.webdavPath);
    this.renderCopyRow(container, "WebDAV URL", url);
    this.renderCopyRow(container, "Username", created.username);
    this.renderCopyRow(container, "Password", created.password);
  }

  private renderCopyRow(container: HTMLElement, name: string, value: string): void {
    new Setting(container)
      .setName(name)
      .setDesc(value)
      .addButton((button) =>
        button.setButtonText("Copy").onClick(async () => {
          await navigator.clipboard.writeText(value);
          new Notice(`${name} copied`);
        })
      );
  }

  private async refreshList(): Promise<void> {
    const listEl = this.listEl;
    if (!listEl) return;
    listEl.empty();
    try {
      const entries = await this.gitService.listDevicePasswords();
      if (entries.length === 0) {
        listEl.createEl("p", { text: "No device passwords yet." });
        return;
      }
      for (const entry of entries) {
        new Setting(listEl)
          .setName(entry.label)
          .setDesc(describeDevicePassword(entry, this.serverUrl))
          .addButton((button) =>
            button.setButtonText("Copy URL").onClick(async () => {
              await navigator.clipboard.writeText(deviceUrl(entry, this.serverUrl));
              new Notice(entry.kind === "saber" ? "Server URL for Saber copied" : "WebDAV URL copied");
            })
          )
          .addButton((button) =>
            button
              .setWarning()
              .setButtonText("Revoke")
              .onClick(async () => {
                try {
                  button.setDisabled(true);
                  await this.gitService.revokeDevicePassword(entry.id);
                  new Notice(`Revoked device password for ${entry.label}`);
                  await this.refreshList();
                } catch (error) {
                  button.setDisabled(false);
                  this.setStatus(`Could not revoke device password: ${errorMessage(error)}`);
                }
              })
          );
      }
    } catch (error) {
      this.setStatus(`Could not load device passwords: ${errorMessage(error)}`);
    }
  }

  private setStatus(message: string): void {
    this.statusEl?.setText(message);
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
