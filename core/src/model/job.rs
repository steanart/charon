//! A sync job: two folders and the rules for carrying files between them.

use crate::error::{Error, Result};
use crate::model::RunSummary;
use crate::scan::{Keys, Volume};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub type JobId = String;

/// How files cross between the two folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    /// One-way: the destination becomes an exact copy, deletions carry over.
    Mirror,
    /// One-way: new and changed files are copied, nothing is ever deleted on the destination.
    Backup,
    /// Changes flow both ways; Charon remembers the last synced state to tell
    /// "deleted here" apart from "new there".
    TwoWay,
}

impl SyncMode {
    pub fn is_two_way(self) -> bool {
        self == Self::TwoWay
    }
}

/// What happens when a file changed on both sides since the last sync (two-way only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictRule {
    /// Keep both: the destination's version is kept next to it as "name (conflict …)".
    KeepBoth,
    /// The most recently modified version wins.
    NewestWins,
    /// Leave the file alone and ask in the window.
    Ask,
}

/// When a job runs by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Trigger {
    /// Only when "Sync now" is pressed.
    Manual,
    /// A few seconds after changes in the folder settle down.
    OnChange { settle_seconds: u32 },
    /// Every N minutes.
    Interval { minutes: u32 },
    /// Once a day at a local time.
    Daily { hour: u8, minute: u8 },
    /// As soon as both folders become reachable (an external drive is plugged in).
    DriveConnected,
}

impl Trigger {
    /// Whether this trigger wants the folders watched for changes.
    pub fn syncs_on_change(self) -> bool {
        matches!(self, Self::OnChange { .. })
    }
}

/// Everything the person chooses when creating or editing a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSetup {
    pub name: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub mode: SyncMode,
    pub conflict: ConflictRule,
    pub trigger: Trigger,
    /// Patterns like `.DS_Store`, `node_modules` or `*.tmp`. Without a `/` a pattern
    /// matches a name anywhere; with one, a path from the folder's root.
    pub excludes: Vec<String>,
    /// Deleted and overwritten files go to a `.charon-trash` folder instead of vanishing.
    pub safe_delete: bool,
    /// Runs by itself without showing the list of changes first. Off by default: then
    /// Charon only prepares the list when the job is due, and waits for the person's OK.
    #[serde(default)]
    pub full_automation: bool,
}

impl JobSetup {
    /// Exclude patterns a new job starts with: system clutter nobody wants copied.
    pub fn default_excludes() -> Vec<String> {
        [
            ".DS_Store",
            "._*",
            ".Spotlight-V100",
            ".Trashes",
            ".fseventsd",
            "Thumbs.db",
            "desktop.ini",
            "$RECYCLE.BIN",
            "~$*",
            "*.tmp",
        ]
        .map(String::from)
        .to_vec()
    }

    /// A name built from the folder names, like "Photos → Backup SSD".
    pub fn suggested_name(source: &Path, destination: &Path, mode: SyncMode) -> String {
        let arrow = if mode.is_two_way() { "⇄" } else { "→" };
        format!("{} {arrow} {}", Self::folder_name(source), Self::folder_name(destination))
    }

    /// Checks that the setup makes sense before it's saved. Asks the disk.
    pub fn validate(&mut self) -> Result<()> {
        self.validate_as(|paths| paths.iter().map(|path| Some(Folder::of(path))).collect())
    }

