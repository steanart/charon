// Settings: appearance, start-up, background, notifications, trash, checks and updates. Saved on every change.
// "Pause everything" has its own command; its state only ever comes from Rust.

import { Api } from "../api.js";
import { Dom } from "../dom.js";
import { Icons } from "../icons.js";
import { Modal } from "./modal.js";
import { Platform } from "../platform.js";
import { Sheet } from "./sheet.js";
import { Theme } from "../theme.js";

const { html, raw } = Dom;

export class SettingsPanel extends Sheet {
  constructor(store, toast) {
    super(Dom.$("#settings"));
    this.store = store;
    this.toast = toast;
    this.unsaved = new Map();           // field → value, changes still on their way to Rust
    this.saving = Promise.resolve();    // saves go out one at a time
    this.element.addEventListener("change", event => this.onChange(event));
    this.element.addEventListener("click", event => {
      if (event.target.closest("[data-do=close]")) this.close();
      const appearance = event.target.closest("[data-appearance]");
      if (appearance) this.save("appearance", appearance.dataset.appearance);
    });
  }

  show() {
    this.render();
    this.open();
  }

  toggleOpen() {
    this.isOpen ? this.close() : this.show();
  }

  /** New settings from Rust: redraws, unless the person is typing in a field. */
  refresh() {
    if (!this.isOpen) return;
    if (this.element.contains(document.activeElement) && document.activeElement.matches("input.input")) return;
    this.redraw(() => this.render());
  }

  /** The settings as Rust has them, with the changes still on their way. */
  current() {
    return { ...this.store.settings, ...Object.fromEntries(this.unsaved) };
  }

  render() {
    if (!this.store.settings) return;
    const s = this.current();
    const toggle = (field, title, text) => html`<label class="toggle">
        <span class="copy"><strong>${title}</strong><small>${text}</small></span>
        <input type="checkbox" data-field="${field}" ${s[field] ? "checked" : ""}><span class="switch"></span>
      </label>`;
    const number = (field, title, text, unit, min, max) => html`<div class="setting-row">
        <span class="copy"><strong>${title}</strong><small>${text}</small></span>
        <span class="unit"><input class="input small" type="number" min="${min}" max="${max}" data-field="${field}" value="${s[field]}">${unit}</span>
      </div>`;
    const appearance = s.appearance || "system";
    const choice = (value, label) => html`<button type="button" data-appearance="${value}" class="${appearance === value ? "on" : ""}">${label}</button>`;
    this.element.innerHTML = html`
      <header>
        <div><span class="eyebrow"><span class="num">02</span> Settings</span><h2>The ferryman's rules</h2></div>
        <button class="icon-btn" data-do="close" title="Close"><span class="ico">${raw(Icons.close)}</span></button>
      </header>
      <div class="scroll"><div class="settings-list">
        <div class="setting-row">
          <span class="copy"><strong>Appearance</strong><small>Dark like the river at night, light like paper, or whatever your system uses.</small></span>
          <span class="segmented">${raw(choice("system", "System"))}${raw(choice("light", "Light"))}${raw(choice("dark", "Dark"))}</span>
        </div>
        ${raw(toggle("launchAtLogin", "Launch at login", `Charon starts quietly in the ${Platform.tray()} when you log in.`))}
        ${raw(toggle("runInBackground", "Keep running when the window is closed", `Charon keeps looking for changes from the ${Platform.tray()}, and jobs with full automation keep syncing.`))}
        ${raw(toggle("notifications", "Notifications", "Errors, conflicts, big syncs, and changes waiting for your OK."))}
        ${raw(toggle("pausedAll", "Pause everything", "Nothing syncs by itself. Charon still looks for changes and shows what's waiting."))}
        ${raw(number("trashDays", "Keep deleted files", "Charon keeps removed and replaced files in a hidden .charon-trash folder on each drive for this many days. 0 keeps them until you empty it. Only a sync you confirmed empties it: syncs with full automation never do.", "days", 0, 3650))}
        ${raw(number("checkMinutes", "Look for changes", "How often Charon looks for changes in each job, besides watching the folders.", "min", 1, 1440))}
        ${raw(toggle("checkUpdates", "Look for new versions", "Charon asks GitHub at start-up and once a day. Nothing is downloaded without you."))}
      </div></div>`;
  }

  async onChange(event) {
    const el = event.target;
    const field = el.dataset.field;
    if (!field) return;
    if (field === "pausedAll") return this.setPausedAll(el.checked);
    if (el.type === "checkbox") return this.save(field, el.checked);
    const value = Math.min(Number(el.max) || Infinity, Math.max(Number(el.min) || 0, Math.round(Number(el.value) || 0)));
    if (field === "trashDays" && this.shortensTrash(value) && !(await this.confirmShorterTrash(value))) return this.render();
    this.save(field, value);
  }

  /** Keeping deleted files for fewer days than now (0 means until emptied). */
  shortensTrash(days) {
    const now = this.current().trashDays;
    return days > 0 && (now === 0 || days < now);
  }

  confirmShorterTrash(days) {
    return Modal.confirm({
      title: "Keep deleted files for less time?",
      body: `Files in .charon-trash older than ${days === 1 ? "1 day" : `${days} days`} will be removed for good at the next sync of each job. Continue?`,
      action: "Continue",
    });
  }

  /** "Pause everything": the switch shows what Rust says once it has answered. */
  async setPausedAll(paused) {
    try {
      await Api.setAllPaused(paused);
    } catch (error) {
      this.toast.error(error);
      this.render();
    }
  }

  /**
   * Saves one setting; appearance changes show at once. Rust replaces every setting with
   * what it's sent, so saves go out one after another, each with the changes made before
   * it: a second change made while the first is on its way never sends the first one back.
   */
  save(field, value) {
    this.unsaved.set(field, value);
    if (field === "appearance") {
      Theme.apply(value);
      this.redraw(() => this.render());
    }
    this.saving = this.saving.catch(error => console.error(error)).then(() => this.send(field, value));
  }

  /** Sends the settings with every change still on its way. A refusal shows what Rust has. */
  async send(field, value) {
    let refused = false;
    try {
      await Api.setSettings(this.current());
      // The settings event brings Rust's copy; until then, keep the one field we know changed.
      this.store.settings = { ...this.store.settings, [field]: value };
    } catch (error) {
      refused = true;
      this.toast.error(error);
    }
    if (this.unsaved.get(field) !== value) return; // changed again meanwhile: that save comes next
    this.unsaved.delete(field);
    if (refused) {
      Theme.apply(this.store.settings?.appearance);
      this.redraw(() => this.render());
    }
  }
}
