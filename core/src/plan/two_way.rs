//! Two-way sync: changes flow both ways.
//!
//! When a file is missing on one side, Charon has to know whether it was deleted there or
//! newly created on the other side. So it keeps the *baseline*: how each side looked the
//! last time both agreed (see `plan::baseline`). Comparing each side with its own record
//! tells who changed:
//!
//! | source vs baseline | destination vs baseline | what happens                        |
//! |--------------------|-------------------------|-------------------------------------|
//! | same               | same                    | nothing                             |
//! | changed            | same                    | the source's change crosses over    |
//! | same               | changed                 | the destination's change crosses    |
//! | changed            | changed (differently)   | conflict, settled by the job's rule |
//!
//! A change can be a new item, an edit or a deletion. In a conflict between an edit and a
//! deletion, the edit wins: Charon never throws away work. A file whose details alone
//! changed on one side (a tag, a permission, an app noting that it was opened: its size and
//! time are as recorded) doesn't stand against a real edit on the other side (not a time a
//! drive moved by itself, by whole quarter hours say): that edit crosses, and the version
//! it replaces goes to the trash, like every version a two-way job replaces. Against a
//! deletion it is an edit, and wins: an edit can put a file's old time back. An item one
//! side changed into another kind (a folder that became a file) crosses like any change,
//! unless the folder it replaces holds changes or items Charon leaves alone; a file meeting
//! a folder that both sides changed, and a link taking the place of a file or folder, are a
//! question for the person. When the newest version wins a conflict, the other version
//! still goes to the trash; and when "newest" can't be told reliably (a FAT or exFAT drive,
//! times whole hours apart, a time that went backwards), both versions are kept instead.
//! Places that couldn't be read on either side are left alone completely, and their
//! baseline is kept.
//!
//! A file whose time is now *older* than at the last sync isn't an ordinary change either:
//! it was restored from an older copy, or the drive lost its last writes (pulled out too
//! soon). Carrying it over would replace the newer version on the other side, so it's
//! treated as a conflict, settled by the job's rule (never by "newest").
//!
//! Without a baseline (the first sync, or a memory Charon can't trust) every difference is
//! a copy or a conflict, settled by the job's rule: nothing is removed, and a version that
//! "newest wins" replaces goes to the trash.
//!
//! An app library or a database whose files changed on both sides is never mixed: the
//! whole unit becomes one conflict (see `plan::units`).

use crate::model::ConflictRule;
use crate::plan::action::LEFT_ALONE;
use crate::plan::ground::Ground;
use crate::plan::{Action, Baseline, NoteKind, Plan, Side, Unit};
use crate::scan::{Entry, Snapshot};
use std::collections::BTreeSet;

pub struct TwoWayPlanner {
    rule: ConflictRule,
    /// Pairs that changed on both sides and look alike, but were read and differ.
    different: BTreeSet<String>,
}

impl TwoWayPlanner {
    pub fn new(rule: ConflictRule) -> Self {
        Self { rule, different: BTreeSet::new() }
    }

    /// Pairs known to differ in contents though their sizes and dates match.
    pub fn knowing_different(mut self, different: BTreeSet<String>) -> Self {
        self.different = different;
        self
    }

    /// What it takes to bring both folders together, given how each side looked when they
    /// last agreed.
    pub fn plan(&self, source: &Snapshot, destination: &Snapshot, baseline: &Baseline) -> Plan {
        let mut plan = Plan::default();
        let mut ground = Ground::new(source, destination, &[Side::Source, Side::Destination], &mut plan);
        let all_paths: BTreeSet<&String> = source.paths().chain(destination.paths()).chain(baseline.paths()).collect();
        let split = self.split_units(&all_paths, source, destination, baseline, &ground);

        for path in all_paths {
            if ground.is_off_limits(path) {
                continue;
            }
            if let Some(unit) = Unit::of(path, source, destination).filter(|unit| split.contains(unit)) {
                Self::ask_unit(&mut plan, &mut ground, &unit, path, source, destination);
                continue;
            }
            let s = source.get(path);
            let d = destination.get(path);
            let (source_changed, destination_changed) = self.changes_that_cross(path, source, destination, baseline);

            match (source_changed, destination_changed) {
                // Both sides are as the last sync left them, even if they look different (a
                // drive that doesn't keep times, or contents found identical), or both
                // changed (or there's no record yet) and now look alike: nothing to do.
                (false, false) => {}
                // One side changed since the last sync: that change crosses, even when the two
                // sides happen to look alike within the 2 seconds some drives round times to.
                // Its time moved, so its contents may have too.
                // Unless that side's file went back in time.
                (true, false) if Self::went_back(s, baseline.side(path, Side::Source)) => {
                    self.settle_conflict(&mut plan, &mut ground, baseline, path, s, d)
                }
                (false, true) if Self::went_back(d, baseline.side(path, Side::Destination)) => {
                    self.settle_conflict(&mut plan, &mut ground, baseline, path, s, d)
                }
                (true, false) => self.carry(&mut plan, &mut ground, baseline, path, s, d, Side::Destination, false),
                (false, true) => self.carry(&mut plan, &mut ground, baseline, path, s, d, Side::Source, false),
                (true, true) => self.settle_conflict(&mut plan, &mut ground, baseline, path, s, d),
            }
        }
        ground.hold_removals(&mut plan);
        ground.finish(&mut plan);
        plan
    }

