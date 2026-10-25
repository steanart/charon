//! The engine: owns the jobs, looks at their folders, syncs them and tells the window what
//! changed.
//!
//! * `mod.rs`      – `Engine`: opening, settings, reading rows, events
//! * `jobs.rs`     – adding, editing, pausing and removing jobs
//! * `survey.rs`   – looking at both folders and planning, with every safety check
//! * `overlaps.rs` – folders that are one place under two names, in a job or between jobs
//! * `review.rs`   – the list of changes waiting for the person's OK, and confirming it
//! * `runs.rs`     – syncing: confirmed runs, full automation, held-back runs
//! * `checks.rs`   – background looks for changes, and reminders when changes wait
//! * `status.rs`   – turning a job and its live state into the row the window shows
//! * `runtime.rs`  – the live state of each job (running, progress, last look, review)
//! * `reach.rs`    – questions to the disk about the jobs' folders, with a time limit
//! * `quiet.rs`    – drives run by a driver Apple didn't make, kept quiet while a sync writes
//!
//! `Engine` is cheap to clone; all clones share the same jobs. Locks are always taken in
//! the same order, the stored state before the live state. Nothing that looks at a job's
//! folders or tells the window happens while either is held (saving in the app's own data
//! folder does: the state file, so a change counts only once it's saved, what looks found
//! missing, so two saves never come in the wrong order, and the files looks found identical,
//! so a job removed or edited meanwhile keeps nothing of its old folders). What the autopilot
//! and the rows ask about the jobs' folders is asked with a time limit (see `reach.rs`), so a
//! share that doesn't answer holds up neither the other jobs nor the window.

mod checks;
mod jobs;
mod overlaps;
mod quiet;
pub(crate) use quiet::AUTOPILOT_THREAD;
pub(crate) mod reach;
mod review;
mod runs;
mod runtime;
mod status;
mod survey;

pub use review::{Approval, NotePage, StepPage};
pub use runs::SyncStart;

use crate::error::{Error, Result};
use crate::model::{Finished, Job, JobId, JobView, Overview, Progress, Settings, UpgradeNotice, WaitingJob};
use crate::plan::Side;
use crate::run::BusyWatch;
use crate::store::{Absences, Baselines, DataFolder, DataLock, Journal, Landings, LogLine, SameContents, StateFile, StoredState, Unattended};
pub(crate) use reach::{Presence, Whereabouts};
use reach::Reach;
pub(crate) use runtime::Moment;
use runtime::{FoundAhead, JobRuntime, Look, Missing};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// While the state file can't be saved, a new save is tried this often.
const SAVE_RETRY: Duration = Duration::from_secs(60);

/// Something the window (or the tray, or notifications) may want to know.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A job's row changed.
    Job(Box<JobView>),
    /// A job was removed.
    JobRemoved(JobId),
    /// Live progress of a running sync (frequent, small).
    Progress { id: JobId, progress: Progress },
    /// A run finished.
    Finished(Finished),
    /// The overall picture changed (for the tray icon and the header).
    Overview(Overview),
    /// The settings changed.
    Settings(Settings),
    /// Items counted so far while looking at one side of a job.
    ScanProgress { id: JobId, side: Side, items: u64 },
    /// Pairs of files read so far to tell whether they are identical (after the scans, when
    /// some files differ only by date).
    CompareProgress { id: JobId, files: u64, total_files: u64, bytes: u64, total_bytes: u64 },
    /// Something changed in the job's folders after its list was made: it can't be
    /// confirmed anymore.
    ReviewStale { id: JobId, review_id: String },
    /// Jobs whose changes wait for the person's OK and are due a mention (one notification).
    Waiting(Vec<WaitingJob>),
    /// A run nobody looked at was held back for the person's OK.
    Held { id: JobId, name: String, reason: String },
}

/// Receives every engine event. The app shell forwards them to the window.
pub type Emitter = Arc<dyn Fn(EngineEvent) + Send + Sync>;

