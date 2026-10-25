//! What kind of drive a folder is on, and what that drive can hold.
//!
//! Drives differ in ways that matter for a sync: FAT and exFAT keep times in local time
//! (so a daylight-saving change moves every file by an hour) and can't hold links, files
//! over 4 GB on FAT, or names with characters like `:` or `?`; most Mac and Windows drives
//! treat "Report.pdf" and "report.pdf" as one name; network shares may show another
//! computer's changes late. Charon looks at each folder's drive before it plans, so an item
//! the other drive can't hold becomes a note instead of a step that fails on every run.

use crate::scan::{is_partial, new_partial_name};
use serde::{Deserialize, Serialize};
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// The family of file system a drive uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsKind {
    Apfs,
    Hfs,
    Fat,
    Exfat,
    Ntfs,
    Refs,
    Smb,
    Nfs,
    Afp,
    Ext,
    Btrfs,
    Xfs,
    Zfs,
    Other,
}

/// One drive, as seen from one folder on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub kind: FsKind,
    /// The file system's name as the system reports it ("apfs", "msdos", "NTFS", "0xef53").
    pub fs: String,
    /// Where the drive is mounted (macOS, Linux), or its serial number (Windows). A folder
    /// whose drive changed (a leftover mount folder on the computer's own disk) shows here.
    pub mount: String,
    /// A name for the person: "Backup SSD", "startup disk".
    pub name: String,
    /// A network share: other computers may change it, and it may show their changes late.
    pub network: bool,
    /// Whether "Report.pdf" and "report.pdf" are two names on this drive.
    pub case_sensitive: bool,
    /// The drive couldn't be asked how it treats case (an empty folder Charon can't write
    /// to, or names without letters), so `case_sensitive` is what its kind usually does.
    /// Kept with the job's record of its drives, so a later answer isn't taken for a change.
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_guessed: bool,
    /// A sync found that this drive refuses symbolic links (some network shares do, though
    /// their kind would allow them). Kept with the job's record of its drives.
    #[serde(default, skip_serializing_if = "is_false")]
    pub refuses_links: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What a drive turned out not to hold when a sync tried to create an item there (see
/// `run::Executor`). Looks after that leave the item out, with a note, instead of trying
/// again on every sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The drive finds another item under this name: a Windows short name (REPORT~1.PDF
    /// for "Report final.pdf"), or a rule for upper and lower case Charon doesn't share.
    TakenName,
    /// The drive refused the name.
    Name,
    /// The name, or the path to it, is too long for the drive.
    TooLong,
    /// The file is too big for the drive.
    TooBig,
    /// A conflict copy can't be made next to the item: the path to even the shortest name
    /// for it is too long for the drive. Both versions stay as they are.
    ConflictCopy,
}

impl Refusal {
    /// Why the item can't be created on `drive` ("Backup SSD (NTFS)"), for a note.
    pub fn reason(self, drive: &str) -> String {
        match self {
            Refusal::TakenName => format!(
                "the {drive} finds another item under its name (a short name like REPORT~1.PDF, or another rule for upper and lower case)"
            ),
            Refusal::Name => format!("the {drive} refused its name"),
            Refusal::TooLong => format!("its name, or the path to it, is too long for the {drive}"),
            Refusal::TooBig => format!("it's too big for the {drive}"),
            Refusal::ConflictCopy => format!(
                "the path to a conflict copy next to it would be too long for the {drive}, so both versions stay as they are until you rename or move one"
            ),
        }
    }

