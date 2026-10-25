//! Asking the operating system to show a folder or open a link: Finder / `open` on macOS,
//! Explorer on Windows, the default file manager and browser (`xdg-open`) on Linux.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Desktop;

impl Desktop {
    /// Shows a file or folder in the file manager. If it no longer exists, opens the
    /// closest folder that still does.
    pub fn reveal(path: &Path) -> Result<(), String> {
        let existing = Self::closest_existing(path).ok_or("This location doesn't exist.")?;
        let select = existing == path; // select the item itself, not just its folder
        Self::reveal_command(&existing, select)
            .status()
            .map(|_| ())
            .map_err(|e| format!("Couldn't open the file manager: {e}"))
    }

    /// Opens a web page in the default browser.
    pub fn open_url(url: &str) -> Result<(), String> {
        Self::open_command(url)
            .status()
            .map(|_| ())
            .map_err(|e| format!("Couldn't open the browser: {e}"))
    }

    /// macOS: opens System Settings › Privacy & Security › Full Disk Access. Other systems
    /// have no such switch, so it does nothing there.
    pub fn open_privacy_settings() -> Result<(), String> {
        if cfg!(target_os = "macos") {
            Self::open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        } else {
            Ok(())
        }
    }

    /// `path`, or the nearest parent folder that exists.
    fn closest_existing(path: &Path) -> Option<PathBuf> {
        let mut current = path.to_path_buf();
        while !current.exists() {
            current = current.parent()?.to_path_buf();
        }
        Some(current)
    }

    // ---------------------------------------------------------------- per platform

    #[cfg(target_os = "macos")]
    fn reveal_command(path: &Path, select: bool) -> Command {
        let mut cmd = Command::new("open");
        if select {
            cmd.arg("-R");
        }
        cmd.arg(path);
        cmd
    }

    #[cfg(target_os = "macos")]
    fn open_command(url: &str) -> Command {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    }

    #[cfg(windows)]
    fn reveal_command(path: &Path, select: bool) -> Command {
        let mut cmd = Command::new("explorer");
        if select {
            // Explorer expects "/select,<path>" as a single argument.
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(path);
            cmd.arg(arg);
        } else {
            cmd.arg(path);
        }
        cmd
    }

    #[cfg(windows)]
    fn open_command(url: &str) -> Command {
        // Hands the link to Windows' default handler without going through a shell.
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn reveal_command(path: &Path, select: bool) -> Command {
        // File managers on Linux can't reliably select an item, so open its folder.
        let folder = if select && path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let mut cmd = Command::new("xdg-open");
        cmd.arg(folder);
        cmd
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn open_command(url: &str) -> Command {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    }
}
