//! Walks a folder and records every item that isn't excluded, and every place it couldn't
//! read. Nothing goes unnoticed: a folder that can't be read is never mistaken for an
//! empty one (that would look like "everything in it was deleted").

use crate::error::{Error, Result};
use crate::scan::clients::ClientSettings;
use crate::scan::cloud::CloudFolders;
use crate::scan::volume::windows_name_problem;
use crate::scan::{
    is_charon_folder_in_other_case, is_claim, is_clutter, is_legacy_partial, is_partial, is_sync_journal, is_system_folder, CharonFolder, Entry, ExcludeFilter, Keys,
    Placeholder, RootId, Snapshot, Volume, MARKER_FOLDER, TRASH_FOLDER,
};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// How often (in items) a scan reports how far it got.
const COUNT_EVERY: u64 = 500;
/// How often a scan that sees items more slowly reports how far it got, at the least.
const COUNT_WITHIN: Duration = Duration::from_secs(5);

/// Reads folders into snapshots. Symbolic links are recorded as links, never followed.
pub struct Scanner<'a> {
    filter: &'a ExcludeFilter,
    /// The scan stops early as soon as one of these becomes true.
    stops: Vec<&'a AtomicBool>,
    counter: Option<&'a (dyn Fn(u64) + Sync)>,
    /// How often `counter` is called at the least, while items come (`COUNT_WITHIN`).
    count_within: Duration,
    /// Folders elsewhere whose drive the scan doesn't ask about (see `keeping_off`).
    keep_off: Option<&'a (dyn Fn(&Path) -> bool + Sync)>,
    /// Where the Nextcloud and ownCloud clients keep their settings.
    clients: ClientSettings,
}

impl<'a> Scanner<'a> {
    pub fn new(filter: &'a ExcludeFilter) -> Self {
        Self { filter, stops: Vec::new(), counter: None, count_within: COUNT_WITHIN, keep_off: None, clients: ClientSettings::here() }
    }

    /// Stops the scan early when `cancel` becomes true (any of them, when called again).
    pub fn cancellable(mut self, cancel: &'a AtomicBool) -> Self {
        self.stops.push(cancel);
        self
    }