    /// The refusal an error from creating or renaming an item stands for, if it stands
    /// for one (rather than a passing problem like a full disk or a missing permission).
    pub fn of_naming(error: &std::io::Error) -> Option<Refusal> {
        let code = error.raw_os_error()?;
        #[cfg(unix)]
        {
            match code {
                libc::EINVAL | libc::EILSEQ => Some(Refusal::Name),
                libc::ENAMETOOLONG => Some(Refusal::TooLong),
                _ => None,
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{ERROR_FILENAME_EXCED_RANGE, ERROR_INVALID_NAME};
            match code as u32 {
                ERROR_INVALID_NAME => Some(Refusal::Name),
                ERROR_FILENAME_EXCED_RANGE => Some(Refusal::TooLong),
                _ => None,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = code;
            None
        }
    }

    /// The refusal an error from writing a file stands for, if it stands for one.
    pub fn of_writing(error: &std::io::Error) -> Option<Refusal> {
        let code = error.raw_os_error()?;
        #[cfg(unix)]
        {
            (code == libc::EFBIG).then_some(Refusal::TooBig)
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::ERROR_FILE_TOO_LARGE;
            (code as u32 == ERROR_FILE_TOO_LARGE).then_some(Refusal::TooBig)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = code;
            None
        }
    }
}

impl Volume {
    /// The drive `root` is on, without asking how it treats case: quick enough to ask
    /// every second. `None` where the system can't tell.
    pub fn basics(root: &Path) -> Option<Volume> {
        system::volume_of(root).map(|volume| pretend::applied(root, volume))
    }

    /// The drive of the folder `root`. Never fails: what can't be found out is assumed in the
    /// careful direction (another kind of drive, ignoring case).
    pub fn of(root: &Path) -> Volume {
        let mut volume = system::volume_of(root).unwrap_or_else(|| Volume {
            kind: FsKind::Other,
            fs: "unknown".into(),
            mount: String::new(),
            name: Self::folder_name(root),
            network: false,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        });
        let answer = system::case_sensitive(root).or_else(|| probe_case(root)).or_else(|| probe_case_with_own_file(root));
        volume.case_sensitive = answer.unwrap_or_else(|| volume.kind.usually_case_sensitive());
        volume.case_guessed = answer.is_none();
        pretend::applied(root, volume)
    }

    /// The kind of drive in words: "APFS", "exFAT", "network share (SMB)".
    pub fn kind_label(&self) -> String {
        match self.kind {
            FsKind::Apfs => "APFS".into(),
            FsKind::Hfs => "Mac OS Extended".into(),
            FsKind::Fat => "FAT32".into(),
            FsKind::Exfat => "exFAT".into(),
            FsKind::Ntfs => "NTFS".into(),
            FsKind::Refs => "ReFS".into(),
            FsKind::Smb => "network share (SMB)".into(),
            FsKind::Nfs => "network share (NFS)".into(),
            FsKind::Afp => "network share (AFP)".into(),
            FsKind::Ext => "ext4".into(),
            FsKind::Btrfs => "Btrfs".into(),
            FsKind::Xfs => "XFS".into(),
            FsKind::Zfs => "ZFS".into(),
            FsKind::Other if self.network => format!("network share ({})", self.fs),
            FsKind::Other => self.fs.clone(),
        }
    }

    /// Whether this drive may show changes late: a network share (another computer's
    /// change), or a drive run by a program instead of the system (FUSE: sshfs, rclone and
    /// most cloud drives), which keeps its own picture of the folders for a while. A look at
    /// one item there opens it, so the drive itself answers.
    pub fn shows_changes_late(&self) -> bool {
        self.network || self.run_by_a_program()
    }

    /// Whether a program runs this drive instead of the system (FUSE). A disk of the
    /// computer's own that a program reads (on Linux, "fuseblk": a USB drive through
    /// ntfs-3g or exfat-fuse) doesn't count: nobody else changes it.
    fn run_by_a_program(&self) -> bool {
        let fs = self.fs.to_ascii_lowercase();
        fs.contains("fuse") && fs != "fuseblk"
    }

    /// Whether this drive is run by a driver Apple didn't make, on a Mac: Paragon's or
    /// Tuxera's NTFS, macFUSE and the like. Such drivers work inside the system's own core,
    /// and one has brought the whole Mac down (a kernel panic) during a sync, while Spotlight
    /// read the drive too. So Charon works on one job at a time there, and while a sync writes
    /// to such a drive, nothing else of Charon's asks it anything (see `engine::quiet`). A
    /// drive whose kind couldn't be found out counts too, the careful direction. A disk image
    /// kept on such a drive (a VeraCrypt volume over macFUSE) is Apple's own file system, and
    /// doesn't count. Elsewhere the system's own drivers are used.
    pub fn run_by_an_add_on_driver(&self) -> bool {
        #[cfg(test)]
        if self.fs == pretend::ADD_ON {
            return true;
        }
        cfg!(target_os = "macos") && (!apple_made(&self.fs) || has_add_on_bundle(&self.fs))
    }

    /// Whether this drive keeps times in local time, so a daylight-saving change or a trip
    /// to another time zone moves every file's time by whole hours.
    pub fn shifts_times(&self) -> bool {
        matches!(self.kind, FsKind::Fat | FsKind::Exfat)
    }

    /// Whether this drive keeps a trustworthy change time for each file (it moves with
    /// every edit, even one that puts the old modification time back). Local Mac and Linux
    /// drives do; FAT and exFAT have none, and network shares and other drivers report
    /// whatever they like.
    pub fn keeps_change_times(&self) -> bool {
        !self.network && matches!(self.kind, FsKind::Apfs | FsKind::Hfs | FsKind::Ext | FsKind::Btrfs | FsKind::Xfs | FsKind::Zfs)
    }

    /// Whether this drive keeps modification times in steps of a second or more (Mac OS
    /// Extended, FAT, exFAT, network shares, and drives run by a program, like sshfs), so two
    /// writes within one step look the same.
    pub fn coarse_times(&self) -> bool {
        self.shows_changes_late() || matches!(self.kind, FsKind::Hfs | FsKind::Fat | FsKind::Exfat | FsKind::Smb | FsKind::Nfs | FsKind::Afp)
    }

    /// Whether this drive's times can be held against another drive's to say which version
    /// is newer: not on network shares, nor on drivers Charon doesn't know (a FAT drive seen
    /// through one keeps local times nobody can account for).
    pub fn comparable_times(&self) -> bool {
        !self.network && (self.kind != FsKind::Other || matches!(self.fs.as_str(), "tmpfs" | "overlay"))
    }

    /// Whether a file removed from this drive gives its room back at once. A file system that
    /// may keep snapshots (APFS, Btrfs, ZFS, ReFS) can go on holding a removed file's data,
    /// and so can a network share's server, an overlay whose file came from a layer below, or
    /// a drive Charon doesn't know.
    pub fn frees_removed_files(&self) -> bool {
        !self.network
            && (matches!(self.kind, FsKind::Hfs | FsKind::Fat | FsKind::Exfat | FsKind::Ntfs | FsKind::Ext | FsKind::Xfs)
                || matches!(self.fs.as_str(), "tmpfs" | "fuseblk"))
    }

    /// Whether symbolic links can be created here.
    pub fn holds_links(&self) -> bool {
        !self.refuses_links && !matches!(self.kind, FsKind::Fat | FsKind::Exfat)
    }

    /// Whether this drive may refuse a link with another answer than "not supported" (a
    /// network share or a drive Charon doesn't know may say "permission denied" or
    /// "invalid argument" instead).
    pub fn answers_links_vaguely(&self) -> bool {
        self.network || matches!(self.kind, FsKind::Smb | FsKind::Nfs | FsKind::Afp | FsKind::Other)
    }

    /// Whether extended attributes (Finder tags and the like) can be kept here. FAT and exFAT
    /// drives only fake them with "._" files next to every file.
    pub fn holds_extended_attributes(&self) -> bool {
        !matches!(self.kind, FsKind::Fat | FsKind::Exfat)
    }

    /// Why a file of `size` bytes can't be stored here, if it can't.
    pub fn too_big(&self, size: u64) -> Option<String> {
        (self.kind == FsKind::Fat && size > u32::MAX as u64)
            .then(|| format!("it's over 4 GB, and the {} can't hold files that big", self.describe()))
    }

    /// Why an item named `name` can't be created here, if it can't.
    pub fn cannot_hold(&self, name: &str) -> Option<String> {
        if self.kind.uses_windows_names() {
            if let Some(reason) = windows_name_problem(name) {
                return Some(format!("{reason}, which the {} can't hold", self.describe()));
            }
        }
        // Mac OS Extended stores names decomposed (é as e and an accent), which makes accented
        // names longer there than they look.
        let too_long = if self.kind == FsKind::Hfs {
            name.nfd().map(char::len_utf16).sum::<usize>() > 255
        } else if self.counts_utf16() {
            name.encode_utf16().count() > 255
        } else {
            name.len() > 255
        };
        too_long.then(|| format!("its name is too long for the {}", self.describe()))
    }

    /// Whether this drive counts the length of a name in UTF-16 units rather than bytes (see
    /// `FsKind::counts_utf16`). On Linux, a disk a program reads ("fuseblk") is an NTFS or
    /// exFAT drive read through ntfs-3g or exfat-fuse, which count so too.
    fn counts_utf16(&self) -> bool {
        self.kind.counts_utf16() || self.fs == "fuseblk"
    }

    /// "Backup SSD (exFAT)".
    pub fn describe(&self) -> String {
        format!("{} ({})", self.name, self.kind_label())
    }

    fn folder_name(path: &Path) -> String {
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
    }
}

impl FsKind {
    /// Drives that keep Windows' rules for names (also when used from a Mac or Linux).
    fn uses_windows_names(self) -> bool {
        matches!(self, FsKind::Fat | FsKind::Exfat | FsKind::Ntfs | FsKind::Refs | FsKind::Smb)
    }

    /// Drives that count name length in UTF-16 units rather than bytes: the drives made for
    /// Windows, and Apple's (APFS holds every name Mac OS Extended could, so a name of 90
    /// Japanese characters fits there, though it takes 270 bytes).
    fn counts_utf16(self) -> bool {
        matches!(self, FsKind::Fat | FsKind::Exfat | FsKind::Ntfs | FsKind::Refs | FsKind::Smb | FsKind::Hfs | FsKind::Apfs)
    }

    /// The usual case setting of this kind of drive, when the drive can't be asked.
    pub(crate) fn usually_case_sensitive(self) -> bool {
        matches!(self, FsKind::Ext | FsKind::Btrfs | FsKind::Xfs | FsKind::Zfs | FsKind::Nfs)
    }

    fn from_name(fs: &str) -> FsKind {
        match fs.to_ascii_lowercase().as_str() {
            "apfs" => FsKind::Apfs,
            "hfs" | "hfsplus" => FsKind::Hfs,
            "msdos" | "vfat" | "fat" | "fat32" | "fat16" | "fat12" => FsKind::Fat,
            "exfat" => FsKind::Exfat,
            // The kernel's own, and the drivers Macs use to write to NTFS (Paragon, Tuxera).
            "ntfs" | "ntfs3" | "ufsd_ntfs" | "tuxera_ntfs" | "fusefs_txantfs" => FsKind::Ntfs,
            "refs" => FsKind::Refs,
            "smbfs" | "cifs" | "smb2" | "smb3" => FsKind::Smb,
            "nfs" | "nfs4" => FsKind::Nfs,
            "afpfs" => FsKind::Afp,
            "ext2" | "ext3" | "ext4" => FsKind::Ext,
            "btrfs" => FsKind::Btrfs,
            "xfs" => FsKind::Xfs,
            "zfs" => FsKind::Zfs,
            _ => FsKind::Other,
        }
    }
}

/// Whether macOS itself brings the file system named `fs` (as the system names it: "apfs",
/// "msdos", "smbfs"). Paragon's drivers name theirs "ufsd_NTFS" and the like, Tuxera's
/// "tuxera_ntfs", macFUSE's "macfuse".
fn apple_made(fs: &str) -> bool {
    matches!(
        fs.to_ascii_lowercase().as_str(),
        "apfs" | "hfs" | "msdos" | "exfat" | "ntfs" | "lifs" | "smbfs" | "nfs" | "afpfs" | "webdav" | "ftp" | "acfs" | "cd9660" | "udf" | "cddafs" | "devfs" | "autofs" | "mtmfs" | "nullfs"
    )
}

/// Whether another maker installed a driver for the file system named `fs` (in
/// /Library/Filesystems; Apple's own are in /System/Library/Filesystems), which would then
/// run the drive even under one of Apple's names.
fn has_add_on_bundle(fs: &str) -> bool {
    !fs.is_empty() && !fs.contains(['/', '\\']) && !fs.starts_with('.') && Path::new("/Library/Filesystems").join(format!("{fs}.fs")).exists()
}

/// What Windows (and any drive formatted for it) can't take in a name.
pub(crate) fn windows_name_problem(name: &str) -> Option<&'static str> {
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("its name ends with a dot or a space");
    }
    if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20) {
        return Some("its name has a character like : ? * \" < > | or \\");
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    reserved.then_some("its name is reserved by Windows (like CON or NUL)")
}

