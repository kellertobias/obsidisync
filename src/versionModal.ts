import { App, Modal, Notice, Setting } from "obsidian";
import { base64ToArrayBuffer } from "./base64";
import { GitService } from "./gitService";
import { HistoryEntry, VersionFileResponse } from "./protocol";

export class FileVersionsModal extends Modal {
  constructor(
    app: App,
    private readonly gitService: GitService,
    private readonly filePath: string
  ) {
    super(app);
  }

  async onOpen(): Promise<void> {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: `Versions: ${this.filePath}` });
    contentEl.createEl("p", { text: "Select a version to preview it read-only." });

    try {
      const history = await this.gitService.history(this.filePath);
      if (history.length === 0) {
        contentEl.createEl("p", { text: "No versions found." });
        return;
      }

      for (const entry of history.slice(0, 50)) {
        new Setting(contentEl)
          .setName(entry.subject || entry.hash.slice(0, 12))
          .setDesc(`${entry.date} - ${entry.author} - ${entry.hash.slice(0, 12)}`)
          .addButton((button) =>
            button.setButtonText("Preview").onClick(async () => {
              await this.preview(entry);
            })
          );
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      contentEl.createEl("p", { text: `Could not load versions: ${message}` });
    }
  }

  private async preview(entry: HistoryEntry): Promise<void> {
    const version = await this.gitService.fileAtVersion(this.filePath, entry.hash);
    const preview = new VersionPreviewModal(this.app, this.filePath, entry, version);
    preview.open();
  }
}

class VersionPreviewModal extends Modal {
  private content: ArrayBuffer;
  private text: string | null;

  constructor(
    app: App,
    private readonly filePath: string,
    private readonly entry: HistoryEntry,
    private readonly version: VersionFileResponse
  ) {
    super(app);
    this.content = base64ToArrayBuffer(version.contentBase64);
    this.text = decodeTextIfPossible(this.content);
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: this.filePath });
    contentEl.createEl("p", { text: `${this.entry.date} - ${this.entry.hash.slice(0, 12)}` });

    if (this.text !== null) {
      const textarea = contentEl.createEl("textarea");
      textarea.value = this.text;
      textarea.readOnly = true;
      textarea.style.width = "100%";
      textarea.style.height = "45vh";

      new Setting(contentEl)
        .addButton((button) =>
          button.setButtonText("Copy text").onClick(async () => {
            await navigator.clipboard.writeText(this.text ?? "");
            new Notice("Version text copied");
          })
        )
        .addButton((button) =>
          button.setButtonText("Replace current file").onClick(async () => {
            await this.app.vault.adapter.write(this.filePath, this.text ?? "");
            new Notice("Current file replaced with selected version");
            this.close();
          })
        );
      return;
    }

    contentEl.createEl("p", { text: "Binary version preview is not displayed as text." });
    new Setting(contentEl).addButton((button) =>
      button.setButtonText("Restore binary file").onClick(async () => {
        await this.app.vault.adapter.writeBinary(this.filePath, this.content);
        new Notice("Current binary file replaced with selected version");
        this.close();
      })
    );
  }
}

function decodeTextIfPossible(buffer: ArrayBuffer): string | null {
  const bytes = new Uint8Array(buffer);
  if (bytes.includes(0)) return null;
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}
