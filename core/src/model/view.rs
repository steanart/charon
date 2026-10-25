//! What the window receives: one row per job, live progress and run summaries.

use crate::model::{Job, JobId};
use crate::plan::{NoteKind, Side, Unit};
use crate::scan::{Entry, EntryKind, Volume, PRIVACY_REFUSAL};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The colour and icon of a job row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    /// Never synced yet.
    New,
    /// Green check: both folders match.
    InSync,
    /// Charon hasn't looked at the folders since it started (or since a folder came back),
    /// and a look is on its way: whether they match isn't known yet.
    Looking,
    /// Amber: changes are waiting to cross, and the job will carry them by itself
    /// (full automation).
    Pending,
    /// Changes wait for the person's OK, or a run nobody looked at was held back for one.
    Review,
    /// Blue, spinning: a sync is running.
    Syncing,
    /// Grey: won't run by itself.
    Paused,
    /// Purple: a file changed on both sides and needs a decision.
    Conflict,
    /// Red: the last run failed or some files couldn't be copied.
    Error,
    /// Dimmed: a folder (or its drive) isn't reachable.
    Offline,
}

/// The stage a running sync is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Scanning,
    Copying,
    Finishing,
}

/// Live progress of a running sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: Phase,
    pub done_items: u64,
    pub total_items: u64,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// The file being carried right now.
    pub current: Option<String>,
}

impl Progress {
    /// Progress at the very start of a run.
    pub fn scanning() -> Self {
        Self {
            phase: Phase::Scanning,
            done_items: 0,
            total_items: 0,
            done_bytes: 0,
            total_bytes: 0,
            current: None,
        }
    }
}

/// Changes waiting to cross, found by the last check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub items: u64,
    pub bytes: u64,
    /// Files the check couldn't compare by their contents yet (they may differ): the next
    /// sync reads them. Not among `items`.
    pub uncompared: u64,
    pub checked_at: DateTime<Utc>,
}

impl Pending {
    /// Whether all that waits to cross is files the last check couldn't compare yet: the next
    /// sync compares them, and may well find nothing to carry.
    pub fn only_to_compare(&self) -> bool {
        self.items == 0 && self.uncompared > 0
    }
}

/// An item that changed on both sides and waits for a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictItem {
    /// The item's key in the plan (see `scan::Keys`).
    pub key: String,
    /// The path as spelled on disk, for the person.
    pub path: String,
    /// What each side has: a file, a folder or a link (`None`: deleted there).
    pub source_kind: Option<EntryKind>,
    pub destination_kind: Option<EntryKind>,
    pub source_modified: Option<i64>,
    pub destination_modified: Option<i64>,
    pub source_size: Option<u64>,
    pub destination_size: Option<u64>,
    /// "an app library or document" or "a database": the item's files only work together,
    /// so one side's copy is kept as a whole (never both).
    #[serde(default)]
    pub whole: Option<String>,
}

impl ConflictItem {
    pub fn new(key: &str, path: &str, source: Option<Entry>, destination: Option<Entry>) -> Self {
        Self {
            key: key.to_string(),
            path: path.to_string(),
            source_kind: source.map(|e| e.kind),
            destination_kind: destination.map(|e| e.kind),
            source_modified: source.map(|e| e.modified),
            destination_modified: destination.map(|e| e.modified),
            source_size: source.map(|e| e.size),
            destination_size: destination.map(|e| e.size),
            whole: None,
        }
    }

    /// The same conflict, about a whole app library or database (see `plan::Unit`).
    pub fn whole(mut self, unit: Option<&Unit>) -> Self {
        self.whole = unit.map(|unit| unit.describe().to_string());
        self
    }
}

/// How the person settles a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictChoice {
    KeepSource,
    KeepDestination,
    KeepBoth,
}

