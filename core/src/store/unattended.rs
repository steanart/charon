//! Full automation: what runs nobody looked at removed or replaced, since the person last
//! confirmed a list.
//!
//! The guard (see `plan::Guard`) holds back a run that removes or replaces too much at once,
//! and adds up what automatic runs did during the last day. Damage that spreads more slowly
//! than that (a broken tool rewriting a few dozen files an hour, for weeks) would still get
//! through, one small run at a time, until every file is replaced. The originals of those
//! files are in .charon-trash, and runs nobody looked at never empty it, so the damage can
//! still be undone as long as someone notices.
//!
//! So Charon keeps a second, long count here: every file or link an automatic run removed
//! or replaced, per side, for as long as the trash keeps the originals, until the person
//! confirms a list again. It lives in its own file per job, apart from the settings, because
//! it can grow to many thousands of names. If the file can't be read, Charon doesn't know
//! what automatic runs did, and says so instead of starting the count again from nothing.

use crate::error::Result;
use crate::model::JobId;
use crate::plan::{Footprints, LongCount, Side};
use crate::store::atomic;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

const VERSION: u32 = 1;

/// What's inside a job's file: each item's key, with when an automatic run last removed or
/// replaced it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredFootprints {
    version: u32,
    #[serde(default)]
    source: BTreeMap<String, DateTime<Utc>>,
    #[serde(default)]
    destination: BTreeMap<String, DateTime<Utc>>,
}

impl StoredFootprints {
    fn side(&mut self, side: Side) -> &mut BTreeMap<String, DateTime<Utc>> {
        match side {
            Side::Source => &mut self.source,
            Side::Destination => &mut self.destination,
        }
    }

    /// Forgets what's older than `days` (0 keeps everything until the next confirmed list).
    fn forget_older_than(&mut self, days: u32, now: DateTime<Utc>) {
        if days == 0 {
            return;
        }
        let oldest = now - chrono::Duration::days(days as i64);
        for side in [Side::Source, Side::Destination] {
            self.side(side).retain(|_, at| *at >= oldest);
        }
    }
}

pub struct Unattended {
    folder: PathBuf,
    /// Notes and removals happen one at a time.
    writing: Mutex<()>,
}

impl Unattended {
    /// At most this many names are kept per side; the newest stay. Far more than any run
    /// the guard lets through within a day, so only weeks of steady changes reach it.
    pub const KEPT: usize = 100_000;

    pub fn new(folder: PathBuf) -> Self {
        Self { folder, writing: Mutex::new(()) }
    }

    /// The long count of `job`, without what's older than `days`.
    pub fn load(&self, job: &JobId, days: u32, now: DateTime<Utc>) -> LongCount {
        let mut stored = match self.read(job) {
            Ok(stored) => stored,
            Err(e) => {
                log::warn!("couldn't read what automatic syncs of job {job} changed: {e}");
                return LongCount { lost: true, ..LongCount::default() };
            }
        };
        stored.forget_older_than(days, now);
        let footprints = |items: BTreeMap<String, DateTime<Utc>>| Footprints { overflowing: items.len() >= Self::KEPT, keys: items.into_keys().collect() };
        LongCount { source: footprints(stored.source), destination: footprints(stored.destination), lost: false }
    }

    /// Adds what one automatic run removed or replaced (keys per side) at `now`, and forgets
    /// what's older than `days`. A file that can't be read is replaced by a fresh count that
    /// can't be trusted either: it stays unreadable, so the guard keeps asking until the
    /// person confirms a list.
    pub fn note(&self, job: &JobId, now: DateTime<Utc>, source: &[String], destination: &[String], days: u32) -> Result<()> {
        if source.is_empty() && destination.is_empty() {
            return Ok(());
        }
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let mut stored = self.read(job)?;
        stored.forget_older_than(days, now);
        for (side, changed) in [(Side::Source, source), (Side::Destination, destination)] {
            let items = stored.side(side);
            for key in changed {
                items.insert(key.clone(), now);
            }
            if items.len() > Self::KEPT {
                let mut by_age: Vec<(DateTime<Utc>, String)> = std::mem::take(items).into_iter().map(|(key, at)| (at, key)).collect();
                by_age.sort_unstable_by(|a, b| b.cmp(a));
                *items = by_age.into_iter().take(Self::KEPT).map(|(at, key)| (key, at)).collect();
            }
        }
        stored.version = VERSION;
        atomic::write(&self.file(job), &serde_json::to_vec(&stored)?)
    }

