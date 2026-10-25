//! Mirror and Backup: the destination follows the source.

use crate::plan::action::LEFT_ALONE;
use crate::plan::ground::Ground;
use crate::plan::{Action, Baseline, NoteKind, Plan, Side};
use crate::scan::{Entry, EntryKind, Snapshot};

pub struct OneWayPlanner {
    /// Mirror removes what's no longer in the source; Backup never removes anything.
    mirror: bool,
    /// Copies whose dates didn't stick on the destination, as both sides looked right after
    /// them (see `store::Landings`).
    landed: Baseline,
}

impl OneWayPlanner {
    pub fn mirror() -> Self {
        Self { mirror: true, landed: Baseline::new() }
    }

    pub fn backup() -> Self {
        Self { mirror: false, landed: Baseline::new() }
    }

    /// Plans knowing the copies whose dates didn't stick. Such a copy is left alone while
    /// neither side changed since; the record never removes or replaces anything.
    pub fn knowing_landed(mut self, landed: Baseline) -> Self {
        self.landed = landed;
        self
    }

    /// Whether the file at `path` is a copy whose date didn't stick, and neither side
    /// changed since it landed.
    fn landed_unchanged(&self, path: &str, wanted: &Entry, existing: &Entry) -> bool {
        self.landed.get(path).is_some_and(|copy| {
            wanted.is_file() && copy.source.is_file() && wanted.unchanged(&copy.source) && existing.unchanged(&copy.destination)
        })
    }

    /// How many files the plan leaves alone because they are copies whose dates didn't
    /// stick (for the review's note).
    pub fn landed_alike(&self, source: &Snapshot, destination: &Snapshot) -> u64 {
        self.landed
            .iter()
            .filter(|(path, _)| match (source.get(path), destination.get(path)) {
                (Some(wanted), Some(existing)) => !existing.same_as(wanted) && self.landed_unchanged(path, wanted, existing),
                _ => false,
            })
            .count() as u64
    }

    /// What it takes for `destination` to follow `source`.
    pub fn plan(&self, source: &Snapshot, destination: &Snapshot) -> Plan {
        let mut plan = Plan::default();
        let mut ground = Ground::new(source, destination, &[Side::Destination], &mut plan);
        let shifting = source.shifts_times() || destination.shifts_times();

        for (path, wanted) in source.iter() {
            if ground.is_off_limits(path) {
                continue;
            }
            match destination.get(path) {
                Some(existing) if existing.same_as(wanted) => {}
                // Copied before, but the destination's drive didn't keep the date.
                Some(existing) if self.landed_unchanged(path, wanted, existing) => {}
                // A FAT or exFAT drive moved its times by whole hours (daylight saving).
                Some(existing)
                    if shifting && existing.is_file() && wanted.is_file() && existing.size == wanted.size
                        && Entry::whole_hours_apart(existing.modified, wanted.modified) => {}
                // Something of another kind is in the way (a file where a folder goes, a link…).
                Some(existing) if existing.kind != wanted.kind => {
                    if wanted.is_link() {
                        // Usually a file or folder moved to another drive with a link left
                        // behind: the copy here may be the only other copy of what it holds.
                        let reason = if existing.is_dir() {
                            "kept: the source now has a link here instead of this folder, so Charon keeps the folder and what's in it"
                        } else {
                            "kept: the source now has a link here instead of this file, so Charon keeps the file"
                        };
                        plan.note(NoteKind::Blocked, Some(Side::Destination), destination.spelling(path), reason);
                        ground.block(path);
                        continue;
                    }
                    if !self.mirror {
                        plan.note(
                            NoteKind::Blocked,
                            Some(Side::Destination),
                            destination.spelling(path),
                            format!(
                                "left alone: it's {} on the destination but {} in the source, and Backup never removes anything",
                                Self::a(existing.kind),
                                Self::a(wanted.kind)
                            ),
                        );
                        ground.block(path);
                        continue;
                    }
                    if existing.is_dir() && destination.is_occupied(path) {
                        plan.note(
                            NoteKind::KeptFolder,
                            Some(Side::Destination),
                            destination.spelling(path),
                            format!("kept: the source has a file here, but this folder holds {LEFT_ALONE}"),
                        );
                        ground.block(path);
                        continue;
                    }
                    // What's inside a folder in the way is removed item by item (below).
                    plan.push(Action::delete(Side::Destination, path, *existing, Some(*wanted)));
                    Self::bring(&mut plan, path, wanted, None);
                }
                existing => Self::bring(&mut plan, path, wanted, existing.copied()),
            }
        }
        if self.mirror {
            Self::plan_removals(source, destination, &ground, &mut plan);
        }
        ground.hold_removals(&mut plan);
        ground.finish(&mut plan);
        plan
    }

