//! Syncing: runs the person confirmed, and runs of jobs with full automation.
//!
//! A run carries out a plan made from a survey. A confirmed run uses the very survey the
//! person looked at; a run nobody looked at makes a fresh one, and goes ahead only when
//! nothing about it needs a look (`Survey::holds`). Otherwise it is held back: nothing is
//! touched, and the job waits for the person's OK.
//!
//! While files are being carried, the job's record says so (`Job::run_in_progress`). Only
//! when every change is on the drives does Charon write the markers, save the two-way
//! memory and clear that mark. A run that never got there (Charon closed, the computer
//! stopped) is recognised at the next look, which then doesn't trust the memory.

use crate::engine::checks::ChangeAt;
use crate::engine::runtime::{Moment, OwnWrites, Wanted, MISSING_FOR};
use crate::engine::survey::{Purpose, Survey};
use crate::engine::quiet::{Asker, Claimed, Quiet};
use crate::engine::{Engine, EngineEvent, JobRuntime, Look};
use crate::error::{Error, Result};
use crate::model::{plural, Drives, Finished, Folder, Job, JobId, JobSetup, Mark, Phase, Place, Progress, ReviewKind, RunSummary, FAILED_NAMED, PLACES_SHOWN};
use crate::plan::{Action, Baseline, NoteKind, Plan, Side};
use crate::run::{BusyNote, Care, Claim, Executor, Found, Marker, Markers, Outcome, ProgressMeter, Shores, Spellings, Trash};
use crate::scan::{never_download_on_this_thread, ExcludeFilter, Keys, Scanner, Snapshot, MARKER_FOLDER};
use crate::store::{Landings, Memory, StoredState};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Why a job waits for an OK after the person stopped its sync.
pub(crate) const STOPPED: &str = "You stopped the last sync. Nothing more happens until you confirm the list.";
/// Why a job waits for an OK after a sync Charon couldn't record.
pub(crate) const UNRECORDED: &str = "Charon couldn't record the last sync, so nothing more happens until you confirm the list.";
/// Why a job with full automation waits for an OK after Charon couldn't count what an
/// automatic sync removed or replaced.
pub(crate) const UNCOUNTED: &str =
    "Charon couldn't count what the last automatic sync removed or replaced, so nothing more happens until you confirm the list.";
/// Why a job waits for an OK after its sync ended on a mistake in Charon itself.
pub(crate) const CRASHED: &str = "The last sync stopped on an error of Charon's own, so nothing more happens until you confirm the list.";
/// Why nothing syncs while an older Charon runs at the same time.
pub(crate) const OLDER_CHARON: &str = "An older version of Charon is running at the same time, and syncs your jobs by itself without this version's safety checks. \
     Quit it (from its icon in the menu bar or the system tray), then quit and reopen Charon. Until then, nothing syncs.";
/// Why runs nobody looks at wait while Charon's state can't be saved.
pub(crate) const CANNOT_SAVE: &str = "Charon can't save its jobs and settings right now (is the disk full?), so nothing syncs by itself until it can again.";
/// Why a run a trigger started doesn't begin.
const PAUSED: &str = "The job is paused, or everything is: nothing syncs by itself until you resume it.";
/// Why Sync all stopped starting jobs.
const PAUSED_DURING_SYNC_ALL: &str = "You paused everything while Sync all was starting the jobs, so it started no more of them.";
/// What a sync wrote is forgotten once it ended this long ago. Only the changes the folder
/// watcher reports right after a sync are told apart by it (see `note_change_while_syncing`).
const OWN_WRITES_KEPT: Duration = Duration::from_secs(60);
/// Live progress reaches the window at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);
/// At most this many failed or skipped items are written to the log per run.
const LOGGED_ITEMS: usize = 50;

/// What "Sync now" did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStart {
    /// The job runs by itself: its sync started.
    Started,
    /// The job waits for the person's OK: the window shows the list first.
    Review,
}

/// How a run came about.
#[derive(Debug, Clone)]
pub(crate) struct Crossing {
    pub kind: ReviewKind,
    /// The person confirmed the list.
    pub approved: bool,
    /// The person started it: they confirmed its list, or asked for it (Sync now, Sync
    /// all). Only a run a trigger started is held back while the job or everything is
    /// paused.
    pub by_hand: bool,
    /// For the log: "confirmed by you", "automatically on schedule".
    pub reason: String,
    /// The conflict this run settles (a resolution), by its key.
    pub resolving: Option<String>,
    /// The list is older than the folders: it waited for another sync, which changed them
    /// (heard of or not), and a list that waited is carried out all the same. What's left
    /// once it's done is for a new look to say.
    pub outdated: bool,
}

impl Crossing {
    /// A run nobody looks at (full automation), started by a trigger or `by_hand`.
    pub(crate) fn unattended(reason: &str, by_hand: bool) -> Crossing {
        Crossing { kind: ReviewKind::Sync, approved: false, by_hand, reason: reason.to_string(), resolving: None, outdated: false }
    }
}

/// What a run nobody looks at is started for (see `Engine::start_unreviewed`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Unreviewed<'a> {
    /// The person pressed Sync now.
    SyncNow,
    /// The job's wanted run, as it was read before the run began (see `Engine::start_wanted`).
    Wanted(&'a Wanted),
}

impl Unreviewed<'_> {
    /// How the run comes about.
    fn crossing(self) -> Crossing {
        match self {
            Unreviewed::SyncNow => Crossing::unattended("started by hand", true),
            Unreviewed::Wanted(wanted) => Crossing::unattended(&wanted.reason, wanted.by_hand),
        }
    }

    /// Whether the run is still asked for, as the job is now (`rt`, under the lock that marks
    /// it as running). The job's wanted run is only while the job still wants it as it was
    /// read: pausing may have called it off since, and one the person asked for would begin
    /// all the same, since pausing holds back only a trigger's run.
    fn still_asked_for(self, rt: &JobRuntime) -> bool {
        match self {
            Unreviewed::SyncNow => true,
            Unreviewed::Wanted(read) => rt.wanted.as_ref().is_some_and(|now| read.asks_as(now)),
        }
    }
}

/// For each other job: its setup as it was looked at, and whether it shares a folder with
/// the job about to start (worked out before any lock is taken, since it asks the disk).
pub(crate) struct Neighbours {
    mine: JobSetup,
    others: HashMap<JobId, (JobSetup, bool)>,
}

impl Engine {
    // ---------------------------------------------------------------- starting

    /// "Sync now": starts the sync of a job that runs by itself; any other job shows its
    /// list of changes first, and so does one that must ask after a stopped or held-back
    /// run (see `must_ask`). Works for paused jobs too: pausing only stops automatic runs.
    pub fn sync_now(&self, id: &JobId) -> Result<SyncStart> {
        let job = self.stored_job(id)?;
        if !job.runs_by_itself() || self.must_ask(id) {
            return Ok(SyncStart::Review);
        }
        self.start_unreviewed(id, Unreviewed::SyncNow)?;
        Ok(SyncStart::Started)
    }

    /// "Sync all": starts every job that runs by itself (not paused, folders reachable)
    /// and returns the other jobs that could sync now: their lists need the person's OK. A
    /// pause that comes meanwhile holds back the jobs it hasn't started yet: the job paused,
    /// or every job, which the answer says.
    pub fn sync_all(&self) -> Result<Vec<JobId>> {
        // Counted before the jobs are read, which may be minutes old by the time a job is
        // started (see `want_for_sync_all`).
        let pauses = self.pauses();
        let mut asking = Vec::new();
        let jobs = self.job_list();
        // Folders that don't answer in time count as away.
        let whereabouts = self.whereabouts_of(&jobs);
        for (job, found) in jobs.into_iter().zip(whereabouts) {
            // A job whose confirmed list waits for another sync is on its way already.
            if job.paused || !found.both_there() || self.is_running(&job.id) || self.confirmed_waits(&job.id) {
                continue;
            }
            if job.runs_by_itself() && !self.must_ask(&job.id) {
                // Another job using the folders makes it wait, not give up (see `start_wanted`),
                // even under *Pause everything*: the person asked for it.
                if self.want_for_sync_all(&job.id, pauses) {
                    self.start_wanted(&job.id);
                }
            } else {
                asking.push(job.id);
            }
        }
        if self.pauses() != pauses {
            return Err(Error::Invalid(PAUSED_DURING_SYNC_ALL.into()));
        }
        Ok(asking)
    }

    /// Stops a running sync (what's already carried stays), or calls off a confirmed one
    /// that waits for another sync. The next sync of the job waits for the person's OK.
    pub fn cancel(&self, id: &JobId) {
        // Under one lock: a confirmed sync that starts now either sees it called off, or
        // runs with its stop flag set.
        let called_off = self.change_known_runtime(id, |rt| {
            rt.stop_for_person();
            rt.call_off_confirmed()
        });
        if called_off == Some(true) {
            self.note_called_off(id, "");
            self.publish(id);
        }
    }

    /// Pausing the job: stops it as Stop does, and drops the run it wants, even one the
    /// person asked for (Sync all, or Sync now that met another sync): they paused it since.
    /// Under one lock, as Stop: a run that begins now either runs with its stop flag set, or
    /// finds its wanted run called off (see `start_unreviewed`).
    pub(crate) fn stop_paused(&self, id: &JobId) {
        let called_off = self.change_known_runtime(id, |rt| {
            rt.stop_for_person();
            (rt.call_off_confirmed(), rt.forget_wanted(|_| true))
        });
        let Some((confirmed, waited_for)) = called_off else { return };
        if confirmed {
            self.note_called_off(id, "");
        }
        if let Some(who) = &waited_for {
            self.note_wanted_called_off(id, who, "the job is paused");
        }
        if confirmed || waited_for.is_some() {
            self.publish(id);
        }
    }

    /// *Pause everything*: nothing syncs by itself now, and nothing starts without the person.
    /// Stops every sync nobody confirmed a list for, as Stop would (what it already carried
    /// stays, and the next sync of its job waits for the person's OK), drops every wanted run,
    /// and calls off every confirmed list that waits for another sync. A sync the person
    /// confirmed that's at work goes on.
    ///
    /// All under one lock, which also counts the pause (see `pauses`). A wanted run that
    /// begins now either is stopped here, or finds itself called off (see `start_unreviewed`).
    /// A confirmed list whose sync is about to start is marked as running, so it isn't
    /// called off here, but sees the count went up, and is called off where it ends up (see
    /// `carry_out`). And Sync all starts no job it hadn't got to (see `want_for_sync_all`).
    pub(crate) fn pause_everything(&self) {
        let (mut stopped, mut dropped, mut called_off) = (Vec::new(), Vec::new(), Vec::new());
        {
            let mut runtime = self.runtime();
            self.inner.pauses.fetch_add(1, Ordering::SeqCst);
            for (id, rt) in runtime.iter_mut() {
                // One already stopping (Stop, Charon quitting) ends as it was going to.
                if rt.cancel.as_ref().is_some_and(|cancel| rt.unattended && !cancel.swap(true, Ordering::Relaxed)) {
                    rt.stopped_by_person = true;
                    stopped.push(id.clone());
                }
                if let Some(who) = rt.forget_wanted(|_| true) {
                    dropped.push((id.clone(), who));
                }
                if !rt.is_running() && rt.call_off_confirmed() {
                    called_off.push(id.clone());
                }
            }
        }
        for id in stopped {
            self.inner.journal.info(&id, "everything paused: this sync stops, since you didn't confirm its list");
        }
        for (id, who) in dropped {
            self.note_wanted_called_off(&id, &who, "everything is paused");
            self.publish(&id);
        }
        for id in called_off {
            self.note_called_off(&id, " (Pause everything)");
            self.publish(&id);
        }
    }