    /// Starts the count again: the person confirmed a list, or the job's folders changed or
    /// it was removed.
    pub fn forget(&self, job: &JobId) -> Result<()> {
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::remove_file(self.file(job)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::Error::file(self.file(job), e)),
        }
    }

    /// The stored count; empty when there is none yet.
    fn read(&self, job: &JobId) -> Result<StoredFootprints> {
        let path = self.file(job);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StoredFootprints::default()),
            Err(e) => return Err(crate::Error::file(path, e)),
        };
        match serde_json::from_slice::<StoredFootprints>(&bytes) {
            Ok(stored) if stored.version == VERSION => Ok(stored),
            Ok(stored) => Err(crate::Error::Invalid(format!("{} is from another version ({})", path.display(), stored.version))),
            Err(e) => Err(crate::Error::Invalid(format!("{} is damaged ({e})", path.display()))),
        }
    }

    fn file(&self, job: &JobId) -> PathBuf {
        self.folder.join(format!("{job}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn automatic_changes_add_up_until_the_trash_lets_their_originals_go() {
        let tmp = tempfile::tempdir().unwrap();
        let unattended = Unattended::new(tmp.path().join("unattended"));
        let job = "job".to_string();
        let now = Utc::now();
        unattended.note(&job, now - chrono::Duration::days(40), &keys(&["ancient.txt"]), &[], 30).unwrap();
        unattended.note(&job, now - chrono::Duration::days(20), &keys(&["a.txt", "b.txt"]), &[], 30).unwrap();
        unattended.note(&job, now - chrono::Duration::days(2), &keys(&["a.txt"]), &keys(&["c.txt"]), 30).unwrap();
        let count = unattended.load(&job, 30, now);
        assert_eq!(count.source.keys, set(&["a.txt", "b.txt"]), "older than the trash keeps is forgotten, a file counts once");
        assert_eq!(count.destination.keys, set(&["c.txt"]));
        assert!(!count.lost && !count.source.overflowing);
        // With a trash that keeps everything, nothing is forgotten until the next confirmed list.
        assert_eq!(unattended.load(&job, 0, now + chrono::Duration::days(400)).source.keys, set(&["a.txt", "b.txt"]));
        unattended.forget(&job).unwrap();
        assert_eq!(unattended.load(&job, 30, now), LongCount::default());
    }

    #[test]
    fn a_count_that_cant_be_read_is_never_taken_for_none() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("unattended");
        let unattended = Unattended::new(folder.clone());
        let job = "job".to_string();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("job.json"), "{ damaged").unwrap();
        assert!(unattended.load(&job, 30, Utc::now()).lost);
        // Nothing is added to a count nobody can read, and it isn't silently reset either.
        assert!(unattended.note(&job, Utc::now(), &keys(&["a.txt"]), &[], 30).is_err());
        assert!(unattended.load(&job, 30, Utc::now()).lost);
        unattended.forget(&job).unwrap();
        assert!(!unattended.load(&job, 30, Utc::now()).lost);
    }

    #[test]
    fn a_full_count_keeps_the_newest_and_says_it_overflows() {
        let tmp = tempfile::tempdir().unwrap();
        let unattended = Unattended::new(tmp.path().join("unattended"));
        let job = "job".to_string();
        let now = Utc::now();
        let first: Vec<String> = (0..Unattended::KEPT).map(|i| format!("old/{i}.txt")).collect();
        unattended.note(&job, now - chrono::Duration::hours(30), &first, &[], 30).unwrap();
        unattended.note(&job, now, &keys(&["new.txt"]), &[], 30).unwrap();
        let count = unattended.load(&job, 30, now);
        assert!(count.source.overflowing);
        assert_eq!(count.source.keys.len(), Unattended::KEPT);
        assert!(count.source.keys.contains("new.txt"));
    }
}