    /// Creates the folder, or copies the file or link.
    fn bring(plan: &mut Plan, path: &str, wanted: &Entry, existing: Option<Entry>) {
        if wanted.is_dir() {
            plan.push(Action::CreateDir { side: Side::Destination, path: path.to_string() });
        } else {
            plan.push(Action::copy(Side::Destination, path, *wanted, existing));
        }
    }

    /// Mirror: whatever the source no longer has, listed item by item. A folder that holds
    /// things Charon leaves alone (see `LEFT_ALONE`) stays.
    fn plan_removals(source: &Snapshot, destination: &Snapshot, ground: &Ground, plan: &mut Plan) {
        for (path, entry) in destination.iter() {
            if source.contains(path) || ground.is_off_limits(path) {
                continue;
            }
            if entry.is_dir() && destination.is_occupied(path) {
                plan.note(
                    NoteKind::KeptFolder,
                    Some(Side::Destination),
                    destination.spelling(path),
                    format!("kept: the source no longer has it, but it holds {LEFT_ALONE}"),
                );
                continue;
            }
            plan.push(Action::delete(Side::Destination, path, *entry, None));
        }
    }

    fn a(kind: EntryKind) -> &'static str {
        match kind {
            EntryKind::File => "a file",
            EntryKind::Dir => "a folder",
            EntryKind::Link => "a link",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[(&str, Option<(u64, i64)>)]) -> Snapshot {
        items
            .iter()
            .map(|(p, f)| (p.to_string(), f.map_or(Entry::dir(), |(s, m)| Entry::file(s, m))))
            .collect()
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.ordered().iter().map(|a| format!("{:?} {}", std::mem::discriminant(*a), a.path())).collect()
    }

    #[test]
    fn copies_new_and_changed_files() {
        let src = snap(&[("a.txt", Some((1, 1000))), ("b.txt", Some((2, 9000))), ("docs", None), ("docs/c.txt", Some((3, 1000)))]);
        let dst = snap(&[("a.txt", Some((1, 1500))), ("b.txt", Some((2, 1000)))]);
        let plan = OneWayPlanner::backup().plan(&src, &dst);
        let s = plan.summary();
        assert_eq!(s.copies, 2); // b changed, c new; a is within the time tolerance
        assert_eq!(s.replaces, 1);
        assert_eq!(s.new_folders, 1);
        assert_eq!(s.deletes, 0);
    }

    #[test]
    fn backup_never_deletes() {
        let src = snap(&[]);
        let dst = snap(&[("old.txt", Some((1, 1)))]);
        assert!(OneWayPlanner::backup().plan(&src, &dst).is_settled());
    }

    #[test]
    fn mirror_lists_every_removed_item_deepest_first() {
        let src = snap(&[("keep.txt", Some((1, 1)))]);
        let dst = snap(&[("keep.txt", Some((1, 1))), ("gone", None), ("gone/a.txt", Some((1, 1))), ("gone-too.txt", Some((1, 1)))]);
        let plan = OneWayPlanner::mirror().plan(&src, &dst);
        let ordered: Vec<&str> = plan.ordered().iter().map(|a| a.path()).collect();
        assert_eq!(ordered, vec!["gone/a.txt", "gone", "gone-too.txt"]);
    }

    #[test]
    fn mirror_keeps_folders_holding_excluded_items() {
        let src = snap(&[]);
        let mut dst = snap(&[("old", None), ("old/a.txt", Some((1, 1)))]);
        dst.occupy("old"); // e.g. old/build.tmp is excluded
        let plan = OneWayPlanner::mirror().plan(&src, &dst);
        let ordered: Vec<&str> = plan.ordered().iter().map(|a| a.path()).collect();
        assert_eq!(ordered, vec!["old/a.txt"]);
        assert_eq!(plan.notes[0].kind, NoteKind::KeptFolder);
        assert_eq!(plan.notes[0].reason, "kept: the source no longer has it, but it holds items Charon leaves alone (excluded, left out or unreadable items, or what Charon or a cloud service keeps for its own use)");
    }

    #[test]
    fn mirror_replaces_items_of_the_other_kind_one_by_one() {
        let src = snap(&[("x", Some((1, 1)))]);
        let dst = snap(&[("x", None), ("x/inner.txt", Some((1, 1)))]);
        let plan = OneWayPlanner::mirror().plan(&src, &dst);
        let ordered = plan.ordered();
        assert!(matches!(ordered[0], Action::Delete { path, .. } if path == "x/inner.txt"));
        assert!(matches!(ordered[1], Action::Delete { path, item: EntryKind::Dir, .. } if path == "x"));
        assert!(matches!(ordered[2], Action::Copy { replaces: false, .. }));
    }