    /// Calls `counter` with the number of items seen so far, now and then: every
    /// `COUNT_EVERY` items, and at least every few seconds while items come, then at the end
    /// (unless it just said that many).
    pub fn counting(mut self, counter: &'a (dyn Fn(u64) + Sync)) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Doesn't ask the drive of a folder elsewhere that `keep_off` names, when the scan would
    /// only ask it where that folder really is (a folder a sync client syncs, see `cloud`):
    /// it's taken as written. For a drive a sync keeps quiet (see `engine::quiet`).
    pub fn keeping_off(mut self, keep_off: &'a (dyn Fn(&Path) -> bool + Sync)) -> Self {
        self.keep_off = Some(keep_off);
        self
    }

    /// `counting`, at least every `within` (a test's own pace).
    #[cfg(test)]
    fn counting_within(mut self, within: Duration) -> Self {
        self.count_within = within;
        self
    }

    /// Whether one of the stop flags is set.
    fn stopped(&self) -> bool {
        self.stops.iter().any(|stop| stop.load(Ordering::Relaxed))
    }

    /// Every item under `root`, on the drive `volume`. Fails if `root` isn't reachable or
    /// can't be read. Places inside it that can't be read are recorded as frozen, so
    /// nothing there is touched.
    pub fn scan(&self, root: &Path, volume: Volume) -> Result<Snapshot> {
        if self.stopped() {
            return Err(Error::Cancelled);
        }
        Self::check_root(root)?;
        let root_id = RootId::of(root).map_err(|e| Error::Unreadable { path: root.to_path_buf(), reason: Self::io_reason(&e) })?;
        let mut snapshot = Snapshot::new();
        snapshot.facts_mut().root = root_id;
        let change_times = volume.keeps_change_times();
        snapshot.facts_mut().volume = Some(volume);
        let drive_top = Self::top_of_drive(root);
        let mut walker = WalkDir::new(root).follow_links(false).min_depth(1).into_iter();
        let mut seen: u64 = 0;
        // When the counter was last called, and with how many.
        let (mut counted, mut said) = (Instant::now(), 0);
        let mut cloud = CloudFolders::of(root, &self.clients, &|folder| self.keep_off.is_some_and(|keep_off| keep_off(folder)));
        // Files a cloud service freed up, by their keys, with why each is left alone.
        let mut freed_up: Vec<(String, String)> = Vec::new();

        while let Some(item) = walker.next() {
            if self.stopped() {
                return Err(Error::Cancelled);
            }
            seen += 1;
            if let Some(counter) = self.counter.filter(|_| seen.is_multiple_of(COUNT_EVERY) || counted.elapsed() >= self.count_within) {
                (counted, said) = (Instant::now(), seen);
                counter(seen);
            }
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    Self::note_unreadable(root, &error, &mut snapshot)?;
                    continue;
                }
            };
            let file_type = item.file_type();
            let Some(disk) = Self::relative(root, item.path()) else {
                // A name that isn't valid Unicode can't be named safely on the other side.
                // It stays where it is, and so does its folder.
                if file_type.is_dir() {
                    walker.skip_current_dir();
                }
                Self::leave_out_unnamed(root, item.path(), "its name isn't valid Unicode, so Charon can't carry it", &mut snapshot);
                continue;
            };
            let key = Keys::normalize(&disk);
            let name = Keys::name(&key);
            cloud.note(&disk, file_type.is_file());

            if item.depth() == 1 && is_system_folder(name) {
                if file_type.is_dir() {
                    walker.skip_current_dir();
                }
                // At the top of a drive it's the drive's own. Below the top it's the person's
                // own item, only named like one of the drive's folders: left alone too, and
                // said so.
                if drive_top {
                    snapshot.reserve(&key, &disk);
                } else {
                    snapshot.leave_out(disk, Some(&key), "named like a folder drives keep for themselves, so Charon leaves it alone");
                }
                continue;
            }
            if file_type.is_file() && is_sync_journal(name) {
                // A Nextcloud or ownCloud client's journal of the folder it syncs: the
                // client's own, never synced or removed, and its folder isn't empty.
                snapshot.reserve(&key, &disk);
                snapshot.occupy_folders_of(&key);
                continue;
            }
            let placeholder = Placeholder::named(name).filter(|placeholder| file_type.is_file() && cloud.is_placeholder(&disk, placeholder.service));
            if let Some(placeholder) = placeholder {
                // A cloud service's placeholder for a file it freed up: the service's own,
                // never synced, and its folder isn't empty. The file is the person's, only in
                // the cloud now, and unless it's excluded or here after all, it's left alone
                // on both sides (missing, it would look removed).
                let item = Self::freed_up(&key, placeholder);
                snapshot.reserve(&key, &disk);
                snapshot.occupy_folders_of(&key);
                if !self.filter.excludes(&item) {
                    let why = Self::freed_up_reason(&disk, cloud.journal_of(&disk, placeholder.service));
                    freed_up.push((item, why));
                }
                continue;
            }
            // When keys are folded, the filter matches the folded key: every spelling of a
            // name is excluded alike, on both sides.
            if self.filter.excludes(&key) {
                if file_type.is_dir() {
                    walker.skip_current_dir();
                }
                if ExcludeFilter::is_internal(&key, name) {
                    if is_partial(name) && file_type.is_file() {
                        if let Ok(meta) = item.metadata() {
                            // Its change time too: a copy being finished gets the original's
                            // modification time, which can be long ago.
                            let changed = if change_times { Self::change_time(&meta) } else { 0 };
                            snapshot.facts_mut().partials.push((disk.clone(), Entry::file(meta.len(), Self::modified_ms(&meta)).changed_at(changed)));
                        }
                        if is_legacy_partial(name) {
                            snapshot.leave_out(
                                disk,
                                Some(&key),
                                "named like a half-copied file of Charon 0.2, so Charon doesn't sync it (if it's yours, rename it)",
                            );
                        } else {
                            snapshot.reserve(&key, &disk);
                            snapshot.occupy_folders_of(&key);
                        }
                    } else if Self::is_link_leftover(name, file_type) {
                        // A link copy Charon was making, or a link it had moved aside to remove
                        // or replace it, when a sync was interrupted: Charon's own, tidied
                        // like a half-copied file.
                        if let Some(link) = Self::link_of(item.path()) {
                            snapshot.facts_mut().partials.push((disk.clone(), link));
                        }
                        snapshot.reserve(&key, &disk);
                        snapshot.occupy_folders_of(&key);
                    } else if is_partial(name) {
                        // A folder only named like a file Charon is copying, or a link named
                        // like one of Charon 0.2's (which never carried links).
                        snapshot.leave_out(disk, Some(&key), "named like a file Charon is copying, so Charon leaves it alone (if it's yours, rename it)");
                    } else if (name == MARKER_FOLDER || name == TRASH_FOLDER) && !Self::looks_like_charons(item.path(), name, file_type.is_dir()) {
                        // The person's own item, only named like one of Charon's folders:
                        // left alone, and said so.
                        snapshot.leave_out(disk, Some(&key), "named like one of Charon's own folders, so Charon leaves it alone");
                    } else {
                        snapshot.reserve(&key, &disk);
                        if name == MARKER_FOLDER {
                            Self::note_charon_folder(item.path(), &disk, &mut snapshot);
                        }
                        if Keys::parent(&key).is_some() {
                            // Another job's trash or marker inside this folder: never removed.
                            snapshot.occupy_folders_of(&key);
                        }
                    }
                } else if !is_clutter(name) {
                    snapshot.occupy_folders_of(&key);
                }
                continue;
            }
            if self.filter.folds() && is_charon_folder_in_other_case(name) {
                // Where a drive ignores case, Charon's own folder is this item, and on the
                // other side its key is that folder's: synced, the other side's copy would
                // become Charon's folder there. Left alone on both sides, and said so.
                if file_type.is_dir() {
                    walker.skip_current_dir();
                }
                snapshot.leave_out(
                    disk,
                    Some(&key),
                    "named like one of Charon's own folders, only in other upper and lower case (one of the drives doesn't tell them apart), so Charon leaves it alone",
                );
                continue;
            }
            if cfg!(windows) {
                if let Some(reason) = windows_name_problem(Keys::name(&disk)) {
                    if file_type.is_dir() {
                        walker.skip_current_dir();
                    }
                    snapshot.leave_out(disk, Some(&key), format!("{reason}, which Windows can't handle safely"));
                    continue;
                }
            }

            if file_type.is_dir() {
                if let Some(reason) = Self::other_drive(&item, root_id).or_else(|| Self::only_in_the_cloud(item.path())) {
                    // A drive or share mounted inside the folder (if it's ever unmounted, its
                    // folder looks empty), or a folder whose contents aren't downloaded.
                    walker.skip_current_dir();
                    snapshot.freeze(&key, reason);
                    continue;
                }
                Self::add(&mut snapshot, key, disk, Entry::dir());
            } else if file_type.is_file() {
                match Self::file_metadata(&item) {
                    Ok(meta) => match Self::placeholder(&meta) {
                        Some(reason) => snapshot.freeze(&key, reason),
                        None => {
                            let changed = if change_times { Self::change_time(&meta) } else { 0 };
                            let entry = Entry::file(meta.len(), Self::modified_ms(&meta)).changed_at(changed);
                            if is_claim(&entry) {
                                // An empty file that held this name for an interrupted sync: a
                                // leftover of Charon's, like a half-copied file.
                                snapshot.facts_mut().partials.push((disk, entry));
                                snapshot.occupy_folders_of(&key);
                            } else {
                                Self::add(&mut snapshot, key, disk, entry)
                            }
                        }
                    },
                    Err(e) => snapshot.freeze(&key, Self::io_reason(&e)),
                }
            } else if file_type.is_symlink() {
                Self::add_link(&mut snapshot, item.path(), key, disk);
            } else {
                snapshot.leave_out(disk, Some(&key), "isn't a file or a folder (a socket, pipe or device)");
            }
        }
        for (item, why) in freed_up {
            if !snapshot.contains(&item) && !snapshot.leaves_alone(&item) {
                snapshot.freeze(&item, why);
            }
        }
        snapshot.facts_mut().clouds = cloud.places();
        if let Some(counter) = self.counter.filter(|_| seen == 0 || said != seen) {
            counter(seen);
        }
        Ok(snapshot)
    }

    /// The key of the file a cloud service freed up, when the file at `key` is the
    /// `placeholder` it left in its place.
    fn freed_up(key: &str, placeholder: Placeholder) -> String {
        match Keys::parent(key) {
            Some(folder) => format!("{folder}/{}", placeholder.item),
            None => placeholder.item.to_string(),
        }
    }

    /// Why a file a cloud service freed up is left alone, when only its placeholder at `disk`
    /// is here. Where only a sync client's `journal` says a client syncs the folder, the note
    /// names it: in a copy of a synced folder (a backup made by a tool that copied it as it
    /// was), the journal and the placeholder are both old copies.
    fn freed_up_reason(disk: &str, journal: Option<&str>) -> String {
        let placeholder = Keys::name(disk);
        match journal {
            Some(journal) => format!(
                "it's only in the cloud (only its placeholder “{placeholder}” is here, inside a folder that holds a Nextcloud or ownCloud client's journal, “{journal}”), so Charon leaves it alone (if no such client on any computer syncs that folder, the journal and the placeholder are old copies: remove them)"
            ),
            None => format!("it's only in the cloud (only its placeholder “{placeholder}” is on this computer), so Charon leaves it alone (if that placeholder is an old copy, remove it)"),
        }
    }

    /// A file's size and time. On Windows they are read from the file itself, the way the
    /// sync checks them later (the folder listing can lag behind a file being written).
    fn file_metadata(item: &walkdir::DirEntry) -> std::io::Result<fs::Metadata> {
        if cfg!(windows) {
            fs::symlink_metadata(item.path())
        } else {
            item.metadata().map_err(|e| e.into_io_error().unwrap_or_else(|| std::io::Error::other("couldn't read it")))
        }
    }

    /// Why the file is left alone if it's only a placeholder for a file in the cloud
    /// (iCloud Drive, OneDrive, Dropbox): reading it would download it, and its size and
    /// time say nothing about its contents here.
    pub(crate) fn placeholder(meta: &fs::Metadata) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            const SF_DATALESS: u32 = 0x4000_0000;
            if meta.st_flags() & SF_DATALESS != 0 {
                return Some(Self::CLOUD.into());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN};
            if meta.file_attributes() & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE) != 0 {
                return Some(Self::CLOUD.into());
            }
        }
        let _ = meta;
        None
    }

    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    const CLOUD: &'static str = "it's only in the cloud (not downloaded to this computer), so Charon leaves it alone";

    /// A folder whose contents are still in the cloud.
    fn only_in_the_cloud(path: &Path) -> Option<String> {
        if cfg!(any(target_os = "macos", windows)) {
            fs::symlink_metadata(path).ok().and_then(|meta| Self::placeholder(&meta))
        } else {
            None
        }
    }

    /// Whether an item named `.charon` or `.charon-trash` really is one of Charon's: a
    /// folder holding only Charon's own files (see `Marker::is_charons_file`), or only dated
    /// trash folders (or nothing). The files the system scatters into every folder it shows
    /// (Finder's `.DS_Store`, Explorer's `desktop.ini`) don't count.
    pub(crate) fn looks_like_charons(path: &Path, name: &str, is_dir: bool) -> bool {
        if !is_dir {
            return false;
        }
        let Ok(items) = fs::read_dir(path) else { return false };
        items.into_iter().all(|item| {
            let Ok(item) = item else { return false };
            let (Ok(kind), Some(inner)) = (item.file_type(), item.file_name().to_str().map(str::to_owned)) else { return false };
            if kind.is_file() && is_clutter(&inner) {
                true
            } else if name == MARKER_FOLDER {
                kind.is_file() && crate::run::Marker::is_charons_file(&inner)
            } else {
                kind.is_dir() && crate::run::Trash::is_run_folder(&inner)
            }
        })
    }

    /// Notes Charon's own folder at `path` (`disk` is that folder as spelled on disk), with
    /// the jobs whose markers it holds: each one is a job whose folder is the one holding it.
    fn note_charon_folder(path: &Path, disk: &str, snapshot: &mut Snapshot) {
        let Some(holder) = path.parent() else { return };
        let folder = Keys::parent(disk).unwrap_or("").to_string();
        let jobs = crate::run::Marker::jobs_in(holder);
        snapshot.facts_mut().charon_folders.push(CharonFolder { folder, jobs });
    }

    /// Why the folder `item` must not be entered: it's on another drive than the scanned
    /// folder, or Charon can't tell.
    #[cfg(unix)]
    fn other_drive(item: &walkdir::DirEntry, root: Option<RootId>) -> Option<String> {
        let root = root?;
        match item.metadata() {
            Ok(meta) if RootId::device_of(&meta) == root.device() => None,
            Ok(_) => Some("another drive or share is mounted here; Charon doesn't cross into it".into()),
            Err(e) => Some(Self::reason(&e)),
        }
    }

    /// Windows mounts drives inside folders as junctions, which are left out as links.
    #[cfg(not(unix))]
    fn other_drive(_item: &walkdir::DirEntry, _root: Option<RootId>) -> Option<String> {
        None
    }

    /// Whether `root` is the top folder of its drive, where the drive keeps folders of its
    /// own (a recycle bin, an index, `lost+found`). When that can't be told, it counts as
    /// the top: such folders are left alone either way, only not mentioned.
    #[cfg(unix)]
    fn top_of_drive(root: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(real) = fs::canonicalize(root) else { return true };
        let Some(parent) = real.parent() else { return true };
        match (fs::metadata(&real), fs::metadata(parent)) {
            (Ok(here), Ok(above)) => here.dev() != above.dev(),
            _ => true,
        }
    }

    #[cfg(not(unix))]
    fn top_of_drive(root: &Path) -> bool {
        fs::canonicalize(root).ok().is_none_or(|real| real.parent().is_none())
    }

    /// The folder must be there and readable.
    fn check_root(root: &Path) -> Result<()> {
        match fs::metadata(root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(Error::Offline(root.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::Offline(root.to_path_buf())),
            Err(e) => return Err(Error::Unreadable { path: root.to_path_buf(), reason: Self::io_reason(&e) }),
        }
        fs::read_dir(root)
            .map(|_| ())
            .map_err(|e| Error::Unreadable { path: root.to_path_buf(), reason: Self::io_reason(&e) })
    }

    /// Records an item, or freezes its name when another item already has the same key
    /// (two spellings of one name in one folder, possible on some drives).
    fn add(snapshot: &mut Snapshot, key: String, disk: String, entry: Entry) {
        if snapshot.contains(&key) {
            snapshot.freeze(&key, "two items here have the same name written in different ways (Unicode forms), so Charon leaves both alone");
            snapshot.facts_mut().clashes.insert(key);
            return;
        }
        if disk != key {
            snapshot.facts_mut().spellings.insert(key.clone(), disk);
        }
        snapshot.insert(key, entry);
    }

    /// Whether an item is a link under one of Charon 0.3's temporary names: Charon makes
    /// link copies under such a name, and moves links aside under one before it removes or
    /// replaces them. Only on macOS and Linux, where Charon carries links.
    fn is_link_leftover(name: &str, file_type: fs::FileType) -> bool {
        cfg!(unix) && file_type.is_symlink() && is_partial(name) && !is_legacy_partial(name)
    }

    /// The link at `path`, as the look right before it's tidied away sees it.
    #[cfg(unix)]
    fn link_of(path: &Path) -> Option<Entry> {
        use std::os::unix::ffi::OsStrExt;
        let target = fs::read_link(path).ok()?;
        let meta = fs::symlink_metadata(path).ok()?;
        Some(Entry::link(target.as_os_str().as_bytes(), Self::modified_ms(&meta)))
    }

    #[cfg(not(unix))]
    fn link_of(_path: &Path) -> Option<Entry> {
        None
    }

    /// Links are carried as links on macOS and Linux. Windows links and junctions are left
    /// alone (creating them needs special rights there).
    #[cfg(unix)]
    fn add_link(snapshot: &mut Snapshot, path: &Path, key: String, disk: String) {
        use std::os::unix::ffi::OsStrExt;
        match (fs::read_link(path), fs::symlink_metadata(path)) {
            (Ok(target), Ok(meta)) => {
                Self::add(snapshot, key, disk, Entry::link(target.as_os_str().as_bytes(), Self::modified_ms(&meta)))
            }
            (Err(e), _) | (_, Err(e)) => snapshot.freeze(&key, Self::io_reason(&e)),
        }
    }

    #[cfg(not(unix))]
    fn add_link(snapshot: &mut Snapshot, _path: &Path, key: String, disk: String) {
        snapshot.leave_out(disk, Some(&key), "is a link or junction; Charon leaves those alone on Windows");
    }

    /// An item that can't be named: noted, and its folder is kept.
    fn leave_out_unnamed(root: &Path, path: &Path, reason: &str, snapshot: &mut Snapshot) {
        let shown = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
        snapshot.leave_out(shown, None, reason);
        let parent = path.parent().and_then(|p| Self::relative(root, p)).map(|p| Keys::normalize(&p));
        if let Some(parent) = parent {
            snapshot.occupy(&parent);
        }
    }

    /// A place the walk couldn't read. Inside the folder it's frozen; the folder itself
    /// failing (or an error without a place) fails the whole scan.
    fn note_unreadable(root: &Path, error: &walkdir::Error, snapshot: &mut Snapshot) -> Result<()> {
        let reason = Self::reason(error);
        let place = error.path().filter(|path| *path != root).and_then(|path| Self::relative(root, path));
        match place {
            Some(disk) => {
                snapshot.freeze(&Keys::normalize(&disk), reason);
                Ok(())
            }
            None => Err(Error::Unreadable { path: root.to_path_buf(), reason }),
        }
    }

    /// `path` relative to `root`, with `/` separators on every system, exactly as spelled
    /// on disk. `None` if a part of the name isn't valid Unicode.
    pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(root).ok()?;
        let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
        let parts = parts?;
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// A file's change time in nanoseconds since 1970 (macOS and Linux), 0 where unknown.
    pub(crate) fn change_time(meta: &fs::Metadata) -> i64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.ctime().saturating_mul(1_000_000_000).saturating_add(meta.ctime_nsec())
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            0
        }
    }

    pub(crate) fn modified_ms(meta: &fs::Metadata) -> i64 {
        meta.modified().map(Self::millis).unwrap_or(0)
    }

    /// Milliseconds since 1970 (negative before).
    pub(crate) fn millis(time: SystemTime) -> i64 {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_millis() as i64,
            Err(before) => -(before.duration().as_millis() as i64),
        }
    }

    fn reason(error: &walkdir::Error) -> String {
        error.io_error().map(Self::io_reason).unwrap_or_else(|| error.to_string())
    }

    /// "permission denied" rather than "Permission denied (os error 13)". On macOS, a
    /// refusal by the privacy settings says where to allow it.
    pub(crate) fn io_reason(error: &std::io::Error) -> String {
        #[cfg(target_os = "macos")]
        match error.raw_os_error() {
            Some(libc::EPERM) => return PRIVACY_REFUSAL.into(),
            // A cloud file or folder Charon declined to download (see `Volume`).
            Some(libc::EDEADLK) => return Self::CLOUD.into(),
            _ => {}
        }
        let text = error.to_string();
        let text = text.split(" (os error").next().unwrap_or(&text).to_string();
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_lowercase().chain(chars).collect(),
            None => text,
        }
    }
}

