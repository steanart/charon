//! The steps of a sync, the notes that go with them, and the plan that holds both.

use crate::scan::{Entry, EntryKind, Keys};
use serde::{Deserialize, Serialize};

/// One of the two folders of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    Source,
    Destination,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Source => Side::Destination,
            Side::Destination => Side::Source,
        }
    }

    /// "source" or "destination", for sentences.
    pub fn word(self) -> &'static str {
        match self {
            Side::Source => "source",
            Side::Destination => "destination",
        }
    }
}

/// One step of a sync. Paths are keys relative to the job's folders (see `scan::Keys`).
///
/// Every step remembers the state it was planned against (`from`, `existing`, `entry`).
/// Right before it acts, the executor checks that the disk still looks like that; if
/// anything changed in between, the step is skipped instead of guessing.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Action {
    /// Create a folder on `side` (the other side has it).
    CreateDir { side: Side, path: String },
    /// Copy a file or link from the opposite side to `to`, replacing what's there if `replaces`.
    /// `settles_conflict`: both sides had changed and the newer version wins; the version it
    /// replaces always goes to the trash, even without safe deletes.
    Copy {
        to: Side,
        path: String,
        item: EntryKind,
        bytes: u64,
        replaces: bool,
        settles_conflict: bool,
        #[serde(skip)]
        from: Entry,
        #[serde(skip)]
        existing: Option<Entry>,
    },
    /// Remove an item from `side` (into the trash, with safe deletes). A folder is only
    /// removed once nothing but system clutter is left in it. `other` is what the other side
    /// had there (usually nothing): if that changed, the removal no longer makes sense.
    Delete {
        side: Side,
        path: String,
        item: EntryKind,
        #[serde(skip)]
        entry: Entry,
        #[serde(skip)]
        other: Option<Entry>,
    },
    /// Both sides changed the file: the destination's version is kept under a conflict name
    /// on both sides, and the source's version under the original name.
    KeepBoth {
        path: String,
        source_bytes: u64,
        destination_bytes: u64,
        #[serde(skip)]
        source: Entry,
        #[serde(skip)]
        destination: Entry,
    },
    /// Both sides changed the item and it needs the person: nothing happens to it.
    Conflict { path: String, source: Option<Entry>, destination: Option<Entry> },
}

impl Action {
    /// A copy of `from` (from the other side) onto `to`, over `existing` if there is one.
    pub fn copy(to: Side, path: &str, from: Entry, existing: Option<Entry>) -> Self {
        Action::Copy {
            to,
            path: path.to_string(),
            item: from.kind,
            bytes: if from.is_file() { from.size } else { 0 },
            replaces: existing.is_some(),
            settles_conflict: false,
            from,
            existing,
        }
    }

    /// The same copy, marked as deciding a conflict (see `Action::Copy`).
    pub fn settling(mut self, settles: bool) -> Self {
        if let Action::Copy { settles_conflict, .. } = &mut self {
            *settles_conflict = settles;
        }
        self
    }

    /// Removing `entry` from `side`, where the other side has `other` (usually nothing).
    pub fn delete(side: Side, path: &str, entry: Entry, other: Option<Entry>) -> Self {
        Action::Delete { side, path: path.to_string(), item: entry.kind, entry, other }
    }

    pub fn path(&self) -> &str {
        match self {
            Action::CreateDir { path, .. }
            | Action::Copy { path, .. }
            | Action::Delete { path, .. }
            | Action::KeepBoth { path, .. }
            | Action::Conflict { path, .. } => path,
        }
    }

    /// Bytes this step carries.
    pub fn bytes(&self) -> u64 {
        match self {
            Action::Copy { bytes, .. } => *bytes,
            Action::KeepBoth { source_bytes, destination_bytes, .. } => source_bytes + destination_bytes,
            _ => 0,
        }
    }

    /// The side a step changes, for the steps that change one side only.
    fn side(&self) -> Option<Side> {
        match self {
            Action::CreateDir { side, .. } | Action::Delete { side, .. } => Some(*side),
            Action::Copy { to, .. } => Some(*to),
            Action::KeepBoth { .. } | Action::Conflict { .. } => None,
        }
    }

    /// Whether the step changes anything on `side` (keeping both versions changes both).
    pub fn changes(&self, side: Side) -> bool {
        match self {
            Action::KeepBoth { .. } => true,
            _ => self.side() == Some(side),
        }
    }

