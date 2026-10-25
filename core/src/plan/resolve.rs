//! Settling one two-way conflict the way the person chose: keep the source's version, keep
//! the destination's, or keep both files.
//!
//! The plan only touches the conflict's own item (and, for a folder, what's inside it), and
//! goes through the same review and the same checks as any sync. Places Charon can't read
//! or must leave alone stay as they are, with a note. An app library or a database (see
//! `plan::units`) is settled as a whole: the other side's copy becomes the chosen one, file
//! by file, and what it replaces or removes goes to .charon-trash.

use crate::model::ConflictChoice;
use crate::plan::action::LEFT_ALONE;
use crate::plan::ground::Ground;
use crate::plan::{Action, Plan, Side, Unit};
use crate::scan::{Entry, Keys, Snapshot};
use std::collections::BTreeSet;

pub struct Resolution;

impl Resolution {
    /// The steps that settle the conflict at `key` by `choice`, or why it can't be done.
    pub fn plan(source: &Snapshot, destination: &Snapshot, key: &str, choice: ConflictChoice) -> Result<Plan, String> {
        let mut everything = Plan::default();
        let mut ground = Ground::new(source, destination, &[Side::Source, Side::Destination], &mut everything);
        // Only the notes about this item and what's inside it.
        let notes = everything
            .notes
            .into_iter()
            .filter(|note| {
                let snapshot = match note.side {
                    Some(Side::Destination) => destination,
                    _ => source,
                };
                let at = snapshot.key_for(&note.path);
                Keys::is_within(&at, key) || Keys::is_within(key, &at)
            })
            .collect();
        let mut plan = Plan { notes, ..Plan::default() };
        if ground.is_off_limits(key) {
            return Err("Charon can't settle this item: it's in a place it leaves alone (see the notes).".into());
        }
        let unit = Unit::of(key, source, destination)
            .filter(|unit| matches!(unit, Unit::Database(_)) || unit.key() == key);
        if let Some(unit) = unit {
            return Self::settle_unit(plan, ground, &unit, choice);
        }
        let (s, d) = (source.get(key), destination.get(key));
        match choice {
            ConflictChoice::KeepSource => Self::keep(&mut plan, &mut ground, key, Side::Source)?,
            ConflictChoice::KeepDestination => Self::keep(&mut plan, &mut ground, key, Side::Destination)?,
            ConflictChoice::KeepBoth => match (s, d) {
                (Some(src), Some(dst)) if src.is_file() && dst.is_file() => plan.push(Action::KeepBoth {
                    path: key.to_string(),
                    source_bytes: src.size,
                    destination_bytes: dst.size,
                    source: *src,
                    destination: *dst,
                }),
                _ => return Err("Only two files can both be kept. Choose one side's version.".into()),
            },
        }
        ground.finish(&mut plan);
        // A folder that holds items Charon leaves alone can't make way: settling the item
        // halfway would only empty part of that folder.
        if choice != ConflictChoice::KeepBoth {
            let winner = if choice == ConflictChoice::KeepSource { Side::Source } else { Side::Destination };
            let loser = winner.opposite();
            let wanted = ground.side(winner).get(key);
            let settled = plan.actions.iter().any(|action| match action {
                Action::Copy { to, path, .. } | Action::CreateDir { side: to, path } => wanted.is_some() && *to == loser && path == key,
                Action::Delete { side, path, .. } => wanted.is_none() && *side == loser && path == key,
                _ => false,
            });
            if !settled {
                return Err(format!(
                    "The {} has a folder “{}” holding {LEFT_ALONE}, so it can't be replaced. Choose the other version, or move your own items out of it first.",
                    loser.word(),
                    ground.side(loser).spelling(key)
                ));
            }
        }
        Ok(plan)
    }

    /// Settles a whole app library or database: the other side's copy becomes the chosen one,
    /// except for what that side's drive can never hold (see `Ground::keep_units_whole`).
    fn settle_unit(mut plan: Plan, ground: Ground, unit: &Unit, choice: ConflictChoice) -> Result<Plan, String> {
        let spelling = {
            let side = if ground.side(Side::Source).contains(unit.key()) { Side::Source } else { Side::Destination };
            ground.side(side).spelling(unit.key()).to_string()
        };
        let winner = match choice {
            ConflictChoice::KeepSource => Side::Source,
            ConflictChoice::KeepDestination => Side::Destination,
            ConflictChoice::KeepBoth => {
                return Err(format!(
                    "“{spelling}” is {}: its files only work together, so it can't be kept twice. \
                     Choose one side's version; what it replaces goes to .charon-trash.",
                    unit.describe()
                ))
            }
        };
        // An item one side's drive can never hold stays out of that side, and stays where it
        // is on the other.
        let keys: BTreeSet<String> = Self::unit_keys(&ground, unit).into_iter().filter(|key| !ground.is_never_there(key)).collect();
        if keys.iter().any(|key| ground.is_off_limits(key)) {
            return Err(format!(
                "Charon can't settle “{spelling}” as a whole: some of its files are in places it leaves alone (see the notes)."
            ));
        }
        Self::follow(&mut plan, &ground, &keys, winner);
        ground.finish(&mut plan);
        Ok(plan)
    }

