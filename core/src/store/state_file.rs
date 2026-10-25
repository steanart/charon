//! `charon-state.json`: the settings and every job.
//!
//! Charon 0.2 kept them in `charon.json`. On the first start of 0.3 they move over, and the
//! old file is renamed to `charon.json.moved-to-0.3`, so an old copy of the app that's
//! started by mistake finds no jobs to run (it would run them without the safety checks).

use crate::error::{Error, Result};
use crate::model::{Job, Settings, Trigger, UpgradeNotice, UpgradedJob};
use crate::store::atomic;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

const VERSION: u32 = 2;

/// What's inside the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredState {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// Shown once after the move from 0.2: the jobs that used to sync by themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_notice: Option<UpgradeNotice>,
}

impl Default for StoredState {
    fn default() -> Self {
        Self { version: VERSION, settings: Settings::default(), jobs: Vec::new(), upgrade_notice: None }
    }
}

pub struct StateFile {
    path: PathBuf,
    /// Where Charon 0.2 kept the same data.
    legacy: PathBuf,
}

impl StateFile {
    pub fn new(path: PathBuf, legacy: PathBuf) -> Self {
        Self { path, legacy }
    }

    /// Reads the file; a missing file means a fresh start (or a move from 0.2). A damaged
    /// file is kept aside as `….damaged-<time>` rather than overwritten.
    pub fn load(&self) -> Result<StoredState> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.move_from_legacy(),
            Err(e) => return Err(Error::file(&self.path, e)),
        };
        match serde_json::from_slice(&bytes) {
            Ok(state) => Ok(state),
            Err(e) => {
                let aside = self.path.with_extension(format!("json.damaged-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
                log::error!("{} is damaged ({e}); kept as {} and starting fresh", self.path.display(), aside.display());
                std::fs::rename(&self.path, &aside).map_err(|e| Error::file(&self.path, e))?;
                Ok(StoredState::default())
            }
        }
    }

    pub fn save(&self, state: &StoredState) -> Result<()> {
        atomic::write(&self.path, &serde_json::to_vec_pretty(state)?)
    }

    /// When Charon 0.2's state file last changed, if it's there. An older Charon still
    /// running writes it again (see `Engine::older_charon_running`).
    pub fn legacy_changed_at(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.legacy).and_then(|metadata| metadata.modified()).ok()
    }

    /// Takes over Charon 0.2's settings and jobs. Every job comes over waiting for the
    /// person's OK (and with full automation off), as the new fields' defaults say.
    fn move_from_legacy(&self) -> Result<StoredState> {
        let bytes = match std::fs::read(&self.legacy) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StoredState::default()),
            Err(e) => return Err(Error::file(&self.legacy, e)),
        };
        let mut state: StoredState = match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(e) => {
                log::error!("{} is damaged ({e}); starting fresh and leaving it where it is", self.legacy.display());
                return Ok(StoredState::default());
            }
        };
        state.version = VERSION;
        let automatic: Vec<UpgradedJob> = state
            .jobs
            .iter()
            .filter(|job| job.setup.trigger != Trigger::Manual)
            .map(|job| UpgradedJob { id: job.id.clone(), name: job.setup.name.clone(), trigger: job.setup.trigger })
            .collect();
        if !automatic.is_empty() {
            state.upgrade_notice = Some(UpgradeNotice { version: env!("CARGO_PKG_VERSION").into(), jobs: automatic, journaled: false });
        }
        self.save(&state)?;
        let moved = self.legacy.with_extension("json.moved-to-0.3");
        if let Err(e) = std::fs::rename(&self.legacy, &moved) {
            log::warn!("couldn't rename {}: {e}", self.legacy.display());
        }
        log::info!("moved {} jobs from {}", state.jobs.len(), self.legacy.display());
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_move_over_from_the_old_file_and_wait_for_an_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("charon.json");
        let old = r#"{"version":1,"settings":{"trashDays":7},"jobs":[{"id":"j1","name":"Docs","source":"/a","destination":"/b",
            "mode":"two-way","conflict":"ask","trigger":{"kind":"on-change","settleSeconds":5},"excludes":[],"safeDelete":true,
            "paused":false,"createdAt":"2026-10-01T10:00:00Z","lastRun":null,"lastSynced":"2026-10-18T10:00:00Z"}]}"#;
        std::fs::write(&legacy, old).unwrap();
        let file = StateFile::new(tmp.path().join("charon-state.json"), legacy.clone());
        let state = file.load().unwrap();
        assert_eq!(state.settings.trash_days, 7);
        let job = &state.jobs[0];
        assert!(job.needs_review && !job.setup.full_automation);
        let notice = state.upgrade_notice.as_ref().expect("the job used to sync by itself");
        assert_eq!(notice.jobs[0].name, "Docs");
        assert!(!legacy.exists(), "an old copy of the app must not find the jobs");
        assert!(tmp.path().join("charon.json.moved-to-0.3").exists());
        // From now on the new file is read.
        assert_eq!(file.load().unwrap().jobs.len(), 1);
    }

    #[test]
    fn a_damaged_file_is_kept_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("charon-state.json");
        std::fs::write(&path, "{broken").unwrap();
        let state = StateFile::new(path.clone(), tmp.path().join("charon.json")).load().unwrap();
        assert!(state.jobs.is_empty());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