    /// Whether each side changed `path` since the last sync. Both sides changing it the same
    /// way (or both having it alike without a record) counts as no change.
    fn changes(&self, path: &str, source: &Snapshot, destination: &Snapshot, baseline: &Baseline) -> (bool, bool) {
        let (s, d) = (source.get(path), destination.get(path));
        let source_changed = !Entry::still_on(s, baseline.side(path, Side::Source), source.shifts_times());
        let destination_changed = !Entry::still_on(d, baseline.side(path, Side::Destination), destination.shifts_times());
        if source_changed && destination_changed && Entry::same(s, d) && !self.different.contains(path) {
            return (false, false);
        }
        (source_changed, destination_changed)
    }

    /// `changes`, for deciding what crosses: when only the details of a file changed on one
    /// side, a real edit of it on the other side is a change of that side alone. A time that
    /// only moved the way drives move times isn't a real edit: then either side may hold the
    /// edit, and the pair stays a conflict (unless its contents were read and found the same,
    /// see `Engine::compare_contents`).
    fn changes_that_cross(&self, path: &str, source: &Snapshot, destination: &Snapshot, baseline: &Baseline) -> (bool, bool) {
        let changes = self.changes(path, source, destination, baseline);
        if changes != (true, true) || self.different.contains(path) {
            return changes;
        }
        let (s, d) = (source.get(path), destination.get(path));
        let only_details = |now: Option<&Entry>, side: Side| {
            now.zip(baseline.side(path, side)).is_some_and(|(now, before)| now.is_file() && now.changed_at(0).unchanged(before))
        };
        // The size changed, or the time moved more than by a rounding or whole quarter hours
        // (see `Entry::drifted_from`, which a change of details alone passes too).
        let edited = |now: Option<&Entry>, side: Side| {
            now.is_some_and(Entry::is_file) && !now.zip(baseline.side(path, side)).is_some_and(|(now, before)| now.drifted_from(before))
        };
        if only_details(s, Side::Source) && edited(d, Side::Destination) {
            (false, true)
        } else if only_details(d, Side::Destination) && edited(s, Side::Source) {
            (true, false)
        } else {
            changes
        }
    }

    /// The app libraries and databases with files changed on both sides: syncing them file
    /// by file would mix two versions. Any change counts here, details too.
    fn split_units(&self, paths: &BTreeSet<&String>, source: &Snapshot, destination: &Snapshot, baseline: &Baseline, ground: &Ground) -> BTreeSet<Unit> {
        let mut changed: std::collections::BTreeMap<Unit, (bool, bool)> = std::collections::BTreeMap::new();
        for path in paths {
            if ground.is_off_limits(path) {
                continue;
            }
            let Some(unit) = Unit::of(path, source, destination) else { continue };
            let (on_source, on_destination) = self.changes(path, source, destination, baseline);
            let sides = changed.entry(unit).or_default();
            sides.0 |= on_source;
            sides.1 |= on_destination;
        }
        changed.into_iter().filter(|(_, (on_source, on_destination))| *on_source && *on_destination).map(|(unit, _)| unit).collect()
    }

    /// Leaves a whole app library or database for the person to decide: one conflict for
    /// the unit, and none of its files moves.
    fn ask_unit(plan: &mut Plan, ground: &mut Ground, unit: &Unit, path: &str, source: &Snapshot, destination: &Snapshot) {
        match unit {
            Unit::Package(folder) => Self::ask(plan, ground, folder, source.get(folder), destination.get(folder)),
            Unit::Database(database) => {
                // The conflict is named after the database file, or after the first of its
                // files still there when the database itself is gone from both sides.
                let key = if source.contains(database) || destination.contains(database) { database.as_str() } else { path };
                Self::ask(plan, ground, key, source.get(key), destination.get(key));
                for file in Unit::database_files(database) {
                    ground.block(&file);
                }
            }
        }
    }

    /// Whether a file's time is now older than the one recorded at the last sync.
    fn went_back(now: Option<&Entry>, before: Option<&Entry>) -> bool {
        matches!((now, before), (Some(now), Some(before))
            if now.is_file() && before.is_file() && now.modified + crate::scan::MTIME_TOLERANCE_MS < before.modified)
    }

    /// Makes side `to` match the other side's version of `path`. `settles` marks a copy that
    /// decides a conflict (the version it replaces always goes to the trash).
    #[allow(clippy::too_many_arguments)]
    fn carry(&self, plan: &mut Plan, ground: &mut Ground, baseline: &Baseline, path: &str, s: Option<&Entry>, d: Option<&Entry>, to: Side, settles: bool) {
        let (wanted, existing) = match to {
            Side::Destination => (s, d),
            Side::Source => (d, s),
        };
        let here = ground.side(to);
        match (wanted, existing) {
            (None, None) => {}
            // Deleted on the other side.
            (None, Some(existing)) => {
                if existing.is_dir() && here.is_occupied(path) {
                    plan.note(
                        NoteKind::KeptFolder,
                        Some(to),
                        here.spelling(path),
                        format!("kept: it was removed on the other side, but it holds {LEFT_ALONE}"),
                    );
                } else {
                    plan.push(Action::delete(to, path, *existing, None));
                }
            }
            // The other side changed the item into another kind (a folder became a file…).
            (Some(wanted), Some(existing)) if wanted.kind != existing.kind => {
                let folder_in_the_way = existing.is_dir() && (here.is_occupied(path) || ground.holds_changes(to, path, baseline));
                // A link in a file's or folder's place would take what it holds away on this
                // side, while it stays reachable only through the link on the other (usually
                // it was moved to another drive, with a link left behind).
                if folder_in_the_way || wanted.is_link() {
                    Self::ask(plan, ground, path, s, d);
                    return;
                }
                plan.push(Action::delete(to, path, *existing, Some(*wanted)));
                Self::bring(plan, to, path, wanted, None, settles);
            }
            (Some(wanted), existing) => Self::bring(plan, to, path, wanted, existing.copied(), settles),
        }
    }

