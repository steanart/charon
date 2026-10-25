// Charon's window: creates the views and connects them to the Rust side.

import { Api } from "./api.js";
import { Dom } from "./dom.js";
import { paintIcons } from "./icons.js";
import { Platform } from "./platform.js";
import { Theme } from "./theme.js";
import { JobStore, onlyToCompare } from "./store.js";
import { Toast } from "./toast.js";
import { River } from "./effects/river.js";
import { Header } from "./views/header.js";
import { JobList } from "./views/job-list.js";
import { JobEditor } from "./views/job-editor.js";
import { ReviewDialog } from "./views/review-dialog.js";
import { WaitingSheet } from "./views/waiting-sheet.js";
import { UpgradeSheet } from "./views/upgrade-sheet.js";
import { SettingsPanel } from "./views/settings-panel.js";
import { AboutPanel } from "./views/about-panel.js";
import { PopMenu } from "./views/pop-menu.js";
import { Modal } from "./views/modal.js";
import { Overlay } from "./views/overlay.js";
import { Sheet } from "./views/sheet.js";
import { Updates } from "./updates.js";

const WAITING_LATER = "The jobs waiting for your OK open once you've closed this.";

class App {
  constructor() {
    this.store = new JobStore();
    this.toast = new Toast(Dom.$("#toast"));
    this.river = new River(Dom.$("#river"));
    this.menu = new PopMenu(Dom.$("#menu"));
    this.header = new Header(this.store, { pickGroup: group => this.pickGroup(group) });
    this.list = new JobList(this.store, {
      newJob: () => this.editor.create(),
      act: (action, id, target) => this.act(action, id, target),
    });
    this.editor = new JobEditor(this.store, this.toast, { saved: (view, info) => this.saved(view, info) });
    this.review = new ReviewDialog(this.store, this.toast, { openConflicts: id => this.openTab(id, "conflicts") });
    this.waiting = new WaitingSheet(this.store, this.review);
    this.upgrade = new UpgradeSheet(this.store, this.toast, {
      automate: id => this.editor.edit(id, { automation: true, over: true }),
      review: id => this.review.show(id),
    });
    this.settings = new SettingsPanel(this.store, this.toast);
    this.updates = new Updates(this.toast);
    this.about = new AboutPanel(this.updates, this.toast);
    this.later = null;   // a sheet asked for while a question or the review was open
    this.quittingShown = false;
    this.store.subscribe(hint => this.render(hint));
    // Once the closing code has finished (a second warning may follow the first), see if it can open now.
    Overlay.afterClose(() => setTimeout(() => this.runLater()));
  }

  /** Loads the state, then listens for changes. */
  async start() {
    Platform.markBody();
    paintIcons();
    this.river.start();
    this.bindControls();
    this.bindShortcuts();
    await this.listenToRust();
    await this.reload();
    await this.updates.start();
    await this.upgrade.start();
    setInterval(() => this.store.changed(), 30_000); // keeps "4 min ago" fresh
  }

  /** Redraws what changed. */
  render(hint) {
    if (hint.scanOnly) return this.list.render(hint);
    Theme.apply(this.store.settings?.appearance);
    this.header.render();
    paintIcons(Dom.$(".titlebar"));
    this.list.render(hint);
    if (hint.settings) this.settings.refresh();
    this.updateRiver();
  }

  /** The boat crosses with the combined progress of every running job. */
  updateRiver() {
    const running = this.store.all().filter(job => job.status === "syncing");
    let done = 0, total = 0;
    for (const job of running) {
      done += job.progress?.doneBytes || job.progress?.doneItems || 0;
      total += job.progress?.totalBytes || job.progress?.totalItems || 0;
    }
    this.river.setActivity(running.length > 0, total ? done / total : 0.05);
  }

  async reload() {
    try {
      this.store.load(await Api.getState());
    } catch (error) {
      this.toast.error(error);
    }
  }

  // ---------------------------------------------------------------- actions

  /** Runs an API call and shows its error, if any. */
  async run(work) {
    try {
      return await work();
    } catch (error) {
      this.toast.error(error);
    }
  }

  /**
   * "Sync now" for a job that runs by itself. Rust may still want an OK first; then the
   * review opens. The row turns to syncing when Rust says so, never before.
   */
  async syncNow(id) {
    const result = await this.run(() => Api.syncNow(id));
    if (result === "review") this.review.show(id);
  }

