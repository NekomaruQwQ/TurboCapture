//! Key-value string store with computed strings and dual-layer persistence.
//!
//! Two persistence layers, loaded in order (higher layer wins on conflict):
//!   1. `data/strings.json`       — single JSON file for short, single-line values
//!   2. `data/strings/<key>.md`   — individual Markdown files for multiline content
//!
//! Computed strings (`$`-prefixed) are server-derived, readonly, in-memory only.
//! They are merged into GET responses but cannot be written or deleted via the API.
//!
//! ## Routes
//!
//! - `GET    /api/strings`           — all key-value pairs (user + computed)
//! - `GET    /api/strings/:key`      — single entry
//! - `WS     /api/strings/ws`        — server-push snapshot stream (full snapshot on connect + on each change)
//! - `PUT    /api/strings/:key`      — set a value (plain text body; 403 for `$`-prefixed keys)
//! - `DELETE /api/strings/:key`      — delete a value (403 for `$`-prefixed keys)
//! - `PUT    /internal/strings/:key` — set a computed string (plain text body; key must start with `$`)
//! - `DELETE /internal/strings/:key` — remove a computed string (key must start with `$`)

use crate::state::AppState;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, put};
use tokio::sync::watch;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

// ── Store ───────────────────────────────────────────────────────────────

pub struct StringStore {
    /// User-managed key-value pairs, persisted to disk.
    user: BTreeMap<String, String>,
    /// Server-derived readonly strings (`$`-prefixed), in-memory only.
    computed: BTreeMap<String, String>,
    /// Path to `data/strings.json`.
    json_path: PathBuf,
    /// Path to `data/strings/` directory.
    dir_path: PathBuf,
    /// Bumped on every mutation; viewers re-read the store on `changed()`.
    /// Multiple writes between polls coalesce into a single notification.
    version: u64,
    /// Notifies WS subscribers that the merged snapshot has changed.
    tx: watch::Sender<u64>,
}

impl StringStore {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let json_path = data_dir.join("strings.json");
        let dir_path = data_dir.join("strings");

        // Ensure data directories exist.
        let _ = std::fs::create_dir_all(&dir_path);

        let (tx, _) = watch::channel(0);
        let mut store = Self {
            user: BTreeMap::new(),
            computed: BTreeMap::new(),
            json_path,
            dir_path,
            version: 0,
            tx,
        };
        store.load_from_disk();
        store
    }

    /// Subscribe to change notifications.  Each `changed()` tick means the
    /// merged snapshot from `get_all()` may have new content.
    pub fn subscribe(&self) -> watch::Receiver<u64> { self.tx.subscribe() }

    /// Bump the version and notify subscribers.  Send errors (no listeners)
    /// are ignored — the watch channel is fire-and-forget here.
    fn notify_changed(&mut self) {
        self.version = self.version.wrapping_add(1);
        let _ = self.tx.send(self.version);
    }

    /// All entries merged: user store + computed (computed wins on conflict).
    pub fn get_all(&self) -> BTreeMap<String, String> {
        let mut result = self.user.clone();
        for (k, v) in &self.computed {
            result.insert(k.clone(), v.clone());
        }
        result
    }

    /// Set a user string.  Returns `Err` if the key is `$`-prefixed or invalid.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), StringStoreError> {
        if key.starts_with('$') {
            return Err(StringStoreError::ComputedReadonly);
        }
        if !is_valid_key(key) {
            return Err(StringStoreError::InvalidKey);
        }

        self.user.insert(key.to_owned(), value.to_owned());

        // Persist: multiline -> .md file (remove from JSON), single-line -> JSON (remove .md).
        if is_multiline(value) {
            let _ = std::fs::write(self.dir_path.join(format!("{key}.md")), value);
            self.remove_from_json(key);
        } else {
            let _ = std::fs::remove_file(self.dir_path.join(format!("{key}.md")));
            self.save_to_json(key, value);
        }

        self.notify_changed();
        Ok(())
    }

    /// Delete a user string.  Returns `Err` if the key is `$`-prefixed or invalid.
    pub fn delete(&mut self, key: &str) -> Result<(), StringStoreError> {
        if key.starts_with('$') {
            return Err(StringStoreError::ComputedReadonly);
        }
        if !is_valid_key(key) {
            return Err(StringStoreError::InvalidKey);
        }

        self.user.remove(key);
        self.remove_from_json(key);
        let _ = std::fs::remove_file(self.dir_path.join(format!("{key}.md")));

        self.notify_changed();
        Ok(())
    }

    /// Push a computed string.  Key must start with `$`.
    pub fn set_computed(&mut self, key: &str, value: String) {
        debug_assert!(key.starts_with('$'), "computed key must start with $");
        self.computed.insert(key.to_owned(), value);
        self.notify_changed();
    }

    /// Remove a computed string.
    pub fn clear_computed(&mut self, key: &str) {
        self.computed.remove(key);
        self.notify_changed();
    }

    /// Reload all user strings from disk (called by `POST /refresh`).
    pub fn reload(&mut self) {
        self.user.clear();
        self.load_from_disk();
        log::info!("reloaded {} user string entries", self.user.len());
        self.notify_changed();
    }

    // ── Internal ────────────────────────────────────────────────────────

    /// Load user strings from both disk layers.
    fn load_from_disk(&mut self) {
        // Layer 1: strings.json (lower priority).
        if let Ok(content) = std::fs::read_to_string(&self.json_path) {
            let map: BTreeMap<String, String> = serde_json::from_str(&content)
                .unwrap_or_else(|e| panic!("corrupt strings.json: {e}"));
            for (k, v) in map {
                self.user.insert(k, v);
            }
        }

        // Layer 2: data/strings/*.md (higher priority, overwrites JSON).
        if let Ok(entries) = std::fs::read_dir(&self.dir_path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(key) = name.strip_suffix(".md")
                    && is_valid_key(key)
                    && let Ok(content) = std::fs::read_to_string(entry.path())
                {
                    self.user.insert(key.to_owned(), content);
                }
            }
        }

        log::info!("loaded {} user string entries", self.user.len());
    }

    /// Update a single key in strings.json (load -> set -> save).
    fn save_to_json(&self, key: &str, value: &str) {
        let mut map: BTreeMap<String, String> = std::fs::read_to_string(&self.json_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        map.insert(key.to_owned(), value.to_owned());
        let json = crate::util::to_json(&map);
        let _ = std::fs::write(&self.json_path, json);
    }

    /// Remove a key from strings.json (load -> delete -> save).
    fn remove_from_json(&self, key: &str) {
        let Ok(content) = std::fs::read_to_string(&self.json_path) else { return };
        let Ok(mut map): Result<BTreeMap<String, String>, _> = serde_json::from_str(&content) else { return };
        if map.remove(key).is_some() {
            let json = crate::util::to_json(&map);
            let _ = std::fs::write(&self.json_path, json);
        }
    }
}