    /// `validate`, with each folder as `resolve` finds it (see `clashes_as`): one it can't
    /// tell now (`None`) is compared with nothing.
    pub fn validate_as(&mut self, resolve: impl Fn(&[&Path]) -> Vec<Option<Folder>>) -> Result<()> {
        self.name = self.name.trim().to_string();
        self.excludes = self
            .excludes
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();

        if self.source.as_os_str().is_empty() || self.destination.as_os_str().is_empty() {
            return Err(Error::Invalid("Choose both a source and a destination folder.".into()));
        }
        if !self.source.is_absolute() || !self.destination.is_absolute() {
            return Err(Error::Invalid("Folders must be full paths.".into()));
        }
        if let [Some(source), Some(destination)] = resolve(&[&self.source, &self.destination]).as_slice() {
            if source.is(destination) {
                return Err(Error::Invalid("Source and destination are the same folder.".into()));
            }
            if source.overlaps(destination) {
                return Err(Error::Invalid(
                    "One folder is inside the other. Charon would end up copying its own copies.".into(),
                ));
            }
        }
        self.trigger = match self.trigger {
            Trigger::OnChange { settle_seconds } => Trigger::OnChange {
                settle_seconds: settle_seconds.clamp(1, 3600),
            },
            Trigger::Interval { minutes } => Trigger::Interval {
                minutes: minutes.clamp(1, 7 * 24 * 60),
            },
            Trigger::Daily { hour, minute } => Trigger::Daily {
                hour: hour.min(23),
                minute: minute.min(59),
            },
            other => other,
        };
        if self.name.is_empty() {
            self.name = Self::suggested_name(&self.source, &self.destination, self.mode);
        }
        Ok(())
    }

    /// Whether this job and `other` mustn't both exist: a Mirror job removes whatever isn't
    /// in its source, so its destination must not share any folder another job writes into.
    /// (The other job would bring files in, Mirror would remove them, and a two-way job would
    /// carry that removal back to where the files came from.) Asks the disk.
    pub fn clashes_with(&self, other: &JobSetup) -> bool {
        self.clashes_as(other, |paths| paths.iter().map(|path| Some(Folder::of(path))).collect())
    }

    /// `clashes_with`, with each folder as `resolve` finds it, in the order given: `None`
    /// for one it can't tell now (a share that doesn't answer), which is compared with nothing.
    pub fn clashes_as(&self, other: &JobSetup, resolve: impl Fn(&[&Path]) -> Vec<Option<Folder>>) -> bool {
        let clash = |mirror: &JobSetup, writer: &JobSetup| {
            if mirror.mode != SyncMode::Mirror {
                return false;
            }
            let paths: Vec<&Path> = std::iter::once(mirror.destination.as_path()).chain(writer.written_folders()).collect();
            match resolve(&paths).split_first() {
                Some((Some(removes_in), written)) => written.iter().flatten().any(|folder| removes_in.overlaps(folder)),
                _ => false,
            }
        };
        clash(self, other) || clash(other, self)
    }

    /// Why a job with this setup can't be saved next to the job `other_name`, which it
    /// clashes with.
    pub fn clash_message(other_name: &str) -> String {
        format!(
            "This job's folders overlap with those of “{other_name}”, and one of the two is a Mirror job that would remove what the other brings in. Choose folders that don't overlap."
        )
    }

    /// Whether a sync of this job writes into the folder of `side`.
    pub fn writes_to(&self, side: crate::plan::Side) -> bool {
        self.mode.is_two_way() || side == crate::plan::Side::Destination
    }

    /// Whether `other` has the same folders and the same mode.
    pub fn same_folders(&self, other: &JobSetup) -> bool {
        self.source == other.source && self.destination == other.destination && self.mode == other.mode
    }

    /// The folder of one side.
    pub fn root(&self, side: crate::plan::Side) -> &Path {
        match side {
            crate::plan::Side::Source => &self.source,
            crate::plan::Side::Destination => &self.destination,
        }
    }

    /// The folders this job writes into.
    pub fn written_folders(&self) -> Vec<&Path> {
        match self.mode {
            SyncMode::TwoWay => vec![&self.source, &self.destination],
            SyncMode::Mirror | SyncMode::Backup => vec![&self.destination],
        }
    }

    /// Whether a change from `before` to `self` changes what a sync does, so the next sync
    /// must be looked at first.
    pub fn changes_what_syncs(&self, before: &JobSetup) -> bool {
        self.source != before.source
            || self.destination != before.destination
            || self.mode != before.mode
            || self.conflict != before.conflict
            || self.excludes != before.excludes
            || self.safe_delete != before.safe_delete
    }

    fn folder_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }
}

