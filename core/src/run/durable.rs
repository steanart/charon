//! Making sure what a run did is really on the drive before Charon writes down that it
//! happened.
//!
//! Drives keep recent changes in memory for a while. If a USB drive is pulled out right
//! after "Sync finished", or the power goes, renames and removals from the last seconds can
//! vanish, while the two-way memory on the computer says they happened. The next sync would
//! then read the missing files as deleted on the drive. So at the end of a run every folder
//! the run changed is flushed, then the drive itself, and only then is anything recorded.

use std::io;
use std::path::Path;

/// Writes a file's contents and details through to its drive. On macOS that's a full flush
/// of the drive's cache (as std's `sync_all` does), and where a drive can't do that (some
/// network shares refuse it), a plain flush.
pub fn sync_file(file: &std::fs::File) -> io::Result<()> {
    match file.sync_all() {
        Ok(()) => Ok(()),
        #[cfg(target_os = "macos")]
        Err(e) if matches!(e.raw_os_error(), Some(code) if code == libc::ENOTSUP || code == libc::EINVAL || code == libc::ENOTTY) => {
            use std::os::unix::io::AsRawFd;
            // SAFETY: the descriptor belongs to `file`, which is open for the whole call.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        Err(e) => Err(e),
    }
}

/// Flushes the list of a folder (new, renamed and removed names) to its drive.
pub fn sync_folder(folder: &Path) -> io::Result<()> {
    system::sync_folder(folder)
}

/// Asks the drive that holds `folder` to write everything it still keeps in its own memory.
pub fn sync_drive(folder: &Path) -> io::Result<()> {
    system::sync_drive(folder)
}

#[cfg(unix)]
mod system {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    pub fn sync_folder(folder: &Path) -> io::Result<()> {
        let dir = File::open(folder)?;
        // Plain fsync: on macOS std's sync_all would ask for a full drive flush every time,
        // which `sync_drive` does once at the end instead.
        // SAFETY: the descriptor belongs to `dir`, which is open for the whole call.
        if unsafe { libc::fsync(dir.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            // Some drives (and some network shares) can't flush a folder; they write it
            // through anyway.
            match error.raw_os_error() {
                Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP => Ok(()),
                _ => Err(error),
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn sync_drive(folder: &Path) -> io::Result<()> {
        let dir = File::open(folder)?;
        // SAFETY: as above. F_FULLFSYNC flushes the whole drive's cache, not only this folder.
        if unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        // Not every drive supports it (network shares); a plain flush is the best there is.
        sync_folder(folder)
    }

    #[cfg(target_os = "linux")]
    pub fn sync_drive(folder: &Path) -> io::Result<()> {
        let dir = File::open(folder)?;
        // SAFETY: as above.
        if unsafe { libc::syncfs(dir.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn sync_drive(folder: &Path) -> io::Result<()> {
        sync_folder(folder)
    }
}

/// Windows: files are flushed before they're renamed, and a rename reaches the drive when
/// its folder is flushed (MOVEFILE_WRITE_THROUGH, which Charon passes, only makes a
/// difference for moves between drives, which Charon never makes). Opening a folder to
/// flush it needs write access to it; where Windows refuses that, the drive writes the
/// folder's changes in its own time.
#[cfg(windows)]
mod system {
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    pub fn sync_folder(folder: &Path) -> io::Result<()> {
        match std::fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(folder) {
            Ok(dir) => {
                if let Err(e) = dir.sync_all() {
                    log::debug!("couldn't flush {}: {e}", folder.display());
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(e),
            Err(e) => {
                log::debug!("couldn't open {} to flush it: {e}", folder.display());
                Ok(())
            }
        }
    }

    pub fn sync_drive(_folder: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod system {
    use std::io;
    use std::path::Path;

    pub fn sync_folder(_folder: &Path) -> io::Result<()> {
        Ok(())
    }

    pub fn sync_drive(_folder: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushing_a_folder_and_its_drive_works() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "x").unwrap();
        sync_folder(tmp.path()).unwrap();
        sync_drive(tmp.path()).unwrap();
    }
}
