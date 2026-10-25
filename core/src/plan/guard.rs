//! The safety checks every plan goes through before it touches anything.
//!
//! * A **refusal** stops the run in every mode: one side is empty while the plan would
//!   delete on the other (almost always an unmounted drive or a wrong folder).
//! * A **concern** is a plan that looks like something went wrong: it removes or replaces a
//!   large part of one side, empties files, or follows a folder that suddenly looks empty.
//!   A run nobody looked at (full automation) stops before it starts and waits for the
//!   person's OK; a review shows the concern in red and asks for an explicit tick.

use crate::model::{human_count, plural};
use crate::plan::{Action, Baseline, Plan, Side};
use crate::scan::{Keys, Snapshot};
use std::collections::{BTreeMap, BTreeSet};

/// Removals that need a look: at least this many files on one side…
const FEW: u64 = 3;
/// …that are at least half of the files there.
const MOST: f64 = 0.5;
/// Removals and replacements that need a look: at least this many files on one side that
/// are at least half of the files there (a small folder rewritten by a broken tool)…
const FEW_DESTRUCTIVE: u64 = 5;
/// Removals and replacements that need a look: more than this many files on one side…
const MANY: u64 = 20;
/// …that are more than this share of the files there, or…
const MASS_SHARE: f64 = 0.25;
/// …more than this many in any case.
const MASS_MAX: u64 = 1_000;
/// A folder that held at least this many items and now looks empty needs a look.
const EMPTIED_FOLDER: usize = 5;
/// At most this many names are listed in one concern.
const LISTED: usize = 3;

/// Files and links removed or replaced by automatic runs, per side (by key: a file replaced
/// again and again counts once): during the last day, and since the person last confirmed a
/// list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recent {
    pub source: BTreeSet<String>,
    pub destination: BTreeSet<String>,
    pub since_confirmed: LongCount,
}

impl Recent {
    fn on(&self, side: Side) -> &BTreeSet<String> {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }
}

/// What automatic runs removed or replaced since the person last confirmed a list, for as
/// long as .charon-trash keeps the originals (see `store::Unattended`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LongCount {
    pub source: Footprints,
    pub destination: Footprints,
    /// The count couldn't be read: nobody can say what automatic runs did.
    pub lost: bool,
}

impl LongCount {
    pub fn on(&self, side: Side) -> &Footprints {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }
}

/// What automatic runs removed or replaced on one side since the last confirmed list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Footprints {
    /// The keys of the files and links, each once.
    pub keys: BTreeSet<String>,
    /// There were more than the count keeps, so not all of them are named.
    pub overflowing: bool,
}

pub struct Guard;

impl Guard {
    /// Why this plan must not run at all, if it mustn't.
    pub fn refusal(plan: &Plan, source: &Snapshot, destination: &Snapshot) -> Option<String> {
        let deletes_on = |side: Side| plan.actions.iter().any(|a| matches!(a, Action::Delete { side: s, .. } if *s == side));
        if source.file_count() == 0 && destination.file_count() > 0 && deletes_on(Side::Destination) {
            return Some(
                "The source folder is empty, so this sync would remove everything in the destination. \
                 Charon won't do that: it's almost always a drive that isn't connected properly or a wrong folder. \
                 If you really want an empty destination, empty it yourself."
                    .into(),
            );
        }
        if destination.file_count() == 0 && source.file_count() > 0 && deletes_on(Side::Source) {
            return Some(
                "The destination folder is empty, so this sync would remove everything in the source. \
                 Charon won't do that: it's almost always a drive that isn't connected properly or a wrong folder."
                    .into(),
            );
        }
        None
    }

