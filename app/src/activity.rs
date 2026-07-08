//! The "How Corti Works" window's data source: one poll command exposing the current pipeline stage
//! (which diagram box pulses) plus the tray status line as detail. Poll-only, like the diagnostics
//! console — no push event, no capability needed.

use std::sync::atomic::Ordering;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::imp::{AppState, Stage};

/// One read of live pipeline activity for the How-Corti-Works diagram. `stage` is the stable stage id
/// (mirrored in `app/ui/src/lib/pipeline.ts`); `detail` is the free-text tray status line; `recording`
/// is true while either capture source is live.
#[derive(Serialize)]
pub struct PipelineActivity {
    pub stage: String,
    pub detail: String,
    pub recording: bool,
}

#[tauri::command]
pub fn get_pipeline_activity(app: AppHandle) -> PipelineActivity {
    let Some(state) = app.try_state::<AppState>() else {
        return PipelineActivity {
            stage: Stage::Idle.as_str().to_string(),
            detail: String::new(),
            recording: false,
        };
    };
    PipelineActivity {
        stage: state.stage().as_str().to_string(),
        detail: state.status.lock().unwrap().clone(),
        recording: state.detector_recording.load(Ordering::Relaxed)
            || state.webinar_recording.load(Ordering::Relaxed),
    }
}
