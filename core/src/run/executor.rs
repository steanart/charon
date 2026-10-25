//! Runs a plan step by step.
//!
//! Every step was planned against a scan (or a review the person confirmed). Right before
//! a step acts, the executor looks at the disk again, on *both* sides: the item it is about
//! to copy, replace or remove must be exactly what the plan saw, and so must the other
//! side's item the step relies on (a file restored on the other side stops its removal
//! here). If anything changed, the step is skipped, never forced. Before every step both
//! folders must still be the folders the plan was made for: a drive that went away stops
//! the whole run. Parent folders are checked one by one, so nothing is ever written or
//! removed through a link. One failed or skipped item doesn't stop the run: it's counted
//! and reported.
//!
//! Every finished step is recorded (`Settled`), so the two-way memory can be updated with
//! what really happened. Before any of it is written down, `flush` makes the run's changes
//! durable and looks once more at what each finished step left (see `retrace`).

use crate::error::{Error, Result};
use crate::plan::{Action, Clearing, Pair, Plan, PutBack, Settled, Side, Step, Unit};
use crate::run::durable;
use crate::run::identity::Identity;
use crate::run::names;
use crate::run::rename::{move_to_new_name, rename_new, swap};
use crate::run::{Copier, KeptCopies, ProgressMeter, Room, Trash};
use crate::scan::{is_clutter, is_sync_journal, Entry, EntryKind, ExcludeFilter, Keys, Refusal, RootId, Scanner, Snapshot, Volume};
use chrono::Local;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;

/// The two folders of a job.
#[derive(Debug, Clone)]
pub struct Shores {
    pub source: PathBuf,
    pub destination: PathBuf,
}

impl Shores {
    pub fn root(&self, side: Side) -> &Path {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }
}

/// How a run treats what it removes and replaces, and what its record is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Care {
    /// Removed items go into the trash instead of being deleted for good.
    pub safe_delete: bool,
    /// Replaced files are kept in the trash even without safe deletes. Two-way jobs always
    /// do this: the replaced version may be someone's last copy of their own work.
    pub keep_replaced: bool,
    /// The run is recorded in a two-way memory, where an item missing on one side next time
    /// reads as removed there on purpose (see `Executor::retrace`).
    pub two_way: bool,
}

/// What a run achieved.
#[derive(Debug, Default)]
pub struct Outcome {
    pub copied: u64,
    pub deleted: u64,
    pub bytes: u64,
    /// `(path, reason)` for every step that failed.
    pub failed: Vec<(String, String)>,
    /// `(path, reason)` for every step left out because the item changed after the scan.
    pub skipped: Vec<(String, String)>,
    /// `(path, reason)` for every step left for the next sync only because the rest of its
    /// app library or database was: another item of it couldn't be synced, or something of it
    /// changed meanwhile (see `Executor::carry`).
    pub waited: Vec<(String, String)>,
    /// `(path, reason)` for every step whose item the drive turned out not to hold (a link,
    /// a name, a path or a size it refused). The next looks list such items as left out.
    pub left_out: Vec<(String, String)>,
    /// Folders that were to be removed but still hold something.
    pub kept: Vec<String>,
    pub cancelled: bool,
    /// Why the whole run stopped before the end (a folder went away), if it did.
    pub stopped: Option<String>,
    /// What each finished step left behind, in the order they finished.
    pub settled: Vec<Settled>,
    /// Files and links removed or replaced on the source and on the destination (keys).
    pub destroyed: (Vec<String>, Vec<String>),
    /// Sides whose drive turned out not to hold symbolic links.
    pub no_links: Vec<Side>,
    /// `(path, what)` for every copy that arrived without some of the original's details
    /// ("Finder tags", "resource fork"…), because the receiving drive can't keep them.
    pub unkept: Vec<(String, String)>,
    /// Items a drive refused to take.
    pub refused: Vec<Refused>,
    /// Two-way: items copied in this run whose original was gone, or an older version, by
    /// its end (moved, renamed or removed meanwhile, or a change its drive lost). They aren't
    /// recorded as synced, so the next look compares them afresh (see `Executor::retrace`).
    pub unrecorded: Vec<String>,
    /// Files the system wouldn't let this run read on the side they were to be copied from.
    pub unread: Vec<Unread>,
    /// Items steps that couldn't finish renamed aside and put back as they were (see
    /// `PutBack`).
    pub put_back: Vec<PutBack>,
}

/// An item a drive refused to take (see `Refusal`): the side it was to go to, its key, why,
/// and the item as it was on the side it came from (once that changes, it's worth a try again).
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub side: Side,
    pub key: String,
    pub why: Refusal,
    pub item: Entry,
    /// Where the drive refused it. For `Refusal::TakenName`, the path at which the drive
    /// found another item: once nothing is there, the name is free again.
    pub at: PathBuf,
}

/// A file the system wouldn't let a run read on the side it was to be copied from (no
/// permission, or another program holds it): that side, its key, why, and where in the
/// file reading stopped. Like a place a scan can't read, it is left alone on both sides
/// while it still can't be read there.
#[derive(Debug, Clone, PartialEq)]
pub struct Unread {
    pub side: Side,
    pub key: String,
    pub reason: String,
    pub at: u64,
}

/// What one step did.
enum Done {
    Copied(u64, u64),
    Deleted,
    Created,
    /// Nothing was needed (the item was already gone).
    Nothing,
    Skipped(String),
    /// Left for the next sync because the item of another kind in its place couldn't go
    /// first (counted with the skipped ones).
    Blocked(String),
    /// Left for the next sync with the rest of its app library or database, which only goes
    /// whole (see `Outcome::waited`).
    Waits(String),
    /// The drive can't hold the item (see `Outcome::left_out`).
    LeftOut(String),
    Kept,
}

/// A copy written next to its target, waiting to take its place (see `Executor::stage`).
struct Staged {
    to: Side,
    key: String,
    from: Entry,
    existing: Option<Entry>,
    settles: bool,
    relative: String,
    target: PathBuf,
    partial: PathBuf,
    /// The original's details the copy couldn't take along.
    unkept: BTreeSet<&'static str>,
}

/// The two files of a swap (see `Executor::replace`): the one checked at the target a
/// moment before, and the copy taking its place.
#[derive(Debug, Clone, Copy)]
struct Swapped {
    checked: Option<Identity>,
    copy: Option<Identity>,
}

/// What a finished step left at its item on each side, and when, for the last look before
/// the run is recorded (see `Executor::retrace`).
struct Footprint {
    key: String,
    source: Trace,
    destination: Trace,
    at: Instant,
}

/// What a finished step left at its item on one side.
#[derive(Debug, Clone, Copy)]
enum Trace {
    /// An item the step copied or created there; `replaced` is the version a copy took the
    /// place of.
    Left { item: Entry, replaced: Option<Entry> },
    /// The item a copy or a new folder was made after, on the side it came from.
    Original(Entry),
    /// The item the step removed from there (or found gone already).
    Removed(Entry),
    /// The other side of a removal, which had nothing there and wasn't touched: whatever is
    /// there now is new, and can't be a lost change of the run.
    Untouched,
}

/// How an item found at the end of a run shows that a finished step's change was lost.
#[derive(Debug)]
enum Loss {
    /// What the step left or copied from isn't there, or something of another kind is.
    Missing,
    /// The version a copy replaced is back in its place.
    Replaced,
    /// A version from before the step is back: older than the one a copy replaced, or than
    /// the original it copied.
    Older,
    /// The item the step removed is back.
    Back,
}

impl Trace {
    /// How `now`, the item at the step's place now, shows that the step's change was lost,
    /// if it does. Only what a lost change leaves counts: the item missing, an older version
    /// back in its place, a removed item back. An item changed in any other way was changed
    /// after the step: an app saved it again, a drive with coarse times rounded its time, a
    /// rename moved its change time. That is a change for the next look, not a lost one, so
    /// a person who goes on working during a long run doesn't make it fail.
    ///
    /// The change time tells versions apart that have the same size and time (an encrypted
    /// container keeps its time when it's edited): only the drive sets it, and it only moves
    /// forward. So the version a copy replaced, or an older one, still has the change time
    /// the scan saw, or an earlier one. This is only used where the scan trusted the drive's
    /// change times (`changed` isn't 0), and never when the copy has that very change time
    /// too (a drive that keeps it in whole seconds).
    fn lost(&self, now: Option<&Entry>) -> Option<Loss> {
        let Some(now) = now else {
            return matches!(self, Trace::Left { .. } | Trace::Original(_)).then_some(Loss::Missing);
        };
        match *self {
            Trace::Left { item, .. } | Trace::Original(item) if now.kind != item.kind => Some(Loss::Missing),
            Trace::Left { item, replaced: Some(replaced) } => {
                let timed = replaced.changed != 0 && now.changed != 0 && now.changed != item.changed;
                if (now.same_as(&replaced) && !now.same_as(&item)) || (timed && now.changed == replaced.changed) {
                    Some(Loss::Replaced)
                } else if timed && now.changed < replaced.changed {
                    Some(Loss::Older)
                } else {
                    None
                }
            }
            Trace::Original(copied) if Self::older(now, &copied) => Some(Loss::Older),
            Trace::Removed(removed) if now.same_as(&removed) => Some(Loss::Back),
            _ => None,
        }
    }

    /// Whether the file `now` is a version from before `copied`, the original a copy was
    /// made from, as its change time tells where the scan trusted the drive's: an earlier
    /// one, or the same one with another size or time (a drive keeps change times in steps
    /// of a few milliseconds, and a save after the copy comes steps later). Saving a file
    /// again moves it forward. Where change times aren't trusted, the next look takes a file
    /// whose modification time went back for a conflict anyway (see `TwoWayPlanner`).
    fn older(now: &Entry, copied: &Entry) -> bool {
        let differs = (now.size, now.modified) != (copied.size, copied.modified);
        now.is_file()
            && copied.is_file()
            && copied.changed != 0
            && now.changed != 0
            && (now.changed < copied.changed || (now.changed == copied.changed && differs))
    }
}

/// The removals a run holds back on a side where something it was to bring there didn't
/// arrive: a removal could take the last copy there of what didn't arrive, like the old
/// copy of a file in a renamed folder whose new copy wasn't made.
#[derive(Default)]
struct Holds {
    /// Sides where a copy failed or a drive refused an item: every removal there waits.
    sides: HashSet<Side>,
    /// Copies left for the next sync because their item changed since the scan, their place
    /// couldn't be cleared, or their app library or database waits: the side each was going
    /// to, its key, the item as planned and why it didn't land. Only the removals there that
    /// could be an old copy of one of them wait: an item with the same name, or with the same
    /// size and time. Holding every removal there would keep a job from ever removing
    /// anything while an app keeps writing to one file.
    unlanded: Vec<(Side, String, Entry, Unlanded)>,
}

/// Why a copy was left for the next sync (see `Holds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unlanded {
    /// Its item changed since the scan.
    Changed,
    /// The item of another kind in its place couldn't go first.
    Blocked,
    /// It waits with the rest of its app library or database.
    Waits,
}

impl Unlanded {
    /// Why a copy whose step had `result` was left for the next sync, if it was.
    fn of(result: &Result<Done>) -> Option<Unlanded> {
        match result {
            Ok(Done::Skipped(_)) | Err(Error::Changed(_)) => Some(Unlanded::Changed),
            Ok(Done::Blocked(_)) => Some(Unlanded::Blocked),
            Ok(Done::Waits(_)) => Some(Unlanded::Waits),
            _ => None,
        }
    }
}

/// Why a removal waits (see `Holds`).
enum Held<'h> {
    /// Copies to its side failed.
    Failed,
    /// It may be an old copy of the item at this key, which was left for the next sync for
    /// this reason.
    OldCopyOf(&'h str, Unlanded),
}

impl Holds {
    /// Notes what a step's result holds back: a copy that didn't land, the removals on its
    /// side.
    fn note(&mut self, action: &Action, result: &Result<Done>) {
        match (action, result, Unlanded::of(result)) {
            (_, Ok(Done::Copied(..)), _) => {}
            // The person sees what a drive refused left out in the next list before anything
            // goes.
            (_, Err(Error::Refused { .. }), _) => self.sides.extend(Executor::receiving(action)),
            (Action::Copy { to, path, from, .. }, _, Some(why)) => self.unlanded.push((*to, path.clone(), *from, why)),
            (Action::KeepBoth { path, source, destination, .. }, _, Some(why)) => {
                self.unlanded.push((Side::Destination, path.clone(), *source, why));
                self.unlanded.push((Side::Source, path.clone(), *destination, why));
            }
            (Action::Copy { .. } | Action::KeepBoth { .. }, _, None) => self.sides.extend(Executor::receiving(action)),
            _ => {}
        }
    }

    /// Why the step `action`, when it's a removal, waits, if it does.
    fn of(&self, action: &Action) -> Option<Held<'_>> {
        let Action::Delete { side, path, entry, .. } = action else { return None };
        if self.sides.contains(side) {
            return Some(Held::Failed);
        }
        self.unlanded
            .iter()
            .find(|(to, key, item, _)| to == side && (Keys::name(key) == Keys::name(path) || item.same_as(entry)))
            .map(|(_, key, _, why)| Held::OldCopyOf(key, *why))
    }
}

/// The steps of one app library or database in a plan (see `Executor::carry`).
#[derive(Default)]
struct UnitSteps<'p> {
    /// The copies and removals of its files and links, carried together.
    carried: Vec<&'p Action>,
    /// The sides the plan changes it on.
    sides: BTreeSet<Side>,
    /// The folders the plan makes in it, with the side each is made on.
    made: BTreeSet<(Side, &'p str)>,
    /// Whether the plan removes something of it, or clears a place in it for an item of
    /// another kind (see `Executor::starts_carry`).
    removes: bool,
}

/// What carrying an app library or database did to each of its carried steps, and why its
/// other steps wait, if they do (see `Executor::carry`).
type Carried<'p> = (Vec<(&'p Action, Result<Done>)>, Option<String>);

/// An app library or database whose new versions are written next to their targets and
/// checked, ready to take their places (see `Executor::prepare`).
struct Prepared<'u, 'p> {
    unit: &'u Unit,
    steps: &'u UnitSteps<'p>,
    staged: Vec<(&'p Action, Staged)>,
    /// The removals among its carried steps.
    removals: Vec<&'p Action>,
}

/// The old journal of a database, renamed aside while the database's new version takes its
/// place (see `Executor::set_aside`).
struct Aside<'p> {
    /// The step that removes it.
    step: &'p Action,
    side: Side,
    key: &'p str,
    /// The journal as the plan saw it.
    planned: Entry,
    /// Its place, as spelled in its side's folder, and in full.
    relative: String,
    target: PathBuf,
    /// Where it is meanwhile; `None` when it was gone already.
    at: Option<PathBuf>,
}

/// What an app library or database whose new versions all landed still removes: it waits
/// for every library and database carried by then (see `Executor::remove_units`).
#[derive(Default)]
struct Leftover<'p> {
    /// The old journals of its databases, renamed aside (see `Executor::set_aside`).
    asides: Vec<Aside<'p>>,
    /// Its other removals.
    removals: Vec<&'p Action>,
}

impl Leftover<'_> {
    /// Whether it removes nothing.
    fn is_empty(&self) -> bool {
        self.asides.is_empty() && self.removals.is_empty()
    }
}

/// Why the steps of an app library or database wait (see `Executor::carry`).
enum Wait {
    /// A removal among them waits (see `Holds`).
    Held,
    /// Something of it changed since the scan: found once its new versions were written next
    /// to their targets (see `still_ready`).
    Changed,
    /// Another of its steps couldn't be carried out.
    NotMixed,
}

/// What became of an item Charon set out to remove, or to move into the trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// It's no longer in its place: removed, or kept in the trash.
    Removed,
    /// It was gone already. For a removal that's what the plan wanted; for an item to be
    /// replaced, it was removed a moment ago, after the plan.
    Gone,
    /// It isn't the item the plan saw anymore, so it stays.
    Changed,
}

/// What the folders above an item are.
enum Parents {
    Real,
    /// One of them is gone (and so is the item).
    Missing,
    /// One of them is now a file or a link.
    NotFolders,
}

/// How each side spells paths on disk, from the scans the plan was made from.
pub struct Spellings<'a> {
    source: &'a Snapshot,
    destination: &'a Snapshot,
    /// Names this run made up (conflict copies), per side.
    learned: RefCell<HashMap<(Side, String), String>>,
}

impl<'a> Spellings<'a> {
    pub fn new(source: &'a Snapshot, destination: &'a Snapshot) -> Self {
        Self { source, destination, learned: RefCell::new(HashMap::new()) }
    }

    fn snapshot(&self, side: Side) -> &'a Snapshot {
        match side {
            Side::Source => self.source,
            Side::Destination => self.destination,
        }
    }

    /// The on-disk relative path of `key` on `side`. An item that's new there gets its
    /// folder as spelled on that side and its own name as spelled where it comes from (in
    /// the NFC form most systems write; Macs read either form).
    pub fn disk(&self, side: Side, key: &str) -> String {
        if let Some(disk) = self.learned.borrow().get(&(side, key.to_string())) {
            return disk.clone();
        }
        let here = self.snapshot(side);
        if here.contains(key) || here.has_spelling(key) {
            return here.spelling(key).to_string();
        }
        let name: String = Keys::name(self.snapshot(side.opposite()).spelling(key)).nfc().collect();
        match Keys::parent(key) {
            Some(parent) => format!("{}/{name}", self.disk(side, parent)),
            None => name,
        }
    }

    /// The key of an on-disk path, as the plan's keys are made.
    fn key_for(&self, disk: &str) -> String {
        self.destination.key_for(disk)
    }

    /// Remembers the spelling of an item this run created under a new name.
    fn learn(&self, side: Side, key: &str, disk: String) {
        self.learned.borrow_mut().insert((side, key.to_string()), disk);
    }
}

pub struct Executor<'a> {
    shores: &'a Shores,
    spellings: Spellings<'a>,
    /// Removed and replaced items are kept here (with safe deletes, and always for the
    /// losing side of a conflict).
    trash: Trash,
    care: Care,
    cancel: &'a AtomicBool,
    /// The folders the plan was made for, as found by its scans.
    roots: (Option<RootId>, Option<RootId>),
    /// The job's exclude filter, as the scans used it: a folder it excludes is never taken
    /// for one a step creates (see `not_left_alone`).
    filter: ExcludeFilter,
    /// Folders already checked to be real folders during this run.
    real_folders: RefCell<HashSet<PathBuf>>,
    /// Folders whose list of names this run changed (flushed to the drive at the end).
    touched: RefCell<BTreeSet<PathBuf>>,
    /// Folders this run removed.
    removed_folders: RefCell<HashSet<PathBuf>>,
    settled: RefCell<Vec<Settled>>,
    /// What each finished step left, for the last look before the run is recorded.
    footprints: RefCell<Vec<Footprint>>,
    destroyed: RefCell<(Vec<String>, Vec<String>)>,
    unkept: RefCell<Vec<(String, String)>>,
    /// What each side's drive answered when asked whether it holds links (see `link`).
    links: RefCell<HashMap<Side, Option<bool>>>,
    /// Items put back as they were by steps that couldn't finish (see `put_back`).
    put_back_items: RefCell<Vec<PutBack>>,
    /// Told the key of each item a step is about to make under a name the plan doesn't have
    /// (see `telling_new_names`).
    new_names: Option<&'a dyn Fn(&str)>,
}

impl<'a> Executor<'a> {
    /// An executor for a plan made from `source` and `destination` (whose spellings and
    /// drives it uses, and whose folders it insists on).
    pub fn new(shores: &'a Shores, source: &'a Snapshot, destination: &'a Snapshot, care: Care, cancel: &'a AtomicBool) -> Self {
        Self {
            shores,
            spellings: Spellings::new(source, destination),
            trash: Trash::for_run(Local::now()),
            care,
            cancel,
            roots: (source.root(), destination.root()),
            filter: ExcludeFilter::none(),
            real_folders: RefCell::new(HashSet::new()),
            touched: RefCell::new(BTreeSet::new()),
            removed_folders: RefCell::default(),
            settled: RefCell::new(Vec::new()),
            footprints: RefCell::default(),
            destroyed: RefCell::default(),
            unkept: RefCell::default(),
            links: RefCell::default(),
            put_back_items: RefCell::default(),
            new_names: None,
        }
    }

    /// The same executor, for a job whose scans left out what `filter` excludes (without
    /// one, only Charon's own items count as excluded).
    pub fn excluding(mut self, filter: ExcludeFilter) -> Self {
        self.filter = filter;
        self
    }

    /// The same executor, telling `hear` the key of each item a step makes under a name the
    /// plan doesn't have (a conflict copy), right before it makes it.
    pub fn telling_new_names(mut self, hear: &'a dyn Fn(&str)) -> Self {
        self.new_names = Some(hear);
        self
    }

    /// The side whose spelling of a step's path the person sees in the progress line: the
    /// side the item comes from, or the side it's removed from.
    fn shown_side(action: &Action) -> Side {
        match action {
            Action::Copy { to, .. } | Action::CreateDir { side: to, .. } => to.opposite(),
            Action::Delete { side, .. } => *side,
            _ => Side::Source,
        }
    }

