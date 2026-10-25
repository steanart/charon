//! Adding, editing, pausing and removing jobs.

use crate::engine::quiet::Asker;
use crate::engine::{Engine, EngineEvent};
use crate::error::{Error, Result};
use crate::model::{Folder, Job, JobId, JobSetup, JobView, SyncMode};
use crate::plan::Side;
use crate::run::Marker;
use crate::scan::ExcludeFilter;
use std::collections::BTreeMap;
use std::path::Path;

impl Engine {
    /// Adds a job and looks at what it would do. Its first sync always waits for the
    /// person's OK.
    pub fn add_job(&self, mut setup: JobSetup) -> Result<JobView> {
        self.resolving_for_saving(|resolve| setup.validate_as(resolve))??;
        ExcludeFilter::check(&setup.excludes)?;
        let overlap = self.overlap_of(&setup, &self.job_list(), None)?;
        let job = Job::new(setup);
        let id = job.id.clone();
        let automation = job.setup.full_automation;
        self.change_state(|state| {
            if let Some(other) = overlap.clash(&state.jobs)? {
                return Err(Error::Invalid(JobSetup::clash_message(other)));
            }
            state.jobs.push(job);
            Ok(())
        })??;
        self.inner.journal.info(&id, "job created");
        if automation {
            self.inner.journal.warn(&id, "full automation turned on (both warnings accepted)");
        }
        self.publish(&id);
        self.check(&id);
        self.job(&id)
    }

    /// Saves a changed setup. Refused while the job syncs. A change to what a sync does
    /// (folders, mode, conflict rule, excludes, safe deletes, or turning full automation on)
    /// makes the next sync wait for the person's OK; new folders or a new mode also make
    /// Charon forget what it remembers of the last sync.
    pub fn update_job(&self, id: &JobId, mut setup: JobSetup) -> Result<JobView> {
        self.resolving_for_saving(|resolve| setup.validate_as(resolve))??;
        ExcludeFilter::check(&setup.excludes)?;
        let (name, overlap) = {
            let jobs = self.job_list();
            let job = jobs.iter().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
            (job.setup.name.clone(), self.overlap_of(&setup, &jobs, Some(id))?)
        };

        // The setup it replaces is read where it's replaced, so two quick edits in a row
        // each compare with the one saved before. A confirmed list that waits for another
        // sync was made for the old setup: the edit calls it off once it's saved, and says so.
        let (old, called_off) = self.with_job_at_rest(
            id,
            &format!("Wait until “{name}” finishes syncing, then edit it."),
            |state| {
                let job = state.jobs.iter().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
                let old = job.setup.clone();
                let folders_changed = !old.same_folders(&setup);
                // An overlap that was there before (from an older Charon) doesn't block an edit
                // that keeps the folders and the mode, such as a new name; every look still warns
                // about it. New folders or a new mode must not overlap at all.
                if folders_changed {
                    if let Some(other) = overlap.clash(&state.jobs)? {
                        return Err(Error::Invalid(JobSetup::clash_message(other)));
                    }
                }
                let review_next = setup.changes_what_syncs(&old) || (setup.full_automation && !old.full_automation);
                let job = state.jobs.iter_mut().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
                if job.setup != setup {
                    job.waiting_since = None;
                }
                job.setup = setup.clone();
                if review_next {
                    job.needs_review = true;
                }
                if folders_changed {
                    job.drives = None;
                    job.destination_token = None;
                    job.source_mark = None;
                    job.run_in_progress = false;
                    job.held = None;
                    job.automatic_changes.clear();
                }
                Ok(old)
            },
            |rt, old| {
                let called_off = rt.confirmed.is_some();
                if old.same_folders(&setup) {
                    rt.forget_review();
                } else {
                    rt.forget_folders();
                }
                // Whatever a look in progress finds is for the old setup: the new one is looked
                // at as soon as nothing else is looking.
                rt.outdate_looks();
                rt.last_checked = None;
                (old, called_off)
            },
        )?;
        let folders_changed = !old.same_folders(&setup);
        let automation_on = setup.full_automation && !old.full_automation;
        let automation_off = !setup.full_automation && old.full_automation;
        if folders_changed {
            if let Err(e) = self.inner.baselines.forget(id) {
                // A memory for other folders is never used (it names its folders), so this
                // only leaves a stale file behind.
                log::warn!("couldn't forget the two-way memory of job {id}: {e}");
            }
            if let Err(e) = self.inner.landings.forget(id) {
                // The record names its folders and the destination's marker, so it isn't used.
                log::warn!("couldn't forget the copies whose dates didn't stick for job {id}: {e}");
            }
            if let Err(e) = self.inner.same_contents.forget(id) {
                // The record names its folders, so it isn't used.
                log::warn!("couldn't forget the files found identical for job {id}: {e}");
            }
            if let Err(e) = self.inner.unattended.forget(id) {
                // A count for other folders only makes the guard more careful.
                log::warn!("couldn't start the count of automatic changes of job {id} again: {e}");
            }
            if let Err(e) = self.inner.absences.forget(id) {
                // The record names its folders, so it isn't used.
                log::warn!("couldn't forget the items missing in the folders of job {id}: {e}");
            }
        }
        self.inner.journal.info(id, if folders_changed { "folders or mode changed: Charon forgot the last sync" } else { "settings changed" });
        if automation_on {
            self.inner.journal.warn(id, "full automation turned on (both warnings accepted)");
        }
        if automation_off {
            self.inner.journal.info(id, "full automation turned off");
        }
        if called_off {
            self.note_called_off(id, " (the job was edited)");
        }
        self.publish(id);
        self.check(id);
        self.job(id)
    }