/// The result of one run, stored with the job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub copied: u64,
    pub deleted: u64,
    pub bytes: u64,
    pub conflicts: u64,
    pub failed: u64,
    /// The first items that failed (at most `FAILED_NAMED`), as spelled on disk, so the row
    /// can name them; the log says why.
    #[serde(default)]
    pub failed_items: Vec<String>,
    /// Steps left out because the item changed after the scan (or the review). The next
    /// look picks them up again.
    #[serde(default)]
    pub skipped: u64,
    /// Steps left for the next sync only because the rest of their app library or database
    /// was: another item of it couldn't be synced, or something of it changed meanwhile.
    #[serde(default)]
    pub waited: u64,
    /// Steps left out because a drive turned out not to hold the item (a link, a name, a
    /// path or a size it refused). The next looks list them, with the reason.
    #[serde(default)]
    pub left_out: u64,
    /// Folders kept because something is still in them.
    #[serde(default)]
    pub kept: u64,
    /// Places that couldn't be read, so nothing in them was synced.
    #[serde(default)]
    pub unreadable: u64,
    /// Files this run found it couldn't read when it came to copy them (counted among
    /// `unreadable` too). The looks after it list them, so they are only new here.
    #[serde(default)]
    pub found_unreadable: u64,
    /// Conflicts settled by Newest wins: the older version went to .charon-trash.
    #[serde(default)]
    pub settled: u64,
    /// Files copied without some of their details (Finder tags, a resource fork, extended
    /// attributes), because the receiving drive can't keep them.
    #[serde(default)]
    pub unkept: u64,
    /// Removals left for a later run nobody looks at: the items went missing only a little
    /// while ago on a drive that may show changes late (see `engine::Missing`).
    #[serde(default)]
    pub left_for_later: u64,
    pub cancelled: bool,
    /// Why the whole run failed, if it did.
    pub error: Option<String>,
    /// The person confirmed this run's list (rather than a run by itself).
    #[serde(default)]
    pub approved: bool,
}

impl RunSummary {
    /// An empty summary of a run that started at `started_at`.
    pub fn starting(started_at: DateTime<Utc>, approved: bool) -> Self {
        Self {
            started_at,
            finished_at: started_at,
            copied: 0,
            deleted: 0,
            bytes: 0,
            conflicts: 0,
            failed: 0,
            failed_items: Vec::new(),
            skipped: 0,
            waited: 0,
            left_out: 0,
            kept: 0,
            unreadable: 0,
            found_unreadable: 0,
            settled: 0,
            unkept: 0,
            left_for_later: 0,
            cancelled: false,
            error: None,
            approved,
        }
    }

    /// Whether the run finished without a problem. Places that couldn't be read are not a
    /// problem of the run (nothing there was touched); the row shows them on their own.
    pub fn is_clean(&self) -> bool {
        self.error.is_none() && self.failed == 0 && !self.cancelled
    }

    /// What went wrong in this run, in words for the row: why the whole run failed, or which
    /// items couldn't be synced. `None` when nothing did.
    pub fn problem(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if self.failed == 0 {
            return None;
        }
        let names: Vec<String> = self.failed_items.iter().map(|path| format!("“{path}”")).collect();
        let more = self.failed.saturating_sub(names.len() as u64);
        let which = match names.as_slice() {
            [] => return Some(format!("{} couldn't be synced. The job's Activity says which, and why.", plural(self.failed, "item"))),
            [one] if more == 0 => one.clone(),
            [first, second] if more == 0 => format!("{first} and {second}"),
            _ => format!("{} and {} more", names.join(", "), human_count(more)),
        };
        Some(format!("{} couldn't be synced: {which}. The job's Activity says why.", plural(self.failed, "item")))
    }

