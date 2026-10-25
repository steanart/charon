// The review: the list of changes a sync would make, prepared by Rust before anything is
// touched. Nothing changes until the person confirms it, and the Confirm button is never
// bound to Enter. The same dialog shows what settling one conflict would do.
//
// A review lives for 15 minutes. When it's older, when Rust says the folders changed since
// it was made, or when Rust refuses to start it, the Confirm button turns into "Look again".

import { Api } from "../api.js";
import { Dom } from "../dom.js";
import { Format } from "../format.js";
import { Icons } from "../icons.js";
import { Overlay } from "./overlay.js";

const { html, raw } = Dom;

const PAGE = 200;          // steps shown at a time
const NOTES_PAGE = 100;    // notes shown at a time
const COPY_PAGE = 500;     // steps fetched at a time for "Copy list"

const FILTERS = [
  { key: "all", label: "All" },
  { key: "removals", label: "Removals" },
  { key: "replacements", label: "Replacements" },
  { key: "conflicts", label: "Conflicts" },
  { key: "new", label: "New" },
];

/** What each kind of step is called in a copied list. */
const STEP_VERBS = {
  new: "copy",
  replace: "replace",
  remove: "remove",
  folder: "new folder",
  "keep-both": "keep both",
  conflict: "left for you",
  settle: "settle",
};

/** The little label next to each note. */
const NOTE_KINDS = {
  unreadable: "Couldn't read",
  "left-out": "Left out",
  "name-clash": "Name clash",
  "kept-folder": "Folder kept",
  blocked: "Blocked",
};

/** What can't be pressed while a confirmed list is being started. */
const WAIT_FOR_APPROVE = ["confirm", "look-again", "leave-out"];

const CHANGED = "Things changed since this list was made.";
const JOB_CHANGED = "The job changed. Look again.";

const message = error => (typeof error === "string" ? error : error?.message ?? String(error));

export class ReviewDialog extends Overlay {
  constructor(store, toast, { openConflicts }) {
    super(Dom.$("#review"));
    this.store = store;
    this.toast = toast;
    this.openConflicts = openConflicts;
    this.state = null;               // what the dialog shows; null while closed
    this.staleReviews = new Set();   // review ids Rust said are out of date
    this.listToken = 0;              // the latest list request; older answers are dropped
    this.searchTimer = null;
    this.element.addEventListener("click", event => this.onClick(event));
    this.element.addEventListener("input", event => this.onInput(event));
    this.element.addEventListener("change", event => this.onChange(event));
    this.element.addEventListener("keydown", event => this.onKey(event));
    this.element.addEventListener("toggle", event => this.onToggle(event), true);
    store.subscribe(hint => this.onStore(hint));
    setInterval(() => this.tick(), 5000);
  }

  // ---------------------------------------------------------------- opening and closing

  /**
   * Looks at a job's folders and shows what a sync would do. Resolves with "confirmed" or
   * "not-confirmed" once the dialog closes.
   */
  show(jobId) {
    return this.begin({ jobId, kind: "sync" }, () => Api.prepareReview(jobId));
  }

  /** Shows a review prepared elsewhere (the waiting sheet). */
  showReview(review) {
    const outcome = this.begin({ jobId: review.jobId, kind: review.kind });
    this.receive(review);
    return outcome;
  }

  /** Shows what settling one conflict would do. Nothing changes until it's confirmed. */
  settle(jobId, conflict, choice) {
    return this.begin({ jobId, kind: "resolve", conflict }, () => Api.prepareResolution(jobId, conflict, choice));
  }

  begin(what, prepare = null) {
    if (this.state) this.finish("not-confirmed"); // one review at a time
    let done;
    const outcome = new Promise(resolve => (done = resolve));
    this.state = { ...what, done, phase: "preparing", review: null };
    this.render();
    this.open();
    if (prepare) this.prepare(prepare);
    return outcome;
  }