    /// A copy of a job, named "… copy", paused, and with full automation off (turning it
    /// on again goes through both warnings). Refused when the copy would clash with a job
    /// (always for a Mirror job, which would share its destination with its copy): a new
    /// job with other folders is the way then.
    pub fn duplicate_job(&self, id: &JobId) -> Result<JobView> {
        let jobs = self.job_list();
        let original = jobs.iter().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
        let mut setup = original.setup.clone();
        setup.name = format!("{} copy", setup.name);
        setup.full_automation = false;
        let overlap = self.overlap_of(&setup, &jobs, None)?;
        let mut job = Job::new(setup);
        job.paused = true;
        let new_id = job.id.clone();
        let name = original.setup.name.clone();
        self.change_state(|state| {
            if let Some(other) = overlap.clash(&state.jobs)? {
                return Err(Error::Invalid(format!(
                    "A copy of “{name}” would share its folders with “{other}”, and one of the two would be a Mirror job that removes what the other brings in. Add a new job with other folders instead."
                )));
            }
            state.jobs.push(job);
            Ok(())
        })??;
        self.inner.journal.info(&new_id, "job created as a copy");
        self.publish(&new_id);
        self.job(&new_id)
    }

    /// Pausing stops a running sync too, and calls off one that waits for another sync.
    /// Paused jobs only sync when asked.
    pub fn set_paused(&self, id: &JobId, paused: bool) -> Result<()> {
        // Saved first: a run a trigger starts from now on sees it and doesn't begin (see
        // `begin_run`), and one that began before is stopped below.
        let saved = self.change_job(id, |job| job.paused = paused);
        if paused {
            // Even when the pause couldn't be saved: the person wants it to stop.
            self.stop_paused(id);
        }
        saved?;
        self.inner.journal.info(id, if paused { "paused" } else { "resumed" });
        self.publish(id);
        Ok(())
    }

    /// Removes a job. Refused while it syncs. Its folders and files are left alone; only
    /// Charon's own marker for the job is taken out of them.
    pub fn remove_job(&self, id: &JobId) -> Result<()> {
        let (job, name) = {
            let job = self.stored_job(id)?;
            let name = job.setup.name.clone();
            (job, name)
        };
        // Removing it takes Charon's marker out of its folders: not on a drive run by an
        // add-on driver that another job's run or list uses right now (see `quiet.rs`). A
        // look in the background there doesn't hold it up: that only reads.
        if let Some(other) = self.drive_used_by_another(id, Asker::Person) {
            return Err(Error::Invalid(format!(
                "“{other}” is using the same drive right now. Its driver isn't Apple's own, so Charon works on one job at a time there. Remove “{name}” once it's done."
            )));
        }
        self.with_job_at_rest(
            id,
            &format!("Stop “{name}” first, then remove it."),
            |state| {
                state.jobs.retain(|job| &job.id != id);
                Ok(())
            },
            |rt, ()| rt.outdate_looks(),
        )?;
        self.runtime().remove(id);
        if let Err(e) = self.inner.baselines.forget(id) {
            log::warn!("couldn't forget the two-way memory of job {id}: {e}");
        }
        if let Err(e) = self.inner.landings.forget(id) {
            log::warn!("couldn't forget the copies whose dates didn't stick for job {id}: {e}");
        }
        if let Err(e) = self.inner.same_contents.forget(id) {
            log::warn!("couldn't forget the files found identical for job {id}: {e}");
        }
        if let Err(e) = self.inner.unattended.forget(id) {
            log::warn!("couldn't forget the count of automatic changes of job {id}: {e}");
        }
        if let Err(e) = self.inner.absences.forget(id) {
            log::warn!("couldn't forget the items missing in the folders of job {id}: {e}");
        }
        self.inner.journal.forget(id);
        for side in [Side::Source, Side::Destination] {
            let root = job.setup.root(side);
            if root.is_dir() {
                Marker::forget(root, id);
            }
        }
        self.emit(EngineEvent::JobRemoved(id.clone()));
        self.emit(EngineEvent::Overview(self.overview()));
        Ok(())
    }

    /// Moves a job to a new position in the list.
    pub fn move_job(&self, id: &JobId, to_index: usize) -> Result<()> {
        self.change_state(|state| {
            if let Some(from) = state.jobs.iter().position(|job| &job.id == id) {
                let job = state.jobs.remove(from);
                let to = to_index.min(state.jobs.len());
                state.jobs.insert(to, job);
            }
        })
    }

