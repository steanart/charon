// The job editor: a numbered walk through everything a job needs.
// 01 Shores · 02 Mode · 03 Conflicts (two-way only) · 04 When · 05 Leave out · 06 Safety · 07 Name
//
// Full automation lets a job change files without showing the list first. It's off by
// default, and the draft only changes after two warnings, the second with a tick and a
// red button that Enter never presses.

import { Api } from "../api.js";
import { Dom } from "../dom.js";
import { Format } from "../format.js";
import { Icons } from "../icons.js";
import { Modal } from "./modal.js";
import { Overlay } from "./overlay.js";
import { Sheet } from "./sheet.js";

const { html, raw } = Dom;

const MODES = [
  { key: "mirror", title: "Mirror" },
  { key: "backup", title: "Backup" },
  { key: "two-way", title: "Two-way" },
];
const RULES = [
  { key: "keep-both", label: "Keep both" },
  { key: "newest-wins", label: "Newest wins" },
  { key: "ask", label: "Ask me" },
];
const TRIGGERS = [
  { key: "manual", label: "Manual", make: () => ({ kind: "manual" }) },
  { key: "on-change", label: "On change", make: () => ({ kind: "on-change", settleSeconds: 5 }) },
  { key: "interval", label: "Every…", make: () => ({ kind: "interval", minutes: 60 }) },
  { key: "daily", label: "Daily", make: () => ({ kind: "daily", hour: 3, minute: 0 }) },
  { key: "drive-connected", label: "Drive connected", make: () => ({ kind: "drive-connected" }) },
];

/** The fields of a job that the editor changes. */
const SETUP_FIELDS = ["name", "source", "destination", "mode", "conflict", "trigger", "excludes", "safeDelete", "fullAutomation"];

/** How full automation can hurt, per mode (the first warning). */
const AUTOMATION_RISKS = {
  "two-way": "In two-way mode a file removed or damaged on either side is removed or replaced on the other, including your originals.",
  mirror: "In Mirror mode anything removed or damaged in the source is removed or replaced in the destination.",
  backup: "In Backup mode a damaged file in the source replaces the good copy in the destination.",
};

/** What a two-way sync without a memory of the last one does with files that differ, per rule. */
const DIFFERING_FILES = {
  ask: "files that differ are left for you to choose",
  "keep-both": "files that differ are kept twice",
  "newest-wins": "where files differ, the version saved last replaces the other, which goes to .charon-trash (both are kept when the times can't tell)",
};

export class JobEditor extends Sheet {
  constructor(store, toast, { saved }) {
    super(Dom.$("#editor"));
    this.store = store;
    this.toast = toast;
    this.saved = saved;
    this.asking = false;   // a warning is open
    this.element.addEventListener("click", event => this.onClick(event));
    this.element.addEventListener("input", event => this.onInput(event));
    this.element.addEventListener("keydown", event => this.onKey(event));
  }

  /** Opens an empty editor for a new job. */
  create() {
    this.editingId = null;
    this.original = null;
    this.nameTouched = false;
    this.automationReset = null;
    this.draft = {
      name: "",
      source: "",
      destination: "",
      mode: "mirror",
      conflict: "keep-both",
      trigger: { kind: "manual" },
      excludes: [...this.store.defaultExcludes],
      safeDelete: true,
      fullAutomation: false,
    };
    this.render();
    this.open();
  }

  /**
   * Opens the editor with an existing job. With `automation`, it goes straight to the full
   * automation switch and its warnings; with `over`, the sheet underneath stays open.
   */
  edit(id, { automation = false, over = false } = {}) {
    const job = this.store.jobs.get(id);
    if (!job) return;
    this.editingId = id;
    this.nameTouched = true;
    this.automationReset = null;
    this.original = structuredClone(Object.fromEntries(SETUP_FIELDS.map(field => [field, job[field]])));
    this.original.fullAutomation = Boolean(this.original.fullAutomation);
    this.draft = structuredClone(this.original);
    this.render();
    this.open({ over });
    if (automation && !this.draft.fullAutomation) {
      const toggle = Dom.$('[data-field="fullAutomation"]', this.element)?.closest(".toggle");
      toggle?.scrollIntoView({ block: "center" });
      toggle?.classList.add("flash");
      this.askFullAutomation();
    }
  }

  // ---------------------------------------------------------------- drawing