    /// The folders whose contents the step changes: the folder it makes or removes, and
    /// every folder its item is in.
    pub fn folders(&self) -> impl Iterator<Item = &str> {
        let own = matches!(self, Action::CreateDir { .. } | Action::Delete { item: EntryKind::Dir, .. }).then_some(self.path());
        own.into_iter().chain(Keys::ancestors(self.path()))
    }

    /// The order a review lists steps in: what deserves a look first.
    fn review_rank(&self) -> u8 {
        match self {
            Action::Delete { .. } => 0,
            Action::Copy { settles_conflict: true, .. } => 1,
            Action::Copy { replaces: true, .. } => 2,
            Action::KeepBoth { .. } => 3,
            Action::Conflict { .. } => 4,
            Action::CreateDir { .. } => 5,
            Action::Copy { .. } => 6,
        }
    }
}

/// A place where an item of another kind takes an item's place (a folder that became a
/// file, a file where a new folder goes): the item there has to go first, with everything
/// inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Clearing<'a> {
    pub side: Side,
    pub key: &'a str,
}

impl Clearing<'_> {
    /// Whether `action` changes this place: a removal at or inside it, or a new folder or a
    /// copy that goes there.
    pub fn holds(&self, action: &Action) -> bool {
        action.side() == Some(self.side) && Keys::is_within(action.path(), self.key)
    }
}

/// One step of a plan in the order it runs (see `Plan::ordered`).
#[derive(Debug, Clone, Copy)]
pub struct Step<'a> {
    pub action: &'a Action,
    /// The place the step clears or fills, when an item of another kind takes an item's
    /// place there.
    pub clearing: Option<Clearing<'a>>,
}

impl<'a> Step<'a> {
    /// A removal that clears the way for an item of another kind.
    pub fn makes_room(&self) -> bool {
        self.clearing.is_some() && matches!(self.action, Action::Delete { .. })
    }

    /// A new folder or a copy that goes where an item of another kind has to go first.
    pub fn takes_place(&self) -> bool {
        self.clearing.is_some() && !matches!(self.action, Action::Delete { .. })
    }

    /// Where the step goes in the order of `Plan::ordered`: its stage, then, in a place that
    /// changes kind, the place and the step's part there, then its depth.
    fn rank(&self) -> (u8, &'a str, Option<Side>, u8, isize) {
        let depth = Keys::depth(self.action.path()) as isize;
        match (self.action, self.clearing) {
            (Action::CreateDir { .. }, None) => (0, "", None, 0, depth),
            (Action::Copy { .. } | Action::KeepBoth { .. }, None) => (1, "", None, 0, 0),
            (Action::Delete { .. }, Some(clearing)) => (2, clearing.key, Some(clearing.side), 0, -depth),
            (Action::CreateDir { .. }, Some(clearing)) => (2, clearing.key, Some(clearing.side), 1, depth),
            (_, Some(clearing)) => (2, clearing.key, Some(clearing.side), 2, 0),
            (Action::Delete { .. }, None) => (3, "", None, 0, -depth),
            (Action::Conflict { .. }, None) => (4, "", None, 0, 0),
        }
    }
}

/// Something the person should know about a plan that isn't a step: places that couldn't
/// be read, items left out, folders kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub kind: NoteKind,
    pub side: Option<Side>,
    /// The path as the person knows it (on-disk spelling where it matters).
    pub path: String,
    pub reason: String,
}

/// What keeps a folder, in the notes about it (see `NoteKind::KeptFolder`): it holds
/// something no step may touch, of any of these kinds.
pub(crate) const LEFT_ALONE: &str = "items Charon leaves alone (excluded, left out or unreadable items, or what Charon or a cloud service keeps for its own use)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteKind {
    /// Couldn't be read: nothing at or inside it is synced.
    Unreadable,
    /// Left out on purpose (links on Windows, special files, names that can't be carried).
    LeftOut,
    /// Two names that one of the drives would see as the same name.
    NameClash,
    /// A folder that should go but holds things Charon leaves alone.
    KeptFolder,
    /// Left alone for the reason the note gives: two kinds of item meet (file and folder) in
    /// a way this mode doesn't settle, a database with a file that can't be synced, or a
    /// removal that waits for a file Charon can't read.
    Blocked,
}

/// Everything one run would do, and what it won't do.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub notes: Vec<Note>,
}

impl Plan {
    pub fn push(&mut self, action: Action) {
        self.actions.push(action);
    }

