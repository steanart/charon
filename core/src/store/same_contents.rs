//! Two-way: pairs of files a full read found identical, with how both sides looked then.
//!
//! A background look reads only so much to compare files by their contents (see
//! `Engine::compare_contents`), and a pair it leaves unread is not compared yet: never "in
//! sync". A large file whose details alone changed on one side (a permission, a tag, an app
//! noting that it was opened), or whose time a drive rounded or shifted, would then be "not
//! compared yet" after every launch, though a list read it and found both sides identical,
//! until some other change was synced.
//!
//! So Charon keeps what full reads found: each pair found identical, with both sides' items
//! exactly as they were then. A background look that can't read a pair goes by this record
//! only while both sides are still exactly as recorded. Where the drive keeps change times
//! (see `Volume::keeps_change_times`), any write to the file moves its change time, and the
//! record no longer matches. Elsewhere it goes by size and modification time, as Charon's
//! memory of the last sync does on such a drive, where an edit that keeps both isn't seen
//! either way.
//!
//! The record belongs to the folders it was made with, and to the markers they carried then
//! (see `run::Marker`): another drive under the same name, a folder restored from a copy, or
//! the next sync makes it miss, and folders without a marker (before their first sync) keep
//! none. It only ever says "these two files were identical when both looked like this", and
//! never decides a removal or a copy: lists and runs read the files themselves. It goes with
//! the job's memory of the last sync.

use crate::error::Result;
use crate::model::{JobId, JobSetup};
use crate::plan::Baseline;
use crate::run::{Found, Markers};
use crate::store::atomic;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

const VERSION: u32 = 1;

/// What's inside a record file.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSameContents {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    /// The markers both folders carried when the pairs were read.
    source_marker: MarkerSeen,
    destination_marker: MarkerSeen,
    /// Whether the keys ignore upper and lower case (see `Keys::fold`).
    folded: bool,
    pairs: Baseline,
}

/// A folder's marker as a look found it: the token of the job's folders, and the count of
/// their syncs (see `run::Marker`).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkerSeen {
    token: String,
    generation: u64,
}

impl MarkerSeen {
    /// The marker `found` in a folder, if it carries one Charon can read.
    fn of(found: &Found) -> Option<Self> {
        match found {
            Found::Marker(marker) => Some(Self { token: marker.token.clone(), generation: marker.generation }),
            Found::Missing | Found::Unusable(_) => None,
        }
    }
}

/// The records of the pairs each job's reads found identical, one file per job.
pub struct SameContents {
    folder: PathBuf,
    /// Saves and removals happen one at a time.
    writing: Mutex<()>,
}

impl SameContents {
    pub fn new(folder: PathBuf) -> Self {
        Self { folder, writing: Mutex::new(()) }
    }