/// A stored sync job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: JobId,
    #[serde(flatten)]
    pub setup: JobSetup,
    /// Paused jobs don't run by themselves ("Sync now" still works).
    pub paused: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<RunSummary>,
    /// When the last *successful* sync finished.
    pub last_synced: Option<DateTime<Utc>>,
    /// The last sync, once Charon found it dated later than now, and for how long Charon has
    /// known so. Kept beside `last_synced`, which stays as it is, until the job syncs again
    /// (see `engine::FoundAhead`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dated_ahead: Option<DatedAhead>,
    /// The next sync must be confirmed by the person, even with full automation: true for a
    /// new job, after its setup changed in a way that changes what a sync does, after a
    /// safety check held a run back, and after the person stopped a run.
    #[serde(default = "Job::yes")]
    pub needs_review: bool,
    /// A sync was carrying files when Charon last stopped. Found set at a later start, it
    /// means that sync never finished (Charon was closed or the computer stopped): the
    /// two-way memory is then not trusted for the next sync.
    #[serde(default)]
    pub run_in_progress: bool,
    /// When a schedule last came due (the job ran or, waiting for an OK, was checked).
    #[serde(default)]
    pub last_scheduled: Option<DateTime<Utc>>,
    /// What automatic runs removed or replaced during the last day (see `plan::Recent`).
    #[serde(default, skip_serializing_if = "AutomaticChanges::is_empty")]
    pub automatic_changes: AutomaticChanges,
    /// Why a run nobody looked at (full automation) was held back for the person's OK, if
    /// one was. Cleared by the next confirmed run.
    #[serde(default)]
    pub held: Option<String>,
    /// The drives both folders were on at the last finished sync. A folder found on another
    /// drive later (an unplugged drive's leftover mount folder, another disk with the same
    /// name) needs a careful look before anything crosses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drives: Option<Drives>,
    /// Mirror and Backup: the token of the marker Charon left in the destination at the last
    /// finished sync (see `run::Marker`). Two-way jobs keep theirs with the two-way memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_token: Option<String>,
    /// Mirror and Backup: the marker Charon left in the source at the last finished sync,
    /// where it could write there. Its counter goes up with every sync, so an older copy
    /// of the source drive (which would empty the newest backups) is told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mark: Option<Mark>,
    /// Since when changes (or conflicts) have waited, while they wait. Kept across restarts,
    /// so a list that waits for days says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<DateTime<Utc>>,
}

/// A job's last sync that Charon found dated later than now (the clock was ahead when the job
/// synced, or behind when Charon found it so), and for how long Charon had known so when this
/// was saved, counted by its own clock: the time while Charon wasn't running doesn't count, as
/// that clock can't tell how long it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatedAhead {
    /// The last sync's date, as `Job::last_synced` keeps it.
    pub last_synced: DateTime<Utc>,
    /// For how long Charon had known it was later than now.
    pub counted_seconds: u64,
}

/// What one of Charon's markers in a folder must say: its token and its counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub token: String,
    pub generation: u64,
}

/// The drives of a job's two folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drives {
    pub source: Volume,
    pub destination: Volume,
}

impl Drives {
    /// The drive of `side`'s folder.
    pub fn side(&self, side: crate::plan::Side) -> &Volume {
        match side {
            crate::plan::Side::Source => &self.source,
            crate::plan::Side::Destination => &self.destination,
        }
    }
}

/// Files one automatic run removed or replaced on each side.
/// The files and links automatic runs removed or replaced during the last day, on each
/// side, with when it last happened to each. A file saved again and again counts once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomaticChanges {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source: BTreeMap<String, DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub destination: BTreeMap<String, DateTime<Utc>>,
}

impl AutomaticChanges {
    /// How many items are kept per side at most: more than the guard's largest count, so a
    /// full window still trips it.
    const KEPT: usize = 1_001;

    pub fn is_empty(&self) -> bool {
        self.source.is_empty() && self.destination.is_empty()
    }

    pub fn clear(&mut self) {
        self.source.clear();
        self.destination.clear();
    }

