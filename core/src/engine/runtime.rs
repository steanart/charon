//! The live state of a job: everything that isn't saved to disk.

use crate::engine::quiet::DriveWait;
use crate::engine::review::StoredReview;
use crate::model::{ConflictItem, DatedAhead, Job, JobSetup, PerSide, Place, Progress, VolumeLabel};
use crate::plan::{Action, Plan, Side, Unit};
use crate::run::{Found, Markers, Refused, Unread};
use crate::scan::{Entry, ExcludeFilter, Keys, Snapshot, Volume};
use crate::store::Absence;
use chrono::{DateTime, Utc};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct JobRuntime {
    /// Set while a sync runs; setting it to true stops the run.
    pub cancel: Option<Arc<AtomicBool>>,
    /// The running sync is one nobody confirmed a list for (full automation): *Pause
    /// everything* stops it.
    pub unattended: bool,
    /// The person pressed Stop on the running sync (not Charon quitting).
    pub stopped_by_person: bool,
    /// Runs nobody looks at are refused until the person confirms a list (they stopped a
    /// sync, or a run was held back). Kept here too, in case the job's record couldn't be saved.
    pub must_ask: bool,
    /// A run nobody looks at is due (full automation). It starts as soon as nothing stands
    /// in its way, so a trigger is never lost while another job uses the folders.
    pub wanted: Option<Wanted>,
    /// The person confirmed the job's list while another sync used its folders: it starts
    /// as soon as that sync is done (see `Engine::start_confirmed`).
    pub confirmed: Option<Confirmed>,
    /// Who the confirmed run, or else the wanted run, waits for, by name: another job whose
    /// sync is at work in its folders, or whose run, list or look holds a drive of them run by
    /// an add-on driver, or whose sync keeps quiet a drive the wanted run must ask about (see
    /// `waits_on`); or another Charon.
    pub waiting_for: Option<String>,
    /// Since when the job's look in the background or its wanted run waits for its turn on a
    /// drive run by an add-on driver (see `quiet.rs`).
    pub drive_wait: Option<DriveWait>,
    /// A folder on a drive another job's sync keeps quiet, which the job's last run nobody
    /// looks at couldn't ask whether it overlaps with this job's (see `Survey::waits_on`):
    /// the wanted run waits until no sync keeps it quiet, and doesn't look again meanwhile.
    pub waits_on: Option<PathBuf>,
    /// Set while a review is being prepared; setting it to true stops that.
    pub preparing: Option<Arc<AtomicBool>>,
    /// Set while a background check runs; setting it to true stops it.
    pub checking: Option<Arc<AtomicBool>>,
    /// When the job's look last moved on (see `Engine::note_look_moved`).
    pub look_moved: Option<Instant>,
    pub progress: Option<Progress>,
    /// What the last look found.
    pub look: Option<Look>,
    /// Goes up whenever what a look in progress would find becomes outdated: a run starts,
    /// the job's setup changes, a folder comes back, a newer look begins. A look notes it
    /// when it begins, and what it found counts only while it hasn't changed (see
    /// `Engine::note_look`).
    pub epoch: u64,
    /// The list waiting for the person's OK.
    pub review: Option<Arc<StoredReview>>,
    /// What went wrong in the last look or run.
    pub problem: Option<String>,
    pub last_checked: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub last_progress_sent: Option<Instant>,
    pub reminder: Reminder,
    /// Pairs of files compared byte for byte in this session (see `Reads`).
    pub reads: Reads,
    /// The drive of the source / destination turned out not to hold symbolic links.
    pub no_links: (bool, bool),
    /// Items a drive refused in a sync (by the side they were to go to, and key), with that
    /// drive. Looks leave them out while the item and the drive stay the same.
    pub refused: HashMap<(Side, String), (Refused, Option<Volume>)>,
    /// Files a sync couldn't read (by the side they are on, and key). While one still can't
    /// be read, looks list it as a place they can't read, and remove nothing on the side it
    /// should be copied to.
    pub unread: HashMap<(Side, String), Unread>,
    /// The job's exclude filter, to tell which folder changes matter: the setup it was made
    /// for, and the filter (matching case the way the job's drives do, once a look has
    /// found out how they do).
    pub filter: Option<(JobSetup, ExcludeFilter)>,
    /// Items looks found missing on a drive that may show changes late, and for how long.
    pub missing: Missing,
    /// Folders inside the job's folders that hold Charon's own folder, as the last look met
    /// them: other jobs' folders, maybe reached under other names, where another sync's busy
    /// note may be (see `run::Claim`).
    pub folders_inside: Vec<PathBuf>,
    /// What the running (or just finished) sync writes, to tell its own writes from the
    /// changes the folder watcher hears about meanwhile.
    pub own_writes: Option<OwnWrites>,
    /// When the look was made whose changes the last automatic run set out to carry, if
    /// that run ended before carrying anything (a folder it couldn't read, Charon's state
    /// that couldn't be saved). Changes found by that look aren't tried again until a newer
    /// look finds them: a run that fails every time mustn't start every second.
    pub tried: Option<DateTime<Utc>>,
    /// The job's last sync, once Charon found it dated later than now, and for how long it
    /// has known so (see `Engine::note_syncs_dated_ahead`).
    pub dated_ahead: Option<FoundAhead>,
}

impl JobRuntime {
    pub fn is_running(&self) -> bool {
        self.cancel.is_some()
    }

    pub fn is_checking(&self) -> bool {
        self.checking.is_some()
    }

