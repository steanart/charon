// A pretend Rust side for opening the window in a plain browser: example jobs, reviews of
// changes that wait for an OK, simulated syncs with live progress, conflicts and places
// that couldn't be read. It answers every command and sends every event the real one does.
// Never used inside the real app.
//
// The tray menu and a few surprises can be tried from the browser console:
//   charonDemo.syncAll()          the tray's "Sync All…"
//   charonDemo.showWaiting()      the tray's "Review waiting jobs…"
//   charonDemo.quit()             the tray's Quit while jobs wait
//   charonDemo.quitting()         Charon quits while a sync runs and stops it safely first
//   charonDemo.change("projects") something changes in a job's folders (open lists go stale)
//   charonDemo.refuseNext()       the next confirm is refused, to see how that looks

const SECOND = 1000;
const MIN = 60 * SECOND;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const REVIEW_LIFETIME = 15 * MIN;

const now = () => new Date().toISOString();
const ago = ms => new Date(Date.now() - ms).toISOString();
const later = ms => new Date(Date.now() + ms).toISOString();
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

const DEFAULT_EXCLUDES = [".DS_Store", "._*", ".Spotlight-V100", ".Trashes", ".fseventsd", "Thumbs.db", "desktop.ini", "$RECYCLE.BIN", "~$*", "*.tmp"];

/** Which kinds of steps each filter of the review shows. */
/** The kinds of note a row lists under "Not synced", as the engine's JobView does. */
const NOT_SYNCED = ["left-out", "name-clash", "blocked"];

const FILTER_KINDS = {
  removals: ["remove"],
  replacements: ["replace", "settle"],
  conflicts: ["conflict", "keep-both", "settle"],
  new: ["new", "folder"],
};

/**
 * Whether a job needs the person, as Rust's JobView::needs_you says: an error or a conflict,
 * whatever its row shows first, but not the reason a job is out of reach or syncing.
 */
const needsYou = j => j.status === "error" || j.status === "conflict" || j.conflicts.length > 0
  || (!["offline", "syncing"].includes(j.status) && Boolean(j.problem || j.lastRun?.error));

/** Whether all that waits is files a look couldn't compare yet, as Rust's Waiting::only_to_compare says. */
const onlyToCompare = waiting => !waiting.held && !waiting.items && !waiting.conflicts && waiting.uncompared > 0;

/** Whether all a job has queued is files to compare at its next sync, as Rust's JobView::compares_at_next_sync says. */
const comparesAtNextSync = j => j.status === "pending" && !j.confirmed && !j.pending?.items && j.pending?.uncompared > 0;

/** The order steps are listed in: what can hurt first. */
const KIND_ORDER = ["conflict", "remove", "replace", "settle", "keep-both", "new", "folder"];

/** Numbers that look random but are the same on every reload. */
function dice(seed) {
  let state = seed;
  return () => (state = (state * 16807) % 2147483647) / 2147483647;
}

/** One planned change. Removals and replacements of files are the dangerous ones. */
function step(kind, side, path, bytes, reason, item = "file") {
  const danger = kind === "remove" || kind === "replace" || (kind === "settle" && side !== null);
  return { kind, side, path, item, bytes: item === "dir" ? 0 : bytes, reason, danger };
}

/** "Monday", for "since Monday". */
const weekday = iso => new Date(iso).toLocaleDateString("en-GB", { weekday: "long" });

/** The fields of a job that decide what a sync does. */
const setupKey = j => JSON.stringify([j.source, j.destination, j.mode, j.conflict, j.excludes, j.safeDelete, j.fullAutomation]);

/** A last run, with every field the real RunSummary has. */
function run(fields) {
  return {
    startedAt: ago(5 * MIN), finishedAt: ago(4 * MIN),
    copied: 0, deleted: 0, bytes: 0, conflicts: 0, failed: 0, skipped: 0, waited: 0, leftOut: 0, kept: 0, unreadable: 0, foundUnreadable: 0, settled: 0, unkept: 0, leftForLater: 0, failedItems: [],
    cancelled: false, error: null, approved: true,
    ...fields,
  };
}

/** One example job, with the fields the real JobView has. */
function job(id, fields) {
  return {
    id,
    name: "",
    source: "",
    destination: "",
    mode: "mirror",
    conflict: "keep-both",
    trigger: { kind: "manual" },
    excludes: DEFAULT_EXCLUDES.slice(0, 6),
    safeDelete: true,
    fullAutomation: false,
    paused: false,
    createdAt: ago(30 * DAY),
    lastRun: run({ copied: 12, deleted: 1, bytes: 8_200_000 }),
    lastSynced: ago(4 * MIN),
    needsReview: false,
    rev: 0,
    status: "in-sync",
    runsByItself: false,
    looking: false,
    waiting: null,
    pending: null,
    progress: null,
    conflicts: [],
    unreadable: [],
    unreadableCount: 0,
    notSynced: [],
    notSyncedCount: 0,
    problem: null,
    sourceOnline: true,
    destinationOnline: true,
    sourceVolume: null,
    destinationVolume: null,
    nextRun: null,
    stale: null,
    waitingFor: null,
    confirmed: false,
    ...fields,
  };
}

// ---------------------------------------------------------------- the example folders

