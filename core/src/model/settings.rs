//! App-wide settings.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Light, dark, or whatever the system uses.
    pub appearance: Appearance,
    /// Start Charon when you log in.
    pub launch_at_login: bool,
    /// Closing the window keeps Charon in the menu bar / system tray.
    pub run_in_background: bool,
    /// System notifications for errors, conflicts and big syncs.
    pub notifications: bool,
    /// Days that files stay in `.charon-trash` before being removed for good (0 = keep).
    pub trash_days: u32,
    /// "Pause everything": no job runs by itself.
    pub paused_all: bool,
    /// How often jobs that don't sync on change look for pending changes, in minutes.
    pub check_minutes: u32,
    /// Ask GitHub at start-up and once a day whether a newer Charon is out.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            launch_at_login: false,
            run_in_background: true,
            notifications: true,
            trash_days: 30,
            paused_all: false,
            check_minutes: 15,
            check_updates: true,
        }
    }
}

impl Settings {
    /// How long the regular look waits between looks at a job: `check_minutes`, and at
    /// least a minute.
    pub fn check_interval(&self) -> Duration {
        Duration::from_secs(self.check_minutes.max(1) as u64 * 60)
    }
}

/// The window's colour scheme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}