  /** Asks Rust for the list; meanwhile the dialog counts what it finds on both shores. */
  async prepare(work) {
    const state = this.state;
    Object.assign(state, { phase: "preparing", review: null, error: null });
    this.store.clearScan(state.jobId);
    this.render();
    try {
      const review = await work();
      if (this.state !== state) return Api.discardReview(review.jobId, review.reviewId).catch(() => {});
      this.receive(review);
    } catch (error) {
      if (this.state !== state) return; // closed meanwhile, for example with Cancel
      state.phase = "failed";
      state.error = message(error);
      this.render();
    }
  }

  /** A review arrived: show it from the top of its list. */
  receive(review) {
    Object.assign(this.state, {
      phase: "ready",
      review,
      error: null,
      refused: null,
      changed: this.staleReviews.has(review.reviewId) ? CHANGED : null,
      expiryShown: false,
      ticked: false,
      filter: "all",
      query: "",
      cluster: null,
      steps: review.steps ?? [],
      total: review.totalSteps,
      loading: false,
      listError: null,
      notes: review.notes ?? [],
      notesTotal: review.totalNotes,
      notesOpen: false,
    });
    this.render();
  }

  close() {
    this.finish("not-confirmed");
  }

  /** Esc and a click outside close the dialog, except while a confirmed sync is being started. */
  dismiss() {
    if (this.state?.phase !== "approving") this.close();
  }

  /** Closes the dialog. A review that wasn't confirmed is let go; a look still running is stopped. */
  finish(outcome) {
    const s = this.state;
    this.state = null;
    clearTimeout(this.searchTimer);
    if (s && outcome !== "confirmed") {
      // A conflict's review can't be stopped; if it arrives after closing, prepare() lets it go.
      if (s.phase === "preparing" && s.kind === "sync") Api.cancelReview(s.jobId).catch(() => {});
      else if (s.review) Api.discardReview(s.review.jobId, s.review.reviewId).catch(() => {});
    }
    super.close();
    s?.done(outcome);
  }

  /** Rust says the folders changed after this list was made. */
  markStale({ reviewId }) {
    this.staleReviews.add(reviewId);
    const s = this.state;
    if (s?.review?.reviewId === reviewId && !s.changed) {
      s.changed = CHANGED;
      this.renderAlert();
      this.renderFooter();
    }
  }

  // ---------------------------------------------------------------- what the dialog knows

  expired() {
    const review = this.state?.review;
    return Boolean(review) && Date.now() > Date.parse(review.expiresAt);
  }

  /** The list can't be confirmed as it is: it's too old, out of date, or Rust refused it. */
  needsFreshLook() {
    const s = this.state;
    return Boolean(s.refused || s.changed || this.expired());
  }

  // ---------------------------------------------------------------- drawing

  render() {
    const s = this.state;
    if (!s) return;
    const job = this.store.jobs.get(s.jobId);
    const name = s.review?.jobName ?? job?.name ?? "";
    const resolve = s.kind === "resolve";
    const title = resolve ? s.review?.title ?? `Settle “${s.conflict?.path ?? ""}”` : name;
    this.element.dataset.phase = s.phase;
    this.element.innerHTML = html`
      <header>
        <div class="review-eyebrow">
          <span class="eyebrow"><span class="num">${resolve ? "Settle a conflict" : "Waiting for your OK"}</span></span>
          <span class="calm">· nothing changes until you confirm</span>
        </div>
        <h2>${title}</h2>
        ${raw(resolve && name ? html`<p>${name}</p>` : "")}
      </header>
      ${raw(this.bodyHtml())}
      ${raw(this.tickHtml())}
      <footer></footer>`;
    this.renderFooter();
    this.renderSteps();
    this.focus();
  }