    /// Runs every step of `plan` in order. The files of an app library or a database are
    /// carried together (see `carry`): when its first step other than a new folder comes up,
    /// or after the plan's copies when the plan removes something of it (see `starts_carry`).
    /// What it removes goes once every library and database carried by then has landed, right
    /// before the plan's first plain removal (see `remove_units`). Its other steps wait when
    /// it couldn't be carried.
    pub fn run(&self, plan: &Plan, meter: &mut ProgressMeter) -> Outcome {
        let mut outcome = Outcome::default();
        // A side where a copy didn't land keeps what this run would remove there that could
        // be the last copy of what didn't arrive.
        let mut holds = Holds::default();
        // The places cleared for an item of another kind: nothing goes into a place whose
        // item couldn't go first.
        let mut cleared: HashSet<Clearing> = HashSet::new();
        let units = self.units(plan);
        // The units carried so far, each with why its other steps wait, if they do.
        let mut carried: HashMap<&Unit, Option<String>> = HashMap::new();
        // What the units whose new versions all landed remove, waiting for the plan's first
        // plain removal.
        let mut removals: Vec<(&Unit, Leftover)> = Vec::new();
        // Whether the plan's plain removals have begun.
        let mut removing = false;
        for step in plan.ordered_steps() {
            let action = step.action;
            if self.cancel.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                break;
            }
            if matches!(action, Action::Conflict { .. }) {
                continue;
            }
            if let Err(reason) = self.check_roots() {
                outcome.stopped = Some(reason);
                break;
            }
            // The libraries and databases still to be carried go before the plan's first plain
            // removal: what it removes could be the other copy of what they bring (see
            // `starts_carry`). Then what each library and database removes goes.
            if matches!(action, Action::Delete { .. }) && step.clearing.is_none() && !removing {
                removing = true;
                let waiting: Vec<(&Unit, &UnitSteps)> = units.iter().filter(|(unit, steps)| steps.removes && !carried.contains_key(unit)).collect();
                if self.carry(&waiting, &mut carried, &mut removals, &mut outcome, &mut holds, meter).is_break()
                    || self.remove_units(&mut removals, &mut carried, &mut outcome, &mut holds, meter).is_break()
                {
                    break;
                }
            }
            let still_taken = step.clearing.filter(|clearing| step.takes_place() && !cleared.contains(clearing));
            let mut waits = None;
            if let Some((unit, steps)) = self.unit_of(action).and_then(|unit| units.get_key_value(&unit)) {
                if !carried.contains_key(unit)
                    && Self::starts_carry(&step, steps)
                    && self.carry(&[(unit, steps)], &mut carried, &mut removals, &mut outcome, &mut holds, meter).is_break()
                {
                    break;
                }
                if Self::carried_along(&step) {
                    continue;
                }
                waits = carried.get(unit).cloned().flatten();
            }
            meter.start(&self.spellings.disk(Self::shown_side(action), action.path()));
            let result = match (action, waits, still_taken, holds.of(action)) {
                (_, Some(why), _, _) => Ok(Done::Waits(why)),
                (_, None, Some(clearing), _) => Ok(Done::Blocked(format!(
                    "left for the next sync: the item of another kind in its place on the {} couldn't go first",
                    clearing.side.word()
                ))),
                (Action::Delete { side, .. }, None, None, Some(held)) => Ok(Done::Skipped(self.why_held(held, *side))),
                (Action::Copy { to, item: EntryKind::Link, .. }, None, None, _) if outcome.no_links.contains(to) => {
                    Ok(Done::LeftOut(format!("left out: the {}'s drive can't hold symbolic links", to.word())))
                }
                _ => self.apply(action, step.makes_room(), meter),
            };
            if let Some(clearing) = step.clearing.filter(|clearing| step.makes_room() && clearing.key == action.path()) {
                if matches!(result, Ok(Done::Deleted | Done::Nothing)) {
                    cleared.insert(clearing);
                }
            }
            if self.record(&mut outcome, &mut holds, action, result).is_break() {
                break;
            }
            meter.finish_item();
        }
        self.keep_asides(removals, &mut outcome, &mut holds, meter);
        meter.finishing();
        outcome.settled = self.settled.take();
        outcome.destroyed = self.destroyed.take();
        outcome.unkept = self.unkept.take();
        outcome.put_back = self.put_back_items.take();
        outcome
    }

    /// `Break`, with why in `outcome`, when the run was stopped or one of its folders went
    /// away.
    fn may_go_on(&self, outcome: &mut Outcome) -> ControlFlow<()> {
        if self.cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            return ControlFlow::Break(());
        }
        if let Err(reason) = self.check_roots() {
            outcome.stopped = Some(reason);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    /// Adds what one step did to the outcome, and what it holds back (see `Holds`). `Break`
    /// when the whole run stops there.
    fn record(&self, outcome: &mut Outcome, holds: &mut Holds, action: &Action, result: Result<Done>) -> ControlFlow<()> {
        holds.note(action, &result);
        match result {
            Ok(Done::Copied(files, bytes)) => {
                outcome.copied += files;
                outcome.bytes += bytes;
            }
            Ok(Done::Deleted) => outcome.deleted += 1,
            Ok(Done::Created | Done::Nothing) => {}
            Ok(Done::Skipped(reason) | Done::Blocked(reason)) => {
                log::info!("skipped {}: {reason}", action.path());
                outcome.skipped.push((action.path().to_string(), reason));
            }
            Ok(Done::Waits(reason)) => {
                log::info!("left {} for the next sync: {reason}", action.path());
                outcome.waited.push((action.path().to_string(), reason));
            }
            Ok(Done::LeftOut(reason)) => outcome.left_out.push((action.path().to_string(), reason)),
            Ok(Done::Kept) => outcome.kept.push(action.path().to_string()),
            Err(Error::Cancelled) => {
                outcome.cancelled = true;
                return ControlFlow::Break(());
            }
            Err(Error::LandedElsewhere(reason)) => {
                outcome.stopped = Some(reason);
                return ControlFlow::Break(());
            }
            Err(Error::Changed(reason)) => outcome.skipped.push((action.path().to_string(), reason)),
            Err(Error::NoLinks) => {
                if let Action::Copy { to, .. } = action {
                    if !outcome.no_links.contains(to) {
                        outcome.no_links.push(*to);
                    }
                    outcome.left_out.push((action.path().to_string(), format!("left out: the {}'s drive can't hold symbolic links", to.word())));
                }
            }
            Err(Error::Refused { path, why }) => {
                log::info!("{}: {} refused: {}", action.path(), path.display(), why.reason("drive"));
                let place = self.locate(action, &path);
                outcome.left_out.push((action.path().to_string(), self.refusal_reason(action.path(), place.as_ref(), why)));
                if let Some(refused) = place.and_then(|(side, key)| self.refused_item(side, key, why, path)) {
                    if !outcome.refused.contains(&refused) {
                        outcome.refused.push(refused);
                    }
                }
            }
            Err(Error::ReadDenied { path, reason, at }) => {
                log::info!("{}: couldn't read {}: {reason}", action.path(), path.display());
                // Like a failed copy, this keeps the run's removals on the receiving side (see
                // `Holds::note`): one could take the last copy there of what didn't arrive (a
                // file renamed, say). The looks after it keep them too, while the file can't be
                // read (see `plan::Ground::hold_removals`).
                match self.unread_item(action, &path, &reason, at) {
                    Some(unread) => {
                        if !outcome.unread.contains(&unread) {
                            outcome.unread.push(unread);
                        }
                    }
                    // A file the scan didn't see (a conflict copy this run named) has no place
                    // to be listed: the step failed.
                    None => outcome.failed.push((action.path().to_string(), Error::ReadDenied { path, reason, at }.to_string())),
                }
            }
            Err(e) => {
                log::warn!("{}: {e}", action.path());
                outcome.failed.push((action.path().to_string(), e.to_string()));
            }
        }
        ControlFlow::Continue(())
    }

    /// `record` for each of `results`, every one of them even when one stops the whole run
    /// (then this is `Break`).
    fn record_all(&self, outcome: &mut Outcome, holds: &mut Holds, results: Vec<(&Action, Result<Done>)>) -> ControlFlow<()> {
        let mut flow = ControlFlow::Continue(());
        for (step, result) in results {
            if self.record(outcome, holds, step, result).is_break() {
                flow = ControlFlow::Break(());
            }
        }
        flow
    }

    /// What the person reads about a removal from `side` that waits (see `Holds`).
    fn why_held(&self, held: Held, side: Side) -> String {
        let Held::OldCopyOf(key, why) = held else {
            return format!("kept for now: some items couldn't be copied to the {} in this run", side.word());
        };
        let (copy, side) = (self.spellings.disk(side.opposite(), key), side.word());
        match why {
            Unlanded::Changed => format!("kept for now: it may be the old copy of “{copy}”, which changed since the scan and wasn't copied to the {side} yet"),
            Unlanded::Blocked => {
                format!("kept for now: it may be the old copy of “{copy}”, which wasn't copied to the {side} yet: the item of another kind in its place there couldn't go first")
            }
            Unlanded::Waits => format!("kept for now: it may be the old copy of “{copy}”, which waits for the next sync with the rest of its app library or database"),
        }
    }

    /// The sides a step brings items to.
    fn receiving(action: &Action) -> Vec<Side> {
        match action {
            Action::Copy { to, .. } => vec![*to],
            Action::CreateDir { side, .. } => vec![*side],
            Action::KeepBoth { .. } => vec![Side::Source, Side::Destination],
            Action::Delete { .. } | Action::Conflict { .. } => Vec::new(),
        }
    }

    /// The side and key of `path`, something `action` tried to create: its own item, or a
    /// folder on the way to it.
    fn locate(&self, action: &Action, path: &Path) -> Option<(Side, String)> {
        Self::receiving(action).into_iter().find_map(|side| {
            let relative = Scanner::relative(self.shores.root(side), path)?;
            Some((side, self.spellings.key_for(&relative)))
        })
    }

    /// What the person reads about the step of `key` when a drive refused `place` (`why`).
    fn refusal_reason(&self, key: &str, place: Option<&(Side, String)>, why: Refusal) -> String {
        match place {
            Some((side, refused)) if refused != key => format!(
                "left out: “{}” can't be created on the {}, because {}",
                self.spellings.disk(*side, refused),
                side.word(),
                why.reason(&self.drive(*side))
            ),
            Some((side, _)) => format!("left out: {}", why.reason(&self.drive(*side))),
            None => format!("left out: {}", why.reason("drive")),
        }
    }

    /// The record of the file at `path` that `action` couldn't read on a side it copies
    /// from, where reading stopped at `at` (`None` for a file the scan didn't see, or one on
    /// a side the step copies to).
    fn unread_item(&self, action: &Action, path: &Path, reason: &str, at: u64) -> Option<Unread> {
        Self::receiving(action).into_iter().map(Side::opposite).find_map(|side| {
            let relative = Scanner::relative(self.shores.root(side), path)?;
            let key = self.spellings.key_for(&relative);
            self.spellings.snapshot(side).contains(&key).then(|| Unread { side, key, reason: reason.to_string(), at })
        })
    }

    /// The record of an item `side`'s drive refused at `at`, with the item as the other side
    /// has it (`None` for a name this run made up, like a conflict copy's).
    fn refused_item(&self, side: Side, key: String, why: Refusal, at: PathBuf) -> Option<Refused> {
        let item = *self.spellings.snapshot(side.opposite()).get(&key)?;
        Some(Refused { side, key, why, item, at })
    }

    /// `side`'s drive as the person knows it: "Backup SSD (exFAT)".
    fn drive(&self, side: Side) -> String {
        self.spellings.snapshot(side).volume().map(Volume::describe).unwrap_or_else(|| format!("{}'s drive", side.word()))
    }

    // ---------------------------------------------------------------- app libraries and databases

    /// The app library or database a step's item is carried with, if any (see `Unit`). A
    /// library the scan didn't find at all on the side the step changes has no version there
    /// its files could be mixed with: they go one by one, like any others, and only a
    /// database in it goes with its journals.
    fn unit_of(&self, action: &Action) -> Option<Unit> {
        let (source, destination) = (self.spellings.snapshot(Side::Source), self.spellings.snapshot(Side::Destination));
        match Unit::of(action.path(), source, destination)? {
            Unit::Package(library) if self.absent_where_changed(&library, action) => Unit::database_of(action.path(), source, destination).map(Unit::Database),
            unit => Some(unit),
        }
    }

    /// Whether the scans found nothing at `key` on each side `action` changes.
    fn absent_where_changed(&self, key: &str, action: &Action) -> bool {
        let changed: Vec<Side> = [Side::Source, Side::Destination].into_iter().filter(|side| action.changes(*side)).collect();
        !changed.is_empty() && changed.iter().all(|side| !self.spellings.snapshot(*side).contains(key))
    }

    /// The app libraries and databases this plan changes, each with its steps.
    fn units<'p>(&self, plan: &'p Plan) -> BTreeMap<Unit, UnitSteps<'p>> {
        let mut units: BTreeMap<Unit, UnitSteps<'p>> = BTreeMap::new();
        for step in plan.ordered_steps() {
            let action = step.action;
            if matches!(action, Action::Conflict { .. }) {
                continue;
            }
            let Some(unit) = self.unit_of(action) else { continue };
            let steps = units.entry(unit.clone()).or_default();
            match action {
                Action::CreateDir { side, path } => {
                    steps.made.insert((*side, path.as_str()));
                }
                Action::Copy { to, path, .. } => steps.made.extend(Keys::ancestors(path).filter(|folder| unit.holds(folder)).map(|folder| (*to, folder))),
                _ => {}
            }
            steps.sides.extend(Self::receiving(action));
            if let Action::Delete { side, .. } = action {
                steps.sides.insert(*side);
            }
            steps.removes |= matches!(action, Action::Delete { .. }) || step.clearing.is_some();
            if Self::carried_along(&step) {
                steps.carried.push(action);
            }
        }
        units
    }

    /// Whether a step of an app library or database is carried with the rest of it (see
    /// `carry`): a copy of a file or link, or the removal of one, that doesn't clear or fill
    /// a place for an item of another kind. Its new folders are made before, and the folders
    /// it no longer has are removed after, each in its own time.
    fn carried_along(step: &Step) -> bool {
        step.clearing.is_none()
            && matches!(
                step.action,
                Action::Copy { item: EntryKind::File | EntryKind::Link, .. } | Action::Delete { item: EntryKind::File | EntryKind::Link, other: None, .. }
            )
    }

    /// Whether an app library or database is carried when this step of it comes up. One the
    /// plan only copies into is carried at its first step but a plain new folder, which its
    /// files may need first. One the plan removes something of, or clears a place in, waits
    /// for every plain copy of the plan, like any removal (see `Plan::ordered`): it's carried
    /// at its first removal or step in a cleared place, or before the plan's first plain
    /// removal if that comes sooner (see `run`). So a copy that failed or changed holds back
    /// what it removes (see `Holds`), a run stopped before then leaves it as it was, and what
    /// it brings still lands before anything else goes that could be its other copy.
    fn starts_carry(step: &Step, steps: &UnitSteps) -> bool {
        if steps.removes {
            matches!(step.action, Action::Delete { .. }) || step.clearing.is_some()
        } else {
            !matches!(step.action, Action::CreateDir { .. })
        }
    }

    /// Carries the app libraries and databases of `batch` (see `Unit`): a mix of two versions
    /// may not open, or be quietly damaged (a database's journals only fit the version they
    /// were written with). First every new version of each is written next to its target (see
    /// `prepare`). That can take hours for a big library, and another program may write to
    /// one meanwhile, so each is checked right before its new versions take their places (see
    /// `still_ready` and `commit_unit`). When there are several, each is also checked once all
    /// are written: what couldn't be written, or changed, is then noted before any of them
    /// takes its place, and holds back what the others remove, so one whose removal waits
    /// waits whole. What each removes waits in `removals` for every library and database the
    /// plan changes (see `remove_units`). If any file of one can't be carried, or anything of
    /// it changed meanwhile, none is but what only adds to it (see `let_additions_land`), and
    /// the next sync tries again.
    ///
    /// Notes in `carried` why the other steps of each wait, if they do, and adds what each
    /// carried step did to the outcome. `Break` when the whole run stops there: what wasn't
    /// carried by then stays as it was.
    fn carry<'u, 'p>(
        &self,
        batch: &[(&'u Unit, &'u UnitSteps<'p>)],
        carried: &mut HashMap<&'u Unit, Option<String>>,
        removals: &mut Vec<(&'u Unit, Leftover<'p>)>,
        outcome: &mut Outcome,
        holds: &mut Holds,
        meter: &mut ProgressMeter,
    ) -> ControlFlow<()> {
        let mut prepared: Vec<Prepared<'u, 'p>> = Vec::new();
        for &(unit, steps) in batch {
            if self.may_go_on(outcome).is_break() {
                return Self::stop_with(prepared);
            }
            match self.prepare(unit, steps, holds, meter) {
                Ok(ready) => prepared.push(ready),
                Err((results, why)) => {
                    carried.insert(unit, why);
                    if self.record_all(outcome, holds, results).is_break() {
                        return Self::stop_with(prepared);
                    }
                }
            }
        }
        if prepared.len() > 1 {
            // Each one written before the others may have changed while they were.
            let mut checked = Vec::new();
            let mut unchecked = prepared.into_iter();
            while let Some(ready) = unchecked.next() {
                match self.still_ready(&ready, holds) {
                    Ok(()) => checked.push(ready),
                    Err(whole) => {
                        if self.wait_whole(ready, whole, carried, outcome, holds, meter).is_break() {
                            return Self::stop_with(checked.into_iter().chain(unchecked));
                        }
                    }
                }
            }
            prepared = checked;
            // What couldn't be written or changed meanwhile may be what another one removes
            // (see `Holds`): that one waits whole, and what it doesn't bring holds back in turn
            // what could be its old copy.
            loop {
                let (held, ready): (Vec<_>, Vec<_>) = prepared.into_iter().partition(|ready| ready.removals.iter().any(|step| holds.of(step).is_some()));
                prepared = ready;
                if held.is_empty() {
                    break;
                }
                let mut held = held.into_iter();
                while let Some(ready) = held.next() {
                    let whole = self.held_whole(ready.unit, &ready.steps.carried, holds);
                    if self.wait_whole(ready, whole, carried, outcome, holds, meter).is_break() {
                        return Self::stop_with(held.chain(prepared));
                    }
                }
            }
        }
        let mut prepared = prepared.into_iter();
        while let Some(ready) = prepared.next() {
            if self.may_go_on(outcome).is_break() {
                return Self::stop_with(std::iter::once(ready).chain(prepared));
            }
            // Right before its new versions take their places: the ones before took time too.
            if let Err(whole) = self.still_ready(&ready, holds) {
                if self.wait_whole(ready, whole, carried, outcome, holds, meter).is_break() {
                    return Self::stop_with(prepared);
                }
                continue;
            }
            let unit = ready.unit;
            let ((results, why), left) = self.commit_unit(ready, meter);
            carried.insert(unit, why);
            if !left.is_empty() {
                removals.push((unit, left));
            }
            if self.record_all(outcome, holds, results).is_break() {
                return Self::stop_with(prepared);
            }
        }
        ControlFlow::Continue(())
    }

    /// Throws away the new versions written for the libraries and databases in `prepared`,
    /// which won't take their places: the run stops.
    fn stop_with<'u, 'p: 'u>(prepared: impl IntoIterator<Item = Prepared<'u, 'p>>) -> ControlFlow<()> {
        for ready in prepared {
            Self::discard_all(ready.staged);
        }
        ControlFlow::Break(())
    }

    /// Notes that the library or database of `ready` waits whole, with what each of its
    /// carried steps did and why its other steps wait (`whole`), but for what only adds to it
    /// (see `let_additions_land`). `Break` when the whole run stops there.
    fn wait_whole<'u, 'p>(
        &self,
        ready: Prepared<'u, 'p>,
        whole: Carried<'p>,
        carried: &mut HashMap<&'u Unit, Option<String>>,
        outcome: &mut Outcome,
        holds: &mut Holds,
        meter: &mut ProgressMeter,
    ) -> ControlFlow<()> {
        let (results, why) = self.let_additions_land(whole, ready.staged, meter);
        carried.insert(ready.unit, why);
        self.record_all(outcome, holds, results)
    }

    /// What each carried step of an app library that doesn't land whole did, and why its
    /// other steps wait (`waiting`), once what only adds to it is carried on its own after all
    /// (see `adds_to_library`): the version there goes on as it was, with one more item it
    /// doesn't use. So the new photos of a library its app keeps writing to land while the
    /// rest of it waits. A new version already written next to its target (in `staged`) takes
    /// its place if its original is still as it was copied; one that wasn't written yet is
    /// copied like any file. The other new versions in `staged` are thrown away, and so is
    /// everything once the run stops (see `stops_run`).
    fn let_additions_land<'p>(&self, waiting: Carried<'p>, staged: Vec<(&'p Action, Staged)>, meter: &mut ProgressMeter) -> Carried<'p> {
        let (results, why) = waiting;
        let mut written: HashMap<(Side, String), Staged> = staged.into_iter().map(|(_, ready)| ((ready.to, ready.key.clone()), ready)).collect();
        let mut going_on = !results.iter().any(|(_, result)| Self::stops_run(result));
        let mut carried = Vec::with_capacity(results.len());
        for (step, result) in results {
            let ready = match step {
                Action::Copy { to, path, .. } => written.remove(&(*to, path.clone())),
                _ => None,
            };
            let adds = matches!(result, Ok(Done::Waits(_))) && self.adds_to_library(step);
            going_on = going_on && !(adds && self.stopping());
            let result = if adds && going_on {
                let added = self.add_on_its_own(step, ready, meter);
                going_on = !Self::stops_run(&added);
                added
            } else {
                if let Some(ready) = ready {
                    Copier::discard(&ready.partial);
                }
                result
            };
            carried.push((step, result));
        }
        for ready in written.into_values() {
            Copier::discard(&ready.partial);
        }
        (carried, why)
    }

    /// Whether a step's result stops the whole run (see `record`): it was stopped, or a drive
    /// went away.
    fn stops_run(result: &Result<Done>) -> bool {
        matches!(result, Err(Error::Cancelled | Error::LandedElsewhere(_)))
    }

    /// Whether the run was stopped, or one of its folders went away (see `may_go_on`).
    fn stopping(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.check_roots().is_err()
    }

    /// Whether `step`, a carried step of an app library or database, only adds a file or link
    /// to the library where the scan saw nothing, and isn't a file of a database (see
    /// `Unit::database_of`): a database's files only go together.
    fn adds_to_library(&self, step: &Action) -> bool {
        let (source, destination) = (self.spellings.snapshot(Side::Source), self.spellings.snapshot(Side::Destination));
        matches!(step, Action::Copy { existing: None, .. }) && Unit::database_of(step.path(), source, destination).is_none()
    }

    /// Carries `step`, a copy that only adds to its app library, on its own (see
    /// `let_additions_land`): `ready`, its new version already written next to its target,
    /// takes its place if the original is still as it was copied; without one it's copied
    /// like any file.
    fn add_on_its_own(&self, step: &Action, ready: Option<Staged>, meter: &mut ProgressMeter) -> Result<Done> {
        let Some(ready) = ready else {
            meter.start(&self.spellings.disk(Self::shown_side(step), step.path()));
            let result = self.apply(step, false, meter);
            meter.finish_item();
            return result;
        };
        let (to, key, from) = (ready.to, ready.key.clone(), ready.from);
        let original = self.look_fresh(to.opposite(), &self.path(to.opposite(), &key));
        if !matches!(&original, Ok(now) if Entry::still(now.as_ref(), Some(&from))) {
            Copier::discard(&ready.partial);
            original?;
            return Ok(Done::Skipped(format!("changed on the {} side since the scan", to.opposite().word())));
        }
        match self.commit(ready)? {
            Some((bytes, landed)) => {
                self.settle_copy(to, &key, from, landed, None);
                Ok(Done::Copied(1, bytes))
            }
            None => Ok(Done::Skipped(format!("changed on the {} side since the scan", to.word()))),
        }
    }

    /// The first half of carrying `unit` (see `carry`): every new version of it written next
    /// to its target, unless a removal of it waits already (see `Holds`). `Err` with what
    /// each of its carried steps did, and why its other steps wait, when it waits whole (but
    /// for what only adds to it, see `let_additions_land`).
    fn prepare<'u, 'p>(&self, unit: &'u Unit, steps: &'u UnitSteps<'p>, holds: &Holds, meter: &mut ProgressMeter) -> std::result::Result<Prepared<'u, 'p>, Carried<'p>> {
        let carried = &steps.carried;
        let removals: Vec<&'p Action> = carried.iter().copied().filter(|step| matches!(step, Action::Delete { .. })).collect();
        if removals.iter().any(|step| holds.of(step).is_some()) {
            return Err(self.let_additions_land(self.held_whole(unit, carried, holds), Vec::new(), meter));
        }
        let mut staged: Vec<(&'p Action, Staged)> = Vec::new();
        for step in carried {
            let Action::Copy { to, path, from, existing, settles_conflict, .. } = step else { continue };
            meter.start(&self.spellings.disk(to.opposite(), path));
            let result = self.stage(*to, path, from, existing.as_ref(), *settles_conflict, meter);
            meter.finish_item();
            match result {
                Ok(Some(ready)) => staged.push((*step, ready)),
                failed => {
                    let failed = failed.map(|_| Done::Skipped(format!("changed on the {} side since the scan", to.word())));
                    let not_mixed = self.waiting(unit, Wait::NotMixed);
                    let waiting = (Self::one_failed(carried, step, failed, &not_mixed), Some(not_mixed));
                    return Err(self.let_additions_land(waiting, staged, meter));
                }
            }
        }
        Ok(Prepared { unit, steps, staged, removals })
    }

    /// Whether the new versions of `ready` may take their places now (see `carry`): none of
    /// its removals waits (see `Holds`), and nothing of it changed since they were written
    /// (see `as_staged`). `Err` with what each of its carried steps did, and why its other
    /// steps wait, when it waits whole.
    fn still_ready<'p>(&self, ready: &Prepared<'_, 'p>, holds: &Holds) -> std::result::Result<(), Carried<'p>> {
        let (unit, carried) = (ready.unit, &ready.steps.carried);
        if ready.removals.iter().any(|step| holds.of(step).is_some()) {
            return Err(self.held_whole(unit, carried, holds));
        }
        match self.as_staged(ready) {
            Ok(true) => Ok(()),
            Ok(false) => {
                let why = self.waiting(unit, Wait::Changed);
                Err((carried.iter().map(|step| (*step, Ok(Done::Waits(why.clone())))).collect(), Some(why)))
            }
            Err(e) => {
                let not_mixed = self.waiting(unit, Wait::NotMixed);
                let results = carried.first().map(|first| Self::one_failed(carried, first, Err(e), &not_mixed)).unwrap_or_default();
                Err((results, Some(not_mixed)))
            }
        }
    }

    /// Whether nothing of the library or database of `ready` changed since its new versions
    /// were written next to their targets: each original still as it was copied, each target
    /// still the version it replaces (or nothing), each item it removes still as the plan saw
    /// it with nothing in its place on the other side, and nothing else of it either, on each
    /// side the plan changes it on: no other item, and nothing added (see `unit_unchanged`).
    /// Its app, or another computer's sync, may have written to it meanwhile. `Err` only when
    /// the run is stopped meanwhile.
    fn as_staged(&self, ready: &Prepared) -> Result<bool> {
        let copies = ready.staged.iter().all(|(_, staged)| {
            let source = self.path(staged.to.opposite(), &staged.key);
            matches!(self.look_fresh(staged.to.opposite(), &source), Ok(now) if Entry::still(now.as_ref(), Some(&staged.from)))
                && matches!(self.look_fresh(staged.to, &staged.target), Ok(now) if Entry::still(now.as_ref(), staged.existing.as_ref()))
        });
        let removals = ready.removals.iter().all(|step| match step {
            Action::Delete { side, path, entry, .. } => {
                let (here, there) = (self.path(*side, path), self.path(side.opposite(), path));
                matches!(self.look_fresh(*side, &here), Ok(now) if Entry::still(now.as_ref(), Some(entry)))
                    && matches!(self.look_fresh(side.opposite(), &there), Ok(None))
            }
            _ => true,
        });
        Ok(copies && removals && self.unit_unchanged(ready.unit, ready.steps)?)
    }

    /// The second half of carrying a unit (see `carry`): its new versions take their places,
    /// one right after the other, once the trash has room for the versions they replace (see
    /// `room_for_replaced`), so that one almost never fails halfway. Nothing of it is
    /// removed before then, but the old journals of its databases are renamed aside first:
    /// they only fit the version they were written with (see `set_aside`). When one can't be
    /// renamed aside, nothing of the unit moves but what only adds to it (see
    /// `let_additions_land`). When a new version can't take its place, the ones after it don't
    /// either, but for what only adds to it, and the journals of those databases are put back.
    ///
    /// Returns what each step did so far, why the unit's other steps wait, if they do, and,
    /// when every new version landed, what it still removes: that waits for every library
    /// and database the plan changes (see `remove_units`).
    fn commit_unit<'p>(&self, ready: Prepared<'_, 'p>, meter: &mut ProgressMeter) -> (Carried<'p>, Leftover<'p>) {
        let Prepared { unit, steps, staged, mut removals } = ready;
        let not_mixed = self.waiting(unit, Wait::NotMixed);
        if let Some((short, e)) = self.room_for_replaced(unit, &staged) {
            let failing = staged[short].0;
            let waiting = (Self::one_failed(&steps.carried, &failing, Err(e), &not_mixed), Some(not_mixed));
            return (self.let_additions_land(waiting, staged, meter), Leftover::default());
        }
        let journals = self.journals_of(&staged, &mut removals);
        let mut asides: Vec<Vec<Aside<'p>>> = Vec::new();
        for of_one in &journals {
            let mut set = Vec::new();
            for journal in of_one {
                match self.set_aside(journal) {
                    Ok(aside) => set.push(aside),
                    Err(e) => {
                        for aside in asides.into_iter().flatten().chain(set) {
                            self.put_back_aside(aside);
                        }
                        let waiting = (Self::one_failed(&steps.carried, journal, Err(e), &not_mixed), Some(not_mixed));
                        return (self.let_additions_land(waiting, staged, meter), Leftover::default());
                    }
                }
            }
            asides.push(set);
        }
        let mut results: Vec<(&'p Action, Result<Done>)> = Vec::new();
        // The old journals of the databases whose new versions landed.
        let mut behind: Vec<Aside<'p>> = Vec::new();
        // The new versions that wait, once one couldn't take its place.
        let mut unlanded: Vec<(&'p Action, Staged)> = Vec::new();
        let mut broken = false;
        for ((step, ready), set) in staged.into_iter().zip(asides) {
            let result = if broken {
                unlanded.push((step, ready));
                Ok(Done::Waits(not_mixed.clone()))
            } else {
                let (to, key, from, existing) = (ready.to, ready.key.clone(), ready.from, ready.existing);
                match self.commit(ready) {
                    Ok(Some((bytes, landed))) => {
                        self.settle_copy(to, &key, from, landed, existing);
                        Ok(Done::Copied(1, bytes))
                    }
                    Ok(None) => Ok(Done::Skipped(format!("changed on the {} side since the scan", to.word()))),
                    Err(e) => Err(e),
                }
            };
            let landed = matches!(result, Ok(Done::Copied(..)));
            results.push((step, result));
            if landed {
                behind.extend(set);
            } else {
                for aside in set {
                    let journal = aside.step;
                    self.put_back_aside(aside);
                    results.push((journal, Ok(Done::Waits(not_mixed.clone()))));
                }
            }
            broken |= !landed;
        }
        if broken {
            // Its removals wait, but the old journals of a database that landed can't go back
            // next to its new version: they're kept in the trash.
            results.extend(self.finish_asides(behind, |_| true, meter));
            results.extend(removals.into_iter().map(|step| (step, Ok(Done::Waits(not_mixed.clone())))));
            return (self.let_additions_land((results, Some(not_mixed)), unlanded, meter), Leftover::default());
        }
        ((results, None), Leftover { asides: behind, removals })
    }

    /// Why the trash on a side has no room for the versions the new versions in `staged`
    /// replace there, if it hasn't: the index of the new version that replaces the largest of
    /// them, and the error. Each is copied into the trash right before its new version takes
    /// its place (see `commit`), leaving the room Charon keeps for other apps. Where the old
    /// version gives its room back once removed, the largest copy is all that needs room at
    /// a time, with those of files that have other names (hard links) on top; where the
    /// drive may hold on to a removed file's data, every copy does; where the copies are
    /// clones, none does (see `KeptCopies`). The copy of a version on another drive (in a
    /// folder mounted inside the job's) is never a clone, and replacing that version gives
    /// its room back on that other drive only: each of those needs room of its own.
    fn room_for_replaced(&self, unit: &Unit, staged: &[(&Action, Staged)]) -> Option<(usize, Error)> {
        for side in [Side::Source, Side::Destination] {
            let copies = self.kept_copies(side);
            let gives_back = |target: &Path| copies == KeptCopies::OneAtATime && Identity::names(target) == Some(1);
            // Where every copy needs room of its own, near and far count alike: no need to ask.
            let near = |target: &Path| copies == KeptCopies::All || self.on_folders_drive(side, target);
            let kept = |ready: &Staged| {
                let old = ready.existing.filter(|old| ready.to == side && old.is_file() && self.keeps_replaced(ready.settles))?;
                Some((old.size, near(&ready.target)))
            };
            let replaced: Vec<(usize, (u64, bool))> = staged.iter().enumerate().filter_map(|(at, (_, ready))| Some((at, kept(ready)?))).collect();
            let nearby = replaced.iter().filter(|(_, (_, near))| *near).map(|&(at, (size, _))| (size, gives_back(&staged[at].1.target)));
            let far = replaced.iter().filter(|(_, (_, near))| !near).fold(0u64, |room, (_, (size, _))| room.saturating_add(*size));
            let bytes = copies.room(nearby).saturating_add(far);
            // The step blamed is the largest of the versions that take room: a clone takes none.
            let takes_room = |near: bool| copies != KeptCopies::Clones || !near;
            let Some(&(largest, _)) = replaced.iter().filter(|(_, (_, near))| takes_room(*near)).max_by_key(|(_, (size, _))| *size).filter(|_| bytes > 0) else {
                continue;
            };
            let what = format!("the replaced versions of “{}”", self.spellings.disk(side, unit.key()));
            if let Err(e) = self.room_to_keep(side, bytes, &what) {
                return Some((largest, e));
            }
        }
        None
    }

    /// Takes out of `removals` the old journals of each database whose new version is in
    /// `staged`, on the side it goes to: a list for each new version, in the order of
    /// `staged`. Each removal is looked at once, so a library with thousands of new and
    /// removed files takes its place in moments.
    fn journals_of<'p>(&self, staged: &[(&'p Action, Staged)], removals: &mut Vec<&'p Action>) -> Vec<Vec<&'p Action>> {
        let (source, destination) = (self.spellings.snapshot(Side::Source), self.spellings.snapshot(Side::Destination));
        let databases: HashMap<(Side, &str), usize> = staged.iter().enumerate().map(|(at, (_, ready))| ((ready.to, ready.key.as_str()), at)).collect();
        let mut journals: Vec<Vec<&'p Action>> = vec![Vec::new(); staged.len()];
        removals.retain(|step| {
            let Action::Delete { side, path, .. } = step else { return true };
            match Unit::journal_of(path, source, destination).and_then(|database| databases.get(&(*side, database.as_str())).copied()) {
                Some(at) => {
                    journals[at].push(*step);
                    false
                }
                None => true,
            }
        });
        journals
    }

    /// Renames the old journal of a database that `step` removes to a temporary name next to
    /// it, if it's still what the plan saw and the other side still has nothing there: the
    /// database's new version then takes its place without ever meeting it. A journal gone
    /// already is noted as such (what the plan wanted). One that changed fails with
    /// `Error::Changed`.
    fn set_aside<'p>(&self, step: &'p Action) -> Result<Aside<'p>> {
        let Action::Delete { side, path, entry, .. } = step else {
            return Err(Error::Invalid(format!("{} isn't a removal", step.path())));
        };
        let (side, planned) = (*side, *entry);
        if self.look_fresh(side.opposite(), &self.path(side.opposite(), path))?.is_some() {
            return Err(Error::Changed(format!("changed on the {} side since the scan", side.opposite().word())));
        }
        let relative = self.spellings.disk(side, path);
        let root = self.shores.root(side);
        let target = root.join(&relative);
        let mut aside = Aside { step, side, key: path, planned, relative, target, at: None };
        match self.parents(root, &aside.relative)? {
            Parents::Real => {}
            Parents::Missing => return Ok(aside),
            Parents::NotFolders => return Err(Error::Changed("a folder above it was replaced since the scan".into())),
        }
        Copier::make_writable(&aside.target);
        let Some(at) = Self::move_aside(&aside.target, root)? else { return Ok(aside) };
        self.touch(&aside.target);
        self.touch(&at);
        let moved = self.look_fresh(side, &at);
        if !matches!(&moved, Ok(Some(found)) if Self::same_after_rename(found, &planned)) {
            self.put_back(side, &at, &aside.target, &aside.relative, None);
            moved?;
            return Err(Error::Changed(format!("changed on the {} side since the scan", side.word())));
        }
        aside.at = Some(at);
        Ok(aside)
    }

    /// Finishes the removal of each old journal in `asides` (see `finish_aside`), the ones
    /// `keep` picks kept in the trash.
    fn finish_asides<'p>(&self, asides: Vec<Aside<'p>>, keep: impl Fn(&Action) -> bool, meter: &mut ProgressMeter) -> Vec<(&'p Action, Result<Done>)> {
        let mut results = Vec::new();
        for aside in asides {
            let step = aside.step;
            meter.start(&self.spellings.disk(aside.side, step.path()));
            let result = self.finish_aside(aside, keep(step));
            meter.finish_item();
            results.push((step, result));
        }
        results
    }

    /// Finishes the removal of an old journal set aside (see `set_aside`), once its
    /// database's new version took its place: into the trash with safe deletes or when
    /// `keep` (its removal waits, but it can't go back next to the new version), deleted for
    /// good otherwise. One that can't go, or that changed since it was set aside where it
    /// can only be copied into the trash (see `trash_aside`), stays under its temporary name,
    /// never next to the new database (a later sync moves it into the trash).
    fn finish_aside(&self, aside: Aside, keep: bool) -> Result<Done> {
        let Aside { side, key, planned, relative, target, at, .. } = aside;
        let Some(at) = at else {
            self.settle_removal(side, key, &planned);
            return Ok(Done::Nothing);
        };
        if self.care.safe_delete || keep {
            self.trash_aside(side, &at, &relative, Some(&planned), Self::removed(&planned))?;
        } else {
            Self::remove_item(&at).map_err(|e| Error::file(&target, e))?;
        }
        self.destroyed_on(side, key);
        self.settle_removal(side, key, &planned);
        Ok(Done::Deleted)
    }

    /// Puts an old journal set aside back where it was (see `set_aside`): its database's new
    /// version didn't take its place.
    fn put_back_aside(&self, aside: Aside) {
        if let Some(at) = &aside.at {
            self.put_back(aside.side, at, &aside.target, &aside.relative, Some(&aside.planned));
        }
    }

    /// Carries out what the libraries and databases in `removals` remove, once the new
    /// versions they bring all landed: after every library and database carried by then, so
    /// each removal is checked against all their results (see `Holds`). A file moved from one
    /// library into another keeps its old copy when its new one didn't land. When one removal
    /// of a library waits, they all do; once one couldn't be carried out, the ones after it
    /// wait. An old journal renamed aside can't wait next to the new version of its database:
    /// one whose removal waits is kept in the trash. `Break` when the whole run stops there,
    /// with what wasn't removed by then left in `removals`.
    fn remove_units<'u, 'p>(
        &self,
        removals: &mut Vec<(&'u Unit, Leftover<'p>)>,
        carried: &mut HashMap<&'u Unit, Option<String>>,
        outcome: &mut Outcome,
        holds: &mut Holds,
        meter: &mut ProgressMeter,
    ) -> ControlFlow<()> {
        while !removals.is_empty() {
            self.may_go_on(outcome)?;
            let (unit, Leftover { asides, removals: steps }) = removals.remove(0);
            let mut results = self.finish_asides(asides, |step| holds.of(step).is_some(), meter);
            let (rest, why) = if steps.iter().any(|step| holds.of(step).is_some()) {
                self.held_whole(unit, &steps, holds)
            } else {
                self.remove_unit(unit, &steps, meter)
            };
            results.extend(rest);
            carried.insert(unit, why);
            self.record_all(outcome, holds, results)?;
        }
        ControlFlow::Continue(())
    }

    /// Keeps in the trash the old journals renamed aside for the databases in `removals`,
    /// when the run stopped before what those libraries and databases remove went (see
    /// `remove_units`): they can't go back next to the new versions. Their other removals
    /// wait for the next sync.
    fn keep_asides(&self, removals: Vec<(&Unit, Leftover)>, outcome: &mut Outcome, holds: &mut Holds, meter: &mut ProgressMeter) {
        for (_, left) in removals {
            for (step, result) in self.finish_asides(left.asides, |_| true, meter) {
                // The run stops anyway.
                let _ = self.record(outcome, holds, step, result);
            }
        }
    }

    /// Removes what `unit` no longer has, one item after the other: once one couldn't go, the
    /// others wait.
    fn remove_unit<'p>(&self, unit: &Unit, steps: &[&'p Action], meter: &mut ProgressMeter) -> Carried<'p> {
        let not_mixed = self.waiting(unit, Wait::NotMixed);
        let mut results: Vec<(&'p Action, Result<Done>)> = Vec::new();
        let mut broken = false;
        for step in steps {
            if broken {
                results.push((*step, Ok(Done::Waits(not_mixed.clone()))));
                continue;
            }
            meter.start(&self.spellings.disk(Self::shown_side(step), step.path()));
            let result = self.apply(step, false, meter);
            meter.finish_item();
            broken |= !matches!(result, Ok(Done::Deleted | Done::Nothing));
            results.push((*step, result));
        }
        (results, broken.then_some(not_mixed))
    }

    /// Every one of `steps`, carried steps of `unit`, waiting because a removal among them
    /// waits (see `Holds`): that removal with its own reason, the others with it.
    fn held_whole<'p>(&self, unit: &Unit, steps: &[&'p Action], holds: &Holds) -> Carried<'p> {
        let why = self.waiting(unit, Wait::Held);
        let results = steps
            .iter()
            .map(|step| {
                let result = match (step, holds.of(step)) {
                    (Action::Delete { side, .. }, Some(held)) => Done::Skipped(self.why_held(held, *side)),
                    _ => Done::Waits(why.clone()),
                };
                (*step, Ok(result))
            })
            .collect();
        (results, Some(why))
    }

    /// Whether `unit` is still, on each side the plan changes it on, exactly what the scan
    /// saw there: every item of it as it was, and nothing in it but those, the folders this
    /// run makes there and the copies it is writing (and system clutter, and what Charon
    /// leaves alone). Another program, or another computer's sync, that wrote into it
    /// meanwhile would be left a mix of two versions. `Err` only when the run is stopped
    /// meanwhile; an item that can't be looked at counts as changed.
    fn unit_unchanged(&self, unit: &Unit, steps: &UnitSteps) -> Result<bool> {
        for &side in &steps.sides {
            let here = self.spellings.snapshot(side);
            let (items, folders): (Vec<(String, Option<&Entry>)>, BTreeSet<&str>) = match unit {
                Unit::Database(database) => (Unit::database_files(database).into_iter().map(|file| (file.clone(), here.get(&file))).collect(), BTreeSet::new()),
                Unit::Package(library) => {
                    let scanned: Vec<(&String, &Entry)> = here.get(library).map(|entry| (library, entry)).into_iter().chain(here.inside(library)).collect();
                    let made = steps.made.iter().filter(|(on, _)| *on == side).map(|(_, folder)| *folder);
                    let folders = scanned.iter().filter(|(_, entry)| entry.is_dir()).map(|(key, _)| key.as_str()).chain(made).collect();
                    let items = scanned.into_iter().filter(|(key, _)| !is_clutter(Keys::name(here.spelling(key))));
                    (items.map(|(key, entry)| (key.clone(), Some(entry))).collect(), folders)
                }
            };
            for (key, scanned) in &items {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let path = self.path(side, key);
                // A file the system won't let Charon open is looked at by its details alone.
                let now = match self.look_fresh(side, &path) {
                    Err(Error::ReadDenied { .. }) => Self::look(&path),
                    now => now,
                };
                if !matches!(now, Ok(now) if Entry::still(now.as_ref(), *scanned)) {
                    log::info!("{} changed since the scan, so its {} waits", path.display(), unit.describe());
                    return Ok(false);
                }
            }
            for folder in folders {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if !self.holds_nothing_new(side, folder, &steps.made) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Whether the folder `folder` of an app library on `side` holds only what the scan saw
    /// there, folders this run makes (`made`), system clutter, and items Charon leaves alone
    /// (the copies this run is writing among them). A folder that isn't there holds nothing
    /// new; one that can't be listed counts as holding something.
    fn holds_nothing_new(&self, side: Side, folder: &str, made: &BTreeSet<(Side, &str)>) -> bool {
        let here = self.spellings.snapshot(side);
        let disk = self.spellings.disk(side, folder);
        let path = self.shores.root(side).join(&disk);
        let items = match fs::read_dir(&path) {
            Ok(items) => items,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
            Err(e) => {
                log::info!("couldn't list {}: {e}", path.display());
                return false;
            }
        };
        for item in items {
            let Ok(item) = item else { return false };
            // A name that isn't valid Unicode was left out by the scan, and is never synced.
            let Some(name) = item.file_name().to_str().map(str::to_string) else { continue };
            let relative = format!("{disk}/{name}");
            let key = self.spellings.key_for(&relative);
            let known = here.contains(&key)
                || made.contains(&(side, key.as_str()))
                || is_clutter(&name)
                || here.leaves_alone(&key)
                || here.frozen(&key).is_some()
                || self.filter.excludes(&Keys::normalize(&relative));
            if !known {
                log::info!("{} is new since the scan", item.path().display());
                return false;
            }
        }
        true
    }

    /// What the person reads about a step of `unit` that waits, and why.
    fn waiting(&self, unit: &Unit, why: Wait) -> String {
        match unit {
            Unit::Database(_) => match why {
                Wait::Held => "kept for now: other copies didn't land in this run, and a database's files are only synced together".into(),
                Wait::Changed => "left for the next sync: a file of its database changed since the scan".into(),
                Wait::NotMixed => "left for the next sync: another file of its database couldn't be synced, and a database's files only go together".into(),
            },
            Unit::Package(library) => {
                let side = if self.spellings.snapshot(Side::Source).contains(library) { Side::Source } else { Side::Destination };
                let library = self.spellings.disk(side, library);
                match why {
                    Wait::Held => {
                        format!("kept for now: other copies didn't land in this run, and the files of “{library}”, an app library or document, are only synced together")
                    }
                    Wait::Changed => format!("left for the next sync: something in “{library}”, an app library or document, changed since the scan"),
                    Wait::NotMixed => {
                        format!("left for the next sync: another item of “{library}”, an app library or document, couldn't be synced, and its files only go together")
                    }
                }
            }
        }
    }

    /// `failing`'s own result, and every other carried step of its unit waiting (`why`; the
    /// failing one last).
    fn one_failed<'p>(steps: &[&'p Action], failing: &&'p Action, result: Result<Done>, why: &str) -> Vec<(&'p Action, Result<Done>)> {
        let mut results: Vec<(&Action, Result<Done>)> = steps
            .iter()
            .filter(|step| !std::ptr::eq(**step, *failing))
            .map(|step| (*step, Ok(Done::Waits(why.to_string()))))
            .collect();
        results.push((*failing, result));
        results
    }

    fn discard_all(staged: Vec<(&Action, Staged)>) {
        for (_, ready) in staged {
            Copier::discard(&ready.partial);
        }
    }

    /// Moves leftover half-copied files from interrupted runs into the trash, each only if
    /// it is still exactly what the scan saw. Returns how many were moved.
    pub fn tidy(&self, side: Side, leftovers: &[(String, Entry)]) -> u64 {
        let root = self.shores.root(side);
        let mut moved = 0;
        for (relative, seen) in leftovers {
            let path = root.join(relative);
            if !matches!(self.parents(root, relative), Ok(Parents::Real)) || !matches!(Self::look(&path), Ok(Some(now)) if now.unchanged(seen)) {
                continue;
            }
            match self.move_to_trash(side, relative, &path, seen) {
                Ok(Removal::Removed) => {
                    self.touch(&path);
                    moved += 1;
                }
                // Gone already: nothing was moved.
                Ok(Removal::Gone) => {}
                Ok(Removal::Changed) => log::info!("left the leftover {}: it changed as it went into the trash", path.display()),
                Err(e) => log::warn!("couldn't move the leftover {} into the trash: {e}", path.display()),
            }
        }
        moved
    }

    /// Makes everything this run changed durable: every folder it touched (and the trash's
    /// lists of what went in), then each drive. Then it looks once more at what the finished
    /// steps left (see `retrace`) and at both folders. Until all of this succeeds, nothing
    /// about the run may be recorded as done; `Error::Undone` says what was found undone.
    /// In a two-way job, an item whose original went away meanwhile is left out of what
    /// `outcome` records (see `Outcome::unrecorded`).
    pub fn flush(&self, outcome: &mut Outcome) -> Result<()> {
        self.trash.write_lists();
        self.touched.borrow_mut().extend(self.trash.folders());
        for folder in self.touched.borrow().iter() {
            match durable::sync_folder(folder) {
                Ok(()) => {}
                // A folder this run removed afterwards.
                Err(e) if e.kind() == io::ErrorKind::NotFound && self.removed_folders.borrow().contains(folder) => {}
                // Any other folder this run changed must still be there: one that's gone took
                // the run's changes inside it along.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(Self::undone(format!("The folder {} isn't there anymore, although this sync had just made changes in it.", folder.display())))
                }
                Err(e) => return Err(Error::file(folder, e)),
            }
        }
        self.roots_still_there()?;
        for side in [Side::Source, Side::Destination] {
            let root = self.shores.root(side);
            durable::sync_drive(root).map_err(|e| Error::file(root, e))?;
        }
        let unrecorded = self.retrace()?;
        // The look took a while: the folders must still be the same after it.
        self.roots_still_there()?;
        for step in outcome.settled.iter_mut().filter(|step| unrecorded.contains(&step.path)) {
            step.pair = None;
        }
        outcome.unrecorded = unrecorded.into_iter().collect();
        Ok(())
    }

    /// How long the client of a network share or of a drive run by a program may answer a
    /// look from the details it keeps of an item (an NFS client keeps them for up to a
    /// minute).
    const CACHED_DETAILS: Duration = Duration::from_secs(60);

    /// The last look before a run is recorded, once its changes were flushed to the drives:
    /// what each finished step left must still be there, and what it removed must still be
    /// gone. A drive pulled out and plugged in again during the run, or a network share whose
    /// server restarted, can come back under the same folder numbers without the run's last
    /// changes, which never reached it. Recorded as done, such a run would make the next sync
    /// take a copy that never arrived for an item removed since, and remove the original. So
    /// any of this fails the whole run, with `Error::Undone`: the drive may have lost more.
    ///
    /// In a two-way job the original a copy was made from must still be there too, as it was
    /// copied: a file whose drive came back without the person's last change to it would
    /// read as removed or changed on purpose, and the next sync would remove or replace the
    /// copy just made, the only one left. A person who moves, renames or removes a synced
    /// item during the run looks just the same, and that must not fail the run. So only such
    /// an item is left out of the record (returned here): the next look compares it afresh
    /// and copies it back rather than remove it. Mirror and Backup keep no such memory, so
    /// there the originals aren't looked at.
    ///
    /// Only what a lost change leaves counts (see `Trace::lost`). And only the items' details
    /// are read, one look per item and side (see `look_again`), so the look stays quick for
    /// runs of many files, on network shares too. A stop asked for meanwhile doesn't cut it
    /// short: a run that wasn't looked at to the end couldn't be recorded at all, and the
    /// next look would distrust the memory of the last sync.
    fn retrace(&self) -> Result<BTreeSet<String>> {
        let mut unrecorded = BTreeSet::new();
        for footprint in self.footprints.borrow().iter() {
            for (side, trace) in [(Side::Source, footprint.source), (Side::Destination, footprint.destination)] {
                match trace {
                    Trace::Untouched => {}
                    Trace::Original(_) if !self.care.two_way => {}
                    // An original that can't be seen can't be vouched for either.
                    Trace::Original(_) => {
                        if !matches!(self.look_again(side, footprint), Ok(now) if trace.lost(now.as_ref()).is_none()) {
                            unrecorded.insert(footprint.key.clone());
                        }
                    }
                    Trace::Left { .. } | Trace::Removed(_) => {
                        let now = self.look_again(side, footprint)?;
                        if let Some(loss) = trace.lost(now.as_ref()) {
                            return Err(self.undone_step(side, &footprint.key, loss));
                        }
                    }
                }
            }
        }
        Ok(unrecorded)
    }

    /// The item at a finished step's place on `side`, as it is now. Only its details are
    /// read: on a network share that's one question to the server. A file whose drive may
    /// still answer from what its client kept is opened instead, so the drive itself answers
    /// (see `look_fresh`): a server that lost the file wouldn't show it.
    fn look_again(&self, side: Side, footprint: &Footprint) -> Result<Option<Entry>> {
        let path = self.path(side, &footprint.key);
        if self.may_answer_from_cache(side, footprint.at.elapsed()) {
            self.look_fresh(side, &path)
        } else {
            Self::look(&path)
        }
    }

    /// Whether `side`'s drive may still answer a look at an item a step finished `ago` from
    /// what its client kept: a drive that may show changes late, and a step finished less
    /// than `CACHED_DETAILS` ago. Older items are asked for afresh anyway, so a long run on
    /// a network share opens only the files of its last minute.
    fn may_answer_from_cache(&self, side: Side, ago: Duration) -> bool {
        self.shows_changes_late(side) && ago < Self::CACHED_DETAILS
    }

    /// The error for a finished step on `side` found undone at the end of the run.
    fn undone_step(&self, side: Side, key: &str, loss: Loss) -> Error {
        let (item, place) = (self.spellings.disk(side, key), side.word());
        Self::undone(match loss {
            Loss::Missing => format!("“{item}” isn't on the {place} anymore, although this sync had just synced it."),
            Loss::Replaced => format!("“{item}” on the {place} is the version this sync replaced, back in place of the new one."),
            Loss::Older => format!("“{item}” on the {place} is a version older than the one this sync replaced, back in place of the new one."),
            Loss::Back => format!("“{item}”, which this sync removed from the {place}, is back."),
        })
    }

    /// The error for a change of this run found undone at its end; `what` says which. It may
    /// also be someone's doing during the run, which the message leaves open.
    fn undone(what: String) -> Error {
        Error::Undone(format!(
            "{what} If nobody did this on purpose, the drive may have been disconnected for a moment and come back without this sync's last changes."
        ))
    }

    /// Both folders must still be the ones the plan was made for.
    fn check_roots(&self) -> std::result::Result<(), String> {
        match self.moved_root() {
            Some(side) => Err(format!(
                "The {} folder went away or was replaced during the sync (was a drive disconnected?). Charon stopped there.",
                side.word()
            )),
            None => Ok(()),
        }
    }

    /// `check_roots` at the end of a run: a folder that went away may have taken the run's
    /// last changes along.
    fn roots_still_there(&self) -> Result<()> {
        match self.moved_root() {
            Some(side) => Err(Error::Undone(format!(
                "The {} folder went away or was replaced before this sync's changes were all written to its drive (was the drive disconnected?).",
                side.word()
            ))),
            None => Ok(()),
        }
    }

    /// The side whose folder isn't the one the plan was made for anymore, if there is one.
    fn moved_root(&self) -> Option<Side> {
        [(Side::Source, self.roots.0), (Side::Destination, self.roots.1)].into_iter().find_map(|(side, expected)| {
            let same = match (RootId::of(self.shores.root(side)), expected) {
                (Ok(Some(now)), Some(expected)) => now.same_folder(&expected),
                (Ok(_), _) => true,
                (Err(_), _) => false,
            };
            (!same).then_some(side)
        })
    }

    /// Carries out one step. `makes_room`: a removal that clears the way for an item of
    /// another kind.
    fn apply(&self, action: &Action, makes_room: bool, meter: &mut ProgressMeter) -> Result<Done> {
        match action {
            Action::CreateDir { side, path } => self.create_dir(*side, path),
            Action::Copy { to, path, from, existing, settles_conflict, .. } => {
                match self.copy(*to, path, from, existing.as_ref(), *settles_conflict, meter)? {
                    Some((bytes, landed)) => {
                        self.settle_copy(*to, path, *from, landed, *existing);
                        Ok(Done::Copied(1, bytes))
                    }
                    None => Ok(Done::Skipped(format!("changed on the {} side since the scan", to.word()))),
                }
            }
            Action::Delete { side, path, entry, other, .. } => self.delete(*side, path, entry, other.as_ref(), makes_room),
            Action::KeepBoth { path, source, destination, .. } => self.keep_both(path, source, destination, meter),
            Action::Conflict { .. } => Ok(Done::Nothing),
        }
    }

    // ---------------------------------------------------------------- the steps

    /// A folder, one level (its parents are checked or created first), as long as the
    /// other side still has it.
    fn create_dir(&self, side: Side, key: &str) -> Result<Done> {
        let there = self.path(side.opposite(), key);
        if !Self::look(&there)?.is_some_and(|e| e.is_dir()) {
            return Ok(Done::Skipped(format!("no longer a folder on the {} side", side.opposite().word())));
        }
        self.prepare_parents(side, key)?;
        self.ensure_folder(side, key)?;
        self.on_its_drive(side, &self.path(side, key))?;
        let traces = Self::on_sides(side, Trace::Left { item: Entry::dir(), replaced: None }, Trace::Original(Entry::dir()));
        self.settle(key, Some(Pair::new(Entry::dir(), Entry::dir())), traces);
        Ok(Done::Created)
    }

    /// Copies `from` (as planned) onto side `to`, over `existing` (as planned). Returns the
    /// bytes carried and the item as it landed, or `None` when the target changed since the
    /// scan. The replaced version goes to the trash with safe deletes, in two-way jobs, and
    /// always when the copy settles a conflict.
    fn copy(&self, to: Side, key: &str, from: &Entry, existing: Option<&Entry>, settles: bool, meter: &mut ProgressMeter) -> Result<Option<(u64, Entry)>> {
        match self.stage(to, key, from, existing, settles, meter)? {
            Some(staged) => self.commit(staged),
            None => Ok(None),
        }
    }

    /// The first half of a copy: the new version, written and checked next to its target.
    /// `None` when the target changed since the scan.
    fn stage(&self, to: Side, key: &str, from: &Entry, existing: Option<&Entry>, settles: bool, meter: &mut ProgressMeter) -> Result<Option<Staged>> {
        let from_path = self.path(to.opposite(), key);
        let relative = self.spellings.disk(to, key);
        let root = self.shores.root(to);
        let target = root.join(&relative);

        if !Entry::still(self.look_fresh(to.opposite(), &from_path)?.as_ref(), Some(from)) {
            return Err(Error::Changed(format!("changed on the {} side since the scan", to.opposite().word())));
        }
        if !Entry::still(Self::look(&target)?.as_ref(), existing) {
            // Something the plan didn't see is there: new since the scan, or the drive
            // taking this name for another item's.
            if existing.is_none() {
                self.not_taken(to, key)?;
            }
            return Ok(None);
        }
        self.prepare_parents(to, key)?;

        let (partial, unkept) = match from.kind {
            EntryKind::File => {
                self.room_for(to, from.size)?;
                let coarse = self.spellings.snapshot(to.opposite()).volume().is_some_and(|v| v.coarse_times());
                let mut copier = Copier::new(self.cancel).coarse_source_times(coarse).within(root);
                if !self.holds_attributes(to) {
                    copier = copier.without_attributes();
                }
                let written = copier.file(&from_path, &target, from, &mut |n| meter.add_bytes(n))?;
                (written.partial, written.unkept)
            }
            EntryKind::Link => (self.link(to, &from_path, &target, from)?, BTreeSet::new()),
            EntryKind::Dir => return Err(Error::Invalid("a folder can't be copied as a file".into())),
        };
        self.touch(&target);
        // Where the path next to the target was too long, the copy waits in a folder above.
        self.touch(&partial);
        // One last look, fresh from the drive: the target must still be what the plan saw.
        if !Entry::still(self.look_fresh(to, &target)?.as_ref(), existing) {
            Copier::discard(&partial);
            return Ok(None);
        }
        Ok(Some(Staged { to, key: key.to_string(), from: *from, existing: existing.copied(), settles, relative, target, partial, unkept }))
    }

    /// Makes the link for a copy onto `to` (see `Copier::link`). An answer that may only be
    /// about the link's own folder (one that can't be written to, or is locked) counts as
    /// the drive refusing links only when the drive refuses a link at the top of the job's
    /// folder too, where it takes a file. Otherwise the copy fails with what the drive said,
    /// and the drive is never taken for one that can't hold links.
    fn link(&self, to: Side, from_path: &Path, target: &Path, from: &Entry) -> Result<PathBuf> {
        let root = self.shores.root(to);
        let vague = self.spellings.snapshot(to).volume().is_some_and(Volume::answers_links_vaguely);
        match Copier::link(from_path, target, from, root) {
            Err(e) if Copier::may_refuse_links(&e, vague) && self.drive_holds_links(to) == Some(false) => Err(Error::NoLinks),
            other => other,
        }
    }

    /// Whether `side`'s drive holds links, asked once per run (see `Copier::drive_holds_links`).
    fn drive_holds_links(&self, side: Side) -> Option<bool> {
        *self.links.borrow_mut().entry(side).or_insert_with(|| Copier::drive_holds_links(self.shores.root(side)))
    }

    /// The second half of a copy: the new version takes the target's place. Returns the
    /// bytes carried and the item as it landed, or `None` when the target changed meanwhile.
    fn commit(&self, staged: Staged) -> Result<Option<(u64, Entry)>> {
        let Staged { to, key, from, existing, settles, relative, target, partial, unkept } = staged;
        let root = self.shores.root(to);
        let keep_old = self.keeps_replaced(settles);
        match existing {
            None => Copier::commit_new(&partial, &target)?,
            Some(old) if old.is_dir() => {
                Copier::discard(&partial);
                return Err(Error::Invalid(format!("a folder named {} is in the way", target.display())));
            }
            Some(old) if keep_old && old.is_file() => {
                // Keep a copy of the old version, then replace it in place (see `Trash`).
                // The old version may be only in the cloud since the scan: then it's left alone.
                let kept = match self.room_for_kept_copy(to, &target, old.size, "the replaced version").and_then(|()| self.trash.preserve(root, &relative)) {
                    Ok(kept) => kept,
                    Err(e) => {
                        Copier::discard(&partial);
                        return Err(e);
                    }
                };
                self.touch(&kept);
                let copy = Identity::at(&partial);
                match self.replace(to, &partial, &target, &relative, &old) {
                    Ok(true) => {}
                    Ok(false) => {
                        Copier::discard(&kept);
                        return Ok(None);
                    }
                    Err(e) => {
                        // With the old version still in its place, its copy in the trash goes:
                        // each sync that fails here (an app holds the file open) would leave
                        // one more.
                        if Self::still_in_place(&target, &old, copy) {
                            Copier::discard(&kept);
                        }
                        return Err(e);
                    }
                }
            }
            Some(old) if keep_old => {
                // A link: into the trash as it is, if it's still the one the plan saw. One
                // that's gone was removed a moment ago, and the new one doesn't take its place.
                match self.move_to_trash(to, &relative, &target, &old) {
                    Ok(Removal::Removed) => {}
                    Ok(Removal::Gone | Removal::Changed) => {
                        Copier::discard(&partial);
                        return Ok(None);
                    }
                    Err(e) => {
                        Copier::discard(&partial);
                        return Err(e);
                    }
                }
                Copier::commit_new(&partial, &target)?;
            }
            Some(old) => {
                if !self.replace(to, &partial, &target, &relative, &old)? {
                    return Ok(None);
                }
            }
        }
        if existing.is_some() {
            self.destroyed_on(to, &key);
        }
        let landed = Self::look(&target)?.ok_or_else(|| Error::Invalid(format!("{} vanished right after it was copied", target.display())))?;
        self.on_its_drive(to, &target)?;
        if !unkept.is_empty() {
            self.unkept.borrow_mut().push((key, unkept.into_iter().collect::<Vec<_>>().join(", ")));
        }
        Ok(Some((if from.is_file() { from.size } else { 0 }, landed)))
    }

    /// Puts the finished copy `partial` in the place of `target` (`relative` in `to`'s
    /// folder), which must still be `expected`. Returns `false`, with the copy thrown away,
    /// when it isn't: an app saved over it a moment ago.
    ///
    /// Where the drive can swap two names in one step, the copy and the old item swap, and
    /// the old item, now under the copy's temporary name, is checked: if it isn't the one
    /// that was checked a moment before (an app saved a new version at that very instant),
    /// the two swap back. Elsewhere the old item is renamed aside and checked first, and
    /// only then does the copy take the name, which it never takes from anything that
    /// appeared there meanwhile. Either way, nothing an app saved is ever written over: it's
    /// put back where it was, or kept in the trash.
    fn replace(&self, to: Side, partial: &Path, target: &Path, relative: &str, expected: &Entry) -> Result<bool> {
        // One last look, fresh from the drive, and which file it is.
        let checked = match self.look_fresh(to, target) {
            Ok(now) if Entry::still(now.as_ref(), Some(expected)) => fs::symlink_metadata(target).ok().filter(|meta| {
                Self::entry_of(target, meta).is_ok_and(|now| now.unchanged(expected))
            }),
            Ok(_) => None,
            Err(e) => {
                Copier::discard(partial);
                return Err(e);
            }
        };
        let Some(checked) = checked else {
            Copier::discard(partial);
            return Ok(false);
        };
        Copier::make_writable(target);
        let swapped = Swapped { checked: Identity::of(&checked), copy: Identity::at(partial) };
        match swap(partial, target) {
            Ok(()) => self.check_swapped(to, partial, target, relative, expected, swapped),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => self.replace_aside(to, partial, target, relative, expected),
            Err(e) => {
                Copier::discard(partial);
                Err(Error::file(target, e))
            }
        }
    }

    /// After a swap, `partial` holds what was at `target`: removed if it's what was checked
    /// (the same file, as it was), swapped back otherwise.
    fn check_swapped(&self, to: Side, partial: &Path, target: &Path, relative: &str, expected: &Entry, swapped: Swapped) -> Result<bool> {
        let swapped_out = fs::symlink_metadata(partial).ok();
        let same = swapped_out.as_ref().is_some_and(|meta| {
            swapped.checked.is_none_or(|checked| Identity::of(meta) == Some(checked))
                && Self::entry_of(partial, meta).is_ok_and(|found| Self::same_after_rename(&found, expected))
        });
        if same {
            if let Err(e) = Self::remove_item(partial) {
                // It stays under the temporary name, and the next sync moves it into the trash.
                log::warn!("couldn't remove the replaced version of {}: {e}", target.display());
            }
            return Ok(true);
        }
        log::info!("{} was saved again while Charon replaced it; that version stays", target.display());
        match swap(partial, target) {
            Ok(()) if swapped.copy.is_some() && Identity::at(partial) == swapped.copy => {
                Copier::discard(partial);
                Ok(false)
            }
            // Yet another version took the name in between: the one swapped out is kept too.
            Ok(()) => {
                self.keep_aside(to, partial, relative);
                Ok(false)
            }
            Err(e) => {
                let kept = if self.keep_aside(to, partial, relative) {
                    "that version is in .charon-trash".to_string()
                } else {
                    format!("that version stays under a temporary name, as {}", self.place_of(to, partial))
                };
                Err(Error::Changed(format!("an app saved a new version while Charon replaced it ({e}); {kept}")))
            }
        }
    }

    /// `replace` on a drive that can't swap names: the old item is renamed aside and checked,
    /// then the copy takes the free name.
    fn replace_aside(&self, to: Side, partial: &Path, target: &Path, relative: &str, expected: &Entry) -> Result<bool> {
        let aside = match Self::move_aside(target, self.shores.root(to)) {
            Ok(Some(aside)) => aside,
            // Removed meanwhile: not what the plan saw anymore.
            Ok(None) => {
                Copier::discard(partial);
                return Ok(false);
            }
            Err(e) => {
                Copier::discard(partial);
                return Err(e);
            }
        };
        let moved = self.look_fresh(to, &aside);
        if !matches!(&moved, Ok(Some(found)) if Self::same_after_rename(found, expected)) {
            Copier::discard(partial);
            self.put_back(to, &aside, target, relative, None);
            return moved.map(|_| false);
        }
        match rename_new(partial, target) {
            Ok(()) => {
                if let Err(e) = Self::remove_item(&aside) {
                    log::warn!("couldn't remove the replaced version of {}: {e}", target.display());
                }
                Ok(true)
            }
            Err(e) => {
                Copier::discard(partial);
                self.put_back(to, &aside, target, relative, Some(expected));
                if e.kind() == io::ErrorKind::AlreadyExists {
                    Ok(false)
                } else {
                    Err(Error::file(target, e))
                }
            }
        }
    }

    /// Renames the item at `path` to a fresh temporary name, and returns that name; `None`
    /// if nothing is there anymore. The name is next to the item, or in the nearest folder
    /// above, up to `top`, where its path isn't too long (see `Copier::temporary`). Whatever
    /// is at `path` at that instant moves, even a version an app saved a moment ago: the
    /// look after the move sees it.
    fn move_aside(path: &Path, top: &Path) -> Result<Option<PathBuf>> {
        match Copier::temporary(path, top, |aside| move_to_new_name(path, aside)) {
            Ok((aside, ())) => Ok(Some(aside)),
            Err((_, e)) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
            Err((_, e)) => Err(Error::file(path, e)),
        }
    }

    /// Puts an item Charon renamed aside back under its name, or, when something new took
    /// the name meanwhile, into the trash: never lost, never written over. `checked` is the
    /// item as the plan saw it, when the look after the move found it so: back in its place
    /// as it was, it's noted with the change time the renames gave it (see `PutBack`).
    fn put_back(&self, side: Side, aside: &Path, target: &Path, relative: &str, checked: Option<&Entry>) {
        if let Err(e) = rename_new(aside, target) {
            log::info!("couldn't put {} back ({e}); it goes into the trash", target.display());
            self.keep_aside(side, aside, relative);
            return;
        }
        let Some(checked) = checked else { return };
        if let Ok(Some(now)) = Self::look(target) {
            if Self::same_after_rename(&now, checked) {
                let item = PutBack { side, path: self.spellings.key_for(relative), found: *checked, now };
                self.put_back_items.borrow_mut().push(item);
            }
        }
    }

    /// Moves an item Charon renamed aside into the trash, under its own name (see
    /// `trash_aside`). Returns whether it's there: one that can't go stays under its
    /// temporary name, and a later sync moves it into the trash.
    fn keep_aside(&self, side: Side, aside: &Path, relative: &str) -> bool {
        match self.trash_aside(side, aside, relative, None, "the version set aside") {
            Ok(_) => true,
            Err(e) => {
                log::error!("couldn't move {} into the trash: {e}; it stays at {}", relative, aside.display());
                false
            }
        }
    }

    /// Moves the item Charon renamed aside to `aside` into the trash as `relative` (its own
    /// place in `side`'s folder), and returns where it is there. Where the trash is on another
    /// drive than the item (a folder mounted inside the job's), nothing moves there in one
    /// step: the item is copied into the trash instead, if the trash's drive has room for
    /// `what` ("the removed file"), and then removed, as long as it's still `expected` (as the
    /// plan saw it) or, without `expected`, as it was right before it was copied. One that
    /// changed meanwhile stays where it is, under its temporary name, and its copy goes. One
    /// that can't be removed stays too, and so does its copy: a drive may say a removal failed
    /// that it carried out (a network share), and then the copy is all that's left of it.
    fn trash_aside(&self, side: Side, aside: &Path, relative: &str, expected: Option<&Entry>, what: &str) -> Result<PathBuf> {
        let root = self.shores.root(side);
        match self.trash.put_from(aside, root, relative) {
            Ok(kept) => {
                self.touch(&kept);
                return Ok(kept);
            }
            Err(Error::File { source, .. }) if source.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e),
        }
        let expected = match expected {
            Some(expected) => *expected,
            None => self.look_fresh(side, aside)?.ok_or_else(|| Error::file(aside, io::Error::from(io::ErrorKind::NotFound)))?,
        };
        // On another drive than the trash: the copy is never a clone.
        self.room_to_keep(side, expected.size, what)?;
        let kept = self.trash.preserve_from(aside, root, relative)?;
        self.touch(&kept);
        let now = self.look_fresh(side, aside);
        match &now {
            Ok(Some(found)) if Self::same_after_rename(found, &expected) => {
                Self::remove_item(aside).map_err(|e| Error::file(aside, e))?;
                self.touch(aside);
            }
            // Gone meanwhile: its copy is all that's left of it.
            Ok(None) => {}
            _ => {
                Copier::discard(&kept);
                now?;
                return Err(Error::Changed(format!(
                    "changed on the {} side since the scan; it stays under a temporary name, as {}",
                    side.word(),
                    self.place_of(side, aside)
                )));
            }
        }
        Ok(kept)
    }

    /// Where `path` is in `side`'s folder, for the person: `/`-separated from the folder's
    /// top, or in full when it isn't inside.
    fn place_of(&self, side: Side, path: &Path) -> String {
        match path.strip_prefix(self.shores.root(side)) {
            Ok(inside) => inside.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"),
            Err(_) => path.display().to_string(),
        }
    }

    /// Whether the version `old` is at `target` after a replacement failed: it never left,
    /// or it was put back. Not when the new copy (`copy`) took the name after all, however
    /// much it looks like the old version: then the trash holds the only copy of that one.
    fn still_in_place(target: &Path, old: &Entry, copy: Option<Identity>) -> bool {
        let looks_old = matches!(Self::look(target), Ok(Some(now)) if Self::same_after_rename(&now, old));
        looks_old && (copy.is_none() || Identity::at(target) != copy)
    }

    /// Whether an item found after it was renamed is still the one seen before: the same
    /// in everything but its change time, which a rename moves.
    fn same_after_rename(found: &Entry, before: &Entry) -> bool {
        found.changed_at(0).unchanged(before)
    }

    /// Removes a file or link for good.
    fn remove_item(path: &Path) -> io::Result<()> {
        Copier::make_writable(path);
        fs::remove_file(path)
    }

    /// Whether the version a copy replaces is kept in the trash: with safe deletes, in two-way
    /// jobs, and always when the copy settles a conflict (`settles`).
    fn keeps_replaced(&self, settles: bool) -> bool {
        self.care.safe_delete || self.care.keep_replaced || settles
    }

    /// The room for a new file of `bytes` on `side`'s drive.
    fn room_for(&self, side: Side, bytes: u64) -> Result<()> {
        match Room::shortage(self.shores.root(side), bytes, &format!("the {}'s drive", side.word())) {
            Some(why) => Err(Error::NoRoom(why)),
            None => Ok(()),
        }
    }

    /// The room for keeping a copy of `what` ("the replaced version"), the item at `item` of
    /// `bytes`, in the trash: none where that copy is a clone.
    fn room_for_kept_copy(&self, side: Side, item: &Path, bytes: u64, what: &str) -> Result<()> {
        if self.clones_at(side, item) {
            Ok(())
        } else {
            self.room_to_keep(side, bytes, what)
        }
    }

    /// The room for keeping copies of `what`, of `bytes`, in the trash, copied in full.
    fn room_to_keep(&self, side: Side, bytes: u64, what: &str) -> Result<()> {
        self.room_for(side, bytes).map_err(|e| Error::NoRoom(format!("{e} (to keep {what} in .charon-trash)")))
    }

    /// Whether a copy of the item at `item` in `side`'s trash is a clone, instant and taking
    /// no room: on APFS, with the item on that drive too. One on a drive mounted inside the
    /// job's folder is copied in full.
    fn clones_at(&self, side: Side, item: &Path) -> bool {
        self.kept_copies(side) == KeptCopies::Clones && self.on_folders_drive(side, item)
    }

    /// Whether the item at `item` is on the drive of `side`'s folder, where its trash is, and
    /// not on another drive mounted inside the job's folder (a folder of the same drive bound
    /// there is on it). One that can't be asked counts as elsewhere.
    #[cfg(unix)]
    fn on_folders_drive(&self, side: Side, item: &Path) -> bool {
        match (fs::metadata(self.shores.root(side)), fs::symlink_metadata(item)) {
            (Ok(folder), Ok(item)) => RootId::device_of(&folder) == RootId::device_of(&item),
            _ => false,
        }
    }

    /// Windows: Charon doesn't tell the drives inside a folder apart there, and copies there
    /// are never clones.
    #[cfg(not(unix))]
    fn on_folders_drive(&self, _side: Side, _item: &Path) -> bool {
        true
    }

    /// How the copies of replaced versions kept in the trash take room on `side`'s drive.
    fn kept_copies(&self, side: Side) -> KeptCopies {
        KeptCopies::on(self.spellings.snapshot(side).volume())
    }

    /// What a step just wrote must be on the drive the folder was on at the scan. When a
    /// drive goes away between two checks, the next item lands in the folder it leaves
    /// behind, on another disk: the run stops there, and nothing of it is recorded.
    #[cfg(unix)]
    fn on_its_drive(&self, side: Side, path: &Path) -> Result<()> {
        let expected = match side {
            Side::Source => self.roots.0,
            Side::Destination => self.roots.1,
        };
        let Some(expected) = expected else { return Ok(()) };
        let meta = fs::symlink_metadata(path).map_err(|e| Error::file(path, e))?;
        if RootId::device_of(&meta) != expected.device() {
            return Err(Error::LandedElsewhere(format!(
                "The {} folder's drive went away during the sync (was it disconnected?). Charon stopped there.",
                side.word()
            )));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn on_its_drive(&self, _side: Side, _path: &Path) -> Result<()> {
        Ok(())
    }

    /// Removes an item that must still be what the plan saw, while the other side still has
    /// what the plan saw there (`other`, usually nothing): a file restored on the other side
    /// meanwhile stops the removal. A folder only goes when nothing but system clutter is
    /// left in it. An item that makes room for one of another kind (`makes_room`) goes into
    /// the trash even without safe deletes: until the new item has landed in its place, the
    /// trash may hold the only copy of what was there.
    fn delete(&self, side: Side, key: &str, planned: &Entry, other: Option<&Entry>, makes_room: bool) -> Result<Done> {
        let there = self.path(side.opposite(), key);
        if !Entry::still(self.look_fresh(side.opposite(), &there)?.as_ref(), other) {
            return Ok(Done::Skipped(format!("changed on the {} side since the scan", side.opposite().word())));
        }
        // With nothing on the other side, a finished removal means both sides agree: gone.
        // (Removals that make room for another item leave the record to the step after them.)
        let settles = other.is_none();
        let relative = self.spellings.disk(side, key);
        let root = self.shores.root(side);
        match self.parents(root, &relative)? {
            Parents::Real => {}
            Parents::Missing => {
                if settles {
                    self.settle_removal(side, key, planned);
                }
                return Ok(Done::Nothing);
            }
            Parents::NotFolders => return Ok(Done::Skipped("a folder above it was replaced since the scan".into())),
        }
        let target = root.join(&relative);
        match self.look_fresh(side, &target)? {
            None => {
                if settles {
                    self.settle_removal(side, key, planned);
                }
                return Ok(Done::Nothing);
            }
            Some(now) if !now.unchanged(planned) => {
                return Ok(Done::Skipped(format!("changed on the {} side since the scan", side.word())))
            }
            Some(_) => {}
        }
        let done = if planned.is_dir() {
            self.remove_folder(side, &relative, &target)?
        } else if self.remove(side, &relative, &target, self.care.safe_delete || makes_room, planned)? {
            self.destroyed_on(side, key);
            Done::Deleted
        } else {
            return Ok(Done::Skipped(format!("changed on the {} side since the scan", side.word())));
        };
        if matches!(done, Done::Deleted) {
            self.touch(&target);
            if settles {
                self.settle_removal(side, key, planned);
            }
        }
        Ok(done)
    }

    /// Both versions survive: the destination's version moves aside under a conflict name
    /// (and is copied to the source under that name); the source's version takes the
    /// original name on the destination.
    fn keep_both(&self, key: &str, source: &Entry, destination: &Entry, meter: &mut ProgressMeter) -> Result<Done> {
        let source_path = self.path(Side::Source, key);
        let destination_path = self.path(Side::Destination, key);
        if !Entry::still(self.look_fresh(Side::Source, &source_path)?.as_ref(), Some(source))
            || !Entry::still(self.look_fresh(Side::Destination, &destination_path)?.as_ref(), Some(destination))
        {
            return Ok(Done::Skipped("changed since the scan".into()));
        }
        let conflict_key = self.free_conflict_name(key)?;
        if let Some(hear) = self.new_names {
            hear(&conflict_key);
        }
        let aside = self.path(Side::Destination, &conflict_key);
        move_to_new_name(&destination_path, &aside).map_err(|e| Error::file(&destination_path, e))?;
        self.touch(&aside);
        self.touch(&destination_path);
        let moved = Self::look(&aside)?.ok_or_else(|| Error::Invalid(format!("{} vanished", aside.display())))?;

        // Should the rename and the copy be lost, the destination's version would be back
        // under the original name: that is the version this copy replaced.
        let carried = self.copy(Side::Destination, key, source, None, false, meter)?;
        if let Some((_, landed)) = carried {
            self.settle_copy(Side::Destination, key, *source, landed, Some(*destination));
        }
        let back = self.copy(Side::Source, &conflict_key, &moved, None, false, meter)?;
        if let Some((_, landed)) = back {
            // Both sides of the conflict copy are this run's doing: the copy on the source,
            // and the destination's version renamed aside.
            let traces = (Trace::Left { item: landed, replaced: None }, Trace::Left { item: moved, replaced: None });
            self.settle(&conflict_key, Some(Pair::new(landed, moved)), traces);
        }
        match (carried, back) {
            (Some((a, _)), Some((b, _))) => Ok(Done::Copied(2, a + b)),
            _ => Ok(Done::Skipped("changed while both versions were being kept".into())),
        }
    }

    // ---------------------------------------------------------------- removing

    /// Into the trash (`keep`), or deleted for good. Links are removed as links. The item must
    /// still be `planned`, and it's checked once more after it was moved: a version an app
    /// saved there at that very moment stays. Returns `false` when that happened.
    ///
    /// On APFS a kept file isn't moved into the trash but cloned there (instant, and it takes
    /// no room), and then removed: an app that has it open follows the file itself, not its
    /// name, so a moved file would take the person's next saves into the trash with it.
    /// Removed, the app saves again where the file was, and the next sync sees it. (One on
    /// another drive mounted inside the job's folder is copied in full, see `clones_at`.)
    fn remove(&self, side: Side, relative: &str, target: &Path, keep: bool, planned: &Entry) -> Result<bool> {
        let removal = if keep && planned.is_file() && self.kept_copies(side) == KeptCopies::Clones {
            self.remove_after_copying(side, relative, target, planned)
        } else if keep {
            self.move_to_trash(side, relative, target, planned)
        } else {
            self.remove_checked(side, relative, target, planned)
        };
        // Gone already: what the plan wanted.
        removal.map(|removal| removal != Removal::Changed)
    }

    /// Moves the file or link at `target` into the trash in one step, if it's still
    /// `planned`, and checks it there: one that isn't goes back (`Changed`). When the trash
    /// can't take it, nothing was touched: an item renamed and put back would carry a new
    /// change time, which the next look would take for an edit on its side. Where the trash is
    /// on another drive than the item (a folder mounted inside the job's), the item is copied
    /// into the trash instead, and then removed. Whatever goes into the trash at its own name
    /// goes through here: a removed or replaced link, the system's files left in a removed
    /// folder, a leftover of an interrupted sync.
    fn move_to_trash(&self, side: Side, relative: &str, target: &Path, planned: &Entry) -> Result<Removal> {
        let kept = match self.trash.put(self.shores.root(side), relative) {
            Ok(kept) => kept,
            Err(Error::File { source, .. }) if source.kind() == io::ErrorKind::CrossesDevices => {
                return self.remove_after_copying(side, relative, target, planned);
            }
            Err(Error::File { source, .. }) if matches!(source.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) && Self::look(target)?.is_none() => {
                return Ok(Removal::Gone);
            }
            Err(e) => return Err(e),
        };
        self.touch(&kept);
        let moved = self.look_fresh(side, &kept);
        if !matches!(&moved, Ok(Some(found)) if Self::same_after_rename(found, planned)) {
            // Back to its place, unless something new took the name meanwhile: then it stays
            // in the trash, never lost and never written over.
            if let Err(e) = rename_new(&kept, target) {
                log::info!("couldn't put {} back ({e}); it stays in the trash", target.display());
            }
            return moved.map(|_| Removal::Changed);
        }
        Ok(Removal::Removed)
    }

    /// Keeps a copy of the file or link at `target` in the trash, then removes it for good if
    /// it's still `planned` (see `remove_checked`). A file's copy is a clone on APFS (instant,
    /// and it takes no room) when the file is on the trash's drive; elsewhere, and from a
    /// drive mounted inside the job's folder, it needs room on the drive.
    fn remove_after_copying(&self, side: Side, relative: &str, target: &Path, planned: &Entry) -> Result<Removal> {
        self.room_for_kept_copy(side, target, planned.size, Self::removed(planned))?;
        let kept = self.trash.preserve(self.shores.root(side), relative)?;
        self.touch(&kept);
        let removed = self.remove_checked(side, relative, target, planned);
        if matches!(removed, Ok(Removal::Changed) | Err(_)) {
            Copier::discard(&kept);
        }
        removed
    }

    /// What a removed item kept in the trash is called where the drive has no room for its
    /// copy (see `room_for_kept_copy`).
    fn removed(item: &Entry) -> &'static str {
        if item.is_link() {
            "the removed link"
        } else {
            "the removed file"
        }
    }

    /// Removes the file or link at `target` for good, if it's still `planned`: renamed aside,
    /// checked, then removed. `Changed` when it isn't (it's put back).
    fn remove_checked(&self, side: Side, relative: &str, target: &Path, planned: &Entry) -> Result<Removal> {
        Copier::make_writable(target);
        let Some(aside) = Self::move_aside(target, self.shores.root(side))? else { return Ok(Removal::Gone) };
        let moved = self.look_fresh(side, &aside);
        if !matches!(&moved, Ok(Some(found)) if Self::same_after_rename(found, planned)) {
            self.put_back(side, &aside, target, relative, None);
            return moved.map(|_| Removal::Changed);
        }
        match Self::remove_item(&aside) {
            Ok(()) => Ok(Removal::Removed),
            Err(e) => {
                self.put_back(side, &aside, target, relative, Some(planned));
                Err(Error::file(target, e))
            }
        }
    }

    /// Removes a folder whose contents were removed by earlier steps. If anything besides
    /// system clutter is still inside (a file added meanwhile, an excluded item, a sync
    /// client's journal), it stays.
    fn remove_folder(&self, side: Side, relative: &str, target: &Path) -> Result<Done> {
        let mut clutter = Vec::new();
        for item in fs::read_dir(target).map_err(|e| Error::file(target, e))? {
            let item = item.map_err(|e| Error::file(target, e))?;
            let is_file = item.file_type().is_ok_and(|t| t.is_file());
            match item.file_name().to_str() {
                Some(name) if is_file && is_clutter(name) && !is_sync_journal(name) => clutter.push(format!("{relative}/{name}")),
                _ => {
                    log::info!("kept {}: it still holds {}", target.display(), item.file_name().to_string_lossy());
                    return Ok(Done::Kept);
                }
            }
        }
        // System files nobody listed (.DS_Store, desktop.ini…) always go to .charon-trash:
        // the person never saw them in the list, so they're never gone for good. Each goes as
        // it is now; one that changes at that moment stays, and so does the folder.
        let root = self.shores.root(side);
        for item in &clutter {
            let path = root.join(item);
            match Self::look(&path)? {
                None => {}
                Some(found) if found.is_file() && self.remove(side, item, &path, true, &found)? => {}
                Some(_) => {
                    log::info!("kept {}: {item} changed as it was being removed", target.display());
                    return Ok(Done::Kept);
                }
            }
        }
        self.real_folders.borrow_mut().clear();
        Copier::make_writable(target);
        match fs::remove_dir(target) {
            Ok(()) => {
                self.removed_folders.borrow_mut().insert(target.to_path_buf());
                Ok(Done::Deleted)
            }
            // Something appeared inside at the last moment: the folder stays.
            Err(e) if fs::read_dir(target).is_ok_and(|mut items| items.next().is_some()) => {
                log::info!("kept {}: {e}", target.display());
                Ok(Done::Kept)
            }
            Err(e) => Err(Error::file(target, e)),
        }
    }

    // ---------------------------------------------------------------- records

    /// Records what a finished step left at `key`: the pair for the two-way memory, and what
    /// it left on the source and on the destination for the last look (see `retrace`).
    fn settle(&self, key: &str, pair: Option<Pair>, (source, destination): (Trace, Trace)) {
        self.settled.borrow_mut().push(Settled { path: key.to_string(), pair });
        self.footprints.borrow_mut().push(Footprint { key: key.to_string(), source, destination, at: Instant::now() });
    }

    /// A copy onto `to` of `from`, which landed there as `landed` in the place of `replaced`.
    fn settle_copy(&self, to: Side, key: &str, from: Entry, landed: Entry, replaced: Option<Entry>) {
        let traces = Self::on_sides(to, Trace::Left { item: landed, replaced }, Trace::Original(from));
        self.settle(key, Some(Self::pair(to, landed, from)), traces);
    }

    /// The item `removed`, gone from `side`, while the other side has nothing there.
    fn settle_removal(&self, side: Side, key: &str, removed: &Entry) {
        self.settle(key, None, Self::on_sides(side, Trace::Removed(*removed), Trace::Untouched));
    }

    /// `here` on `side` and `there` on the other side, as (source, destination).
    fn on_sides(side: Side, here: Trace, there: Trace) -> (Trace, Trace) {
        match side {
            Side::Source => (here, there),
            Side::Destination => (there, here),
        }
    }

    /// The pair for a copy onto `to`: what landed there, and what came from the other side.
    fn pair(to: Side, landed: Entry, from: Entry) -> Pair {
        match to {
            Side::Destination => Pair::new(from, landed),
            Side::Source => Pair::new(landed, from),
        }
    }

    fn destroyed_on(&self, side: Side, key: &str) {
        let mut destroyed = self.destroyed.borrow_mut();
        match side {
            Side::Source => destroyed.0.push(key.to_string()),
            Side::Destination => destroyed.1.push(key.to_string()),
        }
    }

    /// Notes that the folder holding `item` changed.
    fn touch(&self, item: &Path) {
        if let Some(parent) = item.parent() {
            self.touched.borrow_mut().insert(parent.to_path_buf());
        }
    }

    // ---------------------------------------------------------------- looking

    /// The full path of `key` on `side`, as spelled there.
    fn path(&self, side: Side, key: &str) -> PathBuf {
        self.shores.root(side).join(self.spellings.disk(side, key))
    }

    /// `look`, but on a drive that may show changes late (a network share, a cloud drive) a
    /// file is opened and asked directly: its listing can show another computer's change late.
    /// A file the system won't let Charon open there fails with `Error::ReadDenied`.
    fn look_fresh(&self, side: Side, path: &Path) -> Result<Option<Entry>> {
        match Self::look(path)? {
            Some(entry) if self.shows_changes_late(side) && entry.is_file() => match fs::File::open(path).and_then(|file| file.metadata()) {
                Ok(meta) if meta.is_file() => Ok(Some(Entry::file(meta.len(), Scanner::modified_ms(&meta)).changed_at(Scanner::change_time(&meta)))),
                Ok(_) => Err(Error::Changed("changed since the scan".into())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(Copier::unreadable(path, e, 0)),
            },
            other => Ok(other),
        }
    }

    /// Whether `side`'s drive may show changes late (a network share, a cloud drive).
    fn shows_changes_late(&self, side: Side) -> bool {
        self.spellings.snapshot(side).volume().is_some_and(|v| v.shows_changes_late())
    }

    /// Whether extended attributes are worth copying onto `side`'s drive.
    fn holds_attributes(&self, side: Side) -> bool {
        self.spellings.snapshot(side).volume().is_none_or(|v| v.holds_extended_attributes())
    }

    /// The item at `path` as it is now, without following links; `None` if nothing is there.
    /// Any other answer (no permission, a drive error) is an error: it never counts as "gone".
    /// A path too long for the drive fails with `Error::Refused`.
    fn look(path: &Path) -> Result<Option<Entry>> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Self::entry_of(path, &meta).map(Some),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
            Err(e) if Refusal::of_naming(&e) == Some(Refusal::TooLong) => Err(Error::Refused { path: path.to_path_buf(), why: Refusal::TooLong }),
            Err(e) => Err(Error::file(path, e)),
        }
    }

    /// The item at `path`, from its details `meta`.
    fn entry_of(path: &Path, meta: &fs::Metadata) -> Result<Entry> {
        let kind = meta.file_type();
        if kind.is_dir() {
            Ok(Entry::dir())
        } else if kind.is_file() {
            Ok(Entry::file(meta.len(), Scanner::modified_ms(meta)).changed_at(Scanner::change_time(meta)))
        } else if kind.is_symlink() {
            Self::link_entry(path, meta)
        } else {
            Err(Error::Invalid(format!("{} isn't a file or a folder", path.display())))
        }
    }

    #[cfg(unix)]
    fn link_entry(path: &Path, meta: &fs::Metadata) -> Result<Entry> {
        use std::os::unix::ffi::OsStrExt;
        let target = fs::read_link(path).map_err(|e| Error::file(path, e))?;
        Ok(Entry::link(target.as_os_str().as_bytes(), Scanner::modified_ms(meta)))
    }

    /// Windows links are never in a plan; this only keeps them from matching anything.
    #[cfg(not(unix))]
    fn link_entry(_path: &Path, meta: &fs::Metadata) -> Result<Entry> {
        Ok(Entry { kind: EntryKind::Link, size: u64::MAX, modified: Scanner::modified_ms(meta), target: u64::MAX, changed: 0 })
    }

    // ---------------------------------------------------------------- folders on the way

    /// Every folder above `key` on `side` must be a real folder; missing ones are created
    /// one level at a time, below the root only. A file or link in the way stops the step.
    fn prepare_parents(&self, side: Side, key: &str) -> Result<()> {
        let mut folders: Vec<&str> = Keys::ancestors(key).collect();
        folders.reverse();
        for folder in folders {
            self.ensure_folder(side, folder)?;
        }
        Ok(())
    }

    /// What the folders above `relative` are (nothing is created).
    fn parents(&self, root: &Path, relative: &str) -> Result<Parents> {
        let mut current = root.to_path_buf();
        let parts: Vec<&str> = relative.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            current.push(part);
            if self.real_folders.borrow().contains(&current) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() => {
                    self.real_folders.borrow_mut().insert(current.clone());
                }
                Ok(_) => return Ok(Parents::NotFolders),
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(Parents::Missing),
                Err(e) => return Err(Error::file(&current, e)),
            }
        }
        Ok(Parents::Real)
    }

    /// The folder `key` on `side` is a real folder afterwards (created if missing), or this
    /// fails. A name the drive refuses, or takes for another item's (see `not_taken`), fails
    /// with `Error::Refused`; a folder Charon leaves alone fails too (see `not_left_alone`).
    fn ensure_folder(&self, side: Side, key: &str) -> Result<()> {
        let path = self.path(side, key);
        if self.real_folders.borrow().contains(&path) {
            return Ok(());
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                let scanned = self.spellings.snapshot(side).contains(key);
                if !scanned {
                    self.not_taken(side, key)?;
                }
                if meta.file_type().is_symlink() {
                    return Err(Error::Invalid(format!("{} is a link; Charon never writes through links", path.display())));
                }
                if !meta.is_dir() {
                    return Err(Error::Invalid(format!("a file named {} is in the way", path.display())));
                }
                if !scanned {
                    self.not_left_alone(side, key, &path)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&path) {
                Ok(()) => self.touch(&path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return self.ensure_folder(side, key),
                Err(e) => {
                    return Err(match Refusal::of_naming(&e) {
                        Some(why) => Error::Refused { path, why },
                        None => Error::file(&path, e),
                    })
                }
            },
            Err(e) => return Err(Error::file(&path, e)),
        }
        self.real_folders.borrow_mut().insert(path);
        Ok(())
    }

    /// A step found something at `key` on `side` where its scan saw nothing. That is
    /// usually an item new since the scan, but a drive can also take a name for another
    /// item's: a Windows short name (REPORT~1.PDF for "Report final.pdf"), or upper and
    /// lower case by rules Charon's keys don't share. Then the folder above lists no name
    /// with that key, and the step fails with `Refusal::TakenName` instead of waiting for
    /// a next sync that would find the same. The folders above `key` are checked first.
    fn not_taken(&self, side: Side, key: &str) -> Result<()> {
        let here = self.spellings.snapshot(side);
        let mut places: Vec<&str> = Keys::ancestors(key).collect();
        places.reverse();
        places.push(key);
        for place in places {
            let path = self.path(side, place);
            if here.contains(place) || self.real_folders.borrow().contains(&path) {
                continue;
            }
            if Self::look(&path)?.is_none() {
                return Ok(());
            }
            // Still there once the folder was listed: it didn't just go away meanwhile.
            if !self.listed(side, place) && Self::look(&path)?.is_some() {
                return Err(Error::Refused { path, why: Refusal::TakenName });
            }
        }
        Ok(())
    }

    /// A folder found at `key` on `side`, where the scan saw no item, is never taken for the
    /// step's folder when Charon leaves it alone: the scan left that name out, or the name
    /// it has on disk is excluded (by the job's patterns, or as one of Charon's own, like
    /// its trash made during this run), however a drive that ignores case spells the step's.
    /// Writing into it would put the other side's items where no sync looks, and the next
    /// sync would find them gone there.
    fn not_left_alone(&self, side: Side, key: &str, path: &Path) -> Result<()> {
        let folder = Keys::parent(key).map(|parent| self.spellings.disk(side, parent));
        let excluded = |name: &String| {
            let disk = match &folder {
                Some(folder) => format!("{folder}/{name}"),
                None => name.clone(),
            };
            self.filter.excludes(&Keys::normalize(&disk))
        };
        if self.spellings.snapshot(side).leaves_alone(key) || self.listed_names(side, key).is_some_and(|names| names.iter().any(excluded)) {
            return Err(Error::Invalid(format!(
                "{} is a folder Charon leaves alone (excluded, or kept for Charon's own use), so Charon doesn't write into it",
                path.display()
            )));
        }
        Ok(())
    }

    /// Whether the folder that holds `key` on `side` lists a name with `key`'s name. A
    /// folder that can't be listed counts as listing it.
    fn listed(&self, side: Side, key: &str) -> bool {
        self.listed_names(side, key).is_none_or(|names| !names.is_empty())
    }

    /// The names, as written on disk, in the folder that holds `key` on `side` whose key is
    /// `key`'s name. `None` when the folder can't be listed.
    fn listed_names(&self, side: Side, key: &str) -> Option<Vec<String>> {
        let path = self.path(side, key);
        let folder = path.parent()?;
        let items = match fs::read_dir(folder) {
            Ok(items) => items,
            Err(e) => {
                log::info!("couldn't list {}: {e}", folder.display());
                return None;
            }
        };
        let name = Keys::name(key);
        let mut names = Vec::new();
        for item in items {
            let item = item.ok()?;
            if let Some(listed) = item.file_name().to_str().filter(|listed| self.spellings.key_for(listed) == name) {
                names.push(listed.to_string());
            }
        }
        Some(names)
    }

    // ---------------------------------------------------------------- conflict names

    /// A free name for the destination's version of `key`: `notes/plan.txt` → `notes/plan
    /// (conflict 2026-10-16 16.20).txt`, numbered when that name is taken on either side.
    /// Returns its key; the run remembers how it is spelled on each side.
    ///
    /// Where the path to that name is too long for either side's drive, the name is
    /// shortened until it fits. Where not even one letter of the original name fits, both
    /// versions can't be kept: the step is refused with `Refusal::ConflictCopy` on that
    /// side, and the looks after leave the item as it is, with a note, until it changes.
    fn free_conflict_name(&self, key: &str) -> Result<String> {
        // In the NFC form most systems write: a name made on a Mac drive can come decomposed.
        let on_destination = self.spellings.disk(Side::Destination, key);
        let name: String = Keys::name(&on_destination).nfc().collect();
        let place = |side: Side, candidate: &str| match Keys::parent(key) {
            Some(parent) => format!("{}/{candidate}", self.spellings.disk(side, parent)),
            None => candidate.to_string(),
        };
        for n in 1..1000 {
            let mut candidate = Self::conflict_name(&name, n);
            'fitting: loop {
                let mut taken = false;
                for side in [Side::Destination, Side::Source] {
                    match Self::spot(&self.shores.root(side).join(place(side, &candidate)))? {
                        Spot::Free => {}
                        Spot::Taken => taken = true,
                        Spot::TooLong => match Self::conflict_name_within(&name, n, candidate.len() - 1) {
                            Some(shorter) => {
                                candidate = shorter;
                                continue 'fitting;
                            }
                            None => return Err(Error::Refused { path: self.path(side, key), why: Refusal::ConflictCopy }),
                        },
                    }
                }
                if taken {
                    break;
                }
                let destination = place(Side::Destination, &candidate);
                let conflict_key = self.spellings.key_for(&destination);
                self.spellings.learn(Side::Destination, &conflict_key, destination);
                self.spellings.learn(Side::Source, &conflict_key, place(Side::Source, &candidate));
                return Ok(conflict_key);
            }
        }
        Err(Error::Invalid("no free name for the conflict copy".into()))
    }

    /// What is at the place of a conflict copy's name on one side.
    fn spot(path: &Path) -> Result<Spot> {
        match Self::look(path) {
            Ok(None) => Ok(Spot::Free),
            Ok(Some(_)) => Ok(Spot::Taken),
            Err(Error::Refused { why: Refusal::TooLong, .. }) => Ok(Spot::TooLong),
            Err(e) => Err(e),
        }
    }

    /// `plan.txt` → `plan (conflict 2026-10-16 16.20).txt` (`… 16.20 2).txt` for n = 2),
    /// shortened to fit the drive's limit on name length.
    pub fn conflict_name(name: &str, n: u32) -> String {
        let (stem, tail) = Self::conflict_parts(name, n);
        names::fit(stem, &tail)
    }

    /// `conflict_name` in at most `bytes` bytes, keeping at least one letter of the name;
    /// `None` when that doesn't fit.
    fn conflict_name_within(name: &str, n: u32, bytes: usize) -> Option<String> {
        let (stem, tail) = Self::conflict_parts(name, n);
        names::shorten(stem, &tail, bytes, 1)
    }

    /// The part of a conflict copy's name taken from the original (`plan`), and what follows
    /// it (` (conflict 2026-10-16 16.20).txt`).
    fn conflict_parts(name: &str, n: u32) -> (&str, String) {
        let stamp = Local::now().format("%Y-%m-%d %H.%M");
        let (stem, extension) = names::split_extension(name);
        let number = if n > 1 { format!(" {n}") } else { String::new() };
        (stem, format!(" (conflict {stamp}{number}){extension}"))
    }
}

