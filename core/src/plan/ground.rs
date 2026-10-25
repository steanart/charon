//! What both planners need besides the items themselves: the places they must not touch,
//! and a last look at folder removals once every step is known.

use crate::model::human_count;
use crate::plan::{Action, Baseline, NoteKind, Plan, Side, Unit};
use crate::scan::{is_sync_journal, Entry, EntryKind, Keys, Placeholder, Snapshot, Volume};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use unicode_normalization::UnicodeNormalization;

pub(crate) struct Ground<'a> {
    source: &'a Snapshot,
    destination: &'a Snapshot,
    /// Places nothing may touch on either side: unreadable on one side, names written two
    /// ways, names one side leaves out, items the receiving drive can't hold. Everything
    /// inside them is off-limits too.
    frozen: BTreeSet<String>,
    /// The places in `frozen` that the receiving drive doesn't have and can never hold: a
    /// link where it holds none, a name it can't take, a new file too big for it. An app
    /// library holding one is synced without it (see `keep_units_whole`).
    never_there: BTreeSet<String>,
    /// Places a planning decision put off-limits (a file meeting a folder), with everything inside.
    blocked: BTreeSet<String>,
}

impl<'a> Ground<'a> {
    /// The ground for a plan that may copy onto the sides in `receiving`, with a note in
    /// `plan` for every place left alone.
    pub fn new(source: &'a Snapshot, destination: &'a Snapshot, receiving: &[Side], plan: &mut Plan) -> Self {
        let mut ground = Self { source, destination, frozen: BTreeSet::new(), never_there: BTreeSet::new(), blocked: BTreeSet::new() };
        for side in [Side::Source, Side::Destination] {
            let snapshot = ground.side(side);
            for (key, reason) in snapshot.frozen_places() {
                let kind = if snapshot.is_clash(key) { NoteKind::NameClash } else { NoteKind::Unreadable };
                plan.note(kind, Some(side), snapshot.spelling(key), reason.clone());
                ground.frozen.insert(key.clone());
            }
            for skipped in snapshot.skipped() {
                plan.note(NoteKind::LeftOut, Some(side), skipped.path.clone(), skipped.reason.clone());
                // Its name is left alone on both sides, under this one note: a copy into its
                // place would fail on every sync and hold back every removal on that side.
                if let Some(key) = &skipped.key {
                    ground.frozen.insert(key.clone());
                }
            }
        }
        for (side, key, reason) in ground.taken_for_reserved() {
            plan.note(NoteKind::LeftOut, Some(side), ground.side(side).spelling(&key), reason);
            ground.frozen.insert(key);
        }
        for &to in receiving {
            for (key, reason, never_there) in ground.unholdable(to) {
                let from = ground.side(to.opposite());
                plan.note(NoteKind::LeftOut, Some(to.opposite()), from.spelling(&key), reason);
                if never_there {
                    ground.never_there.insert(key.clone());
                }
                ground.frozen.insert(key);
            }
        }
        ground.keep_units_whole(plan);
        ground
    }

    /// The files of an app library or a database only go together (see `Unit`): when one of
    /// them is left alone, so are the others. Otherwise a sync would carry some of a
    /// library's files into a library whose other files stay as they were (an older version
    /// of the one left alone, or one whose change Charon can't see), or a journal without
    /// the database it was written for, and the copy would no longer fit together.
    ///
    /// An item the receiving drive doesn't have and can never hold is left out of a library
    /// on its own: no version of it can ever be in the library there, so none can be left
    /// behind. A database's files still never go without each other.
    fn keep_units_whole(&mut self, plan: &mut Plan) {
        let exists = |key: &str| self.source.contains(key) || self.destination.contains(key);
        let shown = |key: &str| if self.source.contains(key) { self.source.spelling(key).to_string() } else { self.destination.spelling(key).to_string() };
        // Each library kept, with the item left alone inside it that keeps it.
        let mut libraries: Vec<(String, String)> = Vec::new();
        for frozen in &self.frozen {
            if self.never_there.contains(frozen) && Unit::database_of(frozen, self.source, self.destination).is_none() {
                continue;
            }
            let Some(Unit::Package(library)) = Unit::of(frozen, self.source, self.destination) else { continue };
            if library != *frozen && exists(&library) && !self.is_frozen(&library) && !libraries.iter().any(|(other, _)| *other == library) {
                libraries.push((library, shown(frozen)));
            }
        }
        for (library, frozen) in libraries {
            let side = if self.source.contains(&library) { Side::Source } else { Side::Destination };
            plan.note(
                NoteKind::Blocked,
                Some(side),
                self.side(side).spelling(&library),
                format!("left alone: “{frozen}” inside it can't be synced, and the files of an app library or document only go together"),
            );
            self.block(&library);
        }
        // Each database file kept, with the file left alone that keeps it.
        let mut kept: Vec<(String, String)> = Vec::new();
        for frozen in &self.frozen {
            let Some(database) = Unit::database_of(frozen, self.source, self.destination) else { continue };
            for file in Unit::database_files(&database) {
                if file != *frozen && exists(&file) && !self.is_off_limits(&file) && !kept.iter().any(|(other, _)| *other == file) {
                    kept.push((file, shown(frozen)));
                }
            }
        }
        for (file, frozen) in kept {
            let side = if self.source.contains(&file) { Side::Source } else { Side::Destination };
            plan.note(
                NoteKind::Blocked,
                Some(side),
                self.side(side).spelling(&file),
                format!("left alone: “{frozen}”, a file of the same database, can't be synced, and a database's files only go together"),
            );
            self.block(&file);
        }
    }