    /// Stops the running sync, if there is one, as the person asked (Stop, pausing the job):
    /// what it already carried stays, and the job's next sync waits for their OK.
    pub fn stop_for_person(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.stopped_by_person = true;
        }
    }

    /// Stops a running check or review preparation (the job changed under them).
    pub fn stop_looking(&mut self) {
        for flag in [self.checking.as_ref(), self.preparing.as_ref()].into_iter().flatten() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Stops the looks in progress, and makes whatever they find count for nothing: the job
    /// or its folders are about to change, or just did.
    pub fn outdate_looks(&mut self) {
        self.stop_looking();
        self.epoch += 1;
    }

    /// Forgets the list waiting for the person's OK, and a confirmed run waiting to start.
    pub fn forget_review(&mut self) {
        self.review = None;
        self.call_off_confirmed();
    }

    /// Calls off the confirmed list that waits for another sync, keeping the list itself.
    /// Returns whether there was one.
    pub fn call_off_confirmed(&mut self) -> bool {
        let waited = self.confirmed.take().is_some();
        if waited {
            self.waiting_for = None;
        }
        waited
    }

    /// Forgets the wanted run, if `which` says so. Returns the sync it waited for, if the
    /// row said it waited. (What a confirmed list waits for stays: it's the list's.)
    pub fn forget_wanted(&mut self, which: impl FnOnce(&Wanted) -> bool) -> Option<String> {
        self.wanted.take_if(|wanted| which(wanted))?;
        // It doesn't wait for its turn on a drive anymore (a look that still does says so
        // again at its next try), nor for a sync on another job's.
        self.drive_wait = None;
        self.waits_on = None;
        if self.confirmed.is_some() {
            return None;
        }
        self.waiting_for.take()
    }

    /// Forgets everything learned about the folders (the job's folders or mode changed).
    pub fn forget_folders(&mut self) {
        self.stop_looking();
        self.forget_review();
        self.look = None;
        self.problem = None;
        self.reads = Reads::default();
        self.no_links = (false, false);
        self.refused.clear();
        self.unread.clear();
        self.filter = None;
        self.missing = Missing::default();
        self.tried = None;
        self.wanted = None;
        self.waiting_for = None;
        self.waits_on = None;
        self.folders_inside.clear();
    }
}

/// A run nobody looks at that is due, until it starts.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    /// Why, for the log: "automatically after changes", "started with Sync all".
    pub reason: String,
    /// The person asked for it (Sync all, or Sync now that found another sync at work): it
    /// still starts while the job or everything is paused, as a trigger's run doesn't.
    pub by_hand: bool,
}

impl Wanted {
    /// Whether `now`, the job's wanted run as it is now, still asks for what this one (read
    /// earlier) asked for: a run the person asked for, or a trigger's run, which pausing
    /// holds back.
    pub fn asks_as(&self, now: &Wanted) -> bool {
        self.by_hand == now.by_hand
    }
}

/// A list the person confirmed whose sync waits for another sync to finish.
#[derive(Debug, Clone)]
pub(crate) struct Confirmed {
    pub review_id: String,
    /// The person ticked "I've looked at the removals and replacements".
    pub acknowledged: bool,
    /// How many times everything had been paused when the person confirmed it (see
    /// `Engine::pauses`): pausing everything since calls it off.
    pub pauses: u64,
}

/// The items a sync writes, by key: its plan's steps, and the items its steps make under new
/// names (conflict copies), each from right before it's made. Changes the folder watcher hears
/// about anywhere else while the sync runs are the person's (or another app's), and are looked
/// at once it's done. Until the sync knows its plan (it's still looking), nothing it could
/// write is known, and it hasn't written anything: every change is someone else's.
#[derive(Debug, Default)]
pub(crate) struct OwnWrites {
    /// The sync's items and every folder above them (a folder changes when an item in it does).
    keys: HashSet<String>,
    /// The keys ignore upper and lower case (see `Keys::fold`).
    folded: bool,
    /// Keys outside the sync's items that changed while it ran (a few are enough).
    others: BTreeSet<String>,
    /// Something changed before the sync knew its plan.
    changed_before: bool,
}

impl OwnWrites {
    /// The most changes outside the sync's items kept until it's done.
    const KEPT: usize = 100;

    /// The writes of a sync carrying out `plan`, which began before as `before` (with no
    /// plan yet).
    pub fn of(plan: &Plan, folded: bool, before: Option<&OwnWrites>) -> Self {
        let mut own = Self { folded, changed_before: before.is_some_and(OwnWrites::others_changed), ..Self::default() };
        own.add(plan.actions.iter().map(Action::path));
        own
    }

    /// Adds items the sync writes (plan keys, in the plan's form).
    pub fn add<'a>(&mut self, items: impl Iterator<Item = &'a str>) {
        for key in items {
            self.keys.extend(Keys::ancestors(key).map(str::to_string));
            self.keys.insert(key.to_string());
        }
        let keys = &self.keys;
        self.others.retain(|key| !keys.contains(key));
    }

    /// Whether a change at `key` (as spelled on disk) may be the sync's own.
    pub fn is_own(&self, key: &str) -> bool {
        self.keys.contains(&self.form(key))
    }

    /// Notes a change outside the sync's items.
    pub fn note_other(&mut self, key: &str) {
        if self.others.len() < Self::KEPT {
            self.others.insert(self.form(key));
        }
    }

    /// Whether anything outside the sync's items changed while it ran.
    pub fn others_changed(&self) -> bool {
        self.changed_before || !self.others.is_empty()
    }

    /// `key` in the form of the plan's keys.
    fn form(&self, key: &str) -> String {
        if self.folded {
            Keys::fold(key)
        } else {
            key.to_string()
        }
    }
}

/// Pairs of files compared byte for byte in this session: key → both items as compared, and
/// whether they were identical. A verdict is reused while neither file changes and both
/// folders carry the markers they carried when it was read (see `run::Marker`): another
/// drive under the same name, or a folder restored from an older copy, may hold other files
/// that look the same. Folders without a marker Charon can read (before the job's first
/// sync) keep none: nothing tells them from any other folder under the same name.
#[derive(Debug, Default)]
pub(crate) struct Reads {
    /// The markers both folders carried when the pairs were read: a readable marker on each side.
    markers: Option<Markers>,
    pairs: HashMap<String, (Entry, Entry, bool)>,
}

impl Reads {
    /// What the read of the pair at `key` found, while its items are still `source` and
    /// `destination` and both folders carry `markers` (as a look found them before its scans).
    pub fn verdict(&self, key: &str, source: &Entry, destination: &Entry, markers: &Markers) -> Option<bool> {
        if !Self::marked(markers) || self.markers.as_ref() != Some(markers) {
            return None;
        }
        self.pairs.get(key).filter(|(s, d, _)| s == source && d == destination).map(|(_, _, same)| *same)
    }

    /// Notes what the read of the pair at `key` found in folders that carry `markers`. What
    /// was read while they carried other markers goes, and nothing is kept while a folder
    /// carries no marker Charon can read.
    pub fn note(&mut self, key: &str, source: Entry, destination: Entry, same: bool, markers: &Markers) {
        if !Self::marked(markers) {
            *self = Self::default();
            return;
        }
        if self.markers.as_ref() != Some(markers) {
            self.pairs.clear();
            self.markers = Some(markers.clone());
        }
        self.pairs.insert(key.to_string(), (source, destination, same));
    }

