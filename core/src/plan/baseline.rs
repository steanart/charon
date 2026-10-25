//! The two-way memory: what each side looked like the last time both agreed.
//!
//! For every item both folders held in common after the last sync, the baseline keeps how
//! the item looked *on each side*. Asking "did the source change this?" then compares the
//! source with the source's own record, exactly, and the same for the destination. Two
//! drives don't always store times the same way (one rounds them, another doesn't keep
//! them at all), so comparing one side with what the *other* side had would mistake that
//! for a change.
//!
//! After a run, the baseline records what the run actually did, step by step, never what
//! a fresh look at the folders seems to say: a file that changed while the run was going
//! on is then still recognised as a change the next time.

use crate::plan::ground::Ground;
use crate::plan::{Action, Plan, Side};
use crate::scan::{Entry, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One item as each side had it when both agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    #[serde(rename = "s")]
    pub source: Entry,
    #[serde(rename = "d")]
    pub destination: Entry,
}

impl Pair {
    pub fn new(source: Entry, destination: Entry) -> Self {
        Self { source, destination }
    }

    pub fn side(&self, side: Side) -> &Entry {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }
}

/// What a finished step left behind at a path: both sides' items as they are now, or
/// `None` when the item is gone from both.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub path: String,
    pub pair: Option<Pair>,
}