    /// Records what one automatic run removed or replaced, and forgets what is older than a day.
    fn note(&mut self, now: DateTime<Utc>, source: &[String], destination: &[String]) {
        for (items, changed) in [(&mut self.source, source), (&mut self.destination, destination)] {
            items.retain(|_, at| now - *at < chrono::Duration::hours(24));
            for key in changed.iter().take(Self::KEPT) {
                items.insert(key.clone(), now);
            }
            if items.len() > Self::KEPT {
                // The newest stay.
                let mut by_age: Vec<(DateTime<Utc>, String)> = items.iter().map(|(key, at)| (*at, key.clone())).collect();
                by_age.sort_unstable_by(|a, b| b.cmp(a));
                *items = by_age.into_iter().take(Self::KEPT).map(|(at, key)| (key, at)).collect();
            }
        }
    }

    /// The items changed within the last day, per side.
    fn within_a_day(&self, now: DateTime<Utc>) -> (BTreeSet<String>, BTreeSet<String>) {
        let recent = |items: &BTreeMap<String, DateTime<Utc>>| {
            items.iter().filter(|(_, at)| now - **at < chrono::Duration::hours(24)).map(|(key, _)| key.clone()).collect()
        };
        (recent(&self.source), recent(&self.destination))
    }
}

impl Job {
    /// A new job with a fresh id.
    pub fn new(setup: JobSetup) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            setup,
            paused: false,
            created_at: Utc::now(),
            last_run: None,
            last_synced: None,
            dated_ahead: None,
            needs_review: true,
            run_in_progress: false,
            last_scheduled: None,
            automatic_changes: AutomaticChanges::default(),
            held: None,
            drives: None,
            destination_token: None,
            source_mark: None,
            waiting_since: None,
        }
    }

    fn yes() -> bool {
        true
    }

    /// Whether the job may run by itself, without the person confirming the list first.
    pub fn runs_by_itself(&self) -> bool {
        self.setup.full_automation && !self.needs_review
    }

    /// The time automatic schedules count from at `now`: the last run, the last time the
    /// schedule came due, or the job's creation, whichever is latest. A time later than `now`
    /// was taken while the clock was ahead, and has been set right since: it doesn't count.
    /// None when none of them is earlier than `now`.
    pub fn schedule_reference(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        [Some(self.created_at), self.last_run.as_ref().map(|run| run.finished_at), self.last_scheduled]
            .into_iter()
            .flatten()
            .filter(|time| *time <= now)
            .max()
    }

    /// Files and links removed or replaced by automatic runs within the last day, per side.
    pub fn recent_automatic(&self, now: DateTime<Utc>) -> (BTreeSet<String>, BTreeSet<String>) {
        self.automatic_changes.within_a_day(now)
    }

    /// Records an automatic run's removals and replacements (by path), forgetting those
    /// older than a day.
    pub fn note_automatic(&mut self, now: DateTime<Utc>, source: &[String], destination: &[String]) {
        self.automatic_changes.note(now, source, destination);
    }
}

/// A folder path in a form two paths can be compared in: links and `..` resolved as far as
/// the folders exist (a folder not made yet is placed where it will be: under its nearest
/// folder that exists, resolved), one Unicode form, and upper and lower case ignored where
/// the drive ignores them (always on macOS and Windows, whose drives usually do).
#[derive(Debug, Clone)]
pub struct Folder {
    text: String,
    /// The drive ignores upper and lower case: `text` is in folded form (see `Keys::fold`).
    folded: bool,
}