  bodyHtml() {
    const s = this.state;
    if (s.phase === "preparing") {
      return html`<div class="review-body"><div class="scanning">
          <div class="radar"></div>
          <span>Looking at both shores…</span>
          <span class="counts" data-slot="counts">${this.countsText()}</span>
        </div></div>`;
    }
    if (s.phase === "failed") {
      return html`<div class="review-body">${raw(notice("danger", s.error, "Charon couldn't make the list."))}</div>`;
    }
    const review = s.review;
    const nothing = review.totalSteps === 0;
    return html`<div class="review-body">
        ${raw(this.shoresHtml(review))}
        <div data-slot="alert">${raw(this.alertHtml())}</div>
        ${raw(review.blocked ? notice("danger", review.blocked, "This list can't be confirmed.") : "")}
        ${raw(review.warnings.map(w => notice(w.level, w.text)).join(""))}
        ${raw(nothing ? "" : this.tallyHtml(review))}
        ${raw(review.housekeeping.length ? html`<ul class="r-house">${review.housekeeping.map(line => raw(html`<li>${line}</li>`))}</ul>` : "")}
        ${raw(this.clustersHtml(review))}
        ${raw(this.notesHtml())}
        ${raw(nothing ? this.settledHtml(review) : this.listHtml())}
      </div>`;
  }

  /**
   * Nothing to do. Rust says when the empty list is still worth confirming, and why (the job
   * waits for that OK, or the folders are taken as they are now); then it can be confirmed.
   */
  settledHtml(review) {
    const why = review.confirmAnyway ? html`<p class="r-settled-why">${review.confirmAnyway}</p>` : "";
    return `<div class="r-settled">Both shores already match.${why}</div>`;
  }

  /** Whether a list can be confirmed at all: it has steps, or Rust says why to confirm it anyway. */
  canConfirm(review) {
    return review.totalSteps > 0 || Boolean(review.confirmAnyway);
  }

  /** Both folders, with their drives and file counts, and what the job does with removed files. */
  shoresHtml(review) {
    const shore = (label, s) => html`<div class="r-shore">
        <span class="k">${label}</span>
        <span class="path" title="${s.path}"><bdi>${Format.path(s.path)}</bdi></span>
        <span class="meta">${[s.volume ? `on ${Format.volume(s.volume)}` : "", Format.plural(s.files, "file")].filter(Boolean).join(" · ")}</span>
      </div>`;
    const safety = review.safeDelete
      ? "Removed and replaced files go to .charon-trash."
      : review.mode === "two-way"
        ? "Safe deletes are off: removed files are gone at once (replaced versions still go to .charon-trash)."
        : "Safe deletes are off: removed and replaced files are gone at once.";
    return html`<div class="r-shores">
        ${raw(shore("Source", review.source))}
        ${raw(shore("Destination", review.destination))}
        <div class="r-mode">
          <span class="tag">${Format.modeName(review.mode)}</span>
          ${raw(review.fullAutomation ? '<span class="tag auto">Full auto</span>' : "")}
          <span class="${review.safeDelete ? "" : "bad"}">${safety}</span>
        </div>
      </div>`;
  }

  /** Why the list must be looked at again, if it must. */
  alertHtml() {
    const s = this.state;
    if (s.refused) return notice("danger", s.refused, "Nothing was changed.");
    if (s.changed) return notice("caution", s.changed === CHANGED ? `${CHANGED} Look again to see what's there now.` : s.changed);
    if (this.expired()) return notice("caution", "This list is more than 15 minutes old, so it can't be confirmed any more. Look again to see what's there now.");
    return "";
  }

