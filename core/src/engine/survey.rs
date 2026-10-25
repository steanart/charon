//! A survey: both folders as they are now, what it takes to bring them together, and
//! everything about that the person (or a run nobody looks at) must know first.
//!
//! Every look goes through here: the background checks, the reviews the person confirms,
//! and the runs of jobs with full automation. Nothing in a survey changes a file.

use crate::engine::overlaps::WaitsOn;
use crate::engine::{runs, Engine, EngineEvent, Moment};
use crate::error::{Error, Result};
use crate::model::{human_bytes, human_count, plural, ConflictRule, Job, JobSetup, Warning};
use crate::plan::{self, Action, Baseline, Guard, NoteKind, OneWayPlanner, Pair, Plan, Recent, Side, Unit};
use crate::run::{Copier, Found, KeptCopies, Marker, Markers, Refused, Room, Trash, TrashLook, Unread};
use crate::scan::{
    is_legacy_partial, is_partial, never_download_on_this_thread, partial_started, Contents, Entry, EntryKind, ExcludeFilter, Keys, Refusal, Scanner, Snapshot,
    Volume,
};
use crate::store::Memory;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Half-copied files from an interrupted sync are tidied away once they're this old (a
/// younger one may belong to a sync running right now in another job).
const LEFTOVER_AGE_MS: i64 = 60 * 60 * 1000;
/// A list names at most this many of the files named like Charon 0.2's half-copied files.
const LEFTOVERS_NAMED: usize = 5;
/// A background look compares at most this much by contents. What it leaves unread, unless a
/// full read found it identical as both sides are now (see `store::SameContents`), is not
/// compared yet: never "in sync" (a list the person sees, and every run, read it all).
const CHECK_COMPARE_BYTES: u64 = 1 << 30;
/// Comparison progress reaches the window at most this often.
const COMPARE_PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(250);
/// What a look says when it ran into a mistake in Charon itself.
pub(crate) const INTERNAL_LOOK: &str =
    "Charon ran into an error of its own while looking at the folders, so it changed nothing. If this keeps happening, please report it.";

/// Why a survey is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// A background look for changes.
    Check,
    /// A list the person will look at.
    Review,
    /// A run nobody looks at (full automation).
    Automatic,
}

/// What a sync also does besides its steps, on the sides it writes to.
#[derive(Debug, Clone, Default)]
pub(crate) struct Housekeeping {
    /// What each side's trash holds: the dated folders old enough to be emptied, and files
    /// changed after they went in.
    pub trash: Vec<(Side, TrashLook)>,
    /// Half-copied files left by an interrupted sync, and empty files it left holding a
    /// name, per side (on-disk path, item).
    pub leftovers: Vec<(Side, Vec<(String, Entry)>)>,
}

impl Housekeeping {
    /// The lines a review shows.
    pub fn lines(&self, trash_days: u32) -> Vec<String> {
        let mut lines = Vec::new();
        for (side, look) in &self.trash {
            let expired = &look.expired;
            if expired.is_empty() {
                continue;
            }
            let items: u64 = expired.iter().map(|e| e.items).sum();
            let bytes: u64 = expired.iter().map(|e| e.bytes).sum();
            lines.push(format!(
                "Also empties {} older than {} from .charon-trash on the {} ({}).",
                plural(items, "item"),
                plural(trash_days as u64, "day"),
                side.word(),
                human_bytes(bytes)
            ));
        }
        for (side, leftovers) in &self.leftovers {
            let (claims, partials): (Vec<&str>, Vec<&str>) = leftovers.iter().map(|(path, _)| path.as_str()).partition(|path| !is_partial(Keys::name(path)));
            let (legacy, current): (Vec<&str>, Vec<&str>) = partials.into_iter().partition(|path| is_legacy_partial(Keys::name(path)));
            if !current.is_empty() {
                // Most are half-copied files; one may also be a link's copy, or a whole file a
                // sync had renamed aside (an old journal, a file it was removing, a replaced
                // version whose removal failed, or a version an app saved that the trash
                // couldn't take).
                lines.push(format!(
                    "Also moves {} a sync left under a temporary name (most often a half-copied file from an interrupted sync) into .charon-trash on the {}.",
                    plural(current.len() as u64, "item"),
                    side.word()
                ));
            }
            if !claims.is_empty() {
                lines.push(format!(
                    "Also moves {} into .charon-trash on the {}: an interrupted sync left {} there, empty, to hold the name for a copy.",
                    Self::names(&claims),
                    side.word(),
                    if claims.len() == 1 { "it" } else { "them" }
                ));
            }
            if !legacy.is_empty() {
                lines.push(format!(
                    "Also moves {} into .charon-trash on the {}: Charon 0.2 named its half-copied files this way. If one of them is yours, rename it before you confirm.",
                    Self::names(&legacy),
                    side.word()
                ));
            }
        }
        lines
    }

    /// The half-copied files (and empty files left holding a name, see `scan::is_claim`) in
    /// `snapshot` a sync made for `purpose` tidies away at `now`: those nothing wrote to for
    /// an hour, and whose copy started more than an hour ago by the time in their name (a
    /// copy being finished gets the original's old modification time, so its change time
    /// counts too, and a long copy may belong to another computer's sync). Files named like
    /// Charon 0.2's half-copied files may be the person's own: only a list the person sees,
    /// which names them, moves those.
    fn leftovers(snapshot: &Snapshot, now: i64, purpose: Purpose) -> Vec<(String, Entry)> {
        snapshot
            .partials()
            .iter()
            .filter(|(path, entry)| {
                let name = Keys::name(path);
                let last_written = entry.modified.max(entry.changed / 1_000_000);
                let started_long_ago = partial_started(name, now).is_none_or(|started| now - started > LEFTOVER_AGE_MS);
                now - last_written > LEFTOVER_AGE_MS && started_long_ago && (purpose == Purpose::Review || !is_legacy_partial(name))
            })
            .cloned()
            .collect()
    }

    /// “a.txt”, “b.txt” and 3 more.
    fn names(paths: &[&str]) -> String {
        let shown: Vec<String> = paths.iter().take(LEFTOVERS_NAMED).map(|path| format!("“{path}”")).collect();
        let more = paths.len().saturating_sub(LEFTOVERS_NAMED);
        if more > 0 {
            return format!("{} and {} more", shown.join(", "), human_count(more as u64));
        }
        match shown.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => shown.join(", "),
        }
    }
}

/// A pair of files whose contents are read (see `Engine::compare_contents`).
struct Candidate {
    key: String,
    source: Entry,
    destination: Entry,
    /// Both sides look alike by size and date (and changed since the last sync, or have no
    /// record of it).
    alike: bool,
}

/// Which pairs that look alike, with no record Charon can go by, are read all the same
/// (see `Engine::candidate`): the pairs found to differ before and left for the person
/// (`Memory::unsettled`), and, in a fresh comparison, those that a record of the last sync
/// Charon can't trust (see `Engine::distrust`) shows changed since, on either side. It
/// never decides what is carried or removed.
#[derive(Default)]
struct Guide {
    /// The record Charon can't trust, if the comparison is a fresh one.
    pairs: Baseline,
    unsettled: BTreeSet<String>,
    /// The memory was made with the other kind of keys (see `Keys::fold`), so it is keyed,
    /// and looked up, folded.
    folded: bool,
}

impl Guide {
    /// What `memory` tells a look whose keys are `folded` or not; `afresh` when Charon
    /// can't trust its record.
    fn of(memory: &Memory, folded: bool, afresh: bool) -> Self {
        let pairs = if afresh { memory.baseline.clone() } else { Baseline::new() };
        if memory.folded == folded {
            return Self { pairs, unsettled: memory.unsettled.clone(), folded: false };
        }
        Self {
            pairs: pairs.iter().map(|(key, pair)| (Keys::fold(key), *pair)).collect(),
            unsettled: memory.unsettled.iter().map(|key| Keys::fold(key)).collect(),
            folded: true,
        }
    }

    /// Whether the look-alike pair at `key` (`s` and `d` as they are now) is read all the same.
    fn wants_read(&self, key: &str, s: &Entry, d: &Entry, source: &Snapshot, destination: &Snapshot) -> bool {
        self.left_different(key)
            || self.pairs.get(&self.key(key)).is_some_and(|pair| {
                !Entry::still_on(Some(s), Some(&pair.source), source.shifts_times())
                    || !Entry::still_on(Some(d), Some(&pair.destination), destination.shifts_times())
            })
    }

    /// Whether the pair at `key` was found to differ when it was last read, and was left for
    /// the person.
    fn left_different(&self, key: &str) -> bool {
        self.unsettled.contains(&self.key(key))
    }

    /// `key` as the guide's records are keyed.
    fn key(&self, key: &str) -> String {
        if self.folded {
            Keys::fold(key)
        } else {
            key.to_string()
        }
    }
}

/// What reading contents found.
#[derive(Default)]
struct Compared {
    /// Pairs found identical, now recorded as in sync.
    agreed: u64,
    /// Pairs that look alike but whose contents differ.
    different: BTreeSet<String>,
    /// Pairs a background look left unread, with nothing known about their contents.
    uncompared: BTreeSet<String>,
}