    /// Everything about this plan that deserves a careful look, in words. `recent` is given
    /// for jobs with full automation: what automatic runs already removed or replaced, today
    /// and since the person last confirmed a list, counts too, so damage that spreads slowly
    /// is caught as well, while its originals are still in .charon-trash.
    pub fn concerns(plan: &Plan, source: &Snapshot, destination: &Snapshot, baseline: &Baseline, recent: Option<&Recent>) -> Vec<String> {
        let mut concerns = Vec::new();
        for (side, snapshot, other) in [(Side::Source, source, destination), (Side::Destination, destination, source)] {
            let total = snapshot.file_count() as u64;
            let removed = plan.removed_on(side);
            let destructive = plan.destructive_on(side);
            let this_run = if Self::most(removed, total) {
                Some(format!("This sync would remove {} of the {} files in the {}.", human_count(removed), human_count(total), side.word()))
            } else if Self::mass(destructive, total) || Self::most_destructive(destructive, total) {
                Some(format!(
                    "This sync would remove or replace {} of the {} files in the {}.",
                    human_count(destructive),
                    human_count(total),
                    side.word()
                ))
            } else {
                None
            };
            let earlier = match (&this_run, recent) {
                (None, Some(recent)) if destructive > 0 => Self::within_a_day(plan, snapshot, recent.on(side), side)
                    .or_else(|| Self::since_confirmed(plan, snapshot, recent.since_confirmed.on(side), side)),
                _ => None,
            };
            concerns.extend(this_run.into_iter().chain(earlier));
            if let Some(concern) = Self::emptied_files(plan, side) {
                concerns.push(concern);
            }
            concerns.extend(Self::emptied_folders(plan, other, baseline, side));
        }
        let destroys = plan.destructive_on(Side::Source) + plan.destructive_on(Side::Destination) > 0;
        if destroys && recent.is_some_and(|recent| recent.since_confirmed.lost) {
            concerns.push(
                "Charon couldn't read its count of what automatic syncs removed or replaced since you last confirmed a list, \
                 so it can't tell whether damage has been spreading slowly. Please look at this list before anything more is removed or replaced."
                    .into(),
            );
        }
        concerns
    }

    /// At least a few files, and at least half of them.
    fn most(hit: u64, total: u64) -> bool {
        hit >= FEW && hit as f64 >= total as f64 * MOST
    }

    /// Many files and a good share of them, or very many.
    fn mass(hit: u64, total: u64) -> bool {
        Self::large_share(hit, total) || hit > MASS_MAX
    }

    /// Many files that are a good share of them.
    fn large_share(hit: u64, total: u64) -> bool {
        hit > MANY && hit as f64 > total as f64 * MASS_SHARE
    }

    /// Several files that are at least half of them.
    fn most_destructive(hit: u64, total: u64) -> bool {
        hit >= FEW_DESTRUCTIVE && hit as f64 >= total as f64 * MOST
    }

    /// The files this plan and the automatic runs of the last day (`earlier`) removed or
    /// replaced on `side` together, each counted once, if that's too many.
    fn within_a_day(plan: &Plan, snapshot: &Snapshot, earlier: &BTreeSet<String>, side: Side) -> Option<String> {
        let today: BTreeSet<&str> = earlier.iter().map(String::as_str).chain(plan.destructive_paths_on(side)).collect();
        // The files there at the start of the day: those there now, and those removed since.
        let gone = earlier.iter().filter(|key| !snapshot.contains(key)).count() as u64;
        let (today, total) = (today.len() as u64, snapshot.file_count() as u64 + gone);
        (Self::mass(today, total) || Self::most_destructive(today, total)).then(|| {
            format!(
                "Automatic syncs and this one would remove or replace {} of the {} files in the {} within a day.",
                human_count(today),
                human_count(total),
                side.word()
            )
        })
    }

    /// The files this plan and the automatic runs since the last confirmed list (`earlier`)
    /// removed or replaced on `side` together, each counted once, if that's a good share of
    /// them. A day's count can't see damage that spreads more slowly than that (a broken
    /// tool rewriting a few dozen files an hour, or a few files a day in a small folder);
    /// this one holds by the same rules without the day's fixed ceiling, so a folder of any
    /// size is held long before all of it is replaced.
    fn since_confirmed(plan: &Plan, snapshot: &Snapshot, earlier: &Footprints, side: Side) -> Option<String> {
        let hit: BTreeSet<&str> = earlier.keys.iter().map(String::as_str).chain(plan.destructive_paths_on(side)).collect();
        let gone = earlier.keys.iter().filter(|key| !snapshot.contains(key)).count() as u64;
        let (hit, total) = (hit.len() as u64, snapshot.file_count() as u64 + gone);
        (earlier.overflowing || Self::large_share(hit, total) || Self::most_destructive(hit, total)).then(|| {
            format!(
                "Automatic syncs since you last confirmed a list, and this one, would remove or replace {} of the {} files in the {}.",
                human_count(hit),
                human_count(total),
                side.word()
            )
        })
    }