  render() {
    const d = this.draft;
    const editing = Boolean(this.editingId);
    this.element.innerHTML = html`
      <header>
        <div>
          <span class="eyebrow"><span class="num">${editing ? "EDIT" : "NEW"}</span> Sync job</span>
          <h2>${editing ? d.name : "Plan a crossing"}</h2>
        </div>
        <button class="icon-btn" data-do="close" title="Close"><span class="ico">${raw(Icons.close)}</span></button>
      </header>
      <div class="scroll">
        ${raw(this.step(1, "Shores", "Where files come from, and where they go.", this.shoresHtml()))}
        ${raw(this.step(2, "Mode", "How files cross.", this.modesHtml()))}
        ${raw(this.step(3, "When both sides change", "Two-way only: what happens if a file was edited on both sides.", this.rulesHtml(), d.mode !== "two-way"))}
        ${raw(this.step(4, "When", "When Charon looks for changes. It shows you the list and waits for your OK, unless full automation is on.", this.triggerHtml()))}
        ${raw(this.step(5, "Leave out", "Names like node_modules or *.tmp match anywhere; a/b matches from the top folder. Press Enter to add.", this.chipsHtml()))}
        ${raw(this.step(6, "Safety", "", this.safetyHtml()))}
        ${raw(this.step(7, "Name", "", html`<input class="input" data-field="name" value="${d.name}" placeholder="${this.suggestedName() || "Photos → Backup SSD"}" spellcheck="false">`))}
      </div>
      ${raw(this.notesHtml())}
      <footer>
        ${raw(editing ? html`<button class="btn danger" data-do="delete">Delete</button>` : "")}
        <span class="spacer"></span>
        <button class="btn ghost" data-do="close">Cancel</button>
        <button class="btn primary" data-do="save">${editing ? "Save" : "Save and review"}</button>
      </footer>`;
  }

  step(number, title, hint, body, hidden = false) {
    return html`<section class="step ${hidden ? "hidden" : ""}">
        <span class="n">${String(number).padStart(2, "0")}</span>
        <div class="what">
          <h3>${title}</h3>
          ${raw(hint ? html`<p class="hint">${hint}</p>` : "")}
          ${raw(body)}
        </div>
      </section>`;
  }

  shoresHtml() {
    const pick = (field, label) => {
      const path = this.draft[field];
      return html`<div class="field"><span class="field-label">${label}</span>
        <div class="path-pick ${path ? "filled" : ""}" data-do="pick" data-field="${field}">
          <span class="ico">${raw(Icons.folder)}</span>
          <span class="path ${path ? "" : "blank"}">${path || "Choose a folder…"}</span>
          <span class="btn ghost">Choose</span>
        </div></div>`;
    };
    return html`<div class="shores-pick">
        ${raw(pick("source", "Source"))}
        <button class="icon-btn swap" data-do="swap" title="Swap direction"><span class="ico">${raw(Icons.swap)}</span></button>
        ${raw(pick("destination", "Destination"))}
      </div>`;
  }

  modesHtml() {
    return html`<div class="cards">${MODES.map(m => raw(html`
      <button class="card-choice ${this.draft.mode === m.key ? "on" : ""}" data-do="mode" data-value="${m.key}">
        <span class="diagram">${raw(Icons.mode[m.key])}</span>
        <strong>${m.title}</strong>
        <small>${Format.modeSentence(m.key, this.draft.safeDelete)}</small>
      </button>`))}</div>`;
  }

  rulesHtml() {
    return html`${raw(this.segmented("conflict", RULES, this.draft.conflict))}
      <p class="choice-says">${Format.ruleSentence(this.draft.conflict)}</p>`;
  }

  segmented(field, options, current) {
    return html`<div class="segmented">${options.map(o => raw(html`
      <button data-do="choose" data-field="${field}" data-value="${o.key}" class="${current === o.key ? "on" : ""}">${o.label}</button>`))}</div>`;
  }

  triggerHtml() {
    const d = this.draft;
    const t = d.trigger;
    let extra = "";
    if (t.kind === "on-change") {
      extra = html`Wait <input class="input small" type="number" min="1" max="3600" data-trigger="settleSeconds" value="${t.settleSeconds}"> seconds after the last change`;
    } else if (t.kind === "interval") {
      extra = html`Every <input class="input small" type="number" min="1" data-trigger="minutes" value="${t.minutes}"> minutes`;
    } else if (t.kind === "daily") {
      extra = html`At <input class="input small" type="time" data-trigger="time" value="${Format.clock(t.hour, t.minute)}" style="width: 110px"> every day`;
    } else if (t.kind === "drive-connected") {
      extra = "As soon as both folders are reachable, for example when you plug in the backup drive.";
    }
    return html`${raw(this.segmented("trigger", TRIGGERS, t.kind))}
      <div class="trigger-extra">${raw(extra)}</div>
      <p class="choice-says" data-slot="trigger-says">${this.triggerSays()}</p>
      <label class="toggle danger automation">
        <span class="copy"><strong>Full automation</strong>
          <small>Syncs by itself, without showing you the list of changes first. Off: Charon prepares the list and waits for your OK.</small></span>
        <input type="checkbox" data-field="fullAutomation" ${d.fullAutomation ? "checked" : ""}><span class="switch"></span>
      </label>`;
  }