/** What differs between the two folders of each example job, as the next review would list it. */
const WORLDS = {
  projects() {
    const roll = dice(7);
    const size = (min, max) => Math.round(min + roll() * (max - min));
    const steps = [];
    // From the NAS to the source: these change the person's originals.
    for (const path of ["Website/assets/hero-v3.png", "Website/assets/hero-v3@2x.png", "Talks/2026-ferry.key", "Talks/notes.md"]) {
      steps.push(step("new", "source", path, size(20_000, 9_000_000), "new on the destination"));
    }
    for (const path of ["Charon/notes/roadmap.md", "Charon/notes/pricing.md"]) {
      steps.push(step("replace", "source", path, size(3_000, 40_000), "changed on the destination since the last sync"));
    }
    for (const path of ["Charon/drafts/old-plan.txt", "Charon/drafts/website-copy-v1.md", "Charon/drafts/logo-sketch.png"]) {
      steps.push(step("remove", "source", path, size(2_000, 900_000), "removed on the destination since the last sync"));
    }
    // From the source to the NAS.
    const fresh = [
      "core/src/plan/guard.rs", "core/src/plan/baseline.rs", "core/src/plan/ground.rs",
      "core/src/run/durable.rs", "core/src/run/marker.rs", "core/src/run/rename.rs",
      "core/src/scan/contents.rs", "core/src/scan/keys.rs", "core/src/scan/root.rs",
      "ui/js/views/review-dialog.js", "ui/js/views/waiting-sheet.js", "ui/js/views/upgrade-sheet.js",
      "ui/js/views/modal.js", "ui/js/views/overlay.js", "ui/css/review.css",
      "docs/review.md", "website/index.html", "CHANGELOG.md",
    ];
    for (const path of fresh) steps.push(step("new", "destination", `Charon/${path}`, size(2_000, 60_000), "new on the source"));
    steps.push(step("folder", "destination", "Charon/core/src/plan", 0, "new on the source", "dir"));
    steps.push(step("folder", "destination", "Charon/ui/js/views/review", 0, "new on the source", "dir"));
    for (const path of ["core/src/model/job.rs", "core/src/plan/two_way.rs", "ui/js/main.js", "ui/js/store.js", "Cargo.toml"]) {
      steps.push(step("replace", "destination", `Charon/${path}`, size(1_000, 50_000), "changed on the source since the last sync"));
    }
    steps.push(step("remove", "destination", "Renders/2019", 0, "removed on the source since the last sync", "dir"));
    for (let i = 1; i <= 860; i++) {
      steps.push(step("remove", "destination", `Renders/2019/shot-${String(i).padStart(4, "0")}.exr`, size(4_000_000, 9_000_000), "removed on the source since the last sync"));
    }
    for (let i = 1; i <= 20; i++) {
      steps.push(step("remove", "destination", `Scratch/tmp-${String(i).padStart(3, "0")}.txt`, size(200, 9_000), "removed on the source since the last sync"));
    }
    steps.push(step("keep-both", null, "Charon/README.md", 6_400, "changed on both sides: both versions are kept, the destination's as “README (conflict 2026-10-19).md”"));
    return {
      files: { source: 12_034, destination: 11_998 },
      steps,
      agreed: 8_000,
      compare: { files: 36, bytes: 412_000_000 },
      warnings: [
        { level: "danger", text: "880 files would be removed on the destination, among them the whole folder Renders/2019 (860 files). Check that this is what you meant to do on the source." },
        { level: "caution", text: "3 files would be removed and 2 replaced on the source, where your originals are." },
      ],
      housekeeping: ["Also empties 120 items older than 30 days from .charon-trash on the destination (1.2 GB)"],
      notes: [
        { kind: "unreadable", side: "source", path: "Private/keys", reason: "Permission denied", canLeaveOut: true },
        { kind: "name-clash", side: "destination", path: "Docs/Readme.md", reason: "Docs/README.md is there too, and the NAS doesn't tell upper and lower case apart. Rename one of them; until then neither crosses.", canLeaveOut: false },
        { kind: "kept-folder", side: "destination", path: "build", reason: "Kept, though it was removed on the source: it still holds files your excludes leave out (node_modules).", canLeaveOut: false },
        { kind: "left-out", side: "source", path: "Charon/target/latest", reason: "A symbolic link, and the NAS can't hold links: it stays on the source.", canLeaveOut: false },
      ],
      looked: true,
      since: ago(25 * HOUR),
    };
  },

  photos() {
    const roll = dice(11);
    const steps = [];
    for (let i = 0; i < 12; i++) {
      steps.push(step("new", "destination", `2026/10/IMG_${4120 + i}.HEIC`, Math.round(1_800_000 + roll() * 2_400_000), "new on the source"));
    }
    steps.push(step("remove", "destination", "2026/09/IMG_3999 (1).HEIC", 2_310_000, "no longer in the source"));
    return {
      files: { source: 48_210, destination: 48_199 },
      steps,
      agreed: 0,
      warnings: [{ level: "info", text: "Full automation is on: when the drive is connected, this job syncs by itself without showing you the list." }],
      housekeeping: [],
      notes: [],
      looked: false,
    };
  },

  documents() {
    return {
      files: { source: 3_402, destination: 3_398 },
      steps: [
        step("new", "destination", "Invoices/2026-10-18.pdf", 182_000, "new on the source"),
        step("new", "destination", "Letters/bank-reply.pdf", 96_000, "new on the source"),
        step("replace", "destination", "Invoices/summary-2026.xlsx", 48_000, "changed on the source; the older copy goes to .charon-trash"),
      ],
      agreed: 0,
      warnings: [],
      housekeeping: [],
      notes: [
        { kind: "unreadable", side: "source", path: "Mail Downloads", reason: "Operation not permitted (macOS privacy)", canLeaveOut: true },
        { kind: "unreadable", side: "source", path: "Taxes/2025/scan.pdf", reason: "Permission denied", canLeaveOut: true },
      ],
      looked: false,
    };
  },

  notes() {
    return {
      files: { source: 812, destination: 809 },
      steps: [
        step("conflict", null, "Ideas/charon-website.md", 0, "changed on both sides: left for you"),
        step("conflict", null, "Todo.txt", 0, "changed on the source, removed on the destination: left for you"),
        step("conflict", null, "Drafts", 0, "a folder on the source, a file on the destination: left for you", "dir"),
        step("new", "destination", "Journal/2026-10-20.md", 2_100, "new on the source"),
      ],
      agreed: 0,
      warnings: [],
      housekeeping: [],
      notes: [],
      looked: true,
      since: ago(2 * HOUR),
    };
  },

  music() {
    return { files: { source: 9_870, destination: 0 }, steps: [], agreed: 0, warnings: [], housekeeping: [], notes: [], looked: false };
  },

  archive() {
    const roll = dice(23);
    const steps = [];
    for (let i = 1; i <= 120; i++) {
      steps.push(step("new", "destination", `Scans/2026/scan-${String(i).padStart(4, "0")}.pdf`, Math.round(300_000 + roll() * 2_000_000), "new on the source"));
    }
    for (const path of ["Contracts/lease-2024.pdf", "Contracts/insurance.pdf", "Taxes/2025/summary.xlsx", "Taxes/2025/receipts.zip"]) {
      steps.push(step("replace", "destination", path, Math.round(40_000 + roll() * 3_000_000), "changed on the source; the older copy goes to .charon-trash"));
    }
    const since = ago(4 * DAY);
    return { files: { source: 20_410, destination: 20_290 }, steps, agreed: 0, warnings: [], housekeeping: [], notes: [], looked: true, since };
  },

  camera() {
    const roll = dice(31);
    const steps = [step("remove", "destination", "100CANON", 0, "not in the source, which is empty", "dir")];
    for (let i = 1; i <= 4_210; i++) {
      steps.push(step("remove", "destination", `100CANON/IMG_${String(i).padStart(4, "0")}.JPG`, Math.round(3_800_000 + roll() * 2_600_000), "not in the source, which is empty"));
    }
    return {
      files: { source: 0, destination: 4_210 },
      steps,
      agreed: 0,
      warnings: [{ level: "danger", text: "The source is empty. Every file in the destination would be removed." }],
      housekeeping: [],
      notes: [],
      blocked: "The source is empty, so every file in the destination would be removed. Charon won't do that: check that the right card is in the reader, then look again.",
      held: "the source looks empty, so 4,210 files would be removed",
      looked: true,
      since: ago(40 * MIN),
    };
  },
};

