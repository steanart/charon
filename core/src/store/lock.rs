//! Only one Charon may work with a data folder at a time. Two would run the same jobs
//! against each other, and each would trust a two-way memory the other one just changed.

use crate::error::{Error, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

/// An exclusive lock on `charon.lock` in the data folder, held for as long as the engine
/// lives. The system releases it when Charon quits, crashes or is killed.
#[derive(Debug)]
pub struct DataLock {
    _file: File,
}

impl DataLock {
    /// Takes the lock, or fails with `Error::AlreadyRunning` when another Charon holds it.
    pub fn acquire(folder: &Path) -> Result<Self> {
        fs::create_dir_all(folder).map_err(|e| Error::file(folder, e))?;
        let path = folder.join("charon.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::file(&path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
            // A disk without lock support. The app's data folder is always on the computer's
            // own disk, so this shouldn't happen; if it does, carry on rather than refuse to start.
            Err(TryLockError::Error(e)) => {
                log::warn!("couldn't lock {}: {e}", path.display());
                Ok(Self { _file: file })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_the_same_folder_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let first = DataLock::acquire(tmp.path()).unwrap();
        assert!(matches!(DataLock::acquire(tmp.path()), Err(Error::AlreadyRunning)));
        drop(first);
        assert!(DataLock::acquire(tmp.path()).is_ok(), "released when the first one goes away");
    }
}