  /** "Looks every hour and asks you." or, with full automation, "Syncs every hour." */
  triggerSays() {
    return `${Format.trigger(this.draft.trigger, this.draft.fullAutomation)}.`;
  }

  chipsHtml() {
    return html`<div class="chips" data-do="focus-chips">
        ${this.draft.excludes.map((pattern, i) => raw(html`<span class="chip">${pattern}<button data-do="unchip" data-index="${i}" title="Remove">×</button></span>`))}
        <input data-field="chip" placeholder="Add a pattern" spellcheck="false">
      </div>`;
  }

  safetyHtml() {
    const off = !this.draft.safeDelete;
    return html`<label class="toggle">
        <span class="copy"><strong>Safe deletes</strong>
          <small>${Format.trashSentence(this.trashDays())}</small></span>
        <input type="checkbox" data-field="safeDelete" ${off ? "" : "checked"}><span class="switch"></span>
      </label>
      ${raw(off ? '<p class="choice-says bad">Safe deletes are off for this job: removed and replaced files are gone at once.</p>' : "")}`;
  }

  /** Notes that stay in sight above the buttons while the draft says so. */
  notesHtml() {
    const notes = [];
    if (this.automationReset && !this.draft.fullAutomation) {
      notes.push(html`<div class="notice caution"><span class="ico">${raw(Icons.warn)}</span>
        <span>Full automation was turned off because ${this.automationReset}. The warnings you accepted spoke of the job as it was; turn it on again to read what it means now.</span></div>`);
    }
    if (this.draft.fullAutomation) {
      notes.push(html`<div class="notice danger"><span class="ico">${raw(Icons.warn)}</span>
        <span>Full automation is on: this job changes files without asking you first.</span></div>`);
    }
    if (this.forgetsWhatWasSynced()) {
      notes.push(html`<div class="notice caution"><span class="ico">${raw(Icons.warn)}</span><span>${this.resetWarning()}</span></div>`);
    }
    return `<div class="sheet-notes">${notes.join("")}</div>`;
  }

  trashDays() {
    return this.store.settings?.trashDays ?? 30;
  }

  /** "Photos → Backup SSD", from the chosen folders. */
  suggestedName() {
    const { source, destination, mode } = this.draft;
    if (!source || !destination) return "";
    const base = p => p.split(/[\\/]/).filter(Boolean).pop() || p;
    return `${base(source)} ${Format.arrow(mode)} ${base(destination)}`;
  }

  /** Redraws, keeping the scroll position and the focus. */
  refresh() {
    this.redraw(() => this.render());
  }

  // ---------------------------------------------------------------- what changed

  /** Changing the folders or the mode of a saved two-way job makes Charon forget what was synced. */
  forgetsWhatWasSynced() {
    const o = this.original;
    const d = this.draft;
    if (!o || o.mode !== "two-way") return false;
    return o.source !== d.source || o.destination !== d.destination || o.mode !== d.mode;
  }

  /**
   * What the next sync does once Charon forgot what was synced, by the mode the draft has:
   * two-way compares both folders afresh and removes nothing, Mirror makes the destination a
   * copy of the source, and Backup puts the source's version over files that differ.
   */
  resetWarning() {
    const d = this.draft;
    const forget = "Charon will forget what was synced.";
    switch (d.mode) {
      case "mirror": {
        const gone = d.safeDelete ? "" : " Safe deletes are off, so what it removes or replaces is gone for good.";
        return `${forget} As a mirror, the next sync makes the destination a copy of the source: it removes everything that is only in the destination, and the source's version replaces files that differ.${gone}`;
      }
      case "backup": {
        const gone = d.safeDelete ? "" : " Safe deletes are off, so what it replaces is gone for good.";
        return `${forget} As a backup, the next sync removes nothing, but where a file differs, the source's version replaces the destination's.${gone}`;
      }
      default:
        return `${forget} This time it compares both folders afresh and removes nothing, so files deleted on one side since the last sync come back, and ${DIFFERING_FILES[d.conflict]}.`;
    }
  }

