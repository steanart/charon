//! Reading files' contents, and comparing two files byte for byte.
//!
//! Before the first two-way sync (or when Charon starts afresh) there is no memory of which
//! side changed what. Two files of the same size with different times may well be the same
//! file, copied by hand or by a tool that didn't keep the time. Rather than calling every
//! such pair a conflict, Charon reads both and compares.
//!
//! Whatever Charon reads, it never downloads: a file that is only in the cloud (iCloud
//! Drive, OneDrive, Dropbox free up space that way, also after a scan) is refused instead.

use crate::scan::Scanner;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const CHUNK: usize = 1024 * 1024;

pub struct Contents;

/// A file found only in the cloud when Charon came to read it (see `Contents::open`).
#[derive(Debug)]
struct OnlyInTheCloud;

impl std::fmt::Display for OnlyInTheCloud {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("it's only in the cloud (not downloaded to this computer)")
    }
}

impl std::error::Error for OnlyInTheCloud {}

impl Contents {
    /// Opens the file at `path` to read its contents, never downloading it. On macOS a
    /// thread that reads contents can't download (see `never_download_on_this_thread`):
    /// reading such a file fails. On Windows a file only in the cloud reads like any other
    /// and is downloaded as it's read, so it's opened without being brought back where the
    /// system allows that (`FILE_FLAG_OPEN_NO_RECALL`). On both, the opened file's own
    /// details must say its contents are on this computer.
    pub fn open(path: &Path) -> io::Result<File> {
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_NO_RECALL;
            std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_NO_RECALL).open(path)?
        };
        #[cfg(not(windows))]
        let file = File::open(path)?;
        if Scanner::placeholder(&file.metadata()?).is_some() {
            return Err(io::Error::other(OnlyInTheCloud));
        }
        Ok(file)
    }

    /// Whether `error`, from opening or reading a file, says it's only in the cloud.
    pub fn only_in_the_cloud(error: &io::Error) -> bool {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EDEADLK) {
            return true;
        }
        error.get_ref().is_some_and(|inner| inner.is::<OnlyInTheCloud>())
    }

    /// Whether the files at `a` and `b` hold exactly the same bytes. Stops early with
    /// `Interrupted` when `cancel` becomes true: before each step on either file, as one may
    /// have waited for its drive (a share that didn't answer) while the other's drive was
    /// given to someone else. Calls `moved` after each piece compared. When it ends after it
    /// was told to stop, however it ends, it doesn't close the files it has open: it hands
    /// them to `left`, with their paths (closing a file is work for its drive too).
    pub fn same(a: &Path, b: &Path, cancel: Option<&AtomicBool>, moved: &dyn Fn(), left: &mut Vec<(PathBuf, File)>) -> io::Result<bool> {
        let stopped = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
        let (mut first, mut second) = (None, None);
        let same = Self::compare(a, b, &stopped, moved, &mut first, &mut second);
        if stopped() {
            left.extend([first.map(|file| (a.to_path_buf(), file)), second.map(|file| (b.to_path_buf(), file))].into_iter().flatten());
        }
        same
    }

    /// `same`, keeping the files it opens in `first` and `second`.
    fn compare(a: &Path, b: &Path, stopped: &dyn Fn() -> bool, moved: &dyn Fn(), first: &mut Option<File>, second: &mut Option<File>) -> io::Result<bool> {
        let go_on = || match stopped() {
            true => Err(io::Error::new(io::ErrorKind::Interrupted, "stopped")),
            false => Ok(()),
        };
        let first = first.insert(Self::open(a)?);
        go_on()?;
        let second = second.insert(Self::open(b)?);
        go_on()?;
        let size = first.metadata()?.len();
        go_on()?;
        if size != second.metadata()?.len() {
            return Ok(false);
        }
        let (mut left, mut right) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
        loop {
            go_on()?;
            let n = Self::fill(first, &mut left)?;
            go_on()?;
            let m = Self::fill(second, &mut right)?;
            if n != m || left[..n] != right[..m] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
            moved();
        }
    }

    /// Reads until `buffer` is full or the file ends.
    fn fill(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_bytes_not_times() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
        std::fs::write(&a, "same words").unwrap();
        std::fs::write(&b, "same words").unwrap();
        std::fs::write(&c, "some words").unwrap();
        assert!(Contents::same(&a, &b, None, &|| {}, &mut Vec::new()).unwrap());
        assert!(!Contents::same(&a, &c, None, &|| {}, &mut Vec::new()).unwrap());
    }

    #[test]
    fn says_when_it_compared_each_piece() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        let bytes = vec![7u8; CHUNK * 2 + CHUNK / 2];
        std::fs::write(&a, &bytes).unwrap();
        std::fs::write(&b, &bytes).unwrap();
        let pieces = std::cell::Cell::new(0);
        assert!(Contents::same(&a, &b, None, &|| pieces.set(pieces.get() + 1), &mut Vec::new()).unwrap());
        assert_eq!(pieces.get(), 3);
    }

    #[test]
    fn a_comparison_told_to_stop_goes_on_to_the_other_file_no_more() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::write(&a, "words").unwrap();
        let stop = AtomicBool::new(true);
        let mut left = Vec::new();
        let stopped = Contents::same(&a, &tmp.path().join("b, never opened"), Some(&stop), &|| {}, &mut left).unwrap_err();
        assert_eq!(stopped.kind(), io::ErrorKind::Interrupted, "{stopped}");
        // It doesn't close what it opened: that's for its caller to do, when the drive is free.
        assert_eq!(left.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>(), vec![a.clone()]);
        // Not told to stop, it hands over nothing.
        let b = tmp.path().join("b");
        std::fs::write(&b, "other").unwrap();
        let mut left = Vec::new();
        assert!(!Contents::same(&a, &b, Some(&AtomicBool::new(false)), &|| {}, &mut left).unwrap());
        assert!(left.is_empty());
    }

    #[test]
    fn only_a_file_found_only_in_the_cloud_says_so() {
        assert!(Contents::only_in_the_cloud(&io::Error::other(OnlyInTheCloud)));
        assert!(!Contents::only_in_the_cloud(&io::Error::other("the disk broke")));
        assert!(!Contents::only_in_the_cloud(&io::Error::from(io::ErrorKind::NotFound)));
        #[cfg(target_os = "macos")]
        assert!(Contents::only_in_the_cloud(&io::Error::from_raw_os_error(libc::EDEADLK)));
    }
}
