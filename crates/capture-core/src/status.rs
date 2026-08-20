//! Diagnostic status supplied by the native media owner.

use serde::{Deserialize, Serialize};

/// Externally meaningful lifecycle state of a running stream process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// No eligible capture target currently exists.
    #[default]
    Waiting,
    /// The media owner is replacing one ordinary target with another.
    Switching,
    /// Frames from the selected target are being captured and encoded.
    Capturing,
}

/// Human-meaningful target facts that deliberately omit native identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSummary {
    /// Selector profile that admitted the current target.
    pub profile: String,
    /// Executable name suitable for operator diagnostics.
    pub executable_name: String,
    /// Current window title.
    pub title: String,
}

/// Latest-value status snapshot published by the native media thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaStatus {
    /// Current target/capture lifecycle state.
    pub state: CaptureState,
    /// Current target summary, absent while waiting.
    pub target: Option<TargetSummary>,
    /// Total captured frames since process startup.
    pub captured_frames: u64,
    /// Total encoded access units since process startup.
    pub encoded_frames: u64,
    /// Recent capture rate measured by the media owner.
    pub capture_rate: f64,
    /// Recent encode rate measured by the media owner.
    pub encode_rate: f64,
}

impl Default for MediaStatus {
    fn default() -> Self {
        Self {
            state: CaptureState::Waiting,
            target: None,
            captured_frames: 0,
            encoded_frames: 0,
            capture_rate: 0.0,
            encode_rate: 0.0,
        }
    }
}
