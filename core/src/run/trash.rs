//! Safe deletes: removed and replaced items go into `.charon-trash/<run time>_<run id>/` at
//! the root of their folder, and are cleared out after a number of days. Clearing out is
//! part of what a review lists ("Also empties 120 items older than 30 days…"): only the
//! dated folders the person saw listed are removed, so runs nobody looked at (full
//! automation) never empty the trash.
//!
//! A removed item is moved there. A replaced file is *copied* there instead and then
//! replaced in place: an app that has the file open (Pages, TextEdit, Preview…) follows the
//! file itself, not its name, so after a move it would go on saving the person's work into
//! the trash. Replaced in place, the app notices the new version instead. (On APFS, where a
//! copy takes no time and no room, a removed file is copied there too and then removed. So is
//! anything that goes into the trash from another drive than the trash's, in a folder
//! mounted inside the job's: a removed file or link, the old journal of a database, the
//! system's files left in a removed folder, a leftover of an interrupted sync. See
//! `Executor::move_to_trash` and `Executor::trash_aside`.)
//!
//! Each dated folder holds a list of what its run put there, with each file's size and time
//! (`.charon`). A file that no longer matches it was changed after it went into the trash:
//! an app may still be saving someone's work into it. A folder holding such a file is never
//! cleared out, and reviews say where the file is. The files the system scatters into folders
//! (Finder's .DS_Store) never count, nor does the placeholder a cloud service leaves for a
//! listed file it frees up, or for a package of listed files (iCloud Drive frees up a package
//! as one item), as long as it says the file or the package was as listed. On FAT and exFAT
//! drives, a time moved by whole hours (a daylight-saving change) doesn't count either.
//!
//! The trash folder must be a real folder: if `.charon-trash` is a link or a file, nothing
//! is moved into it and nothing is cleared out of it.
//!
//! In the trash an item's path is 42 bytes longer than its own (`.charon-trash/<run time>_<run
//! id>/`). Where that is too long for the drive, the item goes in under its own name at the
//! top of the dated folder instead of failing on every sync.

use crate::error::{Error, Result};
#[cfg(windows)]
use crate::run::identity::Identity;
use crate::run::marker::hide;
use crate::run::names;
use crate::run::rename::{move_to_new_name, rename_new};
use crate::run::{durable, Copier};
use crate::scan::{is_clutter, is_partial, CloudService, Entry, Keys, Placeholder, Refusal, Scanner, MTIME_TOLERANCE_MS, TRASH_FOLDER};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const STAMP_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";
/// `2026-10-20_09-00-00` is 19 characters long.
const STAMP_LENGTH: usize = 19;
/// The list of what a run put into its dated folder. No item of a synced folder can have
/// this name: Charon never carries anything named `.charon`.
const LIST: &str = ".charon";
const LIST_VERSION: u32 = 1;
/// Without a list (a run that was interrupted, or a dated folder from before the lists), a
/// file counts as changed in the trash when its time is this much later than its folder's:
/// a network drive's clock may be a little ahead of the computer's.
const UNLISTED_MARGIN_MS: i64 = 60 * 60 * 1000;

/// The files of one dated folder: path inside it (`/`-separated) → size and modification
/// time (milliseconds since 1970) as they landed.
type Listed = BTreeMap<String, (u64, i64)>;

/// What a dated folder's list says about one item in it.
#[derive(Debug, Clone, Copy)]
enum ListedItem {
    /// A file, with its size and time as it landed.
    File(u64, i64),
    /// A folder the list names files in, with their size together: a package Charon put
    /// into the trash file by file (the list names files only, never a folder).
    Folder(u64),
}

impl ListedItem {
    /// What `list` says about the item at `item` (a path in the dated folder): the file it
    /// names there, or else the listed files inside a folder there. `None` when it names
    /// neither.
    fn find(list: &Listed, item: &str) -> Option<Self> {
        if let Some(&(size, modified)) = list.get(item) {
            return Some(Self::File(size, modified));
        }
        let inside = format!("{item}/");
        let mut sizes = list.range(inside.clone()..).take_while(|(path, _)| path.starts_with(&inside)).map(|(_, &(size, _))| size).peekable();
        sizes.peek()?;
        Some(Self::Folder(sizes.sum()))
    }
}

#[derive(Serialize, Deserialize)]
struct StoredList {
    version: u32,
    items: Listed,
}

/// The trash of one run. Every item removed during the run goes into the same dated folder.
pub struct Trash {
    /// The dated folder's name: when the run started, and a short id of the run (two runs
    /// in the same second, from two computers sharing a drive, never share a folder).
    folder: String,
    /// What this run put into the trash of each folder (by the folder's root).
    kept: RefCell<BTreeMap<PathBuf, Listed>>,
    /// The folders whose list of names this run changed, from each root down.
    folders: RefCell<BTreeSet<PathBuf>>,
}

/// A dated trash folder old enough to be cleared out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expired {
    /// The folder's name inside `.charon-trash` (its date and time).
    pub name: String,
    /// Files and links in it, and their size.
    pub items: u64,
    pub bytes: u64,
}

/// What one look at a folder's trash found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashLook {
    /// Dated folders old enough to be cleared out, with nothing changed in them.
    pub expired: Vec<Expired>,
    /// Files changed after Charon put them into the trash, from the job's folder
    /// (`.charon-trash/2026-10-20_09-00-00_3fa2c1/report.pages`).
    pub revived: Vec<String>,
    /// Bytes in the dated folders looked into (all of them, when the look was thorough).
    pub bytes: u64,
}

/// What one dated folder holds.
#[derive(Debug, Default)]
struct Contents {
    items: u64,
    bytes: u64,
    /// Its files that changed after they went in (paths inside the folder).
    revived: Vec<String>,
    /// Part of it couldn't be read, so nobody can say what's in there.
    unreadable: bool,
}

impl Trash {
    /// A trash for a run starting now.
    pub fn for_run(started: DateTime<Local>) -> Self {
        let id = uuid::Uuid::new_v4().as_u128() as u32 & 0x00ff_ffff;
        Self { folder: format!("{}_{id:06x}", started.format(STAMP_FORMAT)), kept: RefCell::default(), folders: RefCell::default() }
    }

    /// Moves `root/relative` (on-disk spelling) into `root/.charon-trash/<folder>/relative`.
    /// Moving stays on the same drive, so it's instant even for big files.
    pub fn put(&self, root: &Path, relative: &str) -> Result<PathBuf> {
        self.put_from(&root.join(relative), root, relative)
    }

    /// Moves the item at `from` (in `root`'s folder) into the trash as `relative`: an item
    /// Charon had moved aside under a temporary name goes in under its own. When the trash
    /// can't take it (its folders, or a free name in it), this fails with that place in the
    /// trash; when the move itself fails, with `from`.
    pub fn put_from(&self, from: &Path, root: &Path, relative: &str) -> Result<PathBuf> {
        let base = self.dated(root)?;
        self.keep(root, &base, relative, |place| move_to_new_name(from, place).map_err(|e| Error::file(from, e)))
    }

    /// Copies the file or link `root/relative` into the trash, leaving it where it is. On
    /// drives that support it (APFS, Btrfs) a file's copy is a clone: instant, and it takes no
    /// room. When the file can't be read, this fails as `Copier::unreadable` says. When the
    /// trash can't take the copy (its folders, or the copy's place in it), it fails with that
    /// place in the trash: that is never a file Charon couldn't read.
    pub fn preserve(&self, root: &Path, relative: &str) -> Result<PathBuf> {
        self.preserve_from(&root.join(relative), root, relative)
    }