// ── Error Type ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum StringStoreError {
    ComputedReadonly,
    InvalidKey,
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Key must be alphanumeric, hyphens, underscores.
fn is_valid_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A value is multiline if it contains a newline after trimming trailing whitespace.
fn is_multiline(value: &str) -> bool { value.trim_end().contains('\n') }

// ── Routes ──────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/strings", get(get_all))
        .route("/api/strings/ws", get(subscribe_ws))
        .route("/api/strings/{key}", get(get_one).put(put_one).delete(delete_one))
        .route("/internal/strings/{key}", put(put_computed).delete(delete_computed))
}

/// `GET /api/strings` — return all entries as a flat JSON object.
async fn get_all(State(state): State<Arc<AppState>>) -> Json<BTreeMap<String, String>> {
    let store = state.strings.read().await;
    Json(store.get_all())
}

/// `WS /api/strings/ws` — push the merged snapshot on connect and on every change.
///
/// Late joiners receive the current snapshot immediately; thereafter every
/// mutation produces a fresh JSON message.  Multiple writes that land between
/// poll ticks coalesce into a single notification (watch-channel semantics),
/// which is fine because we always re-serialize the latest state.
async fn subscribe_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_subscribe_ws(socket, state))
}

async fn handle_subscribe_ws(mut socket: WebSocket, state: Arc<AppState>) {
    // Subscribe before reading the initial snapshot so we don't miss
    // mutations that race between snapshot and subscribe.
    let mut rx = state.strings.read().await.subscribe();

    log::info!("strings viewer connected");

    // Send current snapshot immediately on connect.
    if send_snapshot(&mut socket, &state).await.is_err() {
        return;
    }

    // Push on every change.
    while rx.changed().await.is_ok() {
        if send_snapshot(&mut socket, &state).await.is_err() {
            break;
        }
    }

    log::info!("strings viewer disconnected");
}

/// Serialize the current merged snapshot and send it as a single WS text frame.
async fn send_snapshot(socket: &mut WebSocket, state: &Arc<AppState>) -> Result<(), ()> {
    let snapshot = state.strings.read().await.get_all();
    let json = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_owned());
    socket.send(Message::Text(json.into())).await.map_err(|_err| ())
}

/// `GET /api/strings/:key` — return a single entry.
async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let all = state.strings.read().await.get_all();
    match all.get(&key) {
        Some(value) => Json(serde_json::json!({ "value": value })).into_response(),
        None => (StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" }))).into_response(),
    }
}

/// `PUT /api/strings/:key` — set a string value (plain text body).
async fn put_one(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    body: String,
) -> impl IntoResponse {
    let mut store = state.strings.write().await;
    match store.set(&key, &body) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(StringStoreError::ComputedReadonly) =>
            (StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "computed strings are readonly" }))).into_response(),
        Err(StringStoreError::InvalidKey) =>
            (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid key" }))).into_response(),
    }
}

/// `DELETE /api/strings/:key` — delete a string.
async fn delete_one(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut store = state.strings.write().await;
    match store.delete(&key) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(StringStoreError::ComputedReadonly) =>
            (StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "computed strings are readonly" }))).into_response(),
        Err(StringStoreError::InvalidKey) =>
            (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid key" }))).into_response(),
    }
}

/// `PUT /internal/strings/:key` — set a computed string from an external process (plain text body).
///
/// Key must start with `$` (returns 400 otherwise).  This is the bridge for
/// external scripts (e.g. Nushell) to write `$`-prefixed computed strings that
/// are readonly via the public API.
async fn put_computed(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    body: String,
) -> impl IntoResponse {
    if !key.starts_with('$') {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "key must start with $" }))).into_response();
    }
    state.strings.write().await.set_computed(&key, body);
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `DELETE /internal/strings/:key` — remove a computed string.
///
/// Key must start with `$` (returns 400 otherwise).  Used by external scripts
/// to signal absence (e.g. `$microphone` is deleted when Cubase is not running).
async fn delete_computed(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    if !key.starts_with('$') {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "key must start with $" }))).into_response();
    }
    state.strings.write().await.clear_computed(&key);
    Json(serde_json::json!({ "ok": true })).into_response()
}