/// Both folders now, the plan, and what deserves a look.
#[derive(Debug, Clone)]
pub(crate) struct Survey {
    /// The job's setup the survey was made with.
    pub setup: JobSetup,
    pub source: Snapshot,
    pub destination: Snapshot,
    /// Whether keys ignore upper and lower case (one of the drives does).
    pub folded: bool,
    /// The job's exclude filter, as the scans used it.
    pub filter: ExcludeFilter,
    /// The job's markers in both folders, read before the scans. A sync records nothing
    /// unless they are still the same when it's done.
    pub markers: Markers,
    /// Two-way: the memory as loaded, with the reason this look couldn't trust it, if it
    /// couldn't (see `Engine::distrust`).
    pub memory: Option<Memory>,
    /// Two-way: the plan was made with the stored memory.
    pub trusted: bool,
    /// The baseline the plan was made with (the trusted memory, plus pairs found identical).
    pub baseline: Baseline,
    /// Mirror and Backup: the copies whose dates didn't stick that the plan was made with.
    pub landed: Baseline,
    pub plan: Plan,
    /// Removals a run nobody looks at took out of `plan` and left for a later run (see
    /// `Missing`).
    pub left_for_later: Plan,
    /// Pairs of files found identical by their contents, though their dates differ.
    pub agreed: u64,
    /// Pairs that look alike but were found to differ by their contents (see
    /// `Memory::unsettled`).
    pub different: BTreeSet<String>,
    /// Pairs a background look left unread, with nothing known about their contents: they
    /// may differ. Its plan leaves them as they are, and it says so (see `Look::uncompared`).
    pub uncompared: BTreeSet<String>,
    /// Why this plan must not run at all.
    pub refusal: Option<String>,
    /// Why no plan of these folders may run: one is inside the other, whatever their paths
    /// say (see `Overlaps::refusal`). It's in `refusal` too.
    pub nesting: Option<String>,
    /// Where an overlap with another job's folder can't be told until another job's sync is
    /// done, when that's all that holds a run back (see `overlaps::Waiting`): a run nobody
    /// looks at waits for that sync.
    pub waits_on: Option<WaitsOn>,
    pub warnings: Vec<Warning>,
    /// Why a run nobody looks at must not go ahead.
    pub holds: Vec<String>,
    /// Confirming needs an explicit tick.
    pub needs_tick: bool,
    /// A folder isn't as the last sync left it: it's on another drive, or (Mirror and
    /// Backup) it lacks the marker Charon left there. Only a confirmed sync takes it as it is.
    pub unfamiliar: bool,
    pub housekeeping: Housekeeping,
    pub made_at: DateTime<Utc>,
}

impl Survey {
    pub fn volume(&self, side: Side) -> Option<&Volume> {
        self.snapshot(side).volume()
    }

    pub fn snapshot(&self, side: Side) -> &Snapshot {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }

    /// The path of `key` as the person sees it: as spelled on the side that has it.
    pub fn spelling(&self, key: &str) -> String {
        Self::spelled(&self.source, &self.destination, key)
    }

    /// `spelling`, from the two scans before a survey is made of them.
    pub(crate) fn spelled(source: &Snapshot, destination: &Snapshot, key: &str) -> String {
        if source.contains(key) || source.has_spelling(key) {
            source.spelling(key).to_string()
        } else {
            destination.spelling(key).to_string()
        }
    }

    /// The sides a sync of this job writes to.
    pub fn written_sides(setup: &JobSetup) -> &'static [Side] {
        if setup.mode.is_two_way() {
            &[Side::Source, Side::Destination]
        } else {
            &[Side::Destination]
        }
    }
}

impl Engine {
    /// Scans both folders of `job` and plans its next sync, with every check that goes with
    /// it. `cancel` stops the scans. A mistake in Charon itself (a panic) ends the look with
    /// an error, like any other failed look: nothing is changed, and the job isn't left
    /// marked as being looked at forever.
    pub(crate) fn survey(&self, job: &Job, cancel: &AtomicBool, purpose: Purpose) -> Result<Survey> {
        std::panic::catch_unwind(AssertUnwindSafe(|| self.look_at(job, cancel, purpose))).unwrap_or_else(|_| {
            log::error!("looking at the folders of {} ran into a panic", job.setup.name);
            Err(Error::Invalid(INTERNAL_LOOK.into()))
        })
    }

