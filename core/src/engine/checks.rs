//! Looking for changes without touching anything, and telling the person when changes wait
//! for their OK.

use crate::engine::survey::{Purpose, Survey, INTERNAL_LOOK};
use crate::engine::quiet::Asker;
use crate::engine::{Engine, EngineEvent, FoundAhead, Moment};
use crate::error::{Error, Result};
use crate::model::{DatedAhead, Job, JobId, JobStatus, Trigger, WaitingJob};
use crate::scan::{ExcludeFilter, Keys};
use chrono::Utc;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// The first notice that changes wait comes at most this often per job…
const FIRST_NOTICE: Duration = Duration::from_secs(30 * 60);
/// …the first reminder after this long…
const SECOND_NOTICE: Duration = Duration::from_secs(2 * 60 * 60);
/// …and then one a day.
const LATER_NOTICES: Duration = Duration::from_secs(24 * 60 * 60);
/// After a start, reminders wait for every job's first look, at most this long.
const FIRST_LOOKS: Duration = Duration::from_secs(2 * 60);
/// While a job's last sync is dated later than now, how long Charon has known so is saved for
/// the next launch at least this often.
const DATED_AHEAD_SAVED_EVERY: Duration = Duration::from_secs(10 * 60);

/// Where in a job's folders the folder watcher heard a change.
pub(crate) enum ChangeAt {
    /// At one of the folders themselves (a drive mounted or unmounted there), or at a path
    /// Charon can't place: anything in them may have changed.
    Folder,
    /// At an item inside, by its key (as spelled on disk).
    Item(String),
    /// At an item the job leaves out.
    Ignored,
}

impl Engine {
    /// Looks for changes in the background: scans both folders and plans, but changes
    /// nothing. Does nothing while the job runs, is being looked at, or a look is running,
    /// nor while another job runs or is looked at on a drive of its folders that's run by an
    /// add-on driver, or began to wait for its turn there first (it's looked at in its turn;
    /// see `quiet.rs`). What it finds counts only if nothing outdated it meanwhile (see
    /// `note_look`).
    pub fn check(&self, id: &JobId) {
        self.look_in_background(id, Asker::Charon);
    }

    /// `check`, for the person's *Look for changes*: when another job uses a drive of the
    /// folders that's run by an add-on driver, says so, and that the look comes once it's done.
    pub fn look_now(&self, id: &JobId) -> Result<()> {
        match self.look_in_background(id, Asker::PersonsLook) {
            Some(other) => Err(Error::Invalid(format!(
                "“{other}” is using the same drive right now. Its driver isn't Apple's own (like Paragon's NTFS), so Charon works on one \
                 job at a time there, and looks at these folders once it's done."
            ))),
            None => Ok(()),
        }
    }

