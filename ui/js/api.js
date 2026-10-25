// Talking to the Rust side. Every command and event the window uses is listed here.
//
// Events (listen with `Api.on`):
//   job            a job's view; each carries `rev`, and an older `rev` than the one known is ignored
//   job-removed    a job's id
//   progress       {id, progress} while a job syncs
//   overview       the numbers for the title bar
//   finished       {id, name, summary} when a sync ends
//   settings       the settings, whenever they change (the only source of `pausedAll`)
//   scan-progress  {id, side, items}: items counted so far while preparing a review or scanning
//   review-stale   {id, reviewId}: something changed in the folders after that list was made
//   sync-all       the tray's "Sync All…"
//   show-waiting   the tray's "Review waiting jobs…"
//   confirm-quit   {jobs, toCompare}: the tray's Quit while jobs wait for an OK, or have files to compare
//   update         news about a new version of Charon
//
// Opened in a plain browser (no Tauri), the window talks to a simulated backend with
// example jobs instead, which is handy for working on the interface.

let backend = window.__TAURI__ ?? null;

export class Api {
  /** The real Rust side, or the simulation outside the app. */
  static async backend() {
    if (!backend) backend = (await import("./demo/backend.js")).DemoBackend.create();
    return backend;
  }

  /** Calls a Rust command. */
  static async call(command, args) {
    const tauri = await this.backend();
    return tauri.core.invoke(command, args);
  }

  /** Runs `handler(payload)` whenever the Rust side sends `event`. */
  static async on(event, handler) {
    const tauri = await this.backend();
    return tauri.event.listen(event, ({ payload }) => handler(payload));
  }

  // ---- reading
  static getState() { return this.call("get_state"); }
  static about() { return this.call("about"); }
  static updateInfo() { return this.call("update_info"); }
  static checkUpdates() { return this.call("check_updates"); }
  static jobLog(id) { return this.call("job_log", { id }); }
  static suggestName(source, destination, mode) { return this.call("suggest_name", { source, destination, mode }); }
  static upgradeNotice() { return this.call("upgrade_notice"); }
  static dismissUpgradeNotice() { return this.call("dismiss_upgrade_notice"); }

  // ---- jobs
  static addJob(setup) { return this.call("add_job", { setup }); }
  static updateJob(id, setup) { return this.call("update_job", { id, setup }); }
  static duplicateJob(id) { return this.call("duplicate_job", { id }); }
  static removeJob(id) { return this.call("remove_job", { id }); }
  static moveJob(id, toIndex) { return this.call("move_job", { id, toIndex }); }
  static setPaused(id, paused) { return this.call("set_paused", { id, paused }); }
  static setAllPaused(paused) { return this.call("set_all_paused", { paused }); }
  /** "Leave this folder out": an exclude for exactly that place. Returns the job's view. */
  static leaveOut(id, side, path) { return this.call("leave_out", { id, side, path }); }

  // ---- running
  /** "started", or "review" when the job waits for the person's OK. */
  static syncNow(id) { return this.call("sync_now", { id }); }
  /** Starts the jobs that run by themselves; returns the ids of those that wait for an OK. */
  static syncAll() { return this.call("sync_all"); }
  static cancel(id) { return this.call("cancel", { id }); }
  static check(id) { return this.call("check", { id }); }

  // ---- the review: the list of changes, shown before anything is touched
  static prepareReview(id) { return this.call("prepare_review", { id }); }
  static cancelReview(id) { return this.call("cancel_review", { id }); }
  static reviewSteps(id, reviewId, filter, query, offset, limit, folder = null) { return this.call("review_steps", { id, reviewId, filter, query, folder, offset, limit }); }
  static reviewNotes(id, reviewId, offset, limit) { return this.call("review_notes", { id, reviewId, offset, limit }); }
  static approve(id, reviewId, acknowledged) { return this.call("approve", { id, reviewId, acknowledged }); }
  static discardReview(id, reviewId) { return this.call("discard_review", { id, reviewId }); }
  /** A review of settling one conflict: `choice` is "keep-source", "keep-destination" or "keep-both". */
  static prepareResolution(id, conflict, choice) { return this.call("prepare_resolution", { id, conflict, choice }); }

  // ---- settings
  static setSettings(settings) { return this.call("set_settings", { settings }); }

  // ---- the desktop
  static pickFolder(start) { return this.call("pick_folder", { start: start || null }); }
  static reveal(path) { return this.call("reveal_path", { path }); }
  static openUrl(url) { return this.call("open_url", { url }); }
  static openPrivacySettings() { return this.call("open_privacy_settings"); }
  static quitApp() { return this.call("quit_app"); }
}