    /// `preserve`, for the file or link at `from` (in `root`'s folder), which goes into the
    /// trash as `relative`: an item Charon had moved aside under a temporary name goes in
    /// under its own.
    pub fn preserve_from(&self, from: &Path, root: &Path, relative: &str) -> Result<PathBuf> {
        let base = self.dated(root)?;
        if fs::symlink_metadata(from).is_ok_and(|meta| meta.is_symlink()) {
            return self.keep(root, &base, relative, |place| Self::copy_link(from, place));
        }
        // Written under a temporary name at the top of the dated folder first: a half-written
        // copy never takes a name, and nothing already in the trash is ever written over.
        let temp = Copier::partial_in(&base);
        Self::copy_file(from, &temp)?;
        self.keep(root, &base, relative, |place| rename_new(&temp, place).map_err(|e| Error::file(place, e))).inspect_err(|_| Copier::discard(&temp))
    }

    /// Gives an item a place in the dated folder `base` of `root`'s trash with `make`, which
    /// must fail with `AlreadyExists` when the place is taken: `relative`, with the folders
    /// above it, numbered when the same path went into the trash before in this run
    /// (replaced, then replaced again). Where that path is too long for the drive, the item
    /// goes in under its own name at the top of `base`. Returns where it is. When the folders
    /// above it can't be made, this fails with their path.
    fn keep(&self, root: &Path, base: &Path, relative: &str, mut make: impl FnMut(&Path) -> Result<()>) -> Result<PathBuf> {
        match self.keep_as(root, base, relative, &mut make) {
            Err(Error::File { source, .. }) if Refusal::of_naming(&source) == Some(Refusal::TooLong) && relative.contains('/') => {
                let name = Keys::name(relative);
                log::info!("{relative} went into the trash as {name}, at the top of {}: with its folders the path was too long for the drive", base.display());
                self.keep_as(root, base, name, &mut make)
            }
            kept => kept,
        }
    }