    /// Calls off every confirmed sync that waits for another sync (Charon quitting). A sync
    /// the person confirmed that already started isn't stopped.
    pub(crate) fn call_off_all_confirmed(&self, why: &str) {
        let called_off: Vec<JobId> = self
            .runtime()
            .iter_mut()
            .filter_map(|(id, rt)| (!rt.is_running() && rt.call_off_confirmed()).then(|| id.clone()))
            .collect();
        for id in called_off {
            self.note_called_off(&id, why);
            self.publish(&id);
        }
    }

    /// Says in the job's log that its confirmed sync was called off, and why (`why` is
    /// empty when the person called it off themselves: Stop, closing the list, pausing).
    pub(crate) fn note_called_off(&self, id: &JobId, why: &str) {
        self.inner.journal.info(id, &format!("the confirmed sync that waited for another sync was called off{why}"));
    }

    /// Whether the job has a confirmed list that waits for another sync.
    fn confirmed_waits(&self, id: &JobId) -> bool {
        self.runtime().get(id).is_some_and(|rt| rt.confirmed.is_some())
    }

    /// Whether runs nobody looks at are refused for this job until the person confirms a
    /// list (see `JobRuntime::must_ask`).
    pub(crate) fn must_ask(&self, id: &JobId) -> bool {
        self.runtime().get(id).is_some_and(|rt| rt.must_ask)
    }

    /// Whether a sync of this job is running.
    pub fn is_running(&self, id: &JobId) -> bool {
        self.runtime().get(id).is_some_and(|rt| rt.is_running())
    }

    /// Whether a sync of the job is running, or one ended less than `time` ago.
    pub fn synced_within(&self, id: &JobId, time: Duration) -> bool {
        self.runtime()
            .get(id)
            .is_some_and(|rt| rt.is_running() || rt.finished_at.is_some_and(|t| t.elapsed() < time))
    }

    /// Notes that a job with full automation should sync, for a trigger (`reason` is for
    /// the log). The run starts with `start_wanted`, now or as soon as nothing stands in its
    /// way.
    pub fn want(&self, id: &JobId, reason: &str) {
        self.change_runtime(id, |rt| Self::keep_wanted(rt, Wanted { reason: format!("automatically {reason}"), by_hand: false }));
    }

    /// Notes that the person asked a job with full automation to sync with Sync all, which
    /// read the jobs when everything had been paused `pauses` times: unlike a trigger's run,
    /// it still starts while everything is paused. Not once the person paused the job, or
    /// everything, since then: that pause came while Sync all was at work, and wins. Returns
    /// whether the job wants the run.
    fn want_for_sync_all(&self, id: &JobId, pauses: u64) -> bool {
        // Read where pausing saves it, and counts it (see `set_paused`, `pause_everything`):
        // a pause that comes after this drops the wanted run.
        let state = self.state();
        let mut runtime = self.runtime();
        let paused = state.jobs.iter().find(|job| &job.id == id).is_none_or(|job| job.paused);
        if paused || self.pauses() != pauses {
            return false;
        }
        Self::keep_wanted(runtime.entry(id.clone()).or_default(), Wanted { reason: "started with Sync all".into(), by_hand: true });
        true
    }

    /// Keeps `wanted` as the job's wanted run, unless it has one.
    fn keep_wanted(rt: &mut JobRuntime, wanted: Wanted) {
        match &mut rt.wanted {
            // Once the person asked for it, pausing doesn't drop it.
            Some(due) => due.by_hand |= wanted.by_hand,
            None => rt.wanted = Some(wanted),
        }
    }

    /// Forgets a wanted run: it can't start now (`why`: a folder is away, or the job waits
    /// for the person's OK).
    pub fn unwant(&self, id: &JobId, why: &str) {
        self.drop_wanted(id, |_| true, why);
    }

    /// Forgets a wanted run a trigger started: the job or everything is paused (`why`). One
    /// the person asked for (Sync all) still starts once nothing stands in its way.
    pub fn unwant_automatic(&self, id: &JobId, why: &str) {
        self.drop_wanted(id, |wanted| !wanted.by_hand, why);
    }

    /// Forgets the job's wanted run, if `which` says so. When its row said it waited for
    /// another sync, the job's log says it was called off, and why.
    pub(crate) fn drop_wanted(&self, id: &JobId, which: impl FnOnce(&Wanted) -> bool, why: &str) {
        let waited_for = self.change_known_runtime(id, |rt| rt.forget_wanted(which)).flatten();
        if let Some(who) = waited_for {
            self.note_wanted_called_off(id, &who, why);
            self.publish(id);
        }
    }

    /// Says in the job's log that its wanted run, which waited for `who` (another sync, or
    /// another job's look or list on the same drive), was called off, and why.
    fn note_wanted_called_off(&self, id: &JobId, who: &str, why: &str) {
        self.inner.journal.info(id, &format!("the sync that waited for “{who}” was called off: {why}"));
    }

    /// Starts the job's wanted run, if it has one. While another sync uses the folders, or
    /// another job holds a drive it needs (see `quiet.rs`), it keeps waiting (the row says for
    /// whom); any other refusal drops it.
    pub fn start_wanted(&self, id: &JobId) {
        let Some(wanted) = self.runtime().get(id).and_then(|rt| rt.wanted.clone()) else { return };
        self.start_as_wanted(id, &wanted);
    }

    /// Starts `wanted`, the job's wanted run as `start_wanted` read it. It may have changed
    /// since (pausing called it off, Sync all asked for it): the run begins only while the job
    /// still wants it so, and a refusal drops it only while it's still so.
    fn start_as_wanted(&self, id: &JobId, wanted: &Wanted) {
        match self.start_unreviewed(id, Unreviewed::Wanted(wanted)) {
            // A run that started took the wanted run along (see `begin_run`); the job's own
            // sync keeps it waiting. One no longer wanted so has nothing to start: the next
            // tick starts what's wanted now.
            Ok(()) | Err(Error::Busy) | Err(Error::Cancelled) => {}
            Err(Error::FoldersBusy(name)) | Err(Error::DriveBusy(name)) => {
                // Only while it's still wanted: pausing may have dropped it meanwhile.
                let changed = self.change_runtime(id, |rt| rt.wanted.is_some() && rt.waiting_for.replace(name.clone()) != Some(name.clone()));
                if changed {
                    log::info!("job {id}: automatic sync waits for {name}");
                    self.publish(id);
                }
            }
            Err(e) => {
                log::info!("job {id}: automatic sync not started: {e}");
                self.drop_wanted(id, |now| wanted.asks_as(now), &e.to_string());
            }
        }
    }

    /// Starts a run nobody looks at (full automation): it surveys first, and holds back for
    /// the person's OK when anything about the plan needs a look. When all that keeps it
    /// from going ahead is another job's sync, which keeps a drive quiet that Charon would
    /// ask whether two folders are one (see `overlaps::Waiting`), it waits for that sync
    /// instead, as the job's wanted run, which doesn't look again until that sync is done. A
    /// run a trigger started doesn't start while the job or everything is paused, and the
    /// job's wanted run only while the job still wants it (`Cancelled` otherwise).
    pub(crate) fn start_unreviewed(&self, id: &JobId, start: Unreviewed) -> Result<()> {
        // A run whose end can't be recorded mustn't start by itself (a failed save tries
        // again at most once a minute).
        if !self.can_save() {
            return Err(Error::Invalid(CANNOT_SAVE.into()));
        }
        let crossing = start.crossing();
        // A wanted run that waits for a sync that keeps another job's drive quiet (see the
        // check below) is told so from memory, before anything is asked of the disk.
        if let Some(name) = self.still_waits(id, start, &crossing) {
            return Err(Error::DriveBusy(name));
        }
        let (job, cancel) = self.begin_run(id, &crossing, |job, rt| {
            Self::unreviewed_refusal(start, job, rt)?;
            // A wanted run that couldn't tell whether a folder on a drive kept quiet overlaps
            // with this job's waits until no sync keeps it quiet, without looking again.
            if let (Unreviewed::Wanted(_), Some(folder)) = (start, &rt.waits_on) {
                match self.inner.quiet.kept_by_another(id, &[folder]) {
                    Some(name) => return Err(Error::DriveBusy(name)),
                    None => rt.waits_on = None,
                }
            }
            Ok(())
        })?;
        self.publish(id);
        self.spawn_crossing(job, cancel, crossing, |engine, job, cancel, crossing| match engine.survey(&job, &cancel, Purpose::Automatic) {
            // Only another job's sync, which keeps a drive quiet, keeps this look from telling
            // whether the job's folders overlap another job's: the run waits for that sync,
            // rather than hold back for the person's OK (see `overlaps::Waiting`).
            Ok(Survey { waits_on: Some(waits), .. }) => {
                engine.change_runtime(&job.id, |rt| rt.waits_on = Some(waits.folder.clone()));
                if engine.not_begun(&job, &crossing, &waits.syncing) {
                    engine.inner.journal.info(
                        &job.id,
                        &format!(
                            "Charon found the hidden .charon folder of “{}” in {}, and can't tell yet whether that's that job's folder or holds a copy of it: \
                             “{}” is syncing on that drive, and Charon asks once it's done. If it's a copy, you can delete that .charon folder.",
                            waits.whose,
                            waits.found.display(),
                            waits.syncing
                        ),
                    );
                }
            }
            Ok(survey) if !survey.holds.is_empty() => engine.hold(&job, &survey),
            Ok(mut survey) => {
                engine.hold_back_recent_removals(&job, &mut survey, Moment::now());
                match engine.claim_folders(&job, &survey) {
                    Ok(claim) => engine.cross(&job, &survey, &cancel, &crossing, claim),
                    Err(busy) => {
                        engine.not_begun(&job, &crossing, busy.waits_for().unwrap_or_default());
                    }
                }
            }
            Err(e) => engine.unfinished(&job, &crossing, e),
        })
    }

    /// Why a run nobody looks at can't start as the job is now (`rt`), if it can't: checked
    /// where `begin_run` marks it as running (see `start_unreviewed`).
    fn unreviewed_refusal(start: Unreviewed, job: &Job, rt: &JobRuntime) -> Result<()> {
        // Checked where `begin_run` takes the wanted run along, so a pause that called it off
        // either finds the run going (and stops it), or this finds it called off.
        if !start.still_asked_for(rt) {
            return Err(Error::Cancelled);
        }
        if !job.runs_by_itself() || rt.must_ask {
            return Err(Error::Invalid("This job waits for your OK before it syncs.".into()));
        }
        if rt.confirmed.is_some() {
            return Err(Error::Invalid("You confirmed a list of this job's changes, and its sync waits for another sync to finish.".into()));
        }
        if rt.preparing.is_some() || rt.review.as_ref().is_some_and(|review| Utc::now() < review.expires_at) {
            return Err(Error::Invalid("A list of this job's changes is open, waiting for your OK.".into()));
        }
        Ok(())
    }