    /// `check`, or `look_now` (as `asker` says). Returns the name of the job that holds the look
    /// back, when another job uses a drive of the folders that's run by an add-on driver, or
    /// began to wait for it first (see `drive_used_among`).
    fn look_in_background(&self, id: &JobId, asker: Asker) -> Option<String> {
        let stop = Arc::new(AtomicBool::new(false));
        let quitting = self.is_shutting_down();
        // The setup and the epoch are read together, as an edit changes them together: a look
        // of the old setup with the new epoch would count.
        let (job, epoch) = {
            let state = self.state();
            let job = state.jobs.iter().find(|job| &job.id == id).cloned()?;
            let mut runtime = self.runtime();
            // Asked with the live state locked: a run of another job that starts on the drive
            // from now on finds this look started, and waits for it or stops it.
            let drive_used = self.drive_used_among(&state.jobs, &runtime, &job, asker);
            let rt = runtime.entry(id.clone()).or_default();
            Self::forget_own_writes(rt);
            let busy = quitting || rt.is_running() || rt.is_checking() || rt.preparing.is_some();
            if busy {
                return None;
            }
            if drive_used.is_some() {
                // Looked at as soon as its turn comes.
                rt.last_checked = None;
                self.note_drive_wait(rt);
                return drive_used;
            }
            self.stop_looks_sharing_a_drive(&state.jobs, &mut runtime, &job);
            let rt = runtime.entry(id.clone()).or_default();
            rt.checking = Some(stop.clone());
            rt.look_moved = Some(Instant::now());
            rt.drive_wait = None;
            (job, rt.epoch)
        };
        let engine = self.clone();
        let flag = stop.clone();
        let spawned = thread::Builder::new().name("charon-check".into()).spawn(move || {
            let looked = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let result = engine.survey(&job, &flag, Purpose::Check);
                engine.end_check(&job, &flag, epoch, result);
            }));
            // A mistake in Charon itself never leaves the job marked as being looked at.
            if looked.is_err() {
                log::error!("a look at job {} ended on a panic", job.id);
                engine.change_known_runtime(&job.id, |rt| {
                    rt.problem = Some(INTERNAL_LOOK.into());
                    if rt.checking.as_ref().is_some_and(|f| Arc::ptr_eq(f, &flag)) {
                        rt.checking = None;
                    }
                    rt.last_checked = Some(Instant::now());
                });
                engine.publish(&job.id);
            }
        });
        if spawned.is_err() {
            self.change_runtime(id, |rt| rt.checking = None);
        }
        None
    }

    /// Ends a background look that began at `epoch` with `flag` as its stop flag: what it
    /// found, or what went wrong, counts only if nothing outdated it meanwhile (see
    /// `note_look`).
    fn end_check(&self, job: &Job, flag: &Arc<AtomicBool>, epoch: u64, result: Result<Survey>) {
        // A look made for a setup that changed meanwhile tells nothing.
        let setup_current = self.stored_job(&job.id).is_ok_and(|now| now.setup == job.setup);
        let counted = match &result {
            Ok(survey) => self.note_look(job, survey, Some(epoch)),
            Err(Error::Cancelled) | Err(Error::Offline(_)) => false,
            Err(e) => self
                .change_known_runtime(&job.id, |rt| {
                    let current = setup_current && rt.epoch == epoch && !rt.is_running();
                    if current {
                        rt.problem = Some(e.to_string());
                    }
                    current
                })
                .unwrap_or(false),
        };
        self.change_known_runtime(&job.id, |rt| {
            if rt.checking.as_ref().is_some_and(|f| Arc::ptr_eq(f, flag)) {
                rt.checking = None;
            }
            // A look that was outdated meanwhile doesn't count as one: the job is looked at
            // again as soon as nothing else is looking. (One that found a folder away does,
            // or it would be tried again every second.)
            if counted || (setup_current && rt.epoch == epoch) {
                rt.last_checked = Some(Instant::now());
            }
        });
        self.publish(&job.id);
    }

    /// Whether the last look is older than `interval` (or there was none), and nothing is
    /// looking or running now.
    pub fn check_due(&self, id: &JobId, interval: Duration) -> bool {
        self.runtime().get(id).is_none_or(|rt| {
            !rt.is_checking() && !rt.is_running() && rt.preparing.is_none() && rt.last_checked.is_none_or(|t| t.elapsed() >= interval)
        })
    }

    /// Whether the last look found changes a job that runs by itself would carry, or files
    /// it couldn't compare yet (the run reads them), and no automatic run has tried them yet
    /// and ended before carrying anything. Removals that wait for an item to be missing a
    /// while longer don't count (see `Missing`).
    pub fn has_pending(&self, id: &JobId) -> bool {
        let now = Moment::now();
        self.runtime().get(id).is_some_and(|rt| {
            rt.look.as_ref().is_some_and(|look| look.items + look.uncompared > rt.missing.waiting(now) && rt.tried.is_none_or(|tried| look.at > tried))
        })
    }

    /// Whether removals a run left for later can be carried out now (see `Missing`): the job
    /// then syncs once more, whatever starts its syncs otherwise.
    pub fn take_follow_up(&self, id: &JobId) -> bool {
        self.take_follow_up_at(id, Moment::now())
    }

    /// `take_follow_up` at the moment `now`.
    pub(crate) fn take_follow_up_at(&self, id: &JobId, now: Moment) -> bool {
        if !self.runtime().get(id).is_some_and(|rt| rt.missing.follow_up_due(now)) {
            return false;
        }
        let Ok(job) = self.stored_job(id) else { return false };
        self.change_missing(&job, |missing| missing.take_follow_up(now))
    }

    /// Whether a list of the job's changes is being prepared or waits for the person's OK
    /// (automatic runs of the job wait meanwhile).
    pub fn review_open(&self, id: &JobId) -> bool {
        self.runtime()
            .get(id)
            .is_some_and(|rt| rt.preparing.is_some() || rt.review.as_ref().is_some_and(|review| Utc::now() < review.expires_at))
    }

    /// Records that the job's schedule came due now, whether it ran or only looked, so the
    /// next time is counted from here.
    pub fn mark_scheduled(&self, id: &JobId) {
        if let Err(e) = self.change_job(id, |job| job.last_scheduled = Some(Utc::now())) {
            log::warn!("couldn't save the schedule of job {id}: {e}");
        }
    }

    /// Something changed at `path` inside one of the job's folders. Returns whether the
    /// change matters (it isn't an excluded item or one of Charon's own files); a list
    /// made before it can't be confirmed anymore.
    pub fn note_change(&self, id: &JobId, path: &Path) -> bool {
        let Ok(job) = self.stored_job(id) else { return false };
        if let ChangeAt::Ignored = self.change_at(&job, path) {
            return false;
        }
        self.mark_review_stale(id);
        true
    }

    /// Where in the job's folders `path` is (see `ChangeAt`).
    pub(crate) fn change_at(&self, job: &Job, path: &Path) -> ChangeAt {
        let relative = [&job.setup.source, &job.setup.destination].into_iter().find_map(|root| path.strip_prefix(root).ok());
        let Some(relative) = relative.and_then(|r| r.to_str()) else { return ChangeAt::Folder };
        let key = Keys::normalize(&relative.replace('\\', "/"));
        if key.is_empty() {
            return ChangeAt::Folder;
        }
        let excluded = self.change_runtime(&job.id, |rt| {
            let fresh = rt.filter.as_ref().is_some_and(|(setup, _)| setup == &job.setup);
            if !fresh {
                // Until a look finds out how the drives treat case, patterns match it exactly:
                // they then leave out the fewest items, so a change that matters is never missed.
                rt.filter = ExcludeFilter::new(&job.setup.excludes, false).ok().map(|filter| (job.setup.clone(), filter));
            }
            rt.filter.as_ref().is_some_and(|(_, filter)| {
                filter.excludes(&key) || Keys::ancestors(&key).any(|folder| filter.excludes(folder))
            })
        });
        if excluded {
            ChangeAt::Ignored
        } else {
            ChangeAt::Item(key)
        }
    }

    /// Tells the person (in one notification) about the jobs whose changes or conflicts
    /// wait for them, or whose files a look couldn't compare yet, and that haven't been
    /// mentioned for a while. Paused and offline jobs aren't
    /// mentioned. Right after Charon starts it waits for the first looks, so the jobs found
    /// waiting then come in one notification.
    pub fn remind(&self) {
        if self.settings().paused_all {
            return;
        }
        let first_looks_done = self.inner.opened.elapsed() >= FIRST_LOOKS
            || self.job_list().iter().all(|job| job.paused || self.runtime().get(&job.id).is_some_and(|rt| rt.last_checked.is_some()));
        if !first_looks_done {
            return;
        }
        let mut due = Vec::new();
        for job in self.job_list() {
            if job.paused {
                continue;
            }
            // Only jobs whose last look found something, or that were held back.
            let candidate = job.held.is_some() || self.runtime().get(&job.id).and_then(|rt| rt.look.as_ref()).is_some_and(|look| look.needs_a_list());
            if !candidate {
                continue;
            }
            let Ok(view) = self.job(&job.id) else { continue };
            // A list the person confirmed (its sync waits for another one) needs nothing more.
            if view.confirmed || matches!(view.status, JobStatus::Offline | JobStatus::Syncing) {
                continue;
            }
            let conflicts = view.conflicts.len() as u64;
            if view.waiting.is_none() && conflicts == 0 {
                continue;
            }
            let tell = self.change_runtime(&job.id, |rt| {
                let gap = match rt.reminder.count {
                    0 => FIRST_NOTICE,
                    1 => SECOND_NOTICE,
                    _ => LATER_NOTICES,
                };
                let now = Utc::now();
                // A clock set back counts as time gone by: a reminder is never put off for good.
                let tell = rt.reminder.last.is_none_or(|last| now < last || (now - last).to_std().is_ok_and(|gone| gone >= gap));
                if tell {
                    rt.reminder.last = Some(now);
                    rt.reminder.count += 1;
                }
                tell
            });
            if tell {
                due.push(WaitingJob {
                    id: job.id.clone(),
                    name: job.setup.name.clone(),
                    items: view.waiting.as_ref().map_or(0, |w| w.items),
                    removals: view.waiting.as_ref().map_or(0, |w| w.removals.total()),
                    conflicts,
                    uncompared: view.waiting.as_ref().map_or(0, |w| w.uncompared),
                    held: job.held.is_some(),
                    drive_connected: job.setup.trigger == Trigger::DriveConnected,
                });
            }
        }
        if !due.is_empty() {
            self.emit(EngineEvent::Waiting(due));
        }
    }

    /// The job's drive was just connected: what its next look finds is mentioned at once
    /// (the person may be about to eject the drive again).
    pub fn mention_next_look(&self, id: &JobId) {
        self.change_runtime(id, |rt| rt.reminder.last = None);
    }

    /// A folder of the job came back (a drive plugged in again, a share reconnected). What
    /// the last look found, it found before: the row mustn't say "In sync" from then. The
    /// look is put aside, so is what a look still under way finds, and the job is looked
    /// at again at once. A share that just came back may list a folder before it has loaded,
    /// so the items found missing before have to be found missing again for a while before
    /// a run nobody looks at follows them (see `Missing::look_again`).
    pub fn forget_look(&self, id: &JobId) {
        self.change_runtime(id, |rt| {
            rt.epoch += 1;
            rt.look = None;
            rt.tried = None;
            rt.last_checked = None;
            rt.missing.look_again();
        });
    }

    /// Charon wasn't looking at the folders for a while (the computer slept, or the clock
    /// on the wall jumped): the shares may have gone away and come back meanwhile, so the
    /// items found missing on them have to be found missing again for a while before a run
    /// nobody looks at follows them (see `Missing::look_again`).
    pub fn after_a_gap(&self) {
        for rt in self.runtime().values_mut() {
            rt.missing.look_again();
        }
    }

    /// Notes the jobs whose last sync is dated later than `now`. Such a job synced while the
    /// clock on the wall was ahead, or the clock is behind now, as it often is for a moment
    /// after the computer starts. Which of the two can't be told, so the date Charon keeps
    /// stays as it is, and Charon counts for how long it has known so, by its own clock, until
    /// the job syncs again (see `FoundAhead`). The job's row shows the sync as made when
    /// Charon found it so, when that's earlier than the date (see `last_synced`): it then
    /// never says "Synced just now" for a sync days old, even once the clock is past the date,
    /// and its "Not synced for N days" comes once the job hasn't synced for that long since
    /// then. The count is saved with the job when the date is found, and then at least every
    /// `DATED_AHEAD_SAVED_EVERY`, so the next launch counts on from there.
    pub(crate) fn note_syncs_dated_ahead(&self, now: Moment) {
        let mut found = Vec::new();
        let mut save = false;
        {
            let state = self.state();
            let mut runtime = self.runtime();
            for job in &state.jobs {
                let noted = runtime.get(&job.id).and_then(|rt| rt.dated_ahead);
                let ahead = noted
                    .filter(|noted| job.last_synced == Some(noted.at))
                    .or_else(|| job.last_synced.filter(|at| *at > now.wall).map(|at| FoundAhead::found(at, now.own)));
                if ahead.map(|ahead| ahead.at) != noted.map(|noted| noted.at) {
                    runtime.entry(job.id.clone()).or_default().dated_ahead = ahead;
                    found.push(job.id.clone());
                }
                save |= Self::save_dated_ahead(job.dated_ahead.as_ref(), ahead.map(|ahead| ahead.kept(now.own)).as_ref());
            }
        }
        // While Charon's state can't be saved, `can_save` tries again now and then.
        if save && self.saving_failed().is_none() {
            let saved = self.change_state(|state| {
                let runtime = self.runtime();
                for job in &mut state.jobs {
                    let ahead = runtime.get(&job.id).and_then(|rt| rt.dated_ahead).filter(|ahead| job.last_synced == Some(ahead.at));
                    job.dated_ahead = ahead.map(|ahead| ahead.kept(now.own));
                }
            });
            if let Err(e) = saved {
                log::warn!("couldn't keep the last syncs dated later than now for the next launch: {e}");
            }
        }
        for id in found {
            self.publish(&id);
        }
    }

    /// Whether a job's record of its last sync dated later than now, saved as `stored`, must
    /// be saved again as `kept`: when the date is found, when it no longer is the job's last
    /// sync, and while the count goes on, once it's `DATED_AHEAD_SAVED_EVERY` ahead of the one
    /// saved.
    fn save_dated_ahead(stored: Option<&DatedAhead>, kept: Option<&DatedAhead>) -> bool {
        match (stored, kept) {
            (Some(stored), Some(kept)) if stored.last_synced == kept.last_synced => {
                kept.counted_seconds >= stored.counted_seconds.saturating_add(DATED_AHEAD_SAVED_EVERY.as_secs())
            }
            (stored, kept) => stored != kept,
        }
    }
}

