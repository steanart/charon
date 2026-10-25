//! Where Charon keeps its jobs, settings and logs.

use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// The app's identifier before 0.1.1, when Charon was published by DigiFix.
#[cfg_attr(debug_assertions, allow(dead_code))]
const OLD_IDENTIFIER: &str = "ninja.digifix.charon";

/// The files that hold Charon's jobs and settings (since 0.3, and before it). A data folder
/// holding one of them is Charon's; one without them holds no jobs yet.
#[cfg_attr(debug_assertions, allow(dead_code))]
const STATE_FILES: [&str; 2] = ["charon-state.json", "charon.json"];

/// A development build (`cargo tauri dev`) keeps its own data next to the installed app's,
/// so trying something out never touches the real jobs and their two-way memory.
#[cfg(debug_assertions)]
const DEV_FOLDER: &str = "dev.stean.charon-dev";

pub struct DataFolder;

impl DataFolder {
    /// The data folder (macOS: ~/Library/Application Support/dev.stean.charon/).
    /// The first time 0.1.1 or later starts, it moves the data of 0.1.0 into place, so jobs
    /// and settings carry over to the new identifier.
    #[cfg(not(debug_assertions))]
    pub fn open(app: &AppHandle) -> tauri::Result<PathBuf> {
        let folder = app.path().app_data_dir()?;
        if let Some(old) = folder.parent().map(|parent| parent.join(OLD_IDENTIFIER)) {
            Self::carry_over(&old, &folder);
        }
        Ok(folder)
    }

    /// A development build's own data folder (macOS:
    /// ~/Library/Application Support/dev.stean.charon-dev/). Nothing is moved into it.
    #[cfg(debug_assertions)]
    pub fn open(app: &AppHandle) -> tauri::Result<PathBuf> {
        let installed = app.path().app_data_dir()?;
        Ok(installed.parent().map(|parent| parent.join(DEV_FOLDER)).unwrap_or(installed))
    }

    /// Moves the data of Charon 0.1.0 from `old` into `folder`. The new folder may exist
    /// already: on Linux the log and the window's own storage live inside it and are made
    /// before Charon gets here. So what counts is whether it holds jobs yet; when it does,
    /// nothing is moved.
    ///
    /// Each item moves on its own, and nothing already in `folder` is replaced (a folder of
    /// the same name has its items moved into it, one by one). The files with the jobs move
    /// last, so if Charon is stopped halfway, the next start finds no jobs yet and moves the
    /// rest. (Jobs that arrive without their two-way memory compare both folders afresh, and
    /// that removes nothing.)
    #[cfg_attr(debug_assertions, allow(dead_code))]
    fn carry_over(old: &Path, folder: &Path) {
        if !old.is_dir() || Self::holds_jobs(folder) || !Self::holds_jobs(old) {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(folder) {
            log::error!("couldn't make the data folder {}: {e}", folder.display());
            return;
        }
        Self::move_items(old, folder, &|name| !STATE_FILES.contains(&name));
        Self::move_items(old, folder, &|name| STATE_FILES.contains(&name));
        match std::fs::remove_dir(old) {
            Ok(()) => log::info!("moved the data of Charon 0.1.0 from {}", old.display()),
            Err(_) => log::warn!("moved the jobs of Charon 0.1.0; what couldn't move is still in {}", old.display()),
        }
    }

    /// Whether `folder` holds Charon's jobs.
    #[cfg_attr(debug_assertions, allow(dead_code))]
    fn holds_jobs(folder: &Path) -> bool {
        STATE_FILES.iter().any(|name| folder.join(name).symlink_metadata().is_ok())
    }

    /// Moves the items of `from` whose names `wanted` lets through into `to`, never
    /// replacing one that is there already.
    #[cfg_attr(debug_assertions, allow(dead_code))]
    fn move_items(from: &Path, to: &Path, wanted: &dyn Fn(&str) -> bool) {
        let items = match std::fs::read_dir(from) {
            Ok(items) => items,
            Err(e) => {
                log::error!("couldn't read {}: {e}", from.display());
                return;
            }
        };
        for item in items.flatten() {
            let name = item.file_name();
            if !wanted(&name.to_string_lossy()) {
                continue;
            }
            let (source, target) = (item.path(), to.join(&name));
            if target.symlink_metadata().is_err() {
                if let Err(e) = std::fs::rename(&source, &target) {
                    log::error!("couldn't move {} to {}: {e}", source.display(), target.display());
                }
            } else if source.is_dir() && !source.is_symlink() && target.is_dir() {
                Self::move_items(&source, &target, &|_| true);
                let _ = std::fs::remove_dir(&source);
            } else {
                log::warn!("left {} where it was: {} is there already", source.display(), target.display());
            }
        }
    }

    /// The name of the log file (in the system's log folder for Charon).
    pub fn log_name() -> &'static str {
        if cfg!(debug_assertions) {
            "charon-dev"
        } else {
            "charon"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn the_jobs_of_0_1_0_move_into_a_folder_the_log_made_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join(OLD_IDENTIFIER), tmp.path().join("dev.stean.charon"));
        write(&old.join("charon.json"), "jobs");
        write(&old.join("baselines").join("job.json"), "memory");
        write(&old.join("logs").join("job.log"), "old journal");
        write(&old.join("logs").join("charon.log"), "old app log");
        // On Linux the app's log is in the data folder, and is written before Charon starts.
        write(&new.join("logs").join("charon.log"), "new app log");

        DataFolder::carry_over(&old, &new);
        assert_eq!(fs::read_to_string(new.join("charon.json")).unwrap(), "jobs");
        assert_eq!(fs::read_to_string(new.join("baselines").join("job.json")).unwrap(), "memory");
        assert_eq!(fs::read_to_string(new.join("logs").join("job.log")).unwrap(), "old journal");
        assert_eq!(fs::read_to_string(new.join("logs").join("charon.log")).unwrap(), "new app log", "nothing is replaced");
        assert_eq!(fs::read_to_string(old.join("logs").join("charon.log")).unwrap(), "old app log", "what can't move stays");
    }

    #[test]
    fn nothing_moves_into_a_folder_that_holds_jobs_already() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join(OLD_IDENTIFIER), tmp.path().join("dev.stean.charon"));
        write(&old.join("charon.json"), "old jobs");
        write(&old.join("baselines").join("job.json"), "old memory");
        write(&new.join("charon-state.json"), "new jobs");

        DataFolder::carry_over(&old, &new);
        assert!(!new.join("baselines").exists());
        assert!(old.join("charon.json").exists() && old.join("baselines").join("job.json").exists());
    }

    #[test]
    fn the_jobs_move_after_their_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join(OLD_IDENTIFIER), tmp.path().join("dev.stean.charon"));
        write(&old.join("charon.json"), "jobs");
        write(&old.join("baselines").join("job.json"), "memory");
        // The memory can't move (something of that name is in the way): the jobs wait with it.
        write(&new.join("baselines"), "in the way");
        DataFolder::carry_over(&old, &new);
        assert!(new.join("charon.json").exists(), "the jobs move; the next look compares afresh without memory");

        // Interrupted before the jobs moved: the next start moves the rest.
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join(OLD_IDENTIFIER), tmp.path().join("dev.stean.charon"));
        write(&old.join("charon.json"), "jobs");
        write(&new.join("baselines").join("job.json"), "memory moved before the stop");
        DataFolder::carry_over(&old, &new);
        assert_eq!(fs::read_to_string(new.join("charon.json")).unwrap(), "jobs");
        assert!(!old.exists(), "the empty old folder goes");
    }
}