/// A Windows path as it's written for people: `\\?\C:\Photos` → `C:\Photos`, and
/// `\\?\UNC\nas\share` → `\\nas\share`.
#[cfg_attr(not(windows), allow(dead_code))]
fn plain_windows_path(path: &str) -> String {
    match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(path).to_string(),
    }
}

/// Whether a Windows path is on a network share: `\\nas\share`, or `\\?\UNC\nas\share`.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_share_path(path: &str) -> bool {
    path.starts_with(r"\\?\UNC\") || (path.starts_with(r"\\") && !path.starts_with(r"\\?\"))
}

/// Asks the drive whether it tells upper and lower case apart, using names already in the
/// folder: if "Photos" exists, is "pHOTOS" the same item? Only ASCII letters are swapped.
/// Charon's own temporary files are passed over (another look's probe, or a copy being
/// made, may be gone a moment later), and a name only found once in its own spelling counts
/// when it's still there after the look for the other spelling. `None` when no name in the
/// folder can tell.
fn probe_case(root: &Path) -> Option<bool> {
    let items = std::fs::read_dir(root).ok()?;
    let names = items.take(64).flatten().filter_map(|item| item.file_name().to_str().map(str::to_string));
    case_told_by(names, |name| std::fs::symlink_metadata(root.join(name)))
}

