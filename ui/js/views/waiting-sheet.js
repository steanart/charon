// "Waiting for your OK": the jobs whose changes wait for the person, after Sync all, from
// the tray, or before quitting. Every job's list is prepared at once; the person looks at
// each one and confirms it on its own. There is deliberately no "confirm all".

import { Api } from "../api.js";
import { Dom } from "../dom.js";
import { Format } from "../format.js";
import { Icons } from "../icons.js";
import { Overlay } from "./overlay.js";
import { Sheet } from "./sheet.js";

const { html, raw } = Dom;

const message = error => (typeof error === "string" ? error : error?.message ?? String(error));

export class WaitingSheet extends Sheet {
  constructor(store, review) {
    super(Dom.$("#waiting"));
    this.store = store;
    this.review = review;
    this.entries = [];   // one per job: {id, state, review, error, stale}
    this.started = 0;    // jobs Sync all started by themselves
    this.element.addEventListener("click", event => this.onClick(event));
    store.subscribe(hint => this.onStore(hint));
  }

  /** Opens with these jobs and starts looking at all of them at once. `over` keeps the sheets underneath open. */
  show(ids, { started = 0, over = false } = {}) {
    this.letGo();
    this.started = started;
    this.entries = ids.map(id => ({ id, state: "looking", review: null, error: null, stale: false }));
    this.render();
    this.open({ over });
    for (const entry of this.entries) this.look(entry);
  }

  close() {
    this.letGo();
    super.close();
  }

  /** Stops looks still running and lets go of lists nobody opened. */
  letGo() {
    for (const entry of this.entries) {
      if (entry.state === "looking") Api.cancelReview(entry.id).catch(() => {});
      if (entry.review) Api.discardReview(entry.review.jobId, entry.review.reviewId).catch(() => {});
      entry.review = null;
      entry.token = null; // answers still on their way are let go when they come
    }
    this.entries = [];
  }

  /** Prepares one job's list. */
  async look(entry) {
    const job = this.store.jobs.get(entry.id);
    if (job && (!job.sourceOnline || !job.destinationOnline)) {
      entry.state = "offline";
      return this.render();
    }
    const token = (entry.token = Symbol("look"));
    Object.assign(entry, { state: "looking", review: null, error: null, stale: false });
    this.store.clearScan(entry.id);
    this.render();
    try {
      const review = await Api.prepareReview(entry.id);
      if (entry.token !== token) return Api.discardReview(review.jobId, review.reviewId).catch(() => {});
      // An empty list is let go, unless Rust says why it's worth confirming anyway.
      if (!this.review.canConfirm(review)) {
        entry.state = "nothing";
        Api.discardReview(review.jobId, review.reviewId).catch(() => {});
      } else {
        entry.review = review;
        entry.state = review.blocked ? "blocked" : "ready";
      }
    } catch (error) {
      if (entry.token !== token) return;
      const now = this.store.jobs.get(entry.id);
      entry.state = now && (!now.sourceOnline || !now.destinationOnline) ? "offline" : "failed";
      entry.error = message(error);
    }
    this.render();
  }

  /** Hands the prepared list to the review dialog, and notes how it ended. */
  async openReview(entry) {
    const review = entry.review;
    if (!review) return;
    entry.review = null;
    entry.state = "reviewing";
    this.render();
    const outcome = await this.review.showReview(review);
    if (!this.entries.includes(entry)) return;
    entry.state = outcome === "confirmed" ? "confirmed" : "not-confirmed";
    this.render();
    // The button that opened the review is gone: the focus goes to this job's new one, or the next "Review…".
    if (this.isOpen && Overlay.top() === this && document.activeElement === this.element) {
      const again = Dom.$(`.wait-item[data-id="${CSS.escape(entry.id)}"] button`, this.element) ?? Dom.$('[data-do="review"]', this.element);
      again?.focus({ preventScroll: true });
    }
  }

  /** Rust says a list went out of date before anyone looked at it. */
  markStale({ reviewId }) {
    const entry = this.entries.find(e => e.review?.reviewId === reviewId);
    if (!entry) return;
    entry.stale = true;
    this.render();
  }

  // ---------------------------------------------------------------- drawing

  render() {
    const n = this.entries.length;
    const started = this.started === 1
      ? "1 job that runs by itself is on its way. "
      : this.started > 1 ? `${this.started} jobs that run by themselves are on their way. ` : "";
    this.redraw(() => (this.element.innerHTML = html`
      <header>
        <div><span class="eyebrow"><span class="num">${String(n).padStart(2, "0")}</span> ${n === 1 ? "Job" : "Jobs"}</span><h2>Waiting for your OK</h2></div>
        <button class="icon-btn" data-do="close" title="Close"><span class="ico">${raw(Icons.close)}</span></button>
      </header>
      <div class="scroll">
        <p class="sheet-intro">${started}${n === 1 ? "This job waits" : "These jobs wait"} for your OK. Look at each list and confirm the ones you agree with; nothing changes until you do.</p>
        <div class="waiting-list">${this.entries.map(entry => raw(this.entryHtml(entry)))}</div>
      </div>
      <footer>
        <span class="note">Each job is confirmed on its own.</span>
        <span class="spacer"></span>
        <button class="btn ghost" data-do="close">Close</button>
      </footer>`));
  }