    /// The same ground, without notes (for working out a baseline).
    pub fn quiet(source: &'a Snapshot, destination: &'a Snapshot, receiving: &[Side]) -> Self {
        Self::new(source, destination, receiving, &mut Plan::default())
    }

    /// Items on either side whose key is a name one of the sides keeps for Charon's or the
    /// drive's own use (see `Snapshot::reserved`), with the side they are on and the reason
    /// they are left alone. Such names are matched as they are written, so the same name
    /// written another way has the key of the one kept, and a drive that ignores case would
    /// take it for that one. The scans already leave out the names of Charon's folders and
    /// the drive's in other upper and lower case; this catches any other spelling of a name
    /// one side keeps.
    fn taken_for_reserved(&self) -> Vec<(Side, String, String)> {
        let mut reserved: BTreeMap<&String, (Side, &String)> = BTreeMap::new();
        for side in [Side::Source, Side::Destination] {
            for (key, disk) in self.side(side).reserved() {
                reserved.entry(key).or_insert((side, disk));
            }
        }
        let mut found = Vec::new();
        for (key, (side, disk)) in reserved {
            if self.is_frozen(key) {
                continue;
            }
            for holder in [Side::Source, Side::Destination] {
                if self.side(holder).contains(key) {
                    let reason = match Placeholder::named(Keys::name(disk)) {
                        Some(placeholder) => format!(
                            "left out: on the {}, “{disk}” is the placeholder {} left for a file that's only in the cloud, so Charon leaves this name alone",
                            side.word(),
                            placeholder.service.name()
                        ),
                        None if is_sync_journal(Keys::name(disk)) => format!(
                            "left out: on the {}, “{disk}” is a file Nextcloud or ownCloud keeps to sync its folder, so Charon leaves this name alone",
                            side.word()
                        ),
                        None => format!(
                            "left out: its name differs only in upper and lower case from “{disk}” on the {}, a name Charon or the drive keeps for itself",
                            side.word()
                        ),
                    };
                    found.push((holder, key.clone(), reason));
                }
            }
        }
        found
    }