    /// One line for the log and notifications: "copied 12 files (8.2 MB), deleted 1".
    pub fn describe(&self) -> String {
        if let Some(error) = &self.error {
            return format!("failed: {error}");
        }
        if self.cancelled {
            return "stopped".into();
        }
        let mut parts = Vec::new();
        if self.copied > 0 {
            parts.push(format!(
                "copied {} ({})",
                plural(self.copied, "file"),
                human_bytes(self.bytes)
            ));
        }
        if self.deleted > 0 {
            parts.push(format!("removed {}", plural(self.deleted, "item")));
        }
        if self.settled > 0 {
            parts.push(format!("{} settled by Newest wins", plural(self.settled, "conflict")));
        }
        if self.conflicts > 0 {
            parts.push(format!("{} to resolve", plural(self.conflicts, "conflict")));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", plural(self.failed, "file")));
        }
        if self.skipped > 0 {
            parts.push(format!("{} skipped (changed meanwhile)", plural(self.skipped, "item")));
        }
        if self.waited > 0 {
            let their = if self.waited == 1 { "its" } else { "their" };
            parts.push(format!("{} left for the next sync with the rest of {their} app library or database", plural(self.waited, "item")));
        }
        if self.left_out > 0 {
            let them = if self.left_out == 1 { "it" } else { "them" };
            parts.push(format!("{} left out (a drive couldn't hold {them})", plural(self.left_out, "item")));
        }
        if self.kept > 0 {
            parts.push(format!("kept {} that still hold files", plural(self.kept, "folder")));
        }
        if self.unreadable > 0 {
            parts.push(format!("{} couldn't be read", plural(self.unreadable, "place")));
        }
        if self.unkept > 0 {
            let their = if self.unkept == 1 { "its" } else { "their" };
            parts.push(format!("{} arrived without {their} Finder tags or other details", plural(self.unkept, "file")));
        }
        if self.left_for_later > 0 {
            parts.push(format!(
                "{} left for a later sync (missing for only a little while on a drive that may show changes late)",
                plural(self.left_for_later, "removal")
            ));
        }
        if parts.is_empty() {
            "already in sync".into()
        } else {
            parts.join(", ")
        }
    }
}

/// A number for each of the two folders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerSide {
    pub source: u64,
    pub destination: u64,
}

impl PerSide {
    pub fn on(&self, side: Side) -> u64 {
        match side {
            Side::Source => self.source,
            Side::Destination => self.destination,
        }
    }

    pub fn add(&mut self, side: Side, n: u64) {
        match side {
            Side::Source => self.source += n,
            Side::Destination => self.destination += n,
        }
    }

    pub fn total(&self) -> u64 {
        self.source + self.destination
    }
}

/// Changes that wait for the person's OK. Shown on the row whatever its status is (a paused
/// job, or one with conflicts or errors, can have changes waiting too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub items: u64,
    pub bytes: u64,
    /// Files and links that would be removed on each side.
    pub removals: PerSide,
    /// Files and links that would be replaced on each side.
    pub replacements: PerSide,
    pub conflicts: u64,
    /// Why a run nobody looked at was held back for a look, if one was.
    pub held: Option<String>,
    /// Files the last look couldn't compare by their contents yet (they may differ): the list
    /// reads them. Not among `items`.
    pub uncompared: u64,
    /// Since when changes have been waiting.
    pub since: DateTime<Utc>,
    /// False when the numbers above aren't known yet: changes waited for an OK when Charon
    /// last looked, before it was closed (or before a folder went away), and it hasn't
    /// looked again since.
    pub counted: bool,
}

impl Waiting {
    /// Whether all that waits is files the last look couldn't compare yet: the list compares
    /// them, and may well find nothing to confirm.
    pub fn only_to_compare(&self) -> bool {
        self.held.is_none() && self.items == 0 && self.conflicts == 0 && self.uncompared > 0
    }
}

/// A place the last look couldn't read or had to leave out: nothing there is synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    /// Unreadable, left out (the other drive can't hold it, a special file), a name clash,
    /// or blocked (a file meeting a folder).
    pub kind: NoteKind,
    pub side: Side,
    /// As spelled on disk, relative to the job's folder.
    pub path: String,
    pub reason: String,
    /// macOS refused to let Charon read it (Privacy & Security settings).
    pub privacy: bool,
}

impl Place {
    /// A place of `kind` on `side`, and whether macOS's privacy settings keep Charon out.
    pub fn new(kind: NoteKind, side: Side, path: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self { kind, side, path: path.into(), privacy: reason == PRIVACY_REFUSAL, reason }
    }
}

/// The most places a row lists (unreadable, and not synced); the rest are counted.
pub const PLACES_SHOWN: usize = 100;

/// The failed items a run's summary names (the others are counted).
pub const FAILED_NAMED: usize = 2;

/// A drive, the way the person knows it: "Backup SSD", "exFAT".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeLabel {
    pub name: String,
    pub kind: String,
}

impl VolumeLabel {
    pub fn of(volume: &Volume) -> Self {
        Self { name: volume.name.clone(), kind: volume.kind_label() }
    }
}