/** What a job the person just made would find: a first crossing. */
function freshWorld(j) {
  const roll = dice(j.name.length * 97 + 13);
  const steps = [];
  const files = 20 + Math.round(roll() * 60);
  for (let i = 1; i <= files; i++) {
    const folder = ["Documents", "Pictures", "Notes"][i % 3];
    steps.push(step("new", "destination", `${folder}/file-${String(i).padStart(3, "0")}.dat`, Math.round(5_000 + roll() * 4_000_000), "new on the source"));
  }
  steps.push(step("folder", "destination", "Documents", 0, "new on the source", "dir"));
  if (j.mode === "two-way") {
    steps.push(step("new", "source", "From the other side/readme.txt", 1_200, "new on the destination"));
    if (j.conflict === "newest-wins") steps.push(step("settle", "destination", "Notes/shared.md", 3_100, "changed on both sides: the source's version is newer and replaces the other"));
  }
  return { files: { source: files + 4, destination: 0 }, steps, agreed: 0, warnings: [], housekeeping: [], notes: [], looked: false };
}

// ---------------------------------------------------------------- the pretend backend

export class DemoBackend {
  static create() {
    const backend = new DemoBackend();
    window.charonDemo = backend.console();
    console.info("Charon demo: try charonDemo.syncAll(), charonDemo.showWaiting(), charonDemo.quit(), charonDemo.quitting(), charonDemo.change(\"projects\") or charonDemo.refuseNext().");
    return { core: { invoke: (c, a) => backend.invoke(c, a) }, event: { listen: (e, h) => backend.listen(e, h) } };
  }

  constructor() {
    this.listeners = {};
    this.settings = { appearance: "system", launchAtLogin: true, runInBackground: true, notifications: true, trashDays: 30, pausedAll: false, checkMinutes: 15, checkUpdates: true };
    this.worlds = new Map();    // id → what differs between its folders (never sent as is)
    this.reviews = new Map();   // reviewId → {review, steps, notes, jobId, setup, stale, settles}
    this.looks = new Map();     // id → set of running looks, so cancel_review can stop them
    this.checked = new Set();   // ids of the jobs looked at since the demo started
    this.timers = new Map();    // id → a running sync's timer
    this.logs = new Map();      // id → log lines, newest first
    this.nextRuns = new Map();  // id → when its schedule comes next (sent only while it isn't paused)
    this.reviewCount = 0;
    this.refuseOnce = false;
    this.noticeSeen = false;

    const jobs = [
      job("projects", {
        name: "Projects ⇄ NAS", source: "/Users/andrei/Projects", destination: "/Volumes/NAS/Projects",
        mode: "two-way", trigger: { kind: "on-change", settleSeconds: 5 }, excludes: [...DEFAULT_EXCLUDES.slice(0, 6), "node_modules"],
        destinationVolume: { name: "NAS", kind: "SMB" }, lastSynced: ago(26 * HOUR), lastRun: run({ startedAt: ago(26 * HOUR + MIN), finishedAt: ago(26 * HOUR), copied: 40, deleted: 2, bytes: 31_000_000 }),
      }),
      job("photos", {
        name: "Photos → Backup SSD", source: "/Users/andrei/Pictures/Photos", destination: "/Volumes/Backup SSD/Photos",
        trigger: { kind: "drive-connected" }, fullAutomation: true, destinationVolume: { name: "Backup SSD", kind: "exFAT" }, lastSynced: ago(26 * MIN),
      }),
      job("documents", {
        name: "Documents → Cloud Drive", source: "/Users/andrei/Documents", destination: "/Users/andrei/Cloud Drive/Documents",
        mode: "backup", trigger: { kind: "interval", minutes: 60 }, nextRun: later(14 * MIN), lastSynced: ago(46 * MIN),
        unreadable: [
          { side: "source", path: "Mail Downloads", reason: "Operation not permitted", privacy: true },
          { side: "source", path: "Taxes/2025/scan.pdf", reason: "Permission denied", privacy: false },
        ],
      }),
      job("notes", {
        name: "Notes ⇄ Laptop", source: "/Users/andrei/Notes", destination: "/Volumes/Laptop/Notes", mode: "two-way", conflict: "ask",
        trigger: { kind: "on-change", settleSeconds: 3 }, destinationVolume: { name: "Laptop", kind: "APFS" }, lastSynced: ago(2 * HOUR),
        conflicts: [
          { key: "c-ideas", path: "Ideas/charon-website.md", sourceKind: "file", destinationKind: "file", sourceModified: Date.now() - 12 * MIN, destinationModified: Date.now() - 40 * MIN, sourceSize: 4_210, destinationSize: 3_980 },
          { key: "c-todo", path: "Todo.txt", sourceKind: "file", destinationKind: null, sourceModified: Date.now() - 3 * MIN, destinationModified: null, sourceSize: 820, destinationSize: null },
          { key: "c-drafts", path: "Drafts", sourceKind: "dir", destinationKind: "file", sourceModified: null, destinationModified: Date.now() - 2 * DAY, sourceSize: null, destinationSize: 14_200 },
        ],
      }),
      job("music", {
        name: "Music → Travel drive", source: "/Users/andrei/Music", destination: "/Volumes/TRAVEL/Music", trigger: { kind: "drive-connected" },
        destinationOnline: false, destinationVolume: { name: "TRAVEL", kind: "FAT32" }, problem: "The destination isn't reachable. Is the drive connected?",
        lastSynced: ago(6 * DAY), stale: "Not synced for 6 days",
      }),
      job("archive", {
        name: "Archive → Cold storage", source: "/Users/andrei/Archive", destination: "/Volumes/Cold/Archive", mode: "backup",
        trigger: { kind: "daily", hour: 3, minute: 0 }, paused: true, destinationVolume: { name: "Cold", kind: "APFS" }, lastSynced: ago(5 * DAY),
      }),
      job("camera", {
        name: "Camera card → Imports", source: "/Volumes/EOS_DIGITAL/DCIM", destination: "/Users/andrei/Pictures/Imports",
        trigger: { kind: "drive-connected" }, fullAutomation: true, needsReview: true, sourceVolume: { name: "EOS_DIGITAL", kind: "FAT32" },
        lastSynced: ago(2 * DAY), lastRun: run({ startedAt: ago(2 * DAY), finishedAt: ago(2 * DAY - MIN), copied: 312, bytes: 1_480_000_000 }),
      }),
    ];
    this.jobs = jobs;
    for (const j of jobs) {
      if (j.nextRun) this.nextRuns.set(j.id, j.nextRun);
      this.worlds.set(j.id, WORLDS[j.id]());
      this.logs.set(j.id, this.startingLog(j));
      this.settle(j);
    }
    this.find("archive").stale = `Changes have waited for your OK since ${weekday(this.world("archive").since)}`;
    this.settle(this.find("archive"));
  }

