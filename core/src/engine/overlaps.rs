//! Folders that are one place under two names: a job whose two folders are one inside the
//! other, and jobs that would undo each other's work because they share a folder.
//!
//! A job's folders are checked when it's saved, by their paths. Paths can't always tell: a
//! link, a drive or share mounted in a second place, or a folder made after the job was
//! saved can turn two different paths into one folder. And a job saved by an older Charon
//! was never checked at all. So every look checks again:
//!
//! * the job's own two folders, by the folders themselves (see `RootId::is_within`), and by
//!   the job's own markers: one of its folders found inside the other under another name
//!   (a share mounted twice, where the folders' numbers can't tell);
//! * every other job, by its folders' paths as they resolve now (see `JobSetup::clashes_with`),
//!   asked with a time limit: a folder that doesn't answer is compared at a later look;
//! * every other job, by the markers it left in its folders (see `run::Marker`): they are
//!   found inside this job's folders, at their top, or in a folder above, under whatever
//!   name the folder is reached.
//!
//! A marker found where it doesn't belong may also be a copy: the person copied a job's
//! folder, hidden `.charon` folder and all (restoring files from a backup, say). A copy is
//! harmless, and is only mentioned, so it can be deleted (see `Marker::origin`).

use crate::engine::Engine;
use crate::error::Result;
use crate::model::{Job, JobId, JobSetup, SyncMode};
use crate::plan::Side;
use crate::run::{Found, Marker, Origin};
use crate::scan::{Place, Snapshot};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What a look found about the job's folders and those of the other jobs.
#[derive(Debug, Default)]
pub(crate) struct Overlaps {
    /// Why this job must not sync at all: its two folders are one, or one is inside the other.
    pub refusal: Option<String>,
    /// The other jobs this one would undo the work of (or that would undo its work), each
    /// said in a sentence.
    pub clashes: Vec<String>,
    /// Folders holding a copy of a job's hidden `.charon` folder, each said in a sentence:
    /// harmless, and mentioned so the person can delete them.
    pub copies: Vec<String>,
    /// What couldn't be told only because another job's sync keeps a drive quiet.
    pub waiting: Waiting,
}

/// The clashes a look couldn't rule out only because another job's sync keeps the drive of
/// one of the folders quiet, so the disk wasn't asked whether a marker found is a copy (see
/// `clashes_by_markers`): they count as maybe one place until that sync is done. A run
/// nobody looks at that has no other reason to hold back waits for that sync instead.
#[derive(Debug, Default)]
pub(crate) struct Waiting {
    /// Each said in a sentence.
    pub clashes: Vec<String>,
    /// The first of them.
    pub on: Option<WaitsOn>,
}

/// A marker found where the disk couldn't be asked whether it's a copy, as a sync keeps the
/// drive quiet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WaitsOn {
    /// The folder on the drive kept quiet: the other job's, or the one holding its marker.
    pub folder: PathBuf,
    /// The folder holding the other job's marker.
    pub found: PathBuf,
    /// The name of the job whose marker it is.
    pub whose: String,
    /// The name of the job whose sync keeps the drive quiet.
    pub syncing: String,
}

/// A job's marker found around one of this job's folders.
struct MarkerAround {
    /// The side of this job where it was found.
    side: Side,
    /// The folder holding the marker.
    folder: PathBuf,
    job: JobId,
}

