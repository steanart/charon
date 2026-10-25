//! Which folder a path really is: the drive it's on and the folder's number on that drive.
//!
//! A path alone can lie. When an external drive or a network share goes away, the folder
//! where it was mounted can stay behind, empty, under the same name. Anything written there
//! lands on the computer's own disk and disappears again when the drive comes back, so a
//! run must stop the moment the folder it started with isn't the one under the path anymore.
//!
//! The drive and folder numbers alone can repeat: a drive unplugged and another one plugged
//! in can get the same device number, and the top folder of a volume often has the same
//! number on every volume of a kind. So the folder's creation time counts too, where the
//! system keeps a believable one: two volumes formatted at different times differ there.
//!
//! A drive cloned from another (a sector copy) keeps all of these too: the volume's serial
//! number, the folders' numbers and their creation times. Where the system gives every
//! volume a name of its own (Windows), that name counts too, so the original and the clone,
//! both plugged in, are two places.
//!
//! The very same drive, pulled out and plugged in again, keeps all of these. Where the
//! system numbers each mount without ever using a number twice (Linux 6.8 and later), the
//! mount counts too, so a drive that went away and came back is seen. Elsewhere a run finds
//! out by looking at what it changed before it records anything (see `Executor::flush`).

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootId {
    /// The drive (device or volume number).
    device: u64,
    /// The folder's number on that drive.
    index: u64,
    /// When the folder was created, in nanoseconds since 1970, where the system keeps it.
    born: Option<i128>,
    /// The mount the folder was seen through, where the system never numbers two mounts
    /// alike. Unknown in an identity an older Charon kept.
    #[serde(default)]
    mount: Option<u64>,
    /// The volume's own name, where the system gives every volume one no other volume has
    /// (Windows: the GUID of its `\\?\Volume{…}` name, which a clone doesn't share). Unknown
    /// on a network share, and in an identity an older Charon kept.
    #[serde(default)]
    volume: Option<u128>,
}

/// A creation time this close to now isn't believed: some network and cloud file systems
/// report the moment they were asked instead of a real creation time.
const BORN_JUST_NOW_SECS: u64 = 10;

impl RootId {
    /// The identity of the folder at `path` (following a link at `path` itself, as the scan
    /// does). `None` where the system can't tell, which never counts as a change.
    pub fn of(path: &Path) -> io::Result<Option<RootId>> {
        system::id_of(path)
    }

    /// Whether the folder `inner` is the folder `outer` or somewhere inside it, judged by the
    /// folders themselves rather than their paths, so a link, a bind mount or another
    /// spelling of a name can't hide it. `false` where the system can't tell.
    pub fn is_within(inner: &Path, outer: &Path) -> bool {
        Place::of(inner).is_within(&Place::of(outer))
    }

    /// Whether `self` and `other` are the same folder at one moment, through whichever
    /// mount: a bind mount shows a folder through a mount of its own, so here the mount
    /// counts for nothing (it only tells a drive that came back from the one seen before).
    fn same_place(&self, other: &RootId) -> bool {
        RootId { mount: None, ..*self }.same_folder(other)
    }

    /// Whether `self` and `other` are the same folder. A creation time, a mount or a
    /// volume name only one of them knows (or neither) doesn't count as a difference.
    pub fn same_folder(&self, other: &RootId) -> bool {
        fn agree<T: PartialEq>(a: Option<T>, b: Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        self.device == other.device && self.index == other.index && agree(self.born, other.born) && agree(self.mount, other.mount) && agree(self.volume, other.volume)
    }

    /// The GUID of a volume from a path that starts with its `\\?\Volume{…}\` name (as
    /// Windows gives the real path of an open folder).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn volume_in(path: &str) -> Option<u128> {
        let (guid, _) = path.strip_prefix(r"\\?\Volume{")?.split_once('}')?;
        let digits: String = guid.chars().filter(|&c| c != '-').collect();
        if digits.len() != 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u128::from_str_radix(&digits, 16).ok()
    }

    /// The folder's creation time, when it's believable: not missing, not the start of
    /// 1970, and not "just now".
    fn born(meta: &std::fs::Metadata) -> Option<i128> {
        let created = meta.created().ok()?;
        let since_1970 = created.duration_since(std::time::UNIX_EPOCH).ok()?;
        if since_1970.is_zero() {
            return None;
        }
        let age = std::time::SystemTime::now().duration_since(created).unwrap_or_default();
        (age.as_secs() >= BORN_JUST_NOW_SECS).then_some(since_1970.as_nanos() as i128)
    }

    /// The drive the item at `path` is on, without following a link there (macOS and Linux).
    #[cfg(unix)]
    pub fn device_of(meta: &std::fs::Metadata) -> u64 {
        use std::os::unix::fs::MetadataExt;
        meta.dev()
    }

    pub fn device(&self) -> u64 {
        self.device
    }
}