    /// Every key of the unit on either side, in order (a folder before what's inside it).
    fn unit_keys(ground: &Ground, unit: &Unit) -> BTreeSet<String> {
        let (source, destination) = (ground.side(Side::Source), ground.side(Side::Destination));
        match unit {
            Unit::Package(folder) => std::iter::once(folder.clone())
                .chain(source.inside(folder).map(|(key, _)| key.clone()))
                .chain(destination.inside(folder).map(|(key, _)| key.clone()))
                .filter(|key| source.contains(key) || destination.contains(key))
                .collect(),
            Unit::Database(database) => {
                Unit::database_files(database).into_iter().filter(|key| source.contains(key) || destination.contains(key)).collect()
            }
        }
    }

    /// Makes every one of `keys` on the loser's side what it is on `winner`'s side.
    fn follow(plan: &mut Plan, ground: &Ground, keys: &BTreeSet<String>, winner: Side) {
        let loser = winner.opposite();
        // Items settled with everything inside them (removed, or brought over whole).
        let mut whole: Vec<&str> = Vec::new();
        for key in keys {
            if whole.iter().any(|top| Keys::is_within(key, top)) {
                continue;
            }
            let (from, here) = (ground.side(winner), ground.side(loser));
            match (from.get(key).copied(), here.get(key).copied()) {
                (Some(wanted), Some(existing)) if wanted.kind == existing.kind => {
                    if !wanted.is_dir() && !existing.same_as(&wanted) {
                        plan.push(Action::copy(loser, key, wanted, Some(existing)).settling(true));
                    }
                }
                (Some(wanted), Some(existing)) => {
                    Self::remove(plan, ground, loser, key, existing, Some(wanted));
                    Self::bring(plan, ground, loser, key, wanted);
                    whole.push(key);
                }
                (Some(wanted), None) => {
                    Self::bring(plan, ground, loser, key, wanted);
                    whole.push(key);
                }
                (None, Some(existing)) => {
                    Self::remove(plan, ground, loser, key, existing, None);
                    whole.push(key);
                }
                (None, None) => {}
            }
        }
    }

    /// Makes the other side match `winner`'s version of `key`.
    fn keep(plan: &mut Plan, ground: &mut Ground, key: &str, winner: Side) -> Result<(), String> {
        let loser = winner.opposite();
        let (from, here) = (ground.side(winner), ground.side(loser));
        match (from.get(key), here.get(key)) {
            (None, None) => Err("Neither side has this item anymore. Look again.".into()),
            (None, Some(existing)) => {
                Self::remove(plan, ground, loser, key, *existing, None);
                Ok(())
            }
            (Some(wanted), None) => {
                Self::bring(plan, ground, loser, key, *wanted);
                Ok(())
            }
            (Some(wanted), Some(existing)) if wanted.kind == existing.kind => {
                if wanted.is_dir() {
                    return Err("Both sides have a folder here, so there's nothing to settle. Look again.".into());
                }
                plan.push(Action::copy(loser, key, *wanted, Some(*existing)).settling(true));
                Ok(())
            }
            (Some(wanted), Some(existing)) => {
                Self::remove(plan, ground, loser, key, *existing, Some(*wanted));
                Self::bring(plan, ground, loser, key, *wanted);
                Ok(())
            }
        }
    }

    /// Removes `key` from `side`, with everything inside it. `instead`: what takes its place.
    fn remove(plan: &mut Plan, ground: &Ground, side: Side, key: &str, entry: Entry, instead: Option<Entry>) {
        let snapshot = ground.side(side);
        let inside: Vec<(String, Entry)> = snapshot.inside(key).map(|(path, entry)| (path.clone(), *entry)).collect();
        for (path, entry) in inside {
            if !ground.is_off_limits(&path) {
                plan.push(Action::delete(side, &path, entry, None));
            }
        }
        plan.push(Action::delete(side, key, entry, instead));
    }

