//! Sends engine events to the window (by name), the tray icon and notifications.

use crate::notifier::Notifier;
use crate::tray::Tray;
use charon_core::plan::Side;
use charon_core::{Emitter, EngineEvent, JobId, Progress};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter as _};

/// Payload of the "progress" event.
#[derive(Clone, Serialize)]
struct ProgressEvent {
    id: JobId,
    progress: Progress,
}

/// Payload of the "scan-progress" event.
#[derive(Clone, Serialize)]
struct ScanProgressEvent {
    id: JobId,
    side: Side,
    items: u64,
}

/// Payload of the "compare-progress" event.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompareProgressEvent {
    id: JobId,
    files: u64,
    total_files: u64,
    bytes: u64,
    total_bytes: u64,
}

/// Payload of the "review-stale" event.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewStaleEvent {
    id: JobId,
    review_id: String,
}

/// An emitter that forwards every engine event.
pub fn forward_to(app: &AppHandle) -> Emitter {
    let app = app.clone();
    Arc::new(move |event: EngineEvent| {
        let _ = match event {
            EngineEvent::Job(view) => app.emit("job", view),
            EngineEvent::JobRemoved(id) => app.emit("job-removed", id),
            EngineEvent::Progress { id, progress } => app.emit("progress", ProgressEvent { id, progress }),
            EngineEvent::Finished(finished) => {
                Notifier::announce(&app, &finished);
                app.emit("finished", finished)
            }
            EngineEvent::Overview(overview) => {
                Tray::refresh(&app, &overview);
                app.emit("overview", overview)
            }
            EngineEvent::Settings(settings) => app.emit("settings", settings),
            EngineEvent::ScanProgress { id, side, items } => app.emit("scan-progress", ScanProgressEvent { id, side, items }),
            EngineEvent::CompareProgress { id, files, total_files, bytes, total_bytes } => {
                app.emit("compare-progress", CompareProgressEvent { id, files, total_files, bytes, total_bytes })
            }
            EngineEvent::ReviewStale { id, review_id } => app.emit("review-stale", ReviewStaleEvent { id, review_id }),
            EngineEvent::Waiting(jobs) => {
                Notifier::waiting(&app, &jobs);
                Ok(())
            }
            EngineEvent::Held { name, reason, .. } => {
                Notifier::held(&app, &name, &reason);
                Ok(())
            }
        };
    })
}
