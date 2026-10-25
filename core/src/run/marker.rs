//! The marker Charon leaves in a job's folders: `.charon/<job>.json` at the top of the
//! folder, with a random token and a counter that goes up with every finished sync. Both
//! sides of a two-way job carry one; a Mirror or Backup leaves one in the destination, and
//! in the source too where it can write there.
//!
//! The two-way memory only means something for the very folders it was made with. A path
//! can't tell: a replaced drive formatted with the same name, two backup drives used in
//! turns, a drive restored from last week's copy, or the empty folder an unplugged drive
//! leaves behind all sit under the same path. The marker can tell:
//!
//! * no marker, or another token: not the folder Charon synced with;
//! * an older counter: the folder went back in time (restored from a copy), or the last
//!   sync's changes never reached the drive (pulled out without ejecting).
//!
//! In each of those cases a two-way job doesn't trust its memory: it compares both sides
//! afresh, which never removes anything, and asks the person to look first. A Mirror or
//! Backup waits for the person's look too. And a marker that changed between the list the
//! person confirmed and the end of the sync means another drive came in between: nothing
//! about that sync is recorded.

use crate::model::JobId;
use crate::plan::Side;
use crate::scan::MARKER_FOLDER;
use crate::store::atomic;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What the name of the probe file ends with that tells whether two folders are one (see
/// `Marker::origin`). It's taken away again at once, unless the question was stopped
/// meanwhile: its look was stopped, or a sync began to keep the drive of either folder quiet
/// (see `Marker::same_place`).
pub const PROBE_EXTENSION: &str = ".probe";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub job: JobId,
    pub side: Side,
    pub token: String,
    pub generation: u64,
}

/// What a side's marker says, as far as Charon can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Marker(Marker),
    Missing,
    /// There, but unreadable or damaged.
    Unusable(String),
}

/// Where a job's marker found in a folder other than the job's own comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The folder is the job's own folder, reached under another name (a drive or share
    /// mounted twice, a link).
    SameFolder,
    /// The folder holds a copy of the job's marker: the person copied the job's folder,
    /// hidden `.charon` folder and all, or the copy is older than the marker there now.
    Copy,
    /// The job's own folder can't be reached now.
    Away,
    /// Charon can't tell (neither Charon folder takes a file, or the job's own marker can't
    /// be read).
    Unknown,
}

/// A job's markers on both sides, as found at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Markers {
    pub source: Found,
    pub destination: Found,
}

impl Markers {
    /// Reads the markers of `job` at the top of both folders.
    pub fn read(source: &Path, destination: &Path, job: &JobId) -> Self {
        Self { source: Marker::read(source, job), destination: Marker::read(destination, job) }
    }

    pub fn side(&self, side: Side) -> &Found {
        match side {
            Side::Source => &self.source,
            Side::Destination => &self.destination,
        }
    }

    /// The first side whose marker isn't what it was in `before`: something replaced the
    /// folder (another drive under the same name) or wrote to it in between.
    pub fn changed_since(&self, before: &Markers) -> Option<Side> {
        [Side::Source, Side::Destination].into_iter().find(|&side| self.side(side) != before.side(side))
    }
}

impl Marker {
    pub fn new(job: &JobId, side: Side, token: &str, generation: u64) -> Self {
        Self { job: job.clone(), side, token: token.to_string(), generation }
    }