/// What the first of `names` that can tell says about case (see `probe_case`), looking
/// items up with `look`. Charon's own temporary files are passed over, and a name that
/// isn't found in other case only tells when its own item is still there afterwards: an
/// item removed in between (a temporary file an app cleared) would make any drive look
/// case-sensitive.
fn case_told_by(names: impl Iterator<Item = String>, look: impl Fn(&str) -> std::io::Result<std::fs::Metadata>) -> Option<bool> {
    for name in names {
        let swapped = swap_case(&name);
        if swapped == name || is_partial(&name) {
            continue;
        }
        let Ok(original) = look(&name) else { continue };
        match look(&swapped) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if look(&name).is_ok_and(|again| same_item(&original, &again)) {
                    return Some(true);
                }
            }
            Err(_) => continue,
            Ok(other) => return Some(!same_item(&original, &other)),
        }
    }
    None
}

/// Asks the drive with a file of Charon's own where the folder's names can't tell (an empty
/// folder, or names without letters), so a folder gets the same answer empty as once it
/// fills up. The file has a name no other look or sync uses (one of Charon's temporary
/// names, which every look passes over), so looks at the same folder at once each get the
/// drive's answer, and it's removed at once. `None` where the folder can't be written to.
fn probe_case_with_own_file(root: &Path) -> Option<bool> {
    let name = new_partial_name();
    let probe = root.join(&name);
    std::fs::OpenOptions::new().write(true).create_new(true).open(&probe).ok()?;
    let answer = match std::fs::symlink_metadata(root.join(swap_case(&name))) {
        Ok(_) => Some(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
        Err(_) => None,
    };
    if let Err(e) = std::fs::remove_file(&probe) {
        log::warn!("couldn't remove the case probe {}: {e}", probe.display());
    }
    answer
}

/// `name` with the upper and lower case of its ASCII letters swapped.
fn swap_case(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() })
        .collect()
}

/// Whether two looks found the same item. Some network shares make up item numbers per
/// name, so equal size, time and kind count as the same item too.
fn same_item(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            return true;
        }
    }
    a.file_type() == b.file_type() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// Keeps the current thread from downloading files that are only in the cloud (iCloud Drive,
/// Dropbox, OneDrive placeholders) when it looks into them: such places fail quickly and are
/// left alone, instead of filling the disk with downloads. macOS only; elsewhere placeholders
/// are recognised by their attributes.
pub fn never_download_on_this_thread() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
        }
        const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
        const IOPOL_SCOPE_THREAD: libc::c_int = 1;
        const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: libc::c_int = 1;
        // SAFETY: plain values; the call only changes this thread's I/O policy.
        let result = unsafe { setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD, IOPOL_MATERIALIZE_DATALESS_FILES_OFF) };
        if result != 0 {
            log::debug!("couldn't turn off downloads of cloud files: {}", std::io::Error::last_os_error());
        }
    }
}