impl Folder {
    /// Asks the disk: for links, and for how the drive treats upper and lower case.
    pub fn of(path: &Path) -> Self {
        use unicode_normalization::UnicodeNormalization;
        #[cfg(test)]
        asking::note(path);
        let (resolved, existing) = Self::resolve(path);
        let mut text: String = resolved.to_string_lossy().nfc().collect();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            text = rest.to_string();
        }
        let text = text.replace('\\', "/");
        let text = text.trim_end_matches('/').to_string();
        let folded = Self::ignores_case(existing.as_deref());
        Self { text: if folded { Keys::fold(&text) } else { text }, folded }
    }

    /// The same folder.
    pub fn is(&self, other: &Folder) -> bool {
        let (mine, theirs) = self.comparable(other);
        mine == theirs
    }

    /// The same folder, or one inside the other.
    pub fn overlaps(&self, other: &Folder) -> bool {
        let (mine, theirs) = self.comparable(other);
        // The top of a drive ("/" or "C:/") ends up empty, and holds everything.
        mine.is_empty() || theirs.is_empty() || Keys::is_within(&mine, &theirs) || Keys::is_within(&theirs, &mine)
    }

    /// Both texts in a form they can be compared in: upper and lower case are ignored when
    /// either drive ignores them.
    fn comparable<'a>(&'a self, other: &'a Folder) -> (Cow<'a, str>, Cow<'a, str>) {
        let either = self.folded || other.folded;
        let form = |folder: &'a Folder| {
            if either && !folder.folded {
                Cow::Owned(Keys::fold(&folder.text))
            } else {
                Cow::Borrowed(folder.text.as_str())
            }
        };
        (form(self), form(other))
    }

    /// `path` with links and `..` resolved as far as its folders exist, and the nearest of
    /// its folders that exists (`None` when not even the first one does).
    fn resolve(path: &Path) -> (PathBuf, Option<PathBuf>) {
        if let Ok(real) = std::fs::canonicalize(path) {
            return (real.clone(), Some(real));
        }
        let parts: Vec<std::path::Component> = path.components().collect();
        for kept in (1..parts.len()).rev() {
            let existing: PathBuf = parts[..kept].iter().collect();
            if let Ok(real) = std::fs::canonicalize(&existing) {
                let rest: PathBuf = parts[kept..].iter().collect();
                return (Self::lexical(&real.join(rest)), Some(real));
            }
        }
        (Self::lexical(path), None)
    }

    /// Whether the drive of `existing` ignores upper and lower case. macOS and Windows
    /// drives are taken to ignore it; elsewhere the kind of drive tells (FAT, exFAT, NTFS
    /// and SMB shares ignore it on Linux too, and so does a drive of unknown kind, the
    /// careful direction). The drive isn't probed: paths are compared often (before every
    /// sync and look), and probing can mean writing a file of Charon's into the folder.
    fn ignores_case(existing: Option<&Path>) -> bool {
        if cfg!(any(target_os = "macos", windows)) {
            return true;
        }
        existing.is_some_and(|folder| Volume::basics(folder).is_none_or(|volume| !volume.kind.usually_case_sensitive()))
    }

    /// `a/./b/../c` → `a/c`, without asking the disk.
    fn lexical(path: &Path) -> PathBuf {
        use std::path::Component;
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
}

/// What `Folder::of` asked the disk about, for tests that make sure a drive isn't asked.
#[cfg(test)]
pub(crate) mod asking {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        /// The folders `Folder::of` asked the disk about on this thread.
        static ASKED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    /// Notes that `Folder::of` asks the disk about `path` on this thread.
    pub(crate) fn note(path: &Path) {
        ASKED.with(|asked| asked.borrow_mut().push(path.to_path_buf()));
    }