    /// Creates `key` on `to` as the other side has it, with everything inside it.
    fn bring(plan: &mut Plan, ground: &Ground, to: Side, key: &str, wanted: Entry) {
        let from = ground.side(to.opposite());
        Self::bring_one(plan, to, key, &wanted);
        if wanted.is_dir() {
            let inside: Vec<(String, Entry)> = from.inside(key).map(|(path, entry)| (path.clone(), *entry)).collect();
            for (path, entry) in inside {
                if !ground.is_off_limits(&path) {
                    Self::bring_one(plan, to, &path, &entry);
                }
            }
        }
    }

    fn bring_one(plan: &mut Plan, to: Side, key: &str, wanted: &Entry) {
        if wanted.is_dir() {
            plan.push(Action::CreateDir { side: to, path: key.to_string() });
        } else {
            plan.push(Action::copy(to, key, *wanted, None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[(&str, Entry)]) -> Snapshot {
        items.iter().map(|(p, e)| (p.to_string(), *e)).collect()
    }

    #[test]
    fn keeping_one_version_replaces_the_other_as_a_settlement() {
        let (s, d) = (Entry::file(1, 10_000), Entry::file(2, 20_000));
        let plan = Resolution::plan(&snap(&[("a.txt", s)]), &snap(&[("a.txt", d)]), "a.txt", ConflictChoice::KeepSource).unwrap();
        assert_eq!(plan.actions, vec![Action::copy(Side::Destination, "a.txt", s, Some(d)).settling(true)]);
    }

    #[test]
    fn a_folder_meeting_a_file_is_settled_item_by_item() {
        let source = snap(&[("x", Entry::file(3, 1))]);
        let destination = snap(&[("x", Entry::dir()), ("x/a.txt", Entry::file(1, 1)), ("x/b.txt", Entry::file(1, 1))]);
        let plan = Resolution::plan(&source, &destination, "x", ConflictChoice::KeepSource).unwrap();
        let ordered: Vec<String> = plan.ordered().iter().map(|a| format!("{:?}", std::mem::discriminant(*a)) + a.path()).collect();
        assert_eq!(plan.actions.len(), 4, "{:?}", plan.actions);
        // The folder's contents and the folder go first, then the file takes its place.
        assert!(ordered.last().unwrap().ends_with('x'));
        assert!(matches!(plan.ordered().last(), Some(Action::Copy { .. })));
    }

    #[test]
    fn only_two_files_can_both_be_kept() {
        let source = snap(&[("x", Entry::file(3, 1))]);
        let destination = snap(&[("x", Entry::dir())]);
        assert!(Resolution::plan(&source, &destination, "x", ConflictChoice::KeepBoth).is_err());
    }

    #[test]
    fn a_folder_holding_unreadable_places_is_never_half_emptied() {
        let source = snap(&[]);
        let mut destination = snap(&[("x", Entry::dir()), ("x/locked", Entry::dir()), ("x/a.txt", Entry::file(1, 1))]);
        destination.freeze("x/locked", "permission denied");
        let refused = Resolution::plan(&source, &destination, "x", ConflictChoice::KeepSource).unwrap_err();
        assert!(refused.contains("leaves alone"), "{refused}");
        // The other way round touches nothing locked: it's fine.
        assert!(Resolution::plan(&source, &destination, "x", ConflictChoice::KeepDestination).is_ok());
    }

    #[test]
    fn a_folder_holding_excluded_items_is_never_half_emptied() {
        let source = snap(&[("x", Entry::file(3, 1))]);
        let mut destination = snap(&[("x", Entry::dir()), ("x/a.txt", Entry::file(1, 1)), ("x/b.txt", Entry::file(1, 1))]);
        destination.occupy("x"); // cache.tmp, excluded, is in there too
        let refused = Resolution::plan(&source, &destination, "x", ConflictChoice::KeepSource).unwrap_err();
        assert_eq!(
            refused,
            "The destination has a folder “x” holding items Charon leaves alone (excluded, left out or unreadable items, or what Charon or a cloud service keeps for its own use), so it can't be replaced. Choose the other version, or move your own items out of it first."
        );
    }

    #[test]
    fn a_library_is_settled_as_a_whole() {
        let lib = "Lib.photoslibrary";
        let source = snap(&[(lib, Entry::dir()), ("Lib.photoslibrary/a.jpg", Entry::file(5, 20_000)), ("Lib.photoslibrary/new.jpg", Entry::file(7, 20_000))]);
        let destination = snap(&[
            (lib, Entry::dir()),
            ("Lib.photoslibrary/a.jpg", Entry::file(1, 10_000)),
            ("Lib.photoslibrary/old", Entry::dir()),
            ("Lib.photoslibrary/old/b.jpg", Entry::file(6, 20_000)),
        ]);
        let plan = Resolution::plan(&source, &destination, lib, ConflictChoice::KeepSource).unwrap();
        // a.jpg replaced, new.jpg copied, old/b.jpg and old removed.
        assert_eq!(plan.actions.len(), 4, "{:?}", plan.actions);
        assert!(plan.actions.iter().all(|a| match a {
            Action::Copy { to, .. } | Action::CreateDir { side: to, .. } | Action::Delete { side: to, .. } => *to == Side::Destination,
            _ => false,
        }));
        assert!(plan.actions.contains(&Action::copy(Side::Destination, "Lib.photoslibrary/a.jpg", Entry::file(5, 20_000), Some(Entry::file(1, 10_000))).settling(true)));
        assert!(Resolution::plan(&source, &destination, lib, ConflictChoice::KeepBoth).is_err(), "a library is never kept twice");
    }

    /// A library holding a link the other side's drive can't hold is still settled as a
    /// whole: the chosen version goes over without the link, and the link stays where it is.
    #[test]
    fn a_library_is_settled_without_what_the_other_drive_can_never_hold() {
        let (lib, event, clip) = ("Movie.fcpbundle", "Movie.fcpbundle/CurrentVersion.fcpevent", "Movie.fcpbundle/clip.mov");
        let (on_source, on_destination) = (Entry::file(8, 20_000), Entry::file(6, 30_000));
        let source = snap(&[(lib, Entry::dir()), (event, on_source), (clip, Entry::link(b"/Volumes/Media/clip.mov", 1))]);
        let mut destination = snap(&[(lib, Entry::dir()), (event, on_destination)]);
        destination.refuse_links();
        let plan = Resolution::plan(&source, &destination, lib, ConflictChoice::KeepSource).unwrap();
        assert_eq!(plan.actions, vec![Action::copy(Side::Destination, event, on_source, Some(on_destination)).settling(true)]);
        assert!(plan.notes.iter().any(|n| n.path == clip), "{:?}", plan.notes);
        let plan = Resolution::plan(&source, &destination, lib, ConflictChoice::KeepDestination).unwrap();
        assert_eq!(plan.actions, vec![Action::copy(Side::Source, event, on_destination, Some(on_source)).settling(true)], "the link stays");
    }

    #[test]
    fn a_database_is_settled_with_its_journals() {
        let source = snap(&[("data.db", Entry::file(8, 20_000)), ("data.db-wal", Entry::file(0, 20_000))]);
        let destination = snap(&[("data.db", Entry::file(4, 10_000)), ("data.db-wal", Entry::file(3, 30_000)), ("data.db-shm", Entry::file(1, 30_000))]);
        let plan = Resolution::plan(&source, &destination, "data.db", ConflictChoice::KeepSource).unwrap();
        let mut paths: Vec<&str> = plan.actions.iter().map(|a| a.path()).collect();
        paths.sort();
        assert_eq!(paths, vec!["data.db", "data.db-shm", "data.db-wal"]);
        assert!(Resolution::plan(&source, &destination, "data.db", ConflictChoice::KeepBoth).is_err());
    }

    /// Only a folder is an app's package, and only files make a database: a single-file
    /// Numbers document can be kept twice, and settling a folder next to a file named like
    /// its journal settles that folder alone.
    #[test]
    fn items_only_named_like_a_library_or_database_are_settled_on_their_own() {
        let (s, d) = (Entry::file(2, 50_000), Entry::file(3, 90_000));
        let plan = Resolution::plan(&snap(&[("Budget.numbers", s)]), &snap(&[("Budget.numbers", d)]), "Budget.numbers", ConflictChoice::KeepBoth).unwrap();
        assert!(matches!(plan.actions.as_slice(), [Action::KeepBoth { .. }]), "{:?}", plan.actions);

        let source = snap(&[("Garden", Entry::dir()), ("Garden/plan.txt", Entry::file(1, 1))]);
        let destination = snap(&[("Garden-journal", Entry::dir()), ("Garden-journal/day1.txt", Entry::file(1, 1))]);
        let plan = Resolution::plan(&source, &destination, "Garden", ConflictChoice::KeepSource).unwrap();
        let mut paths: Vec<&str> = plan.actions.iter().map(|a| a.path()).collect();
        paths.sort();
        assert_eq!(paths, vec!["Garden", "Garden/plan.txt"], "the destination's “Garden-journal” stays: {:?}", plan.actions);
        assert!(plan.actions.iter().all(|a| !matches!(a, Action::Delete { .. })));
    }
}