struct Inner {
    state_file: StateFile,
    baselines: Baselines,
    /// Mirror and Backup: copies whose dates didn't stick on the destination.
    landings: Landings,
    /// Full automation: what automatic runs changed since the person last confirmed a list.
    unattended: Unattended,
    /// Items found missing on a drive that may show changes late (see `Missing`).
    absences: Absences,
    /// Two-way: pairs of files a full read found identical, for the background looks after
    /// it, even after a restart.
    same_contents: SameContents,
    journal: Journal,
    state: Mutex<StoredState>,
    runtime: Mutex<HashMap<JobId, JobRuntime>>,
    emit: Emitter,
    /// Every view gets the next number (see `JobView::rev`).
    rev: AtomicU64,
    /// Charon is quitting: nothing new starts.
    quitting: AtomicBool,
    /// How many times everything was paused while Charon runs. It goes up only while the live
    /// state is locked, together with what the pause stops and calls off (see
    /// `pause_everything`), so what was decided before a pause can tell that one came since.
    pauses: AtomicU64,
    /// When this engine opened (reminders wait for the first looks after a start).
    opened: Instant,
    /// The busy notes of syncs at work in the folders, this Charon's own and others' (see
    /// `run::BusyWatch`).
    busy: Arc<BusyWatch>,
    /// Asks the disk about the jobs' folders, with a time limit.
    reach: Reach,
    /// The drives kept quiet while a sync writes to them (see `quiet.rs`).
    quiet: quiet::QuietDrives,
    /// When the last save of the state file failed, if it did (a full disk?): runs nobody
    /// looks at are refused until a save works again (see `can_save`), and a run the person
    /// starts fails before it changes anything. Its lock is taken last, and no other lock is
    /// taken while it's held.
    cannot_save: Mutex<Option<Instant>>,
    /// When Charon 0.2's state file last changed, as found when the engine opened (it was
    /// moved aside then, so usually it isn't there).
    legacy_seen: Option<SystemTime>,
    /// An older Charon wrote its state file while this one runs: it's running too, and
    /// syncs by itself without this one's checks. Nothing syncs until Charon restarts.
    older_charon: AtomicBool,
    /// Only one Charon works with the data folder at a time.
    _lock: DataLock,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// Opens (or creates) Charon's data in `data_folder`. Fails with
    /// `Error::AlreadyRunning` while another Charon works with the same folder.
    pub fn open(data_folder: impl Into<PathBuf>, emit: Emitter) -> Result<Self> {
        let folder = DataFolder::new(data_folder);
        let lock = DataLock::acquire(folder.root())?;
        let state_file = StateFile::new(folder.state_file(), folder.legacy_state_file());
        let state = state_file.load()?;
        let absences = Absences::new(folder.absences());
        // What looks found missing before this launch, and for how long; and the last syncs
        // found dated later than now, and for how long Charon has known so.
        let launch = Moment::now();
        let runtime = state
            .jobs
            .iter()
            .filter_map(|job| {
                let kept = absences.load(&job.id, &job.setup);
                let dated_ahead = FoundAhead::restored(job, launch.own);
                (!kept.is_empty() || dated_ahead.is_some())
                    .then(|| (job.id.clone(), JobRuntime { missing: Missing::restored(kept, launch), dated_ahead, ..JobRuntime::default() }))
            })
            .collect();
        let legacy_seen = state_file.legacy_changed_at();
        let engine = Self {
            inner: Arc::new(Inner {
                state_file,
                baselines: Baselines::new(folder.baselines()),
                landings: Landings::new(folder.landings()),
                unattended: Unattended::new(folder.unattended()),
                absences,
                same_contents: SameContents::new(folder.same_contents()),
                journal: Journal::new(folder.logs()),
                state: Mutex::new(state),
                runtime: Mutex::new(runtime),
                emit,
                rev: AtomicU64::new(0),
                quitting: AtomicBool::new(false),
                pauses: AtomicU64::new(0),
                opened: Instant::now(),
                busy: Arc::new(BusyWatch::open(&folder.instances())),
                reach: Reach::new(reach::LIMIT),
                quiet: Default::default(),
                cannot_save: Mutex::new(None),
                legacy_seen,
                older_charon: AtomicBool::new(false),
                _lock: lock,
            }),
        };
        engine.journal_upgrade();
        Ok(engine)
    }

    /// Writes the move to confirm-first syncing into the log of each job it changed (once).
    fn journal_upgrade(&self) {
        let Some(notice) = self.state().upgrade_notice.clone().filter(|notice| !notice.journaled) else { return };
        for job in &notice.jobs {
            self.inner.journal.info(
                &job.id,
                &format!("Charon {}: this job no longer syncs by itself; it looks for changes and waits for your OK", notice.version),
            );
        }
        if let Err(e) = self.change_state(|state| {
            if let Some(notice) = state.upgrade_notice.as_mut() {
                notice.journaled = true;
            }
        }) {
            log::warn!("couldn't save that the update was logged: {e}");
        }
    }