  /** True when the saved change alters what a sync does, so the next sync waits for an OK. */
  changesWhatSyncs() {
    const o = this.original;
    const d = this.draft;
    if (!o) return false;
    return o.source !== d.source
      || o.destination !== d.destination
      || o.mode !== d.mode
      || o.conflict !== d.conflict
      || o.safeDelete !== d.safeDelete
      || JSON.stringify(o.excludes) !== JSON.stringify(d.excludes)
      || (!o.fullAutomation && d.fullAutomation);
  }

  /**
   * The warnings someone accepted describe one mode and one safe-deletes setting. When
   * either changes, full automation goes off in the draft, and turning it on again shows
   * the warnings as they read now.
   */
  resetAutomation(because) {
    if (!this.draft.fullAutomation) return;
    this.draft.fullAutomation = false;
    this.automationReset = because;
  }

  // ---------------------------------------------------------------- the two warnings

  /**
   * Asks twice before full automation goes on. Cancel at any point leaves it off; only the
   * red button of the second warning puts it in the draft.
   */
  async askFullAutomation() {
    if (this.asking) return;
    this.asking = true;
    const d = this.draft;
    try {
      const name = d.name.trim() || this.suggestedName() || "this job";
      const days = this.trashDays();
      const trash = d.safeDelete
        ? `Removed and replaced files go to .charon-trash on the same drive and are kept ${days > 0 ? `for ${Format.plural(days, "day")}` : "until you empty it"}.`
        : "Safe deletes are off for this job: removed and replaced files are gone at once.";
      const first = await Modal.ask({
        tone: "danger",
        title: "Full automation can lose data",
        body: [
          `With full automation, “${name}” copies, replaces and removes files by itself, without showing you the list first. ${AUTOMATION_RISKS[d.mode]}`,
          "A wrong folder, a drive mounted in another place, damaged or encrypted files, or a mistake on one side is carried over before you notice.",
          "Charon still stops when one side looks empty, when too much would be removed or replaced within a day (half of a side's files, 5 or more; more than a quarter of them, 21 or more; or over 1,000), or when its own syncs since you last confirmed a list have removed or replaced half of a side's files (5 or more) or more than a quarter of them (21 or more). Damage smaller than that is carried over.",
          trash,
        ],
        buttons: [
          { label: "Cancel", value: null, focus: true },
          { label: "I understand, continue", value: "continue", kind: "caution" },
        ],
      });
      if (first !== "continue") return;
      const second = await Modal.ask({
        tone: "danger",
        title: "Last warning",
        body: "Data can be lost in this mode, no matter the reason. Use it only with care, and only if you know exactly what it does.",
        tick: `I understand that data can be lost and I accept the risk.${d.safeDelete ? "" : " Removed and replaced files are gone at once."}`,
        buttons: [
          { label: "Cancel", value: null, focus: true },
          { label: "Turn on full automation", value: "on", kind: "danger", needsTick: true },
        ],
      });
      if (second === "on" && this.draft === d && this.isOpen) d.fullAutomation = true;
    } finally {
      this.asking = false;
      if (this.draft === d && this.isOpen) this.refresh(); // the switch shows what the draft says
    }
  }

  /** Turning safe deletes off asks first. */
  async askSafeDeletesOff() {
    const d = this.draft;
    const body = ["Removed and replaced files will be gone for good, with no copy in .charon-trash. Turn safe deletes off?"];
    if (d.mode === "two-way") body.push("In two-way jobs Charon still keeps the replaced version of a file.");
    if (d.fullAutomation) body.push("Full automation goes off too: turn it on again to read what it means without safe deletes.");
    const off = await Modal.confirm({ title: "Safe deletes", body, action: "Turn safe deletes off" });
    if (off && this.draft === d) {
      d.safeDelete = false;
      this.resetAutomation("safe deletes were turned off");
    }
    if (this.draft === d && this.isOpen) this.refresh();
  }

  // ---------------------------------------------------------------- input