    /// `keep`, at `relative` in `base` only.
    fn keep_as(&self, root: &Path, base: &Path, relative: &str, make: &mut impl FnMut(&Path) -> Result<()>) -> Result<PathBuf> {
        if let Some(parent) = base.join(relative).parent() {
            fs::create_dir_all(parent).map_err(|e| Error::file(parent, e))?;
        }
        for n in 1..=1000 {
            let name = Self::numbered(relative, n);
            match make(&base.join(&name)) {
                Ok(()) => return Ok(self.list(root, base, &name)),
                Err(Error::File { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::file(base.join(relative), io::Error::new(io::ErrorKind::AlreadyExists, "no free name in the trash")))
    }

    /// Writes down, in each dated folder this run used, what it put there (see the module's
    /// notes). Returns the folders written to, for flushing.
    pub fn write_lists(&self) -> Vec<PathBuf> {
        let mut written = Vec::new();
        for (root, listed) in self.kept.borrow().iter() {
            let Ok(trash) = Self::real_trash(root, false) else { continue };
            let base = trash.join(&self.folder);
            match Self::write_list(&base, listed) {
                Ok(()) => written.push(base),
                Err(e) => log::warn!("couldn't write the list of {}: {e}", base.display()),
            }
        }
        written
    }

    /// The folders this run added names to, from each folder's root down to the items: all
    /// of them must be flushed for what went into the trash to survive a power cut.
    pub fn folders(&self) -> Vec<PathBuf> {
        self.folders.borrow().iter().cloned().collect()
    }

    /// The dated folder of this run in `root`'s trash (created when missing). When the trash
    /// can't be used, or the folder can't be made, this fails with that path.
    fn dated(&self, root: &Path) -> Result<PathBuf> {
        let base = Self::real_trash(root, true).map_err(|e| Error::file(root.join(TRASH_FOLDER), e))?.join(&self.folder);
        match fs::create_dir(&base) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(Error::file(base, e)),
            _ => Ok(base),
        }
    }

    /// Notes how the item `name` landed in `base` (a copy's time may have been refused by
    /// the drive), and returns where it is.
    fn list(&self, root: &Path, base: &Path, name: &str) -> PathBuf {
        let landed = base.join(name);
        let mut folders = self.folders.borrow_mut();
        for folder in landed.ancestors().skip(1) {
            if !folders.insert(folder.to_path_buf()) || folder == root {
                break;
            }
        }
        drop(folders);
        match fs::symlink_metadata(&landed) {
            Ok(meta) if !meta.is_dir() => {
                self.kept.borrow_mut().entry(root.to_path_buf()).or_default().insert(name.to_string(), (meta.len(), Scanner::modified_ms(&meta)));
            }
            Ok(_) => {}
            Err(e) => log::warn!("couldn't look at {} in the trash: {e}", landed.display()),
        }
        landed
    }

    fn write_list(base: &Path, listed: &Listed) -> io::Result<()> {
        let bytes = serde_json::to_vec(&StoredList { version: LIST_VERSION, items: listed.clone() }).map_err(io::Error::other)?;
        let list = base.join(LIST);
        let temp = Copier::partial_path(&list);
        let written = fs::OpenOptions::new().write(true).create_new(true).open(&temp).and_then(|mut file| {
            file.write_all(&bytes)?;
            durable::sync_file(&file)
        });
        match written.and_then(|()| fs::rename(&temp, &list)) {
            Ok(()) => {
                hide(&list);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    /// The list a dated folder holds, if it has one Charon can read.
    fn read_list(folder: &Path) -> Option<Listed> {
        let bytes = fs::read(folder.join(LIST)).ok()?;
        match serde_json::from_slice::<StoredList>(&bytes) {
            Ok(stored) if stored.version == LIST_VERSION => Some(stored.items),
            Ok(_) => None,
            Err(e) => {
                log::warn!("the list of {} is damaged ({e}); not used", folder.display());
                None
            }
        }
    }

    /// `a/plan.txt` for the first, `a/plan.txt (2)` for the second and so on, shortened to
    /// fit the drive's name limit.
    fn numbered(relative: &str, n: u32) -> String {
        if n == 1 {
            return relative.to_string();
        }
        let (folder, name) = match relative.rfind('/') {
            Some(i) => (&relative[..=i], &relative[i + 1..]),
            None => ("", relative),
        };
        format!("{folder}{}", names::fit(name, &format!(" ({n})")))
    }

    /// The contents, permissions and modification time of a regular file. Looking at the
    /// file, opening it and copying it fail as `Copier::unreadable` says: the copy goes into
    /// the folder this run just made, so a refusal there is about reading the file (another
    /// program holds it, or part of it). What's left to do to the copy fails with its path.
    fn copy_file(from: &Path, to: &Path) -> Result<()> {
        let unreadable = |e| Copier::unreadable(from, e, 0);
        let meta = fs::symlink_metadata(from).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(Error::file(from, io::Error::other("only files are copied into the trash")));
        }
        // A file only in the cloud is refused rather than downloaded (see
        // `scan::Contents::open`); it stays open while it's copied.
        let _reading = crate::scan::Contents::open(from).map_err(unreadable)?;
        if let Err(e) = fs::copy(from, to) {
            Copier::discard(to);
            return Err(unreadable(e));
        }
        // The copy must be on the drive before the original is replaced.
        if let Err(e) = Self::unlock(to).and_then(|()| Self::flush(to)) {
            Copier::discard(to);
            return Err(Error::file(to, e));
        }
        let modified = filetime::FileTime::from_last_modification_time(&meta);
        if let Err(e) = filetime::set_file_mtime(to, modified) {
            log::debug!("couldn't keep the time of {} in the trash: {e}", from.display());
        }
        Ok(())
    }

    /// Makes a link like the one at `from` at `to`, with its time: the link itself, never what
    /// it points to. Fails with `AlreadyExists` when `to` is taken.
    #[cfg(unix)]
    fn copy_link(from: &Path, to: &Path) -> Result<()> {
        let target = fs::read_link(from).map_err(|e| Error::file(from, e))?;
        std::os::unix::fs::symlink(&target, to).map_err(|e| Error::file(to, e))?;
        if let Ok(meta) = fs::symlink_metadata(from) {
            let modified = filetime::FileTime::from_last_modification_time(&meta);
            if let Err(e) = filetime::set_symlink_file_times(to, modified, modified) {
                log::debug!("couldn't keep the time of the link {} in the trash: {e}", from.display());
            }
        }
        Ok(())
    }

    /// Windows plans never hold links (see `Copier::link`).
    #[cfg(not(unix))]
    fn copy_link(from: &Path, _to: &Path) -> Result<()> {
        Err(Error::file(from, io::Error::other("only files are copied into the trash")))
    }

    /// A copy of a file locked in the Finder is locked too (macOS copies a file's flags
    /// along): it could neither take its place in the trash nor ever be cleared out of it.
    /// The copy's lock is lifted; the file itself keeps its own.
    #[cfg(target_os = "macos")]
    fn unlock(copy: &Path) -> io::Result<()> {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;
        let flags = fs::symlink_metadata(copy)?.st_flags();
        let locks = libc::UF_IMMUTABLE | libc::UF_APPEND;
        if flags & locks == 0 {
            return Ok(());
        }
        let path = std::ffi::CString::new(copy.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid NUL-terminated string that lives through the call.
        if unsafe { libc::chflags(path.as_ptr(), flags & !locks) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Only macOS copies a file's lock along with it.
    #[cfg(not(target_os = "macos"))]
    fn unlock(_copy: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Writes a file's contents to its drive. Windows needs write access for that, which a
    /// read-only copy only gets for the moment of the flush.
    fn flush(path: &Path) -> io::Result<()> {
        match fs::OpenOptions::new().read(true).write(cfg!(windows)).open(path) {
            Ok(file) => durable::sync_file(&file),
            #[cfg(windows)]
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Self::flush_read_only(path),
            Err(e) => Err(e),
        }
    }

    #[cfg(windows)]
    fn flush_read_only(path: &Path) -> io::Result<()> {
        let mut permissions = fs::metadata(path)?.permissions();
        if !permissions.readonly() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions.clone())?;
        let flushed = fs::OpenOptions::new().write(true).open(path).and_then(|file| durable::sync_file(&file));
        permissions.set_readonly(true);
        let marked = fs::set_permissions(path, permissions);
        flushed.and(marked)
    }

    /// Looks at `root`'s trash: the dated folders older than `days` (none when `days` is 0,
    /// which keeps everything), and the files changed since they went in. Only a `thorough`
    /// look checks the younger folders too (a background look leaves them for a review).
    /// `shifting` says the drive keeps local times (see `Volume::shifts_times`).
    pub fn look(root: &Path, days: u32, thorough: bool, shifting: bool) -> TrashLook {
        Self::look_going_on(root, days, thorough, shifting, &|| true)
    }

    /// `look`, asking `go_on` before it looks at the trash, and before each folder and item
    /// in it, which may note that the look moves on: when it says no, the look ends there,
    /// and a folder it didn't look at in full is never taken for one to empty.
    pub fn look_going_on(root: &Path, days: u32, thorough: bool, shifting: bool, go_on: &dyn Fn() -> bool) -> TrashLook {
        let mut look = TrashLook::default();
        if !go_on() {
            return look;
        }
        let Ok(trash) = Self::real_trash(root, false) else { return look };
        let Ok(items) = fs::read_dir(&trash) else { return look };
        let cutoff = Self::cutoff(days);
        for item in items.flatten().filter(|item| item.file_type().is_ok_and(|t| t.is_dir())) {
            if !go_on() {
                break;
            }
            let Some(name) = item.file_name().to_str().map(str::to_owned) else { continue };
            let Some(stamp) = Self::parse_stamp(&name) else { continue };
            let expired = cutoff.is_some_and(|cutoff| stamp < cutoff);
            if !expired && !thorough {
                continue;
            }
            let contents = Self::look_into(&item.path(), stamp, expired, shifting, go_on);
            look.bytes += contents.bytes;
            look.revived.extend(contents.revived.iter().map(|path| format!("{TRASH_FOLDER}/{name}/{path}")));
            if expired && contents.revived.is_empty() && !contents.unreadable {
                look.expired.push(Expired { name, items: contents.items, bytes: contents.bytes });
            }
        }
        look.expired.sort_by(|a, b| a.name.cmp(&b.name));
        look.revived.sort();
        look
    }

    /// What the dated `folder` (made at `stamp`) holds. With a list and no `walk`, only the
    /// listed files are looked at; otherwise everything in it is.
    /// `go_on` is asked before each item (see `look_going_on`).
    fn look_into(folder: &Path, stamp: DateTime<Local>, walk: bool, shifting: bool, go_on: &dyn Fn() -> bool) -> Contents {
        let list = Self::read_list(folder);
        match &list {
            Some(list) if !walk => Self::look_at_listed(folder, list, shifting, go_on),
            _ => Self::walk(folder, stamp, list.as_ref(), shifting, go_on),
        }
    }

    fn look_at_listed(folder: &Path, list: &Listed, shifting: bool, go_on: &dyn Fn() -> bool) -> Contents {
        let mut contents = Contents::default();
        for (path, listed) in list {
            if !go_on() {
                contents.unreadable = true;
                break;
            }
            match fs::symlink_metadata(folder.join(path)) {
                Ok(meta) => {
                    contents.items += 1;
                    contents.bytes += meta.len();
                    if meta.is_dir() || (!Self::as_listed(&meta, listed, shifting) && !Self::is_nobodys_work(Keys::name(path))) {
                        contents.revived.push(path.clone());
                    }
                }
                // Taken out of the trash by the person.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::debug!("couldn't look at {path} in {}: {e}", folder.display());
                    contents.unreadable = true;
                }
            }
        }
        contents
    }

    fn walk(folder: &Path, stamp: DateTime<Local>, list: Option<&Listed>, shifting: bool, go_on: &dyn Fn() -> bool) -> Contents {
        let mut contents = Contents::default();
        let made = stamp.timestamp_millis();
        let now = Local::now().timestamp_millis();
        for inner in walkdir::WalkDir::new(folder).min_depth(1).follow_links(false) {
            if !go_on() {
                contents.unreadable = true;
                break;
            }
            let inner = match inner {
                Ok(inner) => inner,
                Err(e) => {
                    log::debug!("couldn't look into {}: {e}", folder.display());
                    contents.unreadable = true;
                    continue;
                }
            };
            if inner.file_type().is_dir() {
                continue;
            }
            let Ok(inside) = inner.path().strip_prefix(folder) else { continue };
            let path = inside.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            if path == LIST {
                continue;
            }
            let Ok(meta) = inner.metadata() else {
                contents.unreadable = true;
                continue;
            };
            contents.items += 1;
            contents.bytes += meta.len();
            if Self::is_nobodys_work(&inner.file_name().to_string_lossy()) {
                continue;
            }
            let modified = Scanner::modified_ms(&meta);
            let revived = match list {
                Some(list) => match list.get(&path) {
                    Some(listed) => !Self::as_listed(&meta, listed, shifting),
                    None if Self::stands_in_for_listed(folder, &path, &meta, list, shifting) => false,
                    None => modified > made,
                },
                None => modified > made + UNLISTED_MARGIN_MS && modified <= now + 60_000,
            };
            if revived {
                contents.revived.push(path);
            }
        }
        contents
    }

    /// Dated folders made before this are old enough to be cleared out when they're kept
    /// for `days` (none when `days` is 0, which keeps everything).
    fn cutoff(days: u32) -> Option<DateTime<Local>> {
        (days > 0).then(|| Local::now() - chrono::Duration::days(days as i64))
    }

    /// Whether the file at `path` in the dated `folder`, with the details `meta`, is a cloud
    /// service's placeholder for a file the folder's list names, left when the service freed
    /// that file up as it was listed: the file is no longer there, and the placeholder says
    /// what the list says about it. Nextcloud and ownCloud give a placeholder the file's own
    /// time; iCloud Drive before macOS 14 turns `report.txt` into a new, hidden
    /// `.report.txt.icloud` that holds the file's size. It frees up a package (a folder the
    /// Finder shows as one document, `Doc.rtfd`) as one item, with one `.Doc.rtfd.icloud` for
    /// all of it, while the list names each file of the package: that placeholder must hold
    /// the size of all of them together. Such a stand-in holds nobody's work, whatever its own
    /// time, and goes along with its dated folder. One that says otherwise may stand for work
    /// saved into the trash after Charon put it there, and one that says nothing can't tell:
    /// either is judged by its time like any other file.
    fn stands_in_for_listed(folder: &Path, path: &str, meta: &fs::Metadata, list: &Listed, shifting: bool) -> bool {
        let Some(placeholder) = Placeholder::named(Keys::name(path)).filter(|_| meta.is_file()) else { return false };
        let item = match Keys::parent(path) {
            Some(parent) => format!("{parent}/{}", placeholder.item),
            None => placeholder.item.to_string(),
        };
        let Some(listed) = ListedItem::find(list, &item) else { return false };
        if !fs::symlink_metadata(folder.join(&item)).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
            return false;
        }
        match (placeholder.service, listed) {
            // The client's time is to the second; on FAT and exFAT it may have moved by whole
            // hours since, as for any file there.
            (CloudService::SyncClient, ListedItem::File(_, modified)) => {
                let own = Scanner::modified_ms(meta);
                (own - modified).abs() <= MTIME_TOLERANCE_MS || (shifting && Entry::whole_hours_apart(own, modified))
            }
            // Nextcloud and ownCloud leave placeholders for files only, and a folder of files
            // has no one time to compare theirs with.
            (CloudService::SyncClient, ListedItem::Folder(_)) => false,
            (CloudService::ICloud, ListedItem::File(size, _) | ListedItem::Folder(size)) => placeholder.recorded_size(&folder.join(path)) == Some(size),
        }
    }

    /// Whether a file named `name` in the trash never holds someone's work, whatever its
    /// time: one of Charon's own half-written copies, or a file the system writes into a
    /// folder by itself when the person looks into it (Finder's `.DS_Store`, Explorer's
    /// `Thumbs.db`). It goes along with its dated folder.
    fn is_nobodys_work(name: &str) -> bool {
        is_partial(name) || is_clutter(name)
    }

    /// Whether a file in the trash is still as it landed there (`listed`). On a drive that
    /// keeps local times (`shifting`: FAT, exFAT), a time moved by whole hours since (a
    /// daylight-saving change, another time zone) is no change, as for any file it holds.
    fn as_listed(meta: &fs::Metadata, &(size, modified): &(u64, i64), shifting: bool) -> bool {
        let now = Entry::file(meta.len(), Scanner::modified_ms(meta));
        Entry::still_on(Some(&now), Some(&Entry::file(size, modified)), shifting)
    }

    /// Permanently removes the dated folders `names` from `root`'s trash (those a review
    /// listed) that are still older than `days`, the setting as it is now: a list can wait a
    /// long time for another sync, and a setting changed meanwhile to keep more (or to keep
    /// everything, 0) keeps them. Anything else in the trash stays, and so does a folder in
    /// which something changed since the review. `shifting` is as for `look`.
    pub fn sweep(root: &Path, names: &[String], days: u32, shifting: bool) {
        let Some(cutoff) = Self::cutoff(days) else { return };
        if names.is_empty() {
            return;
        }
        let Ok(trash) = Self::real_trash(root, false) else { return };
        for name in names {
            let Some(stamp) = Self::parse_stamp(name).filter(|stamp| *stamp < cutoff) else { continue };
            let folder = trash.join(name);
            // Only a real folder: remove_dir_all never follows links inside it either.
            if !fs::symlink_metadata(&folder).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let contents = Self::look_into(&folder, stamp, true, shifting, &|| true);
            if !contents.revived.is_empty() || contents.unreadable {
                log::warn!("kept {}: something in it changed after it went into the trash, or can't be read", folder.display());
                continue;
            }
            if let Err(e) = Self::clear_out(&folder) {
                log::warn!("couldn't clear old trash {}: {e}", folder.display());
            }
        }
        // Leave no empty trash folder behind.
        let _ = fs::remove_dir(&trash);
    }

    /// Removes the dated `folder` with everything in it. On Windows what went into the trash
    /// keeps its read-only mark: NTFS removes a marked item all the same, FAT and exFAT
    /// refuse to. Only when the drive refuses are the marks lifted (see
    /// `lift_read_only_marks`), and the folder removed once more.
    fn clear_out(folder: &Path) -> io::Result<()> {
        match fs::remove_dir_all(folder) {
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
                Self::lift_read_only_marks(folder);
                fs::remove_dir_all(folder)
            }
            removed => removed,
        }
    }

    /// Windows: lifts the read-only marks of `folder` and of everything in it, never through
    /// a link. A file with more than one name (hard links) keeps its mark: the mark is the
    /// file's, so it would be lifted for every one of its names, which may be the person's
    /// files outside the trash. (FAT and exFAT, which need the marks lifted, have no hard
    /// links.)
    #[cfg(windows)]
    fn lift_read_only_marks(folder: &Path) {
        for item in walkdir::WalkDir::new(folder).follow_links(false).into_iter().flatten() {
            if item.file_type().is_file() && Identity::names(item.path()) != Some(1) {
                continue;
            }
            Copier::make_writable(item.path());
        }
    }

    /// Read-only marks are Windows' own.
    #[cfg(not(windows))]
    fn lift_read_only_marks(_folder: &Path) {}

    /// `root/.charon-trash`, if it is a real folder (created when `create` and missing).
    fn real_trash(root: &Path, create: bool) -> io::Result<PathBuf> {
        let trash = root.join(TRASH_FOLDER);
        match fs::symlink_metadata(&trash) {
            Ok(meta) if meta.is_dir() => Ok(trash),
            Ok(_) => Err(io::Error::other(format!("{TRASH_FOLDER} at the top of the folder isn't a real folder (a link or a file), so Charon won't use it"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                match fs::create_dir(&trash) {
                    Ok(()) => {
                        hide(&trash);
                        Ok(trash)
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::real_trash(root, false),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Whether `name` is one of the dated folders a trash holds.
    pub(crate) fn is_run_folder(name: &str) -> bool {
        Self::stamp_of(name).is_some()
    }

    /// When the dated folder `name` was made, in the computer's time zone.
    fn parse_stamp(name: &str) -> Option<DateTime<Local>> {
        let naive = Self::stamp_of(name)?;
        // A time that doesn't exist where the computer is now (it moved to another time
        // zone) is read as UTC: a few hours off, but never "never".
        Local.from_local_datetime(&naive).earliest().or_else(|| Some(naive.and_utc().with_timezone(&Local)))
    }

    /// `2026-10-20_09-00-00`, alone (earlier versions of Charon) or with a run id after it
    /// (`2026-10-20_09-00-00_3fa2c1`).
    fn stamp_of(name: &str) -> Option<NaiveDateTime> {
        let (stamp, id) = name.split_at_checked(STAMP_LENGTH)?;
        let id_fits = id.is_empty() || id.strip_prefix('_').is_some_and(|id| id.len() == 6 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        if !id_fits {
            return None;
        }
        NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days_ago(days: i64) -> DateTime<Local> {
        Local::now() - chrono::Duration::days(days)
    }

    fn set_time(path: &Path, at: DateTime<Local>) {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(at.timestamp(), 0)).unwrap();
    }

    /// Writes at `path` the placeholder iCloud Drive before macOS 14 leaves when it frees up a
    /// file of `size` bytes: a property list about the file, written now.
    fn icloud_placeholder(path: &Path, size: u64) {
        icloud_stand_in(path, "NSURLFileResourceTypeRegular", Some(size));
    }

    /// Writes at `path` the placeholder iCloud Drive before macOS 14 leaves when it frees up a
    /// package as one item: a property list about the package, with the size of all it held
    /// when there is one.
    fn icloud_package_placeholder(path: &Path, size: Option<u64>) {
        icloud_stand_in(path, "NSURLFileResourceTypeDirectory", size);
    }

    /// A property list about the item of type `kind` that the placeholder at `path` stands
    /// for, with the item's size when there is one, written now.
    fn icloud_stand_in(path: &Path, kind: &str, size: Option<u64>) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut about = plist::Dictionary::new();
        about.insert("NSURLFileResourceTypeKey".into(), plist::Value::String(kind.into()));
        if let Some(size) = size {
            about.insert("NSURLFileSizeKey".into(), size.into());
        }
        about.insert("NSURLNameKey".into(), plist::Value::String(Placeholder::named(&name).unwrap().item.into()));
        plist::Value::Dictionary(about).to_writer_binary(fs::File::create(path).unwrap()).unwrap();
    }

    /// Writes at `path` the placeholder Nextcloud leaves when it frees up a file: one space,
    /// with the file's own time, `time`.
    fn nextcloud_placeholder(path: &Path, time: DateTime<Local>) {
        fs::write(path, " ").unwrap();
        set_time(path, time);
    }

    #[test]
    fn moves_items_and_numbers_repeats() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(Local::now());
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/x.txt"), "1").unwrap();
        let first = trash.put(tmp.path(), "a/x.txt").unwrap();
        fs::write(tmp.path().join("a/x.txt"), "2").unwrap();
        let second = trash.put(tmp.path(), "a/x.txt").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), "1");
        assert_eq!(fs::read_to_string(second).unwrap(), "2");
    }

    #[test]
    fn preserving_copies_and_leaves_the_file_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(Local::now());
        fs::write(tmp.path().join("doc.txt"), "old version").unwrap();
        let kept = trash.preserve(tmp.path(), "doc.txt").unwrap();
        assert_eq!(fs::read_to_string(&kept).unwrap(), "old version");
        assert_eq!(fs::read_to_string(tmp.path().join("doc.txt")).unwrap(), "old version");
        let again = trash.preserve(tmp.path(), "doc.txt").unwrap();
        assert_ne!(kept, again);
        let leftovers: Vec<_> = fs::read_dir(kept.parent().unwrap()).unwrap().flatten().filter(|i| is_partial(&i.file_name().to_string_lossy())).collect();
        assert!(leftovers.is_empty(), "no temporary copy is left behind");
    }

    /// macOS: a file locked in the Finder is kept in the trash like any other, and its copy
    /// there carries no lock, so it can be cleared out later. The file keeps its own.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_file_is_kept_in_the_trash_without_its_lock() {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;
        let lock = |path: &Path, flags: u32| {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `path` is a valid NUL-terminated string that lives through the call.
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), flags) }, 0);
        };
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("locked.txt");
        fs::write(&file, "locked").unwrap();
        lock(&file, libc::UF_IMMUTABLE);
        let kept = Trash::for_run(Local::now()).preserve(tmp.path(), "locked.txt");
        assert_ne!(fs::symlink_metadata(&file).unwrap().st_flags() & libc::UF_IMMUTABLE, 0, "the file keeps its lock");
        lock(&file, 0);
        let kept = kept.unwrap();
        assert_eq!(fs::read_to_string(&kept).unwrap(), "locked");
        assert_eq!(fs::symlink_metadata(&kept).unwrap().st_flags() & libc::UF_IMMUTABLE, 0);
        fs::remove_file(&kept).unwrap();
    }

    /// When the trash can't take an item, the error names the trash, never the person's
    /// item, which stays where it is. When the item itself can't be moved, it names the item.
    #[test]
    fn a_refusal_names_the_trash_or_the_item_it_is_about() {
        let tmp = tempfile::tempdir().unwrap();
        let (item, trash_path) = (tmp.path().join("a.txt"), tmp.path().join(TRASH_FOLDER));
        fs::write(&item, "x").unwrap();
        fs::write(&trash_path, "not a folder").unwrap();
        let trash = Trash::for_run(Local::now());
        let put = trash.put(tmp.path(), "a.txt");
        assert!(matches!(&put, Err(Error::File { path, .. }) if path == &trash_path), "{put:?}");
        let preserved = trash.preserve(tmp.path(), "a.txt");
        assert!(matches!(&preserved, Err(Error::File { path, .. }) if path == &trash_path), "{preserved:?}");
        assert_eq!(fs::read_to_string(&item).unwrap(), "x");

        fs::remove_file(&trash_path).unwrap();
        let missing = trash.put(tmp.path(), "gone.txt");
        assert!(matches!(&missing, Err(Error::File { path, source }) if path == &tmp.path().join("gone.txt") && source.kind() == io::ErrorKind::NotFound), "{missing:?}");
    }

    #[test]
    fn two_runs_in_the_same_second_use_their_own_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Local::now();
        let (one, two) = (Trash::for_run(started), Trash::for_run(started));
        fs::write(tmp.path().join("a.txt"), "1").unwrap();
        fs::write(tmp.path().join("b.txt"), "2").unwrap();
        let (a, b) = (one.put(tmp.path(), "a.txt").unwrap(), two.put(tmp.path(), "b.txt").unwrap());
        assert_ne!(a.parent(), b.parent());
        for folder in [a.parent().unwrap(), b.parent().unwrap()] {
            assert!(Trash::is_run_folder(&folder.file_name().unwrap().to_string_lossy()));
        }
    }

    #[test]
    fn dated_folders_are_recognised_with_and_without_a_run_id() {
        assert!(Trash::is_run_folder("2026-10-20_09-00-00"));
        assert!(Trash::is_run_folder("2026-10-20_09-00-00_3fa2c1"));
        assert!(!Trash::is_run_folder("2026-10-20_09-00-00_3fa2c"));
        assert!(!Trash::is_run_folder("2026-10-20_09-00-00 copy"));
        assert!(!Trash::is_run_folder("2026-10-20"));
        assert!(!Trash::is_run_folder("Photos"));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_trash_folder_is_never_used() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, elsewhere) = (tmp.path().join("root"), tmp.path().join("elsewhere"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(elsewhere.join("2000-01-01_00-00-00")).unwrap();
        fs::write(elsewhere.join("2000-01-01_00-00-00/keep.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(TRASH_FOLDER)).unwrap();
        fs::write(root.join("a.txt"), "x").unwrap();
        assert!(Trash::for_run(Local::now()).put(&root, "a.txt").is_err());
        assert!(Trash::look(&root, 1, true, false).expired.is_empty());
        Trash::sweep(&root, &["2000-01-01_00-00-00".to_string()], 1, false);
        assert!(elsewhere.join("2000-01-01_00-00-00/keep.txt").exists(), "sweeping never follows the link");
    }

    #[test]
    fn lists_and_sweeps_only_old_dated_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join(TRASH_FOLDER);
        fs::create_dir_all(trash.join("2000-01-01_00-00-00/a")).unwrap();
        fs::write(trash.join("2000-01-01_00-00-00/a/x.txt"), "12345").unwrap();
        set_time(&trash.join("2000-01-01_00-00-00/a/x.txt"), Local.with_ymd_and_hms(1999, 12, 31, 0, 0, 0).unwrap());
        fs::create_dir_all(trash.join("not-a-stamp")).unwrap();
        let fresh = Local::now().format(STAMP_FORMAT).to_string();
        fs::create_dir_all(trash.join(&fresh)).unwrap();
        let look = Trash::look(tmp.path(), 30, false, false);
        assert_eq!(look.expired, vec![Expired { name: "2000-01-01_00-00-00".into(), items: 1, bytes: 5 }]);
        assert!(look.revived.is_empty());
        assert!(Trash::look(tmp.path(), 0, false, false).expired.is_empty(), "0 keeps everything");
        // The setting may have changed since the list was made: only what it allows now goes.
        let named = [look.expired[0].name.clone(), "not-a-stamp".into()];
        Trash::sweep(tmp.path(), &named, 0, false);
        assert!(trash.join("2000-01-01_00-00-00").exists(), "0 keeps everything, even a folder a list named");
        Trash::sweep(tmp.path(), &named, 365 * 100, false);
        assert!(trash.join("2000-01-01_00-00-00").exists(), "kept for longer now");
        Trash::sweep(tmp.path(), &named, 30, false);
        assert!(!trash.join("2000-01-01_00-00-00").exists());
        assert!(trash.join("not-a-stamp").exists());
        assert!(trash.join(fresh).exists());
    }

    #[test]
    fn a_look_told_to_stop_takes_no_folder_for_one_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join(TRASH_FOLDER).join("2000-01-01_00-00-00");
        fs::create_dir_all(&old).unwrap();
        for n in 0..3 {
            let file = old.join(format!("{n}.txt"));
            fs::write(&file, "x").unwrap();
            set_time(&file, Local.with_ymd_and_hms(1999, 12, 31, 0, 0, 0).unwrap());
        }
        // It asks before the trash, the folder and each item in it. Told to stop partway, it
        // ends there.
        let asked = std::cell::Cell::new(0);
        let look = Trash::look_going_on(tmp.path(), 30, false, false, &|| {
            asked.set(asked.get() + 1);
            asked.get() <= 3
        });
        assert!(look.expired.is_empty(), "{look:?}");
        assert_eq!(asked.get(), 4);
        let asked = std::cell::Cell::new(0);
        let look = Trash::look_going_on(tmp.path(), 30, false, false, &|| {
            asked.set(asked.get() + 1);
            true
        });
        assert_eq!(look.expired.len(), 1);
        assert_eq!(asked.get(), 5);
        // Told to stop before it began, it doesn't even list the trash.
        fs::remove_dir_all(&old).unwrap();
        let asked = std::cell::Cell::new(0);
        let look = Trash::look_going_on(tmp.path(), 30, false, false, &|| {
            asked.set(asked.get() + 1);
            false
        });
        assert_eq!((look.expired.len(), asked.get()), (0, 1));
    }

    /// An app that had the file open goes on saving into the trash: that folder is never
    /// cleared out, and the look says where the file is.
    #[test]
    fn a_file_saved_into_the_trash_is_never_cleared_out() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        fs::write(tmp.path().join("report.pages"), "the old report").unwrap();
        fs::write(tmp.path().join("other.txt"), "x").unwrap();
        set_time(&tmp.path().join("report.pages"), days_ago(41));
        let kept = trash.put(tmp.path(), "report.pages").unwrap();
        trash.put(tmp.path(), "other.txt").unwrap();
        assert_eq!(trash.write_lists().len(), 1);
        let folder = kept.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();

        let look = Trash::look(tmp.path(), 30, false, false);
        assert_eq!(look.expired.iter().map(|e| (e.name.as_str(), e.items)).collect::<Vec<_>>(), vec![(folder.as_str(), 2)], "the list itself isn't an item");

        // Days later, the app saves the person's work into the file it followed.
        fs::write(&kept, "the report, with this week's work").unwrap();
        set_time(&kept, days_ago(2));
        let look = Trash::look(tmp.path(), 30, false, false);
        assert!(look.expired.is_empty());
        assert_eq!(look.revived, vec![format!("{TRASH_FOLDER}/{folder}/report.pages")]);
        Trash::sweep(tmp.path(), std::slice::from_ref(&folder), 30, false);
        assert_eq!(fs::read_to_string(&kept).unwrap(), "the report, with this week's work");
    }

    /// Finder writes a `.DS_Store` into every folder the person opens (Explorer a
    /// `Thumbs.db`), also a dated folder of the trash they look into for a removed file.
    /// That's nobody's work: the folder is still cleared out when its time comes.
    #[test]
    fn what_the_system_writes_into_the_trash_is_nobodys_work() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        fs::write(tmp.path().join("old.txt"), "x").unwrap();
        set_time(&tmp.path().join("old.txt"), days_ago(41));
        fs::create_dir_all(tmp.path().join("Docs")).unwrap();
        fs::write(tmp.path().join("Docs/.DS_Store"), "view").unwrap();
        set_time(&tmp.path().join("Docs/.DS_Store"), days_ago(41));
        let kept = trash.put(tmp.path(), "old.txt").unwrap();
        let listed = trash.put(tmp.path(), "Docs/.DS_Store").unwrap();
        trash.write_lists();
        let dated = kept.parent().unwrap();
        let folder = dated.file_name().unwrap().to_string_lossy().into_owned();

        // Opened in Finder and Explorer days later.
        for written in [dated.join(".DS_Store"), dated.join("Docs/Thumbs.db"), dated.join("Docs/._old.txt"), listed] {
            fs::write(&written, "the system's own").unwrap();
            set_time(&written, days_ago(2));
        }
        for thorough in [false, true] {
            let look = Trash::look(tmp.path(), 30, thorough, false);
            assert!(look.revived.is_empty(), "{:?}", look.revived);
            assert_eq!(look.expired.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec![folder.as_str()]);
        }
        let young = Trash::for_run(days_ago(1));
        fs::write(tmp.path().join("Docs/.DS_Store"), "view").unwrap();
        let listed = young.put(tmp.path(), "Docs/.DS_Store").unwrap();
        young.write_lists();
        fs::write(&listed, "the view, changed").unwrap();
        assert!(Trash::look(tmp.path(), 30, true, false).revived.is_empty(), "a listed file of the system's changed by the system");
        Trash::sweep(tmp.path(), std::slice::from_ref(&folder), 30, false);
        assert!(!dated.exists());
    }

