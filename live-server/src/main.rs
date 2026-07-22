//! `live-server` — M4 relay server for Nekomaru LiveUI.
//!
//! Thin HTTP/WS relay.  All capture intelligence lives in the Rust workers
//! (`live-encoder`, `live-kpm`). The server relays binary frames, stores
//! config, and proxies frontend assets from Vite.
//!
//! ## Usage
//!
//! ```text
//! LIVE_PORT=3000 LIVE_VITE_PORT=5173 live-server
//! ```

mod audio;
mod events;
mod events_ws;
mod kpm;
mod selector;
mod state;
mod strings;
mod util;
mod video;
mod vite_proxy;

use state::AppState;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::post;
use clap::Parser;

use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;

// ── CLI ─────────────────────────────────────────────────────────────────

/// Nekomaru LiveUI — M4 relay server.
#[derive(Parser)]
#[command(name = "live-server")]
struct Cli {
    /// HTTP server port.
    #[arg(long, env = "LIVE_PORT")]
    port: u16,

    /// Vite dev server port.  Spawns `bunx vite` as a child process
    /// and proxies non-API requests to it.
    #[arg(long, env = "LIVE_VITE_PORT")]
    vite_port: u16,
}

// ── Main ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    use tap::prelude::*;
    use win32job::{Job, ExtendedLimitInfo};

    pretty_env_logger::init();

    let args = Cli::parse();
    let addr = format!("0.0.0.0:{}", args.port);
    let data_dir = resolve_data_dir();
    log::info!("data dir: {}", data_dir.display());

    // Job object auto-kills child processes when this process exits,
    // including crashes and Task Manager kills.
    let job =
        ExtendedLimitInfo::new()
            .limit_kill_on_job_close()
            .pipe(|info| Job::create_with_limit_info(info))
            .expect("failed to create job object");
    job
        .assign_current_process()
        .expect("failed to assign current process to job object");

    let _vite = spawn_vite(args.vite_port);

    let state = Arc::new(AppState::new(&data_dir));

    // Read revision timestamp from jj (non-fatal on failure).
    if let Some(ts) = read_jj_timestamp() {
        state.strings.write().await.set_computed("$timestamp", ts);
    }

    let app =
        Router::new()
            .merge(video::router())
            .merge(kpm::router())
            .merge(audio::router())
            .merge(strings::router())
            .merge(selector::router())
            .merge(events::router())
            .merge(events_ws::router())
            .route("/api/refresh", post(refresh))
            .with_state(Arc::clone(&state))
            .fallback(vite_proxy::fallback(args.vite_port));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    log::info!("listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            log::info!("Ctrl+C received, shutting down");
        })
        .await
        .expect("server error");

    // _job is dropped here → kills all child processes if still running.
}

// ── Refresh ─────────────────────────────────────────────────────────────

/// `POST /api/refresh` — reload string store and selector config from disk.
async fn refresh(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    state.strings.write().await.reload();
    state.selector.write().await.reload();
    Json(serde_json::json!({ "ok": true }))
}

// ── Data Dir Resolution ─────────────────────────────────────────────────

/// Resolve the `data/` directory relative to the repo root.
///
/// The binary lives at `<repo>/target/release/live-server.exe`, so we
/// walk up three parents to reach the repo root.  Falls back to `./data`
/// if resolution fails.
fn resolve_data_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"))
}

// ── Vite Dev Server ─────────────────────────────────────────────────────

/// Spawn `bunx vite` as a child process.  The server proxies non-API
/// requests to Vite for frontend assets.
fn spawn_vite(vite_port: u16) -> Option<Child> {
    let frontend_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(|d| d.join("frontend")))
        .unwrap_or_else(|| PathBuf::from("frontend"));

    log::info!("spawning vite on port {vite_port} (frontend: {})",
        frontend_dir.display());

    match std::process::Command::new("bunx")
        .arg("--bun")
        .arg("vite")
        .current_dir(&frontend_dir)
        .env("LIVE_VITE_PORT", vite_port.to_string())
        .spawn()
    {
        Ok(child) => Some(child),
        Err(e) => {
            log::error!("failed to spawn vite: {e}");
            None
        }
    }
}

// ── Jujutsu Timestamp ───────────────────────────────────────────────────

/// Read the committer timestamp of the `@-` revision via `jj log`.
fn read_jj_timestamp() -> Option<String> {
    let output = std::process::Command::new("jj")
        .args(["log", "-r", "@-", "--no-graph", "-T", "self.committer().timestamp()"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let ts = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if ts.is_empty() {
                log::warn!("jj log returned empty timestamp");
                None
            } else {
                log::info!("revision timestamp: {ts}");
                Some(ts)
            }
        }
        Ok(o) => {
            log::warn!("jj log failed: {}", String::from_utf8_lossy(&o.stderr).trim());
            None
        }
        Err(e) => {
            log::warn!("jj not available: {e}");
            None
        }
    }
}