    #[test]
    fn backup_leaves_items_of_the_other_kind_alone() {
        let src = snap(&[("x", None), ("x/new.txt", Some((1, 1)))]);
        let dst = snap(&[("x", Some((5, 5)))]);
        let plan = OneWayPlanner::backup().plan(&src, &dst);
        assert!(plan.actions.is_empty(), "{:?}", paths(&plan));
        assert_eq!(plan.notes[0].kind, NoteKind::Blocked);
    }

    #[test]
    fn a_link_never_replaces_a_folder() {
        let src: Snapshot = [("footage".to_string(), Entry::link(b"/Volumes/SSD/footage", 1))].into_iter().collect();
        let dst = snap(&[("footage", None), ("footage/a.mov", Some((1, 1)))]);
        let plan = OneWayPlanner::mirror().plan(&src, &dst);
        assert!(plan.actions.is_empty(), "{:?}", paths(&plan));
        assert_eq!(plan.notes[0].kind, NoteKind::Blocked);
    }

    #[test]
    fn a_link_never_replaces_a_file() {
        // clip.mov moved to another drive, with a link left in its place.
        let src: Snapshot = [("clip.mov".to_string(), Entry::link(b"/Volumes/SSD/clip.mov", 1))].into_iter().collect();
        let dst = snap(&[("clip.mov", Some((4_000, 1)))]);
        for planner in [OneWayPlanner::mirror(), OneWayPlanner::backup()] {
            let plan = planner.plan(&src, &dst);
            assert!(plan.actions.is_empty(), "{:?}", paths(&plan));
            assert_eq!(plan.notes[0].kind, NoteKind::Blocked);
        }
        // A link that takes another link's place is simply carried.
        let dst: Snapshot = [("clip.mov".to_string(), Entry::link(b"/Volumes/Old/clip.mov", 1))].into_iter().collect();
        assert_eq!(OneWayPlanner::mirror().plan(&src, &dst).summary().replaces, 1);
    }

    #[test]
    fn unreadable_source_folders_leave_the_backup_alone() {
        let mut src = snap(&[("photos", None)]);
        src.freeze("photos", "permission denied");
        let dst = snap(&[("photos", None), ("photos/cat.jpg", Some((1, 1)))]);
        let plan = OneWayPlanner::mirror().plan(&src, &dst);
        assert!(plan.actions.is_empty(), "{:?}", paths(&plan));
        assert_eq!(plan.notes[0].kind, NoteKind::Unreadable);
    }

    #[test]
    fn a_copy_whose_date_didnt_stick_is_left_alone_until_a_side_changes() {
        // From 1975 in the source; FAT can't go before 1980.
        let (source_time, landed_time) = (157_766_400_000, 315_532_800_000);
        let src = snap(&[("old.txt", Some((4, source_time)))]);
        let dst = snap(&[("old.txt", Some((4, landed_time)))]);
        let mut landed = Baseline::new();
        landed.insert("old.txt", crate::plan::Pair::new(Entry::file(4, source_time), Entry::file(4, landed_time)));
        assert_eq!(OneWayPlanner::backup().plan(&src, &dst).actions.len(), 1, "without the record it's copied again");
        let planner = OneWayPlanner::mirror().knowing_landed(landed);
        assert!(planner.plan(&src, &dst).actions.is_empty());
        assert_eq!(planner.landed_alike(&src, &dst), 1);
        // Edited in the source since (same size): copied.
        let edited = snap(&[("old.txt", Some((4, source_time + 60_000)))]);
        assert_eq!(planner.plan(&edited, &dst).summary().replaces, 1);
        // Touched in the destination since: copied over again.
        let touched = snap(&[("old.txt", Some((4, landed_time + 60_000)))]);
        assert_eq!(planner.plan(&src, &touched).summary().replaces, 1);
        assert_eq!(planner.landed_alike(&src, &touched), 0);
    }

    #[test]
    fn the_record_of_copies_never_spares_a_removal() {
        let src = snap(&[]);
        let dst = snap(&[("old.txt", Some((4, 315_532_800_000)))]);
        let mut landed = Baseline::new();
        landed.insert("old.txt", crate::plan::Pair::new(Entry::file(4, 157_766_400_000), Entry::file(4, 315_532_800_000)));
        let plan = OneWayPlanner::mirror().knowing_landed(landed).plan(&src, &dst);
        assert_eq!(plan.summary().deletes, 1);
    }
}
