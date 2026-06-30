import { App, Modal, Setting } from "obsidian";
import type { TextComponent } from "obsidian";

export interface InitialSyncHandlers {
  forcePush: () => Promise<void>;
  overwriteLocal: (backupFolder: string) => Promise<void>;
}

export class InitialSyncModal extends Modal {
  private backupFolder: string;

  constructor(
    app: App,
    private readonly defaultBackupFolder: string,
    private readonly handlers: InitialSyncHandlers,
    private readonly onDismiss?: () => void
  ) {
    super(app);
    this.backupFolder = defaultBackupFolder;
  }

  onOpen(): void {
    this.renderChoice();
  }

  onClose(): void {
    this.contentEl.empty();
    this.onDismiss?.();
  }

  private renderChoice(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "First sync for this vault" });
    contentEl.createEl("p", {
      text:
        "This vault has not been synced with the server yet. Choose how to reconcile your local files with what is already on the server."
    });

    new Setting(contentEl)
      .setName("Upload local vault to server")
      .setDesc("Force push: the server is overwritten so it matches this device. Anything only on the server is removed.")
      .addButton((button) =>
        button
          .setWarning()
          .setButtonText("Force push")
          .onClick(() => this.renderForcePushConfirm())
      );

    new Setting(contentEl)
      .setName("Download server vault to this device")
      .setDesc("Overwrite local: your local files are replaced with the server's. A backup is saved first.")
      .addButton((button) =>
        button
          .setCta()
          .setButtonText("Overwrite local")
          .onClick(() => this.renderBackupPrompt())
      );

    new Setting(contentEl).addButton((button) =>
      button.setButtonText("Cancel").onClick(() => this.close())
    );
  }

  private renderForcePushConfirm(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Overwrite the server?" });
    contentEl.createEl("p", {
      text:
        "Force push replaces everything on the server with the contents of this device. All data currently on the server for this vault will be permanently lost. This cannot be undone."
    });
    contentEl.createEl("p", { text: "Are you sure you want to overwrite all server data?" });

    new Setting(contentEl)
      .addButton((button) => button.setButtonText("Go back").onClick(() => this.renderChoice()))
      .addButton((button) =>
        button
          .setWarning()
          .setButtonText("Yes, overwrite the server")
          .onClick(() => this.run(() => this.handlers.forcePush()))
      );
  }

  private renderBackupPrompt(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Back up before overwriting" });
    contentEl.createEl("p", {
      text:
        "Your local files will be replaced with the server's. They are copied to a backup folder inside this vault first. Leave the field empty to skip the backup."
    });

    let input: TextComponent | null = null;
    new Setting(contentEl)
      .setName("Backup folder")
      .setDesc("Folder path inside this vault where the current local files are copied.")
      .addText((text) => {
        input = text;
        text
          .setPlaceholder(this.defaultBackupFolder)
          .setValue(this.backupFolder)
          .onChange((value) => {
            this.backupFolder = value;
          });
      });

    new Setting(contentEl)
      .addButton((button) => button.setButtonText("Go back").onClick(() => this.renderChoice()))
      .addButton((button) =>
        button
          .setCta()
          .setButtonText("Back up and download")
          .onClick(() => this.run(() => this.handlers.overwriteLocal(this.backupFolder.trim())))
      );

    window.setTimeout(() => input?.inputEl.focus(), 0);
  }

  private run(operation: () => Promise<void>): void {
    this.close();
    void operation();
  }
}
