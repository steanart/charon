//! Copies one file (or link) without ever leaving a half-written file under the real name.
//!
//! The data goes to a new file with a short temporary name next to the target
//! (`.charon-<16 hex digits>.part`), never opened through a link. The source is checked
//! before and after reading: if it changed while it was being copied, the copy is thrown
//! away (the next look picks the file up again). Only a complete copy, with the original's
//! modification time, permissions and chosen extended attributes, takes the real name
//! (`commit_new` here, or `Executor::replace` when it replaces an older version).
//!
//! A target with a short name deep down has a temporary name longer than its own, whose
//! path may be too long for the drive where the target's fits. Then the temporary name goes
//! into the nearest folder above where it fits (see `Copier::temporary`): only the target
//! itself must fit.

use crate::error::{Error, Result};
use crate::run::durable;
use crate::run::rename::rename_new;
use crate::scan::{new_partial_name, Contents, Entry, Refusal, Scanner};
use std::collections::BTreeSet;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const CHUNK: usize = 1024 * 1024;
/// Why a file the cloud took back since the scan was left alone.
const CLOUD_SINCE_SCAN: &str = "only in the cloud since the scan (not downloaded to this computer), so Charon left it alone";
/// On a drive that keeps times in steps of a second or more, a file saved this recently
/// may still be being written within the same step, where its time can't show it.
const STILL_BEING_WRITTEN_MS: i64 = 3_000;
/// A network drive's clock may be a few minutes ahead of the computer's, so a file saved
/// there a moment ago can carry a time up to this far ahead. A time further ahead is no save
/// going on: a camera with its clock set wrong, or a FAT stick written in a time zone further
/// east.
const CLOCK_AHEAD_MS: i64 = 5 * 60 * 1000;

/// A finished copy under its temporary name, waiting to take the real one.
#[derive(Debug)]
pub struct Written {
    pub partial: PathBuf,
    /// The details of the original the copy couldn't take along ("Finder tags", "resource
    /// fork", "extended attributes"), because the target drive can't keep them.
    pub unkept: BTreeSet<&'static str>,
}

pub struct Copier<'a> {
    cancel: &'a AtomicBool,
    /// Whether the target drive keeps extended attributes (FAT and exFAT only fake them).
    attributes: bool,
    /// The source's drive keeps times in steps of a second or more (see `Volume::coarse_times`).
    coarse_source: bool,
    /// The highest folder a temporary copy may go in when the path next to its target is too
    /// long (see `temporary`): the job's folder on the target's side.
    top: Option<&'a Path>,
}

impl<'a> Copier<'a> {
    pub fn new(cancel: &'a AtomicBool) -> Self {
        Self { cancel, attributes: true, coarse_source: false, top: None }
    }

    /// Lets temporary copies go up to the folder `top` where the path next to the target is
    /// too long for the drive. Without it, they only ever go next to the target.
    pub fn within(mut self, top: &'a Path) -> Self {
        self.top = Some(top);
        self
    }

    /// Leaves extended attributes out (the target drive can't really hold them).
    pub fn without_attributes(mut self) -> Self {
        self.attributes = false;
        self
    }

    /// The source's drive keeps coarse times: a file saved in the last moments is left for
    /// the next sync rather than copied while it may still be written.
    pub fn coarse_source_times(mut self, coarse: bool) -> Self {
        self.coarse_source = coarse;
        self
    }

    /// Copies the file `from` (which must still be `expected`) to a new temporary file for
    /// `to` (see `temporary`), reporting bytes as they go. Returns the temporary file, ready
    /// for a commit. Fails with `Error::Refused` when the file is too big for the drive.
    pub fn file(&self, from: &Path, to: &Path, expected: &Entry, on_bytes: &mut dyn FnMut(u64)) -> Result<Written> {
        let top = self.top.or(to.parent()).unwrap_or(to);
        let (partial, writer) = Self::create_partial(to, top)?;
        match self.write_partial(from, &partial, writer, expected, on_bytes) {
            Ok(unkept) => Ok(Written { partial, unkept }),
            Err(e) => {
                Self::discard(&partial);
                match e {
                    Error::File { path, source } if path == partial => match Refusal::of_writing(&source) {
                        Some(why) => Err(Error::Refused { path: to.to_path_buf(), why }),
                        None => Err(Error::File { path, source }),
                    },
                    e => Err(e),
                }
            }
        }
    }

