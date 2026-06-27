import { App, Modal, Notice, Setting } from "obsidian";
import type { TextComponent } from "obsidian";

export class ComputerNameModal extends Modal {
  private value: string;

  constructor(
    app: App,
    currentName: string,
    private readonly saveName: (name: string) => Promise<void>
  ) {
    super(app);
    this.value = currentName;
  }

  onOpen(): void {
    const { contentEl } = this;
    contentEl.empty();
    contentEl.createEl("h2", { text: "Computer name" });

    let input: TextComponent | null = null;
    new Setting(contentEl)
      .setName("Name")
      .setDesc("Used as the source computer name for sync history.")
      .addText((text) => {
        input = text;
        text.setPlaceholder("Keller MacBook").setValue(this.value).onChange((value) => {
          this.value = value;
        });
        text.inputEl.addEventListener("keydown", (event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            void this.save();
          }
        });
      });

    new Setting(contentEl).addButton((button) =>
      button
        .setCta()
        .setButtonText("Save")
        .onClick(() => {
          void this.save();
        })
    );

    window.setTimeout(() => {
      if (!input) return;
      input.inputEl.focus();
      input.inputEl.select();
    }, 0);
  }

  private async save(): Promise<void> {
    const name = this.value.trim();
    await this.saveName(name);
    new Notice(name ? `Computer name set to ${name}` : "Computer name cleared");
    this.close();
  }
}