  /** The row's main button: Stop (a sync, or a confirmed one that waits), Sync now, or Review and sync. */
  mainAction(job) {
    if (job.status === "syncing" || job.confirmed) return this.run(() => Api.cancel(job.id));
    if (job.runsByItself && !job.waiting) return this.syncNow(job.id);
    return this.review.show(job.id);
  }

  /**
   * Sync all: Rust starts the jobs that run by themselves and names the others, which
   * wait for an OK; those open in the waiting sheet. From the tray it can come at any
   * time: while a question or the review is open, it waits until they're closed.
   */
  async syncAll() {
    if (!this.sheetCanOpen()) return this.postpone(() => this.syncAll(), "Sync all starts once you've closed this.");
    const ready = job => !job.paused && job.sourceOnline && job.destinationOnline;
    const starting = this.store.all().filter(job => ready(job) && job.runsByItself && !job.waiting).length;
    const ids = await this.run(() => Api.syncAll());
    if (!Array.isArray(ids)) return;
    if (ids.length) return this.openWaiting(ids, { started: starting });
    // Some may first wait for another job that syncs the same folders.
    this.toast.show(starting ? "Every job that runs by itself is on its way." : "Nothing to sync.");
  }

  /** The waiting sheet with every job whose changes wait for an OK (the tray, the quit question). */
  showWaiting() {
    if (!this.sheetCanOpen()) return this.postpone(() => this.showWaiting(), WAITING_LATER);
    const ids = this.store.waiting().map(job => job.id);
    if (ids.length) this.openWaiting(ids);
    else this.toast.show("No job is waiting for your OK.");
  }

  /** Opens the waiting sheet. An open editor stays underneath, with its draft. */
  openWaiting(ids, options = {}) {
    if (!this.sheetCanOpen()) return this.postpone(() => this.openWaiting(ids, options), WAITING_LATER);
    this.waiting.show(ids, { ...options, over: this.editor.isOpen });
  }

  /**
   * Whether a sheet may open now. Never over a question or the review: Esc must close a
   * warning first, then the review, then a sheet.
   */
  sheetCanOpen() {
    return !this.editor.asking && Overlay.stack.every(overlay => overlay instanceof Sheet);
  }

  /** Keeps a request for a sheet until the question or the review on top is closed. */
  postpone(work, message) {
    this.later = work;
    this.toast.show(message);
  }

  runLater() {
    if (!this.later || !this.sheetCanOpen()) return;
    const work = this.later;
    this.later = null;
    work();
  }

  /**
   * The tray's Quit while jobs wait: review them, or quit anyway. Files a look couldn't
   * compare yet are no changes that haven't crossed: they're told apart.
   */
  async confirmQuit({ jobs, toCompare } = {}) {
    const waiting = this.store.waiting();
    const changes = jobs ?? waiting.filter(job => !onlyToCompare(job.waiting)).length;
    const compare = toCompare ?? waiting.length - changes;
    const more = compare === 1 ? "1 more job has" : `${compare} more jobs have`;
    const answer = await Modal.ask({
      title: changes
        ? (changes === 1 ? "1 job has changes that haven't crossed yet." : `${changes} jobs have changes that haven't crossed yet.`)
        : (compare === 1 ? "1 job has files that aren't compared yet." : `${compare} jobs have files that aren't compared yet.`),
      body: changes
        ? `Nothing crosses while Charon is closed. The changes stay where they are until you confirm them.${compare ? ` ${more} files that aren't compared yet: their copies on the two sides may differ.` : ""}`
        : "Their copies on the two sides may differ. Charon compares them when you look at the list, and nothing crosses while it's closed.",
      buttons: [
        { label: "Quit anyway", value: "quit" },
        { label: "Review now", value: "review", kind: "primary", focus: true },
      ],
    });
    if (answer === "review") this.showWaiting();
    if (answer === "quit") this.run(() => Api.quitApp());
  }