    /// iCloud Drive before macOS 14 frees up the files nobody used for long, the trash's old
    /// ones first: the file goes, and a new hidden `.report.txt.icloud` stands in for it, which
    /// holds the file's size. Nextcloud's `plans.odt.nextcloud` has the file's own time
    /// instead. Such a stand-in is nobody's work: the folder is still cleared out when its time
    /// comes, with it. A placeholder for a file the list doesn't name, or for one that's still
    /// there, is judged by its time like any other file.
    #[test]
    fn a_cloud_placeholder_for_a_listed_file_is_nobodys_work() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        fs::create_dir_all(tmp.path().join("Docs")).unwrap();
        let mut kept = Vec::new();
        // The plans were saved while the run was under way, after its folder's time.
        for (name, time) in [("report.txt", days_ago(41)), ("Docs/plans.odt", days_ago(39)), ("notes.txt", days_ago(41))] {
            fs::write(tmp.path().join(name), "the old version").unwrap();
            set_time(&tmp.path().join(name), time);
            kept.push(trash.put(tmp.path(), name).unwrap());
        }
        trash.write_lists();
        let dated = kept[0].parent().unwrap().to_path_buf();
        let folder = dated.file_name().unwrap().to_string_lossy().into_owned();

        // Freed up days later.
        fs::remove_file(dated.join("report.txt")).unwrap();
        icloud_placeholder(&dated.join(".report.txt.icloud"), "the old version".len() as u64);
        fs::remove_file(dated.join("Docs/plans.odt")).unwrap();
        nextcloud_placeholder(&dated.join("Docs/plans.odt.nextcloud"), days_ago(39));
        for thorough in [false, true] {
            let look = Trash::look(tmp.path(), 30, thorough, false);
            assert!(look.revived.is_empty(), "{:?}", look.revived);
            assert_eq!(look.expired.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec![folder.as_str()]);
        }
        for placeholder in [".notes.txt.icloud", ".other.txt.icloud"] {
            icloud_placeholder(&dated.join(placeholder), "the old version".len() as u64);
        }
        let look = Trash::look(tmp.path(), 30, false, false);
        assert!(look.expired.is_empty());
        assert_eq!(look.revived, vec![format!("{TRASH_FOLDER}/{folder}/.notes.txt.icloud"), format!("{TRASH_FOLDER}/{folder}/.other.txt.icloud")]);