    /// How many pairs' reads are kept.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether the read of the pair at `key` is kept.
    #[cfg(test)]
    pub fn has(&self, key: &str) -> bool {
        self.pairs.contains_key(key)
    }

    /// Forgets what was read in folders that carried `markers`: one of them was replaced
    /// while a look read it, and may have been read in part.
    pub fn forget(&mut self, markers: &Markers) {
        if self.markers.as_ref() == Some(markers) {
            *self = Self::default();
        }
    }

    /// Forgets the reads of the pairs at `keys` in folders that carried `markers`: a look
    /// that read them couldn't check its folders' markers at its end.
    pub fn forget_pairs(&mut self, keys: &[String], markers: &Markers) {
        if self.markers.as_ref() == Some(markers) {
            for key in keys {
                self.pairs.remove(key);
            }
        }
    }

    /// A sync of Charon's own moved both folders' markers on from `before` to `after`, and
    /// nothing replaced the folders meanwhile: what was read in them still holds.
    pub fn carry(&mut self, before: &Markers, after: Markers) {
        if self.markers.as_ref() == Some(before) {
            self.markers = Some(after);
        }
    }

    /// Whether both folders carry a marker Charon can read: only then can what was read in
    /// them be told from what other folders under the same names hold.
    fn marked(markers: &Markers) -> bool {
        matches!((&markers.source, &markers.destination), (Found::Marker(_), Found::Marker(_)))
    }
}

/// A moment by two clocks: the one on the wall, which is kept across launches but can be
/// set forward or back (by the person, or by the system when it finds the clock wrong), and
/// Charon's own, which only ever moves forward while Charon runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Moment {
    pub wall: DateTime<Utc>,
    pub own: Instant,
}

impl Moment {
    pub fn now() -> Self {
        Self { wall: Utc::now(), own: Instant::now() }
    }
}

/// A job's last sync that Charon found dated later than now: the clock on the wall was ahead
/// when the job synced, or is behind now. Which of the two can't be told, so the date stays as
/// Charon keeps it, and Charon counts for how long it has known so, by its own clock, until the
/// job syncs again. The sync was made before Charon found its date later than now, so it was
/// made at least that long ago, whatever the clock on the wall did since. The count is kept
/// with the job (`model::DatedAhead`), and the next launch counts on from there: the time while
/// Charon wasn't running doesn't count, as its own clock can't tell how long that was.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FoundAhead {
    /// The last sync's date, as Charon keeps it.
    pub at: DateTime<Utc>,
    /// Since when Charon has counted, by its own clock: when it found the date later than now,
    /// or, for a date found so before Charon started, when Charon started.
    counted_from: Instant,
    /// How long Charon had known so before `counted_from`: nothing for a date found since
    /// Charon started, and what the launches before had counted for one kept from before.
    before: Duration,
}

impl FoundAhead {
    /// A last sync dated `at`, found later than now at `now`.
    pub fn found(at: DateTime<Utc>, now: Instant) -> Self {
        Self { at, counted_from: now, before: Duration::ZERO }
    }

    /// The one kept with `job` before Charon started, at `now`, if it's still about the job's
    /// last sync: its count goes on from what the launches before had counted.
    pub fn restored(job: &Job, now: Instant) -> Option<Self> {
        let kept = job.dated_ahead.as_ref().filter(|kept| job.last_synced == Some(kept.last_synced))?;
        Some(Self { at: kept.last_synced, counted_from: now, before: Duration::from_secs(kept.counted_seconds) })
    }

    /// For how long Charon has known so at `now`, by its own clock.
    pub fn known_for(&self, now: Instant) -> Duration {
        self.before.saturating_add(now.saturating_duration_since(self.counted_from))
    }

    /// What's kept with the job for the next launch, at `now`.
    pub fn kept(&self, now: Instant) -> DatedAhead {
        DatedAhead { last_synced: self.at, counted_seconds: self.known_for(now).as_secs() }
    }

    /// When the job synced at the latest, as seen at `now`: the date Charon keeps, or when
    /// Charon found that date later than now (as long before `now` by the clock on the wall as
    /// Charon has known so by its own), whichever is earlier. Once the clock on the wall is
    /// right, the sync was made no later than either, so the earlier one is the closer to it:
    /// the date kept if the clock was right at the sync, when Charon found it so if the clock
    /// was ahead then.
    pub fn latest(&self, now: Moment) -> DateTime<Utc> {
        let found = chrono::Duration::from_std(self.known_for(now.own)).ok().and_then(|known| now.wall.checked_sub_signed(known));
        found.map_or(self.at, |found| found.min(self.at))
    }
}

/// Items missing on a drive that may show changes late (a network share, a cloud drive),
/// and for how long looks have found them missing. Such a drive can list a folder before it
/// has loaded, or from an old picture of it, so a run nobody looks at removes what follows
/// from an item's absence only once it has been missing for a while (`MISSING_FOR`). A list
/// the person confirms shows every removal and isn't held back.
///
/// The wait is counted by Charon's own clock only, so a clock on the wall set forward or back
/// never shortens it. How long looks have found each item missing is kept across launches (see
/// `store::Absences`), and the next launch counts on from there: the time while Charon wasn't
/// running doesn't count, as its own clock can't tell how long that was. Whenever Charon
/// begins to look at the folders again (it started, a folder went away and came back, the
/// computer slept, the clock jumped), a share may list a folder before it has loaded again, so
/// an item found missing before then also has to be found missing by the looks since then for
/// a while (`MISSING_AGAIN_FOR`).
///
/// A database's files only go together (see `plan::Unit`): the other changes to a database
/// one of whose files waits here wait with it.
#[derive(Debug, Default)]
pub(crate) struct Missing {
    noted: HashMap<(Side, String), Noted>,
    /// The databases with a file noted here, each with how many other steps of the last
    /// look's plan change it.
    databases: Vec<(Unit, u64)>,
}