  /**
   * Charon is quitting and stops running syncs first, at a safe point: says what it
   * waits for, so it doesn't look stuck. The app closes by itself.
   */
  quitting({ jobs = [] } = {}) {
    if (this.quittingShown || !jobs.length) return;
    this.quittingShown = true;
    const names = jobs.map(name => `“${name}”`);
    const list = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names.at(-1)}` : names[0];
    Modal.ask({
      title: "Stopping safely…",
      body: `Charon stops the sync of ${list} at a safe point, then quits. A file it was copying stays as it was, and what already crossed is recorded, so nothing is lost.`,
      buttons: [],
    });
  }

  /** A job was saved in the editor. A new job opens its review: look before crossing. */
  saved(view, { isNew, message }) {
    this.store.put(view);
    if (isNew) this.review.show(view.id);
    else this.toast.show(message);
  }

  pickGroup(group) {
    this.store.group = this.store.group === group ? "all" : group;
    this.store.changed();
  }

  /** A button or click inside a job row. */
  act(action, id, target) {
    const job = this.store.jobs.get(id);
    if (!job) return;
    const syncing = job.status === "syncing";
    switch (action) {
      case "toggle": return this.toggleRow(id);
      case "main": return this.mainAction(job);
      case "review": return this.review.show(id);
      case "unreadable": return this.openTab(id, "unreadable");
      case "not-synced": return this.openTab(id, "not-synced");
      case "pause": return this.run(() => Api.setPaused(id, true));
      case "resume": return this.run(() => Api.setPaused(id, false));
      case "folders": return this.menu.show(target, [
        { label: `Show source in ${Platform.fileManager()}`, icon: "folder", run: () => this.run(() => Api.reveal(job.source)) },
        { label: `Show destination in ${Platform.fileManager()}`, icon: "folder", run: () => this.run(() => Api.reveal(job.destination)) },
      ]);
      case "more": return this.menu.show(target, [
        { label: "Review changes…", icon: "review", run: () => this.review.show(id) },
        { label: "Look for changes", icon: "search", run: () => this.lookForChanges(job) },
        { label: "Activity log", icon: "log", run: () => this.openTab(id, "activity") },
        ...this.trashItems(job),
        "-",
        { label: "Edit…", icon: "edit", disabled: syncing, title: syncing ? "Wait until it finishes syncing" : "", run: () => this.editor.edit(id) },
        { label: "Duplicate", icon: "copy", run: () => this.run(() => Api.duplicateJob(id)) },
        "-",
        { label: "Delete…", icon: "trash", danger: true, disabled: syncing, title: syncing ? "Wait until it finishes syncing" : "", run: () => this.removeJob(job) },
      ]);
      case "tab": return this.openTab(id, target.dataset.tab, { reveal: false });
      case "resolve": return this.resolve(job, target);
      case "leave-out": return this.leaveOut(job, target.dataset.list, Number(target.dataset.index), target);
      case "privacy": return this.run(() => Api.openPrivacySettings());
    }
  }

  /** "Show removed files": the .charon-trash folder on the destination, and for two-way jobs on the source too. */
  trashItems(job) {
    const trash = folder => {
      const separator = folder.includes("\\") && !folder.includes("/") ? "\\" : "/";
      return `${folder.replace(/[\\/]+$/, "")}${separator}.charon-trash`;
    };
    const show = folder => () => this.run(() => Api.reveal(trash(folder)));
    if (job.mode !== "two-way") return [{ label: "Show removed files", icon: "trash", run: show(job.destination) }];
    return [
      { label: "Show removed files on the destination", icon: "trash", run: show(job.destination) },
      { label: "Show removed files on the source", icon: "trash", run: show(job.source) },
    ];
  }

  async lookForChanges(job) {
    await this.run(() => Api.check(job.id));
  }

  /** Expands or folds a row; an expanded row loads its log. */
  toggleRow(id) {
    if (this.store.toggleOpen(id)) this.loadLog(id);
  }

  /**
   * Expands a row at one of its tabs, and scrolls the row to the top of the list so the
   * tab isn't left under the river. `reveal: false` for a tab clicked in an open row.
   */
  openTab(id, tab, { reveal = true } = {}) {
    if (!this.store.open.has(id)) this.store.toggleOpen(id);
    this.store.setTab(id, tab);
    if (tab === "activity") this.loadLog(id);
    if (reveal) this.list.reveal(id);
  }

  async loadLog(id) {
    const lines = await this.run(() => Api.jobLog(id));
    if (lines) this.store.setLog(id, lines);
  }

  /** A choice on a conflict: the review shows what it would do, and nothing changes until it's confirmed. */
  resolve(job, button) {
    const conflict = job.conflicts.find(c => c.key === button.dataset.key);
    if (conflict) this.review.settle(job.id, conflict, button.dataset.choice);
  }

  /** "Leave this folder out" on a place that couldn't be read, or one that isn't synced. */
  async leaveOut(job, list, index, button) {
    const place = (list === "not-synced" ? job.notSynced : job.unreadable)?.[index];
    if (!place) return;
    button.disabled = true;
    const view = await this.run(() => Api.leaveOut(job.id, place.side, place.path));
    if (view) {
      this.store.put(view);
      this.toast.show("Left out. The next sync waits for your OK.");
    } else {
      button.disabled = false;
    }
  }

  async removeJob(job) {
    const sure = await Modal.confirm({
      title: `Delete “${job.name}”?`,
      body: "The folders and files stay where they are. Only the job is removed.",
      action: "Delete job",
    });
    if (sure) await this.run(() => Api.removeJob(job.id));
  }

  // ---------------------------------------------------------------- wiring

  bindControls() {
    const on = (selector, event, handler) => Dom.$(selector).addEventListener(event, handler);
    on("#btn-new", "click", () => this.editor.create());
    on("#btn-sync-all", "click", () => this.syncAll());
    on("#btn-pause-all", "click", () => this.run(() => Api.setAllPaused(!this.store.pausedAll())));
    on("#btn-settings", "click", () => this.settings.toggleOpen());
    on("#btn-update", "click", () => this.about.show());
    on("#btn-about", "click", () => (this.about.isOpen ? this.about.close() : this.about.show()));
    on("#scrim", "click", () => Overlay.dismissTop());
    on("#search", "input", event => {
      this.store.query = event.target.value.trim();
      this.store.changed();
    });
  }

  /**
   * Esc closes only the topmost thing: the menu, a warning, the review, then a sheet.
   * ⌘N, ⌘, ⌘F and ⌘⇧S (Ctrl on Windows and Linux) work only when no dialog or question is
   * open, and not while the editor is open: they would close it and lose its draft.
   */
  bindShortcuts() {
    document.addEventListener("keydown", event => {
      if (event.key === "Escape") {
        event.preventDefault();
        if (this.menu.isOpen) return this.menu.close();
        return Overlay.dismissTop();
      }
      if (event.key === "Tab") return Overlay.trapTab(event);
      if (!(event.metaKey || event.ctrlKey)) return;
      const top = Overlay.top();
      if (top && (!(top instanceof Sheet) || this.editor.isOpen)) return;
      const key = event.key.toLowerCase();
      const shortcuts = {
        n: () => this.editor.create(),
        ",": () => this.settings.show(),
        f: () => !top && Dom.$("#search").focus(),
        s: () => event.shiftKey && this.syncAll(),
      };
      if (shortcuts[key]) {
        event.preventDefault();
        this.menu.close();
        shortcuts[key]();
      }
    });
  }

  /** Events from the Rust side. */
  async listenToRust() {
    await Api.on("job", view => {
      this.store.put(view);
      if (this.store.open.has(view.id) && this.store.tabOf(view.id) === "activity" && view.status !== "syncing") this.loadLog(view.id);
    });
    await Api.on("job-removed", id => this.store.drop(id));
    await Api.on("progress", ({ id, progress }) => this.store.setProgress(id, progress));
    await Api.on("overview", overview => this.store.setOverview(overview));
    await Api.on("settings", settings => this.store.setSettings(settings));
    await Api.on("scan-progress", ({ id, side, items }) => this.store.setScan(id, side, items));
    await Api.on("compare-progress", ({ id, ...compare }) => this.store.setCompare(id, compare));
    await Api.on("review-stale", stale => {
      this.review.markStale(stale);
      this.waiting.markStale(stale);
    });
    await Api.on("sync-all", () => this.syncAll());
    await Api.on("show-waiting", () => this.showWaiting());
    await Api.on("confirm-quit", payload => this.confirmQuit(payload));
    await Api.on("quitting", payload => this.quitting(payload));
  }
}

new App().start();
