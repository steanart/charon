//! Reviews: the list of changes a sync would make, waiting for the person's OK.
//!
//! A review is made from a fresh survey and kept (one per job) until it's confirmed,
//! discarded, replaced by a newer one, or the job changes. Confirming it carries out exactly
//! that list: nothing else, and each step only if its items are still as they were seen. A
//! list older than 15 minutes, or made before something changed in the folders, can't be
//! confirmed: the person looks again. A list confirmed while another sync is at work in the
//! job's folders waits for that sync, and is carried out as soon as it's done (unless the
//! job changed, synced, or a drive was swapped meanwhile: then the row says why it wasn't).

use crate::engine::quiet::{Asker, Claimed};
use crate::engine::runs::Crossing;
use crate::engine::runtime::{Confirmed, JobRuntime};
use crate::engine::survey::{Purpose, Survey};
use crate::engine::{Engine, Look, AUTOPILOT_THREAD};
use crate::error::{Error, Result};
use crate::model::{
    Cluster, ConflictChoice, ConflictItem, JobId, Level, ReviewKind, Review, ReviewNote, ReviewStep, Shore, StepFilter, StepKind,
    Tally, VolumeLabel, Warning,
};
use crate::plan::{Action, Clearing, NoteKind, Resolution, Side, Unit};
use crate::run::Markers;
use crate::scan::{Entry, EntryKind, Keys, RootId};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// How long a list can be confirmed after it was made.
const VALID_FOR_MINUTES: i64 = 15;
/// Steps and notes in the first page of a review.
const FIRST_STEPS: usize = 200;
const FIRST_NOTES: usize = 100;
/// The most steps or notes one page can hold.
const MAX_PAGE: usize = 500;
/// Removed folders listed on their own.
const CLUSTERS: usize = 50;

/// A review as the engine keeps it.
pub(crate) struct StoredReview {
    pub id: String,
    pub kind: ReviewKind,
    pub title: Option<String>,
    pub survey: Survey,
    pub prepared_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Something changed in the folders after the list was made.
    pub stale: AtomicBool,
    /// Some of its folders weren't watched for a while after it was made (another job's sync
    /// kept their drive quiet, see `quiet.rs`): what changed there meanwhile wasn't heard.
    pub unheard: AtomicBool,
    /// When the job's last run had finished when the list was made.
    pub runs_seen: Option<DateTime<Utc>>,
    pub steps: Vec<ReviewStep>,
    pub notes: Vec<ReviewNote>,
    pub warnings: Vec<Warning>,
    pub blocked: Option<String>,
    pub needs_tick: bool,
    /// Why a list without steps is still worth confirming (see `Review::confirm_anyway`).
    pub confirm_anyway: Option<String>,
    /// The conflict a resolution settles, by its key.
    pub resolving: Option<String>,
}

/// One page of a review's steps.
#[derive(Debug, Clone, Serialize)]
pub struct StepPage {
    pub steps: Vec<ReviewStep>,
    pub total: usize,
}

/// One page of a review's notes.
#[derive(Debug, Clone, Serialize)]
pub struct NotePage {
    pub notes: Vec<ReviewNote>,
    pub total: usize,
}

/// What confirming a list did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Approval {
    /// The sync started.
    Started,
    /// Another sync is at work in the job's folders: the confirmed sync starts by itself as
    /// soon as that sync is done, unless the person calls it off (Stop, or closing the list).
    Waiting {
        /// That sync, as the row's `waitingFor` names it: another job's name, "another
        /// Charon", or "Charon on <computer>".
        #[serde(rename = "waitingFor")]
        waiting_for: String,
    },
}

impl Engine {
    /// Looks at both folders now and prepares the list of what the next sync would do.
    /// Slow: it scans both folders (`cancel_review` stops it).
    pub fn prepare_review(&self, id: &JobId) -> Result<Review> {
        let (job, stop, epoch) = self.begin_look(id)?;
        let survey = self.survey(&job, &stop, Purpose::Review);
        self.end_look(id, &stop);
        let survey = survey?;
        if !self.note_look(&job, &survey, Some(epoch)) {
            return Err(Self::overtaken());
        }
        let review = self.keep_review(&job, survey, ReviewKind::Sync, None, None, epoch)?;
        self.publish(id);
        Ok(review)
    }