  // ---------------------------------------------------------------- events

  listen(event, handler) {
    (this.listeners[event] ??= []).push(handler);
    return Promise.resolve(() => {});
  }

  emit(event, payload) {
    for (const handler of this.listeners[event] ?? []) handler({ payload: payload === undefined ? undefined : structuredClone(payload) });
  }

  /** Sends a job's new view, and the overview with it. */
  publish(j) {
    this.settle(j);
    this.emit("job", j);
    this.emit("overview", this.overview());
  }

  overview() {
    const count = (...s) => this.jobs.filter(j => s.includes(j.status)).length;
    return {
      jobs: this.jobs.length,
      syncing: count("syncing"),
      pending: this.jobs.filter(j => j.status === "pending" && !comparesAtNextSync(j)).length,
      pendingCompare: this.jobs.filter(comparesAtNextSync).length,
      waiting: this.jobs.filter(j => j.waiting && !onlyToCompare(j.waiting)).length,
      toCompare: this.jobs.filter(j => j.waiting && onlyToCompare(j.waiting)).length,
      attention: this.jobs.filter(needsYou).length,
      offline: count("offline"),
      looking: count("looking"),
      incomplete: this.jobs.filter(j => j.unreadable.length || j.notSyncedCount > 0).length,
      pausedAll: this.settings.pausedAll,
      problem: null,
    };
  }

  // ---------------------------------------------------------------- a job's state

  find(id) {
    const found = this.jobs.find(j => j.id === id);
    if (!found) throw "This sync job no longer exists.";
    return found;
  }

  world(id) {
    return this.worlds.get(id);
  }

  online(j) {
    return j.sourceOnline && j.destinationOnline;
  }

  syncing(j) {
    return this.timers.has(j.id) || j.status === "syncing";
  }

  /** Works out the derived fields (status, waiting, …) as Rust does, and counts the change. */
  settle(j) {
    const w = this.world(j.id);
    const changes = w.steps.filter(s => s.kind !== "conflict");
    // Until the demo has looked at a job, what waited before still waits, but uncounted.
    const checked = this.checked.has(j.id);
    const waited = w.looked && changes.length > 0;
    const known = checked && waited;
    j.runsByItself = j.fullAutomation && !j.needsReview;
    j.waiting = known && (!j.runsByItself || w.held) ? this.waitingOf(j, w, changes)
      : !checked && waited && !j.runsByItself ? { ...this.waitingOf(j, w, []), conflicts: 0, held: null, counted: false }
      : null;
    // The demo's looks compare every file, so none is left "not compared yet".
    j.pending = known && !j.waiting ? { items: changes.length, bytes: changes.reduce((n, s) => n + s.bytes, 0), uncompared: 0, checkedAt: now() } : null;
    const places = checked && w.looked ? w.notes.filter(n => NOT_SYNCED.includes(n.kind) && n.side) : [];
    j.notSynced = places.slice(0, 100).map(({ kind, side, path, reason }) => ({ kind, side, path, reason, privacy: false }));
    j.notSyncedCount = places.length;
    j.unreadableCount = j.unreadable.length;
    const failedRun = j.lastRun && !j.lastRun.cancelled && (j.lastRun.error || j.lastRun.failed > 0);
    // "Pause everything" pauses each job without changing its own pause, as in Rust.
    const paused = j.paused || this.settings.pausedAll;
    j.nextRun = paused ? null : this.nextRuns.get(j.id) ?? null;
    j.status = this.timers.has(j.id) ? "syncing"
      : !this.online(j) ? "offline"
      : paused ? "paused"
      : j.conflicts.length ? "conflict"
      : failedRun ? "error"
      : j.waiting ? "review"
      : !j.lastSynced ? "new"
      : j.pending ? "pending"
      : !checked ? "looking"
      : "in-sync";
    j.looking = !this.timers.has(j.id) && (this.looks.get(j.id)?.size > 0 || (!checked && this.online(j) && !j.paused));
    if (j.status !== "syncing") j.progress = null;
    j.rev += 1;
  }

  /** Looks at every job once, a moment after the window asks, as Charon does when it starts. */
  lookAtEverything() {
    if (this.lookedAtStart) return;
    this.lookedAtStart = true;
    this.jobs.forEach((j, i) => setTimeout(() => {
      if (!this.jobs.includes(j) || this.checked.has(j.id)) return;
      this.checked.add(j.id);
      this.publish(j);
    }, 1_200 + i * 600));
  }

  waitingOf(j, w, changes) {
    const count = (kind, side) => changes.filter(s => s.kind === kind && s.side === side && s.item !== "dir").length;
    return {
      items: changes.length,
      bytes: changes.reduce((n, s) => n + s.bytes, 0),
      removals: { source: count("remove", "source"), destination: count("remove", "destination") },
      replacements: { source: count("replace", "source"), destination: count("replace", "destination") },
      conflicts: w.steps.filter(s => s.kind === "conflict").length,
      held: w.held ?? null,
      uncompared: 0,
      since: w.since ?? now(),
      counted: true,
    };
  }

