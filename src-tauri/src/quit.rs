//! Quitting: asks first when changes still wait for the person's OK or files wait to be
//! compared, and always lets running syncs stop between two steps, so what they carried is
//! recorded and the two-way memory stays trustworthy.

use crate::state::AppState;
use charon_core::Overview;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// How long a quit waits for running syncs to stop. A sync stops between two files, and
/// inside a big file between two blocks, so this is plenty unless a drive hangs.
const STOP_WAIT: Duration = Duration::from_secs(30);

/// Payload of the "confirm-quit" event.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmQuit {
    /// Jobs whose changes wait for the person's OK.
    jobs: usize,
    /// Jobs with nothing else waiting but files the last look couldn't compare yet: they
    /// may differ, and may as well be identical.
    to_compare: usize,
}

impl ConfirmQuit {
    /// What the window asks before Charon quits, when any job's list waits for the person.
    fn of(overview: &Overview) -> Option<Self> {
        (overview.lists_waiting() > 0).then_some(Self { jobs: overview.waiting, to_compare: overview.to_compare })
    }
}

/// Payload of the "quitting" event: the syncs Charon stops before it goes.
#[derive(Clone, Serialize)]
struct Quitting {
    jobs: Vec<String>,
}

pub struct Quit {
    /// The syncs have stopped: the app may exit now.
    ready: AtomicBool,
}

impl Quit {
    pub fn new() -> Self {
        Self { ready: AtomicBool::new(false) }
    }

    /// The tray's Quit: when jobs have changes waiting for an OK, or files to compare, the
    /// window asks first (it answers with `quit_app`); otherwise Charon quits. The rows it
    /// counts ask the disk (a network share that doesn't answer takes a while), so not on the
    /// thread that draws the menu bar and the window.
    pub fn ask(app: &AppHandle) {
        let app = app.clone();
        std::thread::spawn(move || {
            let question = app.try_state::<AppState>().and_then(|state| ConfirmQuit::of(&state.engine.overview()));
            if let Some(question) = question {
                crate::show_main_window(&app);
                let _ = app.emit("confirm-quit", question);
            } else {
                Self::now(&app);
            }
        });
    }

    /// Stops the syncs (on a background thread, so the window stays responsive) and then
    /// exits. Asks nothing of the disk, as it may run on the thread that draws the window.
    pub fn now(app: &AppHandle) {
        if let Some(state) = app.try_state::<AppState>() {
            let running = state.engine.running();
            if !running.is_empty() {
                // The window says what it waits for, so Charon doesn't look stuck.
                let jobs = state.engine.job_list().into_iter().filter(|job| running.contains(&job.id)).map(|job| job.setup.name).collect();
                let _ = app.emit("quitting", Quitting { jobs });
            }
        }
        let app = app.clone();
        std::thread::spawn(move || {
            Self::stop_syncs(&app);
            app.exit(0);
        });
    }

    /// Whether the app may exit at once. When not, the caller holds the exit back and
    /// calls `now`, which exits once the syncs have stopped.
    pub fn may_exit(app: &AppHandle) -> bool {
        let ready = app.try_state::<Quit>().is_none_or(|quit| quit.ready.load(Ordering::SeqCst));
        let running = app.try_state::<AppState>().is_some_and(|state| !state.engine.running().is_empty());
        ready || !running
    }

    /// Stops every sync and waits for them. Safe to call more than once: once they were
    /// stopped (or waited for in vain), it returns at once, so a sync stuck on a share that
    /// doesn't answer holds up the exit only once.
    pub fn stop_syncs(app: &AppHandle) {
        if app.try_state::<Quit>().is_some_and(|quit| quit.ready.load(Ordering::SeqCst)) {
            return;
        }
        if let Some(state) = app.try_state::<AppState>() {
            if !state.engine.shut_down(STOP_WAIT) {
                log::error!("a sync didn't stop in time; its next look compares both folders afresh");
            }
        }
        if let Some(quit) = app.try_state::<Quit>() {
            quit.ready.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files a look couldn't compare yet are no changes that haven't crossed: the window asks
    /// about them in their own words.
    #[test]
    fn quitting_tells_files_to_compare_from_changes_waiting_for_an_ok() {
        let asked = |overview: Overview| ConfirmQuit::of(&overview).map(|question| (question.jobs, question.to_compare));
        assert_eq!(asked(Overview { jobs: 2, ..Overview::default() }), None);
        assert_eq!(asked(Overview { jobs: 2, to_compare: 1, ..Overview::default() }), Some((0, 1)));
        assert_eq!(asked(Overview { jobs: 2, waiting: 1, to_compare: 1, ..Overview::default() }), Some((1, 1)));
        assert_eq!(asked(Overview { jobs: 2, waiting: 2, ..Overview::default() }), Some((2, 0)));
    }
}