    /// Stops preparing a review of `id`.
    pub fn cancel_review(&self, id: &JobId) {
        if let Some(stop) = self.runtime().get(id).and_then(|rt| rt.preparing.clone()) {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// The list the person closed without confirming it (or, once confirmed, while its sync
    /// waited for another sync).
    pub fn discard_review(&self, id: &JobId, review_id: &str) {
        let waited = self.change_known_runtime(id, |rt| {
            let current = rt.review.as_ref().is_some_and(|review| review.id == review_id);
            let waited = current && rt.confirmed.is_some();
            if current {
                rt.forget_review();
            }
            waited
        });
        if waited == Some(true) {
            self.note_called_off(id, "");
            self.publish(id);
        }
    }

    /// One page of a review's steps, filtered by kind and by a part of the path.
    pub fn review_steps(&self, id: &JobId, review_id: &str, filter: StepFilter, query: &str, offset: usize, limit: usize) -> Result<StepPage> {
        self.review_steps_in(id, review_id, filter, query, None, offset, limit)
    }

    /// One page of a review's steps, like `review_steps`, and only those at or inside
    /// `folder` (a path as the list spells it) when one is given: what goes with a removed
    /// folder, and nothing from another folder whose path merely contains its name.
    #[allow(clippy::too_many_arguments)]
    pub fn review_steps_in(
        &self,
        id: &JobId,
        review_id: &str,
        filter: StepFilter,
        query: &str,
        folder: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<StepPage> {
        let review = self.stored_review(id, review_id)?;
        let query = query.trim().to_lowercase();
        let folder = folder.map(|folder| folder.trim_end_matches('/')).filter(|folder| !folder.is_empty());
        let inside = |path: &str| {
            folder.is_none_or(|folder| path.strip_prefix(folder).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')))
        };
        let matching: Vec<&ReviewStep> = review
            .steps
            .iter()
            .filter(|step| filter.admits(step) && inside(&step.path) && (query.is_empty() || step.path.to_lowercase().contains(&query)))
            .collect();
        let steps = matching.iter().skip(offset).take(limit.min(MAX_PAGE)).map(|step| (*step).clone()).collect();
        Ok(StepPage { steps, total: matching.len() })
    }

    /// One page of a review's notes.
    pub fn review_notes(&self, id: &JobId, review_id: &str, offset: usize, limit: usize) -> Result<NotePage> {
        let review = self.stored_review(id, review_id)?;
        let notes = review.notes.iter().skip(offset).take(limit.min(MAX_PAGE)).cloned().collect();
        Ok(NotePage { notes, total: review.notes.len() })
    }

    /// Carries out exactly the reviewed list, in the background. Refused when the list
    /// can't be trusted anymore, or can't be confirmed as it is. While another sync is at
    /// work in the job's folders, the confirmed list waits for it, and its sync starts as
    /// soon as that sync is done (see `start_confirmed`; the row says what it waits for).
    /// Says which of the two happened.
    pub fn approve(&self, id: &JobId, review_id: &str, acknowledged: bool) -> Result<Approval> {
        let confirmed = Confirmed { review_id: review_id.to_string(), acknowledged, pauses: self.pauses() };
        match self.carry_out(id, &confirmed, false) {
            Ok(()) => Ok(Approval::Started),
            Err(Error::FoldersBusy(who)) | Err(Error::DriveBusy(who)) => self.queue_confirmed(id, confirmed, &who),
            Err(e) => Err(e),
        }
    }

    /// Starts the sync of a list the person confirmed while another sync was at work in the
    /// job's folders, if nothing stands in its way anymore. Returns whether it started. A
    /// list that can't be carried out anymore (the job changed, synced, or a drive was
    /// swapped meanwhile) is dropped, and the row says why. On the autopilot's thread, the
    /// sync claims its folders on its own thread (see `carry_out`): true then means it was
    /// handed to that thread, which keeps the list waiting if it finds another sync at work
    /// there after all.
    pub fn start_confirmed(&self, id: &JobId) -> bool {
        if self.is_shutting_down() {
            return false;
        }
        let Some(confirmed) = self.runtime().get(id).and_then(|rt| rt.confirmed.clone()) else { return false };
        match self.carry_out(id, &confirmed, true) {
            Ok(()) => true,
            Err(Error::FoldersBusy(who)) | Err(Error::DriveBusy(who)) => {
                self.keep_waiting(id, &confirmed, who);
                false
            }
            // Its own sync runs, or a folder is away for a moment: it keeps waiting. (Stopped
            // by the person, or by pausing everything, as it was about to start, it was called
            // off and said so.)
            Err(Error::Busy) | Err(Error::Offline(_)) | Err(Error::Cancelled) => false,
            Err(e) => {
                self.change_known_runtime(id, |rt| {
                    if rt.confirmed.as_ref().is_some_and(|waiting| waiting.review_id == confirmed.review_id) {
                        rt.forget_review();
                        rt.problem = Some(format!("The list you confirmed couldn't be carried out after all. {e}"));
                    }
                });
                self.inner.journal.warn(id, &format!("the confirmed sync that waited for another sync didn't start: {e}"));
                self.publish(id);
                false
            }
        }
    }

    /// Keeps a confirmed list waiting while another sync (`who`) is at work in the job's
    /// folders, if it's still the job's list. Not if the person paused everything since they
    /// confirmed it: the pause came while the list's sync was about to start, and found
    /// nothing waiting to call off (see `pause_everything`). The list is called off then, and
    /// the window and the job's log say so.
    fn queue_confirmed(&self, id: &JobId, confirmed: Confirmed, who: &str) -> Result<Approval> {
        let outcome = self.change_known_runtime(id, |rt| {
            let current = rt.review.as_ref().is_some_and(|review| review.id == confirmed.review_id);
            let paused = self.paused_since(&confirmed);
            if current && !paused {
                rt.confirmed = Some(confirmed);
                rt.waiting_for = Some(who.to_string());
            }
            (current, paused)
        });
        match outcome {
            Some((true, false)) => {
                self.inner.journal.info(id, &format!("confirmed by you; waits until “{who}” is done"));
                self.publish(id);
                Ok(Approval::Waiting { waiting_for: who.to_string() })
            }
            Some((true, true)) => {
                self.inner.journal.info(id, &format!("confirmed by you, then called off: you paused everything while it waited for “{who}”"));
                // The window lets a list go once Rust refused it, and only offers to look again.
                Err(Error::Invalid(format!(
                    "You paused everything just as this list was about to start, and “{who}” isn't done yet, so the list doesn't wait for it. \
                     Look again once it's done."
                )))
            }
            // The list was replaced or closed in between.
            _ => Err(Self::gone()),
        }
    }

    /// Keeps a confirmed list that found another sync (`who`) at work in the job's folders
    /// waiting, and the row saying for whom, while it's still the job's confirmed list (it may
    /// have been called off meanwhile). Not if the person paused everything since they
    /// confirmed it: the pause came while the list's sync was about to start, and found it
    /// marked as running (see `pause_everything`). The list is called off then, and the job's
    /// log says so, unless another try of it began meanwhile: that one sees the pause itself.
    fn keep_waiting(&self, id: &JobId, confirmed: &Confirmed, who: String) {
        let (called_off, changed) = self
            .change_known_runtime(id, |rt| {
                if !rt.confirmed.as_ref().is_some_and(|waiting| waiting.review_id == confirmed.review_id) {
                    return (false, false);
                }
                if self.paused_since(confirmed) {
                    return (!rt.is_running() && rt.call_off_confirmed(), false);
                }
                (false, rt.waiting_for.replace(who.clone()) != Some(who))
            })
            .unwrap_or_default();
        if called_off {
            self.note_called_off(id, " (Pause everything)");
        }
        if called_off || changed {
            self.publish(id);
        }
    }

    /// Whether the person paused everything since they confirmed `confirmed`.
    fn paused_since(&self, confirmed: &Confirmed) -> bool {
        self.pauses() != confirmed.pauses
    }

    /// Ends the sync of a list that waited before it changed anything, because the person
    /// paused everything since they confirmed it (see `carry_out`): the list is called off, and
    /// the job's log says so. One the person stopped meanwhile ends as stopped.
    fn call_off_as_it_starts(&self, job: &crate::model::Job, crossing: &Crossing, confirmed: &Confirmed) {
        let id = &job.id;
        let called_off = self.change_runtime(id, |rt| {
            if !Self::unmark_run(rt) {
                return None;
            }
            Some(rt.confirmed.as_ref().is_some_and(|waiting| waiting.review_id == confirmed.review_id) && rt.call_off_confirmed())
        });
        match called_off {
            None => self.unfinished(job, crossing, Error::Cancelled),
            Some(called_off) => {
                if called_off {
                    self.note_called_off(id, " (Pause everything)");
                }
                self.publish(id);
            }
        }
    }

    /// The confirmed list is being carried out: it no longer waits for an OK.
    pub(crate) fn take_review(&self, id: &JobId, review_id: &str) {
        self.change_known_runtime(id, |rt| {
            if rt.review.as_ref().is_some_and(|review| review.id == review_id) {
                rt.forget_review();
            }
        });
    }

    /// Carries out exactly the list the person confirmed, after the checks that make sure it
    /// can still be trusted. A list that `waited` for another sync was confirmed in time:
    /// neither its age nor the changes made in the folders since count against it (each step
    /// still checks its items right before it acts, and leaves alone what changed; an app
    /// library or a database is only carried when nothing of it changed, see `run::Executor`).
    /// It's called off, though, if the person paused everything since they confirmed it.
    fn carry_out(&self, id: &JobId, confirmed: &Confirmed, waited: bool) -> Result<()> {
        let (review_id, acknowledged) = (confirmed.review_id.as_str(), confirmed.acknowledged);
        let review = self.stored_review(id, review_id)?;
        let list_checks = |job: &crate::model::Job, rt: &mut JobRuntime| {
            // Stopped, paused or edited while it waited: the person called it off, even if
            // that came after `start_confirmed` picked it up.
            if waited && !rt.confirmed.as_ref().is_some_and(|confirmed| confirmed.review_id == review_id) {
                return Err(Error::Cancelled);
            }
            let current = rt.review.as_ref().filter(|r| r.id == review_id).ok_or_else(Self::gone)?;
            if !waited && Utc::now() > current.expires_at {
                return Err(Error::ReviewOutdated(format!("This list was made more than {VALID_FOR_MINUTES} minutes ago. Look again.")));
            }
            if !waited && current.stale.load(Ordering::Relaxed) {
                return Err(Error::ReviewOutdated("Things changed in the folders since this list was made. Look again.".into()));
            }
            if job.setup != current.survey.setup {
                return Err(Error::ReviewOutdated("The job's settings changed since this list was made. Look again.".into()));
            }
            if job.last_run.as_ref().map(|run| run.finished_at) != current.runs_seen {
                return Err(Error::ReviewOutdated("The job synced since this list was made. Look again.".into()));
            }
            if let Some(blocked) = &current.blocked {
                return Err(Error::Invalid(blocked.clone()));
            }
            if current.needs_tick && !acknowledged {
                let tick = if current.steps.is_empty() { "I've read the warnings above" } else { "I've looked at the removals and replacements" };
                return Err(Error::Invalid(format!("Tick “{tick}” first.")));
            }
            Ok(())
        };
        // While another job uses a drive of the folders that's run by an add-on driver,
        // nothing is read there (see `quiet.rs`): the list waits for that job once its own
        // checks pass, and its folders are looked at when it starts.
        if let Some(name) = self.drive_used_by_another(id, Asker::Person) {
            let job = self.stored_job(id)?;
            list_checks(&job, self.runtime().entry(id.clone()).or_default())?;
            return Err(Error::DriveBusy(name));
        }
        // The folders must still be the very folders the list was made from.
        for side in [Side::Source, Side::Destination] {
            let root = review.survey.setup.root(side);
            if !root.is_dir() {
                return Err(Error::Offline(root.to_path_buf()));
            }
            let now = RootId::of(root).map_err(|e| Error::file(root, e))?;
            if let (Some(now), Some(then)) = (now, review.survey.snapshot(side).root()) {
                if !now.same_folder(&then) {
                    return Err(Error::ReviewOutdated(format!(
                        "The {} folder isn't the one this list was made for anymore (was a drive changed?). Look again.",
                        side.word()
                    )));
                }
            }
        }
        // And carry the same markers: a drive swapped for another one under the same name
        // can keep the folder's numbers, but not Charon's marker.
        let markers = Markers::read(review.survey.setup.root(Side::Source), review.survey.setup.root(Side::Destination), id);
        if let Some(side) = markers.changed_since(&review.survey.markers) {
            return Err(Error::ReviewOutdated(format!(
                "Charon's marker in the {} folder changed since this list was made (was a drive changed?). Look again.",
                side.word()
            )));
        }
        let mut reason = match review.kind {
            ReviewKind::Sync => "confirmed by you".to_string(),
            ReviewKind::Resolve => format!("to settle a conflict ({})", review.title.as_deref().unwrap_or("confirmed by you")),
        };
        if waited {
            reason.push_str(", after waiting for another sync");
        }
        let mut crossing =
            Crossing { kind: review.kind, approved: true, by_hand: true, reason, resolving: review.resolving.clone(), outdated: false };
        let (job, cancel) = self.begin_run(id, &crossing, list_checks)?;
        // A list that waited is older than the folders: the sync it waited for changed them,
        // and Charon may not have heard of it (nobody hears what another computer writes to a
        // network share). Nor is what changed in folders that weren't watched while another
        // job's sync kept their drive quiet. Those changes didn't count against the list, and
        // aren't in it: once it's done, the folders are looked at again.
        crossing.outdated = waited || review.unheard.load(Ordering::Relaxed);
        #[cfg(test)]
        tests::before_the_claim();
        if thread::current().name() == Some(AUTOPILOT_THREAD) {
            // Tried again by the autopilot: the sync claims its folders on its own thread. There
            // it waits for the autopilot to stop watching a drive it keeps quiet before it
            // leaves its notes there (see `keep_quiet`); the autopilot can't wait for itself.
            let confirmed = confirmed.clone();
            return self.spawn_crossing(job, cancel, crossing, move |engine, job, cancel, crossing| {
                match engine.claim_for_list(&job, &review, &crossing, &confirmed, waited) {
                    Ok(claim) => {
                        engine.publish(&job.id);
                        engine.take_review(&job.id, &review.id);
                        engine.cross(&job, &review.survey, &cancel, &crossing, claim);
                    }
                    Err(Error::FoldersBusy(who)) | Err(Error::DriveBusy(who)) => engine.keep_waiting(&job.id, &confirmed, who),
                    Err(_) => {}
                }
            });
        }
        let claim = self.claim_for_list(&job, &review, &crossing, confirmed, waited)?;
        self.publish(id);
        self.spawn_crossing(job, cancel, crossing, move |engine, job, cancel, crossing| {
            engine.take_review(&job.id, &review.id);
            engine.cross(&job, &review.survey, &cancel, &crossing, claim);
        })
    }

    /// Claims the folders for the sync of a confirmed list, marked as running (see
    /// `claim_folders`): its busy notes go into the folders before anything changes there, and
    /// before the person is told it started. Another sync at work there ends the run before it
    /// began (`Error::FoldersBusy` or `Error::DriveBusy`: the list waits for it), unless the
    /// person stopped it meanwhile (`Error::Cancelled`).
    fn claim_for_list(&self, job: &crate::model::Job, review: &StoredReview, crossing: &Crossing, confirmed: &Confirmed, waited: bool) -> Result<Claimed> {
        let claim = match self.claim_folders(job, &review.survey) {
            Ok(claim) => claim,
            Err(busy) => return Err(if self.not_begun(job, crossing, busy.waits_for().unwrap_or_default()) { busy } else { Error::Cancelled }),
        };
        // Pausing everything calls off a list that waits, even one whose sync was about to start
        // when the person paused: that pause found the job marked as running, and left it to
        // here. From here on, the sync is at work, and goes on.
        if waited && self.paused_since(confirmed) {
            drop(claim);
            self.call_off_as_it_starts(job, crossing, confirmed);
            return Err(Error::Cancelled);
        }
        Ok(claim)
    }

    /// Prepares the list that settles one conflict the way the person chose. Nothing
    /// changes until it's confirmed like any other list.
    pub fn prepare_resolution(&self, id: &JobId, conflict: &ConflictItem, choice: ConflictChoice) -> Result<Review> {
        let (job, stop, epoch) = self.begin_look(id)?;
        if !job.setup.mode.is_two_way() {
            self.end_look(id, &stop);
            return Err(Error::Invalid("Only two-way jobs have conflicts to settle.".into()));
        }
        let survey = self.survey(&job, &stop, Purpose::Review);
        self.end_look(id, &stop);
        let mut survey = survey?;
        let key = conflict.key.as_str();
        let now = Self::conflict_item(&survey, key, survey.source.get(key).copied(), survey.destination.get(key).copied());
        // Still a conflict now, and the same one (for an app library or a database, a
        // conflict means its files still changed on both sides).
        let still_open = survey.plan.conflicts().iter().any(|(path, ..)| *path == key);
        if &now != conflict || !still_open {
            self.note_look(&job, &survey, Some(epoch));
            self.publish(id);
            return Err(Error::ReviewOutdated("It changed since you saw it. Look again.".into()));
        }
        let plan = Resolution::plan(&survey.source, &survey.destination, key, choice).map_err(Error::Invalid)?;
        let title = format!(
            "Settle “{}”: {}",
            conflict.path,
            match choice {
                ConflictChoice::KeepSource => "keep the source's version",
                ConflictChoice::KeepDestination => "keep the destination's version",
                ConflictChoice::KeepBoth => "keep both versions",
            }
        );
        survey.plan = plan;
        // Folders one inside the other can't settle anything either, whether their paths
        // show it or only Charon's markers do.
        survey.refusal = survey.nesting.clone();
        survey.holds.clear();
        survey.needs_tick = false;
        survey.agreed = 0;
        survey.housekeeping = Default::default();
        // What's said about the drives themselves still holds (another drive, a missing marker).
        survey.warnings.retain(|warning| warning.level == Level::Danger);
        if survey.memory.as_ref().is_some_and(|memory| memory.distrust.is_some()) {
            survey.warnings.push(Warning::info(
                "Charon has no memory of the last sync that it can trust right now, and settling this item doesn't change that: \
                 until you confirm a whole list of this job's changes, it compares both folders afresh and removes nothing.",
            ));
        }
        survey.warnings.push(Warning::info(
            "Only this item changes. The version that is replaced or removed goes to .charon-trash, even when safe deletes are off.",
        ));
        let review = self.keep_review(&job, survey, ReviewKind::Resolve, Some(title), Some(key.to_string()), epoch)?;
        self.publish(id);
        Ok(review)
    }

    // ---------------------------------------------------------------- internals

    /// Marks a look (a review being prepared) as started, if the job isn't busy. Returns the
    /// job, the look's stop flag, and the job's epoch when it began (see `note_look`).
    fn begin_look(&self, id: &JobId) -> Result<(crate::model::Job, Arc<AtomicBool>, u64)> {
        // Another job uses a drive of the folders that's run by an add-on driver: nothing is
        // asked of it, not even the busy notes on it (see `quiet.rs`). The person asked for
        // this list: a look in the background there is stopped (below).
        if let Some(name) = self.drive_used_by_another(id, Asker::Person) {
            return Err(Error::DriveBusy(name));
        }
        // Both ask the disk, so they come before any lock is taken.
        let neighbours = self.neighbours(id);
        let in_use = self.folders_in_use(id);
        let state = self.state();
        let job = state.jobs.iter().find(|job| &job.id == id).cloned().ok_or_else(|| Error::UnknownJob(id.clone()))?;
        let mut runtime = self.runtime();
        if self.is_shutting_down() {
            return Err(Error::Invalid("Charon is quitting.".into()));
        }
        if runtime.get(id).is_some_and(|rt| rt.is_running()) {
            return Err(Error::Busy);
        }
        if let Some(other) = self.drive_used_among(&state.jobs, &runtime, &job, Asker::Person) {
            return Err(Error::DriveBusy(other));
        }
        if let Some(other) = Self::busy_neighbour(&state.jobs, &runtime, &job, &neighbours).or(in_use) {
            return Err(Error::FoldersBusy(other));
        }
        if runtime.get(id).is_some_and(|rt| rt.preparing.is_some()) {
            return Err(Error::Invalid("Charon is already looking at this job's folders.".into()));
        }
        self.stop_looks_sharing_a_drive(&state.jobs, &mut runtime, &job);
        let rt = runtime.entry(id.clone()).or_default();
        // A background look at the same folders would only read them twice, and what it
        // found would be older than what this look finds.
        rt.outdate_looks();
        let stop = Arc::new(AtomicBool::new(false));
        rt.preparing = Some(stop.clone());
        // Its turn on the drive came, if it waited for one.
        rt.drive_wait = None;
        Ok((job, stop, rt.epoch))
    }

    fn end_look(&self, id: &JobId, stop: &Arc<AtomicBool>) {
        self.change_known_runtime(id, |rt| {
            if rt.preparing.as_ref().is_some_and(|flag| Arc::ptr_eq(flag, stop)) {
                rt.preparing = None;
            }
        });
    }

    /// A list made while the job synced, changed or was removed, or while a folder came back.
    fn overtaken() -> Error {
        Error::ReviewOutdated(
            "The job or its folders changed while this list was being made (a sync, an edit, or a drive that came back). Look again.".into(),
        )
    }

    /// A conflict as the window shows it, saying when it's about a whole app library or
    /// database.
    fn conflict_item(survey: &Survey, key: &str, source: Option<Entry>, destination: Option<Entry>) -> ConflictItem {
        let unit = Unit::of(key, &survey.source, &survey.destination).filter(|unit| matches!(unit, Unit::Database(_)) || unit.key() == key);
        ConflictItem::new(key, &survey.spelling(key), source, destination).whole(unit.as_ref())
    }

    fn gone() -> Error {
        Error::ReviewOutdated("This list is no longer current. Look again.".into())
    }

    fn stored_review(&self, id: &JobId, review_id: &str) -> Result<Arc<StoredReview>> {
        self.runtime().get(id).and_then(|rt| rt.review.clone()).filter(|review| review.id == review_id).ok_or_else(Self::gone)
    }

    /// Keeps a review of `survey` for the job (replacing any other, and calling off a confirmed
    /// one that waited, which the job's log says) and returns it. `epoch` is the job's epoch
    /// when the look began: a list the job outgrew meanwhile (it synced, changed or was
    /// removed) isn't kept.
    pub(crate) fn keep_review(
        &self,
        job: &crate::model::Job,
        survey: Survey,
        kind: ReviewKind,
        title: Option<String>,
        resolving: Option<String>,
        epoch: u64,
    ) -> Result<Review> {
        let prepared_at = Utc::now();
        let clearings = survey.plan.clearings();
        let steps: Vec<ReviewStep> = survey.plan.for_review().into_iter().map(|action| Self::step_of(&survey, action, kind, &clearings)).collect();
        let notes: Vec<ReviewNote> = survey
            .plan
            .notes
            .iter()
            .map(|note| ReviewNote {
                kind: note.kind,
                side: note.side,
                path: note.path.clone(),
                reason: note.reason.clone(),
                can_leave_out: note.kind == NoteKind::Unreadable && note.side.is_some(),
            })
            .collect();
        let confirm_anyway = match kind {
            ReviewKind::Sync if steps.is_empty() => Self::confirm_anyway(job, &survey, self.must_ask(&job.id)),
            _ => None,
        };
        let stored = Arc::new(StoredReview {
            id: uuid::Uuid::new_v4().simple().to_string(),
            kind,
            title,
            prepared_at,
            expires_at: prepared_at + Duration::minutes(VALID_FOR_MINUTES),
            stale: AtomicBool::new(false),
            unheard: AtomicBool::new(false),
            runs_seen: job.last_run.as_ref().map(|run| run.finished_at),
            warnings: survey.warnings.clone(),
            blocked: survey.refusal.clone(),
            needs_tick: survey.needs_tick,
            confirm_anyway,
            resolving,
            steps,
            notes,
            survey,
        });
        let review = self.review_of(job, &stored);
        // Returns whether it was kept, and whether that called off a confirmed list.
        let kept = self.change_known_runtime(&job.id, |rt| {
            if rt.epoch != epoch || rt.is_running() {
                return (false, false);
            }
            let called_off = rt.confirmed.is_some();
            rt.forget_review();
            rt.review = Some(stored);
            // The person is looking at the list: the next reminder can wait.
            rt.reminder.last = Some(Utc::now());
            rt.reminder.count = rt.reminder.count.max(1);
            (true, called_off)
        });
        match kept {
            Some((true, called_off)) => {
                if called_off {
                    self.note_called_off(&job.id, " (a newer list took its place)");
                }
                Ok(review)
            }
            Some((false, _)) => Err(Self::overtaken()),
            None => Err(Error::UnknownJob(job.id.clone())),
        }
    }

    /// The review as the window receives it.
    fn review_of(&self, job: &crate::model::Job, stored: &StoredReview) -> Review {
        let survey = &stored.survey;
        let shore = |side: Side| Shore {
            path: survey.setup.root(side).display().to_string(),
            volume: survey.volume(side).map(VolumeLabel::of),
            files: survey.snapshot(side).file_count() as u64,
        };
        let mut housekeeping = survey.housekeeping.lines(self.settings().trash_days);
        housekeeping.retain(|line| !line.is_empty());
        Review {
            review_id: stored.id.clone(),
            job_id: job.id.clone(),
            job_name: job.setup.name.clone(),
            kind: stored.kind,
            title: stored.title.clone(),
            mode: survey.setup.mode,
            safe_delete: survey.setup.safe_delete,
            full_automation: survey.setup.full_automation,
            prepared_at: stored.prepared_at,
            expires_at: stored.expires_at,
            source: shore(Side::Source),
            destination: shore(Side::Destination),
            tally: Self::tally_of(survey),
            warnings: stored.warnings.clone(),
            blocked: stored.blocked.clone(),
            needs_tick: stored.needs_tick,
            confirm_anyway: stored.confirm_anyway.clone(),
            clusters: Self::clusters_of(survey),
            housekeeping,
            total_steps: stored.steps.len(),
            total_notes: stored.notes.len(),
            steps: stored.steps.iter().take(FIRST_STEPS).cloned().collect(),
            notes: stored.notes.iter().take(FIRST_NOTES).cloned().collect(),
        }
    }

    /// Why a list without steps is still worth confirming, in a sentence for the list, or
    /// `None` when it would only say that both folders match. Confirming it changes no file,
    /// but it is a sync like any other: it ends a hold, lets full automation sync by itself
    /// again, and records both folders as they are now (their drives, Charon's markers and,
    /// in two-way, the memory that lets removals cross).
    fn confirm_anyway(job: &crate::model::Job, survey: &Survey, must_ask: bool) -> Option<String> {
        let held = job.held.is_some();
        let why = if job.setup.full_automation && (held || job.needs_review || must_ask) {
            "Confirm once so Charon may sync this job by itself from now on."
        } else if held {
            "Confirm it, and the job stops waiting for your OK."
        } else if survey.unfamiliar {
            "If these are the right folders, confirm it: Charon then takes them as they are now."
        } else if survey.setup.mode.is_two_way() && !survey.trusted {
            "Confirm it, and Charon remembers both folders as they are now, so that later lists can carry what you remove on one side to the other."
        } else if job.last_synced.is_none() {
            "Confirm it as the job's first sync, so Charon knows both folders from now on."
        } else {
            return None;
        };
        Some(why.into())
    }

    fn tally_of(survey: &Survey) -> Tally {
        let mut tally = Tally { agreed: survey.agreed, ..Tally::default() };
        for action in &survey.plan.actions {
            match action {
                Action::CreateDir { side, .. } => tally.side_mut(*side).new_folders += 1,
                Action::Copy { to, bytes, replaces, settles_conflict, .. } => {
                    let side = tally.side_mut(*to);
                    if *replaces {
                        side.replaced += 1;
                    } else {
                        side.new_files += 1;
                    }
                    side.bytes_in += bytes;
                    tally.bytes += bytes;
                    if *settles_conflict {
                        tally.newest_wins += 1;
                    }
                }
                Action::Delete { side, item, .. } => {
                    let side = tally.side_mut(*side);
                    if *item == EntryKind::Dir {
                        side.removed_folders += 1;
                    } else {
                        side.removed += 1;
                    }
                }
                Action::KeepBoth { source_bytes, destination_bytes, .. } => {
                    tally.keep_both += 1;
                    tally.destination.bytes_in += source_bytes;
                    tally.source.bytes_in += destination_bytes;
                    tally.bytes += source_bytes + destination_bytes;
                }
                Action::Conflict { .. } => tally.conflicts += 1,
            }
        }
        tally
    }

    /// Whole folders that go, with what's inside them, biggest first.
    fn clusters_of(survey: &Survey) -> Vec<Cluster> {
        let mut tops: BTreeMap<(Side, String), (u64, u64)> = BTreeMap::new();
        let removed_folders: Vec<(Side, &str)> = survey
            .plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Delete { side, path, item: EntryKind::Dir, .. } => Some((*side, path.as_str())),
                _ => None,
            })
            .collect();
        for &(side, path) in &removed_folders {
            let nested = Keys::ancestors(path).any(|folder| removed_folders.contains(&(side, folder)));
            if !nested {
                tops.insert((side, path.to_string()), (0, 0));
            }
        }
        for action in &survey.plan.actions {
            if let Action::Delete { side, path, entry, .. } = action {
                let top = Keys::ancestors(path).find(|folder| tops.contains_key(&(*side, folder.to_string())));
                if let Some(top) = top {
                    if let Some((items, bytes)) = tops.get_mut(&(*side, top.to_string())) {
                        *items += 1;
                        if entry.is_file() {
                            *bytes += entry.size;
                        }
                    }
                }
            }
        }
        let mut clusters: Vec<Cluster> = tops
            .into_iter()
            .map(|((side, key), (items, bytes))| Cluster { side, path: survey.snapshot(side).spelling(&key).to_string(), items, bytes })
            .collect();
        clusters.sort_by(|a, b| b.items.cmp(&a.items).then_with(|| b.bytes.cmp(&a.bytes)).then_with(|| a.path.cmp(&b.path)));
        clusters.truncate(CLUSTERS);
        clusters
    }

    /// One step as the review shows it, with why it happens. `clearings`: the plan's places
    /// where an item of another kind takes an item's place.
    fn step_of(survey: &Survey, action: &Action, kind: ReviewKind, clearings: &[Clearing]) -> ReviewStep {
        let two_way = survey.setup.mode.is_two_way();
        let resolving = kind == ReviewKind::Resolve;
        let kept = survey.setup.safe_delete || resolving;
        let path = survey.spelling(action.path());
        let chosen = |side: Side| format!("you chose the {}'s version", side.opposite().word());
        match action {
            Action::CreateDir { side, .. } => ReviewStep {
                kind: StepKind::Folder,
                side: Some(*side),
                path,
                item: EntryKind::Dir,
                bytes: 0,
                reason: if resolving {
                    chosen(*side)
                } else if two_way {
                    format!("new folder on the {}", side.opposite().word())
                } else {
                    "a folder in the source".into()
                },
                danger: false,
            },
            Action::Copy { to, item, bytes, replaces, settles_conflict, from, existing, .. } => {
                let fate = if kept || two_way { "the replaced version goes to .charon-trash" } else { "the replaced version is gone for good" };
                let (step, reason) = if resolving {
                    if *replaces {
                        (StepKind::Replace, format!("{}; this side's goes to .charon-trash", chosen(*to)))
                    } else {
                        (StepKind::New, chosen(*to))
                    }
                } else if *settles_conflict {
                    (
                        StepKind::Settle,
                        format!(
                            "changed on both sides; the {}'s version was saved last and replaces this one, which goes to .charon-trash",
                            to.opposite().word()
                        ),
                    )
                } else if *replaces {
                    let why = match existing {
                        Some(existing) if existing.kind != from.kind => format!("the {} has {} here now", Self::other_place(*to, two_way), Self::a(from.kind)),
                        _ if two_way => format!("changed on the {} since the last sync", to.opposite().word()),
                        _ => "changed in the source".into(),
                    };
                    (StepKind::Replace, format!("{why}; {fate}"))
                } else if two_way {
                    (StepKind::New, format!("new on the {}", to.opposite().word()))
                } else {
                    (StepKind::New, "in the source, not yet in the destination".into())
                };
                ReviewStep { kind: step, side: Some(*to), path, item: *item, bytes: *bytes, reason, danger: *replaces }
            }
            Action::Delete { side, item, other, .. } => {
                // What makes room for an item of another kind is kept whatever the setting
                // (see `Executor::delete`).
                let makes_room = clearings.iter().any(|clearing| clearing.holds(action));
                let fate = if kept || makes_room { "goes to .charon-trash" } else { "gone for good" };
                let why = if resolving {
                    chosen(*side)
                } else if let Some(other) = other {
                    format!("makes room: the {} has {} here", Self::other_place(*side, two_way), Self::a(other.kind))
                } else if two_way {
                    format!("removed on the {} since the last sync", side.opposite().word())
                } else {
                    "no longer in the source".into()
                };
                let reason = if *item == EntryKind::Dir {
                    format!("{why}; removed once nothing else is left in it")
                } else {
                    format!("{why}; {fate}")
                };
                ReviewStep { kind: StepKind::Remove, side: Some(*side), path, item: *item, bytes: 0, reason, danger: *item != EntryKind::Dir }
            }
            Action::KeepBoth { source_bytes, destination_bytes, .. } => ReviewStep {
                kind: StepKind::KeepBoth,
                side: None,
                path,
                item: EntryKind::File,
                bytes: source_bytes + destination_bytes,
                reason: if resolving {
                    "you chose to keep both versions; the destination's gets a “(conflict …)” name on both sides".into()
                } else {
                    "changed on both sides: both versions are kept, the destination's under a “(conflict …)” name".into()
                },
                danger: false,
            },
            Action::Conflict { source, destination, path: key } => {
                let unit = Unit::of(key, &survey.source, &survey.destination)
                    .filter(|unit| matches!(unit, Unit::Database(_)) || unit.key() == key);
                let reason = match (source, destination) {
                    _ if unit.is_some() => format!(
                        "{} whose files changed on both sides: left for you to decide which side's copy to keep as a whole",
                        unit.as_ref().map_or("", Unit::describe)
                    ),
                    (Some(s), Some(d)) if s.kind != d.kind => {
                        format!("{} in the source, {} in the destination: left for you to decide", Self::a(s.kind), Self::a(d.kind))
                    }
                    _ => "changed on both sides: left for you to decide".into(),
                };
                ReviewStep {
                    kind: StepKind::Conflict,
                    side: None,
                    path,
                    item: source.or(*destination).map(|e| e.kind).unwrap_or(EntryKind::File),
                    bytes: 0,
                    reason,
                    danger: false,
                }
            }
        }
    }

    /// "the source" / "the destination" for the side across from `side`.
    fn other_place(side: Side, _two_way: bool) -> &'static str {
        side.opposite().word()
    }

    /// "a file", "a folder", "a link".
    fn a(kind: EntryKind) -> &'static str {
        match kind {
            EntryKind::File => "a file",
            EntryKind::Dir => "a folder",
            EntryKind::Link => "a link",
        }
    }

