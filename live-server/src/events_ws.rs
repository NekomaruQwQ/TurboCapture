//! Unified events WebSocket — collapses KPM and strings-store push into a
//! single connection, replacing dedicated `/api/kpm` and `/api/strings/ws`
//! endpoints (which remain live during migration).
//!
//! ## Route
//!
//! - `WS /api/events` — pushes tagged JSON text frames:
//!   - `{"type":"kpm","kpm":N}` or `{"type":"kpm","kpm":null}`
//!   - `{"type":"strings","data":{...full key→value snapshot...}}`
//!
//! On connect: replays the current KPM value and full strings snapshot,
//! atomically (KPM first, strings second).  Thereafter every change to either
//! source produces a single tagged message.

use crate::state::AppState;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use serde_json::json;

use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/events", get(events_ws))
}

async fn events_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_events_ws(socket, state))
}

async fn handle_events_ws(mut socket: WebSocket, state: Arc<AppState>) {
    log::info!("events viewer connected");

    // Subscribe before reading initial snapshots so we don't miss mutations
    // that race between snapshot and subscribe.
    let mut kpm_rx = state.kpm.subscribe();
    let mut strings_rx = state.strings.read().await.subscribe();

    // Initial replay.
    let kpm_initial = *kpm_rx.borrow_and_update();
    if socket.send(Message::Text(kpm_message(kpm_initial).into())).await.is_err() {
        return;
    }
    if send_strings_snapshot(&mut socket, &state).await.is_err() {
        return;
    }
    // Mark strings as seen so the loop only fires on real changes.
    let _ = strings_rx.borrow_and_update();

    // Multiplex updates.  `select!` cancels the unselected futures, which is
    // safe for `watch::Receiver::changed()` (cancellation-safe per docs) and
    // for `WebSocket::recv()` (used here only to observe close, not to drive
    // protocol — drops are fine).
    loop {
        tokio::select! {
            r = kpm_rx.changed() => {
                if r.is_err() { break; }  // sender dropped
                let value = *kpm_rx.borrow_and_update();
                if socket.send(Message::Text(kpm_message(value).into())).await.is_err() {
                    break;
                }
            }
            r = strings_rx.changed() => {
                if r.is_err() { break; }
                let _ = strings_rx.borrow_and_update();
                if send_strings_snapshot(&mut socket, &state).await.is_err() {
                    break;
                }
            }
            // Observe client-initiated close; ignore other inbound frames.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    }

    log::info!("events viewer disconnected");
}

/// Format a KPM value as a tagged JSON text frame.
fn kpm_message(value: Option<i64>) -> String {
    match value {
        Some(n) => format!(r#"{{"type":"kpm","kpm":{n}}}"#),
        None    => r#"{"type":"kpm","kpm":null}"#.to_owned(),
    }
}

/// Serialize the current merged strings snapshot and send it as a tagged
/// JSON text frame.
async fn send_strings_snapshot(socket: &mut WebSocket, state: &Arc<AppState>) -> Result<(), ()> {
    let snapshot = state.strings.read().await.get_all();
    let text = serde_json::to_string(&json!({
        "type": "strings",
        "data": snapshot,
    })).map_err(|_err| ())?;
    socket.send(Message::Text(text.into())).await.map_err(|_err| ())
}