    /// Creates the folder, or copies the file or link, on side `to`.
    fn bring(plan: &mut Plan, to: Side, path: &str, wanted: &Entry, existing: Option<Entry>, settles: bool) {
        if wanted.is_dir() {
            plan.push(Action::CreateDir { side: to, path: path.to_string() });
        } else {
            plan.push(Action::copy(to, path, *wanted, existing).settling(settles));
        }
    }

    /// Both sides changed the item since the last sync (or there is no last sync).
    fn settle_conflict(&self, plan: &mut Plan, ground: &mut Ground, baseline: &Baseline, path: &str, s: Option<&Entry>, d: Option<&Entry>) {
        match (s, d) {
            // Edited on one side, deleted on the other: the edit wins (new on one side, when
            // there was no last sync).
            (Some(_), None) => self.carry(plan, ground, baseline, path, s, d, Side::Destination, false),
            (None, Some(_)) => self.carry(plan, ground, baseline, path, s, d, Side::Source, false),
            (Some(src), Some(dst)) if src.kind == dst.kind && !src.is_dir() => match self.rule {
                ConflictRule::NewestWins if Self::newest_is_clear(ground, baseline, path, src, dst) => {
                    let to = if dst.modified > src.modified { Side::Source } else { Side::Destination };
                    self.carry(plan, ground, baseline, path, s, d, to, true);
                }
                ConflictRule::KeepBoth | ConflictRule::NewestWins if src.is_file() => plan.push(Action::KeepBoth {
                    path: path.to_string(),
                    source_bytes: src.size,
                    destination_bytes: dst.size,
                    source: *src,
                    destination: *dst,
                }),
                // Two different links have no "both": ask.
                _ => Self::ask(plan, ground, path, s, d),
            },
            // A file on one side, a folder on the other: always a question.
            _ => Self::ask(plan, ground, path, s, d),
        }
    }

    /// Whether the two times say which version is newer: not on network shares or drives
    /// Charon doesn't know, not on drives that keep local
    /// times (a time-zone change moves them by hours), not when they're whole hours apart or
    /// too close to call, and not when either side's time went backwards (a restored file,
    /// a wrong clock).
    fn newest_is_clear(ground: &Ground, baseline: &Baseline, path: &str, src: &Entry, dst: &Entry) -> bool {
        let shifting = ground.side(Side::Source).shifts_times() || ground.side(Side::Destination).shifts_times();
        let comparable = |side: Side| ground.side(side).volume().is_none_or(|volume| volume.comparable_times());
        if !comparable(Side::Source) || !comparable(Side::Destination) {
            return false;
        }
        let backwards = |side: Side, now: &Entry| baseline.side(path, side).is_some_and(|before| now.modified < before.modified);
        !shifting
            && (src.modified - dst.modified).abs() > crate::scan::MTIME_TOLERANCE_MS
            && !Entry::whole_hours_apart(src.modified, dst.modified)
            && !backwards(Side::Source, src)
            && !backwards(Side::Destination, dst)
    }