/// One item of `Missing`.
#[derive(Debug, Clone, Copy)]
struct Noted {
    /// When a look first found it missing, by the clock on the wall then: kept with the record
    /// for whoever reads it, never to tell how long the item has been missing.
    since: DateTime<Utc>,
    /// Since when Charon has counted how long it is missing, by its own clock: when a look
    /// first found it missing, or, for an item missing before Charon started, when Charon
    /// started.
    counted_from: Instant,
    /// How long looks had found it missing before `counted_from`, by Charon's own clock:
    /// nothing for an item first found missing since Charon started, and what the looks
    /// before had counted for one kept from before.
    before: Duration,
    /// How long looks have found it missing in all, at the last look that did: kept for the
    /// next launch, which counts on from there.
    counted: Duration,
    /// When a look first found it missing since Charon last began to look at the folders,
    /// by Charon's own clock.
    seen: Option<Instant>,
    /// A run nobody looked at left the removal that follows it for later.
    left_for_later: bool,
}

impl Noted {
    /// An item a look found missing for the first time, at `now`.
    fn first(now: Moment) -> Self {
        Self { since: now.wall, counted_from: now.own, before: Duration::ZERO, counted: Duration::ZERO, seen: Some(now.own), left_for_later: false }
    }

    /// An item kept from before Charon started, at `now`: its wait counts on from what the
    /// looks before had counted. The clock on the wall isn't read for it, as it may have been
    /// set forward or back since, so a record an earlier Charon wrote, which only has that
    /// clock's time, starts its wait over.
    fn restored(absence: &Absence, now: Moment) -> Self {
        let before = Duration::from_secs(absence.counted_seconds.unwrap_or_default());
        Self { since: absence.since, counted_from: now.own, before, counted: before, seen: None, left_for_later: absence.left_for_later }
    }

    /// How long it has been missing at `now`, by Charon's own clock.
    fn missing_for(&self, now: Instant) -> Duration {
        self.before + now.saturating_duration_since(self.counted_from)
    }
}

/// How long an item must have been missing before a run nobody looks at follows it.
pub(crate) const MISSING_FOR: Duration = Duration::from_secs(10 * 60);
/// How long the looks since Charon last began to look at the folders (it started, a folder
/// came back, the computer woke up) must have found an item missing that was already missing
/// before.
pub(crate) const MISSING_AGAIN_FOR: Duration = Duration::from_secs(2 * 60);

impl Missing {
    /// The items kept from before Charon started, as Charon starts at `now`.
    pub fn restored(kept: Vec<Absence>, now: Moment) -> Self {
        let noted = kept
            .into_iter()
            .map(|absence| {
                let noted = Noted::restored(&absence, now);
                ((absence.side, absence.key), noted)
            })
            .collect();
        Self { noted, databases: Vec::new() }
    }

    /// The items to keep for after Charon starts again, in a steady order.
    pub fn kept(&self) -> Vec<Absence> {
        let mut kept: Vec<Absence> = self
            .noted
            .iter()
            .map(|((side, key), noted)| Absence {
                side: *side,
                key: key.clone(),
                since: noted.since,
                counted_seconds: Some(noted.counted.as_secs()),
                left_for_later: noted.left_for_later,
            })
            .collect();
        kept.sort_by(|a, b| (a.side, &a.key).cmp(&(b.side, &b.key)));
        kept
    }

    /// Notes the items whose absence `plan` follows with a removal on the other side, where
    /// they are missing on a drive that may show changes late, as found by a look at `now`.
    /// Every other item is forgotten: one that came back starts afresh if it goes missing
    /// again.
    pub fn note(&mut self, plan: &Plan, source: &Snapshot, destination: &Snapshot, now: Moment) {
        let found: HashSet<(Side, String)> = plan.actions.iter().filter_map(|action| Self::absence(action, source, destination)).collect();
        self.noted.retain(|item, _| found.contains(item));
        for item in &found {
            let noted = self.noted.entry(item.clone()).or_insert_with(|| Noted::first(now));
            noted.seen.get_or_insert(now.own);
            noted.counted = noted.missing_for(now.own);
        }
        self.databases = Vec::new();
        for (_, key) in &found {
            if let Some(database) = Self::database_of(key, source, destination) {
                if !self.databases.iter().any(|(known, _)| *known == database) {
                    let along = plan.actions.iter().filter(|action| Self::goes_with(action, &database, &found, source, destination)).count();
                    self.databases.push((database, along as u64));
                }
            }
        }
    }

    /// Charon begins to look at the job's folders again (a folder went away and came back,
    /// the computer slept, the clock jumped): what looks found missing before has to be
    /// found missing again by the looks from now on, for a while, as after a launch. How
    /// long each item has been missing in all is still counted from when it was.
    pub fn look_again(&mut self) {
        for noted in self.noted.values_mut() {
            noted.seen = None;
        }
    }

    /// Takes out of `plan` the removals that follow an item missing on such a drive for less
    /// than `MISSING_FOR` (or not noted at all), and returns them. They are left for later:
    /// a later run carries them out if the item is still missing then (`take_follow_up`).
    /// A database's files only go together, so every other change to a database one of whose
    /// removals waits is left for later with it.
    pub fn hold_back_recent(&mut self, plan: &mut Plan, source: &Snapshot, destination: &Snapshot, now: Moment) -> Vec<Action> {
        for noted in self.noted.values_mut() {
            noted.left_for_later = false;
        }
        let (carried, mut held): (Vec<Action>, Vec<Action>) =
            std::mem::take(&mut plan.actions).into_iter().partition(|action| match Self::absence(action, source, destination) {
                Some(item) => match self.noted.get_mut(&item) {
                    Some(noted) if Self::lasted(noted, now) => true,
                    Some(noted) => {
                        noted.left_for_later = true;
                        false
                    }
                    None => false,
                },
                None => true,
            });
        let waiting: Vec<Unit> = held.iter().filter_map(|action| Self::database_of(action.path(), source, destination)).collect();
        let (carried, along): (Vec<Action>, Vec<Action>) = carried
            .into_iter()
            .partition(|action| matches!(action, Action::Conflict { .. }) || !waiting.iter().any(|database| database.holds(action.path())));
        held.extend(along);
        plan.actions = carried;
        held
    }

