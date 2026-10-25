//! Which file an item is on its drive, to tell it from another file saved under the same
//! name: its drive and file numbers (device and inode on macOS and Linux, volume serial
//! number and file index on Windows).

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity(u64, u64);

impl Identity {
    /// From an item's details, where they carry its numbers (macOS and Linux).
    pub fn of(meta: &fs::Metadata) -> Option<Identity> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Identity(meta.dev(), meta.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            None
        }
    }

    /// The item at `path`, without following a link there. `None` when it can't be told.
    pub fn at(path: &Path) -> Option<Identity> {
        #[cfg(windows)]
        {
            system::at(path)
        }
        #[cfg(not(windows))]
        {
            fs::symlink_metadata(path).ok().as_ref().and_then(Self::of)
        }
    }

    /// The file `file` is open on.
    pub fn of_file(file: &fs::File) -> Option<Identity> {
        #[cfg(windows)]
        {
            system::of_file(file)
        }
        #[cfg(not(windows))]
        {
            file.metadata().ok().as_ref().and_then(Self::of)
        }
    }

    /// How many names the item at `path` has on its drive, without following a link there:
    /// more than one for a file with hard links, whose data stays while any of them is left.
    /// `None` when it can't be told.
    pub fn names(path: &Path) -> Option<u64> {
        #[cfg(windows)]
        {
            system::names(path)
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::symlink_metadata(path).ok().map(|meta| meta.nlink())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            None
        }
    }
}

#[cfg(windows)]
mod system {
    use super::Identity;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    pub fn at(path: &Path) -> Option<Identity> {
        of_file(&open(path)?)
    }

    pub fn of_file(file: &File) -> Option<Identity> {
        let info = information(file)?;
        Some(Identity(info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }

    pub fn names(path: &Path) -> Option<u64> {
        information(&open(path)?).map(|info| info.nNumberOfLinks as u64)
    }

    /// Opened only to read its details: without access to its contents (so a file another
    /// app holds open opens too), a folder with backup semantics, and a link as itself.
    fn open(path: &Path) -> Option<File> {
        OpenOptions::new().access_mode(0).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).open(path).ok()
    }

    fn information(file: &File) -> Option<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open for the duration of the call and `info` is a valid buffer.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
        (ok != 0).then_some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_saved_under_the_same_name_is_another_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, saved) = (tmp.path().join("notes.txt"), tmp.path().join("saved"));
        fs::write(&path, "one").unwrap();
        let before = Identity::at(&path);
        assert!(before.is_some(), "macOS, Linux and Windows can tell");
        assert_eq!(Identity::of_file(&fs::File::open(&path).unwrap()), before);
        fs::write(&saved, "one").unwrap();
        fs::rename(&saved, &path).unwrap();
        assert_ne!(Identity::at(&path), before);
        assert!(Identity::at(&tmp.path().join("missing")).is_none());
    }

    #[test]
    fn a_file_with_hard_links_has_several_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, other) = (tmp.path().join("take.wav"), tmp.path().join("snapshot of take.wav"));
        fs::write(&path, "a take").unwrap();
        assert_eq!(Identity::names(&path), Some(1), "macOS, Linux and Windows can tell");
        if fs::hard_link(&path, &other).is_err() {
            eprintln!("SKIPPED a_file_with_hard_links_has_several_names: the temporary folder's drive holds no hard links");
            return;
        }
        assert_eq!(Identity::names(&path), Some(2));
        fs::remove_file(&other).unwrap();
        assert_eq!(Identity::names(&path), Some(1));
        assert_eq!(Identity::names(&tmp.path().join("missing")), None);
    }
}