  /** What happens on each side, then the rest: kept versions, conflicts, files that agree. */
  tallyHtml(review) {
    const t = review.tally;
    const twoWay = review.mode === "two-way";
    const side = (label, st, originals) => {
      // In two-way mode the source holds the person's originals: its losses are shown in red.
      const hard = originals ? "bad" : "hard";
      const parts = [];
      if (st.newFiles) parts.push(Dom.escape(`${Format.count(st.newFiles)} new`));
      if (st.replaced) parts.push(html`<b class="${hard}">${Format.count(st.replaced)} replaced</b>`);
      if (st.removed) parts.push(html`<b class="${hard}">${Format.count(st.removed)} removed</b>`);
      if (st.newFolders) parts.push(Dom.escape(Format.plural(st.newFolders, "new folder")));
      if (st.removedFolders) parts.push(html`<b class="${hard}">${Format.plural(st.removedFolders, "folder")} removed</b>`);
      const what = parts.length ? parts.join(", ") : '<span class="faint">nothing changes</span>';
      const comes = st.bytesIn ? html` <span class="faint">· ${Format.bytes(st.bytesIn)} comes in</span>` : "";
      return `<div class="r-side"><span class="k">${Dom.escape(label)}</span><span class="v">${what}${comes}</span></div>`;
    };
    const extras = [];
    if (t.keepBoth) extras.push(`${Format.plural(t.keepBoth, "file")} changed on both sides: both versions are kept`);
    if (t.conflicts) extras.push(`${Format.plural(t.conflicts, "conflict")} left for you: settle ${t.conflicts === 1 ? "it" : "them"} in the job's Conflicts tab`);
    if (t.newestWins) extras.push(`${Format.plural(t.newestWins, "conflict")} settled by newest wins`);
    if (t.agreed) extras.push(`${Format.plural(t.agreed, "file")} ${t.agreed === 1 ? "is the same on both sides apart from its dates" : "are the same on both sides apart from their dates"}: treated as in sync`);
    if (t.bytes) extras.push(`${Format.bytes(t.bytes)} to copy in all`);
    return html`<div class="r-tally ${twoWay ? "two-way" : ""}">
        ${raw(side("On the source:", t.source, twoWay))}
        ${raw(side("On the destination:", t.destination, false))}
        ${raw(extras.length ? html`<ul class="r-extras">${extras.map(line => raw(html`<li>${line}</li>`))}</ul>` : "")}
      </div>`;
  }

  /** Whole folders removed, biggest first. Each one filters the list to what goes with it. */
  clustersHtml(review) {
    if (!review.clusters.length) return "";
    const s = this.state;
    return html`<section class="r-clusters">
        <h3 class="r-h">Removed folders</h3>
        ${review.clusters.map((c, i) => raw(html`
          <button class="r-cluster ${s.cluster === i ? "on" : ""}" data-do="cluster" data-index="${i}" aria-pressed="${s.cluster === i}"
            title="${s.cluster === i ? "Show the whole list again" : "Show what is removed in this folder"}">
            <span class="ico">${raw(Icons.folder)}</span>
            <span class="p"><bdi>${c.path}</bdi></span>
            <span class="m">on the ${c.side}: ${Format.plural(c.items, "item")}, ${Format.bytes(c.bytes)}</span>
          </button>`))}
      </section>`;
  }

  listHtml() {
    const s = this.state;
    return html`<section class="r-list">
        <div class="r-toolbar">
          <div class="segmented r-filters" role="group" aria-label="Show">${FILTERS.map(f => raw(html`
            <button data-do="filter" data-value="${f.key}" class="${s.filter === f.key ? "on" : ""}" aria-pressed="${s.filter === f.key}">${f.label}</button>`))}</div>
          <label class="r-search">
            <span class="ico">${raw(Icons.search)}</span>
            <input data-field="query" value="${s.query}" placeholder="Find a path" spellcheck="false" autocomplete="off" aria-label="Find a path">
          </label>
        </div>
        <div class="r-count" aria-live="polite"></div>
        <div class="r-steps"></div>
        <div class="r-more"></div>
      </section>`;
  }