    /// The sync that the job's wanted run still waits for, as the check in
    /// `start_unreviewed` finds it, told from memory: only where `begin_run` and that check
    /// would get as far as the wait, so answering before them changes nothing but the disk
    /// work it saves.
    fn still_waits(&self, id: &JobId, start: Unreviewed, crossing: &Crossing) -> Option<String> {
        if !matches!(start, Unreviewed::Wanted(_)) || self.older_charon_running() {
            return None;
        }
        let state = self.state();
        let job = state.jobs.iter().find(|job| &job.id == id)?;
        let runtime = self.runtime();
        let rt = runtime.get(id)?;
        let folder = rt.waits_on.as_ref()?;
        if self.may_begin(&state, &runtime, job, crossing).is_err() || Self::unreviewed_refusal(start, job, rt).is_err() {
            return None;
        }
        self.inner.quiet.kept_by_another(id, &[folder])
    }

    /// Leaves for a later run the removals that follow an item missing for only a few
    /// minutes on a drive that may show changes late (see `Missing`), with the other changes
    /// to their databases, and says so in the log. That run comes by itself once they've
    /// waited long enough, whatever starts the job's syncs otherwise (see `take_follow_up`).
    fn hold_back_recent_removals(&self, job: &Job, survey: &mut Survey, now: Moment) {
        let held = self.change_missing(job, |missing| missing.hold_back_recent(&mut survey.plan, &survey.source, &survey.destination, now));
        survey.left_for_later = Plan { actions: held, notes: Vec::new() };
        let removals = survey.left_for_later.removals();
        if removals == 0 {
            return;
        }
        let mut line = format!(
            "left {} for a later sync: the items are missing on a network share or a cloud drive, which can show changes late, \
             so Charon waits until looks have found them missing for {} minutes",
            plural(removals, "removal"),
            MISSING_FOR.as_secs() / 60
        );
        let along = survey.left_for_later.actions.len() as u64 - removals;
        if along > 0 {
            line.push_str(&format!(
                ". {} to the same database {} with them, as a database's files only go together",
                plural(along, "more change"),
                if along == 1 { "waits" } else { "wait" }
            ));
        }
        self.inner.journal.info(&job.id, &line);
    }

    /// Why a run of `job` can't begin as Charon is now, if it can't (under the locks of the
    /// stored `state` and the `runtime`).
    fn may_begin(&self, state: &StoredState, runtime: &HashMap<JobId, JobRuntime>, job: &Job, crossing: &Crossing) -> Result<()> {
        if self.is_shutting_down() {
            return Err(Error::Invalid("Charon is quitting.".into()));
        }
        if runtime.get(&job.id).is_some_and(|rt| rt.is_running()) {
            return Err(Error::Busy);
        }
        // Read where pausing saves it: a pause saved after this finds the run going, and
        // stops it (see `set_paused`, `set_all_paused`).
        if !crossing.by_hand && (job.paused || state.settings.paused_all) {
            return Err(Error::Invalid(PAUSED.into()));
        }
        Ok(())
    }

    /// Marks the job as running for `crossing`, if it may run now: Charon isn't quitting, the
    /// job isn't running already, a run a trigger started finds neither the job nor
    /// everything paused, `check` (the caller's own conditions) passes, and no other sync is
    /// at work in its folders: no running job of this Charon by their paths, and no sync of
    /// this or another Charon by its busy notes (see `run::Claim`; the run leaves its own
    /// notes before it changes anything). Nor is another job at work on a drive of its folders
    /// that's run by an add-on driver (see `quiet.rs`): its run, a list being made for it, or
    /// for a run nobody asked for, a look of it in the background. A run the person asked for
    /// stops such a look instead.
    pub(crate) fn begin_run(
        &self,
        id: &JobId,
        crossing: &Crossing,
        check: impl FnOnce(&Job, &mut JobRuntime) -> Result<()>,
    ) -> Result<(Job, Arc<AtomicBool>)> {
        if self.older_charon_running() {
            return Err(Error::Invalid(OLDER_CHARON.into()));
        }
        // Both ask the disk, so they come before any lock is taken. Not while another job
        // uses a drive of the folders that's run by an add-on driver: nothing is asked of it
        // then, not even the busy notes on it, and the run waits for that job (below, after
        // the caller's checks, so a list that can't go ahead anyway is refused now). A run a
        // trigger started waits for a look in the background there too (stopping it every
        // time could keep that job from ever being looked at), and takes its turn after the
        // jobs that began to wait for the drive before it.
        let asker = if crossing.by_hand || crossing.approved { Asker::Person } else { Asker::Charon };
        let drive_used = self.drive_used_by_another(id, asker);
        let (neighbours, in_use) = match drive_used {
            Some(_) => (Neighbours { mine: Self::nowhere(), others: HashMap::new() }, None),
            None => (self.neighbours(id), self.folders_in_use(id)),
        };
        let state = self.state();
        let job = state.jobs.iter().find(|job| &job.id == id).cloned().ok_or_else(|| Error::UnknownJob(id.clone()))?;
        let mut runtime = self.runtime();
        self.may_begin(&state, &runtime, &job, crossing)?;
        check(&job, runtime.entry(id.clone()).or_default())?;
        if let Some(name) = drive_used.or_else(|| self.drive_used_among(&state.jobs, &runtime, &job, asker)) {
            if asker == Asker::Charon {
                self.note_drive_wait(runtime.entry(id.clone()).or_default());
            }
            return Err(Error::DriveBusy(name));
        }
        // Its turn on the drive came (whatever else may still hold it back).
        runtime.entry(id.clone()).or_default().drive_wait = None;
        if let Some(name) = Self::busy_neighbour(&state.jobs, &runtime, &job, &neighbours).or(in_use) {
            return Err(Error::FoldersBusy(name));
        }
        self.stop_looks_sharing_a_drive(&state.jobs, &mut runtime, &job);
        let rt = runtime.entry(id.clone()).or_default();
        let cancel = Arc::new(AtomicBool::new(false));
        rt.cancel = Some(cancel.clone());
        rt.unattended = !crossing.approved;
        rt.stopped_by_person = false;
        rt.progress = Some(Progress::scanning());
        rt.problem = None;
        // This run does what a wanted run would have done.
        rt.wanted = None;
        rt.waiting_for = None;
        rt.waits_on = None;
        // A look at folders that are about to change would be wrong at once.
        rt.outdate_looks();
        // From now on, changes in the folders are told apart from the run's own writes.
        rt.own_writes = Some(OwnWrites::default());
        Ok((job, cancel))
    }

    /// Who else is at work in the job's folders by their busy notes, if anyone: a job of
    /// this Charon whose folders are the same place under other names, or another Charon
    /// (asks the disk; done before locking). Not asked while another job's sync keeps a drive
    /// of the folders quiet (see `quiet.rs`): the run or the list that asks waits for that job
    /// anyway, and a sync's claim reads the notes again before it changes anything.
    pub(crate) fn folders_in_use(&self, id: &JobId) -> Option<String> {
        let job = self.stored_job(id).ok()?;
        if self.kept_quiet_by_another(&job).is_some() {
            return None;
        }
        let inside = self.runtime().get(id).map(|rt| rt.folders_inside.clone()).unwrap_or_default();
        let me = self.inner.busy.note(id, &job.setup.name);
        self.inner.busy.around(&[&job.setup.source, &job.setup.destination], &inside, &me).first().map(|note| self.busy_with(note))
    }

    /// Leaves the run's busy notes in the job's folders and in the Charon folders inside them
    /// (see `run::Claim`), unless another sync is at work there or in the folders above and
    /// inside them (`Error::FoldersBusy`, saying who). The drives of its folders that are run
    /// by an add-on driver are kept quiet first, unless another job's sync keeps one of them
    /// quiet already (`Error::DriveBusy`; see `quiet.rs`).
    pub(crate) fn claim_folders(&self, job: &Job, survey: &Survey) -> Result<Claimed> {
        let quiet = self.keep_quiet(job, survey)?;
        let inside = Self::folders_inside_now(survey);
        let note = self.inner.busy.note(&job.id, &job.setup.name);
        let roots = [survey.setup.source.as_path(), survey.setup.destination.as_path()];
        let notes = Claim::take_keeping_quiet(&self.inner.busy, note, &roots, &inside, &quiet.as_ref().map(Quiet::roots).unwrap_or_default())
            .map_err(|other| Error::FoldersBusy(self.busy_with(&other)))?;
        Ok(Claimed::new(notes, quiet))
    }

    /// The folders inside the job's folders where the scans met Charon's own folder.
    pub(crate) fn folders_inside(setup: &JobSetup, source: &Snapshot, destination: &Snapshot) -> Vec<PathBuf> {
        [(Side::Source, source), (Side::Destination, destination)]
            .into_iter()
            .flat_map(|(side, snapshot)| {
                let root = setup.root(side);
                snapshot.charon_folders().iter().filter(|found| !found.folder.is_empty()).map(move |found| root.join(&found.folder))
            })
            .collect()
    }

    /// The folders inside the job's folders that hold Charon's own folder, for a run about to
    /// carry out `survey`'s plan: where the scans met one, and where one is now in a folder
    /// whose contents the plan changes. A sync of a folder inside may have started since the
    /// scans (for a confirmed list, maybe hours ago) and made Charon's folder there: its
    /// notes must be seen before this run changes anything in its folder.
    ///
    /// The folders are taken as they are now, not as the scans saw them: since then a folder
    /// may have been replaced by a link, or a link by a folder. (Where the scans met one is
    /// checked the same way when the notes are read and left: see `run::Claim`.)
    fn folders_inside_now(survey: &Survey) -> Vec<PathBuf> {
        let mut inside: BTreeSet<PathBuf> = Self::folders_inside(&survey.setup, &survey.source, &survey.destination).into_iter().collect();
        let spellings = Spellings::new(&survey.source, &survey.destination);
        for side in [Side::Source, Side::Destination] {
            let changed: BTreeSet<&str> = survey.plan.actions.iter().filter(|action| action.changes(side)).flat_map(Action::folders).collect();
            // A folder comes before the folders inside it. One that isn't a folder of its own
            // here now (it's new to this side and not there yet, or it's a link) has nothing
            // inside it to look at: Charon never looks through a link.
            let mut not_folders: HashSet<&str> = HashSet::new();
            for key in changed {
                if Keys::ancestors(key).any(|above| not_folders.contains(above)) {
                    continue;
                }
                let folder = survey.setup.root(side).join(spellings.disk(side, key));
                if !fs::symlink_metadata(&folder).is_ok_and(|meta| meta.is_dir()) {
                    not_folders.insert(key);
                    continue;
                }
                // Only a folder that is Charon's own: one of the person's of that name is
                // left alone.
                let charons = folder.join(MARKER_FOLDER);
                if fs::symlink_metadata(&charons).is_ok_and(|meta| meta.is_dir()) && Scanner::looks_like_charons(&charons, MARKER_FOLDER, true) {
                    inside.insert(folder);
                }
            }
        }
        inside.into_iter().collect()
    }

    /// Who left a busy note, as the person knows them: a job of this Charon by its name, or
    /// another Charon (on another computer, by its name, when the note says).
    fn busy_with(&self, note: &BusyNote) -> String {
        if note.instance == self.inner.busy.instance() {
            return self.stored_job(&note.job).map(|job| job.setup.name).unwrap_or_else(|_| note.name.clone());
        }
        if note.host.is_empty() || note.from_this_computer() {
            "another Charon".into()
        } else {
            format!("Charon on {}", note.host)
        }
    }