    /// The folders `Folder::of` asked the disk about on this thread since the last call.
    pub(crate) fn taken() -> Vec<PathBuf> {
        ASKED.with(|asked| asked.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(mode: SyncMode, source: &str, destination: &str) -> JobSetup {
        JobSetup {
            name: "t".into(),
            source: source.into(),
            destination: destination.into(),
            mode,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        }
    }

    #[test]
    fn folders_compare_after_resolving_dots() {
        assert!(Folder::of(Path::new("/a/b/../c")).overlaps(&Folder::of(Path::new("/a/c/d"))));
        assert!(!Folder::of(Path::new("/a/bc")).overlaps(&Folder::of(Path::new("/a/b"))));
    }

    #[test]
    fn a_mirror_must_not_remove_in_folders_other_jobs_write() {
        let two_way = setup(SyncMode::TwoWay, "/home/me/Docs", "/mnt/nas/Docs");
        let mirror = setup(SyncMode::Mirror, "/home/me/Work", "/mnt/nas/Docs/Work");
        assert!(mirror.clashes_with(&two_way));
        assert!(two_way.clashes_with(&mirror));
        // Backing up a two-way folder somewhere else is fine, and so are chained mirrors.
        let backup = setup(SyncMode::Backup, "/home/me/Docs", "/mnt/usb/Docs");
        assert!(!backup.clashes_with(&two_way));
        let chained = setup(SyncMode::Mirror, "/mnt/nas/Docs", "/mnt/usb/Docs");
        assert!(!chained.clashes_with(&mirror));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_not_made_yet_is_placed_through_the_links_above_it() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("CloudStorage");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("Dropbox")).unwrap();
        let later = Folder::of(&tmp.path().join("Dropbox/Backup"));
        assert!(later.is(&Folder::of(&real.join("Backup"))));
        assert!(later.overlaps(&Folder::of(&real)));
        let dotted = Folder::of(&tmp.path().join("Dropbox/missing/../Backup"));
        assert!(dotted.is(&later));
    }

    #[cfg(unix)]
    #[test]
    fn nesting_through_a_link_is_refused_before_the_inner_folder_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("CloudStorage");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("Dropbox")).unwrap();
        let source = real.to_str().unwrap().to_string();
        let destination = tmp.path().join("Dropbox/Backup").to_str().unwrap().to_string();
        let mut mirror = setup(SyncMode::Mirror, &source, &destination);
        assert!(mirror.validate().is_err());
    }

    #[test]
    fn case_is_ignored_when_either_drive_ignores_it() {
        let share = Folder { text: Keys::fold("/mnt/share/Docs"), folded: true };
        let spelled_otherwise = Folder { text: "/mnt/share/DOCS/Work".into(), folded: false };
        assert!(share.overlaps(&spelled_otherwise));
        assert!(share.is(&Folder { text: "/mnt/Share/Docs".into(), folded: false }));
        let here = Folder { text: "/home/me/Docs".into(), folded: false };
        assert!(!here.overlaps(&Folder { text: "/home/me/docs/Work".into(), folded: false }), "a drive that tells case apart");
    }

    #[test]
    fn nested_folders_in_one_job_are_refused() {
        let mut job = setup(SyncMode::TwoWay, "/home/me", "/home/me/Backup");
        assert!(job.validate().is_err());
        let mut job = setup(SyncMode::TwoWay, "/home/me/a/../Docs", "/home/me/Docs");
        assert!(job.validate().is_err());
    }

    #[test]
    fn schedules_count_from_the_latest_moment() {
        let mut job = Job::new(setup(SyncMode::Backup, "/a", "/b"));
        let later = job.created_at + chrono::Duration::hours(3);
        job.last_scheduled = Some(later);
        assert_eq!(job.schedule_reference(later), Some(later));
        // Seen an hour after the job was made, the later time was taken while the clock was ahead.
        let an_hour_in = job.created_at + chrono::Duration::hours(1);
        assert_eq!(job.schedule_reference(an_hour_in), Some(job.created_at));
        assert_eq!(job.schedule_reference(job.created_at - chrono::Duration::hours(1)), None);
    }

    #[test]
    fn automatic_changes_are_remembered_for_a_day_once_per_file() {
        let keys = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        let mut job = Job::new(setup(SyncMode::TwoWay, "/a", "/b"));
        let now = Utc::now();
        job.note_automatic(now - chrono::Duration::hours(30), &keys(&["old.txt"]), &[]);
        job.note_automatic(now - chrono::Duration::hours(2), &keys(&["log.txt", "a.txt"]), &keys(&["b.txt"]));
        job.note_automatic(now - chrono::Duration::hours(1), &keys(&["log.txt"]), &[]);
        job.note_automatic(now, &[], &[]);
        assert_eq!(job.recent_automatic(now), (set(&["a.txt", "log.txt"]), set(&["b.txt"])));
        assert_eq!(job.automatic_changes.source.len(), 2, "older than a day is forgotten");
    }

    #[test]
    fn a_full_window_keeps_the_newest_items() {
        let mut job = Job::new(setup(SyncMode::TwoWay, "/a", "/b"));
        let now = Utc::now();
        let first: Vec<String> = (0..1_000).map(|n| format!("first/{n}")).collect();
        let second: Vec<String> = (0..10).map(|n| format!("second/{n}")).collect();
        job.note_automatic(now - chrono::Duration::hours(2), &[], &first);
        job.note_automatic(now, &[], &second);
        let (_, destination) = job.recent_automatic(now);
        assert_eq!(destination.len(), 1_001, "still more than the guard's largest count");
        assert!(second.iter().all(|key| destination.contains(key)));
    }
}