/// The row the window shows for one job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    #[serde(flatten)]
    pub job: Job,
    /// Grows with every view the engine makes; the window ignores a view older than the one
    /// it has.
    pub rev: u64,
    pub status: JobStatus,
    /// Full automation is on and nothing waits for a look first.
    pub runs_by_itself: bool,
    /// Charon is looking at the folders right now (a background look, or a list being
    /// made); what the row says may change when it's done.
    pub looking: bool,
    pub waiting: Option<Waiting>,
    /// Changes the job will carry by itself (full automation).
    pub pending: Option<Pending>,
    pub progress: Option<Progress>,
    pub conflicts: Vec<ConflictItem>,
    /// Places the last look couldn't read (at most `PLACES_SHOWN`; `unreadable_count`
    /// counts them all).
    pub unreadable: Vec<Place>,
    pub unreadable_count: u64,
    /// Items the last look had to leave out, name clashes and blocked items: they aren't
    /// synced (at most `PLACES_SHOWN`; `not_synced_count` counts them all).
    pub not_synced: Vec<Place>,
    pub not_synced_count: u64,
    /// What's wrong, in words, when the status is Error or Offline.
    pub problem: Option<String>,
    pub source_online: bool,
    pub destination_online: bool,
    pub source_volume: Option<VolumeLabel>,
    pub destination_volume: Option<VolumeLabel>,
    /// When the schedule next comes due: a sync if the job runs by itself, else a look.
    pub next_run: Option<DateTime<Utc>>,
    /// "Not synced for 3 days", "Changes have waited for your OK since Monday".
    pub stale: Option<String>,
    /// A sync is due but waits for this other sync of the same folders: another job's
    /// name, "another Charon", or "Charon on <computer>".
    pub waiting_for: Option<String>,
    /// The person confirmed the job's list, and its sync starts as soon as `waiting_for` is
    /// done (Stop calls it off).
    pub confirmed: bool,
}

impl JobView {
    /// Whether the job needs the person: an error or a conflict, whatever its row shows
    /// first. A paused job with a conflict, or whose last run failed, needs them too. A job
    /// out of reach or syncing has nothing to fix yet (its problem only says why).
    pub fn needs_you(&self) -> bool {
        let failed = self.problem.is_some() || self.job.last_run.as_ref().is_some_and(|run| run.error.is_some());
        matches!(self.status, JobStatus::Error | JobStatus::Conflict)
            || !self.conflicts.is_empty()
            || (failed && !matches!(self.status, JobStatus::Offline | JobStatus::Syncing))
    }

    /// Whether all the job has queued is files its last look couldn't compare yet, and its
    /// next sync compares them: it syncs by itself, and no list the person confirmed waits to
    /// start (that sync carries out exactly the list).
    pub fn compares_at_next_sync(&self) -> bool {
        self.status == JobStatus::Pending && !self.confirmed && self.pending.as_ref().is_some_and(Pending::only_to_compare)
    }
}

/// One line per state for the tray icon and the header.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub jobs: usize,
    pub syncing: usize,
    /// Jobs with changes queued to cross: they sync by themselves, or the person confirmed
    /// their list while another sync used their folders.
    pub pending: usize,
    /// Jobs that sync by themselves with nothing queued but files the last look couldn't
    /// compare yet (see `JobView::compares_at_next_sync`).
    pub pending_compare: usize,
    /// Jobs with changes waiting for the person's OK, or held back for it.
    pub waiting: usize,
    /// Jobs with nothing else waiting for the person but files the last look couldn't compare
    /// yet (see `Waiting::only_to_compare`).
    pub to_compare: usize,
    /// Jobs that need the person (see `JobView::needs_you`).
    pub attention: usize,
    pub offline: usize,
    /// Jobs Charon hasn't looked at since it started or since a folder came back.
    pub looking: usize,
    /// Jobs that sync, but leave places out (unreadable, left out, name clashes).
    pub incomplete: usize,
    pub paused_all: bool,
    /// What keeps Charon from syncing as it should, in a sentence: an older Charon running
    /// at the same time, or its state that can't be saved.
    pub problem: Option<String>,
}