    /// Marks the job's review as outdated (something changed in its folders) and tells the window.
    pub(crate) fn mark_review_stale(&self, id: &JobId) {
        let stale = self.change_runtime(id, |rt| {
            rt.review.as_ref().filter(|review| !review.stale.swap(true, Ordering::Relaxed)).map(|review| review.id.clone())
        });
        if let Some(review_id) = stale {
            self.emit(crate::engine::EngineEvent::ReviewStale { id: id.clone(), review_id });
        }
    }

    /// Records what a survey found as the job's latest look, and since when its changes
    /// have waited. `epoch` is the job's epoch when the look began: a look the job outgrew
    /// meanwhile (a run began, the job changed or was removed, a folder came back) is
    /// dropped, so it never takes the place of a newer one. A run's own look has none: no
    /// other look can begin while a run is going. Returns whether the look was recorded.
    pub(crate) fn note_look(&self, job: &crate::model::Job, survey: &Survey, epoch: Option<u64>) -> bool {
        let Ok(stored) = self.stored_job(&job.id) else { return false };
        // A look made for a setup that changed meanwhile tells nothing.
        if stored.setup != job.setup {
            return false;
        }
        let held = stored.held.is_some();
        let look = Look::of(survey);
        let at = look.at;
        let has_work = look.has_work();
        let needs_a_list = look.needs_a_list();
        let recorded = self.change_known_runtime(&job.id, |rt| {
            if epoch.is_some_and(|epoch| epoch != rt.epoch || rt.is_running()) {
                return false;
            }
            if !needs_a_list && !held {
                rt.reminder.count = 0;
            }
            rt.problem = None;
            rt.look = Some(look);
            // A list counts as a look: the next background look waits its turn.
            rt.last_checked = Some(Instant::now());
            true
        });
        if recorded != Some(true) {
            return false;
        }
        // Saved only when the wait begins or ends. Files not compared yet aren't changes known
        // to wait: after a restart, the row waits for a look to say what's in them.
        let since = self.stored_job(&job.id).ok().and_then(|stored| stored.waiting_since);
        let wanted = if has_work || held { since.or(Some(at)) } else { None };
        if wanted != since {
            if let Err(e) = self.change_job(&job.id, |stored| stored.waiting_since = wanted) {
                log::warn!("couldn't save since when changes of {} wait: {e}", job.setup.name);
            }
        }
        true
    }
}

