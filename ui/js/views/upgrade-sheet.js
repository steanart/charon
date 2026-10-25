// The first start of Charon 0.3.0: jobs that used to sync by themselves now prepare the list
// of changes and wait for an OK. This says so, job by job, and offers full automation back
// (behind its two warnings) to anyone who wants it. A job with full automation on still waits
// for one confirmed list before it syncs by itself; the sheet says so and offers that list.

import { Api } from "../api.js";
import { Dom } from "../dom.js";
import { Format } from "../format.js";
import { Icons } from "../icons.js";
import { Sheet } from "./sheet.js";

const { html, raw } = Dom;

export class UpgradeSheet extends Sheet {
  constructor(store, toast, { automate, review }) {
    super(Dom.$("#upgrade"));
    this.store = store;
    this.toast = toast;
    this.automate = automate;
    this.review = review;
    this.notice = null;
    this.kept = new Set();   // jobs the person chose to keep asking
    this.element.addEventListener("click", event => this.onClick(event));
    store.subscribe(hint => {
      if (this.isOpen && !hint.progressOnly && !hint.scanOnly && !hint.overviewOnly) this.render();
    });
  }

  /** Shows the notice if Rust has one (only on the first start of a new version). */
  async start() {
    const notice = await Api.upgradeNotice().catch(() => null);
    if (!notice?.jobs?.length) return;
    this.notice = notice;
    this.render();
    this.open();
  }

  render() {
    const n = this.notice;
    if (!n) return;
    this.redraw(() => (this.element.innerHTML = html`
      <header>
        <div><span class="eyebrow"><span class="num">${n.version}</span> New in this version</span><h2>What changed in Charon ${n.version}</h2></div>
        <button class="icon-btn" data-do="close" title="Close"><span class="ico">${raw(Icons.close)}</span></button>
      </header>
      <div class="scroll">
        <p class="upgrade-lead"><span class="ico">${raw(Icons.review)}</span>
          <span>Charon no longer changes your files by itself unless you allow it. Each job now prepares the list of changes and waits for your OK.</span></p>
        <p class="upgrade-lead"><span class="ico">${raw(Icons.info)}</span>
          <span>Symbolic links are now copied as links. Charon 0.2 left them out, so a job's first list may include them. Drives that can't hold links (FAT, exFAT, some network shares) leave them out, and the list says so.</span></p>
        <div class="upgrade-jobs">${n.jobs.map(item => raw(this.jobHtml(item)))}</div>
      </div>
      <footer>
        <span class="spacer"></span>
        <button class="btn primary" data-do="done">Got it</button>
      </footer>`));
  }

  jobHtml(item) {
    const job = this.store.jobs.get(item.id);
    const name = job?.name ?? item.name;
    const before = Format.triggerPhrase(item.trigger, { past: true }) ?? "when you pressed Sync now";
    let choice;
    if (!job) {
      choice = '<span class="upgrade-state">This job has been deleted.</span>';
    } else if (job.runsByItself) {
      choice = '<span class="upgrade-state auto">Full automation is on: it syncs by itself again.</span>';
    } else if (job.fullAutomation) {
      choice = html`<span class="upgrade-state auto">Full automation is on. Confirm its next list once, and it syncs by itself from then on.</span>
        <button class="btn primary small" data-do="review" data-id="${item.id}">Review and sync</button>`;
    } else if (this.kept.has(item.id)) {
      choice = html`<span class="upgrade-state calm">It keeps asking you.</span>
        <button class="btn ghost small" data-do="automate" data-id="${item.id}">Turn on full automation…</button>`;
    } else {
      choice = html`<button class="btn primary small" data-do="keep" data-id="${item.id}">Keep asking (recommended)</button>
        <button class="btn ghost small" data-do="automate" data-id="${item.id}">Turn on full automation…</button>`;
    }
    return html`<div class="upgrade-job">
        <p>“${name}” used to sync ${before}. Now it looks and asks you.</p>
        <div class="choices">${raw(choice)}</div>
      </div>`;
  }

  async onClick(event) {
    const target = event.target.closest("[data-do]");
    if (!target) return;
    const id = target.dataset.id;
    switch (target.dataset.do) {
      case "close": return this.close();
      case "keep":
        this.kept.add(id);
        return this.render();
      case "automate": return this.automate(id);
      case "review": return this.review(id);
      case "done":
        target.disabled = true;
        try {
          await Api.dismissUpgradeNotice();
          this.close();
        } catch (error) {
          target.disabled = false;
          this.toast.error(error);
        }
    }
  }
}