impl Overview {
    /// Counts the statuses of all rows.
    pub fn of(views: &[JobView], paused_all: bool, problem: Option<String>) -> Self {
        let count = |wanted: &[JobStatus]| views.iter().filter(|v| wanted.contains(&v.status)).count();
        Self {
            jobs: views.len(),
            syncing: count(&[JobStatus::Syncing]),
            pending: views.iter().filter(|v| v.status == JobStatus::Pending && !v.compares_at_next_sync()).count(),
            pending_compare: views.iter().filter(|v| v.compares_at_next_sync()).count(),
            waiting: views.iter().filter(|v| v.waiting.as_ref().is_some_and(|w| !w.only_to_compare())).count(),
            to_compare: views.iter().filter(|v| v.waiting.as_ref().is_some_and(Waiting::only_to_compare)).count(),
            attention: views.iter().filter(|v| v.needs_you()).count(),
            offline: count(&[JobStatus::Offline]),
            looking: count(&[JobStatus::Looking]),
            incomplete: views.iter().filter(|v| !v.unreadable.is_empty() || v.not_synced_count > 0).count(),
            paused_all,
            problem,
        }
    }

    /// Jobs whose lists wait for the person: changes to confirm, or files to compare.
    pub fn lists_waiting(&self) -> usize {
        self.waiting + self.to_compare
    }

    /// A short sentence, e.g. "Syncing 2 jobs" or "All in sync".
    pub fn headline(&self) -> String {
        if self.problem.is_some() {
            // Nothing else holds while Charon itself can't work as it should.
            "Charon needs your attention".into()
        } else if self.jobs == 0 {
            "No sync jobs yet".into()
        } else if self.syncing > 0 {
            format!("Syncing {}", plural(self.syncing as u64, "job"))
        } else if let Some(asks) = self.asks() {
            asks
        } else if self.paused_all {
            "Everything paused".into()
        } else if let Some(queued) = self.queued() {
            queued
        } else if self.incomplete > 0 {
            format!("{} {} sync everything", plural(self.incomplete as u64, "job"), if self.incomplete == 1 { "doesn't" } else { "don't" })
        } else if self.looking > 0 {
            "Looking for changes…".into()
        } else if self.offline > 0 {
            // Whether the jobs out of reach are in sync isn't known.
            format!("All reachable jobs in sync · {} offline", human_count(self.offline as u64))
        } else {
            "All in sync".into()
        }
    }

    /// What the jobs that wait for the person wait for, each kind named, so that none is
    /// hidden behind another: "1 job needs attention, 2 wait for your OK, 1 has files to
    /// compare". `None` when no job waits for the person.
    fn asks(&self) -> Option<String> {
        Self::each_named([
            (self.attention, "needs attention", "need attention"),
            (self.waiting, "waits for your OK", "wait for your OK"),
            (self.to_compare, "has files to compare", "have files to compare"),
        ])
    }

    /// What is queued without waiting for the person, each kind named, so that neither is
    /// hidden behind the other: "1 job with changes waiting, 1 with files to compare at its
    /// next sync". `None` when no job has anything queued.
    fn queued(&self) -> Option<String> {
        Self::each_named([
            (self.pending, "with changes waiting", "with changes waiting"),
            (self.pending_compare, "with files to compare at its next sync", "with files to compare at their next sync"),
        ])
    }

    /// The kinds of jobs counted, each with its count and what is said of one job or of more,
    /// in one line: the first one names what is counted. `None` when none is counted.
    fn each_named<const N: usize>(kinds: [(usize, &str, &str); N]) -> Option<String> {
        let named: Vec<String> = kinds
            .into_iter()
            .filter(|(jobs, ..)| *jobs > 0)
            .enumerate()
            .map(|(i, (jobs, one, more))| {
                let count = if i == 0 { plural(jobs as u64, "job") } else { human_count(jobs as u64) };
                format!("{count} {}", if jobs == 1 { one } else { more })
            })
            .collect();
        (!named.is_empty()).then(|| named.join(", "))
    }
}

/// "1 file", "3 files", "1,240 files".
pub fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{} {word}s", human_count(n))
    }
}