    /// "Leave this folder out": excludes exactly the place at `path` (as spelled on disk,
    /// relative to the job's folder). The next sync waits for the person's OK.
    pub fn leave_out(&self, id: &JobId, side: Side, path: &str) -> Result<JobView> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Err(Error::Invalid("Choose a place inside the job's folders.".into()));
        }
        let pattern = ExcludeFilter::pattern_for(path);
        ExcludeFilter::check(std::slice::from_ref(&pattern))?;
        let name = self.stored_job(id)?.setup.name;
        // A confirmed list that waits for another sync didn't leave the place out: it's called
        // off once the change is saved, and the log says so.
        let called_off = self.with_job_at_rest(
            id,
            &format!("Wait until “{name}” finishes syncing."),
            |state| {
                let job = state.jobs.iter_mut().find(|job| &job.id == id).ok_or_else(|| Error::UnknownJob(id.clone()))?;
                if !job.setup.excludes.contains(&pattern) {
                    job.setup.excludes.push(pattern.clone());
                }
                job.needs_review = true;
                Ok(())
            },
            |rt, ()| {
                let called_off = rt.confirmed.is_some();
                rt.outdate_looks();
                rt.forget_review();
                rt.last_checked = None;
                called_off
            },
        )?;
        self.inner.journal.info(id, &format!("left out {path} (on the {})", side.word()));
        if called_off {
            self.note_called_off(id, " (a place was left out)");
        }
        self.publish(id);
        self.check(id);
        self.job(id)
    }
}

/// A setup checked against the other jobs before any lock is taken, because the check asks
/// the disk (a network share that doesn't answer must not hold up every other job): those
/// jobs' folders and modes as they were, and the first job the setup clashes with.
struct Overlap {
    /// The job being edited, which isn't checked against itself.
    edited: Option<JobId>,
    /// Every other job's folders and mode, as checked.
    others: BTreeMap<JobId, Reach>,
    /// The name of the first job the setup clashes with.
    clash: Option<String>,
}

/// What a clash depends on: a job's folders and its mode.
type Reach = (std::path::PathBuf, std::path::PathBuf, SyncMode);

impl Engine {
    /// Checks `setup` against every job in `jobs` but `edited` (see `Overlap`), asking about
    /// folders on a drive a sync keeps quiet only what's known (see `resolving_for_saving`).
    fn overlap_of(&self, setup: &JobSetup, jobs: &[Job], edited: Option<&JobId>) -> Result<Overlap> {
        self.resolving_for_saving(|resolve| Overlap::of(setup, jobs, edited, resolve))
    }

    /// Runs `work` with folders resolved for saving a job: from the disk, but as last found
    /// for those on a drive a sync keeps quiet (see `folders_for_saving`). One of those that
    /// was never found can't be compared with anything, so the save waits for that sync.
    fn resolving_for_saving<T>(&self, work: impl FnOnce(&dyn Fn(&[&Path]) -> Vec<Option<Folder>>) -> T) -> Result<T> {
        let unknown = std::cell::RefCell::new(None);
        let resolve = |paths: &[&Path]| {
            self.folders_for_saving(paths).unwrap_or_else(|name| {
                unknown.borrow_mut().get_or_insert(name);
                vec![None; paths.len()]
            })
        };
        let done = work(&resolve);
        match unknown.into_inner() {
            Some(name) => Err(Error::Invalid(format!(
                "“{name}” is syncing on the same drive right now, and Charon leaves that drive to it. Save this job once it's done."
            ))),
            None => Ok(done),
        }
    }
}

impl Overlap {
    /// Checks `setup` against every job in `jobs` but `edited`, with each folder as
    /// `resolve` finds it (see `JobSetup::clashes_as`).
    fn of(setup: &JobSetup, jobs: &[Job], edited: Option<&JobId>, resolve: impl Fn(&[&Path]) -> Vec<Option<Folder>>) -> Self {
        let others: Vec<&Job> = jobs.iter().filter(|job| Some(&job.id) != edited).collect();
        let clash = others.iter().find(|other| setup.clashes_as(&other.setup, &resolve)).map(|other| other.setup.name.clone());
        Self { edited: edited.cloned(), others: others.into_iter().map(|job| (job.id.clone(), Self::reach(&job.setup))).collect(), clash }
    }

    fn reach(setup: &JobSetup) -> Reach {
        (setup.source.clone(), setup.destination.clone(), setup.mode)
    }

    /// The name of the job the setup clashes with, if any, as long as `jobs` (the stored
    /// jobs, read under the lock) still have the folders and modes it was checked against.
    /// If another job was added, or its folders or mode changed, in between, the save is
    /// refused, to be tried again. (A job renamed meanwhile is named as it was.)
    fn clash(&self, jobs: &[Job]) -> Result<Option<&str>> {
        let now: BTreeMap<JobId, Reach> =
            jobs.iter().filter(|job| self.edited.as_ref() != Some(&job.id)).map(|job| (job.id.clone(), Self::reach(&job.setup))).collect();
        if now != self.others {
            return Err(Error::Invalid("Another job changed while this one was being saved. Save it again.".into()));
        }
        Ok(self.clash.as_deref())
    }
}