impl Look {
    pub(crate) fn of(survey: &Survey) -> Look {
        let plan = &survey.plan;
        let summary = plan.summary();
        let mut removals = crate::model::PerSide::default();
        let mut replacements = crate::model::PerSide::default();
        for side in [Side::Source, Side::Destination] {
            removals.add(side, plan.removed_on(side));
            replacements.add(side, plan.destructive_on(side) - plan.removed_on(side));
        }
        let conflicts = plan
            .conflicts()
            .into_iter()
            .map(|(key, source, destination)| Engine::conflict_item(survey, key, source, destination))
            .collect();
        let places = |kinds: &[NoteKind]| -> Vec<crate::model::Place> {
            plan.notes
                .iter()
                .filter(|note| kinds.contains(&note.kind))
                .filter_map(|note| Some(crate::model::Place::new(note.kind, note.side?, note.path.clone(), note.reason.clone())))
                .collect()
        };
        let mut unreadable = places(&[NoteKind::Unreadable]);
        let unreadable_count = unreadable.len() as u64;
        unreadable.truncate(crate::model::PLACES_SHOWN);
        let mut not_synced = places(&[NoteKind::LeftOut, NoteKind::NameClash, NoteKind::Blocked]);
        let not_synced_count = not_synced.len() as u64;
        not_synced.truncate(crate::model::PLACES_SHOWN);
        let volumes = match (survey.volume(Side::Source), survey.volume(Side::Destination)) {
            (Some(s), Some(d)) => Some((VolumeLabel::of(s), VolumeLabel::of(d))),
            _ => None,
        };
        Look {
            setup: survey.setup.clone(),
            at: survey.made_at,
            items: summary.changes(),
            bytes: summary.copy_bytes,
            removals,
            replacements,
            conflicts,
            unreadable,
            unreadable_count,
            not_synced,
            not_synced_count,
            volumes,
            uncompared: survey.uncompared.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConflictRule, JobSetup, SyncMode, Trigger};
    use crate::run::{BusyNote, BUSY_EXTENSION};
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;

    thread_local! {
        /// What `carry_out` runs on this thread once the job is marked as running, right before
        /// the sync claims its folders: the moment a confirmed list's sync neither waits nor is
        /// at work yet.
        static BEFORE_THE_CLAIM: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// Runs what this thread set to run before the next claim, once (see `when_claiming`).
    pub(super) fn before_the_claim() {
        if let Some(hook) = BEFORE_THE_CLAIM.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    /// Pauses everything on this thread when the next confirmed list's sync is about to claim
    /// its folders, after making sure the job was marked as running then.
    fn pause_everything_when_claiming(engine: &Engine, id: &JobId) {
        let (engine, id) = (engine.clone(), id.clone());
        let hook = move || {
            assert!(engine.is_running(&id), "the job is marked as running");
            engine.set_all_paused(true).unwrap();
        };
        BEFORE_THE_CLAIM.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    /// Whether what this thread set to run before a claim ran.
    fn ran_when_claiming() -> bool {
        BEFORE_THE_CLAIM.with(|hook| hook.borrow().is_none())
    }

    /// A Backup job "Docs" whose list copies Projects/plan.txt into the destination's Projects
    /// folder, ready to confirm. Returns the engine, the job, its destination and the list.
    fn docs(tmp: &Path) -> (Engine, JobId, PathBuf, String) {
        let (src, dst) = (tmp.join("src"), tmp.join("dst"));
        fs::create_dir_all(src.join("Projects")).unwrap();
        fs::create_dir_all(dst.join("Projects")).unwrap();
        fs::write(src.join("Projects/plan.txt"), "plan").unwrap();
        let engine = Engine::open(tmp.join("data"), Arc::new(|_| {})).unwrap();
        let setup = JobSetup {
            name: "Docs".into(),
            source: src,
            destination: dst.clone(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: JobSetup::default_excludes(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while engine.job(&id).unwrap().looking {
            assert!(Instant::now() < deadline, "the first look took too long");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let review = engine.prepare_review(&id).unwrap();
        (engine, id, dst, review.review_id)
    }

    /// Leaves a note in `folder` as Charon on another computer does while it syncs there. In a
    /// folder inside the job's folders that had no Charon folder when they were looked at, only
    /// a sync's claim meets it, once the job is marked as running (see `claim_folders`).
    fn another_charon_syncs(folder: &Path) -> PathBuf {
        another_charon_syncs_on(folder, "studio-mac")
    }

    /// `another_charon_syncs`, from the computer named `host`.
    fn another_charon_syncs_on(folder: &Path, host: &str) -> PathBuf {
        let mut note = BusyNote::new(&"elsewhere".to_string(), "Projects", "another-charon");
        note.host = host.into();
        let file = folder.join(".charon").join(format!("another-charon{BUSY_EXTENSION}"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, serde_json::to_vec(&note).unwrap()).unwrap();
        file
    }

    /// The busy notes in the Charon folder of `folder`.
    fn busy_notes(folder: &Path) -> usize {
        fs::read_dir(folder.join(".charon"))
            .map(|items| items.flatten().filter(|item| item.file_name().to_string_lossy().ends_with(BUSY_EXTENSION)).count())
            .unwrap_or(0)
    }

    fn wait_idle(engine: &Engine, id: &JobId) {
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while engine.is_running(id) {
            assert!(Instant::now() < deadline, "the sync took too long");
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Waits until `done` holds: a sync's own thread says what became of its list (that it
    /// waits on, or was called off) a moment after `wait_idle` sees its run end.
    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "{what} took too long");
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Asserts that the job's confirmed list doesn't wait anymore, and that its log says Pause
    /// everything called it off.
    fn called_off_by_pausing_everything(engine: &Engine, id: &JobId) {
        let called_off = "the confirmed sync that waited for another sync was called off (Pause everything)";
        until("the log line", || engine.log(id, 1).first().is_some_and(|line| line.text == called_off));
        let row = engine.job(id).unwrap();
        assert!(!row.confirmed && row.waiting_for.is_none(), "the list no longer waits");
    }

    #[test]
    fn pausing_everything_while_a_waiting_list_is_tried_again_calls_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());
        let note = another_charon_syncs(&dst.join("Projects"));
        assert!(matches!(engine.approve(&id, &review_id, true).unwrap(), Approval::Waiting { .. }));

        // The list is tried again: its sync is marked as running, and before it finds the other
        // sync still at work, the person pauses everything.
        pause_everything_when_claiming(&engine, &id);
        assert!(!engine.start_confirmed(&id));
        assert!(ran_when_claiming() && engine.settings().paused_all);
        called_off_by_pausing_everything(&engine, &id);

        // The other sync ends: nothing starts by itself.
        fs::remove_file(note).unwrap();
        assert!(!engine.start_confirmed(&id));
        assert!(!engine.is_running(&id) && !dst.join("Projects/plan.txt").exists(), "nothing ran");
    }

    /// Runs `try_it` as the autopilot does, on a thread of its name.
    fn as_the_autopilot<T: Send + 'static>(engine: &Engine, id: &JobId, try_it: impl FnOnce(&Engine, &JobId) -> T + Send + 'static) -> T {
        let (engine, id) = (engine.clone(), id.clone());
        thread::Builder::new().name(AUTOPILOT_THREAD.into()).spawn(move || try_it(&engine, &id)).unwrap().join().unwrap()
    }

    #[test]
    fn a_list_the_autopilot_tries_again_keeps_waiting_when_its_sync_meets_another_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());
        let note = another_charon_syncs(&dst.join("Projects"));
        assert!(matches!(engine.approve(&id, &review_id, true).unwrap(), Approval::Waiting { .. }));

        // The autopilot tries the list again. It hands the sync to a thread of its own, which
        // claims the folders there and meets the other sync still at work: the list waits on.
        // (That sync now says it's on another computer, so the row shows what this try found.)
        another_charon_syncs_on(&dst.join("Projects"), "laptop");
        assert!(as_the_autopilot(&engine, &id, |engine, id| engine.start_confirmed(id)), "handed to its sync's thread");
        wait_idle(&engine, &id);
        until("the row", || engine.job(&id).unwrap().waiting_for.as_deref() == Some("Charon on laptop"));
        assert!(engine.job(&id).unwrap().confirmed, "the list still waits");
        assert!(!dst.join("Projects/plan.txt").exists() && engine.job(&id).unwrap().job.last_run.is_none(), "nothing ran");

        // Once that sync is done, the next try carries it out.
        fs::remove_file(note).unwrap();
        assert!(as_the_autopilot(&engine, &id, |engine, id| engine.start_confirmed(id)));
        wait_idle(&engine, &id);
        assert!(dst.join("Projects/plan.txt").exists());
    }

    #[test]
    fn pausing_everything_as_the_autopilot_starts_a_waiting_list_calls_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());
        let note = another_charon_syncs(&dst.join("Projects"));
        assert!(matches!(engine.approve(&id, &review_id, true).unwrap(), Approval::Waiting { .. }));

        // The other sync ends, and the autopilot tries the list again: its sync is marked as
        // running, and before its thread claims the folders, the person pauses everything.
        fs::remove_file(note).unwrap();
        let started = as_the_autopilot(&engine, &id, |engine, id| {
            pause_everything_when_claiming(engine, id);
            let started = engine.start_confirmed(id);
            assert!(ran_when_claiming());
            started
        });
        assert!(started, "handed to its sync's thread");
        wait_idle(&engine, &id);
        assert!(engine.settings().paused_all && !dst.join("Projects/plan.txt").exists(), "nothing ran");
        called_off_by_pausing_everything(&engine, &id);
        assert!(engine.job(&id).unwrap().job.last_run.is_none(), "no sync began");
        assert_eq!(busy_notes(&dst) + busy_notes(&dst.join("Projects")), 0, "its busy notes are gone");
    }

    #[test]
    fn pausing_everything_just_as_a_waiting_list_starts_calls_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());
        let note = another_charon_syncs(&dst.join("Projects"));
        assert!(matches!(engine.approve(&id, &review_id, true).unwrap(), Approval::Waiting { .. }));

        // The other sync ends, and the list is tried again: its sync is marked as running, and
        // before it claims the folders, the person pauses everything.
        fs::remove_file(note).unwrap();
        pause_everything_when_claiming(&engine, &id);
        let started = engine.start_confirmed(&id);
        assert!(ran_when_claiming() && engine.settings().paused_all);
        wait_idle(&engine, &id);
        assert!(!started && !dst.join("Projects/plan.txt").exists(), "nothing ran");
        called_off_by_pausing_everything(&engine, &id);
        assert!(engine.job(&id).unwrap().job.last_run.is_none(), "no sync began");
        assert_eq!(busy_notes(&dst) + busy_notes(&dst.join("Projects")), 0, "its busy notes are gone");
    }

    #[test]
    fn pausing_everything_as_a_list_is_confirmed_while_another_sync_works_keeps_it_from_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());
        let note = another_charon_syncs(&dst.join("Projects"));