/// "1,240".
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// "8.2 MB".
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A notification-worthy event: what the app shell may tell the person about.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finished {
    pub id: JobId,
    pub name: String,
    pub summary: RunSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(count: u64, names: &[&str]) -> RunSummary {
        let mut run = RunSummary::starting(Utc::now(), true);
        run.failed = count;
        run.failed_items = names.iter().map(|name| name.to_string()).collect();
        run
    }

    #[test]
    fn a_run_with_failed_items_names_them() {
        assert_eq!(failed(0, &[]).problem(), None);
        assert_eq!(
            failed(1, &["Outlook Files/me.pst"]).problem().as_deref(),
            Some("1 item couldn't be synced: “Outlook Files/me.pst”. The job's Activity says why.")
        );
        assert_eq!(failed(2, &["a.txt", "b.txt"]).problem().as_deref(), Some("2 items couldn't be synced: “a.txt” and “b.txt”. The job's Activity says why."));
        assert_eq!(
            failed(1_204, &["a.txt", "b.txt"]).problem().as_deref(),
            Some("1,204 items couldn't be synced: “a.txt”, “b.txt” and 1,202 more. The job's Activity says why.")
        );
        // A run recorded before Charon kept the names.
        assert_eq!(failed(3, &[]).problem().as_deref(), Some("3 items couldn't be synced. The job's Activity says which, and why."));
        // Why a whole run failed says it all.
        let mut stopped = failed(1, &["a.txt"]);
        stopped.error = Some("The destination folder went away during the sync.".into());
        assert_eq!(stopped.problem(), stopped.error);
    }

    /// A two-way job's row with `status`, and nothing else to say.
    fn row(status: JobStatus) -> JobView {
        let job = Job::new(crate::model::JobSetup {
            name: "Notes".into(),
            source: "/a".into(),
            destination: "/b".into(),
            mode: crate::model::SyncMode::TwoWay,
            conflict: crate::model::ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        });
        JobView {
            job,
            rev: 1,
            status,
            runs_by_itself: false,
            looking: false,
            waiting: None,
            pending: None,
            progress: None,
            conflicts: Vec::new(),
            unreadable: Vec::new(),
            unreadable_count: 0,
            not_synced: Vec::new(),
            not_synced_count: 0,
            problem: None,
            source_online: true,
            destination_online: true,
            source_volume: None,
            destination_volume: None,
            next_run: None,
            stale: None,
            waiting_for: None,
            confirmed: false,
        }
    }

    #[test]
    fn the_tray_never_says_all_in_sync_while_a_job_is_offline_or_needs_the_person() {
        let headline = |rows: &[JobView], paused_all: bool| Overview::of(rows, paused_all, None).headline();
        let in_sync = row(JobStatus::InSync);
        assert_eq!(headline(std::slice::from_ref(&in_sync), false), "All in sync");

        // A job out of reach: whether it's in sync isn't known, and its reason is no problem to fix.
        let offline = JobView {
            destination_online: false,
            problem: Some("The destination isn't reachable. Is the drive connected?".into()),
            ..row(JobStatus::Offline)
        };
        let overview = Overview::of(&[in_sync.clone(), offline.clone()], false, None);
        assert_eq!((overview.attention, overview.offline), (0, 1));
        assert_eq!(overview.headline(), "All reachable jobs in sync · 1 offline");
        assert_eq!(headline(&[offline.clone(), offline], false), "All reachable jobs in sync · 2 offline");

        // Paused shows first, but a conflict or a failed run still needs the person.
        let conflict = JobView { conflicts: vec![ConflictItem::new("a.txt", "a.txt", None, None)], ..row(JobStatus::Paused) };
        assert_eq!(headline(&[in_sync.clone(), conflict.clone()], false), "1 job needs attention");
        let mut failed = row(JobStatus::Paused);
        failed.job.last_run = Some(RunSummary { failed: 1, failed_items: vec!["me.pst".into()], ..RunSummary::starting(Utc::now(), true) });
        failed.problem = failed.job.last_run.as_ref().and_then(RunSummary::problem);
        assert_eq!(headline(&[in_sync.clone(), failed.clone()], true), "1 job needs attention");
        assert_eq!(headline(&[in_sync.clone(), conflict, failed], true), "2 jobs need attention");
        // Paused with nothing wrong: everything is paused, nothing needs the person.
        assert_eq!(headline(&[in_sync, row(JobStatus::Paused)], true), "Everything paused");
    }

    /// A job whose look left only files it couldn't compare yet doesn't wait for an OK: its
    /// list may well find both copies identical, and have nothing to confirm.
    #[test]
    fn jobs_with_only_files_to_compare_are_not_said_to_wait_for_an_ok() {
        let waiting = |items: u64, conflicts: u64, uncompared: u64, held: Option<&str>| Waiting {
            items,
            bytes: 0,
            removals: PerSide::default(),
            replacements: PerSide::default(),
            conflicts,
            held: held.map(str::to_string),
            uncompared,
            since: Utc::now(),
            counted: true,
        };
        let row_waiting = |waiting: Waiting| JobView { waiting: Some(waiting), ..row(JobStatus::Review) };
        let videos = row_waiting(waiting(0, 0, 1, None));
        let overview = Overview::of(std::slice::from_ref(&videos), false, None);
        assert_eq!((overview.waiting, overview.to_compare), (0, 1));
        assert_eq!(overview.headline(), "1 job has files to compare");
        assert_eq!(Overview::of(&[videos.clone(), videos.clone()], false, None).headline(), "2 jobs have files to compare");

        // Changes, a conflict or a hold wait for an OK, whatever else waits too.
        for other in [waiting(3, 0, 1, None), waiting(0, 1, 1, None), waiting(0, 0, 1, Some("Charon stopped the sync."))] {
            let overview = Overview::of(&[videos.clone(), row_waiting(other)], false, None);
            assert_eq!((overview.waiting, overview.to_compare), (1, 1));
            assert_eq!(overview.headline(), "1 job waits for your OK, 1 has files to compare");
        }
    }

    /// A job that syncs by itself, and whose look left only files it couldn't compare yet, has
    /// no changes waiting: its next sync compares them, and may well find both copies identical.
    #[test]
    fn jobs_that_sync_by_themselves_with_only_files_to_compare_are_not_said_to_have_changes_waiting() {
        let pending = |items: u64, uncompared: u64| Pending { items, bytes: 0, uncompared, checked_at: Utc::now() };
        let row_pending = |pending: Pending| JobView { runs_by_itself: true, pending: Some(pending), ..row(JobStatus::Pending) };
        let videos = row_pending(pending(0, 1));
        let overview = Overview::of(std::slice::from_ref(&videos), false, None);
        assert_eq!((overview.pending, overview.pending_compare), (0, 1));
        assert_eq!(overview.lists_waiting(), 0, "nothing waits for the person");
        assert_eq!(overview.headline(), "1 job with files to compare at its next sync");
        assert_eq!(Overview::of(&[videos.clone(), videos.clone()], false, None).headline(), "2 jobs with files to compare at their next sync");

        // Changes queued as well, or a list the person confirmed that waits to start (that sync
        // carries out exactly the list, and compares nothing else): changes wait to cross.
        let confirmed = JobView { runs_by_itself: false, confirmed: true, ..videos.clone() };
        for other in [row_pending(pending(3, 1)), confirmed] {
            let overview = Overview::of(&[videos.clone(), other], false, None);
            assert_eq!((overview.pending, overview.pending_compare), (1, 1));
            assert_eq!(overview.headline(), "1 job with changes waiting, 1 with files to compare at its next sync");
        }
    }

    #[test]
    fn changes_waiting_for_an_ok_are_never_hidden_behind_jobs_that_need_attention() {
        let overview = Overview { jobs: 4, attention: 1, waiting: 2, ..Default::default() };
        assert_eq!(overview.headline(), "1 job needs attention, 2 wait for your OK");
        assert_eq!(Overview { waiting: 0, ..overview.clone() }.headline(), "1 job needs attention");
        assert_eq!(Overview { attention: 0, ..overview.clone() }.headline(), "2 jobs wait for your OK");
        // Nor are files to compare, which wait for the person too.
        assert_eq!(Overview { waiting: 0, to_compare: 1, ..overview.clone() }.headline(), "1 job needs attention, 1 has files to compare");
        assert_eq!(Overview { jobs: 6, attention: 2, to_compare: 3, ..overview }.headline(), "2 jobs need attention, 2 wait for your OK, 3 have files to compare");
    }
}