  /** Draws the list of steps. With `append`, only the steps added by "Show more". */
  renderSteps({ append = false } = {}) {
    const s = this.state;
    const box = Dom.$(".r-steps", this.element);
    if (!s || !box) return;
    const filtered = s.filter !== "all" || s.query || s.cluster !== null;
    const cluster = s.cluster === null ? null : s.review.clusters[s.cluster];
    const where = cluster ? ` in ${cluster.path}` : "";
    Dom.$(".r-count", this.element).textContent = s.loading && !append
      ? "Looking…"
      : `${Format.plural(s.total, "step")}${filtered ? " match" : ""}${where}${s.steps.length < s.total ? ` · showing ${Format.count(s.steps.length)}` : ""}`;
    if (s.listError) {
      box.innerHTML = notice("danger", s.listError);
    } else if (!s.steps.length) {
      box.innerHTML = `<div class="r-empty">${s.loading ? "Looking…" : "Nothing here."}</div>`;
    } else if (append) {
      const shown = box.querySelectorAll(".r-step").length;
      box.insertAdjacentHTML("beforeend", s.steps.slice(shown).map(step => this.stepHtml(step)).join(""));
    } else {
      box.innerHTML = s.steps.map(step => this.stepHtml(step)).join("");
    }
    const left = s.total - s.steps.length;
    Dom.$(".r-more", this.element).innerHTML = left > 0 && !s.listError
      ? html`<button class="btn ghost" data-do="more" ${s.loading ? "disabled" : ""}>${s.loading ? "Loading…" : `Show more (${Format.count(left)} left)`}</button>`
      : "";
  }

  stepHtml(step) {
    const size = step.item === "dir" ? "folder" : step.item === "link" ? "link" : Format.bytes(step.bytes);
    const side = step.side ?? "both";
    return html`<div class="r-step ${step.kind} ${step.danger ? "danger" : ""}">
        <span class="ico sym">${raw(Icons.step[step.kind] ?? Icons.step.new)}</span>
        <span class="what"><span class="p"><bdi>${step.path}</bdi></span><small>${step.reason}</small></span>
        <span class="side ${side}">${Format.side(step.side)}</span>
        <span class="size">${size}</span>
      </div>`;
  }

  /** Places left out or not read, and other things worth knowing. Folded until opened. */
  notesHtml() {
    const s = this.state;
    if (!s.notesTotal) return "";
    const left = s.notesTotal - s.notes.length;
    return html`<details class="r-notes" ${s.notesOpen ? "open" : ""}>
        <summary><span class="ico">${raw(Icons.info)}</span><span>Notes</span><span class="badge">${Format.count(s.notesTotal)}</span>
          <small>Places left out or not read, and other things to know</small></summary>
        <div class="r-notes-list">${s.notes.map((note, i) => raw(this.noteHtml(note, i)))}</div>
        ${raw(left > 0 ? html`<button class="btn ghost" data-do="more-notes">Show more (${Format.count(left)} left)</button>` : "")}
      </details>`;
  }

  noteHtml(note, index) {
    const where = note.side ? `On the ${note.side} · ` : "";
    return html`<div class="r-note ${note.kind}">
        <span class="kind">${NOTE_KINDS[note.kind] ?? note.kind}</span>
        <span class="what"><span class="p"><bdi>${note.path}</bdi></span><small>${where}${note.reason}</small></span>
        ${raw(note.canLeaveOut ? html`<button class="btn small" data-do="leave-out" data-index="${index}" ${note.leftOut ? "disabled" : ""}>${note.leftOut ? "Left out" : "Leave out"}</button>` : "")}
      </div>`;
  }

  /**
   * "I've looked at the removals and replacements" when the review asks for it, or for an
   * empty list that can be confirmed, "I've read the warnings above".
   */
  tickHtml() {
    const s = this.state;
    const review = s.review;
    if (s.phase !== "ready" && s.phase !== "approving") return "";
    if (!review.needsTick || review.blocked || !this.canConfirm(review)) return "";
    const what = review.totalSteps > 0 ? "I've looked at the removals and replacements" : "I've read the warnings above";
    return html`<label class="r-tick"><input type="checkbox" data-field="tick" ${s.ticked ? "checked" : ""}>
        <span>${what}</span></label>`;
  }

  renderAlert() {
    const slot = Dom.$('[data-slot="alert"]', this.element);
    if (slot) slot.innerHTML = this.alertHtml();
  }