    /// How many steps of the last look's plan a run nobody looks at would still leave at
    /// `now`: the noted removals that haven't waited long enough, and every other change to
    /// a database one of them belongs to.
    pub fn waiting(&self, now: Moment) -> u64 {
        let waits = |database: &Unit| self.noted.iter().any(|((_, key), noted)| database.holds(key) && !Self::lasted(noted, now));
        let whole: Vec<&(Unit, u64)> = self.databases.iter().filter(|(database, _)| waits(database)).collect();
        let items = self
            .noted
            .iter()
            .filter(|((_, key), noted)| !Self::lasted(noted, now) || whole.iter().any(|(database, _)| database.holds(key)))
            .count() as u64;
        items + whole.iter().map(|(_, steps)| steps).sum::<u64>()
    }

    /// Whether a removal a run left for later can be carried out at `now`.
    pub fn follow_up_due(&self, now: Moment) -> bool {
        self.noted.values().any(|noted| noted.left_for_later && Self::lasted(noted, now))
    }

    /// `follow_up_due`, after which such removals are no longer marked: the run that
    /// carries them marks what it leaves again, and a run that ends before (a folder out of
    /// reach) doesn't start again every second.
    pub fn take_follow_up(&mut self, now: Moment) -> bool {
        let due = self.follow_up_due(now);
        if due {
            for noted in self.noted.values_mut() {
                noted.left_for_later = false;
            }
        }
        due
    }

    /// Whether an item has been missing long enough at `now` for a run nobody looks at: for
    /// `MISSING_FOR` in all, counted by Charon's own clock since it was first found missing
    /// (or since Charon started, on from what the looks before had counted), and for
    /// `MISSING_AGAIN_FOR` by the looks since Charon last began to look at the folders. A
    /// clock on the wall set forward or back, before or after a launch, never makes it so.
    fn lasted(noted: &Noted, now: Moment) -> bool {
        let Some(seen) = noted.seen else { return false };
        now.own.saturating_duration_since(seen) >= MISSING_AGAIN_FOR && noted.missing_for(now.own) >= MISSING_FOR
    }

    /// The item whose absence `action` follows, if it's a removal that does and the item is
    /// missing on a drive that may show changes late.
    fn absence(action: &Action, source: &Snapshot, destination: &Snapshot) -> Option<(Side, String)> {
        let Action::Delete { side, path, other: None, .. } = action else { return None };
        let missing_on = side.opposite();
        let snapshot = match missing_on {
            Side::Source => source,
            Side::Destination => destination,
        };
        snapshot.volume().is_some_and(Volume::shows_changes_late).then(|| (missing_on, path.clone()))
    }

    /// The database `key` is a file of, on either side.
    fn database_of(key: &str, source: &Snapshot, destination: &Snapshot) -> Option<Unit> {
        Unit::database_of(key, source, destination).map(Unit::Database)
    }

    /// Whether `action` is another change to `database` than the removals that follow its
    /// noted files (`found`): one that waits with them.
    fn goes_with(action: &Action, database: &Unit, found: &HashSet<(Side, String)>, source: &Snapshot, destination: &Snapshot) -> bool {
        let noted = Self::absence(action, source, destination).is_some_and(|item| found.contains(&item));
        !noted && !matches!(action, Action::Conflict { .. }) && database.holds(action.path())
    }
}

/// What a look at both folders found (a check, a review, or a run's scan).
#[derive(Debug, Clone)]
pub(crate) struct Look {
    /// The job's setup the look was made with.
    pub setup: JobSetup,
    pub at: DateTime<Utc>,
    /// Steps that change something.
    pub items: u64,
    pub bytes: u64,
    pub removals: PerSide,
    pub replacements: PerSide,
    pub conflicts: Vec<ConflictItem>,
    /// At most `PLACES_SHOWN` of each; the counts count them all.
    pub unreadable: Vec<Place>,
    pub unreadable_count: u64,
    pub not_synced: Vec<Place>,
    pub not_synced_count: u64,
    pub volumes: Option<(VolumeLabel, VolumeLabel)>,
    /// Files a background look couldn't compare by their contents, and knows nothing about:
    /// they may differ, so the job isn't in sync while there are any. Not among `items`: a
    /// list reads them, and so does every run.
    pub uncompared: u64,
}

impl Look {
    /// Whether the look found anything waiting for the person: changes or conflicts.
    pub fn has_work(&self) -> bool {
        self.items > 0 || !self.conflicts.is_empty()
    }

    /// Whether the look found anything for a list to show or settle: changes, conflicts, or
    /// files it couldn't compare yet.
    pub fn needs_a_list(&self) -> bool {
        self.has_work() || self.uncompared > 0
    }
}

/// When the person was last told that changes wait for their OK.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Reminder {
    /// The last notification about this job (kept when the changes are confirmed, so a new
    /// wait right after doesn't notify again at once). By the clock on the wall: the time
    /// a computer sleeps counts too, so "daily" means daily.
    pub last: Option<DateTime<Utc>>,
    /// Notifications sent since changes began to wait.
    pub count: u32,
}