    /// Ends a run that found another sync (`who`) at work in its folders right before it
    /// would have changed anything, or on a drive it had to ask about (see
    /// `start_unreviewed`): nothing happened. A run nobody looks at is wanted again,
    /// and starts once that sync is done; a confirmed run is kept waiting by the one who
    /// started it (see `approve`). Returns false when the person stopped the run meanwhile:
    /// it then ends as stopped. (Syncs that waited for this one are started by the autopilot
    /// within seconds: starting them here could go back and forth between two waiting jobs.)
    pub(crate) fn not_begun(&self, job: &Job, crossing: &Crossing, who: &str) -> bool {
        let id = &job.id;
        let stopped = self.change_runtime(id, |rt| {
            if !Self::unmark_run(rt) {
                return true;
            }
            if !crossing.approved {
                rt.wanted = Some(Wanted { reason: crossing.reason.clone(), by_hand: crossing.by_hand });
                rt.waiting_for = Some(who.to_string());
            }
            false
        });
        if stopped {
            self.unfinished(job, crossing, Error::Cancelled);
            return false;
        }
        if !crossing.approved {
            self.inner.journal.info(id, &format!("waits for “{who}” to finish syncing"));
        }
        self.publish(id);
        true
    }

    /// Takes back what `begin_run` marked for a run that ends before it changed anything.
    /// Returns false, and leaves it marked, when the person stopped the run meanwhile: it then
    /// ends as stopped (see `unfinished`).
    pub(crate) fn unmark_run(rt: &mut JobRuntime) -> bool {
        if rt.cancel.as_ref().is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return false;
        }
        rt.cancel = None;
        rt.progress = None;
        Self::look_again_if_changed(rt);
        true
    }

    /// Which other jobs share a folder with `id` (asks the disk, with a time limit; done
    /// before locking). A job whose folders don't answer in time counts as sharing: where
    /// they lead can't be told, so a sync of it at work keeps this job waiting.
    pub(crate) fn neighbours(&self, id: &JobId) -> Neighbours {
        let jobs = self.job_list();
        let Some(me) = jobs.iter().find(|job| &job.id == id) else {
            return Neighbours { mine: Self::nowhere(), others: HashMap::new() };
        };
        let others: Vec<&Job> = jobs.iter().filter(|other| other.id != me.id).collect();
        let paths: Vec<&Path> =
            std::iter::once(me).chain(others.iter().copied()).flat_map(|job| [job.setup.source.as_path(), job.setup.destination.as_path()]).collect();
        let folders = self.folders_of(&paths);
        let (mine, theirs) = folders.split_at(2);
        let others = others
            .iter()
            .zip(theirs.chunks_exact(2))
            .map(|(other, theirs)| (other.id.clone(), (other.setup.clone(), Self::may_share(mine, theirs))))
            .collect();
        Neighbours { mine: me.setup.clone(), others }
    }

    /// Whether two jobs' folders may be one place, or one inside the other: they are, or
    /// one of them didn't answer (`None`).
    fn may_share(mine: &[Option<Folder>], theirs: &[Option<Folder>]) -> bool {
        mine.iter().any(|a| {
            theirs.iter().any(|b| match (a, b) {
                (Some(a), Some(b)) => a.overlaps(b),
                _ => true,
            })
        })
    }

    /// The name of a running job that shares a folder with `job`, if there is one. A job
    /// whose setup changed since the neighbours were worked out counts as sharing.
    pub(crate) fn busy_neighbour(jobs: &[Job], runtime: &HashMap<JobId, JobRuntime>, job: &Job, neighbours: &Neighbours) -> Option<String> {
        jobs.iter()
            .filter(|other| other.id != job.id && runtime.get(&other.id).is_some_and(|rt| rt.is_running()))
            .find(|other| match neighbours.others.get(&other.id) {
                Some((setup, shares)) if *setup == other.setup && neighbours.mine == job.setup => *shares,
                _ => true,
            })
            .map(|other| other.setup.name.clone())
    }

    fn nowhere() -> JobSetup {
        JobSetup {
            name: String::new(),
            source: Default::default(),
            destination: Default::default(),
            mode: crate::model::SyncMode::Backup,
            conflict: crate::model::ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        }
    }

    /// Runs `work` on its own thread for a job marked as running. The thread never downloads
    /// files that are only in the cloud: one the cloud took back since the list was made is
    /// left alone, as the scan leaves such files alone.
    pub(crate) fn spawn_crossing(
        &self,
        job: Job,
        cancel: Arc<AtomicBool>,
        crossing: Crossing,
        work: impl FnOnce(&Engine, Job, Arc<AtomicBool>, Crossing) + Send + 'static,
    ) -> Result<()> {
        let engine = self.clone();
        let id = job.id.clone();
        let spawned = thread::Builder::new().name("charon-sync".into()).spawn(move || {
            never_download_on_this_thread();
            let (id, started_at, approved) = (job.id.clone(), Utc::now(), crossing.approved);
            let ended = std::panic::catch_unwind(AssertUnwindSafe(|| work(&engine, job, cancel, crossing)));
            if ended.is_err() {
                engine.after_a_panic(&id, started_at, approved);
            }
        });
        if let Err(e) = spawned {
            self.change_runtime(&id, |rt| {
                rt.cancel = None;
                rt.progress = None;
            });
            self.publish(&id);
            return Err(Error::Io(e));
        }
        Ok(())
    }

    /// A sync ended on a mistake in Charon itself (a panic) before it was recorded. What it
    /// carried isn't known: the job's record still has the mark set before anything was
    /// carried, so the next look doesn't trust the two-way memory, and the next sync waits
    /// for the person's OK.
    fn after_a_panic(&self, id: &JobId, started_at: DateTime<Utc>, approved: bool) {
        log::error!("a sync of job {id} ended on a panic");
        let mut summary = RunSummary::starting(started_at, approved);
        summary.finished_at = Utc::now();
        summary.error = Some(CRASHED.into());
        let held = self.change_job_anyway(id, |stored| {
            stored.last_run = Some(summary.clone());
            stored.needs_review = true;
            stored.held = Some(CRASHED.into());
        });
        if let Err(e) = held {
            log::error!("couldn't save that job {id} waits for an OK: {e}");
        }
        self.inner.journal.error(id, "the sync stopped on an error of Charon's own; the next sync waits for your OK");
        self.change_runtime(id, |rt| {
            rt.cancel = None;
            rt.stopped_by_person = false;
            rt.must_ask = true;
            rt.progress = None;
            rt.finished_at = Some(Instant::now());
            rt.problem = summary.error.clone();
            rt.last_checked = None;
        });
        if let Ok(job) = self.stored_job(id) {
            self.emit(EngineEvent::Finished(Finished { id: id.clone(), name: job.setup.name, summary }));
        }
        self.publish(id);
    }

    // ---------------------------------------------------------------- carrying out

    /// Carries out the plan of `survey`, makes it durable and records it. Runs on the
    /// job's sync thread, with the job marked as running and its folders claimed (`claim`,
    /// given up before the run's end is announced, so a sync waiting for it can start). The
    /// busy notes on drives kept quiet are refreshed as it goes, between steps and between
    /// the pieces of a file it copies (see `quiet.rs`).
    pub(crate) fn cross(&self, job: &Job, survey: &Survey, cancel: &AtomicBool, crossing: &Crossing, claim: Claimed) {
        let id = &job.id;
        let journal = &self.inner.journal;
        let started_at = Utc::now();
        let mut summary = RunSummary::starting(started_at, crossing.approved);
        let steps = survey.plan.work().count() as u64;
        journal.info(id, &format!("sync {}: {}", crossing.reason, plural(steps, "step")));
        self.change_runtime(id, |rt| rt.own_writes = Some(OwnWrites::of(&survey.plan, survey.folded, rt.own_writes.as_ref())));

        // Until this run is recorded, the job's record says files are being carried.
        if let Err(e) = self.change_job(id, |job| job.run_in_progress = true) {
            summary.error = Some(format!("Charon couldn't save its state before syncing ({e}), so it changed nothing."));
            let unwatched_since = claim.unwatched_since();
            drop(claim);
            self.finish(job, survey, crossing, summary, None, unwatched_since);
            return;
        }

        let two_way = survey.setup.mode.is_two_way();
        let resolving = crossing.kind == ReviewKind::Resolve;
        let shores = Shores { source: survey.setup.source.clone(), destination: survey.setup.destination.clone() };
        let care = Care { safe_delete: survey.setup.safe_delete || resolving, keep_replaced: two_way || resolving, two_way };
        let observer = |progress: &Progress| {
            claim.beat();
            self.report_progress(id, progress)
        };
        let mut meter = ProgressMeter::for_plan(&survey.plan, &observer);
        // An item a step makes under a name the plan doesn't have (a conflict copy) is this
        // run's own write from right before it's made: a change the folder watcher hears
        // there is no change of the person's.
        let new_name = |key: &str| {
            self.change_runtime(id, |rt| {
                if let Some(own) = rt.own_writes.as_mut() {
                    own.add(std::iter::once(key));
                }
            })
        };
        let executor =
            Executor::new(&shores, &survey.source, &survey.destination, care, cancel).excluding(survey.filter.clone()).telling_new_names(&new_name);
        let mut outcome = executor.run(&survey.plan, &mut meter);

        // The housekeeping the list named, only after a run that went all the way.
        let complete = !outcome.cancelled && outcome.stopped.is_none();
        let mut tidied = 0;
        if complete {
            for (side, leftovers) in &survey.housekeeping.leftovers {
                claim.beat();
                tidied += executor.tidy(*side, leftovers);
            }
        }
        claim.beat();
        let flushed = executor.flush(&mut outcome);
        // Old trash goes only after a list that said so was confirmed. A run nobody looked at
        // never empties it: the originals in there may be the only good copies of files a
        // broken app is slowly damaging (see `Guard::since_confirmed`). And never more than
        // *Keep deleted files* allows now, which may have changed while the list waited.
        if complete && flushed.is_ok() && crossing.approved {
            let days = self.settings().trash_days;
            for (side, look) in &survey.housekeeping.trash {
                claim.beat();
                let names: Vec<String> = look.expired.iter().map(|e| e.name.clone()).collect();
                Trash::sweep(survey.setup.root(*side), &names, days, survey.snapshot(*side).shifts_times());
            }
        }
        claim.beat();

        summary.copied = outcome.copied;
        summary.deleted = outcome.deleted;
        summary.bytes = outcome.bytes;
        summary.conflicts = survey.plan.conflicts().len() as u64;
        summary.failed = outcome.failed.len() as u64;
        summary.failed_items = outcome.failed.iter().take(FAILED_NAMED).map(|(path, _)| survey.spelling(path)).collect();
        summary.skipped = outcome.skipped.len() as u64;
        summary.waited = outcome.waited.len() as u64;
        summary.left_out = outcome.left_out.len() as u64;
        summary.kept = outcome.kept.len() as u64;
        // Places the look couldn't read, and files this run couldn't.
        summary.found_unreadable = outcome.unread.len() as u64;
        summary.unreadable = survey.plan.notes.iter().filter(|n| n.kind == NoteKind::Unreadable).count() as u64 + summary.found_unreadable;
        summary.cancelled = outcome.cancelled;
        let settled = if crossing.kind == ReviewKind::Sync { Self::settled_by_newest(survey, &outcome) } else { Vec::new() };
        summary.settled = settled.len() as u64;
        summary.unkept = outcome.unkept.len() as u64;
        summary.left_for_later = survey.left_for_later.removals();

        let mut problems: Vec<String> = outcome.stopped.iter().cloned().collect();
        let recorded = match &flushed {
            Ok(()) => match self.remember(job, survey, &outcome, crossing) {
                Ok(()) => true,
                Err(problem) => {
                    problems.push(problem);
                    false
                }
            },
            Err(Error::Undone(reason)) => {
                problems.push(format!("{reason} Charon recorded nothing of this sync: its next look compares both folders afresh and waits for your OK."));
                false
            }
            Err(e) => {
                problems.push(format!("Charon couldn't make sure the changes reached the drive ({e}). Its next look compares both folders afresh."));
                false
            }
        };
        if !problems.is_empty() {
            summary.error = Some(problems.join(" "));
        }
        for (path, reason) in outcome.failed.iter().take(LOGGED_ITEMS) {
            journal.warn(id, &format!("{path}: {reason}"));
        }
        for (path, reason) in outcome.skipped.iter().chain(&outcome.waited).chain(&outcome.left_out).take(LOGGED_ITEMS) {
            journal.info(id, &format!("{path}: {reason}"));
        }
        let unread: Vec<Place> = outcome
            .unread
            .iter()
            .map(|item| Place::new(NoteKind::Unreadable, item.side, survey.snapshot(item.side).spelling(&item.key), item.reason.clone()))
            .collect();
        for place in unread.iter().take(LOGGED_ITEMS) {
            journal.info(id, &format!("{}: couldn't be read, so it's left alone until it can be ({})", place.path, place.reason));
        }
        for (path, to) in settled.iter().take(LOGGED_ITEMS) {
            journal.info(
                id,
                &format!(
                    "{path}: changed on both sides; Newest wins kept the {} version, and the {}'s is in .charon-trash",
                    to.opposite().word(),
                    to.word()
                ),
            );
        }
        for path in outcome.unrecorded.iter().take(LOGGED_ITEMS) {
            journal.info(
                id,
                &format!(
                    "{path}: copied, but by the end of the sync its original was gone or an older version (moved or removed meanwhile, or a change its drive lost), so Charon doesn't count it as synced and compares it afresh next time"
                ),
            );
        }
        for (path, what) in outcome.unkept.iter().take(LOGGED_ITEMS) {
            journal.info(id, &format!("{path}: copied without its {what}, which the receiving drive can't keep"));
        }
        if tidied > 0 {
            journal.info(id, &format!("moved {} of interrupted syncs into .charon-trash", plural(tidied, "leftover file")));
        }
        self.change_runtime(id, |rt| {
            for side in &outcome.no_links {
                match side {
                    Side::Source => rt.no_links.0 = true,
                    Side::Destination => rt.no_links.1 = true,
                }
            }
            // An item this run carried after all was taken, and read. One refused waits for
            // a change, one unread until it can be read.
            if !rt.refused.is_empty() || !rt.unread.is_empty() {
                let carried: HashSet<&str> = outcome.settled.iter().map(|step| step.path.as_str()).collect();
                rt.refused.retain(|(_, key), _| !carried.contains(key.as_str()));
                rt.unread.retain(|(_, key), _| !carried.contains(key.as_str()));
            }
            for refused in &outcome.refused {
                rt.refused.insert((refused.side, refused.key.clone()), (refused.clone(), survey.volume(refused.side).cloned()));
            }
            for unread in &outcome.unread {
                rt.unread.insert((unread.side, unread.key.clone()), unread.clone());
            }
        });
        // The drives, with what this run found out about links on them.
        let drives = match (survey.volume(Side::Source), survey.volume(Side::Destination)) {
            (Some(source), Some(destination)) if recorded => {
                let mut drives = Drives { source: source.clone(), destination: destination.clone() };
                for side in &outcome.no_links {
                    match side {
                        Side::Source => drives.source.refuses_links = true,
                        Side::Destination => drives.destination.refuses_links = true,
                    }
                }
                Some(drives)
            }
            _ => None,
        };
        let left = !outcome.failed.is_empty() || !outcome.skipped.is_empty() || !outcome.waited.is_empty() || !outcome.left_out.is_empty() || !unread.is_empty();
        let unwatched_since = claim.unwatched_since();
        drop(claim);
        self.finish(job, survey, crossing, summary, Some(Recorded { recorded, drives, destroyed: outcome.destroyed, left, unread }), unwatched_since);
    }

    /// The conflicts this run settled by Newest wins, with the side the newer version was
    /// copied to (the other version went to .charon-trash).
    fn settled_by_newest<'a>(survey: &'a Survey, outcome: &Outcome) -> Vec<(&'a str, Side)> {
        let done: BTreeSet<&str> = outcome.settled.iter().map(|step| step.path.as_str()).collect();
        survey
            .plan
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::Copy { to, path, settles_conflict: true, .. } if done.contains(path.as_str()) => Some((path.as_str(), *to)),
                _ => None,
            })
            .collect()
    }

    /// Saves what this run did: the markers, then the memory (two-way), or the markers of a
    /// Mirror or Backup. Each happens only once the run's changes are on the drives, and only
    /// while both folders still carry the markers the survey found: otherwise another drive
    /// came in under the same name, and recording this sync would make the next one treat
    /// that drive's differences as changes to carry. The error says why nothing was recorded.
    fn remember(&self, job: &Job, survey: &Survey, outcome: &Outcome, crossing: &Crossing) -> std::result::Result<(), String> {
        let id = &job.id;
        let setup = &survey.setup;
        let now = Markers::read(&setup.source, &setup.destination, id);
        if let Some(side) = now.changed_since(&survey.markers) {
            self.forget_memory(id);
            return Err(format!(
                "Charon's marker in the {} folder changed during the sync, so it may be another drive under the same name. \
                 Charon recorded nothing of this sync: its next look compares both folders afresh and waits for your OK.",
                side.word()
            ));
        }
        if !setup.mode.is_two_way() {
            return self.remember_one_way(job, survey, outcome);
        }
        let memory = survey.memory.clone().unwrap_or_default();
        let next = match crossing.kind {
            ReviewKind::Sync => Baseline::after_run(&survey.baseline, &survey.source, &survey.destination, &survey.plan, &outcome.settled, &outcome.put_back),
            // Settling one item changes the memory of that item only, when there is a memory.
            // Whatever under the settled item didn't make it (a failed copy, a stop) has no
            // record afterwards, so the next look copies it rather than removing it.
            ReviewKind::Resolve if survey.trusted => {
                let mut baseline = memory.baseline.clone();
                if let Some(key) = &crossing.resolving {
                    baseline.forget_within(key);
                }
                baseline.record(&outcome.settled);
                baseline
            }
            // Without a memory it can trust (a sync didn't finish, a marker changed), settling
            // one item can't be recorded. And the memory from before must never be trusted
            // again: a file restored since the interrupted sync would look removed on its
            // side, and the next sync would remove its only copy. So it stays untrusted:
            // until a whole list is confirmed, each look compares both folders afresh and
            // removes nothing, and still says why.
            ReviewKind::Resolve => return self.keep_untrusted(job, survey, &memory, crossing),
        };
        let (tokens, generation) = match (&memory.tokens, survey.trusted) {
            (Some(tokens), true) => (tokens.clone(), memory.generation + 1),
            _ => ((Marker::fresh_token(), Marker::fresh_token()), 1),
        };
        let fail = |what: String| {
            // Without both markers and the memory, the next look starts afresh (it removes nothing).
            self.forget_memory(id);
            format!("The changes were synced, but Charon {what}, so it won't remember this sync: its next look compares both folders afresh and waits for your OK.")
        };
        for (side, token) in [(Side::Source, &tokens.0), (Side::Destination, &tokens.1)] {
            if let Err(e) = Marker::new(id, side, token, generation).write(setup.root(side)) {
                return Err(fail(format!("couldn't leave its marker in the {} ({e})", side.word())));
            }
        }
        // The folders are the ones the sync's look read (their markers didn't change during
        // the sync), and now carry the new markers: what this session read in them still holds.
        let marker = |side: Side, token: &str| Found::Marker(Marker::new(id, side, token, generation));
        let written = Markers { source: marker(Side::Source, &tokens.0), destination: marker(Side::Destination, &tokens.1) };
        self.change_known_runtime(id, |rt| rt.reads.carry(&survey.markers, written));
        // Pairs found to differ though they look alike, still without a record: the next
        // look reads them again (a list that settles one item keeps the others).
        let found = match crossing.kind {
            ReviewKind::Sync => survey.different.clone(),
            ReviewKind::Resolve => memory.unsettled.union(&survey.different).cloned().collect(),
        };
        let unsettled = found.into_iter().filter(|key| !next.contains(key)).collect();
        let memory = Memory { baseline: next, tokens: Some(tokens), generation, folded: survey.folded, distrust: None, earlier: Default::default(), unsettled };
        self.inner.baselines.save(id, setup, &memory).map_err(|e| fail(format!("couldn't save what was synced ({e})")))
    }

    /// Forgets the two-way memory of the job's last sync, and the files found identical with
    /// it (see `store::SameContents`): the next look compares both folders afresh.
    fn forget_memory(&self, id: &JobId) {
        let _ = self.inner.baselines.forget(id);
        let _ = self.inner.same_contents.forget(id);
    }

    /// After an item was settled while Charon couldn't trust its memory (see `remember`):
    /// the memory is kept with the reason the look gave (see `Survey::memory`), so no later
    /// look trusts it, and without the settled item. It still tells those looks what they
    /// bring back and which look-alike files to read. A memory without markers (Charon
    /// 0.2's, a damaged one, one for other folders) is left as it is: no look trusts it.
    fn keep_untrusted(&self, job: &Job, survey: &Survey, memory: &Memory, crossing: &Crossing) -> std::result::Result<(), String> {
        if memory.tokens.is_none() {
            return Ok(());
        }
        let outside = |key: &String| crossing.resolving.as_ref().is_none_or(|settled| !Keys::is_within(key, settled));
        let mut kept = memory.clone();
        if let Some(settled) = &crossing.resolving {
            kept.baseline.forget_within(settled);
        }
        kept.unsettled = memory.unsettled.union(&survey.different).filter(|key| outside(key)).cloned().collect();
        self.inner.baselines.save(&job.id, &survey.setup, &kept).map_err(|e| {
            format!("The item was settled, but Charon couldn't note that it doesn't trust what it remembers of the last sync ({e}), so it waits for your OK before the next sync.")
        })
    }

    /// Mirror and Backup: the destination's marker, then the source's where Charon can
    /// write there. Each is written to the folder before the job's record names it, so a
    /// record that couldn't be saved leaves the next look asking, never trusting.
    fn remember_one_way(&self, job: &Job, survey: &Survey, outcome: &Outcome) -> std::result::Result<(), String> {
        let id = &job.id;
        let setup = &survey.setup;
        let token = job.destination_token.clone().unwrap_or_else(Marker::fresh_token);
        if let Err(e) = Marker::new(id, Side::Destination, &token, 0).write(&setup.destination) {
            // The job keeps the token it had: the next look then finds the folder without
            // it and asks.
            return Err(format!(
                "The files were synced, but Charon couldn't leave its marker in the destination ({e}), so it can't recognise that folder next time and will ask first."
            ));
        }
        self.remember_landings(job, survey, outcome, &token);
        let mark = match &job.source_mark {
            Some(mark) => Mark { token: mark.token.clone(), generation: mark.generation + 1 },
            None => Mark { token: Marker::fresh_token(), generation: 1 },
        };
        // A source Charon can't write to (a card, a read-only share) simply has no marker;
        // its record then stays as it was, and so does what the next look expects.
        let source_marked = match Marker::new(id, Side::Source, &mark.token, mark.generation).write(&setup.source) {
            Ok(()) => true,
            Err(e) => {
                log::info!("no marker in the source of {} ({e})", setup.name);
                false
            }
        };
        self.change_job(id, |job| {
            job.destination_token = Some(token);
            if source_marked {
                job.source_mark = Some(mark);
            }
        })
        .map_err(|e| format!("The files were synced, but Charon couldn't save its record of this sync ({e}), so it will ask before the next one."))
    }

    /// Mirror and Backup: records the copies whose dates didn't stick on the destination,
    /// so the next look doesn't copy them again while neither side changes (see
    /// `store::Landings`). Losing this record only means copying those files once more.
    fn remember_landings(&self, job: &Job, survey: &Survey, outcome: &Outcome, destination_token: &str) {
        let (landed, undated) = Landings::after_run(&survey.landed, &outcome.settled, &survey.source);
        if undated > 0 {
            self.inner.journal.info(
                &job.id,
                &format!(
                    "{} got other dates on the destination's drive than in the source (the drive doesn't keep the dates Charon sets); Charon remembers them, so they aren't copied again until one side changes",
                    plural(undated, "copied file")
                ),
            );
        }
        if let Err(e) = self.inner.landings.save(&job.id, &survey.setup, survey.folded, destination_token, &landed) {
            log::warn!("couldn't save the copies whose dates didn't stick for job {}: {e}", job.id);
            let _ = self.inner.landings.forget(&job.id);
        }
    }

    /// Records the end of a run: the job's record, the log, the live state, and the window.
    /// `unwatched_since`: when the run began to keep a drive of the folders quiet, if it did
    /// (see `quiet.rs`).
    fn finish(
        &self,
        job: &Job,
        survey: &Survey,
        crossing: &Crossing,
        mut summary: RunSummary,
        recorded: Option<Recorded>,
        unwatched_since: Option<Instant>,
    ) {
        let id = &job.id;
        // A Stop from the person means something looked wrong: the next sync waits for an
        // OK. A run stopped only because Charon quit carries on as before next time.
        let stopped_by_person =
            self.runtime().get(id).is_some_and(|rt| rt.stopped_by_person) || (summary.cancelled && !self.is_shutting_down());
        let syncing = crossing.kind == ReviewKind::Sync;
        let confirmed_clean = syncing && crossing.approved && summary.error.is_none() && !summary.cancelled && !stopped_by_person;
        // Files were carried, but what they did couldn't be recorded (or another drive came
        // in): the next sync waits for an OK.
        let unrecorded = recorded.as_ref().is_some_and(|recorded| !recorded.recorded);
        summary.finished_at = Utc::now();
        let finished_at = summary.finished_at;
        // Full automation: what a run nobody looked at removed or replaced joins the long
        // count (see `Unattended`). If that can't be saved, the guard can't see damage that
        // spreads slowly, so the next sync waits for the person's OK.
        let uncounted = match recorded.as_ref().filter(|_| !crossing.approved) {
            Some(recorded) => {
                let (source, destination) = &recorded.destroyed;
                self.inner.unattended.note(id, finished_at, source, destination, self.settings().trash_days).err()
            }
            None => None,
        };
        let saved = self.change_job_anyway(id, |stored| {
            stored.last_run = Some(summary.clone());
            if summary.error.is_none() && !summary.cancelled {
                stored.last_synced = Some(finished_at);
            }
            if let Some(recorded) = &recorded {
                // A run that couldn't be made durable leaves the mark: the next look won't
                // trust the two-way memory.
                stored.run_in_progress = !recorded.recorded;
                if syncing {
                    // What still waits after this run is for the next look to say.
                    stored.waiting_since = None;
                }
                // A settled conflict says nothing about the drives as a whole.
                if let Some(drives) = recorded.drives.as_ref().filter(|_| syncing) {
                    stored.drives = Some(drives.clone());
                }
                if !crossing.approved {
                    stored.note_automatic(finished_at, &recorded.destroyed.0, &recorded.destroyed.1);
                }
            }
            // Settling one conflict doesn't confirm the rest of the job's changes.
            if stopped_by_person {
                stored.needs_review = true;
                stored.held = Some(STOPPED.into());
            } else if unrecorded {
                stored.needs_review = true;
                stored.held = Some(UNRECORDED.into());
            } else if uncounted.is_some() {
                stored.needs_review = true;
                stored.held = Some(UNCOUNTED.into());
            } else if confirmed_clean {
                stored.needs_review = false;
                stored.held = None;
                // The person just confirmed a list that counted what automatic syncs did
                // today (see `Guard::concerns`): the count starts again from here.
                if stored.setup.full_automation {
                    stored.automatic_changes.clear();
                }
            }
        });
        if let Err(e) = saved {
            log::error!("couldn't save the result of a sync of {}: {e}", job.setup.name);
            self.inner.journal.error(id, &format!("Charon couldn't save the result of this sync ({e}); it holds while Charon runs"));
        }
        if let Some(e) = uncounted {
            log::error!("couldn't count what an automatic sync of {} changed: {e}", job.setup.name);
            self.inner.journal.error(id, &format!("Charon couldn't count what this automatic sync removed or replaced ({e}); the next sync waits for your OK"));
        }
        if confirmed_clean {
            // The person saw and confirmed a list: the long count starts again from here.
            if let Err(e) = self.inner.unattended.forget(id) {
                log::warn!("couldn't start the count of automatic changes of {} again: {e}", job.setup.name);
            }
        }
        let line = summary.describe();
        match (&summary.error, summary.failed) {
            (Some(_), _) => self.inner.journal.error(id, &line),
            (None, 0) => self.inner.journal.info(id, &line),
            (None, _) => self.inner.journal.warn(id, &line),
        }
        if stopped_by_person {
            self.inner.journal.info(id, "stopped: the next sync waits for your OK");
        } else if summary.cancelled {
            self.inner.journal.info(id, "stopped because Charon quit");
        }

        // Live state last: whoever sees the run end finds its results.
        let carried = recorded.is_some();
        let left = recorded.as_ref().is_some_and(|r| r.left);
        let unread = recorded.map(|r| r.unread).unwrap_or_default();
        let mut look = Look::of(survey);
        let resolved: Vec<String> = survey.plan.actions.iter().map(|a| a.path().to_string()).collect();
        self.change_runtime(id, |rt| {
            // The folders changed in ways the run's plan didn't know of: something besides
            // its own items changed while it ran, or since its list was made (a list that
            // waited for another sync), maybe as a newer look found. Its own look then says
            // nothing about what's left: the folders are looked at again at once (see below).
            let outdated = rt.own_writes.as_ref().is_some_and(OwnWrites::others_changed)
                || crossing.outdated
                || rt.look.as_ref().is_some_and(|look| look.at > survey.made_at);
            rt.cancel = None;
            rt.stopped_by_person = false;
            if stopped_by_person || unrecorded {
                rt.must_ask = true;
            } else if confirmed_clean {
                rt.must_ask = false;
            }
            rt.progress = None;
            rt.finished_at = Some(Instant::now());
            rt.problem = summary.error.clone();
            // An automatic run that never got to carry anything: the changes its look found
            // wait for a newer look before a run tries them again (see `has_pending`).
            if !carried && !crossing.approved {
                rt.tried = rt.look.as_ref().map(|look| look.at);
            } else if carried {
                rt.tried = None;
            }
            match crossing.kind {
                // A run that never got to carry anything leaves the last look as it was.
                _ if !carried => {}
                ReviewKind::Sync => {
                    // Only the removals this run left for a later one still wait.
                    let later = &survey.left_for_later;
                    look.items = later.summary().changes();
                    look.bytes = 0;
                    look.removals = Default::default();
                    for side in [Side::Source, Side::Destination] {
                        look.removals.add(side, later.removed_on(side));
                    }
                    look.replacements = Default::default();
                    rt.reminder.count = 0;
                    // Until the new look, "In sync" would be a guess.
                    rt.look = (!outdated).then_some(look);
                }
                ReviewKind::Resolve => {
                    if let Some(look) = rt.look.as_mut() {
                        look.conflicts.retain(|c| !resolved.contains(&c.key));
                    }
                }
            }
            // Files this run couldn't read show at once, as the next look will list them.
            if let Some(look) = rt.look.as_mut() {
                for place in unread {
                    if !look.unreadable.contains(&place) {
                        look.unreadable.push(place);
                        look.unreadable_count += 1;
                    }
                }
                look.unreadable.truncate(PLACES_SHOWN);
            }
            // After a stopped run, a confirmed run that left something, or a run whose plan the
            // folders outgrew, look again at once, so the row shows what's still waiting. Runs
            // by themselves wait for the regular look otherwise: a file that fails every time
            // mustn't start a run every few seconds. After a run that left nothing, that look
            // counts from when the folders on a drive the run kept quiet stopped being watched:
            // what changed there since, besides the run's own items, wasn't heard. After one
            // that left something, it counts from now, however long the run kept the drive
            // quiet: what it left is tried again a whole interval later at the soonest.
            let left_something = left || summary.error.is_some();
            if summary.cancelled || outdated || (crossing.approved && left_something) {
                rt.last_checked = None;
            } else {
                let since = unwatched_since.filter(|_| !left_something);
                rt.last_checked = Some(since.unwrap_or_else(Instant::now));
            }
        });
        self.emit(EngineEvent::Finished(Finished { id: id.clone(), name: job.setup.name.clone(), summary }));
        self.publish(id);
        self.start_waiting_confirmed();
    }

    /// Starts the confirmed syncs that waited for another sync, where nothing stands in their
    /// way anymore (a sync just ended).
    fn start_waiting_confirmed(&self) {
        let waiting: Vec<JobId> =
            self.runtime().iter().filter(|(_, rt)| rt.confirmed.is_some() && !rt.is_running()).map(|(id, _)| id.clone()).collect();
        for id in waiting {
            self.start_confirmed(&id);
        }
    }

    /// Whether a change at `path` in the job's folders, heard while the job syncs or a
    /// moment after, counts as a change now (see `note_change`). Most such changes are the
    /// sync's own writes, which don't. A change anywhere else is the person's, or another
    /// app's, and counts: the folders aren't quiet, so a job that syncs on change waits for
    /// them to settle. During the sync it's also kept for its end, which then has the folders
    /// looked at again at once.
    pub fn note_change_while_syncing(&self, id: &JobId, path: &Path) -> bool {
        let Ok(job) = self.stored_job(id) else { return false };
        let key = match self.change_at(&job, path) {
            // The sync writes into the folder itself too.
            ChangeAt::Folder | ChangeAt::Ignored => return false,
            ChangeAt::Item(key) => key,
        };
        let counts = self.change_runtime(id, |rt| {
            let running = rt.is_running();
            match rt.own_writes.as_mut() {
                Some(own) if own.is_own(&key) => false,
                Some(own) if running => {
                    own.note_other(&key);
                    true
                }
                _ => true,
            }
        });
        counts && self.note_change(id, path)
    }

    /// A run that ended without carrying anything (held back, or it found another sync at
    /// work): if something changed in the folders while it looked, they're looked at again
    /// at once.
    fn look_again_if_changed(rt: &mut JobRuntime) {
        if rt.own_writes.take().is_some_and(|own| own.others_changed()) {
            rt.last_checked = None;
        }
    }

    /// Forgets which items the job's last sync wrote, once that's long past.
    pub(crate) fn forget_own_writes(rt: &mut JobRuntime) {
        if !rt.is_running() && rt.finished_at.is_none_or(|at| at.elapsed() >= OWN_WRITES_KEPT) {
            rt.own_writes = None;
        }
    }

    /// A run that ended before carrying anything (a folder went away, it was stopped).
    pub(crate) fn unfinished(&self, job: &Job, crossing: &Crossing, error: Error) {
        let mut summary = RunSummary::starting(Utc::now(), crossing.approved);
        match error {
            Error::Cancelled => summary.cancelled = true,
            e => summary.error = Some(e.to_string()),
        }
        let empty = Survey::empty(job);
        self.finish(job, &empty, crossing, summary, None, None);
    }

    /// Holds back a run nobody looked at: nothing is touched, and the job waits for the
    /// person's OK, saying why.
    fn hold(&self, job: &Job, survey: &Survey) {
        let id = &job.id;
        let mut reason = survey.refusal.clone().unwrap_or_else(|| survey.holds[0].clone());
        let more = survey.holds.len().saturating_sub(1);
        if more > 0 {
            reason.push_str(&format!(" ({} more {} in the list.)", more, if more == 1 { "reason" } else { "reasons" }));
        }
        self.inner.journal.warn(id, &format!("held back for your OK: {reason}"));
        if let Err(e) = self.change_job_anyway(id, |job| {
            job.needs_review = true;
            job.held = Some(reason.clone());
        }) {
            log::error!("couldn't save that {} was held back: {e}", job.setup.name);
            self.inner.journal.error(id, &format!("Charon couldn't save that this job waits for your OK ({e}); it keeps waiting while Charon runs"));
        }
        self.note_look(job, survey, None);
        self.change_runtime(id, |rt| {
            rt.cancel = None;
            rt.progress = None;
            rt.finished_at = Some(Instant::now());
            rt.must_ask = true;
            Self::look_again_if_changed(rt);
        });
        self.emit(EngineEvent::Held { id: id.clone(), name: job.setup.name.clone(), reason });
        self.publish(id);
        self.start_waiting_confirmed();
    }

    /// Keeps the latest progress and sends it to the window now and then.
    fn report_progress(&self, id: &JobId, progress: &Progress) {
        let send = self.change_runtime(id, |rt| {
            rt.progress = Some(progress.clone());
            let due = rt.last_progress_sent.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
            let finished = progress.phase == Phase::Finishing;
            if due || finished {
                rt.last_progress_sent = Some(Instant::now());
            }
            due || finished
        });
        if send {
            self.emit(EngineEvent::Progress { id: id.clone(), progress: progress.clone() });
        }
    }
}