#[cfg(target_os = "macos")]
mod system {
    use super::{FsKind, Volume};
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn volume_of(root: &Path) -> Option<Volume> {
        let path = CString::new(root.as_os_str().as_bytes()).ok()?;
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `info` is a valid buffer for the call.
        if unsafe { libc::statfs(path.as_ptr(), &mut info) } != 0 {
            return None;
        }
        // SAFETY: the system fills both fields with NUL-terminated strings.
        let fs = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }.to_string_lossy().into_owned();
        let mount = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
        let local = info.f_flags & libc::MNT_LOCAL as u32 != 0;
        let kind = FsKind::from_name(&fs);
        let name = match mount.as_str() {
            "/" | "/System/Volumes/Data" => "startup disk".to_string(),
            other => Path::new(other).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| other.to_string()),
        };
        Some(Volume { kind, fs, mount, name, network: !local, case_sensitive: false, case_guessed: false, refuses_links: false })
    }

    /// macOS answers this directly.
    pub fn case_sensitive(root: &Path) -> Option<bool> {
        let path = CString::new(root.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is NUL-terminated.
        match unsafe { libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
mod system {
    use super::{FsKind, Volume};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    /// The kind number `statfs` gives every drive run by a program (FUSE).
    const FUSE: u64 = 0x6573_5546;

    pub fn volume_of(root: &Path) -> Option<Volume> {
        let path = CString::new(root.as_os_str().as_bytes()).ok()?;
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `info` is a valid buffer for the call.
        if unsafe { libc::statfs(path.as_ptr(), &mut info) } != 0 {
            return None;
        }
        let magic = info.f_type as u64 & 0xffff_ffff;
        let (fs, network) = match magic {
            0xEF53 => ("ext4", false),
            0x9123_683E => ("btrfs", false),
            0x5846_5342 => ("xfs", false),
            0x4D44 => ("vfat", false),
            0x2011_BAB0 => ("exfat", false),
            0x5346_544E | 0x7366_746E => ("ntfs", false),
            0x482B => ("hfsplus", false),
            0x2FC1_2FC1 => ("zfs", false),
            0x517B | 0xFF53_4D42 | 0xFE53_4D42 => ("cifs", true),
            0x6969 => ("nfs", true),
            FUSE => ("fuse", false),
            0x0102_1994 => ("tmpfs", false),
            0x794C_7630 => ("overlay", false),
            _ => ("", false),
        };
        let fs = if fs.is_empty() { format!("0x{magic:x}") } else { fs.to_string() };
        // A drive run by a program says which one in the system's list of mounts.
        let fs = if magic == FUSE { fuse_type(root).unwrap_or(fs) } else { fs };
        let mount = mount_point(root);
        let name = match mount.as_str() {
            "/" => "system disk".to_string(),
            other => Path::new(other).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| other.to_string()),
        };
        Some(Volume { kind: FsKind::from_name(&fs), fs, mount, name, network, case_sensitive: true, case_guessed: false, refuses_links: false })
    }

    /// The type the system's list of mounts gives the FUSE drive `root` is on: "fuse.sshfs",
    /// "fuse.rclone", or "fuseblk" for a disk a program reads (ntfs-3g, exfat-fuse).
    fn fuse_type(root: &Path) -> Option<String> {
        let device = std::fs::metadata(root).ok()?.dev();
        let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        mount_type(&mounts, libc::major(device), libc::minor(device))
    }

    /// The type of the mount of device `major:minor` in `mounts` (the lines of
    /// `/proc/self/mountinfo`: the device is the third field, the type follows " - ").
    pub(super) fn mount_type(mounts: &str, major: u32, minor: u32) -> Option<String> {
        let device = format!("{major}:{minor}");
        mounts.lines().find_map(|line| {
            if line.split(' ').nth(2)? != device {
                return None;
            }
            let (_, described) = line.split_once(" - ")?;
            described.split(' ').next().map(str::to_string)
        })
    }

    /// The topmost folder above `root` on the same drive.
    fn mount_point(root: &Path) -> String {
        let Ok(start) = std::fs::canonicalize(root) else { return String::new() };
        let Ok(device) = std::fs::metadata(&start).map(|m| m.dev()) else { return String::new() };
        let mut top = start.as_path();
        while let Some(parent) = top.parent() {
            match std::fs::metadata(parent) {
                Ok(meta) if meta.dev() == device => top = parent,
                _ => break,
            }
        }
        top.display().to_string()
    }

    /// Linux has no general answer; the folder's own names are asked instead.
    pub fn case_sensitive(_root: &Path) -> Option<bool> {
        None
    }
}

#[cfg(windows)]
mod system {
    use super::{FsKind, Volume};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationByHandleW, GetVolumePathNameW, FILE_FLAG_BACKUP_SEMANTICS};

    /// GetDriveTypeW's answer for network drives.
    const DRIVE_REMOTE: u32 = 4;

    pub fn volume_of(root: &Path) -> Option<Volume> {
        let folder = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(root).ok()?;
        let mut label = [0u16; 261];
        let mut fs_name = [0u16; 261];
        let (mut serial, mut longest, mut flags) = (0u32, 0u32, 0u32);
        // SAFETY: the handle is open and every buffer is valid for the sizes given.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                folder.as_raw_handle() as _,
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                &mut longest,
                &mut flags,
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let text = |buffer: &[u16]| String::from_utf16_lossy(&buffer[..buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len())]);
        let fs = text(&fs_name);
        let label = text(&label);
        // Where the folder really is, which its path may not show: through a link or a mapped
        // drive, a share is still a share (`\\?\UNC\nas\share\…`).
        let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let real_text = real.as_os_str().to_string_lossy();
        let plain: Vec<u16> = super::plain_windows_path(&real_text).encode_utf16().chain(std::iter::once(0)).collect();
        let mut mount = [0u16; 1024];
        // SAFETY: the path is NUL-terminated and `mount` is valid for its length.
        let ask = |path: &[u16], mount: &mut [u16; 1024]| unsafe { GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } != 0;
        // A path longer than 260 characters only works in its long form (`\\?\C:\…`), which
        // then comes back in the answer too.
        let has_mount = ask(&plain, &mut mount) || ask(&crate::run::rename::wide(&real), &mut mount);
        let mount_text = if has_mount { super::plain_windows_path(&text(&mount)) } else { String::new() };
        let unc = super::is_share_path(&real_text);
        // SAFETY: `mount` is NUL-terminated when `has_mount`.
        let remote = has_mount && unsafe { GetDriveTypeW(mount.as_ptr()) } == DRIVE_REMOTE;
        let network = unc || remote;
        let name = if !label.is_empty() { label } else if !mount_text.is_empty() { mount_text.trim_end_matches('\\').to_string() } else { "drive".into() };
        let kind = if network { FsKind::Smb } else { FsKind::from_name(&fs) };
        Some(Volume { kind, fs, mount: format!("{serial:08X}"), name, network, case_sensitive: false, case_guessed: false, refuses_links: false })
    }

    /// Windows programs see every drive as ignoring case.
    pub fn case_sensitive(_root: &Path) -> Option<bool> {
        Some(false)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod system {
    use super::Volume;
    use std::path::Path;

    pub fn volume_of(_root: &Path) -> Option<Volume> {
        None
    }

    pub fn case_sensitive(_root: &Path) -> Option<bool> {
        None
    }
}

#[cfg(not(test))]
mod pretend {
    use super::Volume;
    use std::path::Path;

    pub(super) fn applied(_root: &Path, volume: Volume) -> Volume {
        volume
    }
}

/// Tests can't plug in a drive run by an add-on driver: a folder pretends to be one.
#[cfg(test)]
pub(crate) mod pretend {
    use super::Volume;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// The name of the file system such a folder's drive reports.
    pub const ADD_ON: &str = "pretend_add_on";

    static FOLDERS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// While this lives, everything in its folder is on a drive of its own, mounted at that
    /// folder and run by an add-on driver (see `Volume::run_by_an_add_on_driver`).
    pub struct AddOnDrive(PathBuf);

    impl AddOnDrive {
        pub fn at(folder: &Path) -> AddOnDrive {
            FOLDERS.lock().unwrap_or_else(|e| e.into_inner()).push(folder.to_path_buf());
            AddOnDrive(folder.to_path_buf())
        }
    }

    impl Drop for AddOnDrive {
        fn drop(&mut self) {
            FOLDERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|folder| folder != &self.0);
        }
    }

    /// `volume`, the drive of `root`, as it pretends to be.
    pub(super) fn applied(root: &Path, mut volume: Volume) -> Volume {
        let folders = FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(folder) = folders.iter().find(|folder| root.starts_with(folder)) {
            volume.fs = ADD_ON.into();
            volume.mount = folder.to_string_lossy().into_owned();
            volume.name = "Stick".into();
        }
        volume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(kind: FsKind) -> Volume {
        Volume { kind, fs: "x".into(), mount: "/x".into(), name: "Stick".into(), network: false, case_sensitive: false, case_guessed: false, refuses_links: false }
    }

    #[test]
    fn windows_drives_refuse_windows_names() {
        let exfat = volume(FsKind::Exfat);
        assert!(exfat.cannot_hold("a:b.txt").is_some());
        assert!(exfat.cannot_hold("notes.").is_some());
        assert!(exfat.cannot_hold("CON.txt").is_some());
        assert!(exfat.cannot_hold("normal name.txt").is_none());
        assert!(volume(FsKind::Ext).cannot_hold("a:b.txt").is_none());
    }

    #[test]
    fn long_names_are_counted_the_way_the_drive_counts() {
        let name = "é".repeat(200); // 400 bytes, 200 UTF-16 units
        assert!(volume(FsKind::Ext).cannot_hold(&name).is_some());
        assert!(volume(FsKind::Ntfs).cannot_hold(&name).is_none());
        // Mac OS Extended keeps é as two letters: 400 UTF-16 units there.
        assert!(volume(FsKind::Hfs).cannot_hold(&name).is_some());
        assert!(volume(FsKind::Hfs).cannot_hold(&"e".repeat(200)).is_none());
        assert!(volume(FsKind::Apfs).cannot_hold(&"é".repeat(120)).is_none());
        // 90 Japanese characters take 270 bytes, and 90 UTF-16 units.
        let japanese = "\u{3042}".repeat(90);
        assert!(volume(FsKind::Apfs).cannot_hold(&japanese).is_none(), "APFS holds what Mac OS Extended held");
        assert!(volume(FsKind::Ext).cannot_hold(&japanese).is_some());
        assert!(Volume { fs: "fuseblk".into(), ..volume(FsKind::Other) }.cannot_hold(&japanese).is_none(), "NTFS or exFAT through a program");
        assert!(Volume { fs: "fuse.sshfs".into(), ..volume(FsKind::Other) }.cannot_hold(&japanese).is_some());
        assert!(volume(FsKind::Apfs).cannot_hold(&"\u{3042}".repeat(256)).is_some());
        assert!(volume(FsKind::Apfs).cannot_hold(&"\u{1f600}".repeat(128)).is_some(), "an emoji takes two UTF-16 units");
    }

    #[test]
    fn mac_drivers_for_ntfs_follow_windows_rules() {
        for name in ["ufsd_NTFS", "tuxera_ntfs", "fusefs_txantfs"] {
            let drive = volume(FsKind::from_name(name));
            assert_eq!(drive.kind, FsKind::Ntfs, "{name}");
            assert!(drive.cannot_hold("Q3: plan.txt").is_some(), "{name}");
        }
    }

    #[test]
    fn drivers_apple_didnt_make_are_told_apart_on_a_mac() {
        let on = |fs: &str| Volume { fs: fs.into(), ..volume(FsKind::from_name(fs)) }.run_by_an_add_on_driver();
        for (fs, apple) in [
            ("ufsd_NTFS", false),
            ("ufsd_ExtFS", false),
            ("tuxera_ntfs", false),
            ("fusefs_txantfs", false),
            ("macfuse", false),
            ("osxfuse", false),
            ("unknown", false),
            ("apfs", true),
            ("hfs", true),
            ("msdos", true),
            ("exfat", true),
            ("ntfs", true),
            ("smbfs", true),
            ("nfs", true),
            ("lifs", true),
        ] {
            assert_eq!(apple_made(fs), apple, "{fs}");
            if !apple {
                assert_eq!(on(fs), cfg!(target_os = "macos"), "{fs}");
            }
        }
        assert!(!on("ext4") || cfg!(target_os = "macos"));
        assert!(!has_add_on_bundle("") && !has_add_on_bundle("../x") && !has_add_on_bundle("a/b"));
    }

    #[test]
    fn a_folder_can_pretend_to_be_on_an_add_on_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let stick = tmp.path().join("stick");
        std::fs::create_dir_all(stick.join("dest")).unwrap();
        assert!(!Volume::of(tmp.path()).run_by_an_add_on_driver() || cfg!(target_os = "macos") && !apple_made(&Volume::of(tmp.path()).fs));
        let pretending = pretend::AddOnDrive::at(&stick);
        for drive in [Volume::of(&stick.join("dest")), Volume::basics(&stick.join("dest")).unwrap()] {
            assert!(drive.run_by_an_add_on_driver());
            assert_eq!(drive.mount, stick.to_string_lossy());
        }
        assert_ne!(Volume::of(tmp.path()).fs, pretend::ADD_ON);
        drop(pretending);
        assert_ne!(Volume::of(&stick).fs, pretend::ADD_ON);
    }

    #[test]
    fn a_drive_found_refusing_links_holds_none() {
        let mut share = volume(FsKind::Smb);
        assert!(share.holds_links() && share.answers_links_vaguely());
        share.refuses_links = true;
        assert!(!share.holds_links());
        assert!(!volume(FsKind::Apfs).answers_links_vaguely());
        // Only a refusal that was found is kept with the job.
        let saved = serde_json::to_string(&volume(FsKind::Smb)).unwrap();
        assert!(!saved.contains("refusesLinks"));
        let read: Volume = serde_json::from_str(&saved).unwrap();
        assert!(!read.refuses_links);
        let saved = serde_json::to_string(&share).unwrap();
        assert!(serde_json::from_str::<Volume>(&saved).unwrap().refuses_links);
    }

    #[cfg(unix)]
    #[test]
    fn refusals_are_told_from_passing_problems() {
        let error = |code| std::io::Error::from_raw_os_error(code);
        assert_eq!(Refusal::of_naming(&error(libc::EINVAL)), Some(Refusal::Name));
        assert_eq!(Refusal::of_naming(&error(libc::ENAMETOOLONG)), Some(Refusal::TooLong));
        assert_eq!(Refusal::of_naming(&error(libc::ENOSPC)), None, "a full disk can be emptied");
        assert_eq!(Refusal::of_naming(&error(libc::EACCES)), None);
        assert_eq!(Refusal::of_writing(&error(libc::EFBIG)), Some(Refusal::TooBig));
        assert_eq!(Refusal::of_writing(&error(libc::EINVAL)), None);
        assert!(Refusal::Name.reason("Stick (exFAT)").contains("Stick (exFAT)"));
    }

    /// A removed file's room comes back at once only on drives that keep no snapshots and
    /// that Charon sees itself: a network share's server may keep one, whatever its kind.
    #[test]
    fn only_drives_without_snapshots_give_a_removed_files_room_back() {
        for kind in [FsKind::Hfs, FsKind::Fat, FsKind::Exfat, FsKind::Ntfs, FsKind::Ext, FsKind::Xfs] {
            assert!(volume(kind).frees_removed_files(), "{kind:?}");
            assert!(!Volume { network: true, ..volume(kind) }.frees_removed_files(), "{kind:?} on a network share");
        }
        for kind in [FsKind::Apfs, FsKind::Btrfs, FsKind::Zfs, FsKind::Refs, FsKind::Smb, FsKind::Nfs, FsKind::Afp, FsKind::Other] {
            assert!(!volume(kind).frees_removed_files(), "{kind:?}");
        }
        assert!(Volume { fs: "tmpfs".into(), ..volume(FsKind::Other) }.frees_removed_files());
        assert!(Volume { fs: "fuseblk".into(), ..volume(FsKind::Other) }.frees_removed_files(), "NTFS or exFAT through a program");
        assert!(!Volume { fs: "overlay".into(), ..volume(FsKind::Other) }.frees_removed_files());
        assert!(!Volume { fs: "fuse.sshfs".into(), ..volume(FsKind::Other) }.frees_removed_files());
    }

    #[test]
    fn fat_drives_hold_no_big_files_links_or_attributes() {
        let fat = volume(FsKind::Fat);
        assert!(fat.too_big(5_000_000_000).is_some());
        assert!(fat.too_big(1_000).is_none());
        assert!(!fat.holds_links() && !fat.holds_extended_attributes() && fat.shifts_times());
        assert!(volume(FsKind::Exfat).too_big(5_000_000_000).is_none());
    }

    #[test]
    fn shares_and_drives_run_by_a_program_may_show_changes_late() {
        assert!(!volume(FsKind::Ext).shows_changes_late());
        let fuse = Volume { fs: "fuse".into(), ..volume(FsKind::Other) };
        assert!(fuse.shows_changes_late() && fuse.coarse_times());
        assert!(Volume { fs: "macfuse".into(), ..volume(FsKind::Other) }.shows_changes_late());
        assert!(Volume { fs: "fuse.sshfs".into(), ..volume(FsKind::Other) }.shows_changes_late());
        assert!(Volume { network: true, ..volume(FsKind::Smb) }.shows_changes_late());
        // A USB drive read through ntfs-3g or exfat-fuse is the computer's own.
        assert!(!Volume { fs: "fuseblk".into(), ..volume(FsKind::Other) }.shows_changes_late());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_list_of_mounts_tells_which_program_runs_a_drive() {
        let mounts = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
49 28 0:41 / /mnt/nas rw,nosuid,nodev,relatime - fuse.sshfs me@nas:/photos rw,user_id=0,group_id=0
51 28 8:17 / /media/me/USB\\040Stick rw,nosuid,nodev,relatime shared:9 - fuseblk /dev/sdb1 rw,user_id=0,group_id=0,blksize=4096
";
        assert_eq!(system::mount_type(mounts, 0, 41).as_deref(), Some("fuse.sshfs"));
        assert_eq!(system::mount_type(mounts, 8, 17).as_deref(), Some("fuseblk"));
        assert_eq!(system::mount_type(mounts, 8, 2).as_deref(), Some("ext4"));
        assert_eq!(system::mount_type(mounts, 0, 99), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_empty_folder_gets_the_answer_it_gets_once_it_fills_up() {
        use crate::scan::MARKER_FOLDER;
        let tmp = folder_on_tmpfs();
        let found = Volume::of(tmp.path());
        assert!(found.case_sensitive && !found.case_guessed, "asked with a file of Charon's own");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "and nothing is left behind");
        std::fs::write(tmp.path().join("Photos"), "x").unwrap();
        assert!(Volume::of(tmp.path()).case_sensitive);
        // A `.charon` folder that was there before is never touched.
        std::fs::remove_file(tmp.path().join("Photos")).unwrap();
        std::fs::create_dir(tmp.path().join(MARKER_FOLDER)).unwrap();
        assert!(Volume::of(tmp.path()).case_sensitive);
        assert_eq!(std::fs::read_dir(tmp.path().join(MARKER_FOLDER)).unwrap().count(), 0);
    }

    /// A folder on tmpfs, a kind of drive whose case setting Charon can't guess.
    #[cfg(target_os = "linux")]
    fn folder_on_tmpfs() -> tempfile::TempDir {
        let shared_memory = Path::new("/dev/shm");
        if shared_memory.is_dir() { tempfile::tempdir_in(shared_memory) } else { tempfile::tempdir() }.unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn looks_at_an_empty_folder_at_once_each_get_the_drives_answer() {
        for _ in 0..50 {
            let tmp = folder_on_tmpfs();
            let answers: Vec<Volume> = std::thread::scope(|scope| {
                let looks: Vec<_> = (0..4).map(|_| scope.spawn(|| Volume::of(tmp.path()))).collect();
                looks.into_iter().map(|look| look.join().unwrap()).collect()
            });
            for answer in answers {
                assert!(answer.case_sensitive && !answer.case_guessed, "{answer:?}");
            }
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "nothing is left behind");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn charons_own_temporary_files_never_answer_for_the_drive() {
        let tmp = folder_on_tmpfs();
        std::fs::write(tmp.path().join(new_partial_name()), "").unwrap();
        assert_eq!(probe_case(tmp.path()), None, "a copy being made may be gone a moment later");
        assert!(Volume::of(tmp.path()).case_sensitive);
    }

    #[test]
    fn a_guess_is_kept_with_the_record_only_when_there_was_one() {
        let answered = volume(FsKind::Other);
        assert!(!serde_json::to_string(&answered).unwrap().contains("caseGuessed"));
        let guessed = Volume { case_guessed: true, ..volume(FsKind::Other) };
        let saved = serde_json::to_string(&guessed).unwrap();
        assert!(serde_json::from_str::<Volume>(&saved).unwrap().case_guessed);
    }

    /// An app's temporary file can go between the two looks at a name: a name whose item
    /// went away tells nothing about case, and Charon's own temporary files never tell.
    #[test]
    fn an_item_that_goes_away_while_it_is_asked_about_tells_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("probe"), "x").unwrap();
        let item = std::fs::symlink_metadata(tmp.path().join("probe")).unwrap();
        let gone = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let names = || [".charon".to_string()].into_iter();
        let looks = std::cell::Cell::new(0);
        let vanishing = |_: &str| {
            looks.set(looks.get() + 1);
            if looks.get() == 1 { Ok(item.clone()) } else { gone() }
        };
        assert_eq!(case_told_by(names(), vanishing), None, "removed after the first look");
        assert_eq!(case_told_by(names(), |name: &str| if name == ".charon" { Ok(item.clone()) } else { gone() }), Some(true));
        assert_eq!(case_told_by(names(), |_: &str| Ok(item.clone())), Some(false));
        assert_eq!(case_told_by([new_partial_name()].into_iter(), |_: &str| Ok(item.clone())), None);
    }

    #[test]
    fn the_drive_of_a_folder_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let found = Volume::of(tmp.path());
        assert!(!found.fs.is_empty());
        #[cfg(target_os = "linux")]
        {
            std::fs::write(tmp.path().join("Probe"), "x").unwrap();
            assert!(Volume::of(tmp.path()).case_sensitive, "Linux temp folders tell case apart");
        }
    }

    #[test]
    fn windows_paths_tell_shares_apart() {
        assert_eq!(plain_windows_path(r"\\?\C:\Users\me"), r"C:\Users\me");
        assert_eq!(plain_windows_path(r"\\?\UNC\nas\share\Photos"), r"\\nas\share\Photos");
        assert_eq!(plain_windows_path(r"D:\"), r"D:\");
        assert!(is_share_path(r"\\?\UNC\nas\share\Photos") && is_share_path(r"\\nas\share"));
        assert!(!is_share_path(r"\\?\C:\NAS\Photos") && !is_share_path(r"C:\NAS\Photos"));
    }

    /// A folder reached through a link to a network share is on that share: it may show
    /// changes late, and its times can't be held against this computer's clock.
    #[cfg(windows)]
    #[test]
    fn a_share_reached_through_a_link_is_a_share() {
        let tmp = tempfile::tempdir().unwrap();
        // The computer's own drive, as Windows shares it with administrators (\\localhost\C$).
        let real = plain_windows_path(&std::fs::canonicalize(tmp.path()).unwrap().to_string_lossy());
        let Some((drive, inside)) = real.split_once(':') else { return };
        let share = std::path::PathBuf::from(format!(r"\\localhost\{drive}${inside}"));
        if !share.is_dir() {
            return; // no administrative share here
        }
        let link = tmp.path().join("NAS");
        if std::os::windows::fs::symlink_dir(&share, &link).is_err() {
            return; // no right to make links
        }
        let volume = Volume::of(&link);
        assert!(volume.network && volume.kind == FsKind::Smb && volume.shows_changes_late(), "{volume:?}");
        assert!(!Volume::of(tmp.path()).network);
    }
}
