//! Mirror and Backup: the copies whose dates didn't stick on the destination.
//!
//! A one-way job has no memory of the last sync: it compares the source with the
//! destination directly, by size and date. A destination drive that can't keep the dates
//! Charon sets (a file from before 1980 on a FAT drive, a date after 2038 on a NAS that
//! counts time in 32 bits, a WebDAV or other mounted drive that stamps its own time) would
//! then look different after every sync. The same files would wait to be copied again
//! every time, and each copy would put the identical old one into .charon-trash.
//!
//! So for those files only, Charon remembers how both sides looked right after the copy.
//! While neither side has changed since, the copy is still good and is left alone. The
//! record never justifies removing or replacing anything; it only spares a copy. It belongs
//! to the folders it was made with, and to the destination folder whose marker it names
//! (see `run::marker`): for another drive under the same name it isn't used.

use crate::error::Result;
use crate::model::{JobId, JobSetup};
use crate::plan::{Baseline, Settled};
use crate::run::Found;
use crate::scan::Snapshot;
use crate::store::atomic;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

const VERSION: u32 = 1;

/// What's inside a record file.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredLandings {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    /// The token of the marker in the destination when the copies landed.
    destination_token: String,
    /// Whether the keys ignore upper and lower case (see `Keys::fold`).
    folded: bool,
    pairs: Baseline,
}

pub struct Landings {
    folder: PathBuf,
    /// Saves and removals happen one at a time.
    writing: Mutex<()>,
}

impl Landings {
    pub fn new(folder: PathBuf) -> Self {
        Self { folder, writing: Mutex::new(()) }
    }