/// What a conflict copy's name meets on one side (see `Executor::free_conflict_name`).
enum Spot {
    Free,
    Taken,
    /// The path to it is too long for the drive.
    TooLong,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::FsKind;

    #[test]
    fn conflict_names_keep_the_extension() {
        let name = Executor::conflict_name("plan.txt", 1);
        assert!(name.starts_with("plan (conflict "));
        assert!(name.ends_with(").txt"));
        assert!(Executor::conflict_name(".env", 1).starts_with(".env (conflict "));
        assert!(Executor::conflict_name("a.txt", 2).ends_with(" 2).txt"));
        assert!(Executor::conflict_name(&"x".repeat(250), 1).len() <= 255);
    }

    #[test]
    fn new_items_take_the_spelling_of_the_side_they_come_from() {
        let mut source = Snapshot::new();
        source.insert("caf\u{e9}", Entry::dir());
        source.insert("caf\u{e9}/menu.txt", Entry::file(1, 1));
        source.facts_mut().spellings.insert("caf\u{e9}".into(), "cafe\u{301}".into());
        source.facts_mut().spellings.insert("caf\u{e9}/menu.txt".into(), "cafe\u{301}/menu.txt".into());
        let mut destination = Snapshot::new();
        destination.insert("caf\u{e9}", Entry::dir());
        let spell = Spellings::new(&source, &destination);
        assert_eq!(spell.disk(Side::Source, "caf\u{e9}/menu.txt"), "cafe\u{301}/menu.txt");
        // The destination already has the folder (spelled NFC there): the file goes inside it.
        assert_eq!(spell.disk(Side::Destination, "caf\u{e9}/menu.txt"), "caf\u{e9}/menu.txt");
        // A new folder takes the source's name, written in NFC.
        let empty = Snapshot::new();
        let spell = Spellings::new(&source, &empty);
        assert_eq!(spell.disk(Side::Destination, "caf\u{e9}/menu.txt"), "caf\u{e9}/menu.txt");
    }