  /** The footer: when the list was made, Copy list, Close, and Confirm (or Look again). */
  renderFooter() {
    const s = this.state;
    const footer = Dom.$("footer", this.element);
    if (!s || !footer) return;
    const focused = this.element.contains(document.activeElement) ? document.activeElement.dataset.do : null;

    if (s.phase === "preparing") {
      footer.innerHTML = '<span class="spacer"></span><button class="btn ghost" data-do="close" data-autofocus>Cancel</button>';
    } else if (s.phase === "failed") {
      footer.innerHTML = html`<span class="spacer"></span>
        <button class="btn ghost" data-do="close" data-autofocus>Close</button>
        <button class="btn primary" data-do="look-again">Look again</button>`;
    } else {
      const review = s.review;
      const approving = s.phase === "approving";
      const destructive = Format.isDestructive(review.tally);
      const kind = destructive ? "danger-solid" : "primary";
      let confirm = "";
      if (this.canConfirm(review)) {
        if (approving) {
          // Rust is starting this list: nothing here can swap it for another until it answers.
          confirm = html`<button class="btn ${kind} confirm" data-do="confirm" disabled>Starting…</button>`;
        } else if (this.needsFreshLook()) {
          confirm = html`<button class="btn primary" data-do="look-again">Look again</button>`;
        } else {
          const label = destructive ? `Confirm: ${Format.changeList(review.tally)}` : "Confirm";
          const ready = !review.blocked && (!review.needsTick || s.ticked);
          confirm = html`<button class="btn ${kind} confirm" data-do="confirm" ${ready ? "" : "disabled"}>${label}</button>`;
        }
      }
      footer.innerHTML = html`
        <span class="note" data-slot="prepared">${this.preparedText()}</span>
        ${raw(review.totalSteps > 0 ? html`<button class="btn ghost" data-do="copy"><span class="ico">${raw(Icons.copy)}</span>Copy list</button>` : "")}
        <span class="spacer"></span>
        <button class="btn ghost" data-do="close" data-autofocus ${approving ? "disabled" : ""}>Close</button>
        ${raw(confirm)}`;
    }
    // Keep the focus where it was, so redrawing the footer doesn't throw it out of the dialog.
    const again = focused && Dom.$(`footer [data-do="${focused}"]:not([disabled])`, this.element);
    if (again) again.focus({ preventScroll: true });
    else if (!this.element.contains(document.activeElement)) this.focus();
  }

  preparedText() {
    return `Prepared ${Format.ago(this.state.review.preparedAt)}`;
  }

  /** What the look has counted so far; after both scans, the files it compares by contents. */
  countsText() {
    const compare = this.store.compares.get(this.state.jobId);
    if (compare?.totalFiles) {
      return `Comparing files that differ only by date: ${Format.count(compare.files)} of ${Format.count(compare.totalFiles)} (${Format.bytes(compare.bytes)} of ${Format.bytes(compare.totalBytes)})`;
    }
    const scan = this.store.scans.get(this.state.jobId);
    if (!scan) return "Counting…";
    return `source: ${Format.plural(scan.source, "item")} · destination: ${Format.plural(scan.destination, "item")}`;
  }

  /** Keeps "Prepared 3 min ago" fresh, and notices when the list gets too old. */
  tick() {
    const s = this.state;
    if (!s?.review || s.phase !== "ready") return;
    const slot = Dom.$('[data-slot="prepared"]', this.element);
    if (slot) slot.textContent = this.preparedText();
    if (this.expired() && !s.expiryShown) {
      s.expiryShown = true;
      this.renderAlert();
      this.renderFooter();
    }
  }

  // ---------------------------------------------------------------- the list

  /** Fetches the first page for the current filter and search, or the next page with `more`. */
  async loadSteps({ more = false } = {}) {
    const s = this.state;
    const review = s.review;
    const token = ++this.listToken;
    s.loading = true;
    this.renderSteps({ append: more });
    try {
      const folder = s.cluster === null ? null : review.clusters[s.cluster]?.path ?? null;
      const page = await Api.reviewSteps(review.jobId, review.reviewId, s.filter, s.query, more ? s.steps.length : 0, PAGE, folder);
      if (token !== this.listToken || this.state !== s) return;
      s.steps = more ? s.steps.concat(page.steps) : page.steps;
      s.total = page.total;
      s.listError = null;
    } catch (error) {
      if (token !== this.listToken || this.state !== s) return;
      s.listError = message(error);
    }
    s.loading = false;
    this.renderSteps({ append: more && !s.listError });
  }