  async onClick(event) {
    const target = event.target.closest("[data-do]");
    if (!target) return;
    const d = this.draft;
    switch (target.dataset.do) {
      case "close": return this.close();
      case "pick": {
        const field = target.dataset.field;
        const folder = await Api.pickFolder(d[field] || d.source || d.destination);
        if (folder) {
          d[field] = folder;
          this.autoName();
          this.refresh();
        }
        return;
      }
      case "swap":
        // One way, the other folder now gets overwritten: worth reading the warnings again.
        if (d.mode !== "two-way" && d.source !== d.destination) this.resetAutomation("the source and destination were swapped");
        [d.source, d.destination] = [d.destination, d.source];
        this.autoName();
        return this.refresh();
      case "mode":
        if (d.mode !== target.dataset.value) this.resetAutomation(`the mode changed to ${Format.modeName(target.dataset.value)}`);
        d.mode = target.dataset.value;
        this.autoName();
        return this.refresh();
      case "choose": {
        const { field, value } = target.dataset;
        if (field === "trigger") d.trigger = TRIGGERS.find(t => t.key === value).make();
        else d[field] = value;
        return this.refresh();
      }
      case "unchip":
        d.excludes.splice(Number(target.dataset.index), 1);
        return this.refresh();
      case "focus-chips":
        if (event.target === target) Dom.$('[data-field="chip"]', this.element).focus();
        return;
      case "save": return this.save(target);
      case "delete": return this.remove();
    }
  }

  onInput(event) {
    const el = event.target;
    const d = this.draft;
    if (el.dataset.field === "name") {
      d.name = el.value;
      this.nameTouched = el.value.trim() !== "";
    } else if (el.dataset.field === "fullAutomation") {
      if (el.checked) {
        // Stays off until both warnings are accepted.
        el.checked = false;
        this.askFullAutomation();
      } else {
        d.fullAutomation = false;
        this.refresh();
      }
    } else if (el.dataset.field === "safeDelete") {
      if (el.checked) {
        d.safeDelete = true;
        this.refresh();
      } else {
        el.checked = true;
        this.askSafeDeletesOff();
      }
    } else if (el.dataset.trigger === "time") {
      const [hour, minute] = el.value.split(":").map(Number);
      Object.assign(d.trigger, { hour: hour || 0, minute: minute || 0 });
      this.updateTriggerSays();
    } else if (el.dataset.trigger) {
      d.trigger[el.dataset.trigger] = Math.max(1, Number(el.value) || 1);
      this.updateTriggerSays();
    }
  }

  updateTriggerSays() {
    const slot = Dom.$('[data-slot="trigger-says"]', this.element);
    if (slot) slot.textContent = this.triggerSays();
  }

  onKey(event) {
    const el = event.target;
    if (el.dataset.field === "chip" && (event.key === "Enter" || event.key === ",")) {
      event.preventDefault();
      const pattern = el.value.trim().replace(/,$/, "");
      if (pattern && !this.draft.excludes.includes(pattern)) this.draft.excludes.push(pattern);
      this.refresh();
    } else if (el.dataset.field === "chip" && event.key === "Backspace" && el.value === "" && this.draft.excludes.length) {
      this.draft.excludes.pop();
      this.refresh();
    } else if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      // ⌘/Ctrl+Enter saves, but never while a warning or another overlay is on top.
      event.preventDefault();
      if (this.asking || Overlay.anyModal() || Overlay.top() !== this) return;
      this.save(Dom.$('[data-do="save"]', this.element));
    }
  }

  /** Keeps the name in step with the folders until the person types their own. */
  autoName() {
    if (!this.nameTouched) this.draft.name = this.suggestedName();
  }

  async save(button) {
    if (this.asking || button.disabled) return;
    const pending = Dom.$('[data-field="chip"]', this.element)?.value.trim();
    if (pending && !this.draft.excludes.includes(pending)) this.draft.excludes.push(pending);
    button.disabled = true;
    try {
      const isNew = !this.editingId;
      const message = !isNew && this.changesWhatSyncs() ? "Saved. The next sync waits for your OK." : "Saved.";
      const view = isNew
        ? await Api.addJob(this.draft)
        : await Api.updateJob(this.editingId, this.draft);
      this.close();
      this.saved(view, { isNew, message });
    } catch (error) {
      this.toast.error(error);
    } finally {
      button.disabled = false;
    }
  }

  async remove() {
    const id = this.editingId;
    const sure = await Modal.confirm({
      title: `Delete “${this.draft.name}”?`,
      body: "The folders and files stay where they are. Only the job is removed.",
      action: "Delete job",
    });
    if (!sure) return;
    try {
      await Api.removeJob(id);
      this.close();
    } catch (error) {
      this.toast.error(error);
    }
  }
}