impl Engine {
    /// Checks the job's folders against each other and against every other job's (asks the
    /// disk). `source` and `destination` are this look's scans. `go_on` is asked before each
    /// step on a folder: a look that was stopped ends before its next step (see `look_at`).
    /// Only the question whether another job's folder holds a copy of its marker, which has a
    /// time limit, may go on after it (see `clashes_by_markers`).
    pub(crate) fn overlaps(&self, job: &Job, source: &Snapshot, destination: &Snapshot, go_on: &dyn Fn() -> Result<()>) -> Result<Overlaps> {
        let others: Vec<Job> = self.job_list().into_iter().filter(|other| other.id != job.id).collect();
        let around = Self::markers_around(job, source, destination, go_on)?;
        let mut copies = BTreeSet::new();
        let refusal = match Self::nested(&job.setup, go_on)? {
            Some(refusal) => Some(refusal),
            None => self.nested_by_markers(job, &around, &mut copies, go_on)?,
        };
        let mut clashes = Vec::new();
        let mut unseen: Vec<&Job> = Vec::new();
        for other in &others {
            go_on()?;
            // Asked with a time limit. A folder that doesn't answer (a share whose server is
            // away) isn't one of this job's folders, which this look just read: it's compared
            // again at a later look.
            if job.setup.clashes_as(&other.setup, |paths| self.folders_of(paths)) {
                clashes.push(format!(
                    "This job's folders overlap with those of “{}”, and one of the two is a Mirror job, which removes what the other brings in (a two-way job then carries those removals back). \
                     Check the removals in this list, and give one of the two jobs other folders.",
                    other.setup.name
                ));
            } else {
                unseen.push(other);
            }
        }
        let mut waiting = Waiting::default();
        clashes.extend(self.clashes_by_markers(job, &unseen, &around, &mut copies, &mut waiting, go_on)?);
        Ok(Overlaps { refusal, clashes, copies: copies.into_iter().collect(), waiting })
    }

    /// Why the job's two folders can't be synced, if one of them carries the job's marker of
    /// the other side where the other folder is found inside it, at its top or above it:
    /// the folders are one inside the other under names that don't show it (a share mounted
    /// twice). Only the markers the job left at its last finished sync count. A copy of the
    /// marker (in a copy of the other folder) is added to `copies` instead; when Charon can't
    /// tell whether it is one, it refuses, and says so.
    fn nested_by_markers(&self, job: &Job, around: &[MarkerAround], copies: &mut BTreeSet<String>, go_on: &dyn Fn() -> Result<()>) -> Result<Option<String>> {
        for found in around.iter().filter(|found| found.job == job.id) {
            go_on()?;
            let Found::Marker(marker) = Marker::read(&found.folder, &job.id) else { continue };
            if marker.side == found.side || !self.left_by(job, &marker) {
                continue;
            }
            let side = marker.side.word();
            go_on()?;
            let origin = Marker::origin(&job.id, job.setup.root(marker.side), &found.folder, &|| go_on().is_ok());
            // Stopped partway, the look ends, whatever the question could tell.
            go_on()?;
            match origin {
                Origin::SameFolder => {
                    return Ok(Some(format!(
                        "This job's folders are one place, or one is inside the other, under names that don't show it (a drive or share reachable under two names): \
                         Charon found the {side}'s marker in {}. Syncing would copy the job's own copies into themselves. Choose other folders.",
                        found.folder.display()
                    )))
                }
                Origin::Copy => {
                    copies.insert(Self::copy_note(&found.folder, &format!("this job's {side} folder")));
                }
                Origin::Away | Origin::Unknown => {
                    return Ok(Some(format!(
                        "Charon found the marker of this job's {side} folder in {}, and couldn't tell whether that folder is the {side} folder under another name \
                         (a drive or share reachable under two names), or holds a copy of its hidden .charon folder. Until it can tell, nothing syncs. \
                         If it's a copy, delete that .charon folder; otherwise, choose other folders.",
                        found.folder.display()
                    )))
                }
            }
        }
        Ok(None)
    }