    const KEEP: Care = Care { safe_delete: true, keep_replaced: true, two_way: true };

    /// Two empty folders for an executor to work in.
    fn shores() -> (tempfile::TempDir, Shores) {
        let tmp = tempfile::tempdir().unwrap();
        let shores = Shores { source: tmp.path().join("source"), destination: tmp.path().join("destination") };
        fs::create_dir_all(&shores.source).unwrap();
        fs::create_dir_all(&shores.destination).unwrap();
        (tmp, shores)
    }

    fn write(path: &Path, text: &str, seconds: i64) {
        fs::write(path, text).unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
    }

    fn seen(path: &Path) -> Entry {
        Executor::look(path).unwrap().unwrap()
    }

    /// The copies among `steps`, each as if its new version were written next to its target
    /// in `root`'s folder (see `Executor::stage`).
    fn staged_in<'p>(root: &Path, steps: &'p [Action]) -> Vec<(&'p Action, Staged)> {
        steps
            .iter()
            .map(|step| {
                let Action::Copy { to, path, from, existing, .. } = step else { unreachable!("only copies") };
                let target = root.join(path);
                let ready = Staged {
                    to: *to,
                    key: path.clone(),
                    from: *from,
                    existing: *existing,
                    settles: false,
                    relative: path.clone(),
                    partial: Copier::partial_path(&target),
                    target,
                    unkept: BTreeSet::new(),
                };
                (step, ready)
            })
            .collect()
    }

    /// The names in a folder, Charon's own trash left out.
    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder).unwrap().flatten().map(|i| i.file_name().to_string_lossy().into_owned()).filter(|n| n != crate::scan::TRASH_FOLDER).collect();
        names.sort();
        names
    }

    /// An app saves a new version (a new file under the same name, even with the same size
    /// and time) at the very instant Charon swaps its copy in: the app's version goes back.
    #[test]
    fn a_version_saved_while_replacing_is_swapped_back() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        write(&target, "old", 1_600_000_000);
        let (planned, checked) = (seen(&target), Identity::at(&target));
        let saved = shores.destination.join("saved-by-the-app");
        write(&saved, "app", 1_600_000_000);
        fs::rename(&saved, &target).unwrap();
        let partial = Copier::partial_path(&target);
        write(&partial, "new version", 1_700_000_000);
        let swapped = Swapped { checked, copy: Identity::at(&partial) };
        if swap(&partial, &target).is_err() {
            return; // a drive that can't swap: `replace_aside` covers it
        }
        assert!(!executor.check_swapped(Side::Destination, &partial, &target, "notes.txt", &planned, swapped).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "app");
        assert_eq!(names_in(&shores.destination), vec!["notes.txt"], "the copy is gone, nothing else is left");
    }

    /// When the version an app saved can't be swapped back (here the name it would go back to
    /// is gone), the step says where that version is: in the trash, or, when the trash can't
    /// take it, under the temporary name it has.
    #[test]
    fn a_version_that_cant_be_swapped_back_is_said_to_be_where_it_is() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        let planned = Entry::file(3, 1_600_000_000_000);
        let swapped = Swapped { checked: None, copy: None };
        let trash = shores.destination.join(crate::scan::TRASH_FOLDER);
        fs::write(&trash, "a file where the trash would be").unwrap();
        let partial = Copier::partial_path(&target);
        write(&partial, "the app's version", 1_700_000_000);
        let Err(Error::Changed(why)) = executor.check_swapped(Side::Destination, &partial, &target, "notes.txt", &planned, swapped) else {
            panic!("the step says the file changed")
        };
        let name = partial.file_name().unwrap().to_string_lossy().into_owned();
        assert!(why.ends_with(&format!("that version stays under a temporary name, as {name}")), "{why}");
        assert_eq!(fs::read_to_string(&partial).unwrap(), "the app's version");

        fs::remove_file(&trash).unwrap();
        let Err(Error::Changed(why)) = executor.check_swapped(Side::Destination, &partial, &target, "notes.txt", &planned, swapped) else {
            panic!("the step says the file changed")
        };
        assert!(why.ends_with("that version is in .charon-trash"), "{why}");
        assert!(names_in(&shores.destination).is_empty());
        assert_eq!(Trash::look(&shores.destination, 0, true, false).bytes, 17, "the trash holds the app's version");
    }

    #[test]
    fn a_checked_version_is_replaced_and_nothing_is_left_behind() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        write(&target, "old", 1_600_000_000);
        let planned = seen(&target);
        let partial = Copier::partial_path(&target);
        write(&partial, "new version", 1_700_000_000);
        assert!(executor.replace(Side::Destination, &partial, &target, "notes.txt", &planned).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new version");
        assert_eq!(names_in(&shores.destination), vec!["notes.txt"]);

        // Where the drive can't swap names, the old version goes aside first.
        let partial = Copier::partial_path(&target);
        write(&partial, "newest", 1_800_000_000);
        assert!(executor.replace_aside(Side::Destination, &partial, &target, "notes.txt", &seen(&target)).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "newest");
        assert_eq!(names_in(&shores.destination), vec!["notes.txt"]);
    }

    #[test]
    fn a_changed_file_is_never_replaced_aside_either() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        write(&target, "old", 1_600_000_000);
        let planned = seen(&target);
        write(&target, "saved by an app", 1_600_000_100);
        let partial = Copier::partial_path(&target);
        write(&partial, "new version", 1_700_000_000);
        assert!(!executor.replace_aside(Side::Destination, &partial, &target, "notes.txt", &planned).unwrap());
        assert_eq!(fs::read_to_string(&target).unwrap(), "saved by an app");
        assert_eq!(names_in(&shores.destination), vec!["notes.txt"]);
        assert!(!executor.replace(Side::Destination, &Copier::partial_path(&target), &target, "notes.txt", &planned).unwrap());
    }

    /// A replacement that fails while the old version is still in its place (an app holds
    /// the file open, say) leaves no copy of it in the trash: each sync that tries again
    /// would leave one more.
    #[test]
    fn a_replacement_that_fails_leaves_no_copy_of_the_old_version_in_the_trash() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        write(&shores.source.join("report.txt"), "new version", 1_700_000_000);
        let target = shores.destination.join("report.txt");
        write(&target, "old", 1_600_000_000);
        let (from, old) = (seen(&shores.source.join("report.txt")), seen(&target));
        let observer = |_: &crate::model::Progress| {};
        let mut meter = ProgressMeter::for_plan(&Plan::default(), &observer);
        let staged = executor.stage(Side::Destination, "report.txt", &from, Some(&old), false, &mut meter).unwrap().unwrap();
        // The copy can't take the name (here it's gone, as a drive may refuse the rename).
        fs::remove_file(&staged.partial).unwrap();
        assert!(executor.commit(staged).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(Trash::look(&shores.destination, 0, true, false).bytes, 0, "no copy of the old version is left in the trash");
    }

    /// After a failed replacement, only the old version itself counts as still in its place:
    /// never the new copy that took the name, however much it looks like the old one.
    #[test]
    fn only_the_old_version_itself_counts_as_still_in_place() {
        let (_tmp, shores) = shores();
        let target = shores.destination.join("vault.img");
        write(&target, "old", 1_600_000_000);
        let old = seen(&target);
        let copy = Copier::partial_path(&target);
        write(&copy, "new", 1_600_000_000);
        let copy_identity = Identity::at(&copy);
        assert!(Executor::still_in_place(&target, &old, copy_identity));
        fs::rename(&copy, &target).unwrap();
        assert!(!Executor::still_in_place(&target, &old, copy_identity), "the new copy, with the old one's size and time");
        fs::remove_file(&target).unwrap();
        assert!(!Executor::still_in_place(&target, &old, copy_identity));
    }

    /// Something new took the name while the old item was aside: the old item goes into the
    /// trash rather than over it.
    #[test]
    fn an_item_put_back_never_replaces_a_newer_one() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        write(&target, "old", 1_600_000_000);
        let aside = Executor::move_aside(&target, &shores.destination).unwrap().unwrap();
        write(&target, "newer", 1_700_000_000);
        executor.put_back(Side::Destination, &aside, &target, "notes.txt", None);
        assert_eq!(fs::read_to_string(&target).unwrap(), "newer");
        assert_eq!(names_in(&shores.destination), vec!["notes.txt"]);
        let trash = Trash::look(&shores.destination, 0, true, false);
        assert_eq!(trash.bytes, 3, "the old one is in the trash");
    }

    /// A Photos library whose app renamed thousands of its thumbnails (it makes them anew
    /// after an update) brings as many new files as it removes, and a new database whose old
    /// journal goes. The old journals of the databases coming in are found with one look at
    /// each removal: with 4,000 of each that takes moments, where pairing every new file with
    /// every removal took many seconds, more with the square of the library's changes, while
    /// the window showed no progress.
    #[test]
    fn the_journals_of_a_library_with_thousands_of_changes_are_found_in_moments() {
        const RENAMED: usize = 4_000;
        let (_tmp, shores) = shores();
        let database = "Lib.photoslibrary/database/Photos.sqlite";
        let journal = format!("{database}-wal");
        let thumbnail = |name: String| format!("Lib.photoslibrary/resources/derivatives/{name}.jpg");
        let (mut source, mut destination) = (Snapshot::new(), Snapshot::new());
        source.insert(database, Entry::file(20, 2_000));
        destination.insert(database, Entry::file(10, 1_000));
        destination.insert(journal.as_str(), Entry::file(5, 1_000));
        let mut copies = vec![Action::copy(Side::Destination, database, Entry::file(20, 2_000), Some(Entry::file(10, 1_000)))];
        let mut removed = vec![Action::delete(Side::Destination, &journal, Entry::file(5, 1_000), None)];
        for n in 0..RENAMED {
            let (new, old) = (thumbnail(format!("{n}_thumb")), thumbnail(format!("thumb_{n}")));
            source.insert(new.as_str(), Entry::file(1, 1_000));
            destination.insert(old.as_str(), Entry::file(1, 1_000));
            copies.push(Action::copy(Side::Destination, &new, Entry::file(1, 1_000), None));
            removed.push(Action::delete(Side::Destination, &old, Entry::file(1, 1_000), None));
        }
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &source, &destination, KEEP, &cancel);
        let staged = staged_in(&shores.destination, &copies);
        let mut removals: Vec<&Action> = removed.iter().collect();
        let started = Instant::now();
        let journals = executor.journals_of(&staged, &mut removals);
        let took = started.elapsed();
        assert_eq!(journals.len(), staged.len());
        assert_eq!(journals[0], [&removed[0]], "the database's old journal");
        assert!(journals[1..].iter().all(Vec::is_empty), "thumbnails have no journals");
        assert_eq!(removals.len(), RENAMED, "every old thumbnail is still to be removed");
        assert!(took < Duration::from_secs(1), "finding the journals took {took:?}");
    }

    #[test]
    fn a_removal_leaves_a_file_that_changed_at_the_last_moment() {
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        for (care, keep) in [(KEEP, true), (Care { safe_delete: false, keep_replaced: false, two_way: false }, false)] {
            let (_tmp, shores) = shores();
            let executor = Executor::new(&shores, &empty, &empty, care, &cancel);
            let target = shores.destination.join("plan.txt");
            write(&target, "old", 1_600_000_000);
            let planned = seen(&target);
            write(&target, "saved again", 1_600_000_100);
            assert!(!executor.remove(Side::Destination, "plan.txt", &target, keep, &planned).unwrap());
            assert_eq!(fs::read_to_string(&target).unwrap(), "saved again");
            assert_eq!(names_in(&shores.destination), vec!["plan.txt"]);

            let planned = seen(&target);
            assert!(executor.remove(Side::Destination, "plan.txt", &target, keep, &planned).unwrap());
            assert!(names_in(&shores.destination).is_empty());
            assert_eq!(Trash::look(&shores.destination, 0, true, false).bytes > 0, keep, "kept in the trash only with safe deletes");
        }
    }

    /// When the trash can't take a file (a link stands where it should be), the removal fails
    /// before the file is touched: renamed and put back, it would carry a new change time,
    /// which the next look would take for an edit on its side.
    #[cfg(unix)]
    #[test]
    fn a_removal_the_trash_cant_take_leaves_the_file_untouched() {
        let (tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, shores.destination.join(crate::scan::TRASH_FOLDER)).unwrap();
        let target = shores.destination.join("photo.jpg");
        write(&target, "the photo", 1_600_000_000);
        let planned = seen(&target);
        std::thread::sleep(Duration::from_millis(20));
        assert!(executor.remove(Side::Destination, "photo.jpg", &target, true, &planned).is_err());
        assert_eq!(seen(&target), planned, "not even renamed: it has the change time the plan saw");
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        assert!(executor.put_back_items.take().is_empty());
    }

    /// The destination's folder `photos`, on another drive than its trash, as a folder
    /// mounted inside the job's folder is. A link to a folder on another drive (/dev/shm)
    /// stands in for the mount, which only root may make.
    #[cfg(target_os = "linux")]
    fn photos_on_another_drive(shores: &Shores) -> tempfile::TempDir {
        use std::os::unix::fs::MetadataExt;
        let other_drive = tempfile::tempdir_in("/dev/shm").unwrap();
        let device = |path: &Path| fs::metadata(path).unwrap().dev();
        assert_ne!(device(other_drive.path()), device(&shores.destination), "/dev/shm is a drive of its own");
        std::os::unix::fs::symlink(other_drive.path(), shores.destination.join("photos")).unwrap();
        other_drive
    }

    /// What `root`'s trash holds, by path inside each dated folder: a file with its contents,
    /// a link with where it points.
    #[cfg(target_os = "linux")]
    fn trashed(root: &Path) -> Vec<(String, String)> {
        let trash = root.join(crate::scan::TRASH_FOLDER);
        let mut found = Vec::new();
        if !trash.exists() {
            return found;
        }
        for item in walkdir::WalkDir::new(&trash).min_depth(2).follow_links(false).sort_by_file_name() {
            let item = item.unwrap();
            let inside = item.path().strip_prefix(&trash).unwrap().components().skip(1).map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>();
            if item.file_type().is_symlink() {
                found.push((inside.join("/"), format!("link to {}", fs::read_link(item.path()).unwrap().display())));
            } else if item.file_type().is_file() {
                found.push((inside.join("/"), fs::read_to_string(item.path()).unwrap()));
            }
        }
        found
    }

    /// Where the trash is on another drive than the file (a folder mounted inside the job's
    /// folder), the file can't be moved there: it's copied into the trash, then removed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_on_another_drive_than_its_trash_is_copied_there_then_removed() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        let target = shores.destination.join("photos/x.jpg");
        write(&target, "the photo", 1_600_000_000);
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        assert!(executor.remove(Side::Destination, "photos/x.jpg", &target, true, &seen(&target)).unwrap());
        assert!(!other_drive.path().join("x.jpg").exists());
        assert_eq!(Trash::look(&shores.destination, 0, true, false).bytes, 9, "the trash holds the copy");
    }

    /// On APFS a kept copy is a clone and takes no room, but only of a file on the trash's own
    /// drive: one from a drive mounted inside the job's folder is copied in full, so it needs
    /// room like anywhere else. Short of it, the file stays where it is and nothing goes into
    /// the trash.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kept_copy_from_a_drive_mounted_inside_needs_room_even_where_copies_are_clones() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        let mut apfs = Snapshot::new();
        apfs.facts_mut().volume = Some(Volume {
            kind: FsKind::Apfs,
            fs: "apfs".into(),
            mount: "/".into(),
            name: "Macintosh HD".into(),
            network: false,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        });
        let source = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &source, &apfs, KEEP, &cancel);
        let (far, near) = (shores.destination.join("photos/photo.jpg"), shores.destination.join("notes.txt"));
        write(&far, "the photo", 1_600_000_000);
        write(&near, "the notes", 1_600_000_000);
        let huge = u64::MAX / 2;

        assert!(matches!(executor.room_for_kept_copy(Side::Destination, &far, huge, "the replaced version"), Err(Error::NoRoom(_))));
        assert!(executor.room_for_kept_copy(Side::Destination, &near, huge, "the replaced version").is_ok(), "a clone takes no room");

        let planned = Entry { size: huge, ..seen(&far) };
        let e = executor.remove(Side::Destination, "photos/photo.jpg", &far, true, &planned).expect_err("no room for the photo's copy");
        assert!(matches!(e, Error::NoRoom(_)), "{e}");
        assert!(e.to_string().contains("to keep the removed file in .charon-trash"), "{e}");
        assert_eq!(fs::read_to_string(other_drive.path().join("photo.jpg")).unwrap(), "the photo");
        assert_eq!(trashed(&shores.destination), Vec::<(String, String)>::new());

        // A library's replaced versions, checked before any of them is: the same.
        let replaced = |path: &str| vec![Action::copy(Side::Destination, path, Entry::file(10, 2_000), Some(Entry::file(huge, 1_000)))];
        fs::create_dir(shores.destination.join("photos/Photos.photoslibrary")).unwrap();
        write(&shores.destination.join("photos/Photos.photoslibrary/original.heic"), "the original", 1_600_000_000);
        let library = Unit::Package("photos/Photos.photoslibrary".into());
        let far = replaced("photos/Photos.photoslibrary/original.heic");
        let (at, e) = executor.room_for_replaced(&library, &staged_in(&shores.destination, &far)).expect("no room for the original's replaced version");
        assert_eq!(at, 0);
        assert!(e.to_string().contains("to keep the replaced versions of “photos/Photos.photoslibrary” in .charon-trash"), "{e}");
        fs::create_dir(shores.destination.join("Song.logicx")).unwrap();
        write(&shores.destination.join("Song.logicx/take0.wav"), "the take", 1_600_000_000);
        let near = replaced("Song.logicx/take0.wav");
        assert!(executor.room_for_replaced(&Unit::Package("Song.logicx".into()), &staged_in(&shores.destination, &near)).is_none(), "a clone takes no room");
    }

    /// So is everything else that goes into the trash there: the files the system left in a
    /// removed folder (Finder's .DS_Store), and a removed link. Neither makes its step fail
    /// on every sync.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_a_removal_leaves_on_another_drive_than_its_trash_is_copied_there_then_removed() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        fs::create_dir(other_drive.path().join("2019")).unwrap();
        write(&other_drive.path().join("2019/.DS_Store"), "Finder's view settings", 1_600_000_000);
        std::os::unix::fs::symlink("2019/x.jpg", other_drive.path().join("latest")).unwrap();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);

        let folder = shores.destination.join("photos/2019");
        assert!(matches!(executor.remove_folder(Side::Destination, "photos/2019", &folder), Ok(Done::Deleted)));
        assert!(!other_drive.path().join("2019").exists());
        let link = shores.destination.join("photos/latest");
        assert!(executor.remove(Side::Destination, "photos/latest", &link, true, &seen(&link)).unwrap());
        assert!(fs::symlink_metadata(other_drive.path().join("latest")).is_err());
        let kept = vec![
            ("photos/2019/.DS_Store".to_string(), "Finder's view settings".to_string()),
            ("photos/latest".to_string(), "link to 2019/x.jpg".to_string()),
        ];
        assert_eq!(trashed(&shores.destination), kept);
    }

    /// A link replaced on another drive than its trash: the old link is copied into the trash
    /// and removed, and the new one takes its place.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_replaced_on_another_drive_than_its_trash_is_kept_there() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        fs::create_dir(shores.source.join("photos")).unwrap();
        std::os::unix::fs::symlink("2020/y.jpg", shores.source.join("photos/newest")).unwrap();
        std::os::unix::fs::symlink("2019/x.jpg", other_drive.path().join("newest")).unwrap();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);

        let target = shores.destination.join("photos/newest");
        let partial = Copier::partial_path(&target);
        std::os::unix::fs::symlink("2020/y.jpg", &partial).unwrap();
        let (key, from, old) = ("photos/newest".to_string(), seen(&shores.source.join("photos/newest")), seen(&target));
        let staged = Staged {
            to: Side::Destination,
            relative: key.clone(),
            key,
            from,
            existing: Some(old),
            settles: false,
            target: target.clone(),
            partial,
            unkept: BTreeSet::new(),
        };
        assert!(executor.commit(staged).unwrap().is_some());
        assert_eq!(fs::read_link(&target).unwrap(), Path::new("2020/y.jpg"));
        assert_eq!(trashed(&shores.destination), vec![("photos/newest".to_string(), "link to 2019/x.jpg".to_string())]);
    }

    /// The old journal of a database on another drive than its trash, renamed aside by the
    /// step `removal` while the new database takes its place (see `Executor::set_aside`).
    #[cfg(target_os = "linux")]
    fn journal_aside<'p>(shores: &Shores, removal: &'p Action) -> Aside<'p> {
        let Action::Delete { path, entry, .. } = removal else { unreachable!("only removals") };
        let target = shores.destination.join(path);
        let at = Executor::move_aside(&target, &shores.destination).unwrap();
        Aside { step: removal, side: Side::Destination, key: path, planned: *entry, relative: path.clone(), target, at }
    }

    /// The old journal of a database renamed aside on another drive than its trash can't be
    /// moved there either, once the new database took its place: it's copied into the trash
    /// under its own name, then removed. Its step doesn't fail, and nothing is left under the
    /// temporary name.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_old_journal_on_another_drive_than_its_trash_is_copied_there_then_removed() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        let journal = shores.destination.join("photos/data.db-wal");
        write(&journal, "changes not in the database yet", 1_600_000_000);
        let removal = Action::delete(Side::Destination, "photos/data.db-wal", seen(&journal), None);
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let aside = journal_aside(&shores, &removal);
        assert!(matches!(executor.finish_aside(aside, false), Ok(Done::Deleted)));
        assert!(names_in(other_drive.path()).is_empty(), "nothing is left under a temporary name");
        assert_eq!(trashed(&shores.destination), vec![("photos/data.db-wal".to_string(), "changes not in the database yet".to_string())]);
    }

    /// One that changed after it was renamed aside (its app still had it open) isn't what the
    /// plan saw anymore, and its copy might miss what was written into it last: it stays
    /// under its temporary name, its copy goes, and the step says where it is.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_old_journal_that_changed_aside_on_another_drive_than_its_trash_stays_where_it_is() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        let journal = shores.destination.join("photos/data.db-wal");
        write(&journal, "changes not in the database yet", 1_600_000_000);
        let removal = Action::delete(Side::Destination, "photos/data.db-wal", seen(&journal), None);
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let aside = journal_aside(&shores, &removal);
        let at = aside.at.clone().unwrap();
        write(&at, "changes the app wrote after it was renamed aside", 1_600_000_100);
        let Err(Error::Changed(why)) = executor.finish_aside(aside, false) else { panic!("the removal is skipped as changed") };
        let name = at.file_name().unwrap().to_string_lossy().into_owned();
        assert!(why.ends_with(&format!("it stays under a temporary name, as photos/{name}")), "{why}");
        assert_eq!(names_in(other_drive.path()), vec![name]);
        assert_eq!(fs::read_to_string(&at).unwrap(), "changes the app wrote after it was renamed aside");
        assert!(trashed(&shores.destination).is_empty(), "no copy of it is left in the trash");
    }

    /// An item put back on another drive than its trash, while something new took its name,
    /// is copied into the trash and then removed, rather than left under its temporary name.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_item_put_back_on_another_drive_than_its_trash_goes_into_the_trash() {
        let (_tmp, shores) = shores();
        let other_drive = photos_on_another_drive(&shores);
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("photos/notes.txt");
        write(&target, "old", 1_600_000_000);
        let aside = Executor::move_aside(&target, &shores.destination).unwrap().unwrap();
        write(&target, "newer", 1_700_000_000);
        executor.put_back(Side::Destination, &aside, &target, "photos/notes.txt", None);
        assert_eq!(fs::read_to_string(&target).unwrap(), "newer");
        assert_eq!(names_in(other_drive.path()), vec!["notes.txt"]);
        assert_eq!(trashed(&shores.destination), vec![("photos/notes.txt".to_string(), "old".to_string())]);
    }

    /// The person removes a link a moment before the newer one from the other side takes its
    /// place: the newer one doesn't take it, and the step says the link changed since the
    /// plan. The next look finds the removal, and asks about it.
    #[cfg(unix)]
    #[test]
    fn a_link_removed_just_before_its_replacement_stays_removed() {
        let (_tmp, shores) = shores();
        std::os::unix::fs::symlink("2020/y.jpg", shores.source.join("newest")).unwrap();
        let target = shores.destination.join("newest");
        std::os::unix::fs::symlink("2019/x.jpg", &target).unwrap();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let partial = Copier::partial_path(&target);
        std::os::unix::fs::symlink("2020/y.jpg", &partial).unwrap();
        let (key, from, old) = ("newest".to_string(), seen(&shores.source.join("newest")), seen(&target));
        // Removed by the person after the last look, before the commit.
        fs::remove_file(&target).unwrap();
        let staged = Staged {
            to: Side::Destination,
            relative: key.clone(),
            key,
            from,
            existing: Some(old),
            settles: false,
            target: target.clone(),
            partial: partial.clone(),
            unkept: BTreeSet::new(),
        };
        assert!(executor.commit(staged).unwrap().is_none(), "the link changed since the plan");
        assert!(fs::symlink_metadata(&target).is_err(), "it stays removed");
        assert!(fs::symlink_metadata(&partial).is_err(), "the newer one is thrown away");
    }

    /// A sync client's journal that turned up in a folder after the list was made keeps the
    /// folder: named like the system's files (`._*`), it's still the client's own, never
    /// removed.
    #[test]
    fn a_folder_holding_a_sync_clients_journal_is_kept() {
        let (_tmp, shores) = shores();
        let folder = shores.destination.join("Work");
        fs::create_dir(&folder).unwrap();
        write(&folder.join("._sync_ba9876543210.db"), "the client's journal", 1_600_000_000);
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        assert!(matches!(executor.remove_folder(Side::Destination, "Work", &folder), Ok(Done::Kept)));
        assert_eq!(fs::read_to_string(folder.join("._sync_ba9876543210.db")).unwrap(), "the client's journal");
        assert!(!shores.destination.join(crate::scan::TRASH_FOLDER).exists());
    }

    /// A step that can't finish once it renamed the item aside puts it back as it was. The
    /// renames moved its change time, so it's noted with the one it has now: the record of the
    /// last sync follows it (see `Baseline::after_run`), and the next look sees no edit.
    #[cfg(unix)]
    #[test]
    fn an_item_put_back_as_it_was_is_noted_with_its_new_change_time() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let target = shores.destination.join("notes.txt");
        write(&target, "old", 1_600_000_000);
        let planned = seen(&target);
        std::thread::sleep(Duration::from_millis(20));
        // The copy meant to take its place is gone: the old version goes aside, and back.
        let missing = Copier::partial_path(&target);
        assert!(executor.replace_aside(Side::Destination, &missing, &target, "notes.txt", &planned).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        let now = seen(&target);
        assert_ne!(now.changed, planned.changed, "the renames moved its change time");
        let put_back = PutBack { side: Side::Destination, path: "notes.txt".into(), found: planned, now };
        assert_eq!(executor.put_back_items.take(), vec![put_back]);

        // One that changed meanwhile goes back too, but isn't noted: that change is real.
        write(&target, "saved by an app", 1_600_000_100);
        assert!(!executor.replace_aside(Side::Destination, &missing, &target, "notes.txt", &planned).unwrap());
        assert!(executor.put_back_items.take().is_empty());
    }

    /// Windows: a path of over 300 characters and read-only files and folders are replaced,
    /// kept and removed like any others.
    #[cfg(windows)]
    #[test]
    fn long_paths_and_read_only_items_work_on_windows() {
        fn read_only(path: &Path) {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(path, permissions).unwrap();
        }
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        for care in [KEEP, Care { safe_delete: false, keep_replaced: false, two_way: false }] {
            let (_tmp, shores) = shores();
            let executor = Executor::new(&shores, &empty, &empty, care, &cancel);
            let folder = (0..6).map(|i| format!("{i}{}", "x".repeat(50))).collect::<Vec<_>>().join("/");
            let relative = format!("{folder}/notes.txt");
            let target = shores.destination.join(&relative);
            assert!(target.as_os_str().len() > 300);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            write(&target, "old", 1_600_000_000);
            read_only(&target);

            if care.keep_replaced {
                let kept = executor.trash.preserve(&shores.destination, &relative).unwrap();
                assert_eq!(fs::read_to_string(kept).unwrap(), "old");
            }
            let partial = Copier::partial_path(&target);
            write(&partial, "new", 1_700_000_000);
            assert!(executor.replace(Side::Destination, &partial, &target, &relative, &seen(&target)).unwrap());
            assert_eq!(fs::read_to_string(&target).unwrap(), "new");

            read_only(&target);
            assert!(executor.remove(Side::Destination, &relative, &target, care.safe_delete, &seen(&target)).unwrap());
            assert!(!target.exists());

            let emptied = shores.destination.join("0".to_string() + &"x".repeat(50));
            let inner = folder.split('/').skip(1).collect::<Vec<_>>();
            for depth in (0..inner.len()).rev() {
                let path = emptied.join(inner[..=depth].join("/"));
                read_only(&path);
                let relative = path.strip_prefix(&shores.destination).unwrap().to_string_lossy().replace('\\', "/");
                assert!(matches!(executor.remove_folder(Side::Destination, &relative, &path).unwrap(), Done::Deleted));
            }
        }
    }

    #[test]
    fn a_name_the_drive_takes_for_another_items_is_refused() {
        let (_tmp, shores) = shores();
        let (source, destination) = (Snapshot::new(), Snapshot::new());
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &source, &destination, KEEP, &cancel);
        fs::write(shores.destination.join("Report final.pdf"), "theirs").unwrap();
        fs::write(shores.destination.join("new.txt"), "added after the scan").unwrap();
        // A drive with short names finds "Report final.pdf" under REPORT~1.PDF; this test's
        // drive has none, so the spelling stands in for the drive's answer.
        executor.spellings.learn(Side::Destination, "REPORT~1.PDF", "Report final.pdf".into());
        assert!(matches!(
            executor.not_taken(Side::Destination, "REPORT~1.PDF"),
            Err(Error::Refused { why: Refusal::TakenName, .. })
        ));
        // An item that's simply new since the scan is listed under its own name.
        assert!(executor.not_taken(Side::Destination, "new.txt").is_ok());
        assert!(executor.not_taken(Side::Destination, "missing.txt").is_ok());
    }

    /// Only what a lost change leaves counts against a finished step: changes made after it
    /// (an edit, a time a drive rounded, a change time a rename moved, a new file under a
    /// removed name) are for the next look.
    #[test]
    fn only_what_a_lost_change_leaves_counts_against_a_step() {
        let old = Entry::file(3, 1_600_000_000_000);
        let copy = Entry::file(11, 1_700_000_000_000).changed_at(5);
        let left = Trace::Left { item: copy, replaced: Some(old) };
        assert!(matches!(left.lost(None), Some(Loss::Missing)));
        assert!(matches!(left.lost(Some(&Entry::dir())), Some(Loss::Missing)));
        assert!(matches!(left.lost(Some(&old)), Some(Loss::Replaced)));
        assert!(left.lost(Some(&copy)).is_none());
        assert!(left.lost(Some(&copy.changed_at(9))).is_none(), "a rename moved its change time");
        assert!(left.lost(Some(&Entry::file(11, 1_700_000_001_000))).is_none(), "a drive rounded its time");
        assert!(left.lost(Some(&Entry::file(40, 1_800_000_000_000))).is_none(), "edited after it landed");
        // A copy that looks just like the version it replaced, on a drive without trusted
        // change times: only its presence tells.
        let alike = Trace::Left { item: Entry::file(3, 1_600_000_000_000), replaced: Some(old) };
        assert!(alike.lost(Some(&old)).is_none());

        let removed = Trace::Removed(old);
        assert!(removed.lost(None).is_none());
        assert!(matches!(removed.lost(Some(&old.changed_at(7))), Some(Loss::Back)));
        assert!(removed.lost(Some(&Entry::file(20, 1_800_000_000_000))).is_none(), "a new file under the old name");
        assert!(matches!(Trace::Removed(Entry::dir()).lost(Some(&Entry::dir())), Some(Loss::Back)));
        assert!(Trace::Untouched.lost(Some(&old)).is_none());
    }

    /// An encrypted container keeps its size and time when it's edited: where the scan
    /// trusted the drive's change times, the replaced version is told from the copy by its
    /// change time, which only moves forward.
    #[test]
    fn a_replaced_version_with_the_same_size_and_time_is_told_by_its_change_time() {
        let (size, time) = (4_096, 1_600_000_000_000);
        let replaced = Entry::file(size, time).changed_at(100);
        let copy = Entry::file(size, time).changed_at(300);
        let left = Trace::Left { item: copy, replaced: Some(replaced) };
        assert!(matches!(left.lost(Some(&replaced)), Some(Loss::Replaced)), "the replaced version, untouched");
        assert!(matches!(left.lost(Some(&Entry::file(size, time).changed_at(50))), Some(Loss::Older)), "a version from before it");
        assert!(left.lost(Some(&copy)).is_none());
        assert!(left.lost(Some(&copy.changed_at(400))).is_none(), "edited after it landed, or renamed");
        // The copy has the replaced version's change time (a drive that keeps it in whole
        // seconds): the change time tells nothing.
        let same_second = Trace::Left { item: replaced, replaced: Some(replaced) };
        assert!(same_second.lost(Some(&replaced)).is_none());
        // Without a change time the scan trusted (FAT, exFAT, network shares), it isn't used.
        let untimed = Trace::Left { item: copy, replaced: Some(replaced.changed_at(0)) };
        assert!(untimed.lost(Some(&replaced)).is_none());
    }

    /// The original a copy was made from: gone, of another kind, or a version from before
    /// the one copied counts; saved again after the copy doesn't.
    #[test]
    fn an_original_counts_as_lost_when_gone_or_older_than_what_was_copied() {
        let copied = Entry::file(10, 1_600_000_000_000).changed_at(200);
        let original = Trace::Original(copied);
        assert!(matches!(original.lost(None), Some(Loss::Missing)));
        assert!(matches!(original.lost(Some(&Entry::dir())), Some(Loss::Missing)));
        assert!(matches!(original.lost(Some(&Entry::file(8, 1_500_000_000_000).changed_at(100))), Some(Loss::Older)));
        assert!(matches!(original.lost(Some(&Entry::file(8, 1_500_000_000_000).changed_at(200))), Some(Loss::Older)), "saved in the same step of the drive's clock");
        assert!(matches!(original.lost(Some(&copied.changed_at(150))), Some(Loss::Older)), "a look-alike version from before");
        assert!(original.lost(Some(&copied)).is_none());
        assert!(original.lost(Some(&Entry::file(12, 1_500_000_000_000).changed_at(300))).is_none(), "saved again with an old time");
        // Without a change time the scan trusted, only a missing original counts.
        let untimed = Trace::Original(copied.changed_at(0));
        assert!(untimed.lost(Some(&Entry::file(8, 1_500_000_000_000).changed_at(100))).is_none());
        assert!(matches!(untimed.lost(None), Some(Loss::Missing)));
        assert!(Trace::Original(Entry::dir()).lost(Some(&Entry::dir())).is_none());
    }

    /// A copy that failed holds every removal on its side. One left for the next sync
    /// because its file changed holds only the removals there that could be its old copy:
    /// the same name, or the same size and time.
    #[test]
    fn a_copy_that_changed_holds_only_what_could_be_its_old_copy() {
        let photo = Entry::file(100, 1_600_000_000_000);
        let removal = |side: Side, path: &str, entry: Entry| Action::Delete { side, path: path.into(), item: entry.kind, entry, other: None };
        let mut holds = Holds::default();
        let edited = Action::copy(Side::Destination, "Photos 2025/d.jpg", photo, None);
        holds.note(&edited, &Ok(Done::Skipped("changed on the destination side since the scan".into())));
        holds.note(&Action::copy(Side::Destination, "app/activity.log", Entry::file(7, 1), None), &Err(Error::Changed("changed".into())));
        assert!(matches!(holds.of(&removal(Side::Destination, "Photos/d.jpg", Entry::file(1, 1))), Some(Held::OldCopyOf("Photos 2025/d.jpg", Unlanded::Changed))));
        assert!(matches!(holds.of(&removal(Side::Destination, "Photos/renamed.jpg", photo)), Some(Held::OldCopyOf("Photos 2025/d.jpg", Unlanded::Changed))));
        assert!(holds.of(&removal(Side::Destination, "old-project/big.mov", Entry::file(9_000, 5))).is_none());
        assert!(holds.of(&removal(Side::Destination, "old-project", Entry::dir())).is_none());
        assert!(holds.of(&removal(Side::Source, "Photos/d.jpg", photo)).is_none(), "the other side");
        assert!(holds.of(&edited).is_none(), "only removals wait");

        holds.note(&Action::copy(Side::Destination, "a.jpg", photo, None), &Err(Error::Invalid("the drive failed".into())));
        assert!(matches!(holds.of(&removal(Side::Destination, "old-project/big.mov", Entry::file(9_000, 5))), Some(Held::Failed)));
        assert!(holds.of(&removal(Side::Source, "old-project/big.mov", Entry::file(9_000, 5))).is_none());
    }

    /// A removal held back because it may be the old copy of something that didn't land says
    /// why that didn't land, truthfully: it changed, the item of another kind in its place
    /// couldn't go first, or it only waits with the rest of its app library or database (a
    /// database whose journal changed, say), which doesn't mean it changed.
    #[test]
    fn a_held_removal_says_why_what_it_may_be_the_old_copy_of_didnt_land() {
        let (_tmp, shores) = shores();
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel);
        let mut holds = Holds::default();
        let copy = |path: &str, size: u64| Action::copy(Side::Destination, path, Entry::file(size, 1_600_000_000_000), None);
        holds.note(&copy("app/data.db", 10), &Ok(Done::Waits("left for the next sync: another file of its database couldn't be synced".into())));
        holds.note(&copy("app/data.db-wal", 20), &Ok(Done::Skipped("changed on the source side since the scan".into())));
        holds.note(&copy("notes", 30), &Ok(Done::Blocked("left for the next sync: the item of another kind in its place couldn't go first".into())));
        holds.note(&copy("report.txt", 40), &Err(Error::Changed("changed on the destination side since the scan".into())));
        let why = |path: &str| {
            let removal = Action::delete(Side::Destination, path, Entry::file(1, 1), None);
            holds.of(&removal).map(|held| executor.why_held(held, Side::Destination))
        };
        assert_eq!(
            why("old/data.db").as_deref(),
            Some("kept for now: it may be the old copy of “app/data.db”, which waits for the next sync with the rest of its app library or database")
        );
        assert_eq!(
            why("old/data.db-wal").as_deref(),
            Some("kept for now: it may be the old copy of “app/data.db-wal”, which changed since the scan and wasn't copied to the destination yet")
        );
        assert_eq!(
            why("Notes/notes").as_deref(),
            Some("kept for now: it may be the old copy of “notes”, which wasn't copied to the destination yet: the item of another kind in its place there couldn't go first")
        );
        assert_eq!(
            why("old/report.txt").as_deref(),
            Some("kept for now: it may be the old copy of “report.txt”, which changed since the scan and wasn't copied to the destination yet")
        );
        assert_eq!(why("old/other.txt"), None);
    }

    /// The room a library's new versions need in the trash, once all are written next to
    /// their targets (in a two-way job, which keeps every version it replaces). Each version
    /// replaced is copied into the trash right before its new version takes its place. On a
    /// local drive the old version's room is free again once it's removed: a Logic project
    /// whose two takes, each taking 60% of the drive's spare room, were both recorded again
    /// lands. One whose take is bigger than the whole drive can't: it waits, and the take is
    /// named. A take with another name (a hard link, as backup apps make) keeps its room once
    /// removed, and so may every take on a network share, whose server may keep a removed
    /// file's data in a snapshot: each of those needs room of its own.
    #[test]
    fn a_library_needs_room_for_its_replaced_versions_as_the_drive_gives_it_back() {
        let (tmp, shores) = shores();
        let Some(room) = Room::of(&shores.destination) else {
            eprintln!("SKIPPED a_library_needs_room_for_its_replaced_versions_as_the_drive_gives_it_back: the temporary folder's drive can't say how much room it has");
            return;
        };
        let take = room.free.saturating_sub(room.reserve()) / 10 * 6;
        if take == 0 {
            eprintln!("SKIPPED a_library_needs_room_for_its_replaced_versions_as_the_drive_gives_it_back: the temporary folder's drive has no room to spare");
            return;
        }
        let source = Snapshot::new();
        let on = |kind: FsKind, network: bool| {
            let mut destination = Snapshot::new();
            destination.facts_mut().volume = Some(Volume {
                kind,
                fs: String::new(),
                mount: "/x".into(),
                name: "x".into(),
                network,
                case_sensitive: false,
                case_guessed: false,
                refuses_links: false,
            });
            destination
        };
        let (local, share) = (on(FsKind::Ext, false), on(FsKind::Smb, true));
        let cancel = AtomicBool::new(false);
        let unit = Unit::Package("Song.logicx".into());
        let takes = |sizes: &[u64]| -> Vec<Action> {
            sizes
                .iter()
                .enumerate()
                .map(|(n, size)| Action::copy(Side::Destination, &format!("Song.logicx/take{n}.wav"), Entry::file(*size, 2_000), Some(Entry::file(*size, 1_000))))
                .collect()
        };
        fs::create_dir_all(shores.destination.join("Song.logicx")).unwrap();
        for n in 0..3 {
            write(&shores.destination.join(format!("Song.logicx/take{n}.wav")), "the take as it was", 1_000);
        }
        let recorded_again = takes(&[take, take]);
        let executor = Executor::new(&shores, &source, &local, KEEP, &cancel);
        assert!(executor.room_for_replaced(&unit, &staged_in(&shores.destination, &recorded_again)).is_none(), "each take of {take} bytes fits on its own");
        let too_big = takes(&[1_000, room.total, 2_000]);
        let (at, e) = executor.room_for_replaced(&unit, &staged_in(&shores.destination, &too_big)).expect("the take bigger than the drive doesn't fit");
        assert_eq!(at, 1, "the take that doesn't fit");
        assert!(e.to_string().contains("to keep the replaced versions of “Song.logicx” in .charon-trash"), "{e}");

        let executor = Executor::new(&shores, &source, &share, KEEP, &cancel);
        let both = Room::shortage(&shores.destination, take * 2, "the destination's drive").is_some();
        assert_eq!(executor.room_for_replaced(&unit, &staged_in(&shores.destination, &recorded_again)).is_some(), both, "both takes kept need room together");

        if fs::hard_link(shores.destination.join("Song.logicx/take0.wav"), tmp.path().join("take0 in an older backup.wav")).is_err() {
            eprintln!("SKIPPED a_library_needs_room_for_its_replaced_versions_as_the_drive_gives_it_back, hard links: the temporary folder's drive holds none");
            return;
        }
        let executor = Executor::new(&shores, &source, &local, KEEP, &cancel);
        assert!(
            executor.room_for_replaced(&unit, &staged_in(&shores.destination, &recorded_again)).is_some(),
            "the take with another name keeps its room: both takes kept need it together"
        );
    }

    /// The last look opens a file only where its drive may still answer from what its
    /// client kept: a drive that shows changes late, for an item settled in the last minute.
    #[test]
    fn the_last_look_opens_only_recent_files_on_drives_that_show_changes_late() {
        let (_tmp, shores) = shores();
        let mut share = Snapshot::new();
        share.facts_mut().volume = Some(Volume {
            kind: FsKind::Smb,
            fs: "smbfs".into(),
            mount: "/Volumes/share".into(),
            name: "share".into(),
            network: true,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        });
        let local = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &local, &share, KEEP, &cancel);
        assert!(executor.may_answer_from_cache(Side::Destination, Duration::from_secs(5)));
        assert!(!executor.may_answer_from_cache(Side::Destination, Duration::from_secs(120)));
        assert!(!executor.may_answer_from_cache(Side::Source, Duration::from_secs(5)));
    }

    /// A folder already at the place of a new one is only taken for it when Charon doesn't
    /// leave it alone: never a folder the scan left out, one whose name the drive keeps for
    /// itself, one the job excludes, or Charon's own trash made during the run.
    #[test]
    fn a_folder_charon_leaves_alone_is_never_written_into() {
        let (_tmp, shores) = shores();
        let source = Snapshot::new();
        let mut destination = Snapshot::new();
        destination.leave_out("lost+found".into(), Some("lost+found"), "named like a folder drives keep for themselves, so Charon leaves it alone");
        destination.reserve(".Trashes", ".Trashes");
        let cancel = AtomicBool::new(false);
        let filter = ExcludeFilter::new(&["Entw\u{fc}rfe".to_string()], true).unwrap();
        let executor = Executor::new(&shores, &source, &destination, KEEP, &cancel).excluding(filter);
        // "Entwürfe" and ".charon-trash" weren't there at the scan: the person made the one,
        // and the run made the other, for something it replaced.
        let left_alone = ["lost+found", ".Trashes", "Entw\u{fc}rfe", crate::scan::TRASH_FOLDER];
        for name in left_alone.iter().chain(&["photos"]) {
            fs::create_dir(shores.source.join(name)).unwrap();
            fs::create_dir(shores.destination.join(name)).unwrap();
        }
        for name in left_alone {
            assert!(matches!(executor.create_dir(Side::Destination, name), Err(Error::Invalid(_))), "{name}");
        }
        // A folder that is simply new since the scan is the person's own: it's taken.
        assert!(matches!(executor.create_dir(Side::Destination, "photos"), Ok(Done::Created)));
    }

    #[test]
    fn a_refused_name_is_remembered_with_the_place_the_drive_refused() {
        let (_tmp, shores) = shores();
        let report = Entry::file(4, 1_600_000_000_000);
        let mut source = Snapshot::new();
        source.insert("REPORT~1.PDF", report);
        let destination = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &source, &destination, KEEP, &cancel);
        let action = Action::copy(Side::Destination, "REPORT~1.PDF", report, None);
        // The drive finds another item ("Report final.pdf") at the copy's place.
        let at = shores.destination.join("REPORT~1.PDF");
        let (mut outcome, mut holds) = (Outcome::default(), Holds::default());
        let refused = Err(Error::Refused { path: at.clone(), why: Refusal::TakenName });
        assert!(executor.record(&mut outcome, &mut holds, &action, refused).is_continue());
        let expected = Refused { side: Side::Destination, key: "REPORT~1.PDF".into(), why: Refusal::TakenName, item: report, at };
        assert_eq!(outcome.refused, vec![expected]);
        assert_eq!(outcome.left_out.len(), 1);
        let removal = Action::Delete { side: Side::Destination, path: "old.txt".into(), item: EntryKind::File, entry: Entry::file(1, 1), other: None };
        assert!(matches!(holds.of(&removal), Some(Held::Failed)), "the run keeps its removals there");
    }

    #[test]
    fn conflict_copies_get_names_in_one_unicode_form() {
        let (_tmp, shores) = shores();
        let mut source = Snapshot::new();
        source.insert("caf\u{e9}.txt", Entry::file(1, 1));
        let mut destination = Snapshot::new();
        destination.insert("caf\u{e9}.txt", Entry::file(2, 2));
        // The destination is a Mac drive that stores names decomposed.
        destination.facts_mut().spellings.insert("caf\u{e9}.txt".into(), "cafe\u{301}.txt".into());
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &source, &destination, KEEP, &cancel);
        let key = executor.free_conflict_name("caf\u{e9}.txt").unwrap();
        for side in [Side::Source, Side::Destination] {
            let name = executor.spellings.disk(side, &key);
            assert!(unicode_normalization::is_nfc(&name), "{name:?} on the {}", side.word());
        }
    }

    /// The run hears of a conflict copy before it's made on either side: a change the folder
    /// watcher reports there from then on is the run's own write.
    #[test]
    fn a_conflict_copy_is_told_before_it_is_made() {
        let (_tmp, shores) = shores();
        write(&shores.source.join("plan.txt"), "the source's version", 1_700_000_000);
        write(&shores.destination.join("plan.txt"), "the destination's version", 1_700_000_100);
        let (source, destination) = (seen(&shores.source.join("plan.txt")), seen(&shores.destination.join("plan.txt")));
        let mut plan = Plan::default();
        plan.push(Action::KeepBoth { path: "plan.txt".into(), source_bytes: source.size, destination_bytes: destination.size, source, destination });
        // Each key told, and whether it was already there on either side when it was.
        let told: RefCell<Vec<(String, bool)>> = RefCell::default();
        let hear = |key: &str| {
            let there = [&shores.source, &shores.destination].into_iter().any(|root| root.join(key).exists());
            told.borrow_mut().push((key.to_string(), there));
        };
        let empty = Snapshot::new();
        let cancel = AtomicBool::new(false);
        let executor = Executor::new(&shores, &empty, &empty, KEEP, &cancel).telling_new_names(&hear);
        let observer = |_: &crate::model::Progress| {};
        let outcome = executor.run(&plan, &mut ProgressMeter::for_plan(&plan, &observer));
        assert!(outcome.failed.is_empty() && outcome.skipped.is_empty(), "{outcome:?}");

        let copy = names_in(&shores.destination).into_iter().find(|name| name.starts_with("plan (conflict ")).expect("both versions are kept");
        assert_eq!(fs::read_to_string(shores.source.join(&copy)).unwrap(), "the destination's version");
        assert_eq!(*told.borrow(), vec![(copy, false)], "told once, before the copy was there");
    }
}