  setFilter(filter) {
    const s = this.state;
    s.filter = filter;
    s.cluster = null;
    this.markChoices();
    this.loadSteps();
  }

  /** A removed folder: shows only the removals in it, or the whole list again. */
  toggleCluster(index) {
    const s = this.state;
    const off = s.cluster === index;
    s.cluster = off ? null : index;
    s.filter = off ? "all" : "removals";
    s.query = "";
    const input = Dom.$('[data-field="query"]', this.element);
    if (input) input.value = "";
    this.markChoices();
    this.loadSteps();
    if (!off) Dom.$(".r-list", this.element)?.scrollIntoView({ block: "start", behavior: "smooth" });
  }

  /** Marks the chosen filter and folder without redrawing the rest. */
  markChoices() {
    const s = this.state;
    for (const button of Dom.$$('[data-do="filter"]', this.element)) {
      const on = button.dataset.value === s.filter;
      button.classList.toggle("on", on);
      button.setAttribute("aria-pressed", on);
    }
    for (const button of Dom.$$('[data-do="cluster"]', this.element)) {
      const on = Number(button.dataset.index) === s.cluster;
      button.classList.toggle("on", on);
      button.setAttribute("aria-pressed", on);
    }
  }

  async moreNotes(button) {
    const s = this.state;
    const review = s.review;
    button.disabled = true;
    try {
      const page = await Api.reviewNotes(review.jobId, review.reviewId, s.notes.length, NOTES_PAGE);
      if (this.state !== s) return;
      s.notes = s.notes.concat(page.notes);
      s.notesTotal = page.total;
      s.notesOpen = true;
      this.redraw(() => this.render());
    } catch (error) {
      button.disabled = false;
      this.toast.error(error);
    }
  }

  /** Copies every step of the list (all pages) as plain text lines, one step per line. */
  async copyList(button) {
    const s = this.state;
    const review = s.review;
    button.disabled = true;
    try {
      const lines = [];
      let total = Infinity;
      while (lines.length < total) {
        const page = await Api.reviewSteps(review.jobId, review.reviewId, "all", "", lines.length, COPY_PAGE);
        total = page.total;
        if (!page.steps.length) break;
        lines.push(...page.steps.map(stepLine));
      }
      await copyText(lines.join("\n"));
      this.toast.show(`Copied the list: ${Format.plural(lines.length, "line")}.`);
    } catch (error) {
      this.toast.error(error);
    } finally {
      button.disabled = false;
    }
  }

  // ---------------------------------------------------------------- acting

