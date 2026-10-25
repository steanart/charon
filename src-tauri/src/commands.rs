//! Everything the window can ask for. Errors reach the window as plain sentences.
//!
//! Commands that touch the disk (preparing and confirming lists, editing jobs, picking
//! folders, and anything that redraws the rows, which look at the jobs' folders) are `async`
//! and do their work on a background thread, so the window never freezes, even when a
//! network share is slow to answer.

use crate::about::About;
use crate::desktop::Desktop;
use crate::login::LaunchAtLogin;
use crate::quit::Quit;
use crate::state::AppState;
use crate::updates::{UpdateInfo, Updates};
use charon_core::plan::Side;
use charon_core::store::LogLine;
use charon_core::{
    Approval, ConflictChoice, ConflictItem, Engine, JobId, JobSetup, JobView, NotePage, Overview, Review, Settings, StepFilter, StepPage,
    SyncMode, SyncStart, UpgradeNotice,
};
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

type Answer<T> = Result<T, String>;

/// Turns any error into the sentence the window shows.
fn say<E: ToString>(error: E) -> String {
    error.to_string()
}

/// Runs `work` with the engine on a background thread.
async fn in_background<T: Send + 'static>(state: State<'_, AppState>, work: impl FnOnce(&Engine) -> charon_core::Result<T> + Send + 'static) -> Answer<T> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || work(&engine)).await.map_err(say)?.map_err(say)
}

/// Everything the window needs when it opens.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    jobs: Vec<JobView>,
    settings: Settings,
    overview: Overview,
    default_excludes: Vec<String>,
}

/// Every row looks at its folders (are they reachable?), which a slow network share can
/// take a while to answer: off the window's thread.
#[tauri::command]
pub async fn get_state(state: State<'_, AppState>) -> Answer<WindowState> {
    in_background(state, |engine| {
        Ok(WindowState {
            jobs: engine.jobs(),
            settings: engine.settings(),
            overview: engine.overview(),
            default_excludes: JobSetup::default_excludes(),
        })
    })
    .await
}

#[tauri::command]
pub fn about(app: AppHandle) -> About {
    About::of(&app)
}

// ---------------------------------------------------------------- updates

/// Asks GitHub whether a newer Charon is out.
#[tauri::command]
pub async fn check_updates(app: AppHandle) -> Answer<UpdateInfo> {
    tauri::async_runtime::spawn_blocking(move || Updates::check(&app)).await.map_err(say)?
}

/// The last answer from GitHub, if Charon has asked yet.
#[tauri::command]
pub fn update_info(app: AppHandle) -> Option<UpdateInfo> {
    Updates::last(&app)
}

// ---------------------------------------------------------------- jobs

#[tauri::command]
pub async fn add_job(state: State<'_, AppState>, setup: JobSetup) -> Answer<JobView> {
    in_background(state, move |engine| engine.add_job(setup)).await
}

/// Refused while the job syncs.
#[tauri::command]
pub async fn update_job(state: State<'_, AppState>, id: JobId, setup: JobSetup) -> Answer<JobView> {
    in_background(state, move |engine| engine.update_job(&id, setup)).await
}

/// The copy always starts with full automation off.
#[tauri::command]
pub async fn duplicate_job(state: State<'_, AppState>, id: JobId) -> Answer<JobView> {
    in_background(state, move |engine| engine.duplicate_job(&id)).await
}

/// Refused while the job syncs.
#[tauri::command]
pub async fn remove_job(state: State<'_, AppState>, id: JobId) -> Answer<()> {
    in_background(state, move |engine| engine.remove_job(&id)).await
}

#[tauri::command]
pub async fn move_job(state: State<'_, AppState>, id: JobId, to_index: usize) -> Answer<()> {
    in_background(state, move |engine| engine.move_job(&id, to_index)).await
}

#[tauri::command]
pub async fn set_paused(state: State<'_, AppState>, id: JobId, paused: bool) -> Answer<()> {
    in_background(state, move |engine| engine.set_paused(&id, paused)).await
}

#[tauri::command]
pub async fn set_all_paused(state: State<'_, AppState>, paused: bool) -> Answer<()> {
    in_background(state, move |engine| engine.set_all_paused(paused)).await
}

#[tauri::command]
pub fn suggest_name(source: PathBuf, destination: PathBuf, mode: SyncMode) -> String {
    JobSetup::suggested_name(&source, &destination, mode)
}

/// "Leave this folder out": an exclude for exactly that place. The next sync waits for an OK.
#[tauri::command]
pub async fn leave_out(state: State<'_, AppState>, id: JobId, side: Side, path: String) -> Answer<JobView> {
    in_background(state, move |engine| engine.leave_out(&id, side, &path)).await
}

// ---------------------------------------------------------------- syncing

/// Starts the sync of a job that runs by itself; for any other job the window shows the
/// list of changes first ("review").
#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>, id: JobId) -> Answer<SyncStart> {
    in_background(state, move |engine| engine.sync_now(&id)).await
}