#[cfg(test)]
impl Moment {
    /// The moment `by` after this one, by both clocks.
    pub fn after(self, by: Duration) -> Moment {
        Moment { wall: self.wall + chrono::Duration::from_std(by).unwrap(), own: self.own + by }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::FsKind;

    /// A folder holding `paths`, on a network share or on the computer's own drive.
    fn folder(paths: &[&str], share: bool) -> Snapshot {
        let mut snapshot: Snapshot = paths.iter().map(|path| (path.to_string(), Entry::file(1, 1))).collect();
        let kind = if share { FsKind::Smb } else { FsKind::Ext };
        snapshot.facts_mut().volume =
            Some(Volume { kind, fs: "x".into(), mount: "/x".into(), name: "Drive".into(), network: share, case_sensitive: true, case_guessed: false, refuses_links: false });
        snapshot
    }

    /// A Mirror's plan: the destination follows the source, which no longer has `path`.
    fn removing(path: &str) -> Plan {
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, path, Entry::file(1, 1), None));
        plan
    }

    /// How many steps of `removing(path)` a run nobody looks at would carry out at `now`.
    fn carried(missing: &mut Missing, path: &str, source: &Snapshot, destination: &Snapshot, now: Moment) -> usize {
        let mut plan = removing(path);
        missing.hold_back_recent(&mut plan, source, destination, now);
        plan.actions.len()
    }

    fn minutes(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    /// `now` with the clock on the wall moved by `by` (forward or back), and Charon's own
    /// clock where it was.
    fn wall_moved(now: Moment, by: chrono::Duration) -> Moment {
        Moment { wall: now.wall + by, own: now.own }
    }

    #[test]
    fn a_removal_that_follows_a_share_waits_until_the_item_has_been_missing_a_while() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let mut missing = Missing::default();
        let start = Moment::now();
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, start), 0, "never noted: it waits");
        missing.note(&removing("a.txt"), &source, &destination, start);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, start), 0);
        // Still missing at a later look: the removal goes ahead.
        let later = start.after(minutes(10));
        missing.note(&removing("a.txt"), &source, &destination, later);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, later), 1);
    }

    #[test]
    fn an_item_that_came_back_starts_afresh() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let mut missing = Missing::default();
        let start = Moment::now();
        missing.note(&removing("a.txt"), &source, &destination, start);
        // A look in between finds it again (nothing to remove).
        missing.note(&Plan::default(), &source, &destination, start.after(minutes(5)));
        let later = start.after(minutes(10));
        missing.note(&removing("a.txt"), &source, &destination, later);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, later), 0);
    }

    #[test]
    fn removals_that_follow_the_computers_own_drive_or_make_room_never_wait() {
        let mut missing = Missing::default();
        let now = Moment::now();
        let (source, destination) = (folder(&[], false), folder(&["a.txt"], false));
        missing.note(&removing("a.txt"), &source, &destination, now);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, now), 1);
        // A folder that became a file on the share: the removal makes room for the file.
        let (source, destination) = (folder(&["x"], true), folder(&["x/inner.txt"], false));
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, "x", Entry::dir(), Some(Entry::file(1, 1))));
        missing.note(&plan, &source, &destination, now);
        assert!(missing.hold_back_recent(&mut plan, &source, &destination, now).is_empty());
    }

    #[test]
    fn an_item_missing_before_charon_started_is_looked_for_again_first() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        // At the last launch, looks found it missing for ten minutes.
        let start = Moment::now();
        let mut before = Missing::default();
        before.note(&removing("a.txt"), &source, &destination, start);
        before.note(&removing("a.txt"), &source, &destination, start.after(minutes(10)));
        let launch = start.after(minutes(60));
        let mut missing = Missing::restored(before.kept(), launch);
        assert_eq!(missing.kept(), before.kept(), "kept as it was");
        assert_eq!(missing.kept()[0].counted_seconds, Some(10 * 60));
        // Missing long enough, but no look since the launch has found it missing yet.
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, launch), 0);
        missing.note(&removing("a.txt"), &source, &destination, launch);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, launch.after(minutes(1))), 0);
        missing.note(&removing("a.txt"), &source, &destination, launch.after(minutes(2)));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, launch.after(minutes(2))), 1);
    }

    #[test]
    fn the_wait_counts_on_after_a_restart_but_never_the_time_charon_was_not_running() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        // Looks found it missing for three minutes; then Charon was quit, and opened again
        // five hours later.
        let start = Moment::now();
        let mut before = Missing::default();
        before.note(&removing("a.txt"), &source, &destination, start);
        before.note(&removing("a.txt"), &source, &destination, start.after(minutes(3)));
        let launch = start.after(minutes(5 * 60));
        let mut missing = Missing::restored(before.kept(), launch);
        missing.note(&removing("a.txt"), &source, &destination, launch);
        let again = launch.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0, "missing for five minutes of looks, not five hours");
        // Seven more minutes since the launch: ten in all.
        let due = launch.after(minutes(7));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    #[test]
    fn a_restart_after_a_clock_set_forward_keeps_the_whole_wait() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        // The computer starts with its clock five hours behind, and a look finds a.txt missing.
        let boot = Moment::now();
        let behind = wall_moved(boot, -chrono::Duration::hours(5));
        let mut missing = Missing::default();
        missing.note(&removing("a.txt"), &source, &destination, behind);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, behind), 0);
        // The system sets the clock right, which the autopilot takes for a gap, and Charon is
        // opened again a minute later.
        missing.look_again();
        let relaunch = boot.after(minutes(1));
        let mut missing = Missing::restored(missing.kept(), relaunch);
        missing.note(&removing("a.txt"), &source, &destination, relaunch);
        let again = relaunch.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert!(!missing.take_follow_up(again), "the follow-up run is due three minutes after a.txt was first found missing");
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0);
        // Ten minutes of looks since then, by Charon's own clock.
        let due = relaunch.after(minutes(10));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    #[test]
    fn a_clock_ahead_at_launch_and_set_right_keeps_the_wait() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        // Looks found a.txt missing for a minute before Charon was quit. It's opened again
        // with the clock two hours ahead, which the system sets right seconds later.
        let real = Moment::now();
        let a_minute = Absence {
            side: Side::Source,
            key: "a.txt".into(),
            since: real.wall - chrono::Duration::minutes(1),
            counted_seconds: Some(60),
            left_for_later: true,
        };
        let mut missing = Missing::restored(vec![a_minute], wall_moved(real, chrono::Duration::hours(2)));
        let first_look = real.after(Duration::from_secs(10));
        missing.note(&removing("a.txt"), &source, &destination, first_look);
        let again = first_look.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert!(!missing.take_follow_up(again), "the follow-up run is due about three minutes after a.txt was first found missing");
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0);
        // The other nine minutes, by Charon's own clock.
        let almost = real.after(minutes(8));
        missing.note(&removing("a.txt"), &source, &destination, almost);
        assert!(!missing.take_follow_up(almost));
        let due = real.after(minutes(9));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert!(missing.take_follow_up(due));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    #[test]
    fn an_item_kept_by_an_earlier_charon_waits_the_whole_while_again() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        // A record without the count, which only says a.txt went missing a day ago by the clock
        // on the wall: that clock may have been set forward since.
        let launch = Moment::now();
        let yesterday = Absence {
            side: Side::Source,
            key: "a.txt".into(),
            since: launch.wall - chrono::Duration::days(1),
            counted_seconds: None,
            left_for_later: true,
        };
        let mut missing = Missing::restored(vec![yesterday], launch);
        missing.note(&removing("a.txt"), &source, &destination, launch);
        let again = launch.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert!(!missing.take_follow_up(again));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0);
        let due = launch.after(MISSING_FOR);
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    #[test]
    fn an_item_missing_before_its_share_went_away_is_looked_for_again_when_it_comes_back() {
        let (source, destination) = (folder(&[], true), folder(&["photos/x.jpg"], false));
        let mut missing = Missing::default();
        let start = Moment::now();
        missing.note(&removing("photos/x.jpg"), &source, &destination, start);
        assert_eq!(carried(&mut missing, "photos/x.jpg", &source, &destination, start), 0);
        // The share goes away five minutes later, and comes back hours later (the time asleep
        // counts on some systems' own clocks too).
        missing.look_again();
        let back = start.after(minutes(5 * 60));
        assert!(!missing.take_follow_up(back), "no look since it came back has found it missing");
        missing.note(&removing("photos/x.jpg"), &source, &destination, back);
        assert_eq!(carried(&mut missing, "photos/x.jpg", &source, &destination, back), 0, "it may list the folder before it has loaded");
        let later = back.after(minutes(1));
        missing.note(&removing("photos/x.jpg"), &source, &destination, later);
        assert_eq!(carried(&mut missing, "photos/x.jpg", &source, &destination, later), 0);
        let due = back.after(minutes(2));
        missing.note(&removing("photos/x.jpg"), &source, &destination, due);
        assert!(missing.take_follow_up(due));
        assert_eq!(carried(&mut missing, "photos/x.jpg", &source, &destination, due), 1);
    }

    #[test]
    fn removals_left_for_later_are_followed_up_once_they_are_due() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let mut missing = Missing::default();
        let start = Moment::now();
        missing.note(&removing("a.txt"), &source, &destination, start);
        assert_eq!(missing.waiting(start), 1);
        assert!(!missing.take_follow_up(start.after(minutes(10))), "no run left anything for later");
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, start), 0);
        assert!(missing.kept()[0].left_for_later);
        assert!(!missing.take_follow_up(start.after(minutes(9))));
        assert!(missing.take_follow_up(start.after(minutes(10))));
        assert_eq!(missing.waiting(start.after(minutes(10))), 0);
        assert!(!missing.take_follow_up(start.after(minutes(11))), "one follow-up, until a run leaves something for later again");
    }

    #[test]
    fn a_clock_set_forward_never_shortens_the_wait() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let mut missing = Missing::default();
        let boot = Moment::now();
        missing.note(&removing("a.txt"), &source, &destination, boot);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, boot), 0);
        // Seconds later the system finds its clock five hours behind, and sets it forward.
        let stepped = wall_moved(boot.after(Duration::from_secs(6)), chrono::Duration::hours(5));
        assert!(!missing.take_follow_up(stepped));
        missing.note(&removing("a.txt"), &source, &destination, stepped);
        assert_eq!(missing.waiting(stepped), 1);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, stepped), 0);
        let due = stepped.after(minutes(10));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);

        // An item found missing for a day before the launch looks again for the whole while too.
        let yesterday = Absence {
            side: Side::Source,
            key: "a.txt".into(),
            since: boot.wall - chrono::Duration::days(1),
            counted_seconds: Some(24 * 60 * 60),
            left_for_later: true,
        };
        let mut missing = Missing::restored(vec![yesterday], boot);
        missing.note(&removing("a.txt"), &source, &destination, boot);
        let stepped = wall_moved(boot.after(Duration::from_secs(5)), chrono::Duration::hours(5));
        assert!(!missing.take_follow_up(stepped));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, stepped), 0);
    }

    #[test]
    fn a_clock_set_forward_never_shortens_the_wait_when_it_counts_as_a_gap() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let ahead = |moment: Moment| wall_moved(moment, chrono::Duration::hours(5));
        let mut missing = Missing::default();
        let boot = Moment::now();
        missing.note(&removing("a.txt"), &source, &destination, boot);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, boot), 0);
        // Seconds later the clock is set five hours forward. The autopilot takes the jump for
        // a time Charon wasn't looking, so the looks from then on find it missing again.
        let stepped = ahead(boot.after(Duration::from_secs(6)));
        missing.look_again();
        missing.note(&removing("a.txt"), &source, &destination, stepped);
        let again = stepped.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert!(!missing.take_follow_up(again));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0, "missing for two minutes, not five hours");
        let almost = ahead(boot.after(minutes(9)));
        missing.note(&removing("a.txt"), &source, &destination, almost);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, almost), 0);
        // Ten minutes by Charon's own clock since it was first found missing.
        let due = ahead(boot.after(minutes(10)));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert!(missing.take_follow_up(due));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);

        // An item found missing for a minute before Charon started waits the other nine
        // minutes by Charon's own clock.
        let launch = Moment::now();
        let a_minute_ago = Absence {
            side: Side::Source,
            key: "a.txt".into(),
            since: launch.wall - chrono::Duration::minutes(1),
            counted_seconds: Some(60),
            left_for_later: true,
        };
        let mut missing = Missing::restored(vec![a_minute_ago], launch);
        missing.note(&removing("a.txt"), &source, &destination, launch);
        let stepped = ahead(launch.after(Duration::from_secs(6)));
        missing.look_again();
        missing.note(&removing("a.txt"), &source, &destination, stepped);
        let again = stepped.after(MISSING_AGAIN_FOR);
        missing.note(&removing("a.txt"), &source, &destination, again);
        assert!(!missing.take_follow_up(again));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, again), 0);
        let due = ahead(launch.after(minutes(9)));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert!(missing.take_follow_up(due));
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    #[test]
    fn a_clock_set_back_never_makes_a_removal_due() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let mut missing = Missing::default();
        let start = Moment::now();
        missing.note(&removing("a.txt"), &source, &destination, start);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, wall_moved(start, -chrono::Duration::hours(1))), 0);
    }

    #[test]
    fn an_item_noted_while_the_clock_was_ahead_waits_by_charons_own_clock() {
        let (source, destination) = (folder(&[], true), folder(&["a.txt"], false));
        let now = Moment::now();
        // First found missing at the last launch, while the clock was a day ahead; it has been
        // set right since.
        let ahead = Absence {
            side: Side::Source,
            key: "a.txt".into(),
            since: now.wall + chrono::Duration::days(1),
            counted_seconds: Some(0),
            left_for_later: true,
        };
        let mut missing = Missing::restored(vec![ahead], now);
        missing.note(&removing("a.txt"), &source, &destination, now);
        assert!(missing.kept()[0].left_for_later);
        let almost = now.after(minutes(9));
        missing.note(&removing("a.txt"), &source, &destination, almost);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, almost), 0);
        // The wait ends after the usual while, not once the clock reaches tomorrow.
        let due = now.after(minutes(10));
        missing.note(&removing("a.txt"), &source, &destination, due);
        assert_eq!(carried(&mut missing, "a.txt", &source, &destination, due), 1);
    }

    /// A two-way plan: the share's notes.db was saved again and its journals went (the app
    /// closed), so the computer's copy is replaced and its journals are removed.
    fn database_closed_on_the_share() -> (Plan, Snapshot, Snapshot) {
        let share = folder(&["notes.db"], true);
        let mine = folder(&["notes.db", "notes.db-wal", "notes.db-shm"], false);
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "notes.db", Entry::file(2, 2), Some(Entry::file(1, 1))));
        plan.push(Action::delete(Side::Destination, "notes.db-wal", Entry::file(1, 1), None));
        plan.push(Action::delete(Side::Destination, "notes.db-shm", Entry::file(1, 1), None));
        (plan, share, mine)
    }

    #[test]
    fn a_database_whose_journals_wait_waits_as_a_whole() {
        let (plan, share, mine) = database_closed_on_the_share();
        let mut missing = Missing::default();
        let start = Moment::now();
        missing.note(&plan, &share, &mine, start);
        assert_eq!(missing.waiting(start), 3, "the new database waits with its old journals");
        let mut run = plan.clone();
        let held = missing.hold_back_recent(&mut run, &share, &mine, start);
        assert!(run.actions.is_empty(), "nothing of the database is carried: {:?}", run.actions);
        assert_eq!(held.len(), 3);

        // The journals have been missing long enough: all three go together.
        let due = start.after(minutes(10));
        missing.note(&plan, &share, &mine, due);
        assert_eq!(missing.waiting(due), 0);
        let mut run = plan.clone();
        assert!(missing.hold_back_recent(&mut run, &share, &mine, due).is_empty());
        assert_eq!(run.actions.len(), 3);
    }

    #[test]
    fn a_database_waits_as_long_as_any_of_its_journals_does() {
        let (plan, share, mine) = database_closed_on_the_share();
        let mut missing = Missing::default();
        let start = Moment::now();
        // The -wal went missing first; the -shm five minutes later.
        let mut wal_only = plan.clone();
        wal_only.actions.retain(|action| action.path() != "notes.db-shm");
        missing.note(&wal_only, &share, &mine, start);
        let later = start.after(minutes(5));
        missing.note(&plan, &share, &mine, later);
        let due = start.after(minutes(10));
        missing.note(&plan, &share, &mine, due);
        let mut run = plan.clone();
        let held = missing.hold_back_recent(&mut run, &share, &mine, due);
        assert!(run.actions.is_empty());
        assert_eq!(held.len() as u64, missing.waiting(due), "what waits is what a run leaves for later");
    }

    /// The markers of the job's folders after `generation` syncs.
    fn markers(generation: u64) -> Markers {
        let marker = |side: Side| crate::run::Found::Marker(crate::run::Marker::new(&"job".to_string(), side, "token", generation));
        Markers { source: marker(Side::Source), destination: marker(Side::Destination) }
    }

    /// What this session read holds while both files are as they were read, and only for the
    /// folders that carry the markers they carried then: a folder restored from a copy, or
    /// another drive under the same name, may hold other files that look the same.
    #[test]
    fn what_was_read_holds_only_while_the_folders_carry_the_same_markers() {
        let (s, d) = (Entry::file(4, 10_000), Entry::file(4, 20_000));
        let mut reads = Reads::default();
        reads.note("a.txt", s, d, true, &markers(3));
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(3)), Some(true));
        assert_eq!(reads.verdict("a.txt", &s, &Entry::file(4, 30_000), &markers(3)), None, "a file changed since");
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(2)), None, "restored from a copy");

        // What is read in the restored folder replaces what was read before.
        reads.note("b.txt", s, d, false, &markers(2));
        assert_eq!(reads.verdict("b.txt", &s, &d, &markers(2)), Some(false));
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(3)), None, "read in another folder");

        // A folder replaced while a look read it: what was read while the folders carried the
        // look's markers goes, and only that.
        reads.forget(&markers(3));
        assert_eq!(reads.verdict("b.txt", &s, &d, &markers(2)), Some(false), "read while the folders carried other markers");
        reads.forget(&markers(2));
        assert_eq!(reads.verdict("b.txt", &s, &d, &markers(2)), None);
    }

    /// Folders without a marker Charon can read (before the job's first sync, or with a
    /// damaged marker) can't be told from other folders under the same names: what was read
    /// in them is kept for none, and what was read in other folders before goes.
    #[test]
    fn what_was_read_in_folders_without_a_marker_is_kept_for_none() {
        let (s, d) = (Entry::file(4, 10_000), Entry::file(4, 20_000));
        let damaged = || Found::Unusable("damaged".into());
        for unmarked in [
            Markers { source: Found::Missing, destination: Found::Missing },
            Markers { destination: Found::Missing, ..markers(3) },
            Markers { source: damaged(), destination: damaged() },
        ] {
            let mut reads = Reads::default();
            reads.note("a.txt", s, d, true, &markers(3));
            reads.note("b.txt", s, d, false, &unmarked);
            assert_eq!(reads.verdict("b.txt", &s, &d, &unmarked), None, "{unmarked:?}");
            assert_eq!(reads.verdict("a.txt", &s, &d, &markers(3)), None, "{unmarked:?}: read in other folders before");
        }
    }

    /// A sync of Charon's own moves the folders' markers on: what was read in the same
    /// folders still holds, and isn't read again.
    #[test]
    fn what_was_read_holds_after_a_sync_of_charons_own() {
        let (s, d) = (Entry::file(4, 10_000), Entry::file(4, 20_000));
        let mut reads = Reads::default();
        reads.note("a.txt", s, d, false, &markers(3));
        reads.carry(&markers(2), markers(4));
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(4)), None, "read in folders that carried other markers");
        reads.carry(&markers(3), markers(4));
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(4)), Some(false));
        assert_eq!(reads.verdict("a.txt", &s, &d, &markers(3)), None);
    }
}