    /// Items on the other side that side `to`'s drive can't hold: names it can't take, files
    /// too big for it, links where it has none, and what it refused in an earlier sync. Only
    /// the topmost such place is listed, with the reason, and whether it's never there: the
    /// drive doesn't have it, and can never hold it (it didn't only refuse it once). Nor does
    /// Charon make an item a cloud service would take for its placeholder there: the service
    /// could take the file it names for freed up.
    fn unholdable(&self, to: Side) -> Vec<(String, String, bool)> {
        let (from, here) = (self.side(to.opposite()), self.side(to));
        let drive = here.volume().map(Volume::describe).unwrap_or_else(|| format!("{}'s drive", to.word()));
        let mut found: Vec<(String, String, bool)> = Vec::new();
        for (key, entry) in from.iter() {
            if self.is_frozen(key) || found.last().is_some_and(|(top, ..)| Keys::is_within(key, top)) {
                continue;
            }
            let existing = here.get(key);
            let (reason, never_there) = if let Some(why) = here.refused(key) {
                (Some(format!("an earlier sync found that {}", why.reason(&drive))), false)
            } else if let Some(service) = here.placeholder_service(key) {
                // Not for good: the person can remove an old copy of a placeholder.
                let reason = format!(
                    "it's named like a placeholder {} leaves for a file that's only in the cloud, and on the {} {} would take it for its own (if it's an old copy of one, remove it)",
                    service.name(),
                    to.word(),
                    service.name()
                );
                (Some(reason), false)
            } else if existing.is_none() {
                let name: String = Keys::name(from.spelling(key)).nfc().collect();
                let volume = here.volume();
                let reason = volume.and_then(|volume| volume.cannot_hold(&name)).or_else(|| match entry.kind {
                    EntryKind::Link if !here.holds_links() => Some(format!("it's a symbolic link, and the {drive} can't hold links")),
                    EntryKind::File => volume.and_then(|volume| volume.too_big(entry.size)),
                    _ => None,
                });
                (reason, true)
            } else if entry.is_file() && existing.is_some_and(Entry::is_file) {
                (here.volume().and_then(|volume| volume.too_big(entry.size)), false)
            } else {
                (None, false)
            };
            if let Some(reason) = reason {
                found.push((key.clone(), format!("left out: {reason}"), never_there));
            }
        }
        found
    }

    fn is_frozen(&self, key: &str) -> bool {
        self.frozen.contains(key) || Keys::ancestors(key).any(|folder| self.frozen.contains(folder))
    }