/// Starts every job that runs by itself and returns the jobs whose lists need an OK.
/// It says so if the person paused everything while it was starting them.
#[tauri::command]
pub async fn sync_all(state: State<'_, AppState>) -> Answer<Vec<JobId>> {
    in_background(state, |engine| engine.sync_all()).await
}

/// Stops a running sync, or calls off a confirmed one that waits. Off the window's thread:
/// calling one off redraws the rows, which ask the disk.
#[tauri::command]
pub async fn cancel(state: State<'_, AppState>, id: JobId) -> Answer<()> {
    in_background(state, move |engine| {
        engine.cancel(&id);
        Ok(())
    })
    .await
}

/// Looks for changes in the background (changes nothing). Says why when the look waits for
/// another job on the same drive.
#[tauri::command]
pub fn check(state: State<AppState>, id: JobId) -> Answer<()> {
    state.engine.look_now(&id).map_err(say)
}

// ---------------------------------------------------------------- the list of changes

/// Looks at both folders now and prepares the list of changes for the person's OK.
#[tauri::command]
pub async fn prepare_review(state: State<'_, AppState>, id: JobId) -> Answer<Review> {
    in_background(state, move |engine| engine.prepare_review(&id)).await
}

/// Stops a `prepare_review` of the job that is still looking.
#[tauri::command]
pub fn cancel_review(state: State<AppState>, id: JobId) {
    state.engine.cancel_review(&id);
}

/// One page of a list's steps; with `folder`, only those at or inside that folder.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn review_steps(
    state: State<AppState>,
    id: JobId,
    review_id: String,
    filter: StepFilter,
    query: String,
    folder: Option<String>,
    offset: usize,
    limit: usize,
) -> Answer<StepPage> {
    state.engine.review_steps_in(&id, &review_id, filter, &query, folder.as_deref(), offset, limit).map_err(say)
}

/// One page of a list's notes.
#[tauri::command]
pub fn review_notes(state: State<AppState>, id: JobId, review_id: String, offset: usize, limit: usize) -> Answer<NotePage> {
    state.engine.review_notes(&id, &review_id, offset, limit).map_err(say)
}

/// The person confirmed the list: syncs exactly that list. Returns once it started, or once
/// it waits for another sync at work in the job's folders (it then starts by itself).
#[tauri::command]
pub async fn approve(state: State<'_, AppState>, id: JobId, review_id: String, acknowledged: bool) -> Answer<Approval> {
    in_background(state, move |engine| engine.approve(&id, &review_id, acknowledged)).await
}

/// The list was closed without confirming it. Off the window's thread: calling off a
/// confirmed sync that waits redraws the rows, which ask the disk.
#[tauri::command]
pub async fn discard_review(state: State<'_, AppState>, id: JobId, review_id: String) -> Answer<()> {
    in_background(state, move |engine| {
        engine.discard_review(&id, &review_id);
        Ok(())
    })
    .await
}

/// Prepares the list that settles one conflict the way the person chose.
#[tauri::command]
pub async fn prepare_resolution(state: State<'_, AppState>, id: JobId, conflict: ConflictItem, choice: ConflictChoice) -> Answer<Review> {
    in_background(state, move |engine| engine.prepare_resolution(&id, &conflict, choice)).await
}

#[tauri::command]
pub fn job_log(state: State<AppState>, id: JobId) -> Vec<LogLine> {
    state.engine.log(&id, 300)
}

// ---------------------------------------------------------------- after the update

/// What changed for the jobs that came over from an older version (once).
#[tauri::command]
pub fn upgrade_notice(state: State<AppState>) -> Option<UpgradeNotice> {
    state.engine.upgrade_notice()
}

#[tauri::command]
pub fn dismiss_upgrade_notice(state: State<AppState>) -> Answer<()> {
    state.engine.dismiss_upgrade_notice().map_err(say)
}

// ---------------------------------------------------------------- settings

#[tauri::command]
pub async fn set_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> Answer<()> {
    in_background(state, move |engine| {
        if engine.settings().launch_at_login != settings.launch_at_login {
            LaunchAtLogin::apply(&app, settings.launch_at_login);
        }
        engine.set_settings(settings)
    })
    .await
}

// ---------------------------------------------------------------- the desktop

/// Opens the system's folder picker. `None` when cancelled.
#[tauri::command]
pub async fn pick_folder(app: AppHandle, start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = app.dialog().file().set_title("Choose a folder");
    if let Some(start) = start.filter(|p| p.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    dialog.blocking_pick_folder().and_then(|folder| folder.into_path().ok())
}

#[tauri::command]
pub fn reveal_path(path: PathBuf) -> Answer<()> {
    Desktop::reveal(&path)
}

#[tauri::command]
pub fn open_url(url: String) -> Answer<()> {
    if !About::may_open(&url) {
        return Err("Charon only opens its own links.".into());
    }
    Desktop::open_url(&url)
}

/// macOS: opens Privacy & Security › Full Disk Access, where the person can let Charon
/// read protected folders. Elsewhere it does nothing.
#[tauri::command]
pub fn open_privacy_settings() -> Answer<()> {
    Desktop::open_privacy_settings()
}

/// Quits Charon (the window's quit confirmation said so). Running syncs stop cleanly first.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    Quit::now(&app);
}