    /// Makes a link like the one at `from` (which must still be `expected`) under a
    /// temporary name for `to` (see `temporary`, with `top` as the highest folder it may go
    /// in). macOS and Linux only; Windows plans never hold links. Fails with
    /// `Error::NoLinks` only when the drive says it can't hold links at all; an answer that
    /// may only be about this folder fails with what the drive said (see `may_refuse_links`).
    pub fn link(from: &Path, to: &Path, expected: &Entry, top: &Path) -> Result<PathBuf> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = fs::read_link(from).map_err(|e| Error::file(from, e))?;
            let meta = fs::symlink_metadata(from).map_err(|e| Error::file(from, e))?;
            let now = Entry::link(target.as_os_str().as_bytes(), Scanner::modified_ms(&meta));
            if !now.unchanged(expected) {
                return Err(Error::Changed("changed since the scan".into()));
            }
            match Self::temporary(to, top, |partial| std::os::unix::fs::symlink(&target, partial)) {
                Ok((partial, ())) => {
                    let times = filetime::FileTime::from_last_modification_time(&meta);
                    let _ = filetime::set_symlink_file_times(&partial, times, times);
                    Ok(partial)
                }
                Err((_, e)) if Self::refuses_links(&e) => Err(Error::NoLinks),
                Err((partial, e)) => Err(Error::file(partial, e)),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (from, to, expected, top);
            Err(Error::NoLinks)
        }
    }

    /// Whether the error from creating a link says the drive can't hold links at all.
    #[cfg(unix)]
    fn refuses_links(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP || code == libc::ENOSYS)
    }

    /// Whether the error from creating a link may mean the drive can't hold links, though it
    /// may just as well be about the one folder the link was to go in: "operation not
    /// permitted" (drives without links answer that, and so does a locked folder), and on
    /// a drive that answers vaguely (`vague`, see `Volume::answers_links_vaguely`) also
    /// "permission denied" or "invalid argument". Only asking the drive at the top of the
    /// job's folder tells which (see `drive_holds_links`).
    pub fn may_refuse_links(error: &Error, vague: bool) -> bool {
        matches!(error, Error::File { source, .. } if Self::may_mean_no_links(source, vague))
    }

    /// `may_refuse_links`, for the drive's own answer.
    fn may_mean_no_links(error: &io::Error, vague: bool) -> bool {
        #[cfg(unix)]
        {
            match error.raw_os_error() {
                Some(code) if code == libc::EPERM => true,
                Some(code) if code == libc::EACCES || code == libc::EINVAL => vague,
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (error, vague);
            false
        }
    }

    /// Asks the drive of `folder` (the top of a job's folder) whether it holds links: makes
    /// one there under a temporary name, and removes it at once. `Some(false)` only when the
    /// drive refuses the link where it takes a file; `None` when it can't tell (the folder
    /// takes neither, or the drive failed otherwise). A probe an interruption leaves behind
    /// is tidied away like any half-copied file.
    pub fn drive_holds_links(folder: &Path) -> Option<bool> {
        #[cfg(unix)]
        {
            let probe = Self::partial_in(folder);
            match std::os::unix::fs::symlink("charon-link-probe", &probe) {
                Ok(()) => {
                    let _ = fs::remove_file(&probe);
                    Some(true)
                }
                Err(e) if Self::refuses_links(&e) || Self::may_mean_no_links(&e, true) => {
                    let file = Self::partial_in(folder);
                    let takes_files = OpenOptions::new().write(true).create_new(true).open(&file).is_ok();
                    if takes_files {
                        Self::discard(&file);
                    }
                    takes_files.then_some(false)
                }
                Err(e) => {
                    log::info!("couldn't ask {} whether it holds links: {e}", folder.display());
                    None
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = folder;
            None
        }
    }

    /// Gives the finished copy its real name. Nothing may be at `to`: a file that appeared
    /// there at the last moment is never overwritten. Fails with `Error::Refused` when the
    /// drive can't hold the name.
    pub fn commit_new(partial: &Path, to: &Path) -> Result<()> {
        rename_new(partial, to).map_err(|e| {
            Self::discard(partial);
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Changed("something new appeared there meanwhile".into())
            } else if let Some(why) = Refusal::of_naming(&e) {
                Error::Refused { path: to.to_path_buf(), why }
            } else {
                Error::file(to, e)
            }
        })
    }

    /// Removes a copy Charon made: an unfinished one, or one it no longer needs in the trash.
    /// A copy of a read-only file is read-only too, and under Windows FAT and exFAT drives
    /// won't remove it with that mark, so it's lifted first.
    pub fn discard(copy: &Path) {
        Self::make_writable(copy);
        let _ = fs::remove_file(copy);
    }

    /// Lifts the read-only mark of a file or folder about to be replaced or removed: Windows
    /// refuses to replace or delete an item with it (Explorer marks customised folders so).
    /// Only for Charon's own copies and trash, and for items checked to be exactly what the
    /// plan saw.
    /// Read-only marks are Windows' own: elsewhere nothing is done.
    pub(crate) fn make_writable(path: &Path) {
        #[cfg(windows)]
        if let Ok(meta) = fs::symlink_metadata(path) {
            let mut permissions = meta.permissions();
            if (meta.is_file() || meta.is_dir()) && permissions.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                if let Err(e) = fs::set_permissions(path, permissions) {
                    log::warn!("couldn't lift the read-only mark of {}: {e}", path.display());
                }
            }
        }
        #[cfg(not(windows))]
        let _ = path;
    }

    /// A fresh temporary name in the folder of `to` (see `partial_in`).
    pub fn partial_path(to: &Path) -> PathBuf {
        Self::partial_in(to.parent().unwrap_or(Path::new("")))
    }

    /// A fresh temporary name in `folder` (see `scan::new_partial_name`): its time tells a
    /// copy that takes long from the leftover of an interrupted sync, whatever old time the
    /// copy carries at the end (see `scan::partial_started`).
    pub fn partial_in(folder: &Path) -> PathBuf {
        folder.join(new_partial_name())
    }

    /// Makes something under a fresh temporary name for `to` with `make`, which must fail
    /// when the name is taken, and returns that name with what `make` returned. The name
    /// goes next to `to`. Where that path is too long for the drive (a short name deep down
    /// has a temporary name longer than its own), it goes into the nearest folder above,
    /// up to `top`, where it fits; it's renamed into place from there, on the same drive.
    /// When `make` fails otherwise, its error comes back with the name it was tried under.
    pub fn temporary<T>(to: &Path, top: &Path, mut make: impl FnMut(&Path) -> io::Result<T>) -> std::result::Result<(PathBuf, T), (PathBuf, io::Error)> {
        let mut folder = to.parent().unwrap_or(Path::new(""));
        loop {
            let mut tries = 0;
            let (partial, error) = loop {
                let partial = Self::partial_in(folder);
                match make(&partial) {
                    Ok(made) => return Ok((partial, made)),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < 5 => tries += 1,
                    Err(e) => break (partial, e),
                }
            };
            let too_long = Refusal::of_naming(&error) == Some(Refusal::TooLong);
            match folder.parent() {
                Some(above) if too_long && folder != top && above.starts_with(top) => folder = above,
                _ => return Err((partial, error)),
            }
        }
    }

    /// Creates the temporary file for `to` (see `temporary`): always a new file, never an
    /// existing one and never through a link (`create_new`). A temporary path too long even
    /// in `top` is a failure, not a refusal of the target: the target's own path fits.
    fn create_partial(to: &Path, top: &Path) -> Result<(PathBuf, File)> {
        Self::temporary(to, top, |partial| OpenOptions::new().write(true).create_new(true).open(partial)).map_err(|(partial, e)| Error::file(partial, e))
    }

    /// Whether a file last modified at `modified` was saved so recently, as of `now`, that on
    /// a drive with coarse times it may still be being written.
    fn saved_a_moment_ago(modified: i64, now: i64) -> bool {
        modified > now - STILL_BEING_WRITTEN_MS && modified <= now + CLOCK_AHEAD_MS
    }

    /// Writes the copy and gives it the original's details. Returns the details it couldn't
    /// take along.
    fn write_partial(&self, from: &Path, partial: &Path, mut writer: File, expected: &Entry, on_bytes: &mut dyn FnMut(u64)) -> Result<BTreeSet<&'static str>> {
        let mut reader = Contents::open(from).map_err(|e| Self::unreadable(from, e, 0))?;
        let before = reader.metadata().map_err(|e| Error::file(from, e))?;
        let changed = Scanner::change_time(&before);
        if !before.is_file() || !Entry::file(before.len(), Scanner::modified_ms(&before)).changed_at(changed).unchanged(expected) {
            return Err(Error::Changed("changed since the scan".into()));
        }
        if self.coarse_source && Self::saved_a_moment_ago(Scanner::modified_ms(&before), Scanner::millis(std::time::SystemTime::now())) {
            return Err(Error::Changed("saved a moment ago and may still be being written, so it waits for the next sync".into()));
        }
        let mut buffer = vec![0u8; CHUNK];
        let mut written: u64 = 0;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = match reader.read(&mut buffer) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Self::unreadable(from, e, written)),
            };
            if n == 0 {
                break;
            }
            writer.write_all(&buffer[..n]).map_err(|e| Error::file(partial, e))?;
            written += n as u64;
            on_bytes(n as u64);
        }
        durable::sync_file(&writer).map_err(|e| Error::file(partial, e))?;

        // The source must be exactly as it was when reading started: same file, same size,
        // same modification time. Otherwise the copy may mix old and new contents.
        let after_handle = reader.metadata().map_err(|e| Error::file(from, e))?;
        let after_path = fs::symlink_metadata(from).map_err(|e| Error::file(from, e))?;
        // Where the drive keeps change times, an edit that put the old time back shows too.
        let unchanged = |m: &fs::Metadata| {
            m.is_file()
                && m.len() == before.len()
                && m.modified().ok() == before.modified().ok()
                && (expected.changed == 0 || Scanner::change_time(m) == changed)
        };
        if written != before.len() || !unchanged(&after_handle) || !unchanged(&after_path) {
            return Err(Error::Changed("changed while it was being copied".into()));
        }

        let unkept = Self::copy_extended_attributes(from, partial, self.attributes);
        // Times last, through the open file, so the copy carries the original's time. The
        // modification time is required; the creation time is a bonus some drives refuse.
        Self::keep_times(&writer, &before).map_err(|e| Error::file(partial, e))?;
        if let Some(created) = Self::created_of(&before) {
            if let Err(e) = writer.set_times(created) {
                log::debug!("couldn't keep the creation time of {}: {e}", from.display());
            }
        }
        durable::sync_file(&writer).map_err(|e| Error::file(partial, e))?;
        drop(writer);
        // Permissions after the times (a read-only file can't take new times on Windows).
        if let Err(e) = fs::set_permissions(partial, before.permissions()) {
            log::warn!("couldn't copy the permissions of {}: {e}", from.display());
        }
        Ok(unkept)
    }

    /// Why the file at `from` couldn't be read (or copied into the trash). A file the cloud
    /// took back since the scan (iCloud Drive, OneDrive and Dropbox free up space that way)
    /// can't be read without downloading it, which Charon never does: that's a change since
    /// the scan, not a failure. A file the system won't let Charon read is
    /// `Error::ReadDenied` (see `refusal_to_read`), with where reading stopped (`at`).
    pub(crate) fn unreadable(from: &Path, error: std::io::Error, at: u64) -> Error {
        if Contents::only_in_the_cloud(&error) {
            return Error::Changed(CLOUD_SINCE_SCAN.into());
        }
        match Self::refusal_to_read(&error) {
            Some(reason) => Error::ReadDenied { path: from.to_path_buf(), reason, at },
            None => Error::file(from, error),
        }
    }

    /// Why the system refuses to let Charon read a file, when `error` says it does: no
    /// permission (on a Mac, the privacy settings too), or another program has the file
    /// open for itself (Outlook's mailboxes, the NTUSER.DAT of a Windows profile in use) or
    /// has locked part of it. That lasts until someone changes it, so such a file is left
    /// alone like a place a scan can't read, instead of failing on every sync.
    pub fn refusal_to_read(error: &std::io::Error) -> Option<String> {
        let code = error.raw_os_error()?;
        #[cfg(unix)]
        {
            (code == libc::EACCES || code == libc::EPERM).then(|| Scanner::io_reason(error))
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
            match code as u32 {
                ERROR_ACCESS_DENIED => Some(Scanner::io_reason(error)),
                ERROR_SHARING_VIOLATION => {
                    Some("another program has it open and keeps others from reading it; once that program closes it, the next sync copies it".into())
                }
                ERROR_LOCK_VIOLATION => Some("another program has locked it while it works with it; once that program lets go, the next sync copies it".into()),
                _ => None,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = code;
            None
        }
    }

    /// Why the system refuses to let Charon read the file at `path` right now, if it does
    /// (see `refusal_to_read`). It reads what a copy read when it was refused there: the
    /// chunk at `at`, where reading stopped. A program may lock part of a file and still let
    /// others open it and read the rest.
    pub fn read_refusal(path: &Path, at: u64) -> Option<String> {
        let read = Contents::open(path).and_then(|mut file| {
            file.seek(SeekFrom::Start(at))?;
            file.read(&mut vec![0u8; CHUNK])
        });
        read.err().and_then(|e| Self::refusal_to_read(&e))
    }

    /// Gives the copy the original's modification (and access) time. A drive that refuses
    /// the access time gets the modification time alone; one that refuses times altogether
    /// (some network and cloud drives) keeps the copy anyway: the run records the time it
    /// landed with, so the copy isn't mistaken for a change later.
    fn keep_times(writer: &File, before: &fs::Metadata) -> std::io::Result<()> {
        let refused = |e: &std::io::Error| {
            matches!(e.kind(), std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidInput)
        };
        match writer.set_times(Self::times_of(before)) {
            Ok(()) => Ok(()),
            Err(e) if refused(&e) => {
                let Ok(modified) = before.modified() else { return Ok(()) };
                match writer.set_times(FileTimes::new().set_modified(modified)) {
                    Ok(()) => Ok(()),
                    Err(e) if refused(&e) => {
                        log::debug!("the drive doesn't take the file's time: {e}");
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Modification and access time of the original.
    fn times_of(meta: &fs::Metadata) -> FileTimes {
        let mut times = FileTimes::new();
        if let Ok(modified) = meta.modified() {
            times = times.set_modified(modified);
        }
        if let Ok(accessed) = meta.accessed() {
            times = times.set_accessed(accessed);
        }
        times
    }

    /// The original's creation time, on systems that let it be set (macOS and Windows).
    fn created_of(meta: &fs::Metadata) -> Option<FileTimes> {
        #[cfg(any(target_os = "macos", windows))]
        {
            #[cfg(target_os = "macos")]
            use std::os::macos::fs::FileTimesExt;
            #[cfg(windows)]
            use std::os::windows::fs::FileTimesExt;
            meta.created().ok().map(|created| FileTimes::new().set_created(created))
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = meta;
            None
        }
    }

    /// The extended attributes worth carrying: Finder tags, Finder info and resource forks
    /// on macOS, the `user.` ones on Linux. Others (quarantine flags, caches, labels of
    /// one machine) stay behind.
    #[cfg(unix)]
    fn carried(name: &std::ffi::OsStr) -> bool {
        let name = name.to_string_lossy();
        if cfg!(target_os = "macos") {
            matches!(name.as_ref(), TAGS | FINDER_INFO | RESOURCE_FORK)
        } else {
            name.starts_with("user.")
        }
    }

    /// What a carried attribute is, in the words of the run's summary. Finder info (a file's
    /// type and a few Finder flags) and where a download came from aren't worth a mention.
    #[cfg(unix)]
    fn detail(name: &std::ffi::OsStr) -> Option<&'static str> {
        match name.to_string_lossy().as_ref() {
            TAGS => Some("Finder tags"),
            RESOURCE_FORK => Some("resource fork"),
            FINDER_INFO => None,
            other if other.starts_with("user.xdg.origin.") || other.starts_with("user.xdg.referrer.") => None,
            _ => Some("extended attributes"),
        }
    }

    /// Copies the carried extended attributes, and returns those that didn't arrive. On a
    /// drive that can't hold them (`holds` false: FAT and exFAT keep them in `._` files next
    /// to every file), only a resource fork goes along, with the Finder info that belongs to
    /// it: an old Mac font or document keeps its contents there, and without it the copy
    /// would be an empty file.
    #[cfg(unix)]
    fn copy_extended_attributes(from: &Path, to: &Path, holds: bool) -> BTreeSet<&'static str> {
        let mut unkept = BTreeSet::new();
        let names: Vec<std::ffi::OsString> = match xattr::list(from) {
            Ok(names) => names.filter(|name| Self::carried(name)).collect(),
            Err(e) => {
                log::debug!("couldn't list extended attributes of {}: {e}", from.display());
                return unkept;
            }
        };
        let has_fork = names.iter().any(|name| name == RESOURCE_FORK) && xattr::get(from, RESOURCE_FORK).is_ok_and(|fork| fork.is_some_and(|fork| !fork.is_empty()));
        let mut drive_refuses = false;
        for name in &names {
            let wanted = holds || (has_fork && (name == RESOURCE_FORK || name == FINDER_INFO));
            if !wanted || drive_refuses {
                unkept.extend(Self::detail(name));
                continue;
            }
            let copied = xattr::get(from, name).and_then(|value| match value {
                Some(value) => xattr::set(to, name, &value),
                None => Ok(()),
            });
            match copied {
                Ok(()) => {}
                // The destination's drive can't hold extended attributes at all.
                Err(e) if matches!(e.raw_os_error(), Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP) => {
                    log::debug!("{} can't hold extended attributes: {e}", to.display());
                    drive_refuses = true;
                    unkept.extend(Self::detail(name));
                }
                Err(e) => {
                    log::info!("couldn't copy extended attribute {} of {}: {e}", name.to_string_lossy(), from.display());
                    unkept.extend(Self::detail(name));
                }
            }
        }
        unkept
    }

    #[cfg(not(unix))]
    fn copy_extended_attributes(_from: &Path, _to: &Path, _holds: bool) -> BTreeSet<&'static str> {
        BTreeSet::new()
    }
}

/// Finder tags.
#[cfg(unix)]
const TAGS: &str = "com.apple.metadata:_kMDItemUserTags";
/// A file's type, creator and Finder flags.
#[cfg(unix)]
const FINDER_INFO: &str = "com.apple.FinderInfo";
/// The second stream of contents of old Mac files.
#[cfg(unix)]
const RESOURCE_FORK: &str = "com.apple.ResourceFork";

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_of(path: &Path) -> Entry {
        let meta = fs::metadata(path).unwrap();
        Entry::file(meta.len(), Scanner::modified_ms(&meta))
    }

    #[test]
    fn copies_contents_and_modification_time() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "hello").unwrap();
        filetime::set_file_mtime(&from, filetime::FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
        let never = AtomicBool::new(false);
        let written = Copier::new(&never).file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap();
        assert!(written.unkept.is_empty());
        Copier::commit_new(&written.partial, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "hello");
        assert!(entry_of(&to).same_as(&entry_of(&from)));
    }

    #[test]
    fn refuses_a_source_that_changed_since_the_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "hello").unwrap();
        let planned = entry_of(&from);
        fs::write(&from, "hello, edited").unwrap();
        let never = AtomicBool::new(false);
        let result = Copier::new(&never).file(&from, &to, &planned, &mut |_| {});
        assert!(matches!(result, Err(Error::Changed(_))));
        let leftovers: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().filter(|i| crate::scan::is_partial(&i.file_name().to_string_lossy())).collect();
        assert!(leftovers.is_empty(), "the unfinished copy is gone");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_source_edited_with_its_old_time_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.bin"), tmp.path().join("b.bin"));
        fs::write(&from, "version one").unwrap();
        let meta = fs::metadata(&from).unwrap();
        let planned = entry_of(&from).changed_at(Scanner::change_time(&meta));
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Same size, same modification time, other contents (some tools keep the time).
        fs::write(&from, "version two").unwrap();
        filetime::set_file_mtime(&from, filetime::FileTime::from_last_modification_time(&meta)).unwrap();
        let never = AtomicBool::new(false);
        if Scanner::change_time(&fs::metadata(&from).unwrap()) != planned.changed {
            assert!(matches!(Copier::new(&never).file(&from, &to, &planned, &mut |_| {}), Err(Error::Changed(_))));
        }
    }

    #[test]
    fn a_file_found_only_in_the_cloud_is_left_alone_not_failed() {
        let path = Path::new("notes.txt");
        assert!(matches!(Copier::unreadable(path, std::io::Error::other("the disk broke"), 0), Error::File { .. }));
        #[cfg(target_os = "macos")]
        assert!(matches!(
            Copier::unreadable(path, std::io::Error::from_raw_os_error(libc::EDEADLK), 0),
            Error::Changed(why) if why.contains("only in the cloud")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_file_the_cloud_took_back_since_the_scan_is_never_downloaded() {
        use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_OFFLINE};
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "in the cloud").unwrap();
        let planned = entry_of(&from);
        // A cloud drive that frees up a file marks it so; only its name and details stay.
        let path = crate::run::rename::wide(&from);
        // SAFETY: `path` is NUL-terminated.
        unsafe {
            let attributes = GetFileAttributesW(path.as_ptr());
            assert_ne!(SetFileAttributesW(path.as_ptr(), attributes | FILE_ATTRIBUTE_OFFLINE), 0);
        }
        let never = AtomicBool::new(false);
        let result = Copier::new(&never).file(&from, &to, &planned, &mut |_| {});
        assert!(matches!(&result, Err(Error::Changed(why)) if why.contains("only in the cloud")), "{result:?}");
        // Nor is it downloaded to keep a copy of it in the trash.
        let trash = crate::run::Trash::for_run(chrono::Local::now());
        assert!(matches!(trash.preserve(tmp.path(), "a.txt"), Err(Error::Changed(why)) if why.contains("only in the cloud")));
    }

    #[test]
    fn a_file_saved_a_moment_ago_on_a_coarse_drive_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "fresh").unwrap();
        let never = AtomicBool::new(false);
        let result = Copier::new(&never).coarse_source_times(true).file(&from, &to, &entry_of(&from), &mut |_| {});
        assert!(matches!(result, Err(Error::Changed(_))));
        filetime::set_file_mtime(&from, filetime::FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
        assert!(Copier::new(&never).coarse_source_times(true).file(&from, &to, &entry_of(&from), &mut |_| {}).is_ok());
    }

    /// A network drive's clock may be a little ahead, so a file dated a moment ahead waits
    /// too. One dated far ahead (a camera with its clock set wrong) is no save going on: it's
    /// copied, rather than waiting on every sync until the computer's clock catches up.
    #[test]
    fn a_file_dated_far_ahead_on_a_coarse_drive_is_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("IMG_0001.JPG");
        fs::write(&from, "photo").unwrap();
        let never = AtomicBool::new(false);
        let copy = |ahead: std::time::Duration| {
            filetime::set_file_mtime(&from, filetime::FileTime::from_system_time(std::time::SystemTime::now() + ahead)).unwrap();
            Copier::new(&never).coarse_source_times(true).file(&from, &tmp.path().join("copy.jpg"), &entry_of(&from), &mut |_| {})
        };
        assert!(matches!(copy(std::time::Duration::from_secs(30)), Err(Error::Changed(_))));
        let far_ahead = copy(std::time::Duration::from_secs(40 * 24 * 60 * 60)).unwrap();
        assert_eq!(fs::read_to_string(&far_ahead.partial).unwrap(), "photo");
    }

    /// Windows: a copy of a read-only file is read-only too, and is still removed when it's
    /// no longer needed: its mark is lifted first, since FAT and exFAT drives won't remove a
    /// file that has one. (NTFS removes it anyway, so a second name of the same file shows
    /// whether the mark was lifted.)
    #[cfg(windows)]
    #[test]
    fn a_read_only_copy_is_still_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let copy = Copier::partial_in(tmp.path());
        let second_name = tmp.path().join("second name");
        fs::write(&copy, "half").unwrap();
        fs::hard_link(&copy, &second_name).unwrap();
        let mut permissions = fs::metadata(&copy).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&copy, permissions).unwrap();
        Copier::discard(&copy);
        assert!(!copy.exists());
        assert!(!fs::metadata(&second_name).unwrap().permissions().readonly(), "the mark was lifted before the copy was removed");
    }

    #[test]
    fn commit_new_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "copy").unwrap();
        let never = AtomicBool::new(false);
        let partial = Copier::new(&never).file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap().partial;
        fs::write(&to, "appeared meanwhile").unwrap();
        assert!(matches!(Copier::commit_new(&partial, &to), Err(Error::Changed(_))));
        assert_eq!(fs::read_to_string(&to).unwrap(), "appeared meanwhile");
        assert!(!partial.exists());
    }

    #[test]
    fn temporary_names_are_short_fresh_and_recognised() {
        let to = Path::new("/x/a very long name that goes on and on.mov");
        let (a, b) = (Copier::partial_path(to), Copier::partial_path(to));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 29);
        assert!(crate::scan::is_partial(&name));
    }

    /// A drive that can't keep extended attributes gets the file without them, and the copy
    /// says what stayed behind.
    #[cfg(target_os = "linux")]
    #[test]
    fn details_a_drive_cant_keep_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&from, "hello").unwrap();
        if xattr::set(&from, "user.xdg.tags", b"red").is_err() || xattr::set(&from, "user.xdg.origin.url", b"https://example.com").is_err() {
            return; // this drive keeps no extended attributes at all
        }
        let never = AtomicBool::new(false);
        let carried = Copier::new(&never).file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap();
        assert!(carried.unkept.is_empty());
        assert_eq!(xattr::get(&carried.partial, "user.xdg.tags").unwrap().as_deref(), Some(&b"red"[..]));
        let left = Copier::new(&never).without_attributes().file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap();
        assert_eq!(left.unkept.into_iter().collect::<Vec<_>>(), vec!["extended attributes"], "where a download came from isn't worth a word");
        assert_eq!(xattr::get(&left.partial, "user.xdg.tags").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_copied_as_links() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("link"), tmp.path().join("copy"));
        std::os::unix::fs::symlink("../somewhere/else", &from).unwrap();
        let meta = fs::symlink_metadata(&from).unwrap();
        let expected = Entry::link(b"../somewhere/else", Scanner::modified_ms(&meta));
        let partial = Copier::link(&from, &to, &expected, tmp.path()).unwrap();
        Copier::commit_new(&partial, &to).unwrap();
        assert_eq!(fs::read_link(&to).unwrap(), Path::new("../somewhere/else"));
    }

    #[cfg(unix)]
    #[test]
    fn only_a_drive_that_says_so_refuses_links_by_itself() {
        let error = std::io::Error::from_raw_os_error;
        assert!(Copier::refuses_links(&error(libc::EOPNOTSUPP)));
        // A locked folder answers "operation not permitted" too: the drive is asked first.
        assert!(!Copier::refuses_links(&error(libc::EPERM)));
        assert!(Copier::may_refuse_links(&Error::file("/x", error(libc::EPERM)), false));
        // So is "permission denied" from a share, which may be one folder that can't be written to.
        assert!(!Copier::refuses_links(&error(libc::EACCES)));
        assert!(Copier::may_refuse_links(&Error::file("/x", error(libc::EACCES)), true));
        assert!(Copier::may_refuse_links(&Error::file("/x", error(libc::EINVAL)), true));
        assert!(!Copier::may_refuse_links(&Error::file("/x", error(libc::EACCES)), false), "a local drive means just that");
        assert!(!Copier::may_refuse_links(&Error::file("/x", error(libc::EIO)), true));
        assert!(!Copier::may_refuse_links(&Error::NoLinks, true));
    }

    #[cfg(unix)]
    #[test]
    fn a_drive_asked_about_links_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Copier::drive_holds_links(tmp.path()), Some(true));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
        assert_eq!(Copier::drive_holds_links(&tmp.path().join("missing")), None, "a folder that isn't there can't tell");
    }

    /// A short name deep down, where the drive's limit on whole paths leaves room for the
    /// name but not for the longer temporary name next to it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_temporary_name_too_long_next_to_its_target_goes_a_folder_up() {
        let tmp = tempfile::tempdir().unwrap();
        // Paths on Linux end at 4,096 bytes: a folder whose path is 4,070 bytes long.
        let mut deep = tmp.path().to_path_buf();
        while deep.as_os_str().len() + 201 < 4070 {
            deep.push("d".repeat(200));
        }
        deep.push("e".repeat(4070 - deep.as_os_str().len() - 1));
        fs::create_dir_all(&deep).unwrap();
        let to = deep.join("f.txt");
        assert!(to.as_os_str().len() < 4095 && Copier::partial_path(&to).as_os_str().len() > 4095);
        let from = tmp.path().join("a.txt");
        fs::write(&from, "copy").unwrap();
        let never = AtomicBool::new(false);
        // Only next to the target, it can't be written: a failure, not a refusal of the target.
        let alone = Copier::new(&never).file(&from, &to, &entry_of(&from), &mut |_| {});
        assert!(matches!(alone, Err(Error::File { .. })), "{alone:?}");
        let written = Copier::new(&never).within(tmp.path()).file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap();
        assert_eq!(written.partial.parent(), deep.parent());
        Copier::commit_new(&written.partial, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "copy");
    }

    #[cfg(unix)]
    #[test]
    fn refusals_to_read_are_told_from_passing_problems() {
        let error = std::io::Error::from_raw_os_error;
        assert_eq!(Copier::refusal_to_read(&error(libc::EACCES)).as_deref(), Some("permission denied"));
        assert!(Copier::refusal_to_read(&error(libc::EPERM)).is_some());
        assert_eq!(Copier::refusal_to_read(&error(libc::EIO)), None, "a drive error is a failure to report");
        assert_eq!(Copier::refusal_to_read(&error(libc::ENOSPC)), None);
    }

    #[cfg(windows)]
    #[test]
    fn files_other_programs_hold_are_refusals_to_read() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_DISK_FULL, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
        let error = |code: u32| std::io::Error::from_raw_os_error(code as i32);
        assert!(Copier::refusal_to_read(&error(ERROR_SHARING_VIOLATION)).is_some_and(|reason| reason.contains("another program has it open")));
        assert!(Copier::refusal_to_read(&error(ERROR_LOCK_VIOLATION)).is_some_and(|reason| reason.contains("locked")));
        assert!(Copier::refusal_to_read(&error(ERROR_ACCESS_DENIED)).is_some());
        assert_eq!(Copier::refusal_to_read(&error(ERROR_DISK_FULL)), None);
    }

    /// A file the system won't let Charon read is no failure of the copy: it says why.
    #[cfg(unix)]
    #[test]
    fn a_file_charon_may_not_read_is_denied_not_failed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("locked.dat"), tmp.path().join("copy.dat"));
        fs::write(&from, "private").unwrap();
        let planned = entry_of(&from);
        fs::set_permissions(&from, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&from).is_ok() {
            return; // root ignores permissions
        }
        let never = AtomicBool::new(false);
        let result = Copier::new(&never).file(&from, &to, &planned, &mut |_| {});
        assert!(matches!(&result, Err(Error::ReadDenied { path, reason, at: 0 }) if path == &from && reason == "permission denied"), "{result:?}");
        assert_eq!(Copier::read_refusal(&from, 0).as_deref(), Some("permission denied"));
        assert_eq!(Copier::read_refusal(&from, CHUNK as u64).as_deref(), Some("permission denied"), "wherever reading stopped");
        let leftovers: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().filter(|i| crate::scan::is_partial(&i.file_name().to_string_lossy())).collect();
        assert!(leftovers.is_empty(), "the unfinished copy is gone");
        fs::set_permissions(&from, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(Copier::read_refusal(&from, 0), None, "readable again");
        assert_eq!(Copier::read_refusal(&from, 1 << 30), None, "past its end, there's nothing to refuse");
    }

    #[cfg(unix)]
    #[test]
    fn a_name_too_long_for_the_drive_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a.txt"), tmp.path().join("x".repeat(300)));
        fs::write(&from, "copy").unwrap();
        let never = AtomicBool::new(false);
        let partial = Copier::new(&never).file(&from, &to, &entry_of(&from), &mut |_| {}).unwrap().partial;
        assert!(matches!(Copier::commit_new(&partial, &to), Err(Error::Refused { why: Refusal::TooLong, .. })));
        assert!(!partial.exists(), "the unfinished copy is gone");
    }
}