    // ---------------------------------------------------------------- reading

    pub fn settings(&self) -> Settings {
        self.state().settings.clone()
    }

    /// Every job as a row, in the order they were added.
    pub fn jobs(&self) -> Vec<JobView> {
        let (snapshots, settings) = {
            let state = self.state();
            let runtime = self.runtime();
            let snapshots: Vec<_> = state.jobs.iter().map(|job| self.snapshot(job, runtime.get(&job.id))).collect();
            (snapshots, state.settings.clone())
        };
        self.views_of(snapshots, &settings)
    }

    /// One job as a row.
    pub fn job(&self, id: &JobId) -> Result<JobView> {
        let (snapshot, settings) = {
            let state = self.state();
            let runtime = self.runtime();
            let job = state.jobs.iter().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
            (self.snapshot(job, runtime.get(id)), state.settings.clone())
        };
        Ok(self.view_of(snapshot, &settings))
    }

    /// The stored jobs, without live state (for the autopilot).
    pub fn job_list(&self) -> Vec<Job> {
        self.state().jobs.clone()
    }

    pub fn overview(&self) -> Overview {
        Overview::of(&self.jobs(), self.settings().paused_all, self.problem())
    }

    /// What keeps Charon from syncing as it should, for the header and the tray: an older
    /// Charon running at the same time, or a state file that can't be saved.
    pub fn problem(&self) -> Option<String> {
        if self.inner.older_charon.load(Ordering::SeqCst) {
            Some(runs::OLDER_CHARON.into())
        } else if self.saving_failed().is_some() {
            Some(runs::CANNOT_SAVE.into())
        } else {
            None
        }
    }

    /// Whether an older Charon (0.2) runs at the same time as this one: its state file was
    /// written after this Charon opened. (0.2 doesn't know its jobs moved, and saves them
    /// there again after a sync.) It syncs by itself without this version's checks, so
    /// nothing syncs here until Charon is reopened with the older one quit. Once seen, it
    /// stays seen. Reads one small file's date: cheap enough to ask before every sync.
    pub fn older_charon_running(&self) -> bool {
        if self.inner.older_charon.load(Ordering::SeqCst) {
            return true;
        }
        let written = self.inner.state_file.legacy_changed_at();
        if written.is_none() || written == self.inner.legacy_seen {
            return false;
        }
        if !self.inner.older_charon.swap(true, Ordering::SeqCst) {
            log::error!("an older Charon wrote its state file while this one runs: nothing syncs until Charon is reopened");
            self.emit(EngineEvent::Overview(self.overview()));
        }
        true
    }

    /// Whether Charon's state can be saved: the last save worked, or, at most once a minute
    /// while saves fail, a new save does.
    pub(crate) fn can_save(&self) -> bool {
        match self.saving_failed() {
            None => true,
            Some(failed) if failed.elapsed() < SAVE_RETRY => false,
            Some(_) => {
                let saved = self.change_state(|_| ()).is_ok();
                if saved {
                    log::info!("Charon can save its state again");
                    self.emit(EngineEvent::Overview(self.overview()));
                }
                saved
            }
        }
    }

    /// The newest lines of a job's activity log, newest first.
    pub fn log(&self, id: &JobId, limit: usize) -> Vec<LogLine> {
        self.inner.journal.read(id, limit)
    }

    /// The notice shown once after the move to 0.3, while it hasn't been dismissed.
    pub fn upgrade_notice(&self) -> Option<UpgradeNotice> {
        let state = self.state();
        let mut notice = state.upgrade_notice.clone()?;
        // Only jobs that still exist.
        notice.jobs.retain(|upgraded| state.jobs.iter().any(|job| job.id == upgraded.id));
        (!notice.jobs.is_empty()).then_some(notice)
    }

    pub fn dismiss_upgrade_notice(&self) -> Result<()> {
        self.change_state(|state| state.upgrade_notice = None)
    }

    // ---------------------------------------------------------------- settings

    /// Saves new settings. "Pause everything" only changes through `set_all_paused`, so a
    /// window with an old copy of the settings can never resume everything by accident.
    pub fn set_settings(&self, mut settings: Settings) -> Result<()> {
        self.change_state(|state| {
            settings.paused_all = state.settings.paused_all;
            state.settings = settings;
        })?;
        self.emit(EngineEvent::Settings(self.settings()));
        self.publish_all();
        Ok(())
    }