    pub fn side(&self, side: Side) -> &'a Snapshot {
        match side {
            Side::Source => self.source,
            Side::Destination => self.destination,
        }
    }

    /// Whether `key` is in a place the receiving drive doesn't have and can never hold (see
    /// `keep_units_whole`).
    pub fn is_never_there(&self, key: &str) -> bool {
        self.never_there.contains(key) || Keys::ancestors(key).any(|folder| self.never_there.contains(folder))
    }

    /// Whether `key` is in a place nothing may touch.
    pub fn is_off_limits(&self, key: &str) -> bool {
        self.is_frozen(key) || self.blocked.contains(key) || Keys::ancestors(key).any(|folder| self.blocked.contains(folder))
    }

    /// Puts `key` and everything inside it off-limits for the rest of the plan.
    pub fn block(&mut self, key: &str) {
        self.blocked.insert(key.to_string());
    }

    /// Whether anything inside `folder` on `side` differs from that side's record in the
    /// baseline (was added or edited there since the last sync).
    pub fn holds_changes(&self, side: Side, folder: &str, baseline: &Baseline) -> bool {
        let snapshot = self.side(side);
        let shifting = snapshot.shifts_times();
        snapshot.inside(folder).any(|(key, entry)| !Entry::still_on(Some(entry), baseline.side(key, side), shifting))
    }

    /// While a file on one side can't be read (see `Snapshot::unread`), it can't be copied
    /// to the other side, and nothing is removed there either: as in the run that couldn't
    /// read it, a removal could take the last copy of what the file holds (a file renamed,
    /// then held open by its program). Each removal of a file or link waits there, with a
    /// note, until the file is copied or left out; its folders stay too (see `finish`). The
    /// files of an app library or a database only go together, so the rest of its library or
    /// database waits as well. Removals that make way for an item of another kind stay, as
    /// they do in a run.
    pub fn hold_removals(&self, plan: &mut Plan) {
        let reasons = (self.held_reason(Side::Source), self.held_reason(Side::Destination));
        let reason_on = |side: Side| match side {
            Side::Source => reasons.0.as_ref(),
            Side::Destination => reasons.1.as_ref(),
        };
        let held: BTreeSet<(Side, String)> = plan
            .ordered_steps()
            .into_iter()
            .filter_map(|step| match step.action {
                Action::Delete { side, path, item, .. } if *item != EntryKind::Dir && !step.makes_room() && reason_on(*side).is_some() => Some((*side, path.clone())),
                _ => None,
            })
            .collect();
        if held.is_empty() {
            return;
        }
        // The libraries and databases of the files kept, each with the side and the name of
        // one of those files.
        let mut units: Vec<(Unit, Side, String)> = Vec::new();
        for (side, key) in &held {
            if let Some(unit) = Unit::of(key, self.source, self.destination) {
                if !units.iter().any(|(other, ..)| *other == unit) {
                    units.push((unit, *side, self.side(*side).spelling(key).to_string()));
                }
            }
        }
        // The libraries with steps left alone, each noted once, on the side its file is kept.
        let mut libraries: Vec<(&Unit, Side, &String)> = Vec::new();
        for action in std::mem::take(&mut plan.actions) {
            let unit = units.iter().find(|(unit, ..)| unit.holds(action.path()));
            let (side, reason) = match (&action, unit) {
                (Action::Conflict { .. }, _) => {
                    plan.push(action);
                    continue;
                }
                (Action::Delete { side, path, .. }, _) if held.contains(&(*side, path.clone())) => (*side, reason_on(*side).cloned().unwrap_or_default()),
                (_, Some((library @ Unit::Package(_), side, kept))) => {
                    if !libraries.iter().any(|(other, ..)| *other == library) {
                        libraries.push((library, *side, kept));
                    }
                    continue;
                }
                (_, Some((Unit::Database(_), _, kept))) => (
                    Self::side_shown(&action),
                    format!("left alone for now: the removal of “{kept}”, a file of the same database, waits, and a database's files only go together"),
                ),
                (_, None) => {
                    plan.push(action);
                    continue;
                }
            };
            plan.note(NoteKind::Blocked, Some(side), self.side(side).spelling(action.path()), reason);
        }
        for (library, side, kept) in libraries {
            plan.note(
                NoteKind::Blocked,
                Some(side),
                self.side(side).spelling(library.key()),
                format!("left alone for now: the removal of “{kept}” inside it waits, and the files of an app library or document only go together"),
            );
        }
    }

    /// Why removals on `side` wait (see `hold_removals`), or `None` when nothing on the
    /// other side is a file Charon can't read.
    fn held_reason(&self, side: Side) -> Option<String> {
        let from = side.opposite();
        let snapshot = self.side(from);
        let unread: Vec<&str> = snapshot.unread().map(|key| snapshot.spelling(key)).collect();
        let first = unread.first()?;
        Some(match unread.len() {
            1 => format!(
                "kept for now: Charon can't read “{first}” on the {}, so it isn't copied here, and this could be the last copy of what it holds \
                 (if it was renamed, say). This goes once “{first}” is copied, or left out",
                from.word()
            ),
            n => format!(
                "kept for now: Charon can't read {} files on the {} (“{first}” and {} more), so they aren't copied here, and this could be the last copy \
                 of what one of them holds (if it was renamed, say). This goes once they're copied, or left out",
                human_count(n as u64),
                from.word(),
                human_count(n as u64 - 1)
            ),
        })
    }

    /// The side whose item a step is about, for a note: the side it's copied from, or the
    /// side it's removed from.
    fn side_shown(action: &Action) -> Side {
        match action {
            Action::Copy { to, .. } => to.opposite(),
            Action::Delete { side, .. } | Action::CreateDir { side, .. } => *side,
            Action::KeepBoth { .. } | Action::Conflict { .. } => Side::Source,
        }
    }

    /// A folder is only removed when everything inside it is removed by this plan too.
    /// Otherwise the removal is dropped (whatever stays keeps it), and anything that wanted
    /// to take the folder's place is dropped with a note.
    pub fn finish(&self, plan: &mut Plan) {
        let deleted: HashSet<(Side, String)> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Delete { side, path, .. } => Some((*side, path.clone())),
                _ => None,
            })
            .collect();
        let kept: Vec<(Side, String)> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Delete { side, path, item: EntryKind::Dir, .. } => {
                    let snapshot = self.side(*side);
                    let all_go = !snapshot.is_occupied(path)
                        && snapshot.inside(path).all(|(inner, _)| deleted.contains(&(*side, inner.clone())));
                    (!all_go).then(|| (*side, path.clone()))
                }
                _ => None,
            })
            .collect();
        if kept.is_empty() {
            return;
        }
        let kept: HashSet<(Side, String)> = kept.into_iter().collect();
        let mut clashes = Vec::new();
        plan.actions.retain(|a| match a {
            Action::Delete { side, path, item: EntryKind::Dir, .. } => !kept.contains(&(*side, path.clone())),
            Action::Copy { to, path, .. } if kept.contains(&(*to, path.clone())) => {
                clashes.push((*to, path.clone()));
                false
            }
            _ => true,
        });
        for (side, path) in clashes {
            plan.note(
                NoteKind::Blocked,
                Some(side),
                self.side(side).spelling(&path).to_string(),
                "left alone: a folder with this name is still in the way",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{FsKind, Volume};

    fn snap(items: &[(&str, Entry)], kind: FsKind) -> Snapshot {
        let mut s: Snapshot = items.iter().map(|(k, e)| (k.to_string(), *e)).collect();
        s.facts_mut().volume = Some(Volume { kind, fs: "x".into(), mount: "/m".into(), name: "Stick".into(), network: false, case_sensitive: false, case_guessed: false, refuses_links: false });
        s
    }

    #[test]
    fn items_the_receiving_drive_cant_hold_are_left_alone() {
        let source = snap(
            &[
                ("a:b.txt", Entry::file(1, 1)),
                ("big.mov", Entry::file(5_000_000_000, 1)),
                ("shortcut", Entry::link(b"x", 1)),
                ("fine.txt", Entry::file(1, 1)),
            ],
            FsKind::Apfs,
        );
        let destination = snap(&[], FsKind::Fat);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits("a:b.txt"));
        assert!(ground.is_off_limits("big.mov"));
        assert!(ground.is_off_limits("shortcut"));
        assert!(!ground.is_off_limits("fine.txt"));
        assert_eq!(plan.notes.len(), 3);
        // The source never receives anything in a one-way job: nothing to check there.
        let ground = Ground::new(&destination, &source, &[], &mut Plan::default());
        assert!(!ground.is_off_limits("a:b.txt"));
    }

    #[test]
    fn what_a_drive_refused_in_an_earlier_sync_is_left_out_with_everything_inside() {
        let source = snap(
            &[
                ("q3: plans", Entry::dir()),
                ("q3: plans/a.txt", Entry::file(1, 1)),
                ("big.iso", Entry::file(9, 1)),
                ("fine.txt", Entry::file(1, 1)),
            ],
            FsKind::Ext,
        );
        let mut destination = snap(&[("big.iso", Entry::file(5, 1))], FsKind::Other);
        destination.refuse("q3: plans", crate::scan::Refusal::Name);
        destination.refuse("big.iso", crate::scan::Refusal::TooBig);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits("q3: plans/a.txt"));
        assert!(ground.is_off_limits("big.iso"), "even over an older version the drive holds");
        assert!(!ground.is_off_limits("fine.txt"));
        assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
        assert!(plan.notes.iter().all(|n| n.kind == NoteKind::LeftOut && n.reason.starts_with("left out: an earlier sync found that")));
    }

    #[test]
    fn a_name_left_out_on_one_side_is_off_limits_on_both_under_one_note() {
        let source = snap(
            &[
                ("channel", Entry::file(1, 1)),
                ("music", Entry::dir()),
                ("music/a.mp3", Entry::file(1, 1)),
                ("fine.txt", Entry::file(1, 1)),
            ],
            FsKind::Ext,
        );
        let mut destination = snap(&[], FsKind::Ext);
        destination.leave_out("channel".into(), Some("channel"), "isn't a file or a folder (a socket, pipe or device)");
        destination.leave_out("music".into(), Some("music"), "is a link or junction; Charon leaves those alone on Windows");
        destination.leave_out("b\u{fffd}d".into(), None, "its name isn't valid Unicode, so Charon can't carry it");
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Source, Side::Destination], &mut plan);
        assert!(ground.is_off_limits("channel"));
        assert!(ground.is_off_limits("music/a.mp3"), "with everything inside");
        assert!(!ground.is_off_limits("fine.txt"));
        assert_eq!(plan.notes.len(), 3, "one note for each item left out: {:?}", plan.notes);
        assert!(plan.notes.iter().all(|n| n.kind == NoteKind::LeftOut && n.side == Some(Side::Destination)));
    }

    /// A name one side keeps for a sync client's journal is left alone on both, and the
    /// note says whose it is.
    #[test]
    fn a_name_kept_for_a_sync_clients_journal_is_left_alone_on_both_sides() {
        let source = snap(&[(".sync_0123456789ab.db", Entry::dir()), ("fine.txt", Entry::file(1, 1))], FsKind::Ext);
        let mut destination = snap(&[], FsKind::Ext);
        destination.reserve(".sync_0123456789ab.db", ".sync_0123456789ab.db");
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits(".sync_0123456789ab.db"));
        assert!(!ground.is_off_limits("fine.txt"));
        let notes: Vec<(&str, Option<Side>, &str)> = plan.notes.iter().map(|n| (n.path.as_str(), n.side, n.reason.as_str())).collect();
        assert_eq!(
            notes,
            vec![(
                ".sync_0123456789ab.db",
                Some(Side::Source),
                "left out: on the destination, “.sync_0123456789ab.db” is a file Nextcloud or ownCloud keeps to sync its folder, so Charon leaves this name alone"
            )]
        );
    }

    #[test]
    fn a_database_with_a_file_left_alone_is_left_alone_whole() {
        let mut source = snap(
            &[("notes.db", Entry::file(8, 2)), ("notes.db-wal", Entry::file(4, 2)), ("other.db", Entry::file(8, 1)), ("other.db-wal", Entry::file(4, 1))],
            FsKind::Ext,
        );
        source.freeze("notes.db", "permission denied");
        let destination = snap(&[("notes.db", Entry::file(8, 1)), ("notes.db-shm", Entry::file(2, 1))], FsKind::Ext);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits("notes.db"));
        assert!(ground.is_off_limits("notes.db-wal"), "its journal never goes without it");
        assert!(ground.is_off_limits("notes.db-shm"));
        assert!(!ground.is_off_limits("other.db") && !ground.is_off_limits("other.db-wal"));
        let kept: Vec<(&str, Option<Side>)> = plan.notes.iter().filter(|n| n.kind == NoteKind::Blocked).map(|n| (n.path.as_str(), n.side)).collect();
        assert_eq!(kept, vec![("notes.db-wal", Some(Side::Source)), ("notes.db-shm", Some(Side::Destination))]);
        assert!(plan.notes.iter().all(|n| n.kind != NoteKind::Blocked || n.reason.contains("“notes.db”")), "{:?}", plan.notes);
    }

    /// A folder "notes" next to a file "notes-journal" that can't be read: only that file is
    /// left alone, the folder syncs.
    #[test]
    fn a_folder_is_never_kept_for_a_file_named_like_its_journal() {
        let mut source = snap(&[("notes", Entry::dir()), ("notes/new.txt", Entry::file(1, 2)), ("notes-journal", Entry::file(4, 2))], FsKind::Ext);
        source.freeze_unread("notes-journal", "permission denied");
        let destination = snap(&[("notes", Entry::dir())], FsKind::Ext);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits("notes-journal"));
        assert!(!ground.is_off_limits("notes") && !ground.is_off_limits("notes/new.txt"));
        assert!(plan.notes.iter().all(|n| n.kind != NoteKind::Blocked), "{:?}", plan.notes);
    }

    #[test]
    fn nothing_is_removed_where_a_file_that_cant_be_read_should_go() {
        // Mirror: "mail/old.pst" was renamed "mail/new.pst" on the source, where Charon
        // can't read it.
        let mut source = snap(
            &[("keep.txt", Entry::file(1, 1)), ("mail", Entry::dir()), ("mail/new.pst", Entry::file(8, 1)), ("notes.db", Entry::file(9, 60_000))],
            FsKind::Ext,
        );
        source.freeze_unread("mail/new.pst", "permission denied");
        let destination = snap(
            &[
                ("keep.txt", Entry::file(1, 1)),
                ("mail", Entry::dir()),
                ("mail/old.pst", Entry::file(8, 1)),
                ("notes.db", Entry::file(8, 1)),
                ("notes.db-wal", Entry::file(4, 1)),
                ("old", Entry::dir()),
                ("old/gone.txt", Entry::file(1, 1)),
            ],
            FsKind::Ext,
        );
        let plan = crate::plan::OneWayPlanner::mirror().plan(&source, &destination);
        assert!(plan.actions.is_empty(), "nothing goes, and the database isn't half replaced: {:?}", plan.actions);
        let mut kept: Vec<(&str, Option<Side>)> = plan.notes.iter().filter(|n| n.kind == NoteKind::Blocked).map(|n| (n.path.as_str(), n.side)).collect();
        kept.sort();
        assert_eq!(
            kept,
            vec![
                ("mail/old.pst", Some(Side::Destination)),
                ("notes.db", Some(Side::Source)),
                ("notes.db-wal", Some(Side::Destination)),
                ("old/gone.txt", Some(Side::Destination)),
            ]
        );
        let reason = |path: &str| plan.notes.iter().find(|n| n.path == path).map(|n| n.reason.as_str()).unwrap_or_default();
        assert!(reason("mail/old.pst").starts_with("kept for now: Charon can't read “mail/new.pst” on the source"), "{}", reason("mail/old.pst"));
        assert!(reason("notes.db").contains("“notes.db-wal”"), "{}", reason("notes.db"));
        assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Unreadable && n.path == "mail/new.pst"));

        // Two-way: only the side the file should go to waits.
        let file = Entry::file(8, 1);
        let mut a = snap(&[("both.txt", file), ("gone-on-b.txt", file), ("new.pst", file)], FsKind::Ext);
        a.freeze_unread("new.pst", "permission denied");
        let b = snap(&[("both.txt", file), ("old.pst", file)], FsKind::Ext);
        let baseline: Baseline = ["both.txt", "gone-on-b.txt", "old.pst"].iter().map(|key| (key.to_string(), crate::plan::Pair::new(file, file))).collect();
        let plan = crate::plan::TwoWayPlanner::new(crate::model::ConflictRule::Ask).plan(&a, &b, &baseline);
        assert_eq!((plan.removed_on(Side::Source), plan.removed_on(Side::Destination)), (1, 0), "{:?}", plan.actions);
        assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == "old.pst" && n.side == Some(Side::Destination)), "{:?}", plan.notes);
    }

    /// When the removal of a library's file waits, the rest of the library waits too, under
    /// one note for the library.
    #[test]
    fn a_library_whose_removal_waits_waits_whole() {
        let lib = "Song.logicx";
        let item = |name: &str| format!("{lib}/{name}");
        let mut source = snap(&[(lib, Entry::dir()), (&item("take.wav"), Entry::file(9, 60_000)), ("new.pst", Entry::file(8, 1))], FsKind::Ext);
        source.freeze_unread("new.pst", "permission denied");
        let destination = snap(
            &[(lib, Entry::dir()), (&item("take.wav"), Entry::file(8, 1)), (&item("old.wav"), Entry::file(8, 1)), ("old.txt", Entry::file(1, 1))],
            FsKind::Ext,
        );
        let plan = crate::plan::OneWayPlanner::mirror().plan(&source, &destination);
        assert!(plan.actions.is_empty(), "the library isn't half replaced: {:?}", plan.actions);
        let mut kept: Vec<(&str, Option<Side>)> = plan.notes.iter().filter(|n| n.kind == NoteKind::Blocked).map(|n| (n.path.as_str(), n.side)).collect();
        kept.sort();
        let old = item("old.wav");
        assert_eq!(kept, vec![(lib, Some(Side::Destination)), (old.as_str(), Some(Side::Destination)), ("old.txt", Some(Side::Destination))]);
        let reason = plan.notes.iter().find(|n| n.path == lib).map(|n| n.reason.as_str()).unwrap_or_default();
        assert!(reason.contains(&format!("“{old}”")), "{reason}");
    }

    #[test]
    fn an_app_library_with_an_item_left_alone_is_left_alone_whole() {
        let lib = "Photos Library.photoslibrary";
        let item = |name: &str| format!("{lib}/{name}");
        let mut source = snap(&[(lib, Entry::dir()), (&item("database"), Entry::dir()), (&item("database/b.db"), Entry::file(8, 2)), (&item("a.jpg"), Entry::file(4, 2))], FsKind::Ext);
        source.freeze_unread(&item("database/b.db"), "permission denied");
        let destination = snap(&[(lib, Entry::dir()), (&item("a.jpg"), Entry::file(4, 1)), ("fine.txt", Entry::file(1, 1))], FsKind::Ext);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits(lib) && ground.is_off_limits(&item("a.jpg")));
        assert!(!ground.is_off_limits("fine.txt"));
        let kept: Vec<(&str, Option<Side>)> = plan.notes.iter().filter(|n| n.kind == NoteKind::Blocked).map(|n| (n.path.as_str(), n.side)).collect();
        assert_eq!(kept, vec![(lib, Some(Side::Source))], "one note for the library: {:?}", plan.notes);
        assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.reason.contains(&format!("“{}”", item("database/b.db")))));
    }

    /// A Final Cut library whose media was imported with "Leave files in place" holds links,
    /// and a name an exFAT drive can't take: no version of them can ever be in the library
    /// there, so the library is copied without them, each under its own note.
    #[test]
    fn a_library_is_synced_without_what_the_drive_can_never_hold() {
        let lib = "Movie.fcpbundle";
        let item = |name: &str| format!("{lib}/{name}");
        let source = snap(
            &[
                (lib, Entry::dir()),
                (&item("CurrentVersion.fcpevent"), Entry::file(8, 2)),
                (&item("Original Media"), Entry::dir()),
                (&item("Original Media/clip.mov"), Entry::link(b"/Volumes/Media/clip.mov", 1)),
                (&item("Notes: day 1.txt"), Entry::file(3, 1)),
            ],
            FsKind::Apfs,
        );
        let destination = snap(&[], FsKind::Exfat);
        let plan = crate::plan::OneWayPlanner::mirror().plan(&source, &destination);
        let mut paths: Vec<&str> = plan.actions.iter().map(Action::path).collect();
        paths.sort();
        assert_eq!(paths, vec![lib, item("CurrentVersion.fcpevent").as_str(), item("Original Media").as_str()], "{:?}", plan.notes);
        let mut notes: Vec<(NoteKind, &str)> = plan.notes.iter().map(|n| (n.kind, n.path.as_str())).collect();
        notes.sort_by_key(|(_, path)| *path);
        assert_eq!(notes, vec![(NoteKind::LeftOut, item("Notes: day 1.txt").as_str()), (NoteKind::LeftOut, item("Original Media/clip.mov").as_str())]);
    }

    /// What can leave an older version of itself in a library, or hide a change there, still
    /// keeps the library whole: a file grown too big for the drive over the version it holds,
    /// what the drive refused in an earlier sync, and a database's journal (its database
    /// never goes without it).
    #[test]
    fn a_library_with_an_item_that_could_be_left_behind_is_left_alone_whole() {
        let lib = "Photos Library.photoslibrary";
        let item = |name: &str| format!("{lib}/{name}");
        let library = |extra: &[(&str, Entry)], kind: FsKind| {
            let mut items = vec![(lib.to_string(), Entry::dir()), (item("a.jpg"), Entry::file(4, 2))];
            items.extend(extra.iter().map(|(name, entry)| (item(name), *entry)));
            let items: Vec<(&str, Entry)> = items.iter().map(|(key, entry)| (key.as_str(), *entry)).collect();
            snap(&items, kind)
        };
        let big = Entry::file(5_000_000_000, 2);
        let cases = [
            ("a file grown too big for the drive", library(&[("big.mov", big)], FsKind::Apfs), library(&[("big.mov", Entry::file(9, 1))], FsKind::Fat)),
            ("a journal too big for the drive", library(&[("Photos.sqlite", Entry::file(8, 2)), ("Photos.sqlite-wal", big)], FsKind::Apfs), library(&[], FsKind::Fat)),
        ];
        for (case, source, destination) in cases {
            let plan = crate::plan::OneWayPlanner::mirror().plan(&source, &destination);
            assert!(plan.actions.is_empty(), "{case}: {:?}", plan.actions);
            assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == lib), "{case}: {:?}", plan.notes);
        }
        let source = library(&[("q3: plans.txt", Entry::file(1, 2))], FsKind::Apfs);
        let mut destination = library(&[], FsKind::Other);
        destination.refuse(&item("q3: plans.txt"), crate::scan::Refusal::Name);
        let plan = crate::plan::OneWayPlanner::mirror().plan(&source, &destination);
        assert!(plan.actions.is_empty(), "a refusal: {:?}", plan.actions);
        assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == lib), "a refusal: {:?}", plan.notes);
    }

    #[test]
    fn unreadable_places_are_off_limits_with_everything_inside() {
        let mut source = snap(&[("a", Entry::dir()), ("a/b.txt", Entry::file(1, 1))], FsKind::Ext);
        source.freeze("a", "permission denied");
        let destination = snap(&[], FsKind::Ext);
        let mut plan = Plan::default();
        let ground = Ground::new(&source, &destination, &[Side::Destination], &mut plan);
        assert!(ground.is_off_limits("a/b.txt"));
        assert_eq!(plan.notes[0].kind, NoteKind::Unreadable);
    }
}