/// An item a step renamed aside and put back as it was, because the step couldn't finish.
/// On drives whose change times the scans trust, the two renames moved its change time, and
/// nothing else about it changed.
#[derive(Debug, Clone, PartialEq)]
pub struct PutBack {
    pub side: Side,
    pub path: String,
    /// The item as the step found it, and as it is now that it's back.
    pub found: Entry,
    pub now: Entry,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Baseline {
    pairs: BTreeMap<String, Pair>,
}

impl Baseline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &str) -> Option<&Pair> {
        self.pairs.get(path)
    }

    /// What `side` had at `path` when both agreed.
    pub fn side(&self, path: &str, side: Side) -> Option<&Entry> {
        self.pairs.get(path).map(|pair| pair.side(side))
    }

    pub fn insert(&mut self, path: impl Into<String>, pair: Pair) {
        self.pairs.insert(path.into(), pair);
    }

    pub fn remove(&mut self, path: &str) {
        self.pairs.remove(path);
    }

    /// Keeps only the records `keep` says yes to.
    pub fn retain(&mut self, mut keep: impl FnMut(&str, &Pair) -> bool) {
        self.pairs.retain(|path, pair| keep(path, pair));
    }

    pub fn contains(&self, path: &str) -> bool {
        self.pairs.contains_key(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.pairs.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Pair)> {
        self.pairs.iter()
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Whether the baseline knows at least `n` items inside `folder`.
    pub fn held_at_least(&self, folder: &str, n: usize) -> bool {
        let prefix = format!("{folder}/");
        self.pairs.range(prefix.clone()..).take_while(|(path, _)| path.starts_with(&prefix)).take(n).count() >= n
    }

    /// Everything both snapshots hold in agreement (for tests and fresh starts).
    pub fn agreement(source: &Snapshot, destination: &Snapshot) -> Self {
        let pairs = source
            .iter()
            .filter_map(|(path, s)| {
                let d = destination.get(path)?;
                s.same_as(d).then(|| (path.clone(), Pair::new(*s, *d)))
            })
            .collect();
        Self { pairs }
    }

    /// The baseline to keep after a run that was planned from `source` and `destination`
    /// (with `old` as the baseline it used) and finished the steps in `settled`:
    ///
    /// * a finished step: what the step left behind;
    /// * an item both sides already agreed on: both sides as the plan saw them;
    /// * anything else (a skipped or failed step, an open conflict, a place that couldn't be
    ///   read, a run that stopped early): the old record, so the next run decides again. For
    ///   an item a failed step put back (`put_back`), with the change time it has now (see
    ///   `follow`).
    pub fn after_run(old: &Baseline, source: &Snapshot, destination: &Snapshot, plan: &Plan, settled: &[Settled], put_back: &[PutBack]) -> Baseline {
        let ground = Ground::quiet(source, destination, &[Side::Source, Side::Destination]);
        let planned: BTreeSet<&str> = plan.actions.iter().map(Action::path).collect();
        let all: BTreeSet<&String> = source.paths().chain(destination.paths()).chain(old.paths()).collect();

        let mut next = Baseline::new();
        for path in all {
            let keep_old = || old.get(path).copied();
            let pair = if ground.is_off_limits(path) || planned.contains(path.as_str()) {
                keep_old()
            } else {
                match (source.get(path), destination.get(path)) {
                    (Some(s), Some(d)) if s.same_as(d) => Some(Pair::new(*s, *d)),
                    (None, None) => None,
                    _ => keep_old(),
                }
            };
            if let Some(pair) = pair {
                next.insert(path.clone(), pair);
            }
        }
        next.record(settled);
        next.follow(put_back);
        next
    }

    /// The record of each item a failed step put back takes the change time Charon's own
    /// renames gave the item, where the record had the one the step found. The next look then
    /// sees the item as this run saw it: never an edit on its side, which would bring back an
    /// item removed on the other side, or make a conflict of a change on the other side.
    fn follow(&mut self, put_back: &[PutBack]) {
        for item in put_back {
            let Some(pair) = self.pairs.get_mut(&item.path) else { continue };
            let recorded = match item.side {
                Side::Source => &mut pair.source,
                Side::Destination => &mut pair.destination,
            };
            if recorded.changed != 0 && recorded.changed == item.found.changed {
                recorded.changed = item.now.changed;
            }
        }
    }

    /// Records what finished steps left behind (see `Settled`), leaving every other record
    /// as it is.
    pub fn record(&mut self, settled: &[Settled]) {
        for step in settled {
            match step.pair {
                Some(pair) => self.insert(step.path.clone(), pair),
                // A folder that went is gone with everything that was in it.
                None => self.forget_within(&step.path),
            }
        }
    }

    /// Forgets `path` and everything inside it.
    pub fn forget_within(&mut self, path: &str) {
        self.remove(path);
        let prefix = format!("{path}/");
        let inside: Vec<String> = self.pairs.range(prefix.clone()..).take_while(|(p, _)| p.starts_with(&prefix)).map(|(p, _)| p.clone()).collect();
        for path in inside {
            self.remove(&path);
        }
    }
}

impl FromIterator<(String, Pair)> for Baseline {
    fn from_iter<T: IntoIterator<Item = (String, Pair)>>(iter: T) -> Self {
        Self { pairs: iter.into_iter().collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[(&str, Entry)]) -> Snapshot {
        items.iter().map(|(p, e)| (p.to_string(), *e)).collect()
    }

    #[test]
    fn records_what_the_run_did_not_what_a_later_look_would_see() {
        let a1 = Entry::file(1, 10_000);
        let old: Baseline = [("a.txt".to_string(), Pair::new(a1, a1))].into_iter().collect();
        // The plan copies a new version of a.txt to the destination.
        let a2 = Entry::file(2, 20_000);
        let source = snap(&[("a.txt", a2)]);
        let destination = snap(&[("a.txt", a1)]);
        let mut plan = Plan::default();
        plan.push(Action::copy(Side::Destination, "a.txt", a2, Some(a1)));
        let landed = Entry::file(2, 20_000);
        let settled = vec![Settled { path: "a.txt".into(), pair: Some(Pair::new(a2, landed)) }];
        let next = Baseline::after_run(&old, &source, &destination, &plan, &settled, &[]);
        assert_eq!(next.get("a.txt"), Some(&Pair::new(a2, landed)));
        // Saved again on the source meanwhile: the next run sees that as the source's change.
        let a3 = Entry::file(3, 30_000);
        assert!(!a3.unchanged(next.side("a.txt", Side::Source).unwrap()));
    }

    #[test]
    fn a_step_that_didnt_happen_keeps_the_old_record() {
        let a1 = Entry::file(1, 10_000);
        let old: Baseline = [("a.txt".to_string(), Pair::new(a1, a1))].into_iter().collect();
        let source = snap(&[]);
        let destination = snap(&[("a.txt", a1)]);
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, "a.txt", a1, None));
        let next = Baseline::after_run(&old, &source, &destination, &plan, &[], &[]);
        assert_eq!(next.get("a.txt"), Some(&Pair::new(a1, a1)), "the next run still knows it was deleted on the source");
    }

    #[test]
    fn agreement_is_recorded_side_by_side() {
        let s = Entry::file(1, 10_000);
        let d = Entry::file(1, 11_000); // rounded by the destination's drive
        let next = Baseline::after_run(&Baseline::new(), &snap(&[("a", s)]), &snap(&[("a", d)]), &Plan::default(), &[], &[]);
        assert_eq!(next.get("a"), Some(&Pair::new(s, d)));
    }

    #[test]
    fn a_removed_folder_takes_its_records_along() {
        let f = Entry::file(1, 1);
        let old: Baseline = [("x", Entry::dir()), ("x/a", f)].iter().map(|(p, e)| (p.to_string(), Pair::new(*e, *e))).collect();
        let mut plan = Plan::default();
        plan.push(Action::delete(Side::Destination, "x", Entry::dir(), None));
        let settled = vec![Settled { path: "x".into(), pair: None }];
        let next = Baseline::after_run(&old, &snap(&[]), &snap(&[("x", Entry::dir())]), &plan, &settled, &[]);
        assert!(next.is_empty(), "{next:?}");
    }

    /// A failed step put an item back as it was, with the change time Charon's own renames
    /// gave it: its record takes that change time, so the next look sees no edit there. A
    /// record that already differed from what the step found (an edit the plan saw) stays as
    /// it was, and so does one without a change time (a drive whose change times aren't
    /// trusted).
    #[test]
    fn the_record_of_an_item_put_back_follows_its_new_change_time() {
        let other = Entry::file(9, 5_000);
        let synced = Entry::file(4, 1_000).changed_at(10);
        let edited = Entry::file(4, 1_000).changed_at(20);
        let untimed = Entry::file(4, 1_000);
        let old: Baseline = [("photo.jpg", synced), ("doc.txt", synced), ("card.jpg", untimed)]
            .into_iter()
            .map(|(path, entry)| (path.to_string(), Pair::new(other, entry)))
            .collect();
        let mut plan = Plan::default();
        for (path, found) in [("photo.jpg", synced), ("doc.txt", edited), ("card.jpg", untimed)] {
            plan.push(Action::delete(Side::Destination, path, found, None));
        }
        let destination = snap(&[("photo.jpg", synced), ("doc.txt", edited), ("card.jpg", untimed)]);
        let put_back = [
            PutBack { side: Side::Destination, path: "photo.jpg".into(), found: synced, now: synced.changed_at(30) },
            PutBack { side: Side::Destination, path: "doc.txt".into(), found: edited, now: edited.changed_at(40) },
            PutBack { side: Side::Destination, path: "card.jpg".into(), found: untimed, now: untimed.changed_at(50) },
        ];
        let next = Baseline::after_run(&old, &snap(&[]), &destination, &plan, &[], &put_back);
        assert_eq!(next.get("photo.jpg"), Some(&Pair::new(other, synced.changed_at(30))));
        assert_eq!(next.get("doc.txt"), Some(&Pair::new(other, synced)), "an edit the plan saw stays one");
        assert_eq!(next.get("card.jpg"), Some(&Pair::new(other, untimed)));
    }

    #[test]
    fn unreadable_places_keep_their_records() {
        let f = Entry::file(1, 1);
        let old: Baseline = [("x".to_string(), Pair::new(Entry::dir(), Entry::dir())), ("x/a".to_string(), Pair::new(f, f))].into_iter().collect();
        let mut source = snap(&[("x", Entry::dir())]);
        source.freeze("x", "permission denied");
        let destination = snap(&[("x", Entry::dir()), ("x/a", f)]);
        let next = Baseline::after_run(&old, &source, &destination, &Plan::default(), &[], &[]);
        assert!(next.contains("x/a"));
    }

    #[test]
    fn forgetting_a_place_forgets_everything_inside_it_and_nothing_else() {
        let pair = Pair::new(Entry::file(1, 1), Entry::file(1, 1));
        let mut b: Baseline = [("proj", pair), ("proj/a.txt", pair), ("proj/sub/b.txt", pair), ("project.txt", pair)]
            .into_iter()
            .map(|(p, e)| (p.to_string(), e))
            .collect();
        b.forget_within("proj");
        assert!(!b.contains("proj") && !b.contains("proj/a.txt") && !b.contains("proj/sub/b.txt"));
        assert!(b.contains("project.txt"));
        // What a settlement then records is all that's remembered under it.
        b.record(&[Settled { path: "proj".into(), pair: Some(Pair::new(Entry::dir(), Entry::dir())) }]);
        assert!(b.contains("proj") && !b.contains("proj/a.txt"));
    }
}