    /// The pairs found identical in the job's current folders, keyed `folded` or not, while
    /// both folders carry the same `markers` (as read before the scans) as when the pairs
    /// were read. Empty when there is no record, or one Charon can't use: the pairs are then
    /// read again, or are not compared yet.
    pub fn load(&self, job: &JobId, setup: &JobSetup, folded: bool, markers: &Markers) -> Baseline {
        let path = self.file(job);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Baseline::new(),
            Err(e) => {
                log::warn!("couldn't read {}: {e}", path.display());
                return Baseline::new();
            }
        };
        let stored = match serde_json::from_slice::<StoredSameContents>(&bytes) {
            Ok(stored) => stored,
            Err(e) => {
                log::warn!("{} is damaged ({e}); not used", path.display());
                return Baseline::new();
            }
        };
        let same_folders = stored.source == setup.source && stored.destination == setup.destination && stored.folded == folded;
        let same_markers = MarkerSeen::of(&markers.source).is_some_and(|seen| seen == stored.source_marker)
            && MarkerSeen::of(&markers.destination).is_some_and(|seen| seen == stored.destination_marker);
        if stored.version == VERSION && same_folders && same_markers {
            stored.pairs
        } else {
            Baseline::new()
        }
    }

    /// Saves the pairs found identical, keyed `folded` or not, for the folders that carry
    /// `markers` (as read before the scans). No pairs, or a folder without a marker Charon
    /// can read, removes the file.
    pub fn save(&self, job: &JobId, setup: &JobSetup, folded: bool, markers: &Markers, pairs: &Baseline) -> Result<()> {
        let (Some(source_marker), Some(destination_marker)) = (MarkerSeen::of(&markers.source), MarkerSeen::of(&markers.destination)) else {
            return self.forget(job);
        };
        if pairs.is_empty() {
            return self.forget(job);
        }
        let stored = StoredSameContents {
            version: VERSION,
            source: setup.source.clone(),
            destination: setup.destination.clone(),
            source_marker,
            destination_marker,
            folded,
            pairs: pairs.clone(),
        };
        let bytes = serde_json::to_vec(&stored)?;
        let _one_at_a_time = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        atomic::write(&self.file(job), &bytes)
    }

    /// Forgets the record (the job was removed, its folders or mode changed, or Charon forgot
    /// its memory of the last sync).
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
            mode: SyncMode::TwoWay,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        }
    }

    /// A large file found identical on both sides, read after its permissions changed on one.
    fn pairs() -> Baseline {
        let mut pairs = Baseline::new();
        let vault = |changed: i64| Entry::file(1 << 31, 1_700_000_000_000).changed_at(changed);
        pairs.insert("vault.hc", Pair::new(vault(1_700_000_100_000_000_000), vault(1_700_000_000_000_000_000)));
        pairs
    }

    /// The markers of the job's folders after `generation` syncs.
    fn markers(generation: u64) -> Markers {
        let marker = |side: Side| Found::Marker(Marker::new(&"job".to_string(), side, "token", generation));
        Markers { source: marker(Side::Source), destination: marker(Side::Destination) }
    }

    #[test]
    fn a_record_only_serves_its_own_folders_and_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let same = SameContents::new(tmp.path().to_path_buf());
        let job = "job".to_string();
        same.save(&job, &setup("/a", "/b"), false, &markers(3), &pairs()).unwrap();
        assert_eq!(same.load(&job, &setup("/a", "/b"), false, &markers(3)), pairs());
        assert!(same.load(&job, &setup("/a", "/c"), false, &markers(3)).is_empty());
        assert!(same.load(&job, &setup("/x", "/b"), false, &markers(3)).is_empty());
        assert!(same.load(&job, &setup("/a", "/b"), true, &markers(3)).is_empty(), "keys made the other way");
        same.forget(&job).unwrap();
        assert!(same.load(&job, &setup("/a", "/b"), false, &markers(3)).is_empty());
        same.forget(&job).unwrap();
    }

    /// Another drive under the same name, a folder restored from a copy, or a sync since: the
    /// folders don't carry the markers they carried when the pairs were read, and the files
    /// in them may be others that look the same.
    #[test]
    fn a_record_only_serves_folders_that_carry_the_same_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let same = SameContents::new(tmp.path().to_path_buf());
        let job = "job".to_string();
        let folders = setup("/a", "/b");
        same.save(&job, &folders, false, &markers(3), &pairs()).unwrap();
        assert_eq!(same.load(&job, &folders, false, &markers(3)), pairs());
        assert!(same.load(&job, &folders, false, &markers(2)).is_empty(), "restored from a copy");
        assert!(same.load(&job, &folders, false, &markers(4)).is_empty(), "synced since");
        let another = Found::Marker(Marker::new(&job, Side::Destination, "another", 3));
        for destination in [another, Found::Missing, Found::Unusable("damaged".into())] {
            assert!(same.load(&job, &folders, false, &Markers { destination, ..markers(3) }).is_empty());
        }

        same.save(&job, &folders, false, &Markers { destination: Found::Missing, ..markers(3) }, &pairs()).unwrap();
        assert!(!tmp.path().join("job.json").exists(), "a folder without a marker keeps nothing");
    }

    /// Where a drive keeps no change time (FAT, exFAT, a network share), a pair is kept by
    /// size and modification time exactly as they were read, as the memory of the last sync
    /// goes by them there.
    #[test]
    fn pairs_on_drives_that_keep_no_change_times_are_kept_as_they_were_read() {
        let tmp = tempfile::tempdir().unwrap();
        let same = SameContents::new(tmp.path().to_path_buf());
        let job = "job".to_string();
        let file = Entry::file(1 << 31, 1_700_000_000_000);
        let timed = file.changed_at(1_700_000_000_000_000_000);
        let mut found = pairs();
        found.insert("on exFAT.mov", Pair::new(timed, file));
        found.insert("on a share.mov", Pair::new(file, file));
        same.save(&job, &setup("/a", "/b"), false, &markers(3), &found).unwrap();
        assert_eq!(same.load(&job, &setup("/a", "/b"), false, &markers(3)), found);

        same.save(&job, &setup("/a", "/b"), false, &markers(3), &Baseline::new()).unwrap();
        assert!(!tmp.path().join("job.json").exists(), "nothing to keep leaves no file");
    }

    #[test]
    fn a_damaged_record_is_not_used() {
        let tmp = tempfile::tempdir().unwrap();
        let same = SameContents::new(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("job.json"), "{not json").unwrap();
        assert!(same.load(&"job".to_string(), &setup("/a", "/b"), false, &markers(3)).is_empty());
    }
}