  /**
   * Starts the sync of exactly this list, or, while another sync works in the job's folders,
   * leaves it to start by itself once that sync is done. A refusal is shown here, never after
   * closing.
   */
  async approve() {
    const s = this.state;
    const review = s.review;
    s.phase = "approving";
    this.renderFooter();
    try {
      const approval = await Api.approve(review.jobId, review.reviewId, Boolean(s.ticked));
      if (this.state !== s) return;
      this.finish("confirmed");
      this.toast.show(approval?.kind === "waiting"
        ? `Confirmed. “${review.jobName}” syncs as soon as “${approval.waitingFor}” is done.`
        : `Syncing “${review.jobName}”…`);
    } catch (error) {
      if (this.state !== s || s.review !== review) return;
      s.phase = "ready";
      s.refused = message(error);
      this.renderAlert();
      this.renderFooter();
      Dom.$('[data-slot="alert"]', this.element)?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  }

  /** Prepares a fresh list. A conflict is looked at again in its row, where it shows as it is now. */
  lookAgain() {
    const s = this.state;
    if (s.kind === "resolve") {
      const id = s.jobId;
      this.close();
      return this.openConflicts(id);
    }
    if (s.review) Api.discardReview(s.review.jobId, s.review.reviewId).catch(() => {});
    s.review = null;
    s.kind = "sync";
    this.prepare(() => Api.prepareReview(s.jobId));
  }

  /** "Leave out" on a note: excludes that place. The list no longer fits the job, so it must be made again. */
  async leaveOut(index, button) {
    const s = this.state;
    const note = s.notes[index];
    button.disabled = true;
    try {
      const view = await Api.leaveOut(s.jobId, note.side, note.path);
      this.store.put(view);
      if (this.state !== s) return;
      note.leftOut = true;
      button.textContent = "Left out";
      s.changed = JOB_CHANGED;
      this.renderAlert();
      this.renderFooter();
    } catch (error) {
      button.disabled = false;
      this.toast.error(error);
    }
  }

  // ---------------------------------------------------------------- input

  onClick(event) {
    const target = event.target.closest("[data-do]");
    if (!target || target.disabled || !this.state) return;
    // While a confirmed list is being started, nothing may replace it or change the job.
    if (this.state.phase === "approving" && WAIT_FOR_APPROVE.includes(target.dataset.do)) return;
    switch (target.dataset.do) {
      case "close": return this.close();
      case "look-again": return this.lookAgain();
      case "confirm": return this.needsFreshLook() ? this.lookAgain() : this.approve();
      case "copy": return this.copyList(target);
      case "filter": return this.setFilter(target.dataset.value);
      case "cluster": return this.toggleCluster(Number(target.dataset.index));
      case "more": return this.loadSteps({ more: true });
      case "more-notes": return this.moreNotes(target);
      case "leave-out": return this.leaveOut(Number(target.dataset.index), target);
    }
  }

  onInput(event) {
    const s = this.state;
    if (event.target.dataset.field !== "query" || !s) return;
    clearTimeout(this.searchTimer);
    this.searchTimer = setTimeout(() => {
      s.query = event.target.value.trim();
      if (s.cluster !== null) {
        // The folder chose "Removals"; a search of their own starts from the whole list.
        s.cluster = null;
        s.filter = "all";
      }
      this.markChoices();
      this.loadSteps();
    }, 250);
  }

  onChange(event) {
    const s = this.state;
    if (event.target.dataset.field !== "tick" || !s) return;
    s.ticked = event.target.checked;
    this.renderFooter();
  }

  /** The Confirm button is pressed with a click or Space, never with Enter. */
  onKey(event) {
    if (event.key !== "Enter") return;
    if (event.target.closest?.('[data-do="confirm"]')) event.preventDefault();
    if (event.target.dataset?.field === "tick") event.preventDefault();
  }

  onToggle(event) {
    if (event.target.matches?.(".r-notes") && this.state) this.state.notesOpen = event.target.open;
  }

  onStore(hint) {
    const s = this.state;
    if (!s) return;
    if (!this.store.jobs.has(s.jobId)) return this.close(); // the job was deleted
    if (hint.scanOnly && hint.id === s.jobId && s.phase === "preparing") {
      const slot = Dom.$('[data-slot="counts"]', this.element);
      if (slot) slot.textContent = this.countsText();
    }
  }
}

/** A coloured block with an icon and a sentence, optionally led by a few words in bold. */
function notice(level, text, lead = "") {
  const icon = level === "info" ? Icons.info : Icons.warn;
  return html`<div class="notice ${level}"><span class="ico">${raw(icon)}</span>
      <span>${raw(lead ? html`<strong>${lead}</strong> ` : "")}${text}</span></div>`;
}

/** One step as a line of plain text: what, where, which path, how big, and why. */
function stepLine(step) {
  const size = step.item === "dir" ? "folder" : step.item === "link" ? "link" : Format.bytes(step.bytes);
  return [STEP_VERBS[step.kind] ?? step.kind, Format.side(step.side), step.path, size, step.reason].join("\t");
}

/** Puts text on the clipboard, the old way when the new one isn't allowed. */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Fall back below
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.style.cssText = "position: fixed; opacity: 0; pointer-events: none";
  document.body.append(area);
  area.select();
  const copied = document.execCommand("copy");
  area.remove();
  if (!copied) throw "Charon couldn't copy the list.";
}