    /// The other jobs (of `others`, whose paths don't show an overlap) whose markers are
    /// found around this job's folders where the two jobs would undo each other's work, each
    /// said in a sentence. Copies of their markers are added to `copies` instead. A job
    /// whose own folder is away can't sync now: it's looked at again at the next look. A
    /// clash that can't be ruled out only because a sync keeps a drive quiet goes in
    /// `waiting` instead.
    fn clashes_by_markers(
        &self,
        job: &Job,
        others: &[&Job],
        around: &[MarkerAround],
        copies: &mut BTreeSet<String>,
        waiting: &mut Waiting,
        go_on: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<String>> {
        let mut named: BTreeSet<&JobId> = BTreeSet::new();
        let mut clashes = Vec::new();
        for found in around {
            let Some(other) = others.iter().find(|other| other.id == found.job) else { continue };
            if named.contains(&other.id) {
                continue;
            }
            go_on()?;
            let Found::Marker(marker) = Marker::read(&found.folder, &other.id) else { continue };
            // Jobs that can't undo each other's work need no more looking (nor reading the
            // other job's memory, which can be large).
            if !Self::undoes(&job.setup, found.side, &other.setup, marker.side) || !self.left_by(other, &marker) {
                continue;
            }
            let (mine, theirs, name, folder) = (found.side.word(), marker.side.word(), &other.setup.name, found.folder.display());
            // The other job's folder is asked about with a time limit: one that doesn't answer
            // can't tell, the careful direction. The question goes on by itself when it's late
            // (whether the look ends or is stopped meanwhile), but it stops before it would
            // work on a drive a sync keeps quiet. While a sync keeps the drive of either folder
            // quiet, they aren't asked at all: the last time the disk told them apart holds (a
            // copy doesn't turn into the folder itself meanwhile), and a pair never told apart
            // this session can't tell until that sync is done (see `quiet.rs`).
            go_on()?;
            let root = other.setup.root(marker.side);
            let quiet = || self.kept_quiet(root) || self.kept_quiet(&found.folder);
            let skipped = quiet();
            let asked = if skipped {
                None
            } else {
                let (whose, here, engine) = (other.id.clone(), found.folder.clone(), self.clone());
                self.ask_about(root, move |root| engine.origin_unless_kept_quiet(&whose, root, &here))
            };
            // Who keeps the drive quiet, when that's all that keeps this pair from being told apart.
            let mut kept_by = None;
            let origin = match asked {
                Some(origin @ (Origin::SameFolder | Origin::Copy)) => {
                    self.inner.quiet.remember_origin(&other.id, root, &found.folder, origin);
                    origin
                }
                None | Some(Origin::Unknown) if skipped || quiet() => self.inner.quiet.origin(&other.id, root, &found.folder).unwrap_or_else(|| {
                    kept_by = self.inner.quiet.kept_by_another(&job.id, &[root, &found.folder]);
                    if let (None, Some(syncing)) = (&waiting.on, &kept_by) {
                        let folder = if self.kept_quiet(root) { root } else { &found.folder };
                        waiting.on = Some(WaitsOn { folder: folder.to_path_buf(), found: found.folder.clone(), whose: name.clone(), syncing: syncing.clone() });
                    }
                    Origin::Unknown
                }),
                asked => asked.unwrap_or(Origin::Unknown),
            };
            let sure = match origin {
                Origin::SameFolder => true,
                Origin::Unknown => false,
                Origin::Copy => {
                    copies.insert(Self::copy_note(&found.folder, &format!("the {theirs} folder of “{name}”")));
                    continue;
                }
                Origin::Away => continue,
            };
            named.insert(&other.id);
            if let Some(syncing) = kept_by {
                waiting.clashes.push(format!(
                    "This job's {mine} folder and the {theirs} folder of “{name}” may be one place, or one inside the other, though their paths differ (a drive or share reachable under two names, or a link): \
                     Charon found that job's marker in {folder}, and can't tell yet whether that folder is that job's folder or holds a copy of its hidden .charon folder: \
                     “{syncing}” is syncing on that drive, and Charon asks it once that's done. One of the two is a Mirror job, which removes what the other brings in. \
                     Check the removals in this list, or close it and look again once “{syncing}” is done."
                ));
                continue;
            }
            clashes.push(if sure {
                format!(
                    "This job's {mine} folder and the {theirs} folder of “{name}” are one place, or one is inside the other, though their paths differ (a drive or share reachable under two names, or a link): \
                     Charon found that job's marker in {folder}. One of the two is a Mirror job, which removes what the other brings in. \
                     Check the removals in this list, and give one of the two jobs other folders."
                )
            } else {
                format!(
                    "This job's {mine} folder and the {theirs} folder of “{name}” may be one place, or one inside the other, though their paths differ (a drive or share reachable under two names, or a link): \
                     Charon found that job's marker in {folder}, and couldn't tell whether that folder is that job's folder or holds a copy of its hidden .charon folder. \
                     One of the two is a Mirror job, which removes what the other brings in. Check the removals in this list. \
                     If that .charon folder is a copy, delete it; otherwise, give one of the two jobs other folders."
                )
            });
        }
        Ok(clashes)
    }

    /// Where the marker of job `whose` found in `found` comes from, when that job's folder is
    /// `root` (see `Marker::origin`), asking the disk only while no sync keeps the drive of
    /// either folder quiet: the question may go on after the look that asked it, and stops
    /// before its next step once a sync does (it then can't tell).
    pub(crate) fn origin_unless_kept_quiet(&self, whose: &JobId, root: &Path, found: &Path) -> Origin {
        let go_on = || !self.kept_quiet(root) && !self.kept_quiet(found);
        // Asked before its first step too (whether the other job's folder is there).
        if !go_on() {
            return Origin::Unknown;
        }
        Marker::origin(whose, root, found, &go_on)
    }

    /// What the list says about a folder holding a copy of the hidden `.charon` folder of
    /// `whose` folder.
    fn copy_note(folder: &Path, whose: &str) -> String {
        format!(
            "{} holds a copy of the hidden .charon folder of {whose}, probably copied along with the files. It does no harm, and you can delete it.",
            folder.display()
        )
    }

    /// Why the job's two folders can't be synced, if one is inside the other (or they are
    /// one folder), whatever their paths say. `go_on` is asked before each folder.
    pub(crate) fn nested(setup: &JobSetup, go_on: &dyn Fn() -> Result<()>) -> Result<Option<String>> {
        go_on()?;
        let source = Place::of(&setup.source);
        go_on()?;
        let destination = Place::of(&setup.destination);
        Ok(match (source.is_within(&destination), destination.is_within(&source)) {
            (true, true) => Some(
                "The source and the destination are the same folder, reached under two names (through a link, or a drive mounted in two places). Choose other folders."
                    .into(),
            ),
            (true, false) | (false, true) => Some(
                "One of this job's folders is inside the other, though their paths don't show it (it's reached through a link, or a drive mounted in two places). \
                 Syncing would copy the job's own copies into themselves. Choose other folders."
                    .into(),
            ),
            (false, false) => None,
        })
    }

    /// The markers in this job's folders (met by the scans), at their top, and in the
    /// folders above them: other jobs', and this job's own. `go_on` is asked before each
    /// folder's look above it.
    fn markers_around(job: &Job, source: &Snapshot, destination: &Snapshot, go_on: &dyn Fn() -> Result<()>) -> Result<Vec<MarkerAround>> {
        let mut found = Vec::new();
        for (side, snapshot) in [(Side::Source, source), (Side::Destination, destination)] {
            let root = job.setup.root(side);
            for seen in snapshot.charon_folders() {
                let folder = if seen.folder.is_empty() { root.to_path_buf() } else { root.join(&seen.folder) };
                for other in &seen.jobs {
                    found.push(MarkerAround { side, folder: folder.clone(), job: other.clone() });
                }
            }
            go_on()?;
            for (folder, other) in Self::markers_above(root) {
                found.push(MarkerAround { side, folder, job: other });
            }
        }
        Ok(found)
    }

    /// The markers in the folders above `root`, with the folder each is in.
    fn markers_above(root: &Path) -> Vec<(PathBuf, JobId)> {
        let Ok(real) = std::fs::canonicalize(root) else { return Vec::new() };
        real.ancestors()
            .skip(1)
            .flat_map(|folder| Marker::jobs_in(folder).into_iter().map(move |job| (folder.to_path_buf(), job)))
            .collect()
    }

    /// Whether `marker` is the one `other` left in its current folders at its last finished
    /// sync. A marker left in folders the job used before carries another token (a copy of
    /// the current one doesn't: see `Marker::origin`).
    fn left_by(&self, other: &Job, marker: &Marker) -> bool {
        let expected = if other.setup.mode.is_two_way() {
            self.inner.baselines.tokens(&other.id, &other.setup).map(|(source, destination)| match marker.side {
                Side::Source => source,
                Side::Destination => destination,
            })
        } else {
            match marker.side {
                Side::Destination => other.destination_token.clone(),
                Side::Source => other.source_mark.as_ref().map(|mark| mark.token.clone()),
            }
        };
        marker.job == other.id && expected.is_some_and(|token| token == marker.token)
    }

    /// Whether two jobs, whose folders `side` (this one's) and `their_side` (the other's)
    /// are one place or one inside the other, would undo each other's work: one is a Mirror
    /// job removing in its destination what the other writes there (see
    /// `JobSetup::clashes_with`).
    fn undoes(mine: &JobSetup, side: Side, theirs: &JobSetup, their_side: Side) -> bool {
        let removes = |setup: &JobSetup, side: Side| setup.mode == SyncMode::Mirror && side == Side::Destination;
        (removes(mine, side) && theirs.writes_to(their_side)) || (removes(theirs, their_side) && mine.writes_to(side))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConflictRule, Trigger};
    use std::fs;
    use std::sync::Arc;

    fn setup(mode: SyncMode, source: &Path, destination: &Path) -> JobSetup {
        JobSetup {
            name: "t".into(),
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            mode,
            conflict: ConflictRule::Ask,
            trigger: Trigger::Manual,
            excludes: vec![],
            safe_delete: true,
            full_automation: false,
        }
    }

    fn engine(tmp: &Path) -> Engine {
        Engine::open(tmp.join("data"), Arc::new(|_| {})).unwrap()
    }

    /// Adds a Mirror or Backup job named `name` as it is after a sync: its marker in its
    /// destination, with the token the job remembers.
    fn synced(engine: &Engine, name: &str, mode: SyncMode, source: &Path, destination: &Path) -> Job {
        fs::create_dir_all(source).unwrap();
        fs::create_dir_all(destination).unwrap();
        let setup = JobSetup { name: name.into(), ..setup(mode, source, destination) };
        let id = engine.add_job(setup).unwrap().job.id;
        engine.change_job(&id, |job| job.destination_token = Some("token".into())).unwrap();
        Marker::new(&id, Side::Destination, "token", 1).write(destination).unwrap();
        engine.stored_job(&id).unwrap()
    }

    /// Copies the hidden Charon folder of `from` into `to`, as copying the folder would.
    fn copy_charon_folder(from: &Path, to: &Path) {
        let (from, to) = (from.join(crate::scan::MARKER_FOLDER), to.join(crate::scan::MARKER_FOLDER));
        fs::create_dir_all(&to).unwrap();
        for item in fs::read_dir(from).unwrap().flatten() {
            fs::copy(item.path(), to.join(item.file_name())).unwrap();
        }
    }

    /// Moves the folder `from` to `to`. Windows refuses to while anything holds a file inside
    /// open, as a virus scanner may for a moment after files were written, so it is tried
    /// again for a few seconds.
    fn move_folder(from: &Path, to: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match fs::rename(from, to) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("{} can't be moved: {e}", from.display()),
            }
        }
    }

    fn found(side: Side, folder: &Path, job: &Job) -> Vec<MarkerAround> {
        vec![MarkerAround { side, folder: folder.to_path_buf(), job: job.id.clone() }]
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_holding_the_other_under_another_name_is_refused_by_its_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let (nas, again) = (tmp.path().join("NAS"), tmp.path().join("NAS-1"));
        let job = synced(&engine, "Docs", SyncMode::Mirror, &nas.join("Docs"), &again.join("Docs/Backup"));
        // NAS-1 turns out to be the share mounted a second time. A link plays it here: the
        // paths would show a link, but not a second mount, which only the markers show.
        fs::remove_dir_all(&again).unwrap();
        std::os::unix::fs::symlink(&nas, &again).unwrap();
        fs::create_dir_all(nas.join("Docs/Backup")).unwrap();
        Marker::new(&job.id, Side::Destination, "token", 1).write(&again.join("Docs/Backup")).unwrap();
        let mut copies = BTreeSet::new();
        let refusal = engine.nested_by_markers(&job, &found(Side::Source, &nas.join("Docs/Backup"), &job), &mut copies, &|| Ok(())).unwrap();
        assert!(refusal.is_some_and(|text| text.contains("under names that don't show it")));
        assert!(copies.is_empty());
    }

    #[test]
    fn a_copy_of_the_other_folders_marker_is_only_mentioned() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let (documents, usb) = (tmp.path().join("Documents"), tmp.path().join("USB/Documents"));
        let job = synced(&engine, "Backup", SyncMode::Backup, &documents, &usb);
        // The person restores the backup by copying it into the source, hidden folder and all.
        let restored = documents.join("From the USB backup");
        copy_charon_folder(&usb, &restored);
        let mut copies = BTreeSet::new();
        assert_eq!(engine.nested_by_markers(&job, &found(Side::Source, &restored, &job), &mut copies, &|| Ok(())).unwrap(), None);
        let note = copies.into_iter().next().expect("the copy is mentioned");
        assert!(note.contains("From the USB backup") && note.contains("this job's destination folder"), "{note}");
    }

    #[cfg(unix)]
    #[test]
    fn a_job_writing_into_a_mirrors_folder_under_another_name_is_found_by_the_mirrors_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let (nas, again) = (tmp.path().join("NAS"), tmp.path().join("NAS-1"));
        let mirror = synced(&engine, "Photos", SyncMode::Mirror, &tmp.path().join("photos"), &nas.join("Photos"));
        let inbox = synced(&engine, "Inbox", SyncMode::Backup, &tmp.path().join("scans"), &again.join("Photos/Inbox"));
        fs::remove_dir_all(&again).unwrap();
        std::os::unix::fs::symlink(&nas, &again).unwrap();
        let mut copies = BTreeSet::new();
        let clashes = engine.clashes_by_markers(&inbox, &[&mirror], &found(Side::Destination, &again.join("Photos"), &mirror), &mut copies, &mut Waiting::default(), &|| Ok(())).unwrap();
        assert_eq!(clashes.len(), 1);
        assert!(clashes[0].contains("“Photos”") && clashes[0].contains("are one place"), "{}", clashes[0]);
        assert!(copies.is_empty());
    }

    #[test]
    fn a_copy_of_a_mirrors_marker_raises_no_clash_and_nothing_while_the_mirrors_drive_is_away() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let usb = tmp.path().join("USB");
        let mirror = synced(&engine, "Photos", SyncMode::Mirror, &tmp.path().join("photos"), &usb.join("Photos"));
        let docs = synced(&engine, "Docs", SyncMode::TwoWay, &tmp.path().join("Docs"), &tmp.path().join("NAS/Docs"));
        let old_photos = tmp.path().join("Docs/Old photos");
        copy_charon_folder(&usb.join("Photos"), &old_photos);
        let around = found(Side::Source, &old_photos, &mirror);
        let mut copies = BTreeSet::new();
        assert!(engine.clashes_by_markers(&docs, &[&mirror], &around, &mut copies, &mut Waiting::default(), &|| Ok(())).unwrap().is_empty());
        let note = copies.iter().next().expect("the copy is mentioned");
        assert!(note.contains("Old photos") && note.contains("“Photos”"), "{note}");
        // The drive is unplugged: the Mirror can't sync, and nothing is said.
        move_folder(&usb, &tmp.path().join("unplugged"));
        let mut copies = BTreeSet::new();
        assert!(engine.clashes_by_markers(&docs, &[&mirror], &around, &mut copies, &mut Waiting::default(), &|| Ok(())).unwrap().is_empty());
        assert!(copies.is_empty());
    }

    /// A look stopped while it checks the folders ends before its next step on either
    /// folder, or on another job's: it may have waited for a share while a sync was given the
    /// other's drive (see `quiet.rs`).
    #[test]
    fn a_look_stopped_while_it_checks_the_folders_ends_before_its_next_step() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let docs = synced(&engine, "Docs", SyncMode::Mirror, &tmp.path().join("Docs"), &tmp.path().join("USB/Docs"));
        synced(&engine, "Photos", SyncMode::Mirror, &tmp.path().join("Photos"), &tmp.path().join("USB/Photos"));
        // Copies of both jobs' markers in the folder above all of theirs: each is read, and
        // asked whether it's a copy.
        copy_charon_folder(&tmp.path().join("USB/Docs"), tmp.path());
        copy_charon_folder(&tmp.path().join("USB/Photos"), tmp.path());
        let scans = (Snapshot::default(), Snapshot::default());
        // Above each folder, and each folder itself. The job's own marker above its source:
        // read, asked about (both markers, then the probe left, looked for in the other
        // folder and taken away), and the answer. The same marker above its destination,
        // read. The other job, by its paths; its marker above this job's source, read (the
        // two jobs can't undo each other's work there); and above this job's destination,
        // read and asked about.
        let steps = 2 + 2 + (1 + 1 + 5 + 1) + 1 + 1 + 1 + (1 + 1);
        for stop_at in 1..=steps + 1 {
            let asked = std::cell::Cell::new(0);
            // Once stopped, it stays stopped.
            let go_on = || {
                asked.set(asked.get() + 1);
                if asked.get() >= stop_at {
                    Err(crate::error::Error::Cancelled)
                } else {
                    Ok(())
                }
            };
            let looked = engine.overlaps(&docs, &scans.0, &scans.1, &go_on);
            if stop_at <= steps {
                assert!(matches!(looked, Err(crate::error::Error::Cancelled)), "stopped at step {stop_at}");
                // Stopped while it asks whether the marker is a copy, the question ends at
                // once (only, once it left the probe, it asks whether to take it away), and
                // the look at the step after it.
                let ended = match stop_at {
                    7..=9 => stop_at + 1,
                    10 | 11 => 12,
                    _ => stop_at,
                };
                assert_eq!(asked.get(), ended, "stopped at step {stop_at}");
            } else {
                let overlaps = looked.unwrap();
                assert!(overlaps.refusal.is_none() && overlaps.clashes.is_empty(), "{overlaps:?}");
                assert_eq!(overlaps.copies.len(), 2, "{overlaps:?}");
                assert_eq!(asked.get(), steps);
            }
        }
    }

    #[test]
    fn a_mirror_and_a_writer_on_one_place_undo_each_other() {
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let mirror = setup(SyncMode::Mirror, a, b);
        let two_way = setup(SyncMode::TwoWay, a, b);
        let backup = setup(SyncMode::Backup, a, b);
        assert!(Engine::undoes(&mirror, Side::Destination, &two_way, Side::Source));
        assert!(Engine::undoes(&two_way, Side::Destination, &mirror, Side::Destination));
        assert!(Engine::undoes(&mirror, Side::Destination, &backup, Side::Destination));
        // A Mirror's destination read by a Backup, and a Mirror's source, are fine.
        assert!(!Engine::undoes(&mirror, Side::Destination, &backup, Side::Source));
        assert!(!Engine::undoes(&mirror, Side::Source, &two_way, Side::Destination));
        assert!(!Engine::undoes(&two_way, Side::Source, &backup, Side::Destination));
    }

    #[cfg(unix)]
    #[test]
    fn folders_nested_through_a_link_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dropbox = tmp.path().join("CloudStorage/Dropbox");
        std::fs::create_dir_all(dropbox.join("Backup")).unwrap();
        std::os::unix::fs::symlink(&dropbox, tmp.path().join("Dropbox")).unwrap();
        let nested = setup(SyncMode::Mirror, &dropbox, &tmp.path().join("Dropbox/Backup"));
        assert!(Engine::nested(&nested, &|| Ok(())).unwrap().is_some_and(|text| text.contains("inside the other")));
        let same = setup(SyncMode::TwoWay, &dropbox, &tmp.path().join("Dropbox"));
        assert!(Engine::nested(&same, &|| Ok(())).unwrap().is_some_and(|text| text.contains("same folder")));
        std::fs::create_dir_all(tmp.path().join("Elsewhere")).unwrap();
        assert!(Engine::nested(&setup(SyncMode::Mirror, &dropbox, &tmp.path().join("Elsewhere")), &|| Ok(())).unwrap().is_none());
    }
}