  startingLog(j) {
    const lines = [
      { time: ago(3 * DAY), level: "info", text: "job created" },
    ];
    if (j.lastRun) lines.unshift({ time: j.lastRun.finishedAt, level: "info", text: `copied ${j.lastRun.copied} files, removed ${j.lastRun.deleted} items (confirmed by you)` });
    const w = this.world(j.id);
    if (w.looked && w.steps.length) lines.unshift({ time: w.since ?? ago(MIN), level: "warn", text: `${w.steps.length} changes wait for your OK` });
    if (j.id === "documents") lines.unshift({ time: ago(46 * MIN), level: "warn", text: "Mail Downloads: macOS didn't let Charon read this folder" });
    return lines;
  }

  log(j, text, level = "info") {
    this.logs.get(j.id)?.unshift({ time: now(), level, text });
  }

  /** Something changed in a job's folders: every list made before is out of date. */
  outdate(id) {
    for (const [reviewId, entry] of this.reviews) {
      if (entry.jobId !== id || entry.stale) continue;
      entry.stale = true;
      this.emit("review-stale", { id, reviewId });
    }
  }

  // ---------------------------------------------------------------- looking

  /**
   * Pretends to count both folders, then to compare the files that differ only by date,
   * sending counts as it goes. Throws "Stopped." when cancelled.
   */
  async count(j) {
    const w = this.world(j.id);
    const look = { cancelled: false };
    if (!this.looks.has(j.id)) this.looks.set(j.id, new Set());
    this.looks.get(j.id).add(look);
    this.publish(j);
    try {
      const ticks = 14 + Math.round(Math.random() * 12);
      for (let t = 1; t <= ticks; t++) {
        await wait(90);
        if (look.cancelled) throw "Stopped.";
        for (const side of ["source", "destination"]) {
          const share = Math.min(1, (t / ticks) * (side === "source" ? 1.08 : 1));
          this.emit("scan-progress", { id: j.id, side, items: Math.round(w.files[side] * share) });
        }
      }
      const compare = w.compare;
      for (let t = 1; compare && t <= 10; t++) {
        await wait(110);
        if (look.cancelled) throw "Stopped.";
        this.emit("compare-progress", {
          id: j.id, files: Math.round((compare.files * t) / 10), totalFiles: compare.files,
          bytes: Math.round((compare.bytes * t) / 10), totalBytes: compare.bytes,
        });
      }
      this.checked.add(j.id);
    } finally {
      this.looks.get(j.id).delete(look);
      if (this.jobs.includes(j)) this.publish(j);
    }
  }

  /** Why a job can't be looked at or synced now, if it can't. */
  refusal(j) {
    if (!this.online(j)) return j.problem ?? "One of the folders isn't reachable.";
    if (this.syncing(j)) return `“${j.name}” is syncing. Wait until it finishes, then look again.`;
    const other = this.jobs.find(o => o !== j && this.syncing(o) && [o.source, o.destination].some(p => p === j.source || p === j.destination));
    if (other) return `“${other.name}” uses one of these folders and is syncing. Wait until it finishes, then look again.`;
    return null;
  }

  async prepareReview(id) {
    const j = this.find(id);
    const refused = this.refusal(j);
    if (refused) throw refused;
    await this.count(j);
    const w = this.world(id);
    if (!w.looked) {
      w.looked = true;
      w.since = now();
      this.publish(j);
    }
    return this.makeReview(j, { steps: w.steps, notes: w.notes });
  }