/// What a finished run left for the job's record.
struct Recorded {
    /// The run's changes are on the drives.
    recorded: bool,
    drives: Option<Drives>,
    /// Files removed or replaced on the source and on the destination.
    destroyed: (Vec<String>, Vec<String>),
    /// Steps failed, were skipped, waited with their library or database, or were left out,
    /// or files couldn't be read.
    left: bool,
    /// The files this run couldn't read, as places for the row.
    unread: Vec<Place>,
}

impl Survey {
    /// A survey of nothing, for a run that ended before it could look.
    fn empty(job: &Job) -> Survey {
        Survey {
            setup: job.setup.clone(),
            source: Default::default(),
            destination: Default::default(),
            folded: false,
            filter: ExcludeFilter::none(),
            markers: Markers { source: Found::Missing, destination: Found::Missing },
            memory: None,
            trusted: false,
            baseline: Baseline::new(),
            landed: Baseline::new(),
            plan: Default::default(),
            left_for_later: Default::default(),
            agreed: 0,
            different: BTreeSet::new(),
            uncompared: BTreeSet::new(),
            refusal: None,
            nesting: None,
            waits_on: None,
            warnings: Vec::new(),
            holds: Vec::new(),
            needs_tick: false,
            unfamiliar: false,
            housekeeping: Default::default(),
            made_at: Utc::now(),
        }
    }
}