  entryHtml(entry) {
    const job = this.store.jobs.get(entry.id);
    const name = job?.name ?? entry.review?.jobName ?? "A deleted job";
    const { text, tone, action, extra = "" } = this.describe(entry, job);
    return html`<div class="wait-item" data-id="${entry.id}" data-tone="${tone}">
        <div class="what">
          <span class="name">${name}</span>
          ${raw(job ? html`<span class="where"><bdi>${Format.path(job.source)}</bdi> ${Format.arrow(job.mode)} <bdi>${Format.path(job.destination)}</bdi></span>` : "")}
          <span class="state"><i></i><span>${text}</span></span>
          ${raw(extra)}
        </div>
        ${raw(action ? html`<button class="btn ${action.primary ? "primary" : "ghost"}" data-do="${action.do}" data-id="${entry.id}">${action.label}</button>` : "")}
      </div>`;
  }

  /** The line under a job's name, its colour, its button, and anything else worth a line. */
  describe(entry, job) {
    const review = entry.review;
    switch (entry.state) {
      case "looking":
        return { text: "Looking…", tone: "busy", extra: html`<span class="counts" data-counts="${entry.id}">${this.countsText(entry.id)}</span>` };
      case "ready":
      case "blocked": {
        if (entry.stale) return { text: "Things changed since this list was made.", tone: "caution", action: { do: "again", label: "Look again" } };
        const warnings = review.warnings.filter(w => w.level !== "info").length; // info lines aren't warnings
        const danger = review.warnings.some(w => w.level === "danger");
        const extra = warnings
          ? html`<span class="warnings ${danger ? "danger" : ""}"><span class="ico">${raw(Icons.warn)}</span>${Format.plural(warnings, "warning")}</span>`
          : "";
        if (entry.state === "blocked") {
          return { text: `Blocked: ${review.blocked}`, tone: "danger", extra, action: { do: "review", label: "Review…" } };
        }
        const changes = review.totalSteps
          ? Format.changeList(review.tally) || Format.plural(review.totalSteps, "step")
          : `nothing to copy. ${review.confirmAnyway}`;
        return { text: `Ready: ${changes}`, tone: "ready", extra, action: { do: "review", label: "Review…", primary: true } };
      }
      case "nothing": return { text: "Nothing to do", tone: "calm" };
      case "offline": return { text: "Offline", tone: "faint", extra: job?.problem ? html`<span class="why">${job.problem}</span>` : "", action: { do: "again", label: "Try again" } };
      case "failed": return { text: `Couldn't look: ${entry.error}`, tone: "danger", action: { do: "again", label: "Try again" } };
      case "reviewing": return { text: "In review…", tone: "busy" };
      case "confirmed": return { text: "Confirmed", tone: "calm" };
      case "not-confirmed": return { text: "Not confirmed", tone: "faint", action: { do: "again", label: "Look again" } };
      default: return { text: "", tone: "faint" };
    }
  }

  countsText(id) {
    const compare = this.store.compares.get(id);
    if (compare?.totalFiles) return `comparing ${Format.count(compare.files)} of ${Format.count(compare.totalFiles)} files by contents`;
    const scan = this.store.scans.get(id);
    if (!scan) return "Counting…";
    return `source: ${Format.plural(scan.source, "item")} · destination: ${Format.plural(scan.destination, "item")}`;
  }

  // ---------------------------------------------------------------- input

  onClick(event) {
    const target = event.target.closest("[data-do]");
    if (!target) return;
    const entry = this.entries.find(e => e.id === target.dataset.id);
    switch (target.dataset.do) {
      case "close": return this.close();
      case "review": return entry && this.openReview(entry);
      case "again":
        if (!entry) return;
        if (entry.review) Api.discardReview(entry.review.jobId, entry.review.reviewId).catch(() => {});
        entry.review = null;
        return this.look(entry);
    }
  }

  onStore(hint) {
    if (!this.isOpen || hint.overviewOnly || hint.progressOnly) return;
    if (hint.scanOnly) {
      const slot = this.element.querySelector(`[data-counts="${CSS.escape(hint.id)}"]`);
      if (slot) slot.textContent = this.countsText(hint.id);
      return;
    }
    if (!hint.id || this.entries.some(e => e.id === hint.id)) this.render();
  }
}
