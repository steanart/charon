//! How much room a drive has left, so a sync never fills one up.
//!
//! A copy that runs out of room fails cleanly (the half-written file is thrown away), but a
//! drive filled to the last byte makes other apps fail to save the person's work. So
//! Charon keeps a little room free on every drive it writes to, and a step that would eat
//! into it fails with a clear reason instead.
//!
//! On a Mac, the drive's room that the system can free by itself (caches, local Time
//! Machine snapshots, iCloud files kept online only) counts as free too: Finder counts it.
//!
//! A sync that replaces several files on a drive keeps a copy of each replaced version in
//! .charon-trash, made right before its new version takes its place. How much room those
//! copies need at one time depends on the drive (see `KeptCopies`).

use crate::model::human_bytes;
use crate::scan::{FsKind, Volume};
use std::path::Path;

/// The room on one drive, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    /// What may be written there now.
    pub free: u64,
    /// The drive's size.
    pub total: u64,
}

impl Room {
    /// The room on the drive of `path`, quickly. `None` where the drive can't tell.
    pub fn of(path: &Path) -> Option<Room> {
        system::room_of(path)
    }

    /// What Charon leaves free for everything else: 2% of the drive, at most 2 GB.
    pub fn reserve(&self) -> u64 {
        (self.total / 50).min(2_000_000_000)
    }

    /// Whether `bytes` more fit, leaving the reserve.
    pub fn fits(&self, bytes: u64) -> bool {
        self.free >= bytes.saturating_add(self.reserve())
    }

    /// Why `bytes` more don't fit on the drive of `path`, or `None` when they do (or when
    /// the drive can't tell). `drive` names the drive for the person ("the destination's
    /// drive").
    pub fn shortage(path: &Path, bytes: u64, drive: &str) -> Option<String> {
        let room = Self::of(path)?;
        if room.fits(bytes) {
            return None;
        }
        // Asking the system what it could free is slow, so only when it matters.
        let room = match system::freeable(path) {
            Some(free) if free > room.free => Room { free, ..room },
            _ => room,
        };
        (!room.fits(bytes)).then(|| {
            format!(
                "there isn't enough room on {drive}: this needs {}, and it has {} free, of which Charon keeps {} for other apps",
                human_bytes(bytes),
                human_bytes(room.free),
                human_bytes(room.reserve())
            )
        })
    }
}

/// How the copies of replaced versions that a sync keeps in .charon-trash take room on a
/// drive, where it replaces one file after the other: each copy is made right before its new
/// version takes the old version's place, and the old version is removed then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptCopies {
    /// The copies are clones (APFS): they take no room.
    Clones,
    /// Each old version gives its room back once it's removed: only the largest copy needs
    /// room at one time, but for those of files with hard links, which keep theirs.
    OneAtATime,
    /// A removed file's data may stay on the drive (in a snapshot, or on a network share's
    /// server), or Charon doesn't know the drive: every copy needs room of its own.
    All,
}

impl KeptCopies {
    /// How kept copies take room on `volume` (`None`: a drive that couldn't be asked).
    pub fn on(volume: Option<&Volume>) -> KeptCopies {
        match volume {
            Some(volume) if volume.kind == FsKind::Apfs => KeptCopies::Clones,
            Some(volume) if volume.frees_removed_files() => KeptCopies::OneAtATime,
            _ => KeptCopies::All,
        }
    }

    /// The most room that keeping copies of the `replaced` versions, one after the other,
    /// takes at one time. Each is given by its size and whether removing it gives its room
    /// back: a file with hard links doesn't, its data stays under its other names.
    pub fn room(self, replaced: impl IntoIterator<Item = (u64, bool)>) -> u64 {
        match self {
            KeptCopies::Clones => 0,
            KeptCopies::OneAtATime => {
                let (mut held, mut largest) = (0u64, 0u64);
                for (size, gives_back) in replaced {
                    if gives_back {
                        largest = largest.max(size);
                    } else {
                        held = held.saturating_add(size);
                    }
                }
                held.saturating_add(largest)
            }
            KeptCopies::All => replaced.into_iter().fold(0, |room, (size, _)| room.saturating_add(size)),
        }
    }
}

#[cfg(target_os = "linux")]
mod system {
    use super::Room;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn room_of(path: &Path) -> Option<Room> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `stats` is plain data the call fills in; `path` is NUL-terminated.
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        let block = stats.f_frsize as u64;
        Some(Room { free: (stats.f_bavail as u64).saturating_mul(block), total: (stats.f_blocks as u64).saturating_mul(block) })
    }

    pub fn freeable(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(target_os = "macos")]
mod system {
    use super::Room;
    use std::ffi::{c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn room_of(path: &Path) -> Option<Room> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `stats` is plain data the call fills in; `path` is NUL-terminated.
        let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        let block = stats.f_bsize as u64;
        Some(Room { free: stats.f_bavail.saturating_mul(block), total: stats.f_blocks.saturating_mul(block) })
    }

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFTypeRef;
        fn CFURLCreateFromFileSystemRepresentation(allocator: CFTypeRef, buffer: *const u8, length: isize, is_directory: u8) -> CFTypeRef;
        fn CFURLCopyResourcePropertyForKey(url: CFTypeRef, key: CFTypeRef, value: *mut CFTypeRef, error: *mut CFTypeRef) -> u8;
        fn CFGetTypeID(object: CFTypeRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> u8;
        fn CFRelease(object: CFTypeRef);
    }