    /// Pauses or resumes every job at once. Pausing stops the syncs nobody confirmed a list
    /// for, and calls off the syncs that wait for another sync; a sync the person confirmed
    /// that's at work goes on.
    pub fn set_all_paused(&self, paused: bool) -> Result<()> {
        // Saved first: a run a trigger starts from now on sees it and doesn't begin (see
        // `begin_run`), and one that began before is stopped below.
        let saved = self.change_state(|state| state.settings.paused_all = paused);
        // Even when the setting couldn't be saved: nothing goes on, or starts later, without
        // the person.
        if paused {
            self.pause_everything();
        }
        saved?;
        self.emit(EngineEvent::Settings(self.settings()));
        self.publish_all();
        Ok(())
    }

    // ---------------------------------------------------------------- quitting

    /// Before Charon quits: nothing new starts from now on, every look stops, and every
    /// running sync stops between two steps, recording what it carried as usual. Waits at
    /// most `wait` for the syncs; returns whether they all stopped in time.
    pub fn shut_down(&self, wait: Duration) -> bool {
        self.inner.quitting.store(true, Ordering::SeqCst);
        for rt in self.runtime().values_mut() {
            rt.stop_looking();
            if let Some(cancel) = &rt.cancel {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        // A confirmed sync that waits for another sync lives only while Charon runs: the job's
        // log says it was called off, and after a restart the job waits for an OK again.
        self.call_off_all_confirmed(" (Charon quit)");
        let deadline = Instant::now() + wait;
        while self.runtime().values().any(|rt| rt.is_running()) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
        true
    }

    /// Whether Charon is quitting.
    pub fn is_shutting_down(&self) -> bool {
        self.inner.quitting.load(Ordering::SeqCst)
    }

    /// How many times everything was paused while Charon runs (see `pause_everything`).
    pub(crate) fn pauses(&self) -> u64 {
        self.inner.pauses.load(Ordering::SeqCst)
    }

    /// The jobs syncing right now.
    pub fn running(&self) -> Vec<JobId> {
        self.runtime().iter().filter(|(_, rt)| rt.is_running()).map(|(id, _)| id.clone()).collect()
    }

    // ---------------------------------------------------------------- internals

    fn state(&self) -> MutexGuard<'_, StoredState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn runtime(&self) -> MutexGuard<'_, HashMap<JobId, JobRuntime>> {
        self.inner.runtime.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stored_job(&self, id: &JobId) -> Result<Job> {
        self.state()
            .jobs
            .iter()
            .find(|job| &job.id == id)
            .cloned()
            .ok_or_else(|| Error::UnknownJob(id.clone()))
    }

    /// Changes the stored state: the changed copy is saved first, and only then becomes the
    /// state Charon works with. A failed save changes nothing.
    fn change_state<R>(&self, change: impl FnOnce(&mut StoredState) -> R) -> Result<R> {
        let mut state = self.state();
        let mut next = state.clone();
        let result = change(&mut next);
        self.save(&next)?;
        *state = next;
        Ok(result)
    }

    /// Saves the state file (with the state lock held), and notes whether it worked.
    fn save(&self, state: &StoredState) -> Result<()> {
        let saved = self.inner.state_file.save(state);
        let mut failed = self.inner.cannot_save.lock().unwrap_or_else(|e| e.into_inner());
        match &saved {
            Ok(()) => *failed = None,
            Err(e) => {
                if failed.is_none() {
                    log::error!("couldn't save Charon's state: {e}");
                }
                *failed = Some(Instant::now());
            }
        }
        saved
    }

    /// When the last save of the state file failed, if it did.
    fn saving_failed(&self) -> Option<Instant> {
        *self.inner.cannot_save.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes one stored job and saves.
    fn change_job(&self, id: &JobId, change: impl FnOnce(&mut Job)) -> Result<()> {
        self.change_state(|state| state.jobs.iter_mut().find(|job| &job.id == id).map(change))?
            .ok_or_else(|| Error::UnknownJob(id.clone()))
    }

    /// Changes one stored job and saves, like `change_job`, but when the save fails (a full
    /// system disk, a state file another program holds open) the change still holds while
    /// Charon runs, and the next save that works writes it too. For what a job waits for
    /// after a run (held back, stopped, not recorded): every part of Charon must keep
    /// seeing that the job waits for an OK, whether or not the disk took it.
    fn change_job_anyway(&self, id: &JobId, change: impl FnOnce(&mut Job)) -> Result<()> {
        let mut state = self.state();
        let mut next = state.clone();
        next.jobs.iter_mut().find(|job| &job.id == id).map(change).ok_or_else(|| Error::UnknownJob(id.clone()))?;
        let saved = self.save(&next);
        *state = next;
        saved
    }

    /// Changes the stored state and the job's live state together, but only while the job
    /// isn't syncing (refused with `busy`). `change` changes a copy of the stored state, which
    /// is saved first: only once that worked does the copy become the state Charon works with,
    /// and does `apply` change the job's live state, with what `change` returned. A change
    /// that can't be saved changes neither, and says so.
    fn with_job_at_rest<T, R>(
        &self,
        id: &JobId,
        busy: &str,
        change: impl FnOnce(&mut StoredState) -> Result<T>,
        apply: impl FnOnce(&mut JobRuntime, T) -> R,
    ) -> Result<R> {
        let mut state = self.state();
        let mut runtime = self.runtime();
        if runtime.get(id).is_some_and(|rt| rt.is_running()) {
            return Err(Error::Invalid(busy.to_string()));
        }
        let mut next = state.clone();
        let changed = change(&mut next)?;
        self.save(&next).map_err(|e| Error::Invalid(format!("Charon couldn't save this change ({e}), so nothing changed.")))?;
        *state = next;
        Ok(apply(runtime.entry(id.clone()).or_default(), changed))
    }

    /// Changes one job's live state.
    fn change_runtime<R>(&self, id: &JobId, change: impl FnOnce(&mut JobRuntime) -> R) -> R {
        change(self.runtime().entry(id.clone()).or_default())
    }

    /// Changes one job's live state, if the job still has one. For whatever ends after the
    /// job may have been removed (a look, a review being made): it leaves nothing behind.
    fn change_known_runtime<R>(&self, id: &JobId, change: impl FnOnce(&mut JobRuntime) -> R) -> Option<R> {
        self.runtime().get_mut(id).map(change)
    }

    /// Changes what the looks at `job` found missing (see `Missing`), and keeps it for the
    /// next launch when that changed. It's saved while the live state is locked, so two
    /// looks never save in the wrong order; a record that can't be saved is removed, so the
    /// next launch starts the waits over rather than trust an old record. A job that has no
    /// live state any more (it was removed while a look ran) keeps nothing.
    fn change_missing<R>(&self, job: &Job, change: impl FnOnce(&mut Missing) -> R) -> R {
        let mut runtime = self.runtime();
        let Some(rt) = runtime.get_mut(&job.id) else { return change(&mut Missing::default()) };
        let missing = &mut rt.missing;
        let before = missing.kept();
        let result = change(missing);
        let kept = missing.kept();
        if kept != before {
            if let Err(e) = self.inner.absences.save(&job.id, &job.setup, &kept) {
                log::warn!("couldn't keep the items missing in {} for the next launch: {e}", job.setup.name);
                let _ = self.inner.absences.forget(&job.id);
            }
        }
        result
    }

    fn emit(&self, event: EngineEvent) {
        (self.inner.emit)(event);
    }

    /// Sends a job's fresh row, and the overview.
    pub fn publish(&self, id: &JobId) {
        if let Ok(view) = self.job(id) {
            self.emit(EngineEvent::Job(Box::new(view)));
        }
        self.emit(EngineEvent::Overview(self.overview()));
    }

    fn publish_all(&self) {
        let views = self.jobs();
        let overview = Overview::of(&views, self.settings().paused_all, self.problem());
        for view in views {
            self.emit(EngineEvent::Job(Box::new(view)));
        }
        self.emit(EngineEvent::Overview(overview));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_save_that_failed_is_tried_again_after_a_while_and_the_problem_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::open(tmp.path(), Arc::new(|_| {})).unwrap();
        // The state file can't be replaced (a full disk would do the same).
        let state = tmp.path().join("charon-state.json");
        let _ = fs::remove_file(&state);
        fs::create_dir_all(state.join("in the way")).unwrap();
        assert!(engine.change_state(|_| ()).is_err());
        assert_eq!(engine.problem().as_deref(), Some(runs::CANNOT_SAVE));
        fs::remove_dir_all(&state).unwrap();
        assert!(!engine.can_save(), "not tried again at once");
        assert!(engine.problem().is_some());
        // A minute later, a new save is tried, and works.
        *engine.inner.cannot_save.lock().unwrap() = Some(Instant::now().checked_sub(SAVE_RETRY).expect("the computer has run for a minute"));
        assert!(engine.can_save());
        assert_eq!(engine.problem(), None);
        assert!(state.is_file());
    }
}