/// The reason given for places macOS's privacy settings keep Charon out of.
pub const PRIVACY_REFUSAL: &str =
    "macOS didn't let Charon read this. Allow Charon in System Settings › Privacy & Security › Full Disk Access";

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The snapshot of `root`, without the patterns, where no sync client's settings can be
    /// read.
    fn scan(root: &Path, patterns: &[&str]) -> Snapshot {
        scan_with(root, patterns, ClientSettings::at(Vec::new()))
    }

    fn scan_with(root: &Path, patterns: &[&str], clients: ClientSettings) -> Snapshot {
        let filter = ExcludeFilter::new(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>(), false).unwrap();
        Scanner { clients, ..Scanner::new(&filter) }.scan(root, Volume::of(root)).unwrap()
    }

    #[test]
    fn names_windows_cannot_hold() {
        assert!(windows_name_problem("notes.").is_some());
        assert!(windows_name_problem("notes ").is_some());
        assert!(windows_name_problem("con").is_some());
        assert!(windows_name_problem("NUL.txt").is_some());
        assert!(windows_name_problem("com1.log").is_some());
        assert!(windows_name_problem("console.txt").is_none());
        assert!(windows_name_problem("com.txt").is_none());
        assert!(windows_name_problem(".env").is_none());
    }

    /// Ids of jobs, and of a note that a sync is at work, as Charon makes them.
    const MINE: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";
    const NOTE: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn the_persons_own_items_named_like_charons_folders_are_noted_not_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        // Charon's own: the marker folder at the top, and another job's inside.
        write(&tmp.path().join(format!(".charon/{MINE}.json")), "{}");
        write(&tmp.path().join(format!("inner/.charon/{OTHER}.json")), "{}");
        write(&tmp.path().join("inner/.charon-trash/2026-01-02_03-04-05/old.txt"), "x");
        // The person's own, whatever the names of the files in them end with.
        write(&tmp.path().join("projects/app/.charon/settings.toml"), "x");
        write(&tmp.path().join("projects/web/.charon/settings.json"), "{}");
        write(&tmp.path().join("projects/web/.charon/cache.tmp"), "x");
        write(&tmp.path().join("notes/.charon-trash"), "a file");
        let s = scan(tmp.path(), &[]);
        let mut noted: Vec<&str> = s.skipped().iter().map(|skipped| skipped.path.as_str()).collect();
        noted.sort();
        assert_eq!(noted, vec!["notes/.charon-trash", "projects/app/.charon", "projects/web/.charon"]);
        assert!(s.is_occupied("projects/app") && s.is_occupied("inner"));
        assert!(!s.contains("projects/app/.charon/settings.toml"));
        let mut met: Vec<&str> = s.charon_folders().iter().map(|c| c.folder.as_str()).collect();
        met.sort();
        assert_eq!(met, vec!["", "inner"]);
    }

    #[test]
    fn charons_folders_met_are_noted_with_their_markers() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(format!(".charon/{MINE}.json")), "{}");
        write(&tmp.path().join(format!("Work/Inner/.charon/{OTHER}.json")), "{}");
        write(&tmp.path().join(format!("Busy/.charon/{NOTE}.busy")), "{}");
        write(&tmp.path().join("notes/.charon/settings.toml"), "the person's own");
        let s = scan(tmp.path(), &[]);
        let mut seen: Vec<(&str, Vec<String>)> = s.charon_folders().iter().map(|c| (c.folder.as_str(), c.jobs.clone())).collect();
        seen.sort();
        assert_eq!(seen, vec![("", vec![MINE.to_string()]), ("Busy", vec![]), ("Work/Inner", vec![OTHER.to_string()])]);
        assert!(s.is_occupied("Busy"), "a folder holding Charon's own folder is never removed");
    }

    /// Finder leaves a `.DS_Store` in every folder it shows, Charon's trash too when the
    /// person looks for a removed file there. That doesn't make it the person's folder.
    #[test]
    fn the_systems_clutter_in_charons_folders_leaves_them_charons() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(format!(".charon/{MINE}.json")), "{}");
        write(&tmp.path().join(format!(".charon/.{MINE}.json.4711-3.tmp")), "{}");
        write(&tmp.path().join(format!(".charon/{NOTE}.busy")), "{}");
        write(&tmp.path().join(format!(".charon/._{NOTE}.busy")), "x");
        write(&tmp.path().join(".charon/.DS_Store"), "x");
        write(&tmp.path().join(".charon-trash/2026-01-02_03-04-05/old.txt"), "x");
        write(&tmp.path().join(".charon-trash/.DS_Store"), "x");
        write(&tmp.path().join(".charon-trash/desktop.ini"), "x");
        write(&tmp.path().join(format!("inner/.charon/{OTHER}.json")), "{}");
        write(&tmp.path().join("inner/.charon/Thumbs.db"), "x");
        let s = scan(tmp.path(), &[]);
        assert!(s.skipped().is_empty(), "{:?}", s.skipped());
        let reserved: Vec<&str> = s.reserved().map(|(key, _)| key.as_str()).collect();
        assert_eq!(reserved, vec![".charon", ".charon-trash", "inner/.charon"]);
        let mut seen: Vec<(&str, Vec<String>)> = s.charon_folders().iter().map(|c| (c.folder.as_str(), c.jobs.clone())).collect();
        seen.sort();
        assert_eq!(seen, vec![("", vec![MINE.to_string()]), ("inner", vec![OTHER.to_string()])]);
    }

    #[test]
    fn a_drives_own_folders_at_the_top_are_never_looked_at() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("System Volume Information/x.dat"), "x");
        write(&tmp.path().join("photos/lost+found/kept.txt"), "x");
        let s = scan(tmp.path(), &[]);
        assert!(s.get("System Volume Information").is_none());
        assert!(s.get("photos/lost+found/kept.txt").is_some(), "only at the top");
    }

    #[test]
    fn a_persons_own_folders_named_like_a_drives_are_left_alone_and_mentioned() {
        // A temporary folder is never the top of its drive.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("lost+found/notes.txt"), "x");
        write(&tmp.path().join(".TemporaryItems/draft.txt"), "x");
        let s = scan(tmp.path(), &[]);
        assert!(s.get("lost+found/notes.txt").is_none() && s.get(".TemporaryItems").is_none(), "never touched");
        let mut noted: Vec<&str> = s.skipped().iter().map(|skipped| skipped.path.as_str()).collect();
        noted.sort();
        assert_eq!(noted, vec![".TemporaryItems", "lost+found"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_top_of_a_drive_is_told_apart_from_a_folder_on_it() {
        assert!(Scanner::top_of_drive(Path::new("/")));
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Scanner::top_of_drive(tmp.path()));
    }

    #[test]
    fn counts_as_it_goes() {
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..1200 {
            write(&tmp.path().join(format!("f{n}")), "x");
        }
        let seen = std::sync::Mutex::new(Vec::new());
        let counter = |n: u64| seen.lock().unwrap().push(n);
        let filter = ExcludeFilter::none();
        Scanner::new(&filter).counting(&counter).scan(tmp.path(), Volume::of(tmp.path())).unwrap();
        // (A slow machine may count in between too.)
        let seen = seen.into_inner().unwrap();
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]) && seen.contains(&500) && seen.contains(&1000), "{seen:?}");
        assert_eq!(seen.last(), Some(&1200));
        // Seeing items slowly, it counts by the time that went by too, and says the end once.
        let seen = std::sync::Mutex::new(Vec::new());
        let counter = |n: u64| seen.lock().unwrap().push(n);
        Scanner::new(&filter).counting(&counter).counting_within(Duration::ZERO).scan(tmp.path(), Volume::of(tmp.path())).unwrap();
        assert_eq!(seen.into_inner().unwrap(), (1..=1200).collect::<Vec<u64>>());
    }

    #[test]
    fn another_jobs_trash_inside_keeps_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("inner/.charon-trash/2026-01-01_00-00-00/a.txt"), "x");
        write(&tmp.path().join(".charon-trash/2026-01-01_00-00-00/b.txt"), "x");
        let s = scan(tmp.path(), &[]);
        assert!(s.is_occupied("inner"));
        assert!(s.get("inner/.charon-trash").is_none());
        assert!(s.get(".charon-trash").is_none());
    }

    /// Nextcloud and ownCloud (and iCloud Drive before macOS 14, see `cloud`) leave a
    /// placeholder in place of a file they free up. Missing from the scan, the file would look
    /// removed, and the other side's copy would be removed too.
    #[test]
    fn a_file_a_cloud_service_freed_up_is_left_alone_not_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // The client's journal, at the top of the folder it syncs.
        write(&tmp.path().join("Nextcloud/.sync_0123456789ab.db"), "journal");
        let root = tmp.path().join("Nextcloud/Work");
        write(&root.join("Docs/Report.pdf.nextcloud"), " ");
        write(&root.join("Plans.odt.nextcloud"), " ");
        // Named like what the patterns leave out, a placeholder is still one.
        write(&root.join("Docs/Budget.xlsx.owncloud"), " ");
        // Downloaded again: the file is here, and its placeholder is a leftover.
        write(&root.join("Docs/Back.pdf.nextcloud"), " ");
        write(&root.join("Docs/Back.pdf"), "the file");
        // Excluded: left alone by the patterns, like its placeholder.
        write(&root.join("Docs/draft.tmp.nextcloud"), " ");
        let s = scan(&root, &["*.tmp", "*.owncloud"]);
        let mut frozen: Vec<&str> = s.frozen_places().map(|(key, _)| key.as_str()).collect();
        frozen.sort();
        assert_eq!(frozen, vec!["Docs/Budget.xlsx", "Docs/Report.pdf", "Plans.odt"]);
        let why = s.frozen("Docs/Report.pdf").unwrap();
        assert!(why.contains("only in the cloud") && why.contains("“Report.pdf.nextcloud”") && why.contains("“.sync_0123456789ab.db”") && why.contains("remove them"), "{why}");
        assert!(s.get("Docs/Back.pdf").is_some());
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["Docs", "Docs/Back.pdf"], "placeholders are never synced");
        assert!(s.leaves_alone("Docs/Report.pdf.nextcloud") && s.leaves_alone("Docs/Back.pdf.nextcloud") && s.leaves_alone("Docs/draft.tmp.nextcloud"));
        assert!(s.is_occupied("Docs"), "a folder holding a placeholder isn't empty");
        assert!(s.skipped().is_empty());
        assert_eq!(s.placeholder_service("Docs/New.txt.nextcloud"), Some(crate::scan::CloudService::SyncClient), "nothing named so is made here");
        assert_eq!(s.placeholder_service("Docs/New.txt"), None);
    }

    /// A copy of a placeholder where its cloud service keeps no files (a backup made by a tool
    /// that copied a cloud folder as it was) is an ordinary file: it never keeps the file it
    /// names from being synced there.
    #[test]
    fn a_copy_of_a_placeholder_elsewhere_is_an_ordinary_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Nextcloud/.sync_0123456789ab.db"), "journal");
        write(&tmp.path().join("Nextcloud/Docs/Report.pdf.nextcloud"), " ");
        write(&tmp.path().join("Backup/Docs/Report.pdf.nextcloud"), " ");
        write(&tmp.path().join("Backup/Docs/.Plans.key.icloud"), "placeholder");
        // An ownCloud client's journal, with no placeholders in its folder yet.
        write(&tmp.path().join("ownCloud/._sync_ba9876543210.db"), "journal");
        let s = scan(&tmp.path().join("Backup"), &[]);
        assert_eq!(s.frozen_places().count(), 0);
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["Docs", "Docs/.Plans.key.icloud", "Docs/Report.pdf.nextcloud"]);
        assert_eq!(s.placeholder_service("Docs/New.txt.nextcloud"), None);
        assert_eq!(s.placeholder_service("Docs/.New.txt.icloud"), None);

        // Scanned from above, each is told by its own folder.
        let s = scan(tmp.path(), &[]);
        assert_eq!(s.frozen_places().map(|(key, _)| key.as_str()).collect::<Vec<_>>(), vec!["Nextcloud/Docs/Report.pdf"]);
        assert!(s.get("Backup/Docs/Report.pdf.nextcloud").is_some() && s.get("Nextcloud/Docs/Report.pdf.nextcloud").is_none());
        assert!(s.placeholder_service("Nextcloud/New.txt.nextcloud").is_some() && s.placeholder_service("Backup/New.txt.nextcloud").is_none());
        assert!(s.placeholder_service("ownCloud/Docs/New.txt.owncloud").is_some());
    }

    /// Where the sync clients' settings can be read, a folder they don't list but that holds a
    /// client's journal is still one a client syncs: a client on another computer may sync
    /// it. Charon can't tell it from a copy of a synced folder (a backup made by a tool that
    /// copied it as it was), so the note on a file only its placeholder stands for names the
    /// journal: if no client syncs that folder, both are old copies. In a folder the settings
    /// list, the note needs no journal.
    #[test]
    fn a_journal_in_a_folder_the_settings_dont_list_is_named_in_the_note() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings/nextcloud.cfg");
        let synced = tmp.path().join("Nextcloud");
        write(&settings, &format!("[Accounts]\n0\\Folders\\1\\localPath={}/\n", synced.display().to_string().replace('\\', "/")));
        for folder in [&synced, &tmp.path().join("Backup")] {
            write(&folder.join(".sync_0123456789ab.db"), "journal");
            write(&folder.join("Docs/Report.pdf.nextcloud"), " ");
        }
        let clients = || ClientSettings::at(vec![settings.clone()]);

        let s = scan_with(&tmp.path().join("Backup"), &[], clients());
        assert_eq!(s.frozen_places().map(|(key, _)| key.as_str()).collect::<Vec<_>>(), vec!["Docs/Report.pdf"]);
        let why = s.frozen("Docs/Report.pdf").unwrap();
        assert!(why.contains("“Report.pdf.nextcloud”") && why.contains("“.sync_0123456789ab.db”") && why.contains("old copies: remove them"), "{why}");
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["Docs"]);
        assert!(s.placeholder_service("Docs/New.txt.nextcloud").is_some(), "nothing named so is made there");
        let s = scan_with(&synced, &[], clients());
        assert_eq!(s.frozen_places().map(|(key, _)| key.as_str()).collect::<Vec<_>>(), vec!["Docs/Report.pdf"]);
        let why = s.frozen("Docs/Report.pdf").unwrap();
        assert!(why.contains("“Report.pdf.nextcloud”") && why.contains("remove it") && !why.contains("journal"), "{why}");
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["Docs"]);
    }

    /// While a sync keeps a drive quiet, the scan doesn't ask it where a folder a client
    /// syncs really is (see `keeping_off`): the folder is taken as written, so one written
    /// through a link isn't known for the scanned one.
    #[cfg(unix)]
    #[test]
    fn a_folder_a_client_syncs_is_taken_as_written_where_its_drive_isnt_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let (synced, link) = (tmp.path().join("Nextcloud"), tmp.path().join("Link"));
        fs::create_dir_all(&synced).unwrap();
        std::os::unix::fs::symlink(&synced, &link).unwrap();
        let settings = tmp.path().join("settings/nextcloud.cfg");
        write(&settings, &format!("[Accounts]\n0\\Folders\\1\\localPath={}/\n", link.display()));
        let filter = ExcludeFilter::new(&[], false).unwrap();
        let scan = |keep_off: &(dyn Fn(&Path) -> bool + Sync)| {
            Scanner { clients: ClientSettings::at(vec![settings.clone()]), ..Scanner::new(&filter) }.keeping_off(keep_off).scan(&synced, Volume::of(&synced)).unwrap()
        };
        assert!(scan(&|_| false).placeholder_service("New.txt.nextcloud").is_some(), "the link is followed");
        assert!(scan(&|folder| folder.starts_with(&link)).placeholder_service("New.txt.nextcloud").is_none());
    }

    /// A Nextcloud or ownCloud client's journal, and the files that go with it, are the
    /// client's own wherever they are: never synced, so they never make the other side look
    /// synced by the client, and never removed.
    #[test]
    fn a_sync_clients_journal_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let journals = [
            ".sync_0123456789ab.db",
            ".sync_0123456789ab.db-wal",
            ".sync_0123456789ab.db-shm",
            "Old/.csync_journal.db",
            "Old/._sync_ba9876543210.db-journal",
        ];
        for journal in journals {
            write(&tmp.path().join(journal), "journal");
        }
        write(&tmp.path().join("Docs/notes.txt"), "notes");
        // Only named like a journal: a folder.
        fs::create_dir_all(tmp.path().join("Docs/.sync_0123456789ab.db")).unwrap();
        let s = scan(tmp.path(), &[]);
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["Docs", "Docs/.sync_0123456789ab.db", "Docs/notes.txt", "Old"]);
        let mut reserved: Vec<&str> = s.reserved().map(|(key, _)| key.as_str()).collect();
        reserved.sort();
        let mut expected = journals.to_vec();
        expected.sort();
        assert_eq!(reserved, expected);
        assert!(s.is_occupied("Old"), "a folder holding a journal isn't empty");
        assert!(s.skipped().is_empty());
    }

    #[test]
    fn records_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a/b.txt"), "hi");
        let s = scan(tmp.path(), &[]);
        assert!(s.get("a").unwrap().is_dir());
        assert_eq!(s.get("a/b.txt").unwrap().size, 2);
    }

    #[test]
    fn a_missing_folder_is_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let filter = ExcludeFilter::none();
        let gone = tmp.path().join("gone");
        assert!(matches!(Scanner::new(&filter).scan(&gone, Volume::of(&gone)), Err(Error::Offline(_))));
    }

    #[test]
    fn excluded_items_keep_their_folders_but_clutter_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("work/notes.tmp"), "x");
        write(&tmp.path().join("work/deep/more.tmp"), "x");
        write(&tmp.path().join("photos/.DS_Store"), "x");
        let s = scan(tmp.path(), &["*.tmp", ".DS_Store"]);
        assert!(s.get("work/notes.tmp").is_none());
        assert!(s.is_occupied("work") && s.is_occupied("work/deep"));
        assert!(!s.is_occupied("photos"), ".DS_Store alone never keeps a folder");
    }

    #[test]
    fn leftover_partial_files_are_noticed() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a/big.mov.charon-partial"), "half");
        write(&tmp.path().join("a/.charon-00112233445566ff.part"), "half");
        let s = scan(tmp.path(), &[]);
        assert_eq!(s.partials().len(), 2);
        assert!(s.get("a/big.mov.charon-partial").is_none());
        assert!(s.is_occupied("a"), "a folder with a leftover isn't empty");
    }

    #[test]
    fn an_empty_file_left_holding_a_name_is_a_leftover_not_an_item() {
        let tmp = tempfile::tempdir().unwrap();
        let claim = tmp.path().join("a/report.txt");
        write(&claim, "");
        filetime::set_file_mtime(&claim, filetime::FileTime::from_unix_time(crate::scan::CLAIM_MODIFIED_MS / 1000, 0)).unwrap();
        write(&tmp.path().join("a/empty.txt"), "");
        let s = scan(tmp.path(), &[]);
        assert!(s.get("a/report.txt").is_none(), "never taken for the person's new file");
        assert_eq!(s.partials().iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(), vec!["a/report.txt"]);
        assert!(s.get("a/empty.txt").is_some(), "an empty file of the person's own is an item");
        assert!(s.is_occupied("a"));
    }

    #[test]
    fn items_only_named_like_half_copied_files_are_noted() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a/big.mov.charon-partial"), "maybe mine");
        write(&tmp.path().join("a/.charon-00112233445566ff.part"), "half");
        write(&tmp.path().join("b.charon-partial/inside.txt"), "x");
        let s = scan(tmp.path(), &[]);
        let mut noted: Vec<&str> = s.skipped().iter().map(|skipped| skipped.path.as_str()).collect();
        noted.sort();
        // Charon 0.3's own temporary names are its own; the others may be the person's.
        assert_eq!(noted, vec!["a/big.mov.charon-partial", "b.charon-partial"]);
        assert!(s.skipped().iter().all(|skipped| skipped.reason.contains("rename it")));
        assert_eq!(s.partials().len(), 2, "only files count as leftovers");
        assert!(s.get("b.charon-partial/inside.txt").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn charons_own_leftover_links_are_tidied_like_half_copied_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Left by a sync interrupted while it made a link copy, or removed a link.
        write(&tmp.path().join("a/keep.txt"), "x");
        std::os::unix::fs::symlink("../elsewhere", tmp.path().join("a/.charon-0011223344556677.part")).unwrap();
        // Charon 0.2 never carried links: a link named like its half-copied files isn't its own.
        std::os::unix::fs::symlink("notes.txt", tmp.path().join("notes.charon-partial")).unwrap();
        let s = scan(tmp.path(), &[]);
        let leftovers: Vec<&str> = s.partials().iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(leftovers, vec!["a/.charon-0011223344556677.part"]);
        assert!(s.partials()[0].1.is_link());
        let noted: Vec<&str> = s.skipped().iter().map(|skipped| skipped.path.as_str()).collect();
        assert_eq!(noted, vec!["notes.charon-partial"], "Charon's own link isn't told to the person as theirs");
        assert!(s.is_occupied("a"), "its folder isn't empty until it's tidied away");
    }

    #[cfg(unix)]
    #[test]
    fn leftovers_carry_their_change_time() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".charon-00112233445566ff.part"), "half");
        // A copy being finished gets the original's old modification time.
        filetime::set_file_mtime(tmp.path().join(".charon-00112233445566ff.part"), filetime::FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let s = scan(tmp.path(), &[]);
        let (_, entry) = &s.partials()[0];
        if Volume::of(tmp.path()).keeps_change_times() {
            assert!(entry.changed / 1_000_000 > entry.modified, "the change time tells it was written just now");
        }
    }

    #[test]
    fn patterns_match_names_in_either_unicode_form() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Re\u{301}sume\u{301}.pdf"), "x");
        write(&tmp.path().join("Notes.pdf"), "x");
        let s = scan(tmp.path(), &["R\u{e9}sum\u{e9}*"]);
        assert!(s.get("R\u{e9}sum\u{e9}.pdf").is_none(), "a name stored decomposed is excluded by a pattern typed composed");
        assert!(s.get("Notes.pdf").is_some());
        let s = scan(tmp.path(), &["Re\u{301}sume\u{301}*"]);
        assert!(s.get("R\u{e9}sum\u{e9}.pdf").is_none(), "and the other way round");
    }

    /// On a drive that tells case apart, "ENTWÜRFE" is a folder of its own; when the other
    /// drive ignores case, it is the excluded "Entwürfe" there, and must be excluded here too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_folded_filter_leaves_out_every_spelling_of_an_excluded_name() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ENTW\u{dc}RFE/plan.txt"), "x");
        write(&tmp.path().join("R\u{c9}SUM\u{c9}.pdf"), "x");
        write(&tmp.path().join("notes.txt"), "x");
        let patterns = ["Entw\u{fc}rfe", "R\u{e9}sum\u{e9}*"].map(String::from);
        let folded = ExcludeFilter::new(&patterns, true).unwrap();
        let mut s = Scanner::new(&folded).scan(tmp.path(), Volume::of(tmp.path())).unwrap();
        s.fold();
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["notes.txt"]);
        let exact = ExcludeFilter::new(&patterns, false).unwrap();
        let s = Scanner::new(&exact).scan(tmp.path(), Volume::of(tmp.path())).unwrap();
        assert!(s.contains("ENTW\u{dc}RFE/plan.txt"), "where both drives tell case apart, so do patterns");
    }

    #[test]
    fn names_kept_for_charons_own_use_are_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".charon-trash/2026-01-01_00-00-00/a.txt"), "x");
        write(&tmp.path().join(format!("inner/.charon/{OTHER}.json")), "{}");
        write(&tmp.path().join("a/.charon-00112233445566ff.part"), "half");
        write(&tmp.path().join("node_modules/x.js"), "x");
        write(&tmp.path().join("b.charon-partial/inside.txt"), "x");
        let s = scan(tmp.path(), &["node_modules"]);
        let reserved: Vec<&str> = s.reserved().map(|(key, _)| key.as_str()).collect();
        assert_eq!(reserved, vec![".charon-trash", "a/.charon-00112233445566ff.part", "inner/.charon"]);
        assert!(s.leaves_alone("b.charon-partial"), "left out with a note");
        assert!(!s.leaves_alone("node_modules"), "patterns match keys alike on both sides, so they need no record");
    }

    /// Where one of the drives ignores case, ".CHARON-TRASH" has the key of Charon's trash:
    /// it's left out with a note, under that key. Where neither does, it's an item like any
    /// other.
    #[test]
    fn charons_folder_names_in_other_case_are_left_out_where_keys_are_folded() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".CHARON-TRASH/notes.txt"), "x");
        write(&tmp.path().join("inner/.Charon/settings.txt"), "x");
        write(&tmp.path().join("photos/a.jpg"), "x");
        let folded = ExcludeFilter::new(&[], true).unwrap();
        let mut s = Scanner::new(&folded).scan(tmp.path(), Volume::of(tmp.path())).unwrap();
        s.fold();
        assert_eq!(s.paths().collect::<Vec<_>>(), vec!["inner", "photos", "photos/a.jpg"]);
        assert!(s.leaves_alone(".charon-trash") && s.leaves_alone("inner/.charon"));
        assert!(s.is_occupied("inner"), "the folder around it stays");
        let mut left_out: Vec<&str> = s.skipped().iter().map(|skipped| skipped.path.as_str()).collect();
        left_out.sort();
        assert_eq!(left_out, vec![".CHARON-TRASH", "inner/.Charon"], "noted as they are written");
        let s = scan(tmp.path(), &[]);
        assert!(s.contains(".CHARON-TRASH/notes.txt") && s.contains("inner/.Charon/settings.txt"));
        assert!(s.skipped().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_left_out_under_their_own_name() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("run")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(tmp.path().join("run/app.sock")).unwrap();
        let s = scan(tmp.path(), &[]);
        assert!(!s.contains("run/app.sock"));
        assert!(s.leaves_alone("run/app.sock"), "nothing of its name is touched");
        assert!(s.is_occupied("run"), "and its folder stays");
        assert_eq!(s.skipped().len(), 1);
        assert_eq!(s.skipped()[0].path, "run/app.sock");
    }

    #[test]
    fn names_are_compared_in_one_unicode_form() {
        let tmp = tempfile::tempdir().unwrap();
        let nfd = "cafe\u{301}.txt";
        write(&tmp.path().join(nfd), "x");
        let s = scan(tmp.path(), &[]);
        // Some file systems turn the name into NFC on their own; either way the key is NFC.
        assert!(s.get("caf\u{e9}.txt").is_some());
        let on_disk = fs::read_dir(tmp.path()).unwrap().next().unwrap().unwrap().file_name();
        assert_eq!(s.spelling("caf\u{e9}.txt"), on_disk.to_str().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn two_spellings_of_one_name_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("cafe\u{301}.txt"), "nfd");
        write(&tmp.path().join("caf\u{e9}.txt"), "nfc");
        let s = scan(tmp.path(), &[]);
        assert!(s.frozen("caf\u{e9}.txt").is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn case_twins_are_left_alone_once_keys_ignore_case() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("Readme.md"), "a");
        write(&tmp.path().join("README.md"), "b");
        let mut s = scan(tmp.path(), &[]);
        assert_eq!(s.len(), 2, "both names exist on a drive that tells case apart");
        s.fold();
        assert!(s.frozen("readme.md").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_recorded_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("real/inside.txt"), "x");
        std::os::unix::fs::symlink("real", tmp.path().join("shortcut")).unwrap();
        let s = scan(tmp.path(), &[]);
        assert!(s.get("shortcut").unwrap().is_link());
        assert!(s.get("shortcut/inside.txt").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_frozen_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("locked/secret.txt"), "x");
        write(&tmp.path().join("open.txt"), "x");
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(tmp.path().join("locked")).is_ok(); // root ignores permissions
        let s = scan(tmp.path(), &[]);
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            return;
        }
        assert!(s.frozen("locked").is_some());
        assert!(s.frozen("locked/secret.txt").is_some());
        assert!(s.get("open.txt").is_some());
    }
}