    /// A new random token for a pair of folders.
    pub fn fresh_token() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// The marker of `job` at the top of `root`.
    pub fn read(root: &Path, job: &JobId) -> Found {
        let path = Self::path(root, job);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Found::Unusable(format!("{} isn't a file", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Found::Missing,
            Err(e) => return Found::Unusable(e.to_string()),
        }
        match fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).map_err(|e| e.to_string())) {
            Ok(marker) => Found::Marker(marker),
            Err(e) => Found::Unusable(e),
        }
    }

    /// The jobs whose markers are at the top of `root`: the jobs whose folder `root` is.
    pub fn jobs_in(root: &Path) -> Vec<JobId> {
        let Ok(items) = fs::read_dir(root.join(MARKER_FOLDER)) else { return Vec::new() };
        items
            .flatten()
            .filter_map(|item| item.file_name().to_str().and_then(|name| name.strip_suffix(".json")).map(str::to_string))
            .collect()
    }

    /// Whether `name` is the name of a file Charon keeps in its own folder: a job's marker
    /// (`<job>.json`), a probe (`<id>.probe`), a note that a sync is at work (any `.busy`
    /// file, as each one is heeded, see `BusyWatch`), or one of them while it's written (see
    /// `atomic::write`). Markers and probes are named after an id of 32 hex digits, so a
    /// folder of the person's own that's only named like Charon's (holding `settings.json`)
    /// isn't taken for Charon's.
    pub fn is_charons_file(name: &str) -> bool {
        let name = atomic::written_for(name).unwrap_or(name);
        name.ends_with(crate::run::BUSY_EXTENSION) || [".json", PROBE_EXTENSION].iter().any(|extension| name.strip_suffix(extension).is_some_and(Self::is_id))
    }

    /// Whether `id` is an id as Charon makes them, for a job or a probe: 32 hex digits.
    fn is_id(id: &str) -> bool {
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Writes the marker for good (on the drive, not only in its memory).
    pub fn write(&self, root: &Path) -> crate::Result<()> {
        let folder = Self::real_folder(root)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic::write(&folder.join(format!("{}.json", self.job)), &bytes)
    }

    /// Removes the marker of `job` (the job is gone), and the marker folder if nothing else is
    /// in it but probes a look left there (see `same_place`): a probe is only ever left in a
    /// folder that holds the job's marker, so nobody else is looking for one there.
    pub fn forget(root: &Path, job: &JobId) {
        let path = Self::path(root, job);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            let _ = fs::remove_file(&path);
        }
        let folder = root.join(MARKER_FOLDER);
        if fs::symlink_metadata(&folder).is_ok_and(|m| m.is_dir()) {
            let names: Vec<String> = fs::read_dir(&folder).map(|items| items.flatten().map(|item| item.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
            if !names.is_empty() && names.iter().all(|name| name.strip_suffix(PROBE_EXTENSION).is_some_and(Self::is_id)) {
                for name in &names {
                    let _ = fs::remove_file(folder.join(name));
                }
            }
            let _ = fs::remove_dir(&folder); // only when empty
        }
    }

    fn path(root: &Path, job: &JobId) -> PathBuf {
        root.join(MARKER_FOLDER).join(format!("{job}.json"))
    }

    /// Where the marker of `job` found in the Charon folder of `found` comes from, when the
    /// job's own folder is `root` (see `Origin`). The two markers are read again, side by
    /// side: two different ones are two folders. Two identical ones are one folder, or a
    /// copy made since the job's last sync, which nothing in them tells apart, so Charon
    /// leaves a probe file in one of the two Charon folders, and looks for it in the other.
    /// `go_on` is asked before each step after the first. When it says no before a marker is
    /// read or before Charon looks in the other folder, Charon can't tell (`Unknown`); a no
    /// only before the probe is taken away leaves the probe there, and the answer holds.
    pub fn origin(job: &JobId, root: &Path, found: &Path, go_on: &dyn Fn() -> bool) -> Origin {
        if !root.is_dir() {
            return Origin::Away;
        }
        let own = if go_on() { Self::read(root, job) } else { return Origin::Unknown };
        let seen = if go_on() { Self::read(found, job) } else { return Origin::Unknown };
        match (own, seen) {
            (Found::Marker(own), Found::Marker(seen)) if own == seen => match Self::same_place(root, found, go_on) {
                Some(true) => Origin::SameFolder,
                Some(false) => Origin::Copy,
                None => Origin::Unknown,
            },
            (Found::Unusable(_), _) | (_, Found::Unusable(_)) => Origin::Unknown,
            _ => Origin::Copy,
        }
    }

    /// Whether the folders `a` and `b`, which both hold a Charon folder, are one folder under
    /// two names: a probe file left in the Charon folder of one shows up in that of the
    /// other (a new name is looked up on the drive itself, not in what the computer
    /// remembers of the folder). `None` when neither Charon folder takes a file, or `go_on`
    /// says no before Charon looks in the other one. The probe is taken away again while
    /// `go_on` still says so; otherwise it's left (it does no harm, every probe has a name of
    /// its own, and `forget` takes it away with the marker). Never makes a folder.
    fn same_place(a: &Path, b: &Path, go_on: &dyn Fn() -> bool) -> Option<bool> {
        let name = format!("{}{PROBE_EXTENSION}", uuid::Uuid::new_v4().simple());
        for (here, there) in [(a, b), (b, a)] {
            if !go_on() {
                return None;
            }
            let folder = here.join(MARKER_FOLDER);
            if !fs::symlink_metadata(&folder).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            let probe = folder.join(&name);
            if fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_err() {
                continue;
            }
            let seen = go_on().then(|| fs::symlink_metadata(there.join(MARKER_FOLDER).join(&name)).is_ok());
            if go_on() {
                let _ = fs::remove_file(&probe);
            }
            return seen;
        }
        None
    }

    /// `root/.charon`, created when missing; never a link.
    pub(crate) fn real_folder(root: &Path) -> crate::Result<PathBuf> {
        let folder = root.join(MARKER_FOLDER);
        match fs::symlink_metadata(&folder) {
            Ok(meta) if meta.is_dir() => Ok(folder),
            Ok(_) => Err(crate::Error::Invalid(format!("{} isn't a real folder (a link or a file), so Charon won't write into it", folder.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&folder) {
                Ok(()) => {
                    hide(&folder);
                    Ok(folder)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::real_folder(root),
                Err(e) => Err(crate::Error::file(&folder, e)),
            },
            Err(e) => Err(crate::Error::file(&folder, e)),
        }
    }
}

/// Hides one of Charon's own folders on Windows (macOS and Linux hide names starting with a dot).
pub(crate) fn hide(path: &Path) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES};
        let wide = crate::run::rename::wide(path);
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that lives through both calls.
        unsafe {
            let attributes = GetFileAttributesW(wide.as_ptr());
            if attributes != INVALID_FILE_ATTRIBUTES {
                SetFileAttributesW(wide.as_ptr(), attributes | FILE_ATTRIBUTE_HIDDEN);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_markers_read_back_and_go_away() {
        let tmp = tempfile::tempdir().unwrap();
        let job = "job1".to_string();
        assert_eq!(Marker::read(tmp.path(), &job), Found::Missing);
        let marker = Marker::new(&job, Side::Destination, "token", 3);
        marker.write(tmp.path()).unwrap();
        assert_eq!(Marker::read(tmp.path(), &job), Found::Marker(marker));
        Marker::forget(tmp.path(), &job);
        assert_eq!(Marker::read(tmp.path(), &job), Found::Missing);
        assert!(!tmp.path().join(MARKER_FOLDER).exists());
    }

    #[test]
    fn the_jobs_of_a_folder_are_read_from_their_markers() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Marker::jobs_in(tmp.path()).is_empty());
        Marker::new(&"job1".to_string(), Side::Source, "a", 1).write(tmp.path()).unwrap();
        Marker::new(&"job2".to_string(), Side::Destination, "b", 0).write(tmp.path()).unwrap();
        let mut jobs = Marker::jobs_in(tmp.path());
        jobs.sort();
        assert_eq!(jobs, vec!["job1".to_string(), "job2".to_string()]);
    }

    #[test]
    fn charons_own_files_are_told_by_their_names() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let busy = crate::run::BUSY_EXTENSION;
        for name in [format!("{id}.json"), format!("{id}{busy}"), format!("another-charon{busy}"), format!("{id}{PROBE_EXTENSION}"), format!(".{id}.json.4711-3.tmp"), format!(".{id}{busy}.4711-3.tmp")] {
            assert!(Marker::is_charons_file(&name), "{name}");
        }
        for name in ["settings.json", "cache.tmp", ".settings.json.4711-3.tmp", "notes.probe", &format!("{id}.toml"), &format!("x{id}.json")] {
            assert!(!Marker::is_charons_file(name), "{name}");
        }
    }

    #[test]
    fn a_marker_that_changed_names_its_side() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let job = "job1".to_string();
        let before = Markers::read(a.path(), b.path(), &job);
        assert_eq!(Markers::read(a.path(), b.path(), &job).changed_since(&before), None);
        Marker::new(&job, Side::Destination, "token", 1).write(b.path()).unwrap();
        assert_eq!(Markers::read(a.path(), b.path(), &job).changed_since(&before), Some(Side::Destination));
    }

    #[test]
    fn a_damaged_marker_is_unusable_not_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(MARKER_FOLDER)).unwrap();
        fs::write(tmp.path().join(MARKER_FOLDER).join("j.json"), "{nope").unwrap();
        assert!(matches!(Marker::read(tmp.path(), &"j".to_string()), Found::Unusable(_)));
    }

    /// Whether the files at `paths` are all files Charon keeps in its own folder.
    fn left_names_are_charons(paths: &[PathBuf]) -> bool {
        !paths.is_empty() && paths.iter().all(|path| path.file_name().is_some_and(|name| Marker::is_charons_file(&name.to_string_lossy())))
    }

    #[test]
    fn a_copy_of_a_marker_is_told_from_the_folder_it_was_copied_from() {
        let tmp = tempfile::tempdir().unwrap();
        let (backup, restored) = (tmp.path().join("USB/Documents"), tmp.path().join("Documents/From the USB backup"));
        fs::create_dir_all(&backup).unwrap();
        fs::create_dir_all(&restored).unwrap();
        let job = "job".to_string();
        let marker = Marker::new(&job, Side::Destination, "token", 4);
        marker.write(&backup).unwrap();
        // Copied along with the files: the same marker, in another folder.
        marker.write(&restored).unwrap();
        assert_eq!(Marker::origin(&job, &backup, &restored, &|| true), Origin::Copy);
        assert_eq!(fs::read_dir(backup.join(MARKER_FOLDER)).unwrap().count(), 1, "no probe is left behind");
        assert_eq!(fs::read_dir(restored.join(MARKER_FOLDER)).unwrap().count(), 1);
        // Told to stop at any step, it ends there. Once it left a probe, the probe stays (it
        // does no harm), and the job's marker takes it along when it goes.
        let probes = || -> Vec<PathBuf> {
            let items = fs::read_dir(backup.join(MARKER_FOLDER)).unwrap().flatten();
            items.map(|item| item.path()).filter(|path| path.to_string_lossy().ends_with(PROBE_EXTENSION)).collect()
        };
        for stop_at in 1..=5 {
            let asked = std::cell::Cell::new(0);
            let go_on = || {
                asked.set(asked.get() + 1);
                asked.get() < stop_at
            };
            let origin = Marker::origin(&job, &backup, &restored, &go_on);
            assert_eq!(origin, if stop_at < 5 { Origin::Unknown } else { Origin::Copy }, "stopped at step {stop_at}");
            // Before each marker, the probe, the look in the other folder, and taking it away.
            assert_eq!(asked.get(), if stop_at < 4 { stop_at } else { 5 });
            let left = probes();
            assert_eq!(left.len(), usize::from(stop_at >= 4), "stopped at step {stop_at}");
            if stop_at == 4 {
                fs::remove_file(&left[0]).unwrap();
            }
        }
        assert!(left_names_are_charons(&probes()));
        assert_eq!(Marker::origin(&job, &backup, &restored, &|| true), Origin::Copy, "what's left doesn't change the answer");
        Marker::forget(&backup, &job);
        assert!(!backup.join(MARKER_FOLDER).exists(), "the probes went with the marker");
        marker.write(&backup).unwrap();
        // An older copy differs from the marker there now.
        Marker::new(&job, Side::Destination, "token", 5).write(&backup).unwrap();
        assert_eq!(Marker::origin(&job, &backup, &restored, &|| true), Origin::Copy);
        // The drive is away.
        assert_eq!(Marker::origin(&job, &tmp.path().join("Elsewhere/Documents"), &restored, &|| true), Origin::Away);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_reached_under_another_name_is_the_same_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join("NAS/Docs");
        fs::create_dir_all(&share).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("NAS"), tmp.path().join("NAS-1")).unwrap();
        let job = "job".to_string();
        Marker::new(&job, Side::Destination, "token", 4).write(&share).unwrap();
        assert_eq!(Marker::origin(&job, &share, &tmp.path().join("NAS-1/Docs"), &|| true), Origin::SameFolder);
        assert_eq!(fs::read_dir(share.join(MARKER_FOLDER)).unwrap().count(), 1, "no probe is left behind");
    }

    #[cfg(unix)]
    #[test]
    fn a_marker_charon_cannot_probe_leaves_it_unsure() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
        let job = "job".to_string();
        for folder in [&one, &two] {
            fs::create_dir_all(folder).unwrap();
            Marker::new(&job, Side::Source, "token", 1).write(folder).unwrap();
            fs::set_permissions(folder.join(MARKER_FOLDER), fs::Permissions::from_mode(0o555)).unwrap();
        }
        // Root writes anyway: then there's nothing to show.
        let writable = fs::write(one.join(MARKER_FOLDER).join("x"), "x").is_ok();
        let origin = Marker::origin(&job, &one, &two, &|| true);
        for folder in [&one, &two] {
            fs::set_permissions(folder.join(MARKER_FOLDER), fs::Permissions::from_mode(0o755)).unwrap();
        }
        if writable {
            return;
        }
        assert_eq!(origin, Origin::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_marker_folder_is_never_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        let root = tmp.path().join("root");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(MARKER_FOLDER)).unwrap();
        assert!(Marker::new(&"j".to_string(), Side::Source, "t", 1).write(&root).is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }
}