    fn look_at(&self, job: &Job, cancel: &AtomicBool, purpose: Purpose) -> Result<Survey> {
        let setup = job.setup.clone();
        // A look that was stopped ends before its next step on either folder: it may have
        // waited for one of them (a share that didn't answer) while another job was given the
        // other's drive (see `quiet.rs`).
        let go_on = || if cancel.load(Ordering::Relaxed) { Err(Error::Cancelled) } else { Ok(()) };
        for root in [&setup.source, &setup.destination] {
            go_on()?;
            if !root.is_dir() {
                return Err(Error::Offline(root.clone()));
            }
        }
        go_on()?;
        let source_marker = Marker::read(&setup.source, &job.id);
        go_on()?;
        let markers = Markers { source: source_marker, destination: Marker::read(&setup.destination, &job.id) };
        go_on()?;
        let source_volume = Self::drive_of(job, Side::Source);
        go_on()?;
        let volumes = (source_volume, Self::drive_of(job, Side::Destination));
        go_on()?;
        let folded = !(volumes.0.case_sensitive && volumes.1.case_sensitive);
        let filter = ExcludeFilter::new(&setup.excludes, folded)?;
        // The folder watcher tells the changes that matter with the same filter.
        self.change_known_runtime(&job.id, |rt| rt.filter = Some((setup.clone(), filter.clone())));
        let (mut source, mut destination) = self.scan_both(job, &filter, volumes.clone(), cancel, purpose != Purpose::Check)?;
        if folded {
            source.fold();
            destination.fold();
        }
        // Where other jobs' folders are inside these, for the busy notes of their syncs.
        let inside = Self::folders_inside(&setup, &source, &destination);
        let (no_source_links, no_destination_links) = self
            .change_known_runtime(&job.id, |rt| {
                rt.folders_inside = inside;
                rt.no_links
            })
            .unwrap_or_default();
        // A drive found refusing links at an earlier sync still does, while it's the same drive.
        let refused_before = |side: Side, now: &Volume| {
            job.drives.as_ref().is_some_and(|drives| {
                let before = drives.side(side);
                before.refuses_links && Self::same_drive(before, now)
            })
        };
        if no_source_links || refused_before(Side::Source, &volumes.0) {
            source.refuse_links();
        }
        if no_destination_links || refused_before(Side::Destination, &volumes.1) {
            destination.refuse_links();
        }
        self.apply_refusals(job, &mut source, &mut destination, &go_on)?;
        self.apply_unread(job, &mut source, &mut destination, &go_on)?;

        let mut warnings = Vec::new();
        let mut holds = Vec::new();
        let mut needs_tick = false;
        let mut unfamiliar = false;
        // Why the job waits, where this look can't tell by itself.
        match job.held.as_deref() {
            Some(held) if purpose == Purpose::Review && held == runs::STOPPED => warnings.push(Warning::info(held)),
            Some(held) if purpose == Purpose::Review && (held == runs::UNRECORDED || held == runs::UNCOUNTED) => warnings.push(Warning::caution(held)),
            _ => {}
        }

        // Two-way: what Charon remembers of the last sync, if it can be trusted. Where it
        // can't, both folders are compared afresh this time, and the list says why once the
        // plan shows what that brings back.
        let mut afresh = None;
        let mut guide = Guide::default();
        let (memory, trusted, mut baseline) = if setup.mode.is_two_way() {
            let memory = self.inner.baselines.load(&job.id, &setup);
            match self.distrust(job, &memory, folded, &markers) {
                Some(reason) => {
                    afresh = Some(reason.clone());
                    guide = Guide::of(&memory, folded, true);
                    (Some(Memory { distrust: Some(reason), ..memory }), false, Baseline::new())
                }
                None if memory.tokens.is_none() => {
                    warnings.push(Warning::info(format!(
                        "This is the first two-way sync of these folders (or the first since the job's folders or mode changed). \
                         Charon compares both and removes nothing: items only on one side are copied to the other, and {}.",
                        Self::differing_files(setup.conflict)
                    )));
                    if job.last_synced.is_some() {
                        holds.push("Charon has no memory of this job's last sync, so it compares both folders afresh.".into());
                    }
                    (Some(memory), false, Baseline::new())
                }
                None => {
                    let baseline = memory.baseline.clone();
                    guide = Guide::of(&memory, folded, false);
                    (Some(memory), true, baseline)
                }
            }
        } else {
            (None, false, Baseline::new())
        };
        // Mirror and Backup: copies whose dates didn't stick on the destination's drive.
        let landed = if setup.mode.is_two_way() {
            Baseline::new()
        } else {
            self.inner.landings.load(&job.id, &setup, folded, &markers.destination)
        };
        let compared = if setup.mode.is_two_way() {
            Self::without_downloads("The comparison of files", || self.compare_contents(job, &source, &destination, &markers, &mut baseline, &guide, purpose, cancel))?
        } else {
            Compared::default()
        };
        let agreed = compared.agreed;
        go_on()?;

        let record = if setup.mode.is_two_way() { &baseline } else { &landed };
        let plan = plan::plan_for(&setup, &source, &destination, record, &compared.different);
        self.change_missing(job, |missing| missing.note(&plan, &source, &destination, Moment::now()));
        if let (Some(reason), Some(memory)) = (&afresh, &memory) {
            let mut text = format!(
                "{reason} So this time Charon compares both folders afresh: it removes nothing, so items removed since the last sync may come back, and {}.",
                Self::differing_files(setup.conflict)
            );
            if let Some(found) = Self::one_side_only(&plan, memory, |key| Survey::spelled(&source, &destination, key)) {
                text = format!("{text} {found}");
            }
            holds.push(text.clone());
            warnings.push(Warning::caution(text));
        }
        // The folders themselves first: one inside the other (whatever the paths say), and
        // other jobs that would undo this one's work.
        go_on()?;
        let overlaps = self.overlaps(job, &source, &destination, &go_on)?;
        go_on()?;
        let nesting = overlaps.refusal;
        let refusal = nesting.clone().or_else(|| Guard::refusal(&plan, &source, &destination));
        if let Some(refusal) = &refusal {
            holds.push(refusal.clone());
        }
        for clash in overlaps.clashes {
            holds.push(clash.clone());
            warnings.push(Warning::danger(clash));
            needs_tick = true;
        }
        // Those that can't be told until another job's sync is done hold back after all the
        // other reasons (see below).
        let waiting = overlaps.waiting;
        for clash in &waiting.clashes {
            warnings.push(Warning::danger(clash.clone()));
            needs_tick = true;
        }
        warnings.extend(overlaps.copies.into_iter().map(Warning::info));

        // What automatic syncs removed or replaced, today and since the last confirmed list,
        // counts too: for the runs nobody looks at, and in the list of a job that has them,
        // so it shows why a run was held.
        let recent = (purpose == Purpose::Automatic || (purpose == Purpose::Review && setup.full_automation)).then(|| {
            let now = Utc::now();
            let (source, destination) = job.recent_automatic(now);
            let since_confirmed = self.inner.unattended.load(&job.id, self.settings().trash_days, now);
            Recent { source, destination, since_confirmed }
        });
        for concern in Guard::concerns(&plan, &source, &destination, &baseline, recent.as_ref()) {
            holds.push(concern.clone());
            warnings.push(Warning::danger(concern));
            needs_tick = true;
        }

        // The folders must be on the drives they were on at the last sync.
        if let Some(drives) = &job.drives {
            for (side, before, now) in [(Side::Source, &drives.source, &volumes.0), (Side::Destination, &drives.destination, &volumes.1)] {
                if !Self::same_drive(before, now) {
                    let text = format!(
                        "The {} folder is now on {}, but at the last sync it was on {}. If that drive isn't connected, this may be a leftover folder on another disk.",
                        side.word(),
                        now.describe(),
                        before.describe()
                    );
                    holds.push(text.clone());
                    warnings.push(Warning::danger(text));
                    needs_tick = true;
                    unfamiliar = true;
                }
            }
        }
        // Mirror and Backup: both folders must carry the markers of the last sync.
        if !setup.mode.is_two_way() {
            let concerns = [Self::destination_concern(job, &markers.destination), Self::source_concern(job, &markers.source)];
            for text in concerns.into_iter().flatten() {
                holds.push(text.clone());
                warnings.push(Warning::danger(text));
                needs_tick = true;
                unfamiliar = true;
            }
        }

        // What makes room for an item of another kind goes into .charon-trash whatever the
        // setting (see `Executor::delete`).
        let clearings = plan.clearings();
        let removals = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Delete { item, .. } if *item != EntryKind::Dir) && !clearings.iter().any(|clearing| clearing.holds(a)))
            .count() as u64;
        let replacements: u64 = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Copy { replaces: true, item, .. } if *item != EntryKind::Dir))
            .count() as u64;
        if !setup.safe_delete && removals + replacements > 0 {
            let text = if setup.mode.is_two_way() || replacements == 0 {
                "Safe deletes are off for this job: removed files are gone for good, with no copy in .charon-trash."
            } else {
                "Safe deletes are off for this job: removed and replaced files are gone for good, with no copy in .charon-trash."
            };
            warnings.push(Warning::caution(text));
            needs_tick = needs_tick || removals > 0 || !setup.mode.is_two_way();
        }
        let unreadable = plan.notes.iter().filter(|n| n.kind == NoteKind::Unreadable).count() as u64;
        if unreadable > 0 {
            warnings.push(Warning::caution(format!(
                "Charon couldn't read {}, so nothing in {} is synced. {} listed in the notes.",
                plural(unreadable, "place"),
                if unreadable == 1 { "it" } else { "them" },
                if unreadable == 1 { "It's" } else { "They're" }
            )));
        }
        let left_alone = plan.notes.iter().filter(|n| matches!(n.kind, NoteKind::LeftOut | NoteKind::NameClash | NoteKind::Blocked)).count() as u64;
        if left_alone > 0 {
            warnings.push(Warning::caution(format!(
                "{} can't be synced (a name or size the other drive can't hold, two names that clash, a file meeting a folder…). {} listed in the notes, with the reason.",
                if left_alone == 1 { "1 item".to_string() } else { format!("{} items", human_count(left_alone)) },
                if left_alone == 1 { "It's" } else { "They're" }
            )));
        }
        let settled = plan.summary().newest_wins;
        if settled > 0 {
            warnings.push(Warning::info(format!(
                "{} settled by Newest wins: the version saved last replaces the other, and the replaced version goes to .charon-trash. This relies on both computers' clocks.",
                if settled == 1 { "1 conflict is".to_string() } else { format!("{} conflicts are", human_count(settled)) }
            )));
        }
        let undated = OneWayPlanner::backup().knowing_landed(landed.clone()).landed_alike(&source, &destination);
        if undated > 0 {
            warnings.push(Warning::info(format!(
                "{} in the destination {} other dates than in the source, because the destination's drive doesn't keep the dates Charon sets. \
                 {} copied in full and neither side changed since, so Charon leaves {} alone.",
                plural(undated, "file"),
                if undated == 1 { "has" } else { "have" },
                if undated == 1 { "It was" } else { "They were" },
                if undated == 1 { "it" } else { "them" }
            )));
        }
        if let Some(text) = Self::units_warning(&setup, &plan, &source, &destination) {
            warnings.push(Warning::caution(text));
        }
        let links = plan.actions.iter().filter(|a| matches!(a, Action::Copy { item: EntryKind::Link, .. })).count() as u64;
        if links > 0 {
            let mut text = format!(
                "{} copied as {}, not the files {} point to.",
                if links == 1 { "1 symbolic link is".to_string() } else { format!("{} symbolic links are", human_count(links)) },
                if links == 1 { "a link" } else { "links" },
                if links == 1 { "it" } else { "they" }
            );
            // A job synced before, but not since it got a record of its drives: possibly by
            // Charon 0.2, which left links out.
            if job.last_synced.is_some() && job.drives.is_none() {
                text.push_str(" Charon 0.2 left symbolic links out, so this list may include links that were never synced before.");
            }
            warnings.push(Warning::info(text));
        }

        let housekeeping = self.housekeeping(job, &source, &destination, purpose, cancel);
        go_on()?;
        for (side, look) in &housekeeping.trash {
            if let Some(text) = Self::revived_warning(*side, look) {
                holds.push(text.clone());
                warnings.push(Warning::caution(text));
            }
            if purpose != Purpose::Check {
                if let Some(text) = self.trash_size_warning(&setup, *side, look) {
                    warnings.push(Warning::info(text));
                }
            }
        }
        for &side in Survey::written_sides(&setup) {
            let volume = match side {
                Side::Source => &source,
                Side::Destination => &destination,
            }
            .volume();
            let bytes = Self::bytes_onto(&setup, &plan, side, volume);
            if bytes == 0 {
                continue;
            }
            go_on()?;
            if let Some(why) = Room::shortage(setup.root(side), bytes, &format!("the {}'s drive", side.word())) {
                let text = format!(
                    "{}{}. Copies that don't fit will fail, and while copies to the {} fail, nothing is removed there. Make room on the drive first.",
                    why[..1].to_uppercase(),
                    &why[1..],
                    side.word()
                );
                holds.push(text.clone());
                warnings.push(Warning::caution(text));
            }
        }
        // A run nobody looks at waits for that sync only when nothing else holds it back;
        // otherwise those clashes are among the reasons, after the others.
        let waits_on = waiting.on.filter(|_| holds.is_empty());
        holds.extend(waiting.clashes);
        Ok(Survey {
            setup,
            source,
            destination,
            folded,
            filter,
            markers,
            memory,
            trusted,
            baseline,
            landed,
            plan,
            left_for_later: Plan::default(),
            agreed,
            different: compared.different,
            uncompared: compared.uncompared,
            refusal,
            nesting,
            waits_on,
            warnings,
            holds,
            needs_tick,
            unfamiliar,
            housekeeping,
            made_at: Utc::now(),
        })
    }

    /// What a fresh comparison copies to the side that lacks it (items on one side only),
    /// as a sentence for the list, with how many of them the last sync left on both sides by
    /// a record Charon can't decide with (the memory it distrusts, or 0.2's): those were most
    /// likely removed on one side since, and come back. `None` when nothing is on one side
    /// only.
    fn one_side_only(plan: &Plan, memory: &Memory, spelled: impl Fn(&str) -> String) -> Option<String> {
        let only_on = |side: Side| -> Vec<&str> {
            plan.actions
                .iter()
                .filter_map(|action| match action {
                    Action::Copy { to, path, replaces: false, .. } | Action::CreateDir { side: to, path } if *to == side.opposite() => Some(path.as_str()),
                    _ => None,
                })
                .collect()
        };
        let (on_source, on_destination) = (only_on(Side::Source), only_on(Side::Destination));
        let count = on_source.len() + on_destination.len();
        if count == 0 {
            return None;
        }
        let copied = match (on_source.len(), on_destination.len()) {
            (n, 0) => format!("{} only on the source {} copied to the destination.", plural(n as u64, "item"), if n == 1 { "is" } else { "are" }),
            (0, n) => format!("{} only on the destination {} copied to the source.", plural(n as u64, "item"), if n == 1 { "is" } else { "are" }),
            (s, d) => format!("{} are only on one side ({} on the source, {} on the destination) and are copied to the other.", plural(count as u64, "item"), human_count(s as u64), human_count(d as u64)),
        };
        // The record's keys and the plan's may be folded differently: both are compared folded.
        let earlier: BTreeSet<String> = memory.baseline.paths().chain(memory.earlier.iter()).map(|key| Keys::fold(key)).collect();
        let back: Vec<&str> = on_source.iter().chain(on_destination.iter()).copied().filter(|path| earlier.contains(&Keys::fold(path))).collect();
        if back.is_empty() {
            return Some(copied);
        }
        // Named by the topmost of them: a folder that comes back stands for what's inside. The
        // plan's keys are folded where a drive ignores case: the names are as on the disk.
        let topmost: Vec<String> =
            back.iter().copied().filter(|path| !Keys::ancestors(path).any(|folder| back.contains(&folder))).map(spelled).collect();
        let topmost: Vec<&str> = topmost.iter().map(String::as_str).collect();
        Some(format!(
            "{copied} By Charon's record of the last sync, {} of them {} on both sides then, so {} most likely removed since: {}.",
            human_count(back.len() as u64),
            if back.len() == 1 { "was" } else { "were" },
            if back.len() == 1 { "it was" } else { "they were" },
            Housekeeping::names(&topmost)
        ))
    }

    /// What a two-way sync without a memory does with files that differ, by the job's rule
    /// for conflicts.
    fn differing_files(rule: ConflictRule) -> &'static str {
        match rule {
            ConflictRule::Ask => "files that differ are left for you to choose",
            ConflictRule::KeepBoth => "files that differ are kept twice",
            ConflictRule::NewestWins => {
                "where files differ, the version saved last replaces the other, which goes to .charon-trash (both are kept when the times can't tell)"
            }
        }
    }

    /// App libraries and databases this sync changes: copied while their app writes to them,
    /// they may arrive half-written.
    fn units_warning(setup: &JobSetup, plan: &Plan, source: &Snapshot, destination: &Snapshot) -> Option<String> {
        let units: BTreeSet<Unit> = plan.work().filter_map(|action| Unit::of(action.path(), source, destination)).collect();
        let names: Vec<String> = units
            .iter()
            .map(|unit| {
                let side = if source.contains(unit.key()) { source } else { destination };
                format!("“{}”", side.spelling(unit.key()))
            })
            .collect();
        let computers = if setup.mode.is_two_way() { " (on both computers)" } else { "" };
        match names.as_slice() {
            [] => None,
            [name] => Some(format!(
                "{name} is {}. Close the app that uses it before you confirm{computers}, or it may be copied while it's half-written.",
                units.first().map_or("", Unit::describe)
            )),
            _ => {
                let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                let more = if names.len() > 3 { format!(" and {} more", human_count(names.len() as u64 - 3)) } else { String::new() };
                Some(format!(
                    "{} app libraries or databases change in this sync: {shown}{more}. Close the apps that use them before you confirm{computers}, or they may be copied while they're half-written.",
                    human_count(names.len() as u64)
                ))
            }
        }
    }

    /// Scans both folders at once (they're usually on different drives), each on its own
    /// thread, which never downloads files that are only in the cloud. When one side can't
    /// be scanned, the other stops too: the look fails either way, and a big folder or a
    /// share isn't read to the end for nothing. Two folders on one drive run by an add-on
    /// driver are scanned one after the other: such a driver is best given one task at a
    /// time (see `quiet.rs`).
    fn scan_both(&self, job: &Job, filter: &ExcludeFilter, volumes: (Volume, Volume), cancel: &AtomicBool, report: bool) -> Result<(Snapshot, Snapshot)> {
        let (source_volume, destination_volume) = volumes;
        let one_at_a_time = source_volume.run_by_an_add_on_driver() && source_volume.mount == destination_volume.mount && !source_volume.mount.is_empty();
        let failed = AtomicBool::new(false);
        let scan = |side: Side, volume: Volume| {
            let counter = |items: u64| {
                self.note_look_moved(&job.id);
                if report {
                    self.emit(EngineEvent::ScanProgress { id: job.id.clone(), side, items });
                }
            };
            let root = match side {
                Side::Source => &job.setup.source,
                Side::Destination => &job.setup.destination,
            };
            let quiet = |folder: &std::path::Path| self.kept_quiet(folder);
            let scanner = Scanner::new(filter).cancellable(cancel).cancellable(&failed).counting(&counter).keeping_off(&quiet);
            let scanned = scanner.scan(root, volume);
            if scanned.is_err() {
                failed.store(true, Ordering::Relaxed);
            }
            scanned
        };
        if one_at_a_time {
            let source = Self::without_downloads("The scan of the source", || scan(Side::Source, source_volume))?;
            let destination = Self::without_downloads("The scan of the destination", || scan(Side::Destination, destination_volume))?;
            return Ok((source, destination));
        }
        let (source, destination) = std::thread::scope(|scope| {
            let source = std::thread::Builder::new().name("charon-scan".into()).spawn_scoped(scope, || {
                never_download_on_this_thread();
                scan(Side::Source, source_volume)
            });
            let destination = Self::without_downloads("The scan of the destination", || scan(Side::Destination, destination_volume));
            let source = match source {
                Ok(handle) => handle.join().unwrap_or_else(|_| Err(Error::Invalid("The scan of the source stopped unexpectedly.".into()))),
                Err(e) => Err(Error::Io(e)),
            };
            (source, destination)
        });
        match (source, destination) {
            (Ok(source), Ok(destination)) => Ok((source, destination)),
            // The side that failed first says why; the other one only stopped because of it.
            (Err(Error::Cancelled), Err(e)) | (Err(e), Err(Error::Cancelled)) if !cancel.load(Ordering::Relaxed) => Err(e),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    }

    /// Runs `work` on a thread of its own that never downloads files that are only in the
    /// cloud. A look may be asked for on any thread (the window's, for a list), and that
    /// thread keeps its own setting. `what` names the work if it stops unexpectedly.
    fn without_downloads<T: Send>(what: &str, work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
        std::thread::scope(|scope| {
            let handle = std::thread::Builder::new().name("charon-scan".into()).spawn_scoped(scope, || {
                never_download_on_this_thread();
                work()
            })?;
            handle.join().unwrap_or_else(|_| Err(Error::Invalid(format!("{what} stopped unexpectedly."))))
        })
    }

    /// Mirror and Backup: why the destination may not be the folder of the last sync.
    fn destination_concern(job: &Job, found: &Found) -> Option<String> {
        let token = job.destination_token.as_ref()?;
        if matches!(found, Found::Marker(marker) if &marker.token == token) {
            return None;
        }
        Some("The destination doesn't carry the marker Charon left there at the last sync. It may be another drive or folder under the same name.".into())
    }

    /// Mirror and Backup: why the source may not be the folder of the last sync. An older
    /// copy of the source would make the sync remove or replace the newest backups.
    fn source_concern(job: &Job, found: &Found) -> Option<String> {
        let mark = job.source_mark.as_ref()?;
        let text = match found {
            Found::Marker(marker) if marker.token == mark.token && marker.generation == mark.generation => return None,
            Found::Marker(marker) if marker.token == mark.token && marker.generation < mark.generation => {
                "The source went back in time since the last sync: it may be an older copy of the drive, restored from a backup. \
                 Syncing it could replace newer files in the destination with older ones, or remove the newest."
            }
            Found::Marker(marker) if marker.token == mark.token => {
                "The source was synced after Charon last saved its record of this job (was Charon's own data restored from a backup?)."
            }
            Found::Marker(_) => "The source carries another sync's marker, so it may not be the folder Charon synced from (another drive with the same name?).",
            Found::Missing => {
                "The source doesn't carry the marker Charon left there at the last sync. It may be another drive or folder under the same name, or an older copy of it."
            }
            Found::Unusable(_) => "Charon's marker in the source can't be read, so it can't tell whether this is the folder it synced from.",
        };
        Some(text.into())
    }

    /// Why the stored two-way memory can't be trusted for these folders, if it can't.
    fn distrust(&self, job: &Job, memory: &Memory, folded: bool, markers: &Markers) -> Option<String> {
        if let Some(reason) = &memory.distrust {
            return Some(reason.clone());
        }
        if job.run_in_progress {
            return Some("The last sync of this job didn't finish properly (Charon was closed, the computer stopped, or a drive went away).".into());
        }
        // No memory: nothing to trust or distrust before the first sync of these folders
        // (nothing is removed). After one (it has a record of their drives), it was lost.
        if memory.tokens.is_none() {
            return (job.last_synced.is_some() && job.drives.is_some()).then(|| "Charon has no memory of this job's last sync.".into());
        }
        if memory.folded != folded {
            let guessed = job.drives.as_ref().is_some_and(|drives| drives.source.case_guessed || drives.destination.case_guessed);
            return Some(if guessed {
                "At the last sync Charon couldn't ask one of the drives how it treats upper and lower case, and it can now: the drive does it differently than Charon assumed.".into()
            } else {
                "One of the drives treats upper and lower case differently than at the last sync.".into()
            });
        }
        for side in [Side::Source, Side::Destination] {
            let expected = memory.token(side).unwrap_or_default();
            let problem = match markers.side(side) {
                Found::Marker(marker) if marker.token != expected => {
                    format!("The {} folder carries another sync's marker, so it may not be the folder Charon synced with (another drive with the same name?).", side.word())
                }
                Found::Marker(marker) if marker.generation < memory.generation => format!(
                    "The {} folder went back in time since the last sync (restored from a copy, or the last sync's changes never reached the drive).",
                    side.word()
                ),
                Found::Marker(marker) if marker.generation > memory.generation => {
                    format!("The {} folder was synced after Charon last saved what it remembers.", side.word())
                }
                Found::Marker(_) => continue,
                Found::Missing => format!(
                    "The {} folder doesn't carry Charon's marker from the last sync: it may be another folder or drive under the same name, or the marker was removed.",
                    side.word()
                ),
                Found::Unusable(reason) => format!("Charon's marker in the {} folder can't be read ({reason}).", side.word()),
            };
            return Some(problem);
        }
        None
    }

    /// Pairs of files whose contents decide what the plan does, read byte for byte:
    ///
    /// * no record of the last sync (or both sides changed since), the same size but
    ///   different dates: identical pairs are recorded as in sync;
    /// * no record Charon can trust, the same size and date, but a record it can't trust
    ///   shows that one side changed since, or an earlier look found them to differ (see
    ///   `Guide`): an edit that kept the file's size and time (an encrypted container does)
    ///   would otherwise be taken for identical, and recorded as in sync. If their contents
    ///   differ, they're a conflict;
    /// * one side changed since the record but kept its size, and only its details or its
    ///   time moved, by a rounding or by quarter hours (a tag or permission change, a drive
    ///   that shifts or rounds times): identical to the other side, the record is renewed;
    /// * both sides changed since the record and still look alike: if their contents differ
    ///   after all, they're a conflict (`different`), never "already in sync".
    ///
    /// A pair this session read before isn't read again while both sides are still exactly as
    /// they were then and both folders carry the `markers` (as read before the scans) they
    /// carried then (see `Reads`). A background look reads at most `CHECK_COMPARE_BYTES`.
    /// Past that, it goes by what full reads found before: identical, on the same terms (see
    /// `store::SameContents`), or different and left for the person (see `Guide`). The rest
    /// is not compared yet (`Compared::uncompared`): the look never shows as in sync what it
    /// didn't read. What the look read, and the pairs found identical, are kept for the looks
    /// after this one (the pairs even after a restart), only while both folders still carry
    /// the markers they carried before the scans.
    #[allow(clippy::too_many_arguments)]
    fn compare_contents(
        &self,
        job: &Job,
        source: &Snapshot,
        destination: &Snapshot,
        markers: &Markers,
        baseline: &mut Baseline,
        guide: &Guide,
        purpose: Purpose,
        cancel: &AtomicBool,
    ) -> Result<Compared> {
        let candidates: Vec<Candidate> = source.iter().filter_map(|(key, s)| Self::candidate(key, s, source, destination, baseline, guide)).collect();
        let total_bytes: u64 = candidates.iter().map(|c| c.source.size).sum();
        let total_files = candidates.len() as u64;
        let mut compared = Compared::default();
        // The pairs full reads found identical before in these folders, and those this look
        // knows to be.
        let folded = source.is_folded();
        let found_before = self.inner.same_contents.load(&job.id, &job.setup, folded, markers);
        let mut found_now = Baseline::new();
        // The pairs this look read and noted.
        let mut noted: Vec<String> = Vec::new();
        let (mut done_files, mut done_bytes, mut read_bytes) = (0u64, 0u64, 0u64);
        let mut last_sent: Option<std::time::Instant> = None;
        for candidate in candidates {
            let Candidate { key, source: s, destination: d, alike } = candidate;
            if cancel.load(Ordering::Relaxed) {
                return Err(self.stopped_comparing(job, markers, &noted));
            }
            self.note_look_moved(&job.id);
            let pair = Pair::new(s, d);
            let cached = self.change_known_runtime(&job.id, |rt| rt.reads.verdict(&key, &s, &d, markers)).flatten();
            let same = match cached {
                Some(same) => Some(same),
                // A background look that has read enough leaves the rest for the next list. It
                // goes by what a full read found: identical as both sides are now, or different
                // and left for the person.
                None if purpose == Purpose::Check && read_bytes + s.size > CHECK_COMPARE_BYTES => {
                    if found_before.get(&key).is_some_and(|before| *before == pair) {
                        Some(true)
                    } else {
                        guide.left_different(&key).then_some(false)
                    }
                }
                None => {
                    let a = job.setup.source.join(source.spelling(&key));
                    let b = job.setup.destination.join(destination.spelling(&key));
                    read_bytes += s.size;
                    let mut left = Vec::new();
                    let same = Contents::same(&a, &b, Some(cancel), &|| self.note_look_moved(&job.id), &mut left);
                    self.let_go(left);
                    match same {
                        Ok(same) => {
                            self.change_known_runtime(&job.id, |rt| rt.reads.note(&key, s, d, same, markers));
                            noted.push(key.clone());
                            Some(same)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Err(self.stopped_comparing(job, markers, &noted)),
                        // Unreadable now: the plan decides as if they differ (never a removal),
                        // for this look only. The next one reads them again.
                        Err(_) => Some(false),
                    }
                }
            };
            match same {
                Some(true) => {
                    found_now.insert(key.clone(), pair);
                    baseline.insert(key, pair);
                    compared.agreed += 1;
                }
                Some(false) if alike => {
                    compared.different.insert(key);
                }
                Some(false) => {}
                // Not compared: the plan leaves the pair as it is, and the look says why it
                // isn't in sync.
                None => {
                    baseline.insert(key.clone(), pair);
                    compared.uncompared.insert(key);
                }
            }
            done_files += 1;
            done_bytes += s.size;
            if purpose != Purpose::Check && last_sent.is_none_or(|t| t.elapsed() >= COMPARE_PROGRESS_EVERY || done_files == total_files) {
                last_sent = Some(std::time::Instant::now());
                self.emit(EngineEvent::CompareProgress { id: job.id.clone(), files: done_files, total_files, bytes: done_bytes, total_bytes });
            }
        }
        // The last pair may have waited for a share while another job was given the drive.
        if cancel.load(Ordering::Relaxed) {
            return Err(self.stopped_comparing(job, markers, &noted));
        }
        // What the look read, and the pairs it keeps, hold only for the folders it read.
        let keep = found_now != found_before;
        if (!noted.is_empty() || keep) && !self.still_marked(job, markers, &noted, &|| !cancel.load(Ordering::Relaxed)) {
            return Ok(compared);
        }
        if keep {
            self.keep_same_contents(job, folded, markers, &found_now);
        }
        Ok(compared)
    }

    /// Ends a comparison that was stopped, after it read and `noted` some pairs. What it read
    /// holds only for the folders it was read in: it's kept while both still carry `markers`.
    /// But while a sync keeps a drive of them quiet (another job's, or the job's own, which a
    /// look it stopped may outlive), the markers aren't read there (see `quiet.rs`), and what
    /// this look read is forgotten instead; what earlier looks read and checked stays (a sync
    /// of the job's own carries it, see `Reads::carry`). That's asked again before each
    /// marker: the first may wait for a share while a sync is given the other's drive.
    fn stopped_comparing(&self, job: &Job, markers: &Markers, noted: &[String]) -> Error {
        if !noted.is_empty() {
            self.still_marked(job, markers, noted, &|| !self.kept_quiet(&job.setup.source) && !self.kept_quiet(&job.setup.destination));
        }
        Error::Cancelled
    }

    /// Whether both folders of `job` still carry the `markers` read before the scans, after
    /// the look read and `noted` some pairs. If not, a folder was replaced while the look read
    /// it (another drive under the same name, a folder restored from a copy) and may have been
    /// read in part: what this session read while they carried those markers is forgotten, and
    /// the look keeps nothing it found. Each marker is read only while `go_on` says so; when it
    /// says no, the look keeps nothing either, and forgets the pairs it `noted` (what earlier
    /// looks read was checked at their end).
    fn still_marked(&self, job: &Job, markers: &Markers, noted: &[String], go_on: &dyn Fn() -> bool) -> bool {
        match self.markers_still(job, markers, go_on) {
            Some(true) => true,
            Some(false) => {
                self.change_known_runtime(&job.id, |rt| rt.reads.forget(markers));
                false
            }
            None => {
                self.change_known_runtime(&job.id, |rt| rt.reads.forget_pairs(noted, markers));
                false
            }
        }
    }

    /// Whether both folders of `job` still carry `markers`: `None` when `go_on` says no before
    /// a marker is read.
    fn markers_still(&self, job: &Job, markers: &Markers, go_on: &dyn Fn() -> bool) -> Option<bool> {
        for side in [Side::Source, Side::Destination] {
            if !go_on() {
                return None;
            }
            if Marker::read(job.setup.root(side), &job.id) != *markers.side(side) {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Keeps the pairs a look found identical for the background looks after it, even after a
    /// restart, in the folders that carry `markers` (see `store::SameContents`): those that
    /// are no longer to be read go. Saved while the stored state is locked, and only while
    /// the job is still there with the folders and mode the look was made with: removing the
    /// job or changing them forgets the record, and a look that was under way then never
    /// brings it back. A record that can't be saved is removed, and the pairs are read again
    /// or not compared yet.
    fn keep_same_contents(&self, job: &Job, folded: bool, markers: &Markers, pairs: &Baseline) {
        let state = self.state();
        if !state.jobs.iter().any(|stored| stored.id == job.id && stored.setup.same_folders(&job.setup)) {
            return;
        }
        if let Err(e) = self.inner.same_contents.save(&job.id, &job.setup, folded, markers, pairs) {
            log::warn!("couldn't keep the files found identical in {} for later looks: {e}", job.setup.name);
            let _ = self.inner.same_contents.forget(&job.id);
        }
    }

    /// Whether the pair at `key` needs its contents read (see `compare_contents`).
    fn candidate(key: &str, s: &Entry, source: &Snapshot, destination: &Snapshot, baseline: &Baseline, guide: &Guide) -> Option<Candidate> {
        let d = destination.get(key)?;
        if !(s.is_file() && d.is_file() && s.size == d.size) || source.frozen(key).is_some() || destination.frozen(key).is_some() {
            return None;
        }
        let wanted = match baseline.get(key) {
            None => !s.same_as(d) || guide.wants_read(key, s, d, source, destination),
            Some(pair) => {
                let source_still = Entry::still_on(Some(s), Some(&pair.source), source.shifts_times());
                let destination_still = Entry::still_on(Some(d), Some(&pair.destination), destination.shifts_times());
                match (source_still, destination_still) {
                    (true, true) => false,
                    (false, false) => true,
                    (false, true) => s.drifted_from(&pair.source),
                    (true, false) => d.drifted_from(&pair.destination),
                }
            }
        };
        let alike = s.same_as(d);
        wanted.then(|| Candidate { key: key.to_string(), source: *s, destination: *d, alike })
    }

    /// Notes in the scans what a drive refused in an earlier sync of this session (see
    /// `run::Refused`), while the item it refused and the drive are still the same: the plan
    /// then leaves it out, with the reason, instead of trying it again on every sync.
    ///
    /// A name the drive found another item under is refused only while that item is there.
    /// Once it's gone (the sync removed it, or the person renamed it), the refusal lapses and
    /// the next sync carries the item: it would otherwise stay out for the whole session
    /// while the job looks in sync. Likewise, a conflict copy that couldn't be made only
    /// matters while both versions are there.
    ///
    /// `go_on` is asked before each place it looks at (see `look_at`).
    fn apply_refusals(&self, job: &Job, source: &mut Snapshot, destination: &mut Snapshot, go_on: &dyn Fn() -> Result<()>) -> Result<()> {
        let refusals: Vec<_> = self.change_known_runtime(&job.id, |rt| rt.refused.values().cloned().collect()).unwrap_or_default();
        let mut lapsed = Vec::new();
        for (refused, drive) in refusals {
            let (from, here) = match refused.side {
                Side::Source => (&*destination, &mut *source),
                Side::Destination => (&*source, &mut *destination),
            };
            let over = match refused.why {
                Refusal::TakenName => {
                    go_on()?;
                    Self::name_freed(&refused, here)
                }
                Refusal::ConflictCopy => !here.contains(&refused.key),
                _ => false,
            };
            if over {
                lapsed.push(refused);
                continue;
            }
            let same_item = from.get(&refused.key).is_some_and(|now| now.unchanged(&refused.item));
            let same_drive = match (&drive, here.volume()) {
                (Some(before), Some(now)) => Self::same_drive(before, now),
                _ => true,
            };
            if same_item && same_drive {
                here.refuse(&refused.key, refused.why);
            }
        }
        if !lapsed.is_empty() {
            self.change_known_runtime(&job.id, |rt| {
                for refused in lapsed {
                    let place = (refused.side, refused.key.clone());
                    // Only the refusal looked at here: a sync may have recorded a newer one since.
                    if rt.refused.get(&place).is_some_and(|(stored, _)| *stored == refused) {
                        rt.refused.remove(&place);
                    }
                }
            });
        }
        Ok(())
    }

    /// Whether the name a drive found another item under is free again: nothing is at the
    /// place it was refused any more, or the item is now there under its own name. A place
    /// that can't be looked at keeps the refusal.
    fn name_freed(refused: &Refused, here: &Snapshot) -> bool {
        if here.contains(&refused.key) {
            return true;
        }
        match fs::symlink_metadata(&refused.at) {
            Ok(_) => false,
            Err(e) => matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory),
        }
    }

    /// Notes in the scans the files a sync of this session couldn't read (see `run::Unread`)
    /// that still can't be read, whether they changed since or not (the program that holds
    /// one open may go on writing to it). The plan then leaves each alone on both sides, as
    /// it leaves alone a place a scan can't read, instead of trying it again on every sync,
    /// and removes nothing on the side it should be copied to (see `Snapshot::freeze_unread`).
    /// A file that can be read now is tried again. `go_on` is asked before each file (see
    /// `look_at`), and the look moves on with each.
    fn apply_unread(&self, job: &Job, source: &mut Snapshot, destination: &mut Snapshot, go_on: &(dyn Fn() -> Result<()> + Sync)) -> Result<()> {
        let unread: Vec<Unread> = self.change_known_runtime(&job.id, |rt| rt.unread.values().cloned().collect()).unwrap_or_default();
        if unread.is_empty() {
            return Ok(());
        }
        // Reading a file the cloud took back since the scan would download it.
        Self::without_downloads("The look at files Charon couldn't read", || {
            for item in unread {
                let snapshot = match item.side {
                    Side::Source => &mut *source,
                    Side::Destination => &mut *destination,
                };
                if !snapshot.get(&item.key).is_some_and(Entry::is_file) {
                    continue;
                }
                go_on()?;
                self.note_look_moved(&job.id);
                let path = job.setup.root(item.side).join(snapshot.spelling(&item.key));
                if let Some(reason) = Copier::read_refusal(&path, item.at) {
                    snapshot.freeze_unread(&item.key, reason);
                }
            }
            Ok(())
        })
    }

    /// Old trash to empty and leftovers of interrupted syncs to tidy, on the sides the job
    /// writes to. A background look only looks into the trash folders old enough to be
    /// emptied.
    fn housekeeping(&self, job: &Job, source: &Snapshot, destination: &Snapshot, purpose: Purpose, cancel: &AtomicBool) -> Housekeeping {
        let setup = &job.setup;
        let trash_days = self.settings().trash_days;
        let now = Utc::now().timestamp_millis();
        let mut housekeeping = Housekeeping::default();
        // The trash look moves on as the look does, and ends when it's stopped (what it found
        // then counts for nothing: `look_at` ends too).
        let go_on = || {
            let going = !cancel.load(Ordering::Relaxed);
            if going {
                self.note_look_moved(&job.id);
            }
            going
        };
        for &side in Survey::written_sides(setup) {
            let (root, snapshot) = match side {
                Side::Source => (&setup.source, source),
                Side::Destination => (&setup.destination, destination),
            };
            housekeeping.trash.push((side, Trash::look_going_on(root, trash_days, purpose != Purpose::Check, snapshot.shifts_times(), &go_on)));
            housekeeping.leftovers.push((side, Housekeeping::leftovers(snapshot, now, purpose)));
        }
        housekeeping
    }

    /// Files saved into the trash after Charon put them there: an app that had a file open
    /// may have followed it into the trash and gone on saving someone's work there.
    fn revived_warning(side: Side, look: &TrashLook) -> Option<String> {
        let count = look.revived.len() as u64;
        if count == 0 {
            return None;
        }
        let one = count == 1;
        let shown: Vec<String> = look.revived.iter().take(3).map(|path| format!("“{path}”")).collect();
        let more = if count > 3 { format!(", and {} more", human_count(count - 3)) } else { String::new() };
        Some(format!(
            "{} in .charon-trash on the {} changed after Charon put {} there: an app that had {} open may have saved someone's work into {}. \
             Charon won't empty {}; move what you need back, or delete {} yourself: {}{more}.",
            plural(count, "file"),
            side.word(),
            if one { "it" } else { "them" },
            if one { "it" } else { "them" },
            if one { "it" } else { "them" },
            if one { "it" } else { "them" },
            if one { "it" } else { "them" },
            shown.join(", ")
        ))
    }

    /// A trash that takes a large share of its drive.
    fn trash_size_warning(&self, setup: &JobSetup, side: Side, look: &TrashLook) -> Option<String> {
        let room = Room::of(setup.root(side))?;
        if look.bytes < 1_000_000_000 || look.bytes < room.total / 10 {
            return None;
        }
        let days = self.settings().trash_days;
        let emptied = if days == 0 {
            "Settings keep removed files there for good".to_string()
        } else {
            format!("Charon empties what's older than {}", plural(days as u64, "day"))
        };
        Some(format!(
            ".charon-trash on the {} holds {} ({}% of its drive). {emptied}; to make room sooner, delete older dated folders from it yourself.",
            side.word(),
            human_bytes(look.bytes),
            look.bytes.saturating_mul(100) / room.total.max(1)
        ))
    }

    /// The room a sync needs on `side`'s drive (`volume`): its copies there, and the room the
    /// copies of the versions they replace that go into .charon-trash take there at one time
    /// (see `KeptCopies`). Each of those is copied into the trash right before its new version
    /// takes its place. Whether a replaced file has other names (hard links), which keep its
    /// room, isn't in the scan: the run asks the drive before an app library or database lands.
    fn bytes_onto(setup: &JobSetup, plan: &Plan, side: Side, volume: Option<&Volume>) -> u64 {
        let keeps_replaced = setup.safe_delete || setup.mode.is_two_way();
        let mut copies = 0u64;
        let mut replaced = Vec::new();
        for action in plan.work() {
            match action {
                Action::Copy { to, bytes, existing, settles_conflict, .. } if *to == side => {
                    copies = copies.saturating_add(*bytes);
                    replaced.extend(existing.filter(|old| old.is_file() && (keeps_replaced || *settles_conflict)).map(|old| (old.size, true)));
                }
                Action::KeepBoth { source_bytes, destination_bytes, .. } => {
                    copies = copies.saturating_add(match side {
                        Side::Destination => *source_bytes,
                        Side::Source => *destination_bytes,
                    });
                }
                _ => {}
            }
        }
        copies.saturating_add(KeptCopies::on(volume).room(replaced))
    }

    /// The drive of the job's `side` folder. Where the drive can't be asked how it treats
    /// upper and lower case this time, what it answered at the last sync holds while it's the
    /// same drive: a guess never makes Charon distrust what it remembers of the folders.
    fn drive_of(job: &Job, side: Side) -> Volume {
        Self::case_kept_from(Volume::of(job.setup.root(side)), job.drives.as_ref().map(|drives| drives.side(side)))
    }

    /// `now`, with how it treats case taken from `before` (the drive at the last sync) where
    /// `now` is only a guess and it's the same drive.
    fn case_kept_from(mut now: Volume, before: Option<&Volume>) -> Volume {
        if let Some(before) = before.filter(|before| now.case_guessed && Self::same_drive(before, &now)) {
            now.case_sensitive = before.case_sensitive;
            now.case_guessed = before.case_guessed;
        }
        now
    }

    /// Whether a folder is on the same drive as before (unknown details never count as a change).
    pub(crate) fn same_drive(before: &Volume, now: &Volume) -> bool {
        let same_mount = before.mount.is_empty() || now.mount.is_empty() || before.mount == now.mount;
        same_mount && before.kind == now.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 60 * 60 * 1000;

    #[test]
    fn files_named_like_old_half_copies_are_only_moved_from_a_list_that_names_them() {
        let now = 1_800_000_000_000;
        let old = now - 2 * HOUR_MS;
        let mut snapshot = Snapshot::new();
        snapshot.facts_mut().partials = vec![
            ("a/.charon-00112233445566ff.part".into(), Entry::file(1, old)),
            ("notes.charon-partial".into(), Entry::file(1, old)),
            // Its modification time is the original's, but it was written a moment ago.
            ("b/.charon-0011223344556677.part".into(), Entry::file(1, old).changed_at(now * 1_000_000)),
        ];
        let paths = |purpose| Housekeeping::leftovers(&snapshot, now, purpose).into_iter().map(|(path, _)| path).collect::<Vec<_>>();
        assert_eq!(paths(Purpose::Automatic), vec!["a/.charon-00112233445566ff.part"]);
        assert_eq!(paths(Purpose::Review), vec!["a/.charon-00112233445566ff.part", "notes.charon-partial"]);

        let leftovers = vec![(Side::Destination, Housekeeping::leftovers(&snapshot, now, Purpose::Review))];
        let lines = Housekeeping { trash: Vec::new(), leftovers }.lines(30);
        assert_eq!(lines[0], "Also moves 1 item a sync left under a temporary name (most often a half-copied file from an interrupted sync) into .charon-trash on the destination.");
        assert!(lines[1].starts_with("Also moves “notes.charon-partial” into .charon-trash on the destination:"), "{}", lines[1]);
    }

    #[test]
    fn an_empty_file_left_holding_a_name_is_named_in_the_list() {
        let now = 1_800_000_000_000;
        let mut snapshot = Snapshot::new();
        snapshot.facts_mut().partials = vec![("docs/report.txt".into(), Entry::file(0, crate::scan::CLAIM_MODIFIED_MS))];
        let leftovers = vec![(Side::Source, Housekeeping::leftovers(&snapshot, now, Purpose::Automatic))];
        let lines = Housekeeping { trash: Vec::new(), leftovers }.lines(30);
        assert_eq!(
            lines,
            vec!["Also moves “docs/report.txt” into .charon-trash on the source: an interrupted sync left it there, empty, to hold the name for a copy."]
        );
    }

    #[test]
    fn a_name_taken_by_another_item_is_free_again_once_that_item_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("REPORT~1.PDF"), "ours").unwrap();
        let theirs = dst.join("Report final.pdf");
        fs::write(&theirs, "theirs").unwrap();
        let engine = Engine::open(tmp.path().join("data"), std::sync::Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: String::new(),
            source: src.clone(),
            destination: dst.clone(),
            mode: crate::model::SyncMode::TwoWay,
            conflict: crate::model::ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let job = engine.stored_job(&id).unwrap();
        let cancel = AtomicBool::new(false);
        let first = engine.survey(&job, &cancel, Purpose::Review).unwrap();
        // What a sync records when the destination's drive finds "Report final.pdf" under the
        // short name REPORT~1.PDF. The refused place is that other item's own. (Keys are
        // spelled as the snapshot spells them: folded where the drive ignores case. Notes
        // name the item as it's spelled on the disk.)
        let key = first.source.key_for("REPORT~1.PDF");
        let refused = Refused {
            side: Side::Destination,
            key: key.clone(),
            why: Refusal::TakenName,
            item: *first.source.get(&key).unwrap(),
            at: theirs.clone(),
        };
        let remember = |refused: &Refused| {
            let drive = first.destination.volume().cloned();
            engine.change_runtime(&id, |rt| rt.refused.insert((refused.side, refused.key.clone()), (refused.clone(), drive)));
        };
        let copies = |survey: &Survey| {
            survey.plan.actions.iter().any(|a| matches!(a, Action::Copy { to: Side::Destination, path, .. } if *path == key))
        };
        remember(&refused);

        let held = engine.survey(&job, &cancel, Purpose::Review).unwrap();
        assert!(!copies(&held), "{:?}", held.plan.actions);
        assert!(held.plan.notes.iter().any(|n| n.path == "REPORT~1.PDF" && n.reason.contains("finds another item under its name")), "{:?}", held.plan.notes);

        // The other item goes (a sync removed it, or the person renamed it): the name is free.
        fs::rename(&theirs, dst.join("Report final (old).pdf")).unwrap();
        let free = engine.survey(&job, &cancel, Purpose::Check).unwrap();
        assert!(copies(&free), "{:?}", free.plan.actions);
        assert!(engine.change_runtime(&id, |rt| rt.refused.is_empty()), "the refusal lapsed for good");

        // Nor does it hold once the item is there under its own name. (Not on Windows, where
        // that name would reach the renamed item through its own short name.)
        if cfg!(windows) {
            return;
        }
        remember(&Refused { at: dst.join("Report final (old).pdf"), ..refused });
        fs::write(dst.join("REPORT~1.PDF"), "ours").unwrap();
        engine.survey(&job, &cancel, Purpose::Check).unwrap();
        assert!(engine.change_runtime(&id, |rt| rt.refused.is_empty()));
    }

    /// A look stopped while it heeds what a sync of this session couldn't do (names a drive
    /// found another item under, files it couldn't read) ends before its next place or file:
    /// a sync may have been given the drive meanwhile (see `quiet.rs`). Each file it reads
    /// again moves the look on.
    #[test]
    fn a_look_stopped_while_it_heeds_what_a_sync_couldnt_do_ends_before_its_next_place() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(src.join(name), name).unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), std::sync::Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: String::new(),
            source: src,
            destination: dst.clone(),
            mode: crate::model::SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        while engine.runtime().values().any(|rt| rt.checking.is_some()) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let job = engine.stored_job(&id).unwrap();
        let first = engine.survey(&job, &AtomicBool::new(false), Purpose::Review).unwrap();
        engine.change_runtime(&id, |rt| {
            for name in ["a.txt", "b.txt"] {
                let key = first.source.key_for(name);
                let item = *first.source.get(&key).unwrap();
                let refused = Refused { side: Side::Destination, key: key.clone(), why: Refusal::TakenName, item, at: dst.join(format!("{name} (theirs)")) };
                rt.refused.insert((Side::Destination, key.clone()), (refused, None));
                rt.unread.insert((Side::Source, key.clone()), Unread { side: Side::Source, key, reason: "in use".into(), at: 0 });
            }
        });
        let (mut source, mut destination) = (first.source.clone(), first.destination.clone());
        // Whether it went through, and how many times it asked whether to go on. Once
        // stopped, it stays stopped.
        let mut heed = |unread: bool, stop_at: usize| {
            let asked = std::sync::atomic::AtomicUsize::new(0);
            let go_on = || if asked.fetch_add(1, Ordering::Relaxed) + 1 >= stop_at { Err(Error::Cancelled) } else { Ok(()) };
            let heeded = if unread {
                engine.apply_unread(&job, &mut source, &mut destination, &go_on)
            } else {
                engine.apply_refusals(&job, &mut source, &mut destination, &go_on)
            };
            (heeded.is_ok(), asked.into_inner())
        };
        for stop_at in 1..=2 {
            assert_eq!(heed(false, stop_at), (false, stop_at), "names refused, stopped at {stop_at}");
            engine.change_runtime(&id, |rt| rt.look_moved = None);
            assert_eq!(heed(true, stop_at), (false, stop_at), "files unread, stopped at {stop_at}");
            assert_eq!(engine.runtime()[&id].look_moved.is_some(), stop_at == 2, "only the file read moved the look on");
        }
        assert_eq!(heed(true, 3), (true, 2));
        assert_eq!(heed(false, 3), (true, 2));
        assert!(engine.change_runtime(&id, |rt| rt.refused.is_empty()), "both names are free");
    }

    /// A look that ends after the job's folders or mode changed, or after the job was
    /// removed, keeps nothing of what it found identical: the edit or the removal forgot it,
    /// and the record would name the job's folders as they were.
    #[test]
    fn a_look_outdated_by_an_edit_or_a_removal_keeps_no_files_found_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let engine = Engine::open(tmp.path().join("data"), std::sync::Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: String::new(),
            source: src,
            destination: dst,
            mode: crate::model::SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup.clone()).unwrap().job.id;
        let job = engine.stored_job(&id).unwrap();
        let record = tmp.path().join("data/same-contents").join(format!("{id}.json"));
        let file = Entry::file(4, 10_000).changed_at(7);
        let mut found = Baseline::new();
        found.insert("a.txt", Pair::new(file, file));
        let marker = |side: Side| crate::run::Found::Marker(crate::run::Marker::new(&id, side, "token", 1));
        let markers = Markers { source: marker(Side::Source), destination: marker(Side::Destination) };

        engine.keep_same_contents(&job, false, &markers, &found);
        assert!(record.is_file(), "the job is as the look saw it");
        engine.update_job(&id, JobSetup { mode: crate::model::SyncMode::Mirror, ..setup }).unwrap();
        assert!(!record.exists(), "the edit forgot it");
        engine.keep_same_contents(&job, false, &markers, &found);
        assert!(!record.exists(), "a look of the two-way job ended after the edit");

        let mirror = engine.stored_job(&id).unwrap();
        engine.remove_job(&id).unwrap();
        engine.keep_same_contents(&mirror, false, &markers, &found);
        assert!(!record.exists(), "a look ended after the job was removed");
    }

    /// A folder replaced while a look read it (another drive under the same name, a folder
    /// restored from a copy) may have been read in part: what the look read stands for
    /// neither folder, in this session or in the record of the pairs found identical.
    #[test]
    fn a_look_keeps_nothing_it_read_while_a_folder_was_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        for folder in [&src, &dst] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("a.txt"), "same").unwrap();
        }
        let engine = Engine::open(tmp.path().join("data"), std::sync::Arc::new(|_: EngineEvent| {})).unwrap();
        let setup = JobSetup {
            name: String::new(),
            source: src.clone(),
            destination: dst.clone(),
            mode: crate::model::SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let id = engine.add_job(setup).unwrap().job.id;
        let job = engine.stored_job(&id).unwrap();
        let record = tmp.path().join("data/same-contents").join(format!("{id}.json"));
        let marker = |side: Side, token: &str| crate::run::Marker::new(&id, side, token, 1);
        marker(Side::Source, "source").write(&src).unwrap();
        marker(Side::Destination, "now").write(&dst).unwrap();
        let markers = |destination: &str| Markers { source: Found::Marker(marker(Side::Source, "source")), destination: Found::Marker(marker(Side::Destination, destination)) };
        // The same contents under different times: the pair is read.
        let (s, d) = (Entry::file(4, 10_000), Entry::file(4, 20_000));
        let source: Snapshot = [("a.txt".to_string(), s)].into_iter().collect();
        let destination: Snapshot = [("a.txt".to_string(), d)].into_iter().collect();
        let compare = |markers: &Markers| {
            engine.compare_contents(&job, &source, &destination, markers, &mut Baseline::new(), &Guide::default(), Purpose::Review, &AtomicBool::new(false)).unwrap()
        };
        let verdict = |markers: &Markers| engine.change_runtime(&id, |rt| rt.reads.verdict("a.txt", &s, &d, markers));

        // The look found the destination's marker before its scans, and another since.
        let before = markers("before");
        assert_eq!(compare(&before).agreed, 1);
        assert_eq!(verdict(&before), None, "not kept for this session");
        assert!(!record.exists(), "not kept for later launches");

        let now = markers("now");
        assert_eq!(compare(&now).agreed, 1);
        assert_eq!(verdict(&now), Some(true), "the folders stayed the same: kept");
        assert!(record.is_file());
    }

    #[test]
    fn a_fresh_comparison_says_what_it_brings_back() {
        let as_is = |key: &str| key.to_string();
        let file = Entry::file(1, 10_000);
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "a.txt", file, None));
        plan.push(Action::CreateDir { side: Side::Destination, path: "Photos".into() });
        plan.push(Action::copy(Side::Destination, "Photos/x.jpg", file, None));
        plan.push(Action::copy(Side::Source, "b.txt", file, None));
        plan.push(Action::copy(Side::Destination, "c.txt", file, Some(Entry::file(2, 20_000))));
        let mut memory = Memory::default();
        assert_eq!(
            Engine::one_side_only(&plan, &memory, as_is).as_deref(),
            Some("4 items are only on one side (3 on the source, 1 on the destination) and are copied to the other.")
        );
        memory.baseline.insert("Photos", Pair::new(Entry::dir(), Entry::dir()));
        memory.baseline.insert("Photos/x.jpg", Pair::new(file, file));
        memory.earlier.insert("B.txt".into());
        assert_eq!(
            Engine::one_side_only(&plan, &memory, as_is).as_deref(),
            Some(
                "4 items are only on one side (3 on the source, 1 on the destination) and are copied to the other. \
                 By Charon's record of the last sync, 3 of them were on both sides then, so they were most likely removed since: “Photos” and “b.txt”."
            ),
            "a folder stands for what's inside, and case doesn't hide an item"
        );
        let mut one = Plan::default();
        one.push(Action::copy(Side::Source, "b.txt", file, None));
        assert_eq!(
            Engine::one_side_only(&one, &memory, as_is).as_deref(),
            Some("1 item only on the destination is copied to the source. By Charon's record of the last sync, 1 of them was on both sides then, so it was most likely removed since: “b.txt”.")
        );
        assert_eq!(Engine::one_side_only(&Plan::default(), &memory, as_is), None);
        // Where a drive ignores case, the plan's keys are folded: the names are as on the disk.
        let mut folded = Plan::default();
        folded.push(Action::CreateDir { side: Side::Destination, path: "photos".into() });
        let on_disk = |key: &str| if key == "photos" { "Photos".to_string() } else { key.to_string() };
        assert!(Engine::one_side_only(&folded, &memory, on_disk).is_some_and(|text| text.ends_with("removed since: “Photos”.")));
    }

    /// Only what really is an app's package or a database asks for its app to be closed:
    /// not a single-file document or a private key that is only named like one.
    #[test]
    fn only_real_libraries_and_databases_ask_for_their_app_to_be_closed() {
        let setup = JobSetup {
            name: String::new(),
            source: "/a".into(),
            destination: "/b".into(),
            mode: crate::model::SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: crate::model::Trigger::Manual,
            excludes: Vec::new(),
            safe_delete: true,
            full_automation: false,
        };
        let file = Entry::file(1, 10_000);
        let source: Snapshot = [("server.key", file), ("Budget.numbers", file), ("Talk.key", Entry::dir()), ("Talk.key/Index.zip", file)]
            .iter()
            .map(|(key, entry)| (key.to_string(), *entry))
            .collect();
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "server.key", file, None));
        plan.push(Action::copy(Side::Destination, "Budget.numbers", file, None));
        assert_eq!(Engine::units_warning(&setup, &plan, &source, &Snapshot::new()), None);
        plan.push(Action::copy(Side::Destination, "Talk.key/Index.zip", file, None));
        let warning = Engine::units_warning(&setup, &plan, &source, &Snapshot::new()).unwrap();
        assert!(warning.starts_with("“Talk.key” is an app library or document."), "{warning}");
    }

    /// The room a sync needs on a drive, as the list says when it won't fit: its copies, and
    /// the versions they replace that are kept in .charon-trash. Each of those is copied into
    /// the trash right before its new version takes its place: on a local drive the old
    /// version gives its room back once removed, so the largest of them needs room, and on a
    /// network share, whose server may keep a removed file's data in a snapshot, all of them
    /// do. Where kept copies are clones (APFS), or none are kept, only the copies count.
    #[test]
    fn a_sync_needs_room_for_its_copies_and_the_versions_it_keeps_as_the_drive_takes_them() {
        use crate::model::{SyncMode, Trigger};
        use crate::scan::FsKind;
        let drive = |kind: FsKind, network: bool| Volume {
            kind,
            fs: String::new(),
            mount: "/x".into(),
            name: "x".into(),
            network,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        };
        let (local, share, mac) = (drive(FsKind::Ntfs, false), drive(FsKind::Smb, true), drive(FsKind::Apfs, false));
        let setup = |mode: SyncMode, safe_delete: bool| JobSetup {
            name: String::new(),
            source: "/a".into(),
            destination: "/b".into(),
            mode,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: Vec::new(),
            safe_delete,
            full_automation: false,
        };
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "Song.logicx/a.wav", Entry::file(4_000, 2_000), Some(Entry::file(3_000, 1_000))));
        plan.push(Action::copy(Side::Destination, "Song.logicx/b.wav", Entry::file(5_000, 2_000), Some(Entry::file(6_000, 1_000))));
        plan.push(Action::copy(Side::Destination, "notes.txt", Entry::file(100, 2_000), None));
        plan.push(Action::copy(Side::Source, "todo.txt", Entry::file(7, 2_000), None));
        let two_way = setup(SyncMode::TwoWay, false);
        let bytes = |setup: &JobSetup, side: Side, volume: &Volume| Engine::bytes_onto(setup, &plan, side, Some(volume));
        assert_eq!(bytes(&two_way, Side::Destination, &local), 9_100 + 6_000, "the copies, and the larger take kept");
        assert_eq!(bytes(&two_way, Side::Destination, &share), 9_100 + 3_000 + 6_000, "the copies, and both takes kept");
        assert_eq!(Engine::bytes_onto(&two_way, &plan, Side::Destination, None), 9_100 + 3_000 + 6_000, "a drive that couldn't be asked");
        assert_eq!(bytes(&two_way, Side::Destination, &mac), 9_100, "clones take no room");
        assert_eq!(bytes(&two_way, Side::Source, &share), 7);
        assert_eq!(bytes(&setup(SyncMode::Mirror, false), Side::Destination, &share), 9_100, "nothing replaced is kept");
        assert_eq!(bytes(&setup(SyncMode::Mirror, true), Side::Destination, &local), 9_100 + 6_000);
    }

    #[test]
    fn long_lists_of_names_are_cut_short() {
        assert_eq!(Housekeeping::names(&["a"]), "“a”");
        assert_eq!(Housekeeping::names(&["a", "b", "c"]), "“a”, “b” and “c”");
        assert_eq!(Housekeeping::names(&["a", "b", "c", "d", "e", "f", "g"]), "“a”, “b”, “c”, “d”, “e” and 2 more");
    }

    #[test]
    fn a_drive_that_can_only_be_guessed_keeps_its_answer_from_the_last_sync() {
        use crate::scan::FsKind;
        let drive = |mount: &str, case_sensitive: bool, case_guessed: bool| Volume {
            kind: FsKind::Other,
            fs: "tmpfs".into(),
            mount: mount.into(),
            name: "Share".into(),
            network: false,
            case_sensitive,
            case_guessed,
            refuses_links: false,
        };
        let answered = drive("/mnt/share", true, false);
        let guess = drive("/mnt/share", false, true);
        assert_eq!(Engine::case_kept_from(guess.clone(), Some(&answered)), answered);
        assert_eq!(Engine::case_kept_from(guess.clone(), None), guess, "nothing to keep before the first sync");
        let elsewhere = drive("/mnt/other", true, false);
        assert_eq!(Engine::case_kept_from(guess.clone(), Some(&elsewhere)), guess, "another drive's answer says nothing");
        // An answer always counts, also when the last sync had to guess.
        assert_eq!(Engine::case_kept_from(answered.clone(), Some(&guess)), answered);
    }
}
