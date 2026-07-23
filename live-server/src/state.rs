//! Shared application state for the M4 relay server.
//!
//! Wrapped in `Arc<AppState>` and injected into Axum handlers via `State`.
//! Each subsystem owns its state behind a separate lock — no global
//! contention.

use crate::audio::AudioState;
use crate::kpm::KpmState;
use crate::strings::StringStore;
use crate::video::VideoState;

use std::path::Path;
use tokio::sync::RwLock;

/// Top-level server state shared across all Axum handlers.
pub struct AppState {
    pub strings: RwLock<StringStore>,
    pub video: VideoState,
    pub kpm: KpmState,
    pub audio: AudioState,
}

impl AppState {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            strings: RwLock::new(StringStore::new(data_dir)),
            video: VideoState::new(),
            kpm: KpmState::new(),
            audio: AudioState::new(),
        }
    }
}