        for placeholder in [".notes.txt.icloud", ".other.txt.icloud"] {
            fs::remove_file(dated.join(placeholder)).unwrap();
        }
        Trash::sweep(tmp.path(), std::slice::from_ref(&folder), 30, false);
        assert!(!dated.exists(), "cleared out, the stand-ins with it");
    }

    /// iCloud Drive before macOS 14 frees up a package (an `.rtfd`, a Logic project: a folder
    /// the Finder shows as one document) as one item: the folder goes with all it holds, and
    /// one hidden `.Doc.rtfd.icloud` stands in for it, which holds the size of all of it.
    /// Charon put the package's files into the trash one by one and listed each, never the
    /// folder. When they add up to that size, the stand-in is nobody's work: the folder is
    /// still cleared out when its time comes, with it.
    #[test]
    fn a_cloud_placeholder_for_a_package_of_listed_files_is_nobodys_work() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        let files = [
            ("Doc.rtfd/TXT.rtf", "the text of the document"),
            ("Doc.rtfd/photo.png", "a picture in it"),
            // Named like the package, next to it: no part of it.
            ("Doc.rtfd.zip", "a zipped copy of the document"),
            ("Music/Song.logicx/Alternatives/000/ProjectData", "the song"),
            ("Music/Song.logicx/Resources/Info.plist", "about the song"),
        ];
        for (name, text) in files {
            let path = tmp.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            set_time(&path, days_ago(41));
            trash.put(tmp.path(), name).unwrap();
        }
        trash.write_lists();
        let dated = tmp.path().join(TRASH_FOLDER).join(&trash.folder);
        let size_of = |package: &str| files.iter().filter(|(name, _)| name.starts_with(&format!("{package}/"))).map(|(_, text)| text.len() as u64).sum::<u64>();

        // Freed up weeks later, each package as one item.
        for (package, placeholder) in [("Doc.rtfd", ".Doc.rtfd.icloud"), ("Music/Song.logicx", "Music/.Song.logicx.icloud")] {
            fs::remove_dir_all(dated.join(package)).unwrap();
            icloud_package_placeholder(&dated.join(placeholder), Some(size_of(package)));
        }
        for thorough in [false, true] {
            let look = Trash::look(tmp.path(), 30, thorough, false);
            assert!(look.revived.is_empty(), "{:?}", look.revived);
            assert_eq!(look.expired.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec![trash.folder.as_str()]);
        }
        Trash::sweep(tmp.path(), std::slice::from_ref(&trash.folder), 30, false);
        assert!(!dated.exists(), "cleared out, the stand-ins with it");
    }

    /// Someone's work saved into a trashed file holds its dated folder. When the cloud service
    /// then frees that file up, its placeholder says the file changed after it went in:
    /// Nextcloud's has the time of the work, iCloud Drive's its size. The folder stays held,
    /// the work in the cloud with it, and so it does for a placeholder that can't be read and
    /// came after the folder.
    #[test]
    fn a_file_changed_in_the_trash_still_holds_its_folder_once_its_service_frees_it_up() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        let mut kept = Vec::new();
        for name in ["plans.odt", "report.txt", "notes.txt"] {
            fs::write(tmp.path().join(name), "the old version").unwrap();
            set_time(&tmp.path().join(name), days_ago(41));
            kept.push(trash.put(tmp.path(), name).unwrap());
        }
        trash.write_lists();
        let dated = kept[0].parent().unwrap().to_path_buf();
        let folder = dated.file_name().unwrap().to_string_lossy().into_owned();
        // Apps that had them open saved someone's work into them, ten days ago.
        let work = "someone's work, saved into the trash";
        for file in &kept {
            fs::write(file, work).unwrap();
            set_time(file, days_ago(10));
        }
        assert_eq!(Trash::look(tmp.path(), 30, true, false).revived.len(), 3, "held while the work is on disk");

        // The services free them up.
        for file in &kept {
            fs::remove_file(file).unwrap();
        }
        nextcloud_placeholder(&dated.join("plans.odt.nextcloud"), days_ago(10));
        icloud_placeholder(&dated.join(".report.txt.icloud"), work.len() as u64);
        fs::write(dated.join(".notes.txt.icloud"), "not a property list").unwrap();
        let look = Trash::look(tmp.path(), 30, true, false);
        assert!(look.expired.is_empty(), "{look:?}");
        let held = [".notes.txt.icloud", ".report.txt.icloud", "plans.odt.nextcloud"];
        assert_eq!(look.revived, held.map(|placeholder| format!("{TRASH_FOLDER}/{folder}/{placeholder}")));
        Trash::sweep(tmp.path(), std::slice::from_ref(&folder), 30, false);
        assert!(held.iter().all(|placeholder| dated.join(placeholder).exists()), "never cleared out");
    }

    /// A package's placeholder that doesn't say the package held just what Charon listed in
    /// it is judged by its time like any other file, and holds its folder: one with another
    /// size (someone's work saved into the package after it went in), one with no size, and
    /// one next to a package that's still there.
    #[test]
    fn a_cloud_placeholder_for_a_package_changed_in_the_trash_holds_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        let text = "the text of the document";
        for package in ["Present.rtfd", "Saved.rtfd", "Unsized.rtfd"] {
            let name = format!("{package}/TXT.rtf");
            fs::create_dir_all(tmp.path().join(package)).unwrap();
            fs::write(tmp.path().join(&name), text).unwrap();
            set_time(&tmp.path().join(&name), days_ago(41));
            trash.put(tmp.path(), &name).unwrap();
        }
        trash.write_lists();
        let dated = tmp.path().join(TRASH_FOLDER).join(&trash.folder);

        // An app that had one open saved someone's work into it before iCloud freed it up.
        let work = "someone's work, saved into the trash";
        fs::remove_dir_all(dated.join("Saved.rtfd")).unwrap();
        icloud_package_placeholder(&dated.join(".Saved.rtfd.icloud"), Some((text.len() + work.len()) as u64));
        fs::remove_dir_all(dated.join("Unsized.rtfd")).unwrap();
        icloud_package_placeholder(&dated.join(".Unsized.rtfd.icloud"), None);
        icloud_package_placeholder(&dated.join(".Present.rtfd.icloud"), Some(text.len() as u64));
        let look = Trash::look(tmp.path(), 30, true, false);
        assert!(look.expired.is_empty(), "{look:?}");
        let held = [".Present.rtfd.icloud", ".Saved.rtfd.icloud", ".Unsized.rtfd.icloud"];
        assert_eq!(look.revived, held.map(|placeholder| format!("{TRASH_FOLDER}/{}/{placeholder}", trash.folder)));
        Trash::sweep(tmp.path(), std::slice::from_ref(&trash.folder), 30, false);
        assert!(held.iter().all(|placeholder| dated.join(placeholder).exists()), "never cleared out");
    }

    #[test]
    fn a_younger_folder_is_checked_by_its_list_when_the_look_is_thorough() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(1));
        fs::write(tmp.path().join("notes.txt"), "old").unwrap();
        set_time(&tmp.path().join("notes.txt"), days_ago(3));
        let kept = trash.preserve(tmp.path(), "notes.txt").unwrap();
        trash.write_lists();
        assert_eq!(Trash::look(tmp.path(), 30, true, false), TrashLook { bytes: 3, ..Default::default() });
        fs::write(&kept, "edited in the trash").unwrap();
        assert!(Trash::look(tmp.path(), 30, false, false).revived.is_empty(), "a quick look leaves young folders alone");
        assert_eq!(Trash::look(tmp.path(), 30, true, false).revived.len(), 1);
    }

    /// FAT and exFAT keep local times: after a daylight-saving change, the drive reports every
    /// file there whole hours off. In the trash that's no change, so an old folder is still
    /// cleared out. A time moved any other way still counts, and on other drives so does this.
    #[test]
    fn a_time_moved_by_whole_hours_is_no_change_on_a_drive_that_keeps_local_times() {
        let tmp = tempfile::tempdir().unwrap();
        let mut kept = Vec::new();
        for (name, age) in [("old.docx", 40), ("young.docx", 1), ("edited.docx", 1)] {
            let trash = Trash::for_run(days_ago(age));
            let at = days_ago(age + 1);
            fs::write(tmp.path().join(name), "the old version").unwrap();
            set_time(&tmp.path().join(name), at);
            kept.push((trash.put(tmp.path(), name).unwrap(), at));
            trash.write_lists();
        }
        let old_folder = kept[0].0.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        // The clocks changed: the drive reports the files an hour later. One was edited.
        set_time(&kept[0].0, kept[0].1 + chrono::Duration::hours(1));
        set_time(&kept[1].0, kept[1].1 + chrono::Duration::hours(1));
        set_time(&kept[2].0, kept[2].1 + chrono::Duration::minutes(90));
        let edited = format!("{TRASH_FOLDER}/{}/edited.docx", kept[2].0.parent().unwrap().file_name().unwrap().to_string_lossy());

        let look = Trash::look(tmp.path(), 30, true, true);
        assert_eq!(look.revived, vec![edited]);
        assert_eq!(look.expired.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec![old_folder.as_str()]);
        assert_eq!(Trash::look(tmp.path(), 30, true, false).revived.len(), 3, "elsewhere a moved time is a change");
        Trash::sweep(tmp.path(), std::slice::from_ref(&old_folder), 30, true);
        assert!(!kept[0].0.exists());
    }

    /// Finder leaves a .DS_Store in every folder someone looks into, the trash included, and
    /// updates the ones Charon put there with their folders. They hold nobody's work: the
    /// folder is still cleared out, with them.
    #[test]
    fn finders_files_in_the_trash_never_count_as_someones_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut folders = Vec::new();
        for age in [40, 1] {
            let trash = Trash::for_run(days_ago(age));
            fs::create_dir_all(tmp.path().join("docs")).unwrap();
            fs::write(tmp.path().join("docs/report.txt"), "the old report").unwrap();
            fs::write(tmp.path().join("docs/.DS_Store"), "view settings").unwrap();
            for name in ["docs/report.txt", "docs/.DS_Store"] {
                set_time(&tmp.path().join(name), days_ago(age + 1));
                trash.put(tmp.path(), name).unwrap();
            }
            trash.write_lists();
            folders.push(tmp.path().join(TRASH_FOLDER).join(&trash.folder));
        }
        // Someone looks into the trash with Finder.
        for folder in &folders {
            fs::write(folder.join("docs/.DS_Store"), "other view settings").unwrap();
            fs::write(folder.join(".DS_Store"), "view settings").unwrap();
        }

        let look = Trash::look(tmp.path(), 30, true, false);
        assert!(look.revived.is_empty(), "{:?}", look.revived);
        let old = folders[0].file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(look.expired.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec![old.as_str()]);
        Trash::sweep(tmp.path(), &[old], 30, false);
        assert!(!folders[0].exists());
        assert!(folders[1].exists());
    }

    /// Windows: what went into the trash keeps its read-only mark, and is still cleared out
    /// with its folder. (FAT and exFAT drives won't remove a read-only file or folder until
    /// the mark is lifted.)
    #[cfg(windows)]
    #[test]
    fn read_only_items_in_old_trash_are_cleared_out() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = Trash::for_run(days_ago(40));
        fs::create_dir_all(tmp.path().join("from a cd")).unwrap();
        fs::write(tmp.path().join("from a cd/photo.jpg"), "photo").unwrap();
        set_time(&tmp.path().join("from a cd/photo.jpg"), days_ago(41));
        let kept = trash.put(tmp.path(), "from a cd/photo.jpg").unwrap();
        trash.write_lists();
        for item in [kept.as_path(), kept.parent().unwrap()] {
            let mut permissions = fs::metadata(item).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(item, permissions).unwrap();
        }
        let look = Trash::look(tmp.path(), 30, false, false);
        assert_eq!(look.expired.len(), 1);
        Trash::sweep(tmp.path(), &[look.expired[0].name.clone()], 30, false);
        assert!(!tmp.path().join(TRASH_FOLDER).exists());
    }

    /// Windows: where a drive won't remove a marked item (FAT, exFAT), the read-only marks in
    /// a dated folder are lifted, the folder's own included. A file with a name outside the
    /// folder too keeps its mark. (NTFS removes a marked item all the same, so this looks at
    /// the marks themselves.)
    #[cfg(windows)]
    #[test]
    fn the_read_only_marks_in_a_dated_folder_are_lifted() {
        let read_only = |item: &Path| {
            let mut permissions = fs::metadata(item).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(item, permissions).unwrap();
        };
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("2000-01-01_00-00-00");
        let inner = folder.join("from a cd");
        let (file, shared, outside) = (inner.join("photo.jpg"), inner.join("shared.jpg"), tmp.path().join("shared.jpg"));
        fs::create_dir_all(&inner).unwrap();
        fs::write(&file, "photo").unwrap();
        fs::write(&outside, "a file with two names").unwrap();
        fs::hard_link(&outside, &shared).unwrap();
        for item in [&file, &outside, &inner, &folder] {
            read_only(item);
        }
        Trash::lift_read_only_marks(&folder);
        for item in [&file, &inner, &folder] {
            assert!(!fs::metadata(item).unwrap().permissions().readonly(), "{} is still read-only", item.display());
        }
        assert!(fs::metadata(&outside).unwrap().permissions().readonly(), "the file's other name keeps its mark");
    }

    /// Windows: a file in the trash may have other names outside it (hard links: a git clone
    /// of a repository on the same drive shares its objects' files with it), and a read-only
    /// mark is the file's, shared by all of its names. Clearing out old trash leaves that
    /// mark as it is: NTFS removes the trash's name all the same.
    #[cfg(windows)]
    #[test]
    fn clearing_out_the_trash_keeps_the_mark_of_a_file_with_names_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (original, clone) = (tmp.path().join("repository/objects/ab12"), tmp.path().join("clone/objects/ab12"));
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::create_dir_all(clone.parent().unwrap()).unwrap();
        fs::write(&original, "an object").unwrap();
        set_time(&original, days_ago(41));
        fs::hard_link(&original, &clone).unwrap();
        let trash = Trash::for_run(days_ago(40));
        let kept = trash.put(tmp.path(), "clone/objects/ab12").unwrap();
        trash.write_lists();
        let mut permissions = fs::metadata(&original).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&original, permissions).unwrap();

        let look = Trash::look(tmp.path(), 30, false, false);
        assert_eq!(look.expired.len(), 1, "{look:?}");
        Trash::sweep(tmp.path(), &[look.expired[0].name.clone()], 30, false);
        assert!(!kept.exists(), "cleared out");
        assert!(fs::metadata(&original).unwrap().permissions().readonly(), "the repository's object keeps its mark");
        assert_eq!(fs::read_to_string(&original).unwrap(), "an object");
    }

    /// A folder without a list (an interrupted run, an earlier version) is judged by the
    /// times of its files: only one clearly newer than the folder counts as changed there.
    #[test]
    fn without_a_list_only_a_clearly_newer_file_counts_as_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let name = days_ago(40).format(STAMP_FORMAT).to_string();
        let folder = tmp.path().join(TRASH_FOLDER).join(&name);
        fs::create_dir_all(&folder).unwrap();
        for (file, age) in [("older.txt", 50), ("future.txt", -400)] {
            fs::write(folder.join(file), "x").unwrap();
            set_time(&folder.join(file), days_ago(age));
        }
        assert_eq!(Trash::look(tmp.path(), 30, false, false).expired.len(), 1);
        fs::write(folder.join("saved-later.txt"), "x").unwrap();
        set_time(&folder.join("saved-later.txt"), days_ago(10));
        let look = Trash::look(tmp.path(), 30, false, false);
        assert!(look.expired.is_empty());
        assert_eq!(look.revived, vec![format!("{TRASH_FOLDER}/{name}/saved-later.txt")]);
    }
}
