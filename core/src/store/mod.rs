//! What Charon keeps on disk, all inside the app's data folder.
//!
//! * `state_file.rs`    – `StateFile`: settings and jobs (`charon-state.json`), saved atomically
//! * `baselines.rs`     – `Baselines`: the two-way memory of each job
//! * `landings.rs`      – `Landings`: Mirror and Backup copies whose dates didn't stick
//! * `unattended.rs`    – `Unattended`: what automatic runs changed since the last confirmed list
//! * `absences.rs`      – `Absences`: items found missing on a share or cloud drive, and for how long
//! * `same_contents.rs` – `SameContents`: two-way pairs of files a full read found identical
//! * `journal.rs`       – `Journal`: one activity log per job
//! * `lock.rs`          – `DataLock`: only one Charon works with the data folder at a time
//! * `atomic.rs`        – writing a file so a crash never leaves it half-written

mod absences;
pub(crate) mod atomic;
mod baselines;
mod journal;
mod landings;
mod lock;
mod same_contents;
mod state_file;
mod unattended;

pub use absences::{Absence, Absences};
pub use baselines::{Baselines, Memory};
pub use journal::{Journal, LogLine};
pub use landings::Landings;
pub use lock::DataLock;
pub use same_contents::SameContents;
pub use state_file::{StateFile, StoredState};
pub use unattended::Unattended;

use std::path::{Path, PathBuf};

/// Where everything lives inside the data folder.
#[derive(Debug, Clone)]
pub struct DataFolder {
    root: PathBuf,
}

impl DataFolder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("charon-state.json")
    }

    /// Where Charon 0.2 kept its settings and jobs.
    pub fn legacy_state_file(&self) -> PathBuf {
        self.root.join("charon.json")
    }

    pub fn baselines(&self) -> PathBuf {
        self.root.join("baselines")
    }

    /// Mirror and Backup: the copies whose dates didn't stick (see `Landings`).
    pub fn landings(&self) -> PathBuf {
        self.root.join("landings")
    }

    /// Full automation: what automatic runs changed since the last confirmed list (see
    /// `Unattended`).
    pub fn unattended(&self) -> PathBuf {
        self.root.join("unattended")
    }

    /// Items found missing on a drive that may show changes late (see `Absences`).
    pub fn absences(&self) -> PathBuf {
        self.root.join("absences")
    }

    /// Two-way: pairs of files a full read found identical (see `SameContents`).
    pub fn same_contents(&self) -> PathBuf {
        self.root.join("same-contents")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The Charons that worked with this data folder, newest first, with the computer and the
    /// data folder each one ran with (see `run::BusyWatch`).
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances.json")
    }
}