    /// Leaves the item (and anything inside it) for the person to decide.
    fn ask(plan: &mut Plan, ground: &mut Ground, path: &str, s: Option<&Entry>, d: Option<&Entry>) {
        plan.push(Action::Conflict { path: path.to_string(), source: s.copied(), destination: d.copied() });
        ground.block(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Pair;
    use crate::scan::EntryKind;

    fn file(size: u64, modified: i64) -> Entry {
        Entry::file(size, modified)
    }

    fn snap(items: &[(&str, Entry)]) -> Snapshot {
        items.iter().map(|(p, e)| (p.to_string(), *e)).collect()
    }

    /// A baseline where both sides had exactly these items.
    fn base(items: &[(&str, Entry)]) -> Baseline {
        items.iter().map(|(p, e)| (p.to_string(), Pair::new(*e, *e))).collect()
    }

    #[test]
    fn a_version_older_than_its_own_record_is_a_conflict_not_a_change() {
        let synced = file(5, 1_000_000);
        let older = file(4, 400_000);
        let baseline = base(&[("a.txt", synced)]);
        // The destination went back (restored, or its last writes were lost).
        for rule in [ConflictRule::Ask, ConflictRule::KeepBoth, ConflictRule::NewestWins] {
            let plan = TwoWayPlanner::new(rule).plan(&snap(&[("a.txt", synced)]), &snap(&[("a.txt", older)]), &baseline);
            assert!(
                !plan.actions.iter().any(|a| matches!(a, Action::Copy { to: Side::Source, .. })),
                "{rule:?}: the older version must never replace the newer one: {:?}",
                plan.actions
            );
            assert!(plan.actions.iter().any(|a| matches!(a, Action::Conflict { .. } | Action::KeepBoth { .. })), "{rule:?}");
        }
        // A newer edit still simply crosses.
        let newer = file(6, 2_000_000);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&snap(&[("a.txt", newer)]), &snap(&[("a.txt", synced)]), &baseline);
        assert_eq!(plan.actions, vec![Action::copy(Side::Destination, "a.txt", newer, Some(synced))]);
    }

    #[test]
    fn first_run_merges_both_sides() {
        let src = snap(&[("a.txt", file(1, 10_000))]);
        let dst = snap(&[("b.txt", file(2, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &Baseline::new());
        let s = plan.summary();
        assert_eq!((s.to_destination, s.to_source, s.deletes), (1, 1, 0));
    }

    #[test]
    fn without_a_baseline_differences_are_conflicts_never_deletions() {
        let src = snap(&[("a.txt", file(1, 10_000))]);
        let dst = snap(&[("a.txt", file(2, 90_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &Baseline::new());
        assert_eq!(plan.conflicts().len(), 1);
        assert_eq!(plan.summary().deletes, 0);
    }

    #[test]
    fn deletion_crosses_over() {
        let b = base(&[("a.txt", file(1, 10_000))]);
        let src = snap(&[]);
        let dst = snap(&[("a.txt", file(1, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert_eq!(plan.actions, vec![Action::delete(Side::Destination, "a.txt", file(1, 10_000), None)]);
    }

    #[test]
    fn edit_beats_deletion() {
        let b = base(&[("a.txt", file(1, 10_000))]);
        let src = snap(&[]);
        let dst = snap(&[("a.txt", file(5, 90_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert!(matches!(&plan.actions[0], Action::Copy { to: Side::Source, .. }));
    }

    /// A tag, a permission or an app noting that a file was opened moves only its change
    /// time. A real edit on the other side then crosses, by every rule, instead of meeting
    /// it as a conflict. A removal on the other side doesn't: the details may hide an edit
    /// that put the old time back, and an edit beats a removal.
    #[test]
    fn a_change_of_details_yields_to_a_real_edit_on_the_other_side() {
        let synced = file(1000, 10_000).changed_at(7);
        let touched = synced.changed_at(8);
        let edited = file(1200, 90_000).changed_at(9);
        let b = base(&[("report.docx", synced)]);
        for rule in [ConflictRule::Ask, ConflictRule::KeepBoth, ConflictRule::NewestWins] {
            let plan = TwoWayPlanner::new(rule).plan(&snap(&[("report.docx", touched)]), &snap(&[("report.docx", edited)]), &b);
            assert_eq!(plan.actions, vec![Action::copy(Side::Source, "report.docx", edited, Some(touched))], "{rule:?}");
            let plan = TwoWayPlanner::new(rule).plan(&snap(&[("report.docx", edited)]), &snap(&[("report.docx", touched)]), &b);
            assert_eq!(plan.actions, vec![Action::copy(Side::Destination, "report.docx", edited, Some(touched))], "{rule:?}");
        }
        let ask = TwoWayPlanner::new(ConflictRule::Ask);
        let plan = ask.plan(&snap(&[("report.docx", touched)]), &snap(&[]), &b);
        assert_eq!(plan.actions, vec![Action::copy(Side::Destination, "report.docx", touched, None)], "the removal doesn't cross");
        // Edits on both sides, and an older version coming back, are still conflicts.
        let also_edited = file(1100, 50_000).changed_at(8);
        assert_eq!(ask.plan(&snap(&[("report.docx", also_edited)]), &snap(&[("report.docx", edited)]), &b).conflicts().len(), 1);
        let restored = file(900, 4_000).changed_at(9);
        assert_eq!(ask.plan(&snap(&[("report.docx", touched)]), &snap(&[("report.docx", restored)]), &b).conflicts().len(), 1);
        // A time that only moved the way drives move times (by whole quarter hours, on an
        // exFAT drive used under another time-zone offset) isn't a real edit. Such a pair is
        // read, and when the contents differ, either side may hold the edit (one that kept the
        // file's time, as an encrypted container does): it stays a conflict, by every rule.
        let mut shifted = snap(&[("report.docx", file(1000, 10_000 + 30 * 60_000))]);
        shifted.facts_mut().volume = Some(crate::scan::Volume {
            kind: crate::scan::FsKind::Exfat,
            fs: "exfat".into(),
            mount: "/Volumes/STICK".into(),
            name: "STICK".into(),
            network: false,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        });
        for rule in [ConflictRule::Ask, ConflictRule::KeepBoth, ConflictRule::NewestWins] {
            let plan = TwoWayPlanner::new(rule).plan(&snap(&[("report.docx", touched)]), &shifted, &b);
            assert!(matches!(plan.actions.as_slice(), [Action::Conflict { .. } | Action::KeepBoth { .. }]), "{rule:?}: {:?}", plan.actions);
        }
        // Inside an app library, a change of details still counts: the library is one conflict.
        let library = [("Talk.key", Entry::dir()), ("Talk.key/Index.zip", synced)];
        let plan = ask.plan(
            &snap(&[library[0], ("Talk.key/Index.zip", touched)]),
            &snap(&[library[0], ("Talk.key/Index.zip", edited)]),
            &base(&library),
        );
        assert_eq!(plan.conflicts().len(), 1, "{:?}", plan.actions);
    }

    #[test]
    fn each_side_is_compared_with_its_own_record() {
        // The destination's drive rounds times by a second: that's not a change.
        let s = file(4, 10_000);
        let d = file(4, 11_000);
        let b: Baseline = [("a.txt".to_string(), Pair::new(s, d))].into_iter().collect();
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&snap(&[("a.txt", s)]), &snap(&[("a.txt", d)]), &b);
        assert!(plan.actions.is_empty());
        // An edit within the same 2 seconds on one side is still an edit.
        let edited = file(4, 10_900);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&snap(&[("a.txt", edited)]), &snap(&[("a.txt", d)]), &b);
        assert!(matches!(&plan.actions[0], Action::Copy { to: Side::Destination, .. }), "{:?}", plan.actions);
    }

    #[test]
    fn a_drive_that_doesnt_keep_times_never_loops() {
        // After Charon's copy, the destination's drive stamped its own time on the file.
        let s = file(4, 10_000);
        let d = file(4, 99_999_000);
        let b: Baseline = [("a.txt".to_string(), Pair::new(s, d))].into_iter().collect();
        let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&snap(&[("a.txt", s)]), &snap(&[("a.txt", d)]), &b);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
    }

    #[test]
    fn conflict_rules() {
        let b = base(&[("a.txt", file(1, 10_000))]);
        let src = snap(&[("a.txt", file(2, 50_000))]);
        let dst = snap(&[("a.txt", file(3, 90_000))]);

        let newest = TwoWayPlanner::new(ConflictRule::NewestWins).plan(&src, &dst, &b);
        assert!(matches!(&newest.actions[0], Action::Copy { to: Side::Source, settles_conflict: true, .. }));

        let both = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&src, &dst, &b);
        assert!(matches!(&both.actions[0], Action::KeepBoth { .. }));

        let ask = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert_eq!(ask.conflicts().len(), 1);
        assert!(ask.is_settled());
    }

    #[test]
    fn newest_wins_keeps_both_when_newest_cant_be_told() {
        let b = base(&[("a.txt", file(1, 10_000_000))]);
        let rule = TwoWayPlanner::new(ConflictRule::NewestWins);
        // Exactly an hour apart: a time-zone or daylight-saving shift, not a real "newer".
        let src = snap(&[("a.txt", file(2, 20_000_000))]);
        let dst = snap(&[("a.txt", file(3, 20_000_000 + 3_600_000))]);
        assert!(matches!(&rule.plan(&src, &dst, &b).actions[0], Action::KeepBoth { .. }));
        // The source's time went backwards (restored from an old copy).
        let src = snap(&[("a.txt", file(2, 5_000_000))]);
        let dst = snap(&[("a.txt", file(3, 20_000_000))]);
        assert!(matches!(&rule.plan(&src, &dst, &b).actions[0], Action::KeepBoth { .. }));
        // A FAT drive on one side.
        let mut src = snap(&[("a.txt", file(2, 30_000_000))]);
        src.facts_mut().volume = Some(crate::scan::Volume {
            kind: crate::scan::FsKind::Fat,
            fs: "msdos".into(),
            mount: "/Volumes/STICK".into(),
            name: "STICK".into(),
            network: false,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        });
        let dst = snap(&[("a.txt", file(3, 20_000_000))]);
        assert!(matches!(&rule.plan(&src, &dst, &b).actions[0], Action::KeepBoth { .. }));
    }

    #[test]
    fn an_unreadable_folder_is_never_taken_for_a_deletion() {
        let b = base(&[("work", Entry::dir()), ("work/plan.txt", file(1, 10_000))]);
        let mut src = snap(&[("work", Entry::dir())]);
        src.freeze("work", "permission denied");
        let dst = snap(&[("work", Entry::dir()), ("work/plan.txt", file(1, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
    }

    #[test]
    fn a_removed_folder_with_new_work_inside_is_kept() {
        let b = base(&[("f", Entry::dir()), ("f/old.txt", file(1, 10_000))]);
        let src = snap(&[]);
        let dst = snap(&[("f", Entry::dir()), ("f/old.txt", file(1, 10_000)), ("f/new.txt", file(2, 20_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        // old.txt goes, new.txt crosses to the source, and the folder stays on the destination.
        assert!(plan.actions.contains(&Action::delete(Side::Destination, "f/old.txt", file(1, 10_000), None)));
        assert!(plan.actions.iter().any(|a| matches!(a, Action::Copy { to: Side::Source, path, .. } if path == "f/new.txt")));
        assert!(!plan.actions.iter().any(|a| matches!(a, Action::Delete { item: EntryKind::Dir, .. })));
    }

    #[test]
    fn a_folder_turned_file_over_new_work_is_a_question() {
        let b = base(&[("x", Entry::dir()), ("x/a.txt", file(1, 10_000))]);
        let src = snap(&[("x", file(9, 90_000))]);
        let dst = snap(&[("x", Entry::dir()), ("x/a.txt", file(1, 10_000)), ("x/b.txt", file(2, 20_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&src, &dst, &b);
        assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
        assert!(matches!(&plan.actions[0], Action::Conflict { path, .. } if path == "x"));
    }

    #[test]
    fn a_link_taking_a_folders_place_is_a_question() {
        let b = base(&[("footage", Entry::dir()), ("footage/a.mov", file(1, 10_000))]);
        let src = snap(&[("footage", Entry::link(b"/Volumes/SSD/footage", 50_000))]);
        let dst = snap(&[("footage", Entry::dir()), ("footage/a.mov", file(1, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&src, &dst, &b);
        assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
        assert!(matches!(&plan.actions[0], Action::Conflict { path, .. } if path == "footage"));
    }

    #[test]
    fn a_link_taking_a_files_place_is_a_question() {
        let b = base(&[("clip.mov", file(4_000, 10_000))]);
        let link = Entry::link(b"/Volumes/SSD/clip.mov", 50_000);
        for (src, dst) in [(snap(&[("clip.mov", link)]), snap(&[("clip.mov", file(4_000, 10_000))])), (snap(&[("clip.mov", file(4_000, 10_000))]), snap(&[("clip.mov", link)]))] {
            let plan = TwoWayPlanner::new(ConflictRule::NewestWins).plan(&src, &dst, &b);
            assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
            assert!(matches!(&plan.actions[0], Action::Conflict { path, .. } if path == "clip.mov"));
        }
        // A file taking a link's place loses nothing: it crosses.
        let b = base(&[("x", link)]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&snap(&[("x", file(1, 60_000))]), &snap(&[("x", link)]), &b);
        assert!(plan.actions.iter().any(|a| matches!(a, Action::Copy { to: Side::Destination, item: EntryKind::File, .. })), "{:?}", plan.actions);
    }

    #[test]
    fn a_case_only_rename_keeps_the_folder_syncing() {
        // "invoices" became "Invoices" on the destination; one drive ignores case, so both
        // sides' keys are folded and the folder is one item.
        let fold = |items: &[(&str, Entry)]| {
            let mut s = snap(items);
            s.fold();
            s
        };
        let b = base(&[("invoices", Entry::dir()), ("invoices/a.pdf", file(1, 10_000))]);
        let src = fold(&[("invoices", Entry::dir()), ("invoices/a.pdf", file(2, 20_000)), ("invoices/new.pdf", file(3, 30_000))]);
        let dst = fold(&[("Invoices", Entry::dir()), ("Invoices/a.pdf", file(1, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        let mut copied: Vec<&str> = plan.actions.iter().filter(|a| matches!(a, Action::Copy { to: Side::Destination, .. })).map(|a| a.path()).collect();
        copied.sort();
        assert_eq!(copied, vec!["invoices/a.pdf", "invoices/new.pdf"], "{:?}", plan.actions);
        assert_eq!(plan.actions.len(), 2, "nothing is removed: {:?}", plan.actions);
        assert!(plan.notes.is_empty(), "nothing is left alone: {:?}", plan.notes);
        assert_eq!(dst.spelling("invoices/a.pdf"), "Invoices/a.pdf", "the destination keeps its own spelling");
    }

    /// A pipe on the destination where the source has a file: no copy is planned into its
    /// place (it would fail on every sync and hold back every removal there), and the
    /// removal the source asks for still goes ahead.
    #[test]
    fn a_left_out_item_keeps_its_name_out_of_the_plan_and_nothing_else() {
        let kept = file(1, 10_000);
        let gone = file(2, 10_000);
        let b = base(&[("keep.txt", kept), ("gone.txt", gone)]);
        let src = snap(&[("keep.txt", kept), ("channel", file(4, 20_000))]);
        let mut dst = snap(&[("keep.txt", kept), ("gone.txt", gone)]);
        dst.leave_out("channel".into(), Some("channel"), "isn't a file or a folder (a socket, pipe or device)");
        let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&src, &dst, &b);
        assert_eq!(plan.actions, vec![Action::delete(Side::Destination, "gone.txt", gone, None)]);
        assert_eq!(plan.notes.len(), 1, "one note for the name: {:?}", plan.notes);
        assert_eq!((plan.notes[0].kind, plan.notes[0].side, plan.notes[0].path.as_str()), (NoteKind::LeftOut, Some(Side::Destination), "channel"));
    }

    /// A Windows junction where the other side has a real folder: the folder and everything
    /// in it stay as they are, on both sides. Without the junction's contents in the scan,
    /// they would otherwise look removed on the Windows side.
    #[test]
    fn a_folder_whose_name_is_left_out_on_the_other_side_is_neither_filled_nor_emptied() {
        let song = file(9, 10_000);
        let b = base(&[("music", Entry::dir()), ("music/song.mp3", song)]);
        let src = snap(&[("music", Entry::dir()), ("music/song.mp3", song), ("music/new.mp3", file(3, 20_000))]);
        let mut dst = snap(&[]);
        dst.leave_out("music".into(), Some("music"), "is a link or junction; Charon leaves those alone on Windows");
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        let after = Baseline::after_run(&b, &src, &dst, &plan, &[], &[]);
        assert_eq!(after.get("music/song.mp3"), b.get("music/song.mp3"), "the record stays for when the folder is back");
    }

    /// The source tells upper and lower case apart and holds the person's own folder
    /// ".CHARON-TRASH"; the destination ignores case and holds Charon's trash. Nothing of the
    /// person's folder goes into the trash, and nothing in it is taken for removed. (A scan
    /// with folded keys leaves such a folder out itself; the plan leaves it alone anyway.)
    #[test]
    fn a_name_differing_only_in_case_from_charons_trash_is_left_alone() {
        let notes = file(1, 10_000);
        let mut src = snap(&[(".CHARON-TRASH", Entry::dir()), (".CHARON-TRASH/notes.txt", notes), ("a.txt", notes)]);
        let mut dst = snap(&[("a.txt", notes)]);
        dst.reserve(".charon-trash", ".charon-trash");
        src.fold();
        dst.fold();
        let b = base(&[("a.txt", notes)]);
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert_eq!(plan.notes.len(), 1, "{:?}", plan.notes);
        let note = &plan.notes[0];
        assert_eq!((note.kind, note.side, note.path.as_str()), (NoteKind::LeftOut, Some(Side::Source), ".CHARON-TRASH"));
        assert!(note.reason.contains("“.charon-trash” on the destination"), "{}", note.reason);
        // Without a name like it on the other side, a reserved name needs no note.
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&dst, &dst, &b);
        assert!(plan.notes.is_empty() && plan.actions.is_empty());
    }

    #[test]
    fn excluded_items_keep_a_removed_folder() {
        let b = base(&[("f", Entry::dir()), ("f/a.txt", file(1, 10_000))]);
        let src = snap(&[]);
        let mut dst = snap(&[("f", Entry::dir()), ("f/a.txt", file(1, 10_000))]);
        dst.occupy("f"); // f/cache.tmp is excluded
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&src, &dst, &b);
        assert_eq!(plan.actions, vec![Action::delete(Side::Destination, "f/a.txt", file(1, 10_000), None)]);
        assert_eq!(plan.notes[0].kind, NoteKind::KeptFolder);
        assert_eq!(plan.notes[0].reason, "kept: it was removed on the other side, but it holds items Charon leaves alone (excluded, left out or unreadable items, or what Charon or a cloud service keeps for its own use)");
    }

    #[test]
    fn a_database_changed_on_both_sides_is_one_conflict_and_nothing_moves() {
        let (db, wal) = (file(4096, 10_000), file(100, 10_000));
        let baseline = base(&[("app", Entry::dir()), ("app/data.db", db), ("app/data.db-wal", wal)]);
        // The source checkpointed into the database; the destination added to the journal.
        let source = snap(&[("app", Entry::dir()), ("app/data.db", file(8192, 20_000)), ("app/data.db-wal", file(0, 20_000))]);
        let destination = snap(&[("app", Entry::dir()), ("app/data.db", db), ("app/data.db-wal", file(300, 30_000))]);
        for rule in [ConflictRule::Ask, ConflictRule::KeepBoth, ConflictRule::NewestWins] {
            let plan = TwoWayPlanner::new(rule).plan(&source, &destination, &baseline);
            assert_eq!(plan.actions, vec![Action::Conflict { path: "app/data.db".into(), source: Some(file(8192, 20_000)), destination: Some(db) }], "{rule:?}");
        }
        // Changed on one side only: its files cross as usual.
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&source, &snap(&[("app", Entry::dir()), ("app/data.db", db), ("app/data.db-wal", wal)]), &baseline);
        assert_eq!(plan.summary().to_destination, 2);
    }

    #[test]
    fn a_library_changed_on_both_sides_is_one_conflict_at_its_folder() {
        let lib = "Photos Library.photoslibrary";
        let item = |name: &str| format!("{lib}/{name}");
        let baseline = base(&[
            (lib, Entry::dir()),
            (&item("a.jpg"), file(1, 10_000)),
            (&item("b.jpg"), file(2, 10_000)),
            ("notes.txt", file(3, 10_000)),
        ]);
        let source = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(5, 20_000)), (&item("b.jpg"), file(2, 10_000)), ("notes.txt", file(4, 20_000))]);
        let destination = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(1, 10_000)), (&item("b.jpg"), file(6, 20_000)), ("notes.txt", file(3, 10_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::NewestWins).plan(&source, &destination, &baseline);
        let conflicts: Vec<&str> = plan.conflicts().iter().map(|(path, ..)| *path).collect();
        assert_eq!(conflicts, vec![lib]);
        // Nothing inside the library moves; the rest of the job syncs.
        assert!(plan.work().all(|action| action.path() == "notes.txt"), "{:?}", plan.actions);
        assert_eq!(plan.summary().to_destination, 1);
    }

    /// A file of a library that Charon leaves alone (one it couldn't read, or one only in
    /// the cloud) keeps the whole library: syncing its other files would leave a library made
    /// of two versions on the side they go to.
    #[test]
    fn a_library_with_a_file_left_alone_is_left_alone_whole() {
        let lib = "Lib.photoslibrary";
        let item = |name: &str| format!("{lib}/{name}");
        let baseline = base(&[(lib, Entry::dir()), (&item("a.jpg"), file(1, 10_000)), (&item("b.db"), file(2, 10_000)), ("notes.txt", file(3, 10_000))]);
        let source = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(5, 20_000)), (&item("b.db"), file(2, 10_000)), ("notes.txt", file(4, 20_000))]);
        let mut destination = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(1, 10_000)), (&item("b.db"), file(6, 20_000)), ("notes.txt", file(3, 10_000))]);
        // The destination changed "b.db", which the last sync couldn't read there.
        destination.freeze_unread(&item("b.db"), "permission denied");
        let plan = TwoWayPlanner::new(ConflictRule::NewestWins).plan(&source, &destination, &baseline);
        assert!(plan.actions.iter().all(|action| action.path() == "notes.txt"), "nothing inside the library moves: {:?}", plan.actions);
        assert!(
            plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == lib && n.reason.contains(&format!("“{}”", item("b.db")))),
            "{:?}",
            plan.notes
        );
        // Changed on one side only, with a file only in the cloud on the other side.
        let source = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(5, 20_000)), (&item("b.db"), file(7, 20_000))]);
        let mut destination = snap(&[(lib, Entry::dir()), (&item("a.jpg"), file(1, 10_000)), (&item("b.db"), file(2, 10_000))]);
        destination.freeze(&item("b.db"), "it's only in the cloud");
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&source, &destination, &baseline);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert!(plan.notes.iter().any(|n| n.kind == NoteKind::Blocked && n.path == lib), "{:?}", plan.notes);
    }

    /// A link in a library that the other side's drive can't hold is left out on its own: no
    /// version of it can ever be in the library there, and the rest of the library still
    /// syncs.
    #[test]
    fn a_library_syncs_without_a_link_the_other_drive_can_never_hold() {
        let lib = "Movie.fcpbundle";
        let event = format!("{lib}/CurrentVersion.fcpevent");
        let clip = format!("{lib}/clip.mov");
        let baseline = base(&[(lib, Entry::dir()), (&event, file(1, 10_000))]);
        let source = snap(&[(lib, Entry::dir()), (&event, file(1, 10_000)), (&clip, Entry::link(b"/Volumes/Media/clip.mov", 1))]);
        let mut destination = snap(&[(lib, Entry::dir()), (&event, file(2, 20_000))]);
        destination.refuse_links();
        let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&source, &destination, &baseline);
        assert_eq!(plan.actions, vec![Action::copy(Side::Source, &event, file(2, 20_000), Some(file(1, 10_000)))], "{:?}", plan.notes);
        assert_eq!(plan.notes.len(), 1);
        assert_eq!((plan.notes[0].kind, plan.notes[0].path.as_str()), (NoteKind::LeftOut, clip.as_str()));
    }

    /// A Numbers document saved as one file, and a private key: only named like an app's
    /// package. They follow the job's rule like any file.
    #[test]
    fn a_single_file_named_like_a_package_follows_the_rules_for_files() {
        for name in ["Budget.numbers", "server.key"] {
            let baseline = base(&[(name, file(1, 10_000))]);
            let (source, destination) = (snap(&[(name, file(2, 50_000))]), snap(&[(name, file(3, 90_000))]));
            let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&source, &destination, &baseline);
            assert!(matches!(plan.actions.as_slice(), [Action::KeepBoth { .. }]), "{name}: {:?}", plan.actions);
            let plan = TwoWayPlanner::new(ConflictRule::NewestWins).plan(&source, &destination, &baseline);
            assert!(matches!(plan.actions.as_slice(), [Action::Copy { to: Side::Source, settles_conflict: true, .. }]), "{name}: {:?}", plan.actions);
            // Edited on one side, removed on the other: the edit wins.
            let plan = TwoWayPlanner::new(ConflictRule::Ask).plan(&snap(&[]), &destination, &baseline);
            assert!(matches!(plan.actions.as_slice(), [Action::Copy { to: Side::Source, .. }]), "{name}: {:?}", plan.actions);
        }
    }

    /// A folder "notes" next to a file "notes-journal" is no database: the file changed on
    /// both sides follows the job's rule, and the folder syncs as usual.
    #[test]
    fn a_folder_next_to_a_file_named_like_its_journal_syncs_as_usual() {
        let baseline = base(&[("notes", Entry::dir()), ("notes/a.txt", file(1, 10_000)), ("notes-journal", file(1, 10_000))]);
        let source = snap(&[("notes", Entry::dir()), ("notes/a.txt", file(1, 10_000)), ("notes/new.txt", file(4, 20_000)), ("notes-journal", file(2, 50_000))]);
        let destination = snap(&[("notes", Entry::dir()), ("notes/a.txt", file(1, 10_000)), ("notes-journal", file(3, 90_000))]);
        let plan = TwoWayPlanner::new(ConflictRule::KeepBoth).plan(&source, &destination, &baseline);
        let mut paths: Vec<&str> = plan.actions.iter().map(|action| action.path()).collect();
        paths.sort();
        assert_eq!(paths, vec!["notes-journal", "notes/new.txt"], "{:?}", plan.actions);
        assert!(plan.actions.iter().any(|action| matches!(action, Action::KeepBoth { path, .. } if path == "notes-journal")));
        assert!(plan.conflicts().is_empty());
    }
}
