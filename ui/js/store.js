// The window's data: jobs, settings and what the person is looking at.

const isAny = (...statuses) => job => statuses.includes(job.status);

/**
 * Errors and conflicts. Rust shows "paused" before "conflict" and "error", so a paused job
 * still counts when it has conflicts or its last run failed.
 */
export const needsYou = job => job.status === "error" || job.status === "conflict" || job.conflicts?.length > 0
  || (job.status !== "offline" && job.status !== "syncing" && Boolean(job.problem || job.lastRun?.error));

/**
 * Whether all that waits is files the last look couldn't compare yet, as Rust's
 * `Waiting::only_to_compare` says: the list compares them, and may well find nothing to confirm.
 */
export const onlyToCompare = waiting => !waiting.held && !waiting.items && !waiting.conflicts && waiting.uncompared > 0;

/**
 * Whether all a job has queued is files its last look couldn't compare yet, and its next sync
 * compares them, as Rust's `JobView::compares_at_next_sync` says: it syncs by itself, and no
 * list the person confirmed waits to start.
 */
export const comparesAtNextSync = job => job.status === "pending" && !job.confirmed
  && !job.pending?.items && job.pending?.uncompared > 0;

/** The overview tiles and which jobs each one counts. */
export const GROUPS = [
  { key: "all", label: "Jobs", tone: "var(--text-dim)", match: () => true },
  { key: "in-sync", label: "In sync", tone: "var(--in-sync)", match: isAny("in-sync") },
  { key: "syncing", label: "Crossing", tone: "var(--syncing)", match: isAny("syncing") },
  // Jobs whose list waits for the person: changes to confirm, a run held back, or files to compare.
  { key: "to-review", label: "To review", tone: "var(--review)", match: job => Boolean(job.waiting) },
  { key: "queued", label: "Queued", tone: "var(--pending)", match: isAny("pending", "new", "paused", "offline", "looking") },
  { key: "attention", label: "Need you", tone: "var(--error)", match: needsYou },
];

export class JobStore {
  constructor() {
    this.jobs = new Map();
    this.order = [];
    this.settings = null;
    this.overview = null;
    this.defaultExcludes = [];
    this.group = "all";
    this.query = "";
    this.open = new Set();      // ids of expanded rows
    this.tabs = new Map();      // id → open tab in the expanded row
    this.logs = new Map();      // id → log lines
    this.scans = new Map();     // id → {source, destination}: items counted while looking
    this.compares = new Map();  // id → {files, totalFiles, bytes, totalBytes}: files compared by contents
    this.removed = new Set();   // ids of jobs Rust removed (a state taken before can't bring them back)
    this.listeners = [];
  }

  /**
   * Takes the full state from Rust. Rows that changed while it was on its way (a `job`
   * event with a higher `rev`) and jobs removed meanwhile stay as the events left them.
   */
  load(state) {
    const fresher = new Map([...this.jobs].filter(([id, job]) => {
      const loaded = state.jobs.find(other => other.id === id);
      return !loaded || (job.rev ?? 0) > (loaded.rev ?? 0);
    }));
    const jobs = state.jobs.filter(job => !this.removed.has(job.id)).map(job => fresher.get(job.id) ?? job);
    const added = [...fresher.values()].filter(job => !jobs.some(other => other.id === job.id));
    this.jobs = new Map([...jobs, ...added].map(job => [job.id, job]));
    this.order = [...jobs, ...added].map(job => job.id);
    this.settings = state.settings;
    this.overview = state.overview;
    this.defaultExcludes = state.defaultExcludes;
    this.changed();
  }

  /** A new or changed row. A view older than the one we have (lower `rev`) is ignored. */
  put(view) {
    if (this.removed.has(view.id)) return;
    const known = this.jobs.get(view.id);
    if (known && (view.rev ?? 0) < (known.rev ?? 0)) return;
    if (!known) this.order.push(view.id);
    this.jobs.set(view.id, view);
    this.changed({ id: view.id });
  }

  /** A removed row. */
  drop(id) {
    this.removed.add(id);
    this.jobs.delete(id);
    this.order = this.order.filter(other => other !== id);
    this.open.delete(id);
    this.tabs.delete(id);
    this.logs.delete(id);
    this.scans.delete(id);
    this.compares.delete(id);
    this.changed();
  }

  /**
   * Live progress of a running sync (doesn't redraw the whole list). Only a `job` event
   * marks a row as syncing, so progress for a row that isn't syncing is ignored.
   */
  setProgress(id, progress) {
    const job = this.jobs.get(id);
    if (!job || job.status !== "syncing") return;
    job.progress = progress;
    this.changed({ id, progressOnly: true });
  }

  /** Items counted so far on one side while a job's folders are looked at. */
  setScan(id, side, items) {
    const scan = this.scans.get(id) ?? { source: 0, destination: 0 };
    scan[side] = items;
    this.scans.set(id, scan);
    this.changed({ id, scanOnly: true });
  }

  /** Files compared by contents so far while a job's list is made (after both scans). */
  setCompare(id, compare) {
    this.compares.set(id, compare);
    this.changed({ id, scanOnly: true });
  }

  /** Forgets the counts, before a new look starts. */
  clearScan(id) {
    this.scans.delete(id);
    this.compares.delete(id);
  }

  setOverview(overview) {
    this.overview = overview;
    this.changed({ overviewOnly: true });
  }

  /** The settings as Rust has them now; they replace ours, `pausedAll` included. */
  setSettings(settings) {
    this.settings = settings;
    this.changed({ settings: true });
  }

  /** Whether "Pause everything" is on. */
  pausedAll() {
    return Boolean(this.settings?.pausedAll ?? this.overview?.pausedAll);
  }

  setLog(id, lines) {
    this.logs.set(id, lines);
    this.changed({ id });
  }

  /** Expands or folds a row. */
  toggleOpen(id) {
    if (this.open.has(id)) this.open.delete(id);
    else this.open.add(id);
    this.changed({ id });
    return this.open.has(id);
  }

  /** The open tab of an expanded row. */
  tabOf(id) {
    const job = this.jobs.get(id);
    const tab = this.tabs.get(id);
    if (tab === "conflicts" && !job?.conflicts?.length) return "activity";
    if (tab === "unreadable" && !job?.unreadable?.length) return "activity";
    if (tab === "not-synced" && !job?.notSynced?.length) return "activity";
    return tab ?? (job?.conflicts?.length ? "conflicts" : "activity");
  }

  setTab(id, tab) {
    this.tabs.set(id, tab);
    this.changed({ id });
  }

  /** All jobs in list order. */
  all() {
    return this.order.map(id => this.jobs.get(id)).filter(Boolean);
  }

  /** The jobs whose lists wait for the person: changes to confirm, or files to compare. */
  waiting() {
    return this.all().filter(job => job.waiting);
  }

  /** The jobs the current tile and search let through. */
  visible() {
    const group = GROUPS.find(g => g.key === this.group) ?? GROUPS[0];
    const query = this.query.toLowerCase();
    return this.all().filter(job => {
      if (!group.match(job)) return false;
      if (!query) return true;
      return [job.name, job.source, job.destination].some(text => String(text).toLowerCase().includes(query));
    });
  }

  /** How many jobs each tile counts. */
  counts() {
    const jobs = this.all();
    return Object.fromEntries(GROUPS.map(g => [g.key, jobs.filter(g.match).length]));
  }

  subscribe(listener) {
    this.listeners.push(listener);
  }

  /** Tells the views something changed. `hint` says what, so they can redraw less. */
  changed(hint = {}) {
    for (const listener of this.listeners) listener(hint);
  }
}