/// A folder as `RootId::is_within` judges it, found out at one moment: what it is, and the
/// folders its real path goes through, itself first. Each folder is found out by itself, so a
/// look can stop before it asks about the next (see `Engine::nested`).
pub struct Place {
    id: Option<RootId>,
    through: Vec<RootId>,
}

impl Place {
    pub fn of(path: &Path) -> Place {
        let id = RootId::of(path).ok().flatten();
        let through = std::fs::canonicalize(path)
            .map(|real| real.ancestors().filter_map(|folder| RootId::of(folder).ok().flatten()).collect())
            .unwrap_or_default();
        Place { id, through }
    }

    /// Whether this folder is the folder `outer` or somewhere inside it (see
    /// `RootId::is_within`).
    pub fn is_within(&self, outer: &Place) -> bool {
        outer.id.is_some_and(|outer| self.through.iter().any(|id| id.same_place(&outer)))
    }
}

#[cfg(unix)]
mod system {
    use super::RootId;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn id_of(path: &Path) -> io::Result<Option<RootId>> {
        let meta = std::fs::metadata(path)?;
        Ok(Some(RootId { device: meta.dev(), index: meta.ino(), born: RootId::born(&meta), mount: mount_of(path), volume: None }))
    }

    /// The unique number of the mount `path` is seen through (Linux 6.8 and later). The
    /// older mount numbers aren't used: Linux gives a freed one to the next mount, so a drive
    /// plugged in again usually gets its old number back.
    #[cfg(target_os = "linux")]
    fn mount_of(path: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: an all-zero statx is a valid value (it holds numbers only).
        let mut info: libc::statx = unsafe { std::mem::zeroed() };
        // Asked of the kernel directly: older C libraries have no statx. Only the mount is
        // asked for, from what the system already knows, so a network share isn't asked.
        // SAFETY: `path` is a valid C string and `info` a buffer of the size the call fills.
        let done = unsafe {
            libc::syscall(libc::SYS_statx, libc::AT_FDCWD, path.as_ptr(), libc::AT_STATX_DONT_SYNC, libc::STATX_MNT_ID_UNIQUE, &mut info as *mut libc::statx)
        };
        (done == 0 && info.stx_mask & libc::STATX_MNT_ID_UNIQUE != 0).then_some(info.stx_mnt_id)
    }

    #[cfg(not(target_os = "linux"))]
    fn mount_of(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(windows)]
mod system {
    use super::RootId;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, VOLUME_NAME_GUID,
    };

    pub fn id_of(path: &Path) -> io::Result<Option<RootId>> {
        // A folder can only be opened with backup semantics; reading its information needs no rights.
        let folder = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open for the duration of the call and `info` is a valid buffer.
        let ok = unsafe { GetFileInformationByHandle(folder.as_raw_handle() as _, &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(RootId {
            device: info.dwVolumeSerialNumber as u64,
            index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            born: folder.metadata().ok().and_then(|meta| RootId::born(&meta)),
            mount: None,
            volume: volume_of(&folder),
        }))
    }

    /// The GUID of the volume the open `folder` is on, read from its real path under the
    /// volume's own name. `None` on a network share, which has no such name.
    fn volume_of(folder: &std::fs::File) -> Option<u128> {
        let mut path = vec![0u16; 1024];
        for _ in 0..2 {
            // SAFETY: the handle is open for the duration of the call and `path` is valid for its length.
            let length = unsafe { GetFinalPathNameByHandleW(folder.as_raw_handle() as _, path.as_mut_ptr(), path.len() as u32, VOLUME_NAME_GUID) } as usize;
            if length == 0 {
                return None;
            }
            if length < path.len() {
                return RootId::volume_in(&String::from_utf16_lossy(&path[..length]));
            }
            // Too long for the buffer: `length` is the size it needs.
            path.resize(length, 0);
        }
        None
    }
}

#[cfg(not(any(unix, windows)))]
mod system {
    use super::RootId;
    use std::io;
    use std::path::Path;