    pub fn note(&mut self, kind: NoteKind, side: Option<Side>, path: impl Into<String>, reason: impl Into<String>) {
        self.notes.push(Note { kind, side, path: path.into(), reason: reason.into() });
    }

    /// The steps in the order they must run:
    ///
    /// 1. new folders, from the top down;
    /// 2. copies;
    /// 3. each place where an item of another kind takes an item's place (a folder that
    ///    became a file, a file where a new folder goes), one place after the other: the
    ///    item there with everything inside it, deepest first, then the new folders and the
    ///    copies that take its place;
    /// 4. every other removal, deepest first.
    ///
    /// Removing last means that a run which stops halfway (cancelled, a drive pulled out, a
    /// full disk) never leaves an item that was moved or renamed on one side with only one
    /// copy: the new place is filled before the old one is cleared. An item that has to
    /// make room goes as late as it can for the same reason: after every other copy, right
    /// before what takes its place (and `Executor::run` holds it back like any removal when
    /// copies failed).
    pub fn ordered(&self) -> Vec<&Action> {
        self.ordered_steps().into_iter().map(|step| step.action).collect()
    }

    /// `ordered`, with the place each step clears or fills when an item of another kind
    /// takes an item's place (stage 3 above).
    pub fn ordered_steps(&self) -> Vec<Step<'_>> {
        let clearings = self.clearings();
        let mut steps: Vec<Step<'_>> = self
            .actions
            .iter()
            .map(|action| Step { action, clearing: clearings.iter().copied().find(|clearing| clearing.holds(action)) })
            .collect();
        steps.sort_by_key(Step::rank); // stable: keeps path order within a stage
        steps
    }

    /// The places where an item of another kind takes an item's place.
    pub fn clearings(&self) -> Vec<Clearing<'_>> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Delete { side, path, other: Some(_), .. } => Some(Clearing { side: *side, key: path }),
                _ => None,
            })
            .collect()
    }

    /// The steps in the order a review shows them.
    pub fn for_review(&self) -> Vec<&Action> {
        let mut steps: Vec<&Action> = self.actions.iter().collect();
        steps.sort_by(|a, b| a.review_rank().cmp(&b.review_rank()).then_with(|| a.path().cmp(b.path())));
        steps
    }

    /// The steps that change something (conflicts left for the person don't).
    pub fn work(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| !matches!(a, Action::Conflict { .. }))
    }

    /// Whether the folders are already in sync (apart from conflicts waiting for a decision).
    pub fn is_settled(&self) -> bool {
        self.work().next().is_none()
    }

    /// The conflicts waiting for a decision: key and both sides' items.
    pub fn conflicts(&self) -> Vec<(&str, Option<Entry>, Option<Entry>)> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Conflict { path, source, destination } => Some((path.as_str(), *source, *destination)),
                _ => None,
            })
            .collect()
    }

    /// Steps that remove an item (a file, a link or a folder), on either side.
    pub fn removals(&self) -> u64 {
        self.actions.iter().filter(|a| matches!(a, Action::Delete { .. })).count() as u64
    }

    /// Files and links this plan removes from `side`.
    pub fn removed_on(&self, side: Side) -> u64 {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::Delete { side: s, item, .. } if *s == side && *item != EntryKind::Dir))
            .count() as u64
    }

    /// Files and links this plan removes or replaces on `side`: what could hurt if the plan
    /// were wrong.
    pub fn destructive_on(&self, side: Side) -> u64 {
        self.destructive_paths_on(side).count() as u64
    }

    /// The paths of `destructive_on`.
    pub fn destructive_paths_on(&self, side: Side) -> impl Iterator<Item = &str> {
        self.actions.iter().filter_map(move |a| {
            let destructive = match a {
                Action::Delete { side: s, item, .. } => *s == side && *item != EntryKind::Dir,
                Action::Copy { to, replaces: true, .. } => *to == side,
                Action::KeepBoth { .. } => side == Side::Destination,
                _ => false,
            };
            destructive.then(|| a.path())
        })
    }

    /// Counts for the review: "will copy 1,240 files (8.2 GB), replace 3, remove 3".
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for action in &self.actions {
            match action {
                Action::CreateDir { .. } => summary.new_folders += 1,
                Action::Copy { to, bytes, replaces, settles_conflict, .. } => {
                    summary.copies += 1;
                    summary.copy_bytes += bytes;
                    if *replaces {
                        summary.replaces += 1;
                    }
                    if *settles_conflict {
                        summary.newest_wins += 1;
                    }
                    match to {
                        Side::Destination => summary.to_destination += 1,
                        Side::Source => summary.to_source += 1,
                    }
                }
                Action::Delete { side, .. } => {
                    summary.deletes += 1;
                    match side {
                        Side::Destination => summary.deletes_on_destination += 1,
                        Side::Source => summary.deletes_on_source += 1,
                    }
                }
                Action::KeepBoth { .. } => {
                    summary.kept_both += 1;
                    summary.copy_bytes += action.bytes();
                }
                Action::Conflict { .. } => summary.conflicts += 1,
            }
        }
        summary.notes = self.notes.len() as u64;
        summary
    }
}