    /// `kCFNumberSInt64Type`.
    const SINT64: isize = 4;

    /// The room for something the person asked for, counting what the system can free by
    /// itself (what Finder shows as available).
    pub fn freeable(path: &Path) -> Option<u64> {
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: every object created here is released once; `bytes` lives through the
        // call that reads it, and `number` is an i64 as SINT64 asks for.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
            if url.is_null() {
                return None;
            }
            let mut value: CFTypeRef = std::ptr::null();
            let found = CFURLCopyResourcePropertyForKey(url, kCFURLVolumeAvailableCapacityForImportantUsageKey, &mut value, std::ptr::null_mut()) != 0;
            CFRelease(url);
            if !found || value.is_null() {
                return None;
            }
            let mut number: i64 = 0;
            let read = CFGetTypeID(value) == CFNumberGetTypeID() && CFNumberGetValue(value, SINT64, &mut number as *mut i64 as *mut c_void) != 0;
            CFRelease(value);
            (read && number >= 0).then_some(number as u64)
        }
    }

}

#[cfg(windows)]
mod system {
    use super::Room;
    use crate::run::rename::wide;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn room_of(path: &Path) -> Option<Room> {
        let wide = wide(path);
        let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated and the three numbers are valid for writing.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
        (ok != 0).then_some(Room { free: available, total })
    }

    pub fn freeable(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod system {
    use super::Room;
    use std::path::Path;

    pub fn room_of(_path: &Path) -> Option<Room> {
        None
    }

    pub fn freeable(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drive_tells_its_room() {
        let tmp = tempfile::tempdir().unwrap();
        if let Some(room) = Room::of(tmp.path()) {
            assert!(room.total > 0 && room.free <= room.total, "{room:?}");
        }
        assert!(system::freeable(tmp.path()).is_none_or(|free| free > 0));
    }

    #[test]
    fn some_room_is_always_kept() {
        let small = Room { free: 1_000_000_000, total: 8_000_000_000 };
        assert_eq!(small.reserve(), 160_000_000);
        assert!(small.fits(800_000_000));
        assert!(!small.fits(900_000_000));
        let big = Room { free: 100 << 30, total: 2 << 40 };
        assert_eq!(big.reserve(), 2_000_000_000, "at most 2 GB");
        assert!(!Room { free: 10, total: 100 }.fits(u64::MAX), "never overflows");
    }

    /// Keeping the replaced versions of a sync takes the room of the largest of them where
    /// each gives its room back once removed (with those of files with hard links on top),
    /// of all of them where the drive may hold on to a removed file's data, and none where
    /// the copies are clones.
    #[test]
    fn kept_copies_take_room_as_their_drive_gives_it_back() {
        let drive = |kind: FsKind, fs: &str, network: bool| Volume {
            kind,
            fs: fs.into(),
            mount: "/x".into(),
            name: "x".into(),
            network,
            case_sensitive: false,
            case_guessed: false,
            refuses_links: false,
        };
        let on = |volume: Volume| KeptCopies::on(Some(&volume));
        assert_eq!(on(drive(FsKind::Apfs, "apfs", false)), KeptCopies::Clones);
        assert_eq!(on(drive(FsKind::Ntfs, "NTFS", false)), KeptCopies::OneAtATime);
        assert_eq!(on(drive(FsKind::Ext, "ext4", false)), KeptCopies::OneAtATime);
        assert_eq!(on(drive(FsKind::Exfat, "exfat", false)), KeptCopies::OneAtATime);
        assert_eq!(on(drive(FsKind::Btrfs, "btrfs", false)), KeptCopies::All, "a snapshot may hold on to it");
        assert_eq!(on(drive(FsKind::Smb, "smbfs", true)), KeptCopies::All, "so may a network share's server");
        assert_eq!(KeptCopies::on(None), KeptCopies::All);
        let given_back = [(3_000, true), (6_000, true), (4_000, true)];
        assert_eq!(KeptCopies::Clones.room(given_back), 0);
        assert_eq!(KeptCopies::OneAtATime.room(given_back), 6_000);
        assert_eq!(KeptCopies::All.room(given_back), 13_000);
        let one_hard_linked = [(3_000, true), (6_000, true), (4_000, false)];
        assert_eq!(KeptCopies::OneAtATime.room(one_hard_linked), 6_000 + 4_000, "the hard-linked file keeps its room");
        assert_eq!(KeptCopies::All.room(one_hard_linked), 13_000);
        assert_eq!(KeptCopies::Clones.room(one_hard_linked), 0);
        assert_eq!(KeptCopies::OneAtATime.room([]), 0);
        assert_eq!(KeptCopies::All.room([(u64::MAX, true), (1, true)]), u64::MAX, "never overflows");
        assert_eq!(KeptCopies::OneAtATime.room([(u64::MAX, false), (1, true)]), u64::MAX);
    }

    #[test]
    fn a_shortage_is_explained() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(room) = Room::of(tmp.path()) else { return };
        if room.fits(0) {
            assert_eq!(Room::shortage(tmp.path(), 0, "the destination's drive"), None);
        }
        let text = Room::shortage(tmp.path(), u64::MAX / 2, "the destination's drive").unwrap();
        assert!(text.starts_with("there isn't enough room on the destination's drive: this needs"), "{text}");
    }
}