  /** Builds a review and keeps its whole list for paging. */
  makeReview(j, { steps, notes, kind = "sync", title = null, warnings, settles = null }) {
    const w = this.world(j.id);
    const sorted = [...steps].sort((a, b) => (b.danger - a.danger) || (KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind)) || a.path.localeCompare(b.path));
    const clusters = sorted
      .filter(s => s.kind === "remove" && s.item === "dir")
      .map(s => {
        const inside = sorted.filter(o => o.kind === "remove" && o.side === s.side && o.path.startsWith(`${s.path}/`));
        return { side: s.side, path: s.path, items: inside.length, bytes: inside.reduce((n, o) => n + o.bytes, 0) };
      })
      .sort((a, b) => b.items - a.items);
    const trash = this.settings.trashDays;
    const info = j.safeDelete && sorted.some(s => s.danger)
      ? [{ level: "info", text: `Removed and replaced files go to .charon-trash on the same drive and are kept ${trash ? `for ${trash} days` : "until you empty it"}.` }]
      : [];
    const preparedAt = new Date();
    const review = {
      reviewId: `review-${++this.reviewCount}`,
      jobId: j.id,
      jobName: j.name,
      kind,
      title,
      mode: j.mode,
      safeDelete: j.safeDelete,
      fullAutomation: j.fullAutomation,
      preparedAt: preparedAt.toISOString(),
      expiresAt: new Date(preparedAt.getTime() + REVIEW_LIFETIME).toISOString(),
      source: { path: j.source, volume: j.sourceVolume, files: w.files.source },
      destination: { path: j.destination, volume: j.destinationVolume, files: w.files.destination },
      tally: tallyOf(sorted, kind === "sync" ? w.agreed : 0),
      warnings: [...(warnings ?? w.warnings), ...info],
      blocked: kind === "sync" ? w.blocked ?? null : null,
      needsTick: kind === "sync" && sorted.some(s => s.danger),
      confirmAnyway: kind === "sync" && !sorted.length ? this.confirmAnyway(j, w) : null,
      clusters,
      housekeeping: kind === "sync" ? w.housekeeping : [],
      totalSteps: sorted.length,
      totalNotes: notes.length,
      steps: sorted.slice(0, 200),
      notes: notes.slice(0, 100),
    };
    this.reviews.set(review.reviewId, { review, steps: sorted, notes, jobId: j.id, setup: setupKey(j), stale: false, settles });
    return review;
  }

  /** Why a list without steps is still worth confirming, as Rust says it (null: it only says both folders match). */
  confirmAnyway(j, w) {
    if (j.fullAutomation && (j.needsReview || w.held)) return "Confirm once so Charon may sync this job by itself from now on.";
    if (w.held) return "Confirm it, and the job stops waiting for your OK.";
    if (j.lastSynced) return null;
    return j.mode === "two-way"
      ? "Confirm it, and Charon remembers both folders as they are now, so that later lists can carry what you remove on one side to the other."
      : "Confirm it as the job's first sync, so Charon knows both folders from now on.";
  }

  /** A review of settling one conflict, exactly as the person saw it. */
  prepareResolution(id, conflict, choice) {
    const j = this.find(id);
    const refused = this.refusal(j);
    if (refused) throw refused;
    const known = j.conflicts.find(c => c.key === conflict.key);
    if (!known || JSON.stringify(known) !== JSON.stringify(conflict)) throw "It changed since you saw it. Look again.";
    if (choice === "keep-both" && !(known.sourceKind === "file" && known.destinationKind === "file")) throw "Both versions can only be kept when both are files.";
    const keep = choice === "keep-source" ? "source" : "destination";
    const other = keep === "source" ? "destination" : "source";
    const kept = known[`${keep}Kind`];
    const lost = known[`${other}Kind`];
    const item = kept ?? lost ?? "file";
    const bytes = known[`${keep}Size`] ?? 0;
    let steps;
    if (choice === "keep-both") {
      steps = [step("keep-both", null, known.path, known.sourceSize + known.destinationSize, "both versions are kept: the destination's gets a “(conflict 2026-10-20)” name")];
    } else if (kept == null) {
      steps = [step("remove", other, known.path, known[`${other}Size`] ?? 0, `removed on the ${keep}, and you chose the ${keep}'s side`, lost === "dir" ? "dir" : "file")];
    } else if (lost == null) {
      steps = [step("new", other, known.path, bytes, `you chose the ${keep}'s version`, item)];
    } else {
      steps = [step("replace", other, known.path, bytes, `you chose the ${keep}'s version; the ${other}'s goes to .charon-trash`, item)];
    }
    const titles = { "keep-source": "keep the source's version", "keep-destination": "keep the destination's version", "keep-both": "keep both versions" };
    return this.makeReview(j, {
      kind: "resolve",
      title: `Settle “${known.path}”: ${titles[choice]}`,
      steps,
      notes: [],
      warnings: [],
      settles: known.key,
    });
  }

  /** The review, if it's still there and belongs to that job. */
  entry(id, reviewId) {
    const entry = this.reviews.get(reviewId);
    if (!entry || entry.jobId !== id) throw "This list is gone. Look again.";
    return entry;
  }

  approve(id, reviewId, acknowledged) {
    const entry = this.entry(id, reviewId);
    const j = this.find(id);
    const review = entry.review;
    if (Date.now() > Date.parse(review.expiresAt)) throw "This list is more than 15 minutes old. Look again.";
    if (entry.setup !== setupKey(j)) throw "The job changed since this list was made. Look again.";
    const refused = this.refusal(j);
    if (refused) throw refused;
    if (entry.stale) throw "Something changed in the folders since this list was made. Look again.";
    if (review.blocked) throw review.blocked;
    if (review.needsTick && !acknowledged) {
      throw `Tick “${review.totalSteps ? "I've looked at the removals and replacements" : "I've read the warnings above"}” first.`;
    }
    if (this.refuseOnce) {
      this.refuseOnce = false;
      throw "The destination drive was replaced by another one with the same name. Look again.";
    }
    this.reviews.delete(reviewId);
    if (review.kind === "resolve") {
      this.log(j, `you settled “${entry.steps[0].path}”`);
      this.sync(j, entry.steps, { approved: true, settles: entry.settles });
    } else {
      this.log(j, `you confirmed ${entry.steps.length} changes`);
      this.sync(j, entry.steps.filter(s => s.kind !== "conflict"), { approved: true });
    }
    return { kind: "started" };
  }

  // ---------------------------------------------------------------- syncing

  /** Pretends to carry out `steps`, with live progress. */
  sync(j, steps, { approved = false, settles = null } = {}) {
    if (this.timers.has(j.id)) return;
    const w = this.world(j.id);
    const work = steps.filter(s => s.kind !== "conflict");
    const items = Math.max(1, work.length);
    const bytes = work.reduce((n, s) => n + s.bytes, 0);
    const startedAt = now();
    // The list is on its way now: it no longer waits.
    w.steps = w.steps.filter(s => !work.includes(s));
    this.timers.set(j.id, null);
    j.progress = { phase: "scanning", doneItems: 0, totalItems: 0, doneBytes: 0, totalBytes: 0, current: null };
    this.publish(j);
    this.log(j, approved ? "sync confirmed by you" : "sync by itself (full automation)");
    this.count(j).catch(() => {}).then(() => {
      if (!this.timers.has(j.id)) return; // stopped while looking
      let done = 0;
      j.progress = { phase: "copying", doneItems: 0, totalItems: items, doneBytes: 0, totalBytes: bytes, current: work[0]?.path ?? null };
      this.emit("progress", { id: j.id, progress: j.progress });
      const timer = setInterval(() => {
        done = Math.min(items, done + Math.ceil(items / 60));
        const doneBytes = work.slice(0, done).reduce((n, s) => n + s.bytes, 0);
        j.progress = { ...j.progress, doneItems: done, doneBytes, current: work[done - 1]?.path ?? null };
        this.emit("progress", { id: j.id, progress: j.progress });
        if (done >= items) this.finish(j, work, { startedAt, bytes, approved, settles });
      }, 110);
      this.timers.set(j.id, timer);
    });
  }

  finish(j, work, { startedAt, bytes, approved, settles }) {
    clearInterval(this.timers.get(j.id));
    this.timers.delete(j.id);
    const w = this.world(j.id);
    if (settles) {
      j.conflicts = j.conflicts.filter(c => c.key !== settles);
      w.steps = w.steps.filter(s => !(s.kind === "conflict" && j.conflicts.every(c => c.path !== s.path)));
    }
    j.lastSynced = now();
    j.needsReview = false;
    j.stale = null;
    w.held = null;
    w.since = null;
    if (!w.steps.some(s => s.kind !== "conflict")) {
      // Everything crossed: what the warnings and the housekeeping line spoke of is done.
      w.looked = false;
      this.forgetList(w);
    }
    const copied = work.filter(s => ["new", "replace", "keep-both", "settle"].includes(s.kind)).length;
    const deleted = work.filter(s => s.kind === "remove").length;
    j.lastRun = run({ startedAt, finishedAt: j.lastSynced, copied, deleted, bytes, conflicts: j.conflicts.length, approved });
    this.log(j, `copied ${copied} files (${(bytes / 1e6).toFixed(1)} MB), removed ${deleted} items`);
    this.publish(j);
    this.emit("finished", { id: j.id, name: j.name, summary: j.lastRun });
  }

  stop(j) {
    if (!this.timers.has(j.id)) return;
    clearInterval(this.timers.get(j.id));
    this.timers.delete(j.id);
    const w = this.world(j.id);
    // What didn't cross yet waits for an OK again.
    const roll = dice(Date.now() % 1000 + 1);
    const left = Math.round(10 + roll() * 30);
    for (let i = 0; i < left; i++) w.steps.push(step("new", "destination", `Left over/file-${String(i + 1).padStart(3, "0")}.dat`, Math.round(roll() * 2_000_000), "new on the source"));
    this.forgetList(w); // the old warnings spoke of the list that was on its way
    w.looked = true;
    w.since = now();
    j.needsReview = true;
    j.lastRun = run({ startedAt: ago(20 * SECOND), finishedAt: now(), cancelled: true, approved: true });
    this.log(j, "stopped by you; what didn't cross yet waits for your OK", "warn");
    this.publish(j);
  }

  /**
   * Forgets what was said about a list that crossed (or was stopped): its warnings, the
   * housekeeping line, files that only differed in their dates. Notes about the job stay.
   */
  forgetList(w) {
    Object.assign(w, {
      warnings: w.warnings.filter(warning => warning.level === "info"),
      housekeeping: [],
      agreed: 0,
      blocked: null,
      held: null,
    });
  }

  // ---------------------------------------------------------------- commands

  async invoke(command, args = {}) {
    await wait(30);
    switch (command) {
      case "get_state":
        this.lookAtEverything();
        return structuredClone({ jobs: this.jobs, settings: this.settings, overview: this.overview(), defaultExcludes: DEFAULT_EXCLUDES });
      case "about": return { name: "Charon", version: "0.3.0", appWebsite: "https://charonsync.app", sourceCode: "https://github.com/steanart/charon", developer: "stean.DEV", developerWebsite: "https://stean.dev", developerEmail: "a.s@stean.dev", licence: "MIT", copyrightYear: "2026" };
      case "update_info": return null;
      case "check_updates": {
        // Pretends GitHub has a newer version, to show how it looks.
        await wait(900);
        const info = { current: "0.3.0", latest: "0.3.1", available: true, page: "https://github.com/steanart/charon/releases/tag/v0.3.1", download: null };
        this.emit("update", info);
        return info;
      }
      case "upgrade_notice": {
        if (this.noticeSeen || readFlag("charon-demo-notice-seen")) return null;
        return {
          version: "0.3.0",
          jobs: ["projects", "documents", "archive"].map(id => {
            const j = this.find(id);
            return { id, name: j.name, trigger: j.trigger };
          }),
        };
      }
      case "dismiss_upgrade_notice":
        this.noticeSeen = true;
        writeFlag("charon-demo-notice-seen");
        return null;

      case "sync_now": {
        const j = this.find(args.id);
        const refused = this.refusal(j);
        if (refused) throw refused;
        if (!j.runsByItself || j.waiting) return "review";
        this.sync(j, this.world(j.id).steps);
        return "started";
      }
      case "sync_all": {
        const waiting = [];
        for (const j of this.jobs) {
          if (j.paused || !this.online(j)) continue;
          if (j.runsByItself && !j.waiting) {
            if (!this.syncing(j)) this.sync(j, this.world(j.id).steps);
          } else {
            waiting.push(j.id);
          }
        }
        return waiting;
      }
      case "cancel": return this.stop(this.find(args.id));
      case "check": {
        const j = this.find(args.id);
        const refused = this.refusal(j);
        if (refused) throw refused;
        await this.count(j);
        const w = this.world(j.id);
        if (!w.steps.some(s => s.kind !== "conflict")) {
          w.steps.push(
            step("new", "destination", "Inbox/note-from-today.md", 3_400, "new on the source"),
            step("new", "destination", "Inbox/photo-of-whiteboard.jpg", 2_800_000, "new on the source"),
          );
          this.outdate(j.id);
        }
        w.looked = true;
        w.since ??= now();
        // It only looks, as Rust does: a job that runs by itself syncs when its schedule comes due.
        this.log(j, "looked for changes");
        this.publish(j);
        return null;
      }

      case "prepare_review": return this.prepareReview(args.id);
      case "cancel_review": {
        for (const look of this.looks.get(args.id) ?? []) look.cancelled = true;
        return null;
      }
      case "review_steps": {
        const entry = this.entry(args.id, args.reviewId);
        const kinds = FILTER_KINDS[args.filter];
        const query = String(args.query ?? "").toLowerCase();
        const folder = args.folder ? String(args.folder).replace(/\/+$/, "") : null;
        const inside = path => !folder || path === folder || path.startsWith(`${folder}/`);
        const list = entry.steps.filter(s => (!kinds || kinds.includes(s.kind)) && inside(s.path) && (!query || s.path.toLowerCase().includes(query)));
        await wait(60 + Math.random() * 120);
        return { steps: list.slice(args.offset, args.offset + Math.min(500, args.limit)), total: list.length };
      }
      case "review_notes": {
        const entry = this.entry(args.id, args.reviewId);
        return { notes: entry.notes.slice(args.offset, args.offset + Math.min(500, args.limit)), total: entry.notes.length };
      }
      case "approve": return this.approve(args.id, args.reviewId, args.acknowledged);
      case "discard_review":
        this.reviews.delete(args.reviewId);
        return null;
      case "prepare_resolution": {
        await wait(400);
        return this.prepareResolution(args.id, args.conflict, args.choice);
      }
      case "leave_out": {
        const j = this.find(args.id);
        const w = this.world(j.id);
        const pattern = `/${args.path}`;
        if (!j.excludes.includes(pattern)) j.excludes.push(pattern);
        const inside = path => path === args.path || path.startsWith(`${args.path}/`);
        j.unreadable = j.unreadable.filter(p => !(p.side === args.side && inside(p.path)));
        w.notes = w.notes.filter(n => !inside(n.path));
        w.steps = w.steps.filter(s => !inside(s.path));
        j.needsReview = true;
        this.log(j, `left out ${args.path} on the ${args.side}`);
        this.outdate(j.id);
        this.publish(j);
        return structuredClone(j);
      }

      case "set_paused": {
        const j = this.find(args.id);
        j.paused = args.paused;
        return this.publish(j);
      }
      case "set_all_paused":
        this.settings = { ...this.settings, pausedAll: args.paused };
        this.emit("settings", this.settings);
        for (const j of this.jobs) this.publish(j);
        this.emit("overview", this.overview());
        return null;
      case "set_settings":
        // `pausedAll` only changes through set_all_paused.
        this.settings = { ...args.settings, pausedAll: this.settings.pausedAll };
        this.emit("settings", this.settings);
        this.emit("overview", this.overview());
        return null;
      case "job_log": return structuredClone(this.logs.get(args.id) ?? []);
      case "pick_folder": return prompt("Folder path (demo):", args.start || "/Users/andrei/");
      case "suggest_name": {
        const base = p => String(p).split(/[\\/]/).filter(Boolean).pop() || p;
        return `${base(args.source)} ${args.mode === "two-way" ? "⇄" : "→"} ${base(args.destination)}`;
      }
      case "add_job": {
        if (!args.setup.source || !args.setup.destination) throw "Choose both a source and a destination folder.";
        const j = job(`job${Date.now()}`, {
          ...args.setup, name: args.setup.name || "New job", lastSynced: null, lastRun: null, needsReview: true,
          destinationVolume: volumeOf(args.setup.destination), sourceVolume: volumeOf(args.setup.source),
        });
        this.jobs.push(j);
        this.worlds.set(j.id, freshWorld(j));
        this.logs.set(j.id, [{ time: now(), level: "info", text: "job created" }]);
        this.publish(j);
        return structuredClone(j);
      }
      case "update_job": {
        const j = this.find(args.id);
        if (this.syncing(j)) throw `Wait until “${j.name}” finishes syncing, then save your changes.`;
        if (!args.setup.source || !args.setup.destination) throw "Choose both a source and a destination folder.";
        const before = setupKey(j);
        const turnedOn = !j.fullAutomation && args.setup.fullAutomation;
        const moved = j.source !== args.setup.source || j.destination !== args.setup.destination;
        Object.assign(j, args.setup);
        if (moved) {
          j.sourceVolume = volumeOf(j.source);
          j.destinationVolume = volumeOf(j.destination);
          this.worlds.set(j.id, freshWorld(j));
        }
        if (before !== setupKey(j) || turnedOn) {
          j.needsReview = true;
          this.outdate(j.id);
          this.log(j, "settings changed: the next sync waits for your OK");
        }
        this.publish(j);
        return structuredClone(j);
      }
      case "duplicate_job": {
        const original = this.find(args.id);
        const j = job(`job${Date.now()}`, {
          ...structuredClone(original), id: `job${Date.now()}`, name: `${original.name} copy`, rev: 0,
          fullAutomation: false, needsReview: true, lastSynced: null, lastRun: null, conflicts: [], unreadable: [], stale: null,
        });
        this.jobs.push(j);
        this.worlds.set(j.id, freshWorld(j));
        this.logs.set(j.id, [{ time: now(), level: "info", text: `job created as a copy of “${original.name}”` }]);
        this.publish(j);
        return structuredClone(j);
      }
      case "remove_job": {
        const j = this.find(args.id);
        if (this.syncing(j)) throw `Wait until “${j.name}” finishes syncing, then delete it.`;
        this.jobs = this.jobs.filter(other => other !== j);
        this.emit("job-removed", args.id);
        this.emit("overview", this.overview());
        return null;
      }
      case "move_job": {
        const j = this.find(args.id);
        this.jobs = this.jobs.filter(other => other !== j);
        this.jobs.splice(Math.max(0, Math.min(this.jobs.length, args.toIndex)), 0, j);
        return null;
      }
      case "reveal_path":
        console.info(`Charon demo: would show ${args.path}`);
        return null;
      case "open_url":
        window.open(args.url, "_blank", "noopener");
        return null;
      case "open_privacy_settings":
        console.info("Charon demo: would open System Settings › Privacy & Security › Full Disk Access");
        return null;
      case "quit_app":
        console.info("Charon demo: Charon would quit now.");
        return null;
      default:
        throw `Unknown command ${command}`;
    }
  }

  /** The tray and a few surprises, for the browser console. */
  console() {
    return {
      syncAll: () => this.emit("sync-all"),
      showWaiting: () => this.emit("show-waiting"),
      quit: () => this.emit("confirm-quit", {
        jobs: this.jobs.filter(j => j.waiting && !onlyToCompare(j.waiting)).length,
        toCompare: this.jobs.filter(j => j.waiting && onlyToCompare(j.waiting)).length,
      }),
      quitting: () => {
        const running = this.jobs.filter(j => this.syncing(j)).map(j => j.name);
        this.emit("quitting", { jobs: running.length ? running : [this.jobs[0]?.name ?? "Projects"] });
      },
      change: id => this.outdate(id),
      refuseNext: () => { this.refuseOnce = true; },
    };
  }
}