/// The counts a review shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub copies: u64,
    pub copy_bytes: u64,
    pub to_destination: u64,
    pub to_source: u64,
    /// Copies that replace an existing file (the old version goes to the trash with safe deletes).
    pub replaces: u64,
    pub new_folders: u64,
    pub deletes: u64,
    pub deletes_on_destination: u64,
    pub deletes_on_source: u64,
    pub kept_both: u64,
    /// Conflicts the newest version wins (counted among the copies and replacements too).
    pub newest_wins: u64,
    pub conflicts: u64,
    /// Things the person should know that aren't steps (unreadable places, kept folders…).
    pub notes: u64,
}

impl PlanSummary {
    /// Items that would change (what "pending" counts).
    pub fn changes(&self) -> u64 {
        self.copies + self.new_folders + self.deletes + self.kept_both
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(plan: &Plan) -> Vec<String> {
        plan.ordered()
            .iter()
            .map(|action| {
                let kind = match action {
                    Action::CreateDir { .. } => "new folder",
                    Action::Copy { .. } => "copy",
                    Action::Delete { .. } => "remove",
                    Action::KeepBoth { .. } => "keep both",
                    Action::Conflict { .. } => "conflict",
                };
                format!("{kind} {}", action.path())
            })
            .collect()
    }

    /// `Photos` renamed to `Photos 2025` in the source, and a file named `Photos` put in its
    /// place: the folder's items go after every other copy, right before the file takes
    /// their folder's place, and every other removal comes last.
    #[test]
    fn an_item_of_another_kind_takes_its_place_after_every_other_copy() {
        let photo = Entry::file(1, 1);
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, "Photos", Entry::dir(), Some(photo)));
        plan.push(Action::copy(Side::Destination, "Photos", photo, None));
        plan.push(Action::delete(Side::Destination, "Photos/a.jpg", photo, None));
        plan.push(Action::delete(Side::Destination, "old.txt", photo, None));
        plan.push(Action::CreateDir { side: Side::Destination, path: "Photos 2025".into() });
        plan.push(Action::copy(Side::Destination, "Photos 2025/a.jpg", photo, None));
        assert_eq!(
            order(&plan),
            vec!["new folder Photos 2025", "copy Photos 2025/a.jpg", "remove Photos/a.jpg", "remove Photos", "copy Photos", "remove old.txt"]
        );
        let steps = plan.ordered_steps();
        let roles: Vec<(bool, bool)> = steps.iter().map(|step| (step.makes_room(), step.takes_place())).collect();
        assert_eq!(roles, vec![(false, false), (false, false), (true, false), (true, false), (false, true), (false, false)]);
    }

    /// A file where a new folder goes: the file goes first, then the folder is made, then
    /// what goes into it is copied.
    #[test]
    fn a_new_folder_in_a_files_place_is_made_once_the_file_is_gone() {
        let note = Entry::file(1, 1);
        let mut plan = Plan::default();
        plan.push(Action::CreateDir { side: Side::Source, path: "notes".into() });
        plan.push(Action::CreateDir { side: Side::Source, path: "notes/2025".into() });
        plan.push(Action::copy(Side::Source, "notes/2025/june.txt", note, None));
        plan.push(Action::copy(Side::Source, "notes/todo.txt", note, None));
        plan.push(Action::delete(Side::Source, "notes", note, Some(Entry::dir())));
        plan.push(Action::copy(Side::Source, "readme.txt", note, None));
        assert_eq!(
            order(&plan),
            vec!["copy readme.txt", "remove notes", "new folder notes", "new folder notes/2025", "copy notes/2025/june.txt", "copy notes/todo.txt"]
        );
        // The other side's steps never belong to this side's place.
        assert!(!Clearing { side: Side::Destination, key: "notes" }.holds(&plan.actions[0]));
    }
}