    /// The copies whose dates didn't stick, for the job's current folders, as long as the
    /// destination still carries the marker they landed with (`destination`, as read before
    /// the scans). Empty otherwise: Charon then compares by size and date as usual.
    pub fn load(&self, job: &JobId, setup: &JobSetup, folded: bool, destination: &Found) -> Baseline {
        let path = self.file(job);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Baseline::new(),
            Err(e) => {
                log::warn!("couldn't read {}: {e}", path.display());
                return Baseline::new();
            }
        };
        let stored = match serde_json::from_slice::<StoredLandings>(&bytes) {
            Ok(stored) if stored.version == VERSION => stored,
            Ok(stored) => {
                log::warn!("{} is from another version ({}); not used", path.display(), stored.version);
                return Baseline::new();
            }
            Err(e) => {
                log::warn!("{} is damaged ({e}); not used", path.display());
                return Baseline::new();
            }
        };
        let same_folders = stored.source == setup.source && stored.destination == setup.destination;
        let same_destination = matches!(destination, Found::Marker(marker) if marker.token == stored.destination_token);
        if same_folders && same_destination && stored.folded == folded {
            stored.pairs
        } else {
            Baseline::new()
        }
    }

    /// Saves the record after a sync, for the destination folder that carries the marker
    /// `destination_token`. An empty record removes the file.
    pub fn save(&self, job: &JobId, setup: &JobSetup, folded: bool, destination_token: &str, pairs: &Baseline) -> Result<()> {
        if pairs.is_empty() {
            return self.forget(job);
        }
        let stored = StoredLandings {
            version: VERSION,
            source: setup.source.clone(),
            destination: setup.destination.clone(),
            destination_token: destination_token.to_string(),
            folded,
            pairs: pairs.clone(),
        };
        let bytes = serde_json::to_vec(&stored)?;
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        atomic::write(&self.file(job), &bytes)
    }

    /// The record after a sync that was planned with `before` and finished the steps in
    /// `settled`, with `source` as the plan saw it; and how many files this sync copied
    /// whose dates didn't stick. A copy whose date stuck needs no record, and a record
    /// whose source file changed since can't spare a copy anymore.
    pub fn after_run(before: &Baseline, settled: &[Settled], source: &Snapshot) -> (Baseline, u64) {
        let mut next = before.clone();
        let mut undated = 0;
        for step in settled {
            match step.pair {
                Some(pair) if pair.source.is_file() && pair.destination.is_file() && !pair.destination.same_as(&pair.source) => {
                    next.insert(step.path.clone(), pair);
                    undated += 1;
                }
                _ => next.remove(&step.path),
            }
        }
        let done: BTreeSet<&str> = settled.iter().map(|step| step.path.as_str()).collect();
        next.retain(|path, copy| done.contains(path) || source.get(path).is_some_and(|now| now.unchanged(&copy.source)));
        (next, undated)
    }

    /// Forgets the record (the job was removed, or its folders or mode changed).
    pub fn forget(&self, job: &JobId) -> Result<()> {
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::remove_file(self.file(job)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::Error::file(self.file(job), e)),
        }
    }

    fn file(&self, job: &JobId) -> PathBuf {
        self.folder.join(format!("{job}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConflictRule, SyncMode, Trigger};
    use crate::plan::{Pair, Side};
    use crate::run::Marker;
    use crate::scan::Entry;

    fn setup(source: &str, destination: &str) -> JobSetup {
        JobSetup {
            name: "t".into(),
            source: source.into(),
            destination: destination.into(),
            mode: SyncMode::Backup,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        }
    }

    fn marker(token: &str) -> Found {
        Found::Marker(Marker::new(&"job".to_string(), Side::Destination, token, 0))
    }

    fn pairs() -> Baseline {
        let mut pairs = Baseline::new();
        // A file from 1975 that landed on FAT as 1 January 1980.
        pairs.insert("old.txt", Pair::new(Entry::file(4, 157_766_400_000), Entry::file(4, 315_532_800_000)));
        pairs
    }

    #[test]
    fn a_record_only_serves_its_folders_and_its_destination_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let landings = Landings::new(tmp.path().to_path_buf());
        let job = "job".to_string();
        landings.save(&job, &setup("/a", "/b"), false, "t1", &pairs()).unwrap();
        assert_eq!(landings.load(&job, &setup("/a", "/b"), false, &marker("t1")), pairs());
        // Another drive under the same name, no marker, other folders, other keys.
        assert!(landings.load(&job, &setup("/a", "/b"), false, &marker("t2")).is_empty());
        assert!(landings.load(&job, &setup("/a", "/b"), false, &Found::Missing).is_empty());
        assert!(landings.load(&job, &setup("/a", "/c"), false, &marker("t1")).is_empty());
        assert!(landings.load(&job, &setup("/a", "/b"), true, &marker("t1")).is_empty());
    }

    #[test]
    fn only_copies_whose_dates_didnt_stick_are_remembered() {
        let source: Snapshot = [
            ("old.txt".to_string(), Entry::file(4, 157_766_400_000)),
            ("new.txt".to_string(), Entry::file(5, 1_700_000_000_000)),
            ("kept.txt".to_string(), Entry::file(6, 1_000)),
            ("edited.txt".to_string(), Entry::file(7, 9_000)),
        ]
        .into_iter()
        .collect();
        let mut before = Baseline::new();
        // Still as it was: kept. Edited since: forgotten. Gone from the source: forgotten.
        before.insert("kept.txt", Pair::new(Entry::file(6, 1_000), Entry::file(6, 5_000)));
        before.insert("edited.txt", Pair::new(Entry::file(7, 1_000), Entry::file(7, 5_000)));
        before.insert("gone.txt", Pair::new(Entry::file(8, 1_000), Entry::file(8, 5_000)));
        let settled = vec![
            // Landed with another date: remembered.
            Settled { path: "old.txt".into(), pair: Some(Pair::new(Entry::file(4, 157_766_400_000), Entry::file(4, 315_532_800_000))) },
            // Landed with its date: nothing to remember.
            Settled { path: "new.txt".into(), pair: Some(Pair::new(Entry::file(5, 1_700_000_000_000), Entry::file(5, 1_700_000_000_000))) },
        ];
        let (next, undated) = Landings::after_run(&before, &settled, &source);
        assert_eq!(undated, 1);
        assert_eq!(next.paths().map(String::as_str).collect::<Vec<_>>(), vec!["kept.txt", "old.txt"]);
    }

    #[test]
    fn an_empty_record_leaves_no_file_and_a_damaged_one_is_not_used() {
        let tmp = tempfile::tempdir().unwrap();
        let landings = Landings::new(tmp.path().to_path_buf());
        let job = "job".to_string();
        landings.save(&job, &setup("/a", "/b"), false, "t1", &pairs()).unwrap();
        landings.save(&job, &setup("/a", "/b"), false, "t1", &Baseline::new()).unwrap();
        assert!(!tmp.path().join("job.json").exists());
        std::fs::write(tmp.path().join("job.json"), "{nope").unwrap();
        assert!(landings.load(&job, &setup("/a", "/b"), false, &marker("t1")).is_empty());
        landings.forget(&job).unwrap();
        landings.forget(&job).unwrap();
    }
}
