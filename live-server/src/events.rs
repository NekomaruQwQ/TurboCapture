//! Worker info endpoints.
//!
//! Internal HTTP endpoints called by capture workers to report metadata.
//!
//! ## Routes
//!
//! - `POST /internal/streams/:streamId/info` — capture metadata from the
//!   transitional auto encoder or the `live-stream` supervisor.

use crate::state::AppState;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Json;
use axum::routing::post;
use serde::Deserialize;

use std::sync::Arc;

// ── Computed String IDs ─────────────────────────────────────────────────

/// Human-readable label for the captured window.
const CSID_CAPTURE_INFO: &str = "$captureInfo";

/// Current capture mode (e.g. `"auto"`).
const CSID_CAPTURE_MODE: &str = "$captureMode";

/// Mode tag from the matched pattern (e.g. `"code"`, `"game"`).
const CSID_LIVE_MODE: &str = "$liveMode";

// ── Routes ──────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/internal/streams/{streamId}/info", post(stream_info))
}

// ── Stream Info ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamInfoBody {
    /// Whether the stream currently has an allowed selected target.
    #[serde(default = "default_active")]
    active: bool,
    #[expect(dead_code, reason = "received but not used by the server")]
    hwnd: String,
    title: String,
    file_description: String,
    /// Selector profile label used by frontend color-key selection.
    mode: Option<String>,
    /// Supervisor-owned topology name; absent legacy requests remain `auto`.
    #[serde(default = "default_capture_mode")]
    capture_mode: String,
}

/// Legacy requests predate the explicit lifecycle field and are active updates.
const fn default_active() -> bool { true }

/// Preserve the transitional encoder's historical computed metadata value.
fn default_capture_mode() -> String { "auto".to_owned() }

/// `POST /internal/streams/:streamId/info` — periodic capture metadata.
///
/// Transitional auto mode sends periodic active updates. `live-stream` sends
/// selection transitions and explicit inactive updates during generation loss.
async fn stream_info(
    State(state): State<Arc<AppState>>,
    Path(_stream_id): Path<String>,
    Json(body): Json<StreamInfoBody>,
) -> Json<serde_json::Value> {
    let mut store = state.strings.write().await;

    store.set_computed(CSID_CAPTURE_MODE, body.capture_mode);
    if body.active {
        let info = if body.file_description.is_empty() {
            body.title
        } else {
            body.file_description
        };
        store.set_computed(CSID_CAPTURE_INFO, info);
        if let Some(mode) = body.mode {
            store.set_computed(CSID_LIVE_MODE, mode);
        } else {
            store.clear_computed(CSID_LIVE_MODE);
        }
    } else {
        store.clear_computed(CSID_CAPTURE_INFO);
        store.clear_computed(CSID_LIVE_MODE);
    }
    drop(store);

    Json(serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_metadata_defaults_to_active_auto_mode() {
        let body: StreamInfoBody = serde_json::from_str(r#"{
            "hwnd": "0x1234",
            "title": "Editor",
            "file_description": "Code",
            "mode": "code"
        }"#).unwrap();
        assert!(body.active);
        assert_eq!(body.capture_mode, "auto");
    }

    #[test]
    fn supervisor_can_clear_metadata_without_fake_window_values() {
        let body: StreamInfoBody = serde_json::from_str(r#"{
            "active": false,
            "hwnd": "",
            "title": "",
            "file_description": "",
            "mode": null,
            "capture_mode": "shared"
        }"#).unwrap();
        assert!(!body.active);
        assert_eq!(body.capture_mode, "shared");
    }
}