    /// Files with contents that would be replaced by empty files: a crashed or broken app
    /// often leaves those behind.
    fn emptied_files(plan: &Plan, side: Side) -> Option<String> {
        let emptied: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Copy { to, path, from, existing: Some(existing), .. }
                    if *to == side && from.is_file() && from.size == 0 && existing.is_file() && existing.size > 0 =>
                {
                    Some(path.as_str())
                }
                _ => None,
            })
            .collect();
        (!emptied.is_empty()).then(|| {
            format!(
                "{} in the {} would be replaced by {}: {}.",
                Self::subject(emptied.len(), "file"),
                side.word(),
                if emptied.len() == 1 { "an empty file" } else { "empty files" },
                Self::list(&emptied)
            )
        })
    }

    /// Folders on the other side (`other`) that are there but look empty, while the plan
    /// carries that emptiness over to `side`: they held several items at the last sync
    /// (two-way), or the plan removes several items from the same folder on `side` (Mirror,
    /// which keeps no record of the last sync). A share or a cloud folder that lists a folder
    /// before it has loaded looks exactly like that.
    fn emptied_folders(plan: &Plan, other: &Snapshot, baseline: &Baseline, side: Side) -> Vec<String> {
        // How many items the plan removes from each folder on `side`, at any depth.
        let mut removed_inside: BTreeMap<&str, usize> = BTreeMap::new();
        for action in &plan.actions {
            if let Action::Delete { side: s, path, .. } = action {
                if *s == side {
                    for folder in Keys::ancestors(path) {
                        *removed_inside.entry(folder).or_default() += 1;
                    }
                }
            }
        }
        let emptied: Vec<&str> = removed_inside
            .into_iter()
            .filter(|(folder, removed)| {
                let looks_empty = other.get(folder).is_some_and(|e| e.is_dir()) && other.inside(folder).next().is_none();
                looks_empty && (baseline.held_at_least(folder, EMPTIED_FOLDER) || *removed >= EMPTIED_FOLDER)
            })
            .map(|(folder, _)| folder)
            .collect();
        // Only the topmost of nested ones.
        let folders: Vec<&str> =
            emptied.iter().copied().filter(|f| !emptied.iter().any(|top| top != f && Keys::is_within(f, top))).collect();
        let (remembered, removing): (Vec<&str>, Vec<&str>) =
            folders.into_iter().partition(|folder| baseline.held_at_least(folder, EMPTIED_FOLDER));
        let looks = |folders: &[&str]| if folders.len() == 1 { "looks" } else { "look" };
        let mut concerns = Vec::new();
        if !remembered.is_empty() {
            concerns.push(format!(
                "{} on the {} now {} empty, though {} several items at the last sync: {}.",
                Self::subject(remembered.len(), "folder"),
                side.opposite().word(),
                looks(&remembered),
                if remembered.len() == 1 { "it held" } else { "they held" },
                Self::list(&remembered)
            ));
        }
        if !removing.is_empty() {
            concerns.push(format!(
                "{} on the {} now {} empty, while this sync would remove several items from the same {} on the {}: {}.",
                Self::subject(removing.len(), "folder"),
                side.opposite().word(),
                looks(&removing),
                if removing.len() == 1 { "folder" } else { "folders" },
                side.word(),
                Self::list(&removing)
            ));
        }
        concerns
    }

    /// "1 file", "3 files" at the start of a sentence.
    fn subject(n: usize, word: &str) -> String {
        let text = plural(n as u64, word);
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => text,
        }
    }

    /// "a, b, c and 4 more".
    fn list(names: &[&str]) -> String {
        let shown: Vec<&str> = names.iter().take(LISTED).copied().collect();
        if names.len() > LISTED {
            format!("{} and {} more", shown.join(", "), human_count((names.len() - LISTED) as u64))
        } else {
            shown.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Pair;
    use crate::scan::Entry;

    fn files(n: usize) -> Snapshot {
        (0..n).map(|i| (format!("f{i}.txt"), Entry::file(1, 10_000))).collect()
    }

    fn deleting(n: usize) -> Plan {
        let mut plan = Plan::default();
        for i in 0..n {
            plan.push(Action::delete(Side::Destination, &format!("f{i}.txt"), Entry::file(1, 10_000), None));
        }
        plan
    }

    fn concerns(plan: &Plan, source: &Snapshot, destination: &Snapshot) -> Vec<String> {
        Guard::concerns(plan, source, destination, &Baseline::new(), None)
    }

    #[test]
    fn an_empty_side_is_refused_even_with_one_file_left_elsewhere() {
        assert!(Guard::refusal(&deleting(3), &files(0), &files(3)).is_some());
        assert!(Guard::refusal(&deleting(3), &files(1), &files(4)).is_none(), "that's for the concerns to catch");
        assert!(!concerns(&deleting(3), &files(1), &files(4)).is_empty());
    }

    #[test]
    fn small_folders_are_protected_too() {
        // 18 of 19 contracts: no "more than 20" rule would see that.
        assert!(!concerns(&deleting(18), &files(1), &files(19)).is_empty());
        // Two removals are routine.
        assert!(concerns(&deleting(2), &files(2), &files(4)).is_empty());
    }

    #[test]
    fn large_removals_need_a_look() {
        assert!(!concerns(&deleting(99), &files(1), &files(100)).is_empty());
        assert!(concerns(&deleting(5), &files(95), &files(100)).is_empty());
        // 21 of 10,000 is routine; 1,001 of 10,000 isn't.
        assert!(concerns(&deleting(21), &files(9_979), &files(10_000)).is_empty());
        assert!(!concerns(&deleting(1_001), &files(8_999), &files(10_000)).is_empty());
    }

    fn replacing(names: impl IntoIterator<Item = String>) -> Plan {
        let mut plan = Plan::default();
        for name in names {
            plan.push(Action::copy(Side::Destination, &name, Entry::file(2, 20_000), Some(Entry::file(1, 10_000))));
        }
        plan
    }

    fn earlier(names: impl IntoIterator<Item = String>) -> Recent {
        Recent { destination: names.into_iter().collect(), ..Recent::default() }
    }

    fn with_recent(plan: &Plan, source: &Snapshot, destination: &Snapshot, recent: &Recent) -> Vec<String> {
        Guard::concerns(plan, source, destination, &Baseline::new(), Some(recent))
    }

    #[test]
    fn slow_damage_adds_up_over_a_day() {
        let plan = deleting(10);
        let (source, destination) = (files(990), files(1_000));
        assert!(with_recent(&plan, &source, &destination, &Recent::default()).is_empty());
        let busy_day = earlier((100..400).map(|i| format!("f{i}.txt")));
        assert!(!with_recent(&plan, &source, &destination, &busy_day).is_empty());
    }

    #[test]
    fn a_small_folder_rewritten_in_place_needs_a_look() {
        // 19 contracts encrypted in place by ransomware: no removal, no empty file.
        let all = replacing((0..19).map(|i| format!("f{i}.txt")));
        assert!(!concerns(&all, &files(19), &files(19)).is_empty());
        // 4 of 19 edited is ordinary work.
        let few = replacing((0..4).map(|i| format!("f{i}.txt")));
        assert!(concerns(&few, &files(19), &files(19)).is_empty());
    }

    #[test]
    fn a_small_folder_damaged_over_several_runs_needs_a_look() {
        // 15 replaced earlier today, 4 now.
        let plan = replacing((15..19).map(|i| format!("f{i}.txt")));
        let recent = earlier((0..15).map(|i| format!("f{i}.txt")));
        assert!(!with_recent(&plan, &files(19), &files(19), &recent).is_empty());
        // 2 removed now and 4 earlier today, of 6.
        let recent = earlier((2..6).map(|i| format!("f{i}.txt")));
        assert!(!with_recent(&deleting(2), &files(2), &files(2), &recent).is_empty());
    }

    #[test]
    fn a_file_saved_again_and_again_counts_once() {
        // A log replaced in every run of the day, in a folder of 80.
        let recent = earlier(["f0.txt".to_string()]);
        let plan = replacing(["f0.txt".to_string()]);
        assert!(with_recent(&plan, &files(80), &files(80), &recent).is_empty());
        // 21 different files of 80 aren't ordinary.
        let recent = earlier((1..21).map(|i| format!("f{i}.txt")));
        assert!(!with_recent(&plan, &files(80), &files(80), &recent).is_empty());
    }

    #[test]
    fn damage_slower_than_a_day_is_caught_long_before_everything_is_replaced() {
        // A broken tool rewrites 41 photos an hour of 10,000: under the day's count, which
        // never holds fewer than 1,000 files a day on a side this large.
        let names = |hours: std::ops::Range<usize>| (hours.start * 41..hours.end * 41).map(|i| format!("f{i}.txt"));
        let (source, destination) = (files(10_000), files(10_000));
        let mut since_confirmed = LongCount::default();
        let mut hour = 0;
        loop {
            let plan = replacing(names(hour..hour + 1));
            let recent = Recent { destination: names(hour.saturating_sub(23)..hour).collect(), since_confirmed: since_confirmed.clone(), ..Recent::default() };
            let found = with_recent(&plan, &source, &destination, &recent);
            if !found.is_empty() {
                assert!(found[0].contains("since you last confirmed a list"), "{found:?}");
                break;
            }
            since_confirmed.destination.keys.extend(names(hour..hour + 1));
            hour += 1;
            assert!(hour * 41 < 10_000, "every file was replaced without a hold");
        }
        // Held once a quarter of the folder was replaced: within three days.
        assert!(hour < 72, "held only after {hour} hours");

        // Without the long count, the same pace is never held.
        let day_only = Recent { destination: names(hour - 23..hour).collect(), ..Recent::default() };
        assert!(with_recent(&replacing(names(hour..hour + 1)), &source, &destination, &day_only).is_empty());
    }

    #[test]
    fn a_small_folder_damaged_more_slowly_than_a_day_needs_a_look() {
        // 20 contracts, rewritten by a broken tool a few a day with more than a day between
        // automatic runs: 9, then 9, then 2. No day ever sees half of them.
        let names = |range: std::ops::Range<usize>| range.map(|i| format!("f{i}.txt"));
        let (source, destination) = (files(20), files(20));
        let first = Recent::default();
        assert!(with_recent(&replacing(names(0..9)), &source, &destination, &first).is_empty(), "9 of 20 in a run is ordinary work");
        let mut second = Recent::default();
        second.since_confirmed.destination.keys.extend(names(0..9));
        let found = with_recent(&replacing(names(9..18)), &source, &destination, &second);
        assert!(found.iter().any(|concern| concern.contains("since you last confirmed a list")), "{found:?}");
        // Edits of fewer than half the files since the last confirmed list are ordinary.
        let mut few = Recent::default();
        few.since_confirmed.destination.keys.extend(names(0..6));
        assert!(with_recent(&replacing(names(6..9)), &source, &destination, &few).is_empty());
    }

    #[test]
    fn a_long_count_that_overflowed_or_was_lost_needs_a_look() {
        let plan = replacing(["f0.txt".to_string()]);
        let mut recent = Recent::default();
        assert!(with_recent(&plan, &files(1_000_000), &files(1_000_000), &recent).is_empty());
        recent.since_confirmed.destination.overflowing = true;
        assert!(!with_recent(&plan, &files(1_000_000), &files(1_000_000), &recent).is_empty());
        let lost = Recent { since_confirmed: LongCount { lost: true, ..LongCount::default() }, ..Recent::default() };
        assert!(!with_recent(&plan, &files(100), &files(100), &lost).is_empty());
        // A plan that removes and replaces nothing is no danger, whatever the count says.
        let mut adding = Plan::default();
        adding.push(Action::copy(Side::Destination, "new.txt", Entry::file(1, 10_000), None));
        assert!(with_recent(&adding, &files(100), &files(100), &lost).is_empty());
    }

    #[test]
    fn files_emptied_by_a_broken_app_need_a_look() {
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "report.docx", Entry::file(0, 20_000), Some(Entry::file(52_000, 10_000))));
        let found = concerns(&plan, &files(100), &files(100));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("report.docx"));
    }

    #[test]
    fn a_folder_that_suddenly_looks_empty_needs_a_look() {
        let mut baseline = Baseline::new();
        let mut plan = Plan::default();
        baseline.insert("contracts", Pair::new(Entry::dir(), Entry::dir()));
        for i in 0..6 {
            let path = format!("contracts/c{i}.pdf");
            baseline.insert(path.clone(), Pair::new(Entry::file(1, 1), Entry::file(1, 1)));
            plan.push(Action::delete(Side::Source, &path, Entry::file(1, 1), None));
        }
        let mut destination = files(1_000);
        destination.insert("contracts", Entry::dir());
        let found = Guard::concerns(&plan, &files(1_000), &destination, &baseline, None);
        assert!(found.iter().any(|c| c.contains("contracts") && c.contains("destination")), "{found:?}");
    }

    /// A Mirror whose source folder `photos` is there but lists none of the `n` files the
    /// destination holds in it, in folders of a thousand files.
    fn mirror_of_an_empty_looking_folder(n: usize) -> Vec<String> {
        let mut plan = Plan::default();
        let mut source = files(1_000);
        let mut destination = files(1_000);
        source.insert("photos", Entry::dir());
        destination.insert("photos", Entry::dir());
        for i in 0..n {
            let path = format!("photos/p{i}.jpg");
            destination.insert(path.clone(), Entry::file(1, 1));
            plan.push(Action::delete(Side::Destination, &path, Entry::file(1, 1), None));
        }
        concerns(&plan, &source, &destination)
    }

    #[test]
    fn a_mirrored_folder_that_suddenly_looks_empty_needs_a_look() {
        // Mirror keeps no record of the last sync: what the destination holds there counts.
        let found = mirror_of_an_empty_looking_folder(6);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("photos") && found[0].contains("source"), "{found:?}");
        // Emptying a small folder is ordinary work.
        assert!(mirror_of_an_empty_looking_folder(4).is_empty());
    }
}