#[cfg(test)]
impl Engine {
    /// Makes the job's last sync look as if it ended at `at`, by both clocks.
    pub(crate) fn pretend_the_last_sync_ended(&self, id: &JobId, at: Moment) {
        self.change_job(id, |job| job.last_synced = Some(at.wall)).unwrap();
        self.change_runtime(id, |rt| rt.finished_at = Some(at.own));
    }

    /// Begins a run nobody looks at, as a trigger starts one, and returns what looks at the
    /// folders and carries the run out, on the caller's thread: until it's called, the run
    /// is still looking and doesn't know its plan.
    pub(crate) fn begin_automatic_run(&self, id: &JobId, reason: &str) -> impl FnOnce() + '_ {
        let crossing = Crossing::unattended(reason, false);
        let (job, cancel) = self.begin_run(id, &crossing, |_, _| Ok(())).unwrap();
        move || {
            let mut survey = self.survey(&job, &cancel, Purpose::Automatic).unwrap();
            assert!(survey.holds.is_empty(), "{:?}", survey.holds);
            self.hold_back_recent_removals(&job, &mut survey, Moment::now());
            let claim = self.claim_folders(&job, &survey).unwrap();
            self.cross(&job, &survey, &cancel, &crossing, claim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConflictRule, JobStatus, SyncMode, Trigger};
    use crate::SyncStart;
    use crate::scan::{Entry, Snapshot, Volume};
    use std::path::Path;

    fn minutes(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    /// A Mirror that runs by itself whenever its drive is connected, synced once.
    fn synced_mirror(data: &Path, src: &Path, dst: &Path) -> (Engine, JobId) {
        for folder in [src, dst] {
            std::fs::create_dir_all(folder).unwrap();
        }
        for name in ["a.txt", "b.txt", "c.txt", "gone.txt"] {
            std::fs::write(src.join(name), name).unwrap();
        }
        let engine = Engine::open(data, Arc::new(|_| {})).unwrap();
        let setup = JobSetup {
            name: "Photos".into(),
            source: src.to_path_buf(),
            destination: dst.to_path_buf(),
            mode: SyncMode::Mirror,
            conflict: ConflictRule::Ask,
            trigger: Trigger::DriveConnected,
            excludes: JobSetup::default_excludes(),
            safe_delete: true,
            full_automation: true,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let review = engine.prepare_review(&id).unwrap();
        engine.approve(&id, &review.review_id, true).unwrap();
        while engine.is_running(&id) {
            thread::sleep(Duration::from_millis(10));
        }
        (engine, id)
    }

    /// Puts `snapshot` of `folder` on a network share: the folder's drive, as if it were one.
    fn on_a_share(snapshot: &mut Snapshot, folder: &Path) {
        snapshot.facts_mut().volume = Some(Volume { network: true, ..Volume::of(folder) });
    }

    /// A run nobody looks at, as the autopilot starts one at `now`, with the source folder on
    /// a network share. The folders are on this computer's own drive, so the run's look notes
    /// nothing by itself: what a look at the share notes is added, as of `noted`.
    fn run_from_a_share(engine: &Engine, id: &JobId, noted: Moment, now: Moment) {
        let crossing = Crossing::unattended("drive connected", false);
        let (job, cancel) = engine
            .begin_run(id, &crossing, |job, _| {
                assert!(job.runs_by_itself());
                Ok(())
            })
            .unwrap();
        let mut survey = engine.survey(&job, &cancel, Purpose::Automatic).unwrap();
        assert!(survey.holds.is_empty(), "{:?}", survey.holds);
        on_a_share(&mut survey.source, &job.setup.source);
        engine.change_missing(&job, |missing| missing.note(&survey.plan, &survey.source, &survey.destination, noted));
        engine.hold_back_recent_removals(&job, &mut survey, now);
        let claim = engine.claim_folders(&job, &survey).unwrap();
        engine.cross(&job, &survey, &cancel, &crossing, claim);
    }

    /// The job's wanted run, as `start_wanted` reads it before it starts it.
    fn read_wanted(engine: &Engine, id: &JobId) -> Wanted {
        engine.runtime().get(id).and_then(|rt| rt.wanted.clone()).expect("the job wants a run")
    }

    fn wait_idle(engine: &Engine, id: &JobId) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while engine.is_running(id) {
            assert!(Instant::now() < deadline, "the sync took too long");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Asserts that nothing began: no run, nothing copied, and no run wanted anymore.
    fn nothing_began(engine: &Engine, id: &JobId, dst: &Path) {
        assert!(!engine.is_running(id), "a sync began");
        assert!(!dst.join("new.txt").exists(), "a sync copied the new file");
        assert!(engine.runtime().get(id).is_some_and(|rt| rt.wanted.is_none() && rt.waiting_for.is_none()));
        assert!(!engine.log(id, 20).iter().any(|line| line.text.contains("sync started with Sync all")), "{:?}", engine.log(id, 5));
    }

    #[test]
    fn a_run_the_person_asked_for_never_begins_once_pausing_the_job_called_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::write(src.join("new.txt"), "new").unwrap();

        // Sync all under Pause everything wants a run; it's read to be started, and while
        // the folders are looked at before it begins, the person pauses the job.
        engine.set_all_paused(true).unwrap();
        assert!(engine.want_for_sync_all(&id, engine.pauses()));
        let wanted = read_wanted(&engine, &id);
        engine.set_paused(&id, true).unwrap();
        engine.start_as_wanted(&id, &wanted);
        nothing_began(&engine, &id, &dst);
    }

    #[test]
    fn a_run_the_person_asked_for_never_begins_once_pause_everything_called_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::write(src.join("new.txt"), "new").unwrap();

        // Sync all wants a run; it's read to be started, and while the folders are looked at
        // before it begins, the person pauses everything.
        assert!(engine.want_for_sync_all(&id, engine.pauses()));
        let wanted = read_wanted(&engine, &id);
        engine.set_all_paused(true).unwrap();
        engine.start_as_wanted(&id, &wanted);
        nothing_began(&engine, &id, &dst);
    }

    #[test]
    fn a_triggers_run_held_back_by_a_pause_leaves_the_run_the_person_asked_for_since() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::write(src.join("new.txt"), "new").unwrap();

        // A trigger's run is due, and it's read to be started. While the folders are looked
        // at before it begins, the person pauses everything, then presses Sync all.
        engine.want(&id, "after changes");
        let read = read_wanted(&engine, &id);
        engine.set_all_paused(true).unwrap();
        assert!(engine.want_for_sync_all(&id, engine.pauses()));

        // The pause holds back the trigger's run, and only that one.
        engine.start_as_wanted(&id, &read);
        assert!(!engine.is_running(&id));
        assert!(read_wanted(&engine, &id).by_hand, "the run the person asked for is still wanted");
        engine.start_wanted(&id);
        wait_idle(&engine, &id);
        assert!(dst.join("new.txt").exists());
        assert!(engine.log(&id, 20).iter().any(|line| line.text.starts_with("sync started with Sync all")));
    }

    #[test]
    fn removals_left_for_later_stay_in_the_jobs_row_and_are_carried_out_after_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::remove_file(src.join("gone.txt")).unwrap();

        // The share has shown the file missing for 9 minutes: the removal waits.
        let start = Moment::now();
        run_from_a_share(&engine, &id, start, start.after(minutes(9)));
        look_at_the_share(&engine, &id, &src, start.after(minutes(9)));
        assert!(dst.join("gone.txt").exists());
        let row = engine.job(&id).unwrap();
        assert_eq!(row.status, JobStatus::Pending, "never “In sync” while a removal waits");
        assert_eq!(row.pending.map(|pending| pending.items), Some(1));
        assert!(!engine.has_pending(&id), "a removal that waits starts no run by itself");
        assert!(engine.log(&id, 10).iter().any(|line| line.text.starts_with("left 1 removal for a later sync")));
        assert_eq!(row.job.last_run.as_ref().map(|run| run.left_for_later), Some(1), "the run doesn't say “already in sync”");
        assert!(!engine.take_follow_up_at(&id, start.after(minutes(9))), "not due yet");

        // Charon restarts (a drive-connected job may not sync again for days): the wait
        // goes on from the 9 minutes the looks had counted.
        drop(engine);
        let engine = Engine::open(&data, Arc::new(|_| {})).unwrap();
        let launch = Moment::now();
        let kept = engine.runtime().get(&id).map(|rt| rt.missing.kept()).unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].left_for_later && kept[0].since == start.wall);
        assert_eq!(kept[0].counted_seconds, Some(9 * 60));
        assert!(!engine.take_follow_up_at(&id, launch), "no look since the launch has found it missing yet");
        // A look after the launch finds the file missing still.
        look_at_the_share(&engine, &id, &src, launch);
        assert!(!engine.take_follow_up_at(&id, launch.after(minutes(1))));
        assert!(engine.take_follow_up_at(&id, launch.after(minutes(2))), "the job syncs once more, whatever its trigger");
        assert!(!engine.take_follow_up_at(&id, launch.after(minutes(3))), "once");

        // That run carries out the removal.
        run_from_a_share(&engine, &id, launch, launch.after(minutes(11)));
        assert!(!dst.join("gone.txt").exists());
        let row = engine.job(&id).unwrap();
        assert_eq!(row.status, JobStatus::InSync);
        assert!(row.pending.is_none());
    }

    /// A look at the Mirror of `synced_mirror` at `now`, with its source on a network share
    /// where gone.txt is still missing.
    fn look_at_the_share(engine: &Engine, id: &JobId, src: &Path, now: Moment) {
        let job = engine.stored_job(id).unwrap();
        let mut share: Snapshot = ["a.txt", "b.txt", "c.txt"].into_iter().map(|name| (name.to_string(), Entry::file(5, 1))).collect();
        on_a_share(&mut share, src);
        let mine: Snapshot = [("gone.txt".to_string(), Entry::file(8, 1))].into_iter().collect();
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, "gone.txt", Entry::file(8, 1), None));
        engine.change_missing(&job, |missing| missing.note(&plan, &share, &mine, now));
    }

    #[test]
    fn removals_left_for_later_wait_again_when_the_share_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::remove_file(src.join("gone.txt")).unwrap();
        let start = Moment::now();
        run_from_a_share(&engine, &id, start, start);
        assert!(dst.join("gone.txt").exists(), "left for later");

        // Minutes later the share goes away, and hours later it comes back.
        engine.forget_look(&id);
        let back = start.after(minutes(5 * 60));
        assert!(!engine.take_follow_up_at(&id, back), "no look since it came back has found it missing");
        // It may list the folder before it has loaded: the looks since must find the file
        // missing for a while first.
        look_at_the_share(&engine, &id, &src, back);
        assert!(!engine.take_follow_up_at(&id, back.after(minutes(1))));
        assert!(engine.take_follow_up_at(&id, back.after(minutes(2))));
    }

    #[test]
    fn removals_left_for_later_wait_again_after_the_computer_slept() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        std::fs::remove_file(src.join("gone.txt")).unwrap();
        let start = Moment::now();
        run_from_a_share(&engine, &id, start, start);
        // The share was back before the autopilot's first tick after the computer woke up.
        engine.after_a_gap();
        let awake = start.after(minutes(5 * 60));
        assert!(!engine.take_follow_up_at(&id, awake));
        look_at_the_share(&engine, &id, &src, awake);
        assert!(!engine.take_follow_up_at(&id, awake.after(minutes(1))));
        assert!(engine.take_follow_up_at(&id, awake.after(minutes(2))));
    }

    #[test]
    fn a_database_whose_journals_are_left_for_later_is_left_for_later_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst, data) = (tmp.path().join("src"), tmp.path().join("dst"), tmp.path().join("data"));
        let (engine, id) = synced_mirror(&data, &src, &dst);
        // The app on the share closed its database: it was saved one last time, and its
        // journals went. The backup still has the database as it was, with its journals.
        std::fs::write(src.join("notes.db"), "the database, saved at closing").unwrap();
        let saved = filetime::FileTime::from_system_time(std::time::SystemTime::now() - Duration::from_secs(60));
        filetime::set_file_mtime(src.join("notes.db"), saved).unwrap();
        std::fs::write(dst.join("notes.db"), "the database while open").unwrap();
        std::fs::write(dst.join("notes.db-wal"), "changes that only fit the database while open").unwrap();
        std::fs::write(dst.join("notes.db-shm"), "their index").unwrap();

        // The journals went missing only now: the new database must not land next to them.
        let start = Moment::now();
        run_from_a_share(&engine, &id, start, start);
        assert_eq!(std::fs::read_to_string(dst.join("notes.db")).unwrap(), "the database while open");
        assert!(dst.join("notes.db-wal").exists() && dst.join("notes.db-shm").exists());
        let row = engine.job(&id).unwrap();
        assert_eq!(row.pending.map(|pending| pending.items), Some(3), "all three wait");
        assert!(!engine.has_pending(&id), "what waits starts no run by itself");
        assert!(engine.log(&id, 10).iter().any(|line| line.text.contains("1 more change to the same database waits with them")));

        // Once the journals have been missing long enough, the database goes over whole.
        run_from_a_share(&engine, &id, start, start.after(minutes(10)));
        assert_eq!(std::fs::read_to_string(dst.join("notes.db")).unwrap(), "the database, saved at closing");
        assert!(!dst.join("notes.db-wal").exists() && !dst.join("notes.db-shm").exists());
    }

    #[test]
    fn a_sync_that_ends_on_a_panic_waits_for_an_ok_and_distrusts_its_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: "Panics".into(),
            source: a,
            destination: b,
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: true,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        engine.change_job(&id, |job| job.needs_review = false).unwrap();
        assert!(engine.stored_job(&id).unwrap().runs_by_itself());

        // A run starts carrying files, then hits a mistake in Charon itself.
        let crossing = Crossing::unattended("a test", false);
        let (job, cancel) = engine.begin_run(&id, &crossing, |_, _| Ok(())).unwrap();
        engine
            .spawn_crossing(job, cancel, crossing, |engine, job, _, _| {
                engine.change_job(&job.id, |job| job.run_in_progress = true).unwrap();
                panic!("a mistake in the middle of a sync");
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.is_running(&id) {
            assert!(Instant::now() < deadline, "the job stayed marked as syncing");
            thread::sleep(Duration::from_millis(10));
        }

        let view = engine.job(&id).unwrap();
        assert_eq!(view.job.held.as_deref(), Some(CRASHED));
        assert!(view.job.needs_review && !view.runs_by_itself);
        assert!(view.job.run_in_progress, "the next look doesn't trust the two-way memory");
        assert_eq!(view.job.last_run.as_ref().and_then(|run| run.error.as_deref()), Some(CRASHED));
        assert_ne!(view.status, JobStatus::Syncing);
        assert_eq!(engine.sync_now(&id).unwrap(), SyncStart::Review);
        assert!(engine.sync_all().unwrap().contains(&id), "Sync all asks for it");
    }

    /// A sync of Charon's own moves both folders' markers on, and they stay the same folders:
    /// what this session read in them still holds, so a conflict left for the person isn't
    /// read again by every list after the sync.
    #[test]
    fn what_this_session_read_still_holds_after_a_sync_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for folder in [&a, &b] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(a.join("readme.txt"), "hello").unwrap();
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: "Notes".into(),
            source: a.clone(),
            destination: b.clone(),
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let confirm = || {
            let review = engine.prepare_review(&id).unwrap();
            engine.approve(&id, &review.review_id, true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while engine.is_running(&id) {
                assert!(Instant::now() < deadline, "the sync took too long");
                thread::sleep(Duration::from_millis(10));
            }
            review
        };
        // The first sync leaves both folders' markers: only folders that carry them keep what
        // was read in them.
        assert_eq!(confirm().tally.conflicts, 0);
        // Each side holds its own version of the file, of the same size: the list reads both
        // and leaves the conflict for the person.
        for (folder, text, seconds) in [(&a, "ours", 0), (&b, "mine", 60)] {
            std::fs::write(folder.join("notes.txt"), text).unwrap();
            filetime::set_file_mtime(folder.join("notes.txt"), filetime::FileTime::from_unix_time(1_700_000_000 + seconds, 0)).unwrap();
        }
        assert_eq!(confirm().tally.conflicts, 1);

        let markers = Markers::read(&a, &b, &id);
        assert!(matches!(&markers.destination, Found::Marker(marker) if marker.generation == 2), "the sync moved the markers on");
        let filter = ExcludeFilter::new(&[], false).unwrap();
        let entry = |root: &Path| *Scanner::new(&filter).scan(root, Volume::of(root)).unwrap().get("notes.txt").unwrap();
        let verdict = engine.change_runtime(&id, |rt| rt.reads.verdict("notes.txt", &entry(&a), &entry(&b), &markers));
        assert_eq!(verdict, Some(false));
    }

    #[test]
    fn a_job_whose_folders_dont_answer_counts_as_sharing_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let [home, usb, share, copy] = ["home", "usb", "share", "copy"].map(|name| tmp.path().join(name));
        for folder in [&home, &usb, &share, &copy] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_| {})).unwrap();
        let job = |name: &str, source: &Path, destination: &Path| JobSetup {
            name: name.into(),
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        };
        let local = engine.add_job(job("Home", &home, &usb)).unwrap().job.id;
        let shared = engine.add_job(job("Share", &share, &copy)).unwrap().job.id;
        assert!(!engine.neighbours(&local).others[&shared].1, "different folders");

        // The share stops answering: where its folders lead can't be told, so a sync of that
        // job at work would keep this one waiting. Nothing waits for the share itself.
        let answer = engine.silence(&share);
        let asked = Instant::now();
        assert!(engine.neighbours(&local).others[&shared].1);
        assert!(asked.elapsed() < crate::engine::reach::LIMIT, "waited {:?}", asked.elapsed());

        answer.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.is_silent(&share) {
            assert!(Instant::now() < deadline, "the share took too long to answer");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!engine.neighbours(&local).others[&shared].1);
    }
}