        // The person confirms the list; its sync is marked as running, and before it finds the
        // other sync at work, they pause everything.
        pause_everything_when_claiming(&engine, &id);
        let approved = engine.approve(&id, &review_id, true);
        assert!(ran_when_claiming() && engine.settings().paused_all);
        let row = engine.job(&id).unwrap();
        assert!(!row.confirmed && row.waiting_for.is_none(), "the list doesn't wait");
        // The window then lets the list go and only offers to look again, so that's what it asks.
        let refused = approved.expect_err("the window is told it doesn't wait");
        assert_eq!(
            refused.to_string(),
            "You paused everything just as this list was about to start, and “Charon on studio-mac” isn't done yet, \
             so the list doesn't wait for it. Look again once it's done."
        );
        let said = engine.log(&id, 1).first().map(|line| line.text.clone()).unwrap_or_default();
        assert!(said.contains("you paused everything"), "{said}");
        fs::remove_file(&note).unwrap();
        assert!(!engine.start_confirmed(&id));
        assert!(!dst.join("Projects/plan.txt").exists(), "nothing ran");

        // Once that sync is done, the person looks again, as the window does it: the list is let
        // go, and a new one is made. Confirmed while another sync works there, the new list
        // waits, paused or not (the person asked for it), and syncs once that sync is done.
        engine.discard_review(&id, &review_id);
        let again = engine.prepare_review(&id).unwrap();
        let note = another_charon_syncs(&dst.join("Projects"));
        assert!(matches!(engine.approve(&id, &again.review_id, true).unwrap(), Approval::Waiting { .. }));
        fs::remove_file(note).unwrap();
        assert!(engine.start_confirmed(&id));
        wait_idle(&engine, &id);
        assert!(dst.join("Projects/plan.txt").exists());
    }

    #[test]
    fn a_list_confirmed_just_as_everything_is_paused_syncs_when_nothing_else_works_there() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id, dst, review_id) = docs(tmp.path());

        // A sync the person confirmed that's at work goes on.
        pause_everything_when_claiming(&engine, &id);
        assert_eq!(engine.approve(&id, &review_id, true).unwrap(), Approval::Started);
        assert!(ran_when_claiming() && engine.settings().paused_all);
        wait_idle(&engine, &id);
        assert!(dst.join("Projects/plan.txt").exists());
    }

    #[test]
    fn a_confirmed_sync_stopped_after_it_was_picked_up_never_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        let engine = Engine::open(tmp.path().join("data"), Arc::new(|_| {})).unwrap();
        let setup = JobSetup {
            name: "Photos".into(),
            source: src.clone(),
            destination: dst.clone(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: JobSetup::default_excludes(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let review = engine.prepare_review(&id).unwrap();
        // Confirmed while another sync used the folders, as `queue_confirmed` keeps it.
        let waiting = Confirmed { review_id: review.review_id.clone(), acknowledged: true, pauses: engine.pauses() };
        engine.change_runtime(&id, |rt| rt.confirmed = Some(waiting.clone()));

        // `start_confirmed` picks it up, and the person presses Stop before its sync begins.
        let picked = engine.runtime().get(&id).and_then(|rt| rt.confirmed.clone()).unwrap();
        engine.cancel(&id);
        assert!(matches!(engine.carry_out(&id, &picked, true), Err(Error::Cancelled)));
        assert!(!engine.is_running(&id));
        assert!(!dst.join("a.txt").exists(), "nothing ran");

        // The same list, not called off, starts.
        engine.change_runtime(&id, |rt| rt.confirmed = Some(waiting.clone()));
        engine.carry_out(&id, &waiting, true).unwrap();
        while engine.is_running(&id) {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(dst.join("a.txt").exists());
    }
}