#[cfg(test)]
impl Engine {
    /// When the job last synced, as its row shows it at `now`.
    pub(crate) fn last_synced_at(&self, id: &JobId, now: Moment) -> Option<chrono::DateTime<Utc>> {
        let job = self.stored_job(id).unwrap();
        Self::last_synced(&job, self.runtime().get(id).and_then(|rt| rt.dated_ahead.as_ref()), now)
    }

    /// A run nobody looked at found `key` missing at `now` on the job's source, as if that
    /// were a network share, and left its removal from the destination for later.
    pub(crate) fn leave_a_removal_for_later(&self, id: &JobId, key: &str, now: Moment) {
        use crate::plan::{Action, Plan, Side};
        use crate::scan::{Entry, Snapshot, Volume};
        let job = self.stored_job(id).unwrap();
        let mut share = Snapshot::new();
        share.facts_mut().volume = Some(Volume { network: true, ..Volume::of(&job.setup.source) });
        let mine: Snapshot = [(key.to_string(), Entry::file(1, 1))].into_iter().collect();
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, key, Entry::file(1, 1), None));
        self.change_missing(&job, |missing| {
            missing.note(&plan, &share, &mine, now);
            missing.hold_back_recent(&mut plan, &share, &mine, now);
        });
        assert!(plan.actions.is_empty(), "the removal was carried out");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runs::Crossing;
    use crate::model::{ConflictRule, JobSetup, ReviewKind, Settings, SyncMode};
    use std::fs;
    use std::sync::atomic::Ordering;

    /// An engine with one Backup job whose source holds a file that waits to be copied.
    fn engine_with_a_job(tmp: &Path) -> (Engine, Job) {
        let (source, destination) = (tmp.join("source"), tmp.join("destination"));
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("a.txt"), "a").unwrap();
        let engine = Engine::open(tmp.join("data"), Arc::new(|_| {})).unwrap();
        let setup = JobSetup {
            name: "Backup".into(),
            source,
            destination,
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        // The look `add_job` started is done before the test begins.
        while engine.runtime().get(&id).is_some_and(|rt| rt.is_checking()) {
            thread::sleep(Duration::from_millis(5));
        }
        let job = engine.stored_job(&id).unwrap();
        (engine, job)
    }

    /// Opens the engine on `data` again, once the one before has let go of it (its threads may
    /// hold it for a moment after it's dropped).
    fn reopen(data: &Path) -> Engine {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Engine::open(data, Arc::new(|_| {})) {
                Ok(engine) => return engine,
                Err(Error::AlreadyRunning) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("the engine doesn't open again: {e}"),
            }
        }
    }

    /// `job` as a *Drive connected* job, whose row shows its last sync as `shown`.
    fn in_a_drawer(job: &Job, shown: Option<chrono::DateTime<Utc>>) -> Job {
        Job { setup: JobSetup { trigger: Trigger::DriveConnected, ..job.setup.clone() }, last_synced: shown, ..job.clone() }
    }

    fn look_at(engine: &Engine, job: &Job) -> Survey {
        engine.survey(job, &AtomicBool::new(false), Purpose::Check).unwrap()
    }

    fn epoch(engine: &Engine, id: &JobId) -> u64 {
        engine.runtime().get(id).map_or(0, |rt| rt.epoch)
    }

    #[test]
    fn a_look_begun_before_a_sync_never_replaces_what_came_after() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let before = epoch(&engine, &job.id);
        let survey = look_at(&engine, &job);
        // A sync begins (and ends) while the look is still on its way.
        engine.begin_run(&job.id, &Crossing::unattended("a test", false), |_, _| Ok(())).unwrap();
        engine.change_runtime(&job.id, |rt| {
            rt.cancel = None;
            rt.look = None;
        });
        assert!(!engine.note_look(&job, &survey, Some(before)));
        assert!(engine.runtime().get(&job.id).is_some_and(|rt| rt.look.is_none()), "the old look isn't recorded");
        let review = engine.keep_review(&job, survey, ReviewKind::Sync, None, None, before);
        assert!(matches!(review, Err(Error::ReviewOutdated(_))));
        assert!(engine.runtime().get(&job.id).is_some_and(|rt| rt.review.is_none()));
    }

    #[test]
    fn a_look_outdated_by_an_edit_leaves_the_new_setup_to_be_looked_at_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        // A background look is on its way…
        let flag = Arc::new(AtomicBool::new(false));
        let before = engine.change_runtime(&job.id, |rt| {
            rt.look = None;
            rt.checking = Some(flag.clone());
            rt.epoch
        });
        let survey = look_at(&engine, &job);
        // …when the person edits the job.
        let mut setup = job.setup.clone();
        setup.excludes.push("*.tmp".into());
        engine.update_job(&job.id, setup).unwrap();
        assert!(flag.load(Ordering::Relaxed), "the look in progress was told to stop");
        engine.end_check(&job, &flag, before, Ok(survey));
        {
            let runtime = engine.runtime();
            let rt = runtime.get(&job.id).unwrap();
            assert!(rt.look.is_none() && !rt.is_checking());
            assert!(rt.last_checked.is_none(), "the next tick looks at the new setup");
        }
        assert!(engine.check_due(&job.id, Settings::default().check_interval()));
    }

    #[test]
    fn a_look_of_the_old_setup_never_counts_for_the_edited_job() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let flag = Arc::new(AtomicBool::new(false));
        engine.change_runtime(&job.id, |rt| {
            rt.look = None;
            rt.checking = Some(flag.clone());
        });
        let survey = look_at(&engine, &job);
        let mut setup = job.setup.clone();
        setup.excludes.push("*.tmp".into());
        engine.update_job(&job.id, setup).unwrap();
        // The look of the old setup ends with the epoch the edit left.
        let now = epoch(&engine, &job.id);
        engine.end_check(&job, &flag, now, Ok(survey));
        let rt = engine.runtime();
        let rt = rt.get(&job.id).unwrap();
        assert!(rt.look.is_none());
        assert!(rt.last_checked.is_none(), "the edited job is looked at on the next tick");
    }

    #[test]
    fn a_look_that_ends_after_its_job_was_removed_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let before = epoch(&engine, &job.id);
        let survey = look_at(&engine, &job);
        engine.remove_job(&job.id).unwrap();
        assert!(!engine.note_look(&job, &survey, Some(before)));
        let review = engine.keep_review(&job, survey, ReviewKind::Sync, None, None, before);
        assert!(review.is_err());
        engine.end_check(&job, &Arc::new(AtomicBool::new(false)), before, Err(Error::Invalid("gone".into())));
        assert!(engine.runtime().get(&job.id).is_none(), "no live state is left for a removed job");
    }

    #[test]
    fn a_sync_dated_later_than_now_counts_from_when_charon_found_that_date() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let id = &job.id;
        let a_day = Duration::from_secs(24 * 60 * 60);
        // The job synced while the clock was an hour ahead. A day after Charon found it so (by
        // its own clock), with the clock behind that date still, it counts as made a day ago.
        let first = Utc::now() + chrono::Duration::hours(1);
        engine.pretend_the_last_sync_ended(id, Moment { wall: first, own: Instant::now() });
        engine.note_syncs_dated_ahead(Moment::now());
        let found = Moment::now();
        assert_eq!(engine.stored_job(id).unwrap().last_synced, Some(first), "the date Charon keeps changed");
        let next_day = Moment { wall: found.wall, own: found.own + a_day };
        let shown = engine.last_synced_at(id, next_day);
        assert!(shown.is_some_and(|shown| shown <= found.wall - chrono::Duration::days(1)), "{shown:?}");

        // It syncs again while the clock is ahead again: that sync counts from when Charon
        // found its date, not from when it found the one before.
        let second = Utc::now() + chrono::Duration::hours(2);
        engine.pretend_the_last_sync_ended(id, Moment { wall: second, own: Instant::now() });
        let before = Moment::now();
        engine.note_syncs_dated_ahead(Moment::now());
        let shown = engine.last_synced_at(id, Moment { wall: before.wall, own: before.own + a_day }).unwrap();
        assert!(shown >= before.wall - chrono::Duration::days(1), "counted from when Charon found the date before: {shown}");

        // It syncs again with the clock right: that sync shows as kept, and nothing is kept
        // about the one before.
        engine.pretend_the_last_sync_ended(id, Moment { wall: Utc::now() - chrono::Duration::minutes(1), own: Instant::now() });
        engine.note_syncs_dated_ahead(Moment::now());
        assert!(engine.runtime().get(id).is_some_and(|rt| rt.dated_ahead.is_none()));
        assert_eq!(engine.stored_job(id).unwrap().dated_ahead, None);
        assert_eq!(engine.last_synced_at(id, Moment::now()), engine.stored_job(id).unwrap().last_synced);
    }

    #[test]
    fn a_sync_dated_later_than_now_still_counts_from_when_charon_found_it_once_the_clock_is_past_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let id = &job.id;
        // A backup drive's job synced while the clock was three days ahead, which has been set
        // right since, and the drive then went into a drawer. Charon runs all the while, and
        // notes the date at every tick.
        let ahead = Utc::now() + chrono::Duration::days(3);
        engine.pretend_the_last_sync_ended(id, Moment { wall: ahead, own: Instant::now() });
        let found = Moment::now();
        engine.note_syncs_dated_ahead(found);
        let row = |now: Moment| {
            engine.note_syncs_dated_ahead(now);
            let shown = engine.last_synced_at(id, now);
            let stale = Engine::stale(&in_a_drawer(&engine.stored_job(id).unwrap(), shown), None, true, now.wall);
            (shown.unwrap(), stale)
        };
        // A minute after the clock passed the date, the sync still counts as made when Charon
        // found it so, three days before, not just now.
        let (shown, _) = row(found.after(Duration::from_secs((3 * 24 * 60 + 1) * 60)));
        assert_eq!(shown, found.wall);
        // A week and an hour after Charon found it so, the row says the job hasn't synced since.
        let (_, stale) = row(found.after(Duration::from_secs((7 * 24 + 1) * 60 * 60)));
        assert_eq!(stale.as_deref(), Some("Not synced for 7 days"));
    }

    #[test]
    fn how_long_charon_has_known_a_sync_was_dated_later_than_now_counts_on_after_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let (engine, job) = engine_with_a_job(tmp.path());
        let id = job.id.clone();
        let minutes = |n: u64| Duration::from_secs(n * 60);
        // The job synced while the clock was three days ahead, and Charon found it so. That's
        // saved with the job at once, and how long Charon has known so every ten minutes.
        let ahead = Utc::now() + chrono::Duration::days(3);
        engine.pretend_the_last_sync_ended(&id, Moment { wall: ahead, own: Instant::now() });
        let found = Moment::now();
        engine.note_syncs_dated_ahead(found);
        let kept = |engine: &Engine| engine.stored_job(&id).unwrap().dated_ahead;
        assert_eq!(kept(&engine), Some(DatedAhead { last_synced: ahead, counted_seconds: 0 }));
        engine.note_syncs_dated_ahead(found.after(minutes(9)));
        assert_eq!(kept(&engine).map(|kept| kept.counted_seconds), Some(0));
        engine.note_syncs_dated_ahead(found.after(minutes(10)));
        assert_eq!(kept(&engine).map(|kept| kept.counted_seconds), Some(10 * 60));
        // Charon has known so for two days when it's quit, and it's opened again.
        let two_days = minutes(2 * 24 * 60);
        engine.note_syncs_dated_ahead(found.after(two_days));
        drop(engine);
        let engine = reopen(&data);
        let launch = Moment::now();
        assert_eq!(engine.stored_job(&id).unwrap().last_synced, Some(ahead), "the date Charon keeps changed");
        let shown = engine.last_synced_at(&id, launch).unwrap();
        assert!(shown <= launch.wall - chrono::Duration::days(2), "the row shows {shown}");
        // Five days and an hour later (the clock is past the date by then), the row says the
        // job hasn't synced for a week.
        let later = launch.after(minutes((5 * 24 + 1) * 60));
        engine.note_syncs_dated_ahead(later);
        let shown = engine.last_synced_at(&id, later);
        let stale = Engine::stale(&in_a_drawer(&engine.stored_job(&id).unwrap(), shown), None, true, later.wall);
        assert_eq!(stale.as_deref(), Some("Not synced for 7 days"));
    }

    #[test]
    fn a_sync_found_dated_later_than_now_before_a_restart_still_counts_once_the_clock_is_past_it() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let (engine, job) = engine_with_a_job(tmp.path());
        let id = job.id.clone();
        // Launches before found the job's last sync dated later than now, and counted three
        // days since. The clock has passed that date before this launch.
        let synced = Utc::now() - chrono::Duration::hours(1);
        engine.pretend_the_last_sync_ended(&id, Moment { wall: synced, own: Instant::now() });
        let three_days = 3 * 24 * 60 * 60;
        engine.change_job(&id, |job| job.dated_ahead = Some(DatedAhead { last_synced: synced, counted_seconds: three_days })).unwrap();
        drop(engine);
        let engine = reopen(&data);
        let launch = Moment::now();
        engine.note_syncs_dated_ahead(launch);
        let shown = engine.last_synced_at(&id, launch).unwrap();
        assert!(shown <= launch.wall - chrono::Duration::days(3), "the row shows {shown}");
        assert_eq!(engine.stored_job(&id).unwrap().dated_ahead.map(|kept| kept.last_synced), Some(synced));

        // What was kept about another sync than the job's last one says nothing about it.
        let earlier = synced - chrono::Duration::days(1);
        engine.change_job(&id, |job| job.dated_ahead = Some(DatedAhead { last_synced: earlier, counted_seconds: three_days })).unwrap();
        drop(engine);
        let engine = reopen(&data);
        assert_eq!(engine.last_synced_at(&id, Moment::now()), Some(synced));
        engine.note_syncs_dated_ahead(Moment::now());
        assert_eq!(engine.stored_job(&id).unwrap().dated_ahead, None, "kept for the next launch still");
    }

    #[test]
    fn a_look_that_nothing_outdated_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, job) = engine_with_a_job(tmp.path());
        let flag = Arc::new(AtomicBool::new(false));
        let before = engine.change_runtime(&job.id, |rt| {
            rt.look = None;
            rt.checking = Some(flag.clone());
            rt.epoch
        });
        let survey = look_at(&engine, &job);
        engine.end_check(&job, &flag, before, Ok(survey));
        let rt = engine.runtime();
        let rt = rt.get(&job.id).unwrap();
        assert_eq!(rt.look.as_ref().map(|look| look.items), Some(1));
        assert!(rt.last_checked.is_some() && !rt.is_checking());
    }
}
