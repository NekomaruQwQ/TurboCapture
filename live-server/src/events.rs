//! Worker info endpoints.
//!
//! Internal HTTP endpoints called by `live-stream` to report capture metadata.
//!
//! ## Routes
//!
//! - `POST /internal/streams/:streamId/info` — supervised capture metadata.

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

/// Supervisor-owned capture topology (e.g. `"main"`).
const CSID_CAPTURE_MODE: &str = "$captureMode";

/// Profile matched by the selected window (e.g. `"code"`, `"game"`).
const CSID_LIVE_PROFILE: &str = "$liveProfile";

// ── Routes ──────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/internal/streams/{streamId}/info", post(stream_info))
}

// ── Stream Info ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamInfoBody {
    /// Whether the stream currently has an allowed selected target.
    active: bool,
    #[expect(dead_code, reason = "received but not used by the server")]
    hwnd: String,
    title: String,
    file_description: String,
    /// Selector profile label used by frontend color-key selection.
    profile: Option<String>,
    /// Supervisor-owned topology name.
    capture_mode: String,
}

/// `POST /internal/streams/:streamId/info` — capture transition metadata.
///
/// `live-stream` sends selection transitions and explicit inactive updates
/// during generation loss.
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
        if let Some(profile) = body.profile {
            store.set_computed(CSID_LIVE_PROFILE, profile);
        } else {
            store.clear_computed(CSID_LIVE_PROFILE);
        }
    } else {
        store.clear_computed(CSID_CAPTURE_INFO);
        store.clear_computed(CSID_LIVE_PROFILE);
    }
    drop(store);

    Json(serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_requires_explicit_supervisor_lifecycle() {
        let result = serde_json::from_str::<StreamInfoBody>(r#"{
            "hwnd": "0x1234",
            "title": "Editor",
            "file_description": "Code",
            "profile": "code"
        }"#);
        assert!(result.is_err());
    }

    #[test]
    fn supervisor_can_clear_metadata_without_fake_window_values() {
        let body: StreamInfoBody = serde_json::from_str(r#"{
            "active": false,
            "hwnd": "",
            "title": "",
            "file_description": "",
            "profile": null,
            "capture_mode": "main"
        }"#).unwrap();
        assert!(!body.active);
        assert_eq!(body.capture_mode, "main");
    }

    #[test]
    fn legacy_mode_field_is_rejected() {
        let result = serde_json::from_str::<StreamInfoBody>(r#"{
            "active": true,
            "hwnd": "0x1234",
            "title": "Editor",
            "file_description": "Code",
            "mode": "code",
            "capture_mode": "main"
        }"#);
        assert!(result.is_err());
    }
}
