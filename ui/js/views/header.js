// The title bar's status line, the "Pause all" button and the overview tiles.

import { Dom } from "../dom.js";
import { GROUPS, comparesAtNextSync, needsYou, onlyToCompare } from "../store.js";
import { Counter } from "../effects/counter.js";
import { Icons } from "../icons.js";

const TONES = {
  syncing: "var(--syncing)",
  attention: "var(--error)",
  review: "var(--review)",
  paused: "var(--paused)",
  pending: "var(--pending)",
  looking: "var(--text-dim)",
  calm: "var(--in-sync)",
  empty: "var(--text-faint)",
};

export class Header {
  constructor(store, { pickGroup }) {
    this.store = store;
    this.headline = Dom.$("#headline");
    this.problem = Dom.$("#app-problem");
    this.pauseButton = Dom.$("#btn-pause-all");
    this.stats = Dom.$("#stats");
    this.buildTiles();
    this.stats.addEventListener("click", event => {
      const tile = event.target.closest("[data-group]");
      if (tile) pickGroup(tile.dataset.group);
    });
  }

  buildTiles() {
    this.stats.innerHTML = GROUPS.map(g => Dom.html`
      <button class="stat" data-group="${g.key}" style="--tone: ${g.tone}">
        <span class="label"><i></i>${g.label}</span>
        <span class="value" data-value="0">00</span>
      </button>`).join("");
  }

  render() {
    const overview = this.store.overview;
    if (!overview) return;
    const o = this.facts(overview);
    this.headline.querySelector(".text").textContent = this.describe(o);
    this.headline.style.setProperty("--tone", TONES[this.mood(o)]);
    // What keeps Charon itself from syncing, in full, under the title bar.
    this.problem.innerHTML = o.problem
      ? Dom.html`<div class="notice danger"><span class="ico">${Dom.raw(Icons.warn)}</span><span>${o.problem}</span></div>`
      : "";

    const paused = o.pausedAll;
    this.pauseButton.classList.toggle("on", paused);
    this.pauseButton.querySelector(".label").textContent = paused ? "Resume all" : "Pause all";
    this.pauseButton.querySelector(".ico").dataset.icon = paused ? "resume" : "pause";

    const counts = this.store.counts();
    for (const tile of Dom.$$(".stat", this.stats)) {
      tile.classList.toggle("on", tile.dataset.group === this.store.group);
      Counter.roll(tile.querySelector(".value"), counts[tile.dataset.group]);
    }
  }

  /**
   * The overview, checked against the rows: the title bar never looks calmer than the
   * jobs it sums up, even when an overview event is a moment behind.
   */
  facts(overview) {
    const jobs = this.store.all();
    const count = match => jobs.filter(match).length;
    const most = (key, match) => Math.max(overview[key] ?? 0, count(match));
    return {
      ...overview,
      jobs: Math.max(overview.jobs ?? 0, jobs.length),
      pending: most("pending", job => job.status === "pending" && !comparesAtNextSync(job)),
      pendingCompare: most("pendingCompare", comparesAtNextSync),
      waiting: most("waiting", job => job.waiting && !onlyToCompare(job.waiting)),
      toCompare: most("toCompare", job => job.waiting && onlyToCompare(job.waiting)),
      attention: most("attention", needsYou),
      looking: most("looking", job => job.status === "looking"),
      incomplete: most("incomplete", job => job.unreadable?.length || job.notSyncedCount || job.notSynced?.length),
      pausedAll: this.store.pausedAll(),
    };
  }

  mood(o) {
    if (o.problem) return "attention";
    if (o.jobs === 0) return "empty";
    if (o.syncing) return "syncing";
    if (o.attention) return "attention";
    if (o.waiting || o.toCompare) return "review";
    if (o.pausedAll) return "paused";
    if (o.pending || o.pendingCompare || o.incomplete) return "pending";
    if (o.looking) return "looking";
    return "calm";
  }

  /** One line about everything. Never calm while anything waits, needs you, or isn't known yet. */
  describe(o) {
    const waits = o.waiting === 1 ? "1 job waits for your OK" : `${o.waiting} jobs wait for your OK`;
    const compare = o.toCompare === 1 ? "1 job has files to compare" : `${o.toCompare} jobs have files to compare`;
    // What waits for the person: changes to confirm, files to compare, or both.
    const asks = [o.waiting && waits, o.toCompare && compare].filter(Boolean).join(" · ");
    const changes = o.pending === 1 ? "1 job has changes queued" : `${o.pending} jobs have changes queued`;
    const compareNext = o.pendingCompare === 1
      ? "1 job has files to compare at its next sync"
      : `${o.pendingCompare} jobs have files to compare at their next sync`;
    // What is queued without waiting for the person: changes, files to compare, or both.
    const queued = [o.pending && changes, o.pendingCompare && compareNext].filter(Boolean).join(" · ");
    const need = `${o.attention} ${o.attention === 1 ? "job needs" : "jobs need"} your attention`;
    if (o.problem) return "Charon needs your attention";
    if (o.jobs === 0) return "No crossings yet. Create your first sync job.";
    if (o.syncing) return `Ferrying files · ${o.syncing} ${o.syncing === 1 ? "job" : "jobs"} crossing${asks ? ` · ${asks}` : ""}`;
    if (o.attention) return asks ? `${need} · ${asks}` : need;
    if (o.pausedAll) return asks ? `Everything is paused · ${asks}` : "Everything is paused";
    if (asks) return asks;
    if (queued) return queued;
    if (o.incomplete) return `${o.incomplete} ${o.incomplete === 1 ? "job doesn't" : "jobs don't"} sync everything`;
    if (o.looking) return "Looking for changes…";
    return o.offline ? `All reachable jobs in sync · ${o.offline} offline` : "All in sync. The river is calm.";
  }
}
