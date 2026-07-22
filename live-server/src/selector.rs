//! Selector config storage and routes.
//!
//! The server stores the selector config; transitional `live-encoder --mode auto` polls it.
//! No in-process selector management—the legacy implementation lives in `live-encoder`.
//!
//! ## Routes
//!
//! - `GET /api/selector/config`  — full preset config
//! - `PUT /api/selector/config`  — replace full config
//! - `PUT /api/selector/preset`  — switch active preset by name

use crate::state::AppState;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── Config Type ─────────────────────────────────────────────────────────

/// Full preset config shape persisted to disk.
///
/// Matches the JSON format consumed by transitional `live-encoder --mode auto`:
/// ```json
/// {
///   "preset": "main",
///   "presets": { "main": ["@code devenv.exe", ...] }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetConfig {
    pub preset: String,
    pub presets: HashMap<String, Vec<String>>,
}

// ── Selector Config ─────────────────────────────────────────────────────

/// Server-side selector config state.  Just storage + persistence —
/// no foreground polling or pattern matching (currently `live-encoder`'s legacy job).
pub struct SelectorConfig {
    pub config: PresetConfig,
    pub config_path: PathBuf,
}

impl SelectorConfig {
    /// Load from disk.  Falls back to defaults if the file is missing.
    pub fn load(data_dir: &Path) -> Self {
        let config_path = data_dir.join("selector-config.json");

        let config = if let Ok(content) = std::fs::read_to_string(&config_path) {
            serde_json::from_str::<PresetConfig>(&content)
                .unwrap_or_else(|e| {
                    log::warn!("corrupt selector-config.json, using defaults: {e}");
                    default_config()
                })
        } else {
            log::info!("no selector config found, using defaults");
            default_config()
        };

        log::info!("loaded selector config: preset=\"{}\", {} preset(s)",
            config.preset, config.presets.len());

        Self { config, config_path }
    }

    /// Persist to disk.
    pub fn save(&self) {
        let json = crate::util::to_json(&self.config);
        let _ = std::fs::write(&self.config_path, json);
    }

    /// Reload from disk (called by `POST /refresh`).
    pub fn reload(&mut self) {
        if let Ok(content) = std::fs::read_to_string(&self.config_path)
            && let Ok(config) = serde_json::from_str::<PresetConfig>(&content)
        {
            self.config = config;
            log::info!("reloaded selector config: preset=\"{}\", {} preset(s)",
                self.config.preset, self.config.presets.len());
        }
    }
}

/// Default selector config, used when `data/selector-config.json` is missing.
fn default_config() -> PresetConfig {
    PresetConfig {
        preset: "main".to_owned(),
        presets: HashMap::from([(
            "main".to_owned(),
            vec![
                "@code devenv.exe".to_owned(),
                "@code C:/Program Files/Microsoft Visual Studio Code/Code.exe".to_owned(),
                "@code C:/Program Files/JetBrains/".to_owned(),
                "@game D:/7-Games/".to_owned(),
                "@game D:/7-Games.Steam/steamapps/common/".to_owned(),
                "@game E:/Nekomaru-Games/".to_owned(),
                "@game E:/SteamLibrary/steamapps/common/".to_owned(),
                "@exclude gogh.exe".to_owned(),
                "@exclude vtube studio.exe".to_owned(),
            ],
        )]),
    }
}

// ── Routes ──────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/selector/config",
            get(get_config).put(set_config))
        .route("/api/selector/preset",
            put(set_preset))
}

/// `GET /api/selector/config` — full preset config.
async fn get_config(State(state): State<Arc<AppState>>) -> Json<PresetConfig> {
    let sel = state.selector.read().await;
    Json(sel.config.clone())
}

/// `PUT /api/selector/config` — replace full config.
async fn set_config(
    State(state): State<Arc<AppState>>,
    Json(config): Json<PresetConfig>,
) -> Json<serde_json::Value> {
    let mut sel = state.selector.write().await;
    log::info!("config updated: preset=\"{}\", {} preset(s)",
        config.preset, config.presets.len());
    sel.config = config;
    sel.save();
    drop(sel);
    Json(serde_json::json!({ "ok": true }))
}

/// `PUT /api/selector/preset` — switch active preset by name.
async fn set_preset(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> impl IntoResponse {
    let name = String::from_utf8_lossy(&body).trim().to_owned();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "preset name required" }))).into_response();
    }

    let mut sel = state.selector.write().await;

    // Reload from disk first (picks up hand-edits).
    sel.reload();

    if !sel.config.presets.contains_key(&name) {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("preset \"{name}\" not found") }))).into_response();
    }

    name.clone_into(&mut sel.config.preset);
    sel.save();
    drop(sel);
    log::info!("switched to preset \"{name}\"");

    Json(serde_json::json!({ "ok": true })).into_response()
}