    pub fn id_of(path: &Path) -> io::Result<Option<RootId>> {
        std::fs::metadata(path).map(|_| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_replaced_under_the_same_name_is_another_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("photos");
        std::fs::create_dir(&folder).unwrap();
        let before = RootId::of(&folder).unwrap();
        assert_eq!(RootId::of(&folder).unwrap(), before);
        std::fs::rename(&folder, tmp.path().join("old")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        if let (Some(before), Some(now)) = (before, RootId::of(&folder).unwrap()) {
            assert!(!now.same_folder(&before));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_inside_another_is_recognised_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("Dropbox");
        std::fs::create_dir_all(outer.join("Backup")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Elsewhere")).unwrap();
        std::os::unix::fs::symlink(&outer, tmp.path().join("Link")).unwrap();
        assert!(RootId::is_within(&tmp.path().join("Link/Backup"), &outer));
        assert!(RootId::is_within(&outer, &tmp.path().join("Link")), "the same folder");
        assert!(!RootId::is_within(&outer, &outer.join("Backup")));
        assert!(!RootId::is_within(&tmp.path().join("Elsewhere"), &outer));
        assert!(!RootId::is_within(&tmp.path().join("missing"), &outer));
    }

    #[test]
    fn the_same_numbers_with_another_creation_time_are_another_folder() {
        let a = RootId { device: 1, index: 2, born: Some(1_000), mount: None, volume: None };
        let b = RootId { device: 1, index: 2, born: Some(2_000), mount: None, volume: None };
        let unknown = RootId { device: 1, index: 2, born: None, mount: None, volume: None };
        assert!(!a.same_folder(&b));
        assert!(a.same_folder(&unknown) && unknown.same_folder(&b));
        assert!(!a.same_folder(&RootId { device: 1, index: 3, born: Some(1_000), mount: None, volume: None }));
    }

    /// A drive cloned from another keeps its serial number, folder numbers and creation
    /// times. Both plugged in, only the volumes' own names tell them apart.
    #[test]
    fn a_folder_on_a_cloned_drive_is_another_place() {
        let original = RootId { device: 0x1234_abcd, index: 5, born: Some(1_000), mount: None, volume: Some(1) };
        let clone = RootId { volume: Some(2), ..original };
        assert!(!clone.same_place(&original) && !clone.same_folder(&original));
        let unknown = RootId { volume: None, ..original };
        assert!(unknown.same_place(&original) && clone.same_place(&unknown), "an unknown volume name isn't a difference");
    }

    #[test]
    fn the_volume_of_a_folder_is_read_from_its_real_path() {
        let guid = RootId::volume_in(r"\\?\Volume{3ec1fb8a-1f39-11e9-a4a5-806e6f6e6963}\Users\me\Photos");
        assert_eq!(guid, Some(0x3ec1fb8a_1f39_11e9_a4a5_806e6f6e6963));
        assert_eq!(RootId::volume_in(r"\\?\Volume{3ec1fb8a-1f39-11e9-a4a5-806e6f6e6963}\"), guid, "at the top of the drive");
        assert_eq!(RootId::volume_in(r"\\?\UNC\nas\share\Photos"), None);
        assert_eq!(RootId::volume_in(r"\\?\Volume{3ec1fb8a}\"), None);
        assert_eq!(RootId::volume_in(r"C:\Photos"), None);
    }

    /// On Windows, a folder on the computer's own drive knows the volume's own name, and two
    /// folders on one drive know the same one.
    #[cfg(windows)]
    #[test]
    fn folders_know_their_volume_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("inner")).unwrap();
        let (outer, inner) = (RootId::of(tmp.path()).unwrap().unwrap(), RootId::of(&tmp.path().join("inner")).unwrap().unwrap());
        assert!(outer.volume.is_some(), "{outer:?}");
        assert_eq!(outer.volume, inner.volume);
        assert!(RootId::is_within(&tmp.path().join("inner"), tmp.path()));
    }

    /// The same drive plugged in again keeps its numbers, but not its mount.
    #[test]
    fn the_same_folder_mounted_again_is_another_folder() {
        let before = RootId { device: 2049, index: 2, born: None, mount: Some(40), volume: None };
        let again = RootId { mount: Some(41), ..before };
        assert!(!again.same_folder(&before));
        assert!(RootId { mount: None, ..before }.same_folder(&again), "an unknown mount isn't a difference");
        assert!(again.same_place(&before), "at one moment, two mounts of a folder are one place");
    }

    #[test]
    fn an_identity_kept_by_an_older_charon_still_compares() {
        let older: RootId = serde_json::from_str(r#"{"device":2049,"index":2,"born":null}"#).unwrap();
        assert_eq!(older.mount, None);
        assert_eq!(older.volume, None);
        assert!(older.same_folder(&RootId { device: 2049, index: 2, born: None, mount: Some(40), volume: Some(7) }));
        assert!(!older.same_folder(&RootId { device: 2050, index: 2, born: None, mount: Some(40), volume: Some(7) }));
    }

    /// Where Linux numbers mounts for good, a folder knows the mount it's seen through.
    #[cfg(target_os = "linux")]
    #[test]
    fn folders_know_their_mount_where_linux_numbers_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (RootId::of(tmp.path()).unwrap().unwrap(), RootId::of(tmp.path()).unwrap().unwrap());
        assert_eq!(first, second);
        let (top, processes) = (RootId::of(Path::new("/")).unwrap().unwrap(), RootId::of(Path::new("/proc")).unwrap().unwrap());
        if let (Some(top), Some(processes)) = (top.mount, processes.mount) {
            assert_ne!(top, processes, "/proc is a mount of its own");
        }
    }
}
