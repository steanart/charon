// The list of job rows. Rows are redrawn one at a time when their job changes; live
// progress only touches the bar, so hovering and clicking stay smooth during a sync.

import { Dom } from "../dom.js";
import { Logo } from "../logo.js";
import { JobRow } from "./job-row.js";
import { PointerGlow } from "../effects/glow.js";

export class JobList {
  constructor(store, handlers) {
    this.store = store;
    this.handlers = handlers;
    this.element = Dom.$("#jobs");
    this.element.addEventListener("click", event => this.onClick(event));
    PointerGlow.follow(this.element, ".job");
  }

  /** Redraws what `hint` says changed (everything when it says nothing). */
  render(hint = {}) {
    if (hint.overviewOnly) return;
    if (hint.scanOnly) {
      // Counts while looking only matter to a row that is syncing.
      if (this.rowOf(hint.id)?.dataset.status === "syncing") this.renderProgress(hint.id);
      return;
    }
    if (hint.id && this.rowOf(hint.id) && this.store.visible().some(j => j.id === hint.id)) {
      return hint.progressOnly ? this.renderProgress(hint.id) : this.keepFocus(() => this.renderRow(hint.id));
    }
    this.keepFocus(() => this.renderAll());
  }

  /**
   * Redraws with `draw()` without losing the keyboard focus: a control focused in a row is
   * focused again in the row drawn anew, or else that row's first button.
   */
  keepFocus(draw) {
    const active = document.activeElement;
    const row = this.element.contains(active) ? active.closest(".job") : null;
    const own = row && active !== row ? Dom.selectorOf(active) : null;
    draw();
    if (!row || row.isConnected) return;
    const again = this.rowOf(row.dataset.id);
    const target = (own && again?.querySelector(`${own}:not([disabled])`)) ?? again?.querySelector("button:not([disabled])");
    (target ?? this.element).focus({ preventScroll: true });
  }

  renderAll() {
    const jobs = this.store.visible();
    if (this.store.jobs.size === 0) {
      this.element.innerHTML = `
        <div class="empty-state">
          ${Logo.html()}
          <h2>No crossings yet</h2>
          <p>A sync job connects two folders. Charon looks for changes by hand, on change, on a
            schedule or when a drive is connected, and shows you the list before anything crosses.</p>
          <button class="btn primary" data-action="new">Create your first job</button>
        </div>`;
      return;
    }
    if (jobs.length === 0) {
      this.element.innerHTML = '<div class="no-match">No job matches.</div>';
      return;
    }
    this.element.innerHTML = jobs.map((job, index) => this.rowHtml(job, index)).join("");
  }

  renderRow(id) {
    const old = this.rowOf(id);
    const index = Number(old.style.getPropertyValue("--i")) || 0;
    const wrapper = document.createElement("div");
    wrapper.innerHTML = this.rowHtml(this.store.jobs.get(id), index);
    const row = wrapper.firstElementChild;
    row.style.animation = "none"; // no entrance animation on updates
    for (const prop of ["--mx", "--my"]) row.style.setProperty(prop, old.style.getPropertyValue(prop));
    old.replaceWith(row);
  }

  /** Only the progress block, so the row isn't rebuilt many times a second. */
  renderProgress(id) {
    const row = this.rowOf(id);
    const job = this.store.jobs.get(id);
    if (row.dataset.status !== "syncing") return this.keepFocus(() => this.renderRow(id));
    const current = row.querySelector(".progress");
    const wrapper = document.createElement("div");
    wrapper.innerHTML = JobRow.progress(job.progress, this.store.scans.get(id));
    const next = wrapper.firstElementChild;
    const oldFill = current?.querySelector(".bar:not(.indeterminate) .fill");
    const newFill = next.querySelector(".bar:not(.indeterminate) .fill");
    const stillLooking = current?.querySelector(".bar.indeterminate") && next.querySelector(".bar.indeterminate");
    if (oldFill && newFill) {
      // Same bar: move it, so the width animates instead of jumping.
      oldFill.style.setProperty("--p", newFill.style.getPropertyValue("--p"));
      current.querySelector(".numbers").replaceWith(next.querySelector(".numbers"));
    } else if (stillLooking) {
      // Still looking: only the counts change, the drifting bar keeps going.
      current.querySelector(".numbers").replaceWith(next.querySelector(".numbers"));
    } else if (current) {
      current.replaceWith(next);
    }
  }

  rowHtml(job, index) {
    const id = job.id;
    return JobRow.render(job, {
      open: this.store.open.has(id),
      tab: this.store.tabOf(id),
      log: this.store.logs.get(id),
      scan: this.store.scans.get(id),
      trashDays: this.store.settings?.trashDays ?? 30,
      pausedAll: this.store.pausedAll(),
      index,
    });
  }

  rowOf(id) {
    return this.element.querySelector(`.job[data-id="${CSS.escape(id)}"]`);
  }

  /** Scrolls a row to the top of the list, so what opens under it is clear of the faded end. */
  reveal(id) {
    this.rowOf(id)?.scrollIntoView({ block: "start", behavior: "smooth" });
  }

  /** Every click inside the list ends up here. */
  onClick(event) {
    const target = event.target.closest("[data-action]");
    if (!target) return;
    const action = target.dataset.action;
    if (action === "new") return this.handlers.newJob();
    const id = target.closest(".job")?.dataset.id;
    if (!id) return;
    if (action === "toggle" && event.target.closest("button")) return; // buttons inside the row
    this.handlers.act(action, id, target);
  }
}