/** Adds up a list of steps the way the review's tally does. */
function tallyOf(steps, agreed) {
  const side = () => ({ newFiles: 0, replaced: 0, removed: 0, removedFolders: 0, newFolders: 0, bytesIn: 0 });
  const t = { source: side(), destination: side(), keepBoth: 0, conflicts: 0, newestWins: 0, agreed, bytes: 0 };
  for (const s of steps) {
    const st = s.side ? t[s.side] : null;
    switch (s.kind) {
      case "new":
        if (s.item === "dir") st.newFolders++;
        else st.newFiles++;
        st.bytesIn += s.bytes;
        t.bytes += s.bytes;
        break;
      case "folder": st.newFolders++; break;
      case "replace":
        st.replaced++;
        st.bytesIn += s.bytes;
        t.bytes += s.bytes;
        break;
      case "remove":
        if (s.item === "dir") st.removedFolders++;
        else st.removed++;
        break;
      case "settle":
        // Counted among the replaced files of the side it replaces too, as Rust does.
        t.newestWins++;
        if (st) {
          st.replaced++;
          st.bytesIn += s.bytes;
        }
        t.bytes += s.bytes;
        break;
      case "keep-both":
        t.keepBoth++;
        t.bytes += s.bytes;
        break;
      case "conflict": t.conflicts++; break;
    }
  }
  return t;
}

/** "/Volumes/Backup SSD/Photos" is on a drive called "Backup SSD". */
function volumeOf(path) {
  const match = /^\/Volumes\/([^/]+)/.exec(path ?? "");
  return match ? { name: match[1], kind: "APFS" } : null;
}

/** A flag that survives a reload of the demo, where the browser allows it. */
function readFlag(name) {
  try {
    return sessionStorage.getItem(name) === "1";
  } catch {
    return false;
  }
}

function writeFlag(name) {
  try {
    sessionStorage.setItem(name, "1");
  } catch {
    // Not kept; the notice shows again after a reload.
  }
}
