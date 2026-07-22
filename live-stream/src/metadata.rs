//! Selector JSONL consumption and non-blocking stream-metadata posting.

use std::{
    io::{BufRead as _, BufReader},
    process::ChildStdout,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::Context as _;

/// Maximum accepted selector metadata line before parsing is skipped.
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// Bound each metadata request so an unavailable server cannot accumulate work.
const POST_TIMEOUT: Duration = Duration::from_secs(2);

/// JSONL event emitted by `live-selector` in standalone and managed operation.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SelectorEvent {
    /// A complete safety-policy match selected a capture target.
    Selected {
        /// Hexadecimal HWND string emitted without JSON integer truncation.
        hwnd: String,
        /// Current Win32 window title.
        title: String,
        /// Executable description, falling back to the title.
        file_description: String,
        /// First matching enabled selector profile.
        profile: String,
    },
    /// Active policy revoked the prior target.
    Cleared,
}

/// Server request body extending the legacy auto-mode metadata contract.
#[derive(Debug, serde::Serialize, PartialEq, Eq)]
struct StreamInfoUpdate {
    /// Whether target-specific computed strings should be present.
    active: bool,
    /// Opaque HWND retained for compatibility and server diagnostics.
    hwnd: String,
    /// Window title used when no executable description exists.
    title: String,
    /// Preferred human-readable capture label.
    file_description: String,
    /// Selector profile label used by frontend presentation policy.
    mode: Option<String>,
    /// Supervisor-owned stream topology name.
    capture_mode: String,
}

impl StreamInfoUpdate {
    /// Convert a selector transition without interpreting its safety policy.
    fn from_event(event: SelectorEvent, capture_mode: &str) -> Self {
        match event {
            SelectorEvent::Selected {
                hwnd,
                title,
                file_description,
                profile,
            } => Self {
                active: true,
                hwnd,
                title,
                file_description,
                mode: Some(profile),
                capture_mode: capture_mode.to_owned(),
            },
            SelectorEvent::Cleared => Self::inactive(capture_mode),
        }
    }

    /// Clear target-specific metadata while retaining the supervisor mode.
    fn inactive(capture_mode: &str) -> Self {
        Self {
            active: false,
            hwnd: String::new(),
            title: String::new(),
            file_description: String::new(),
            mode: None,
            capture_mode: capture_mode.to_owned(),
        }
    }
}

/// Metadata work item tagged with its resource generation for diagnostics.
struct PostTask {
    /// Resource generation active when this transition was observed.
    generation: u64,
    /// Complete backward-compatible server update.
    update: StreamInfoUpdate,
}

/// Cloneable, non-media channel into one bounded-time HTTP worker.
#[derive(Clone)]
pub struct MetadataPoster {
    /// Unbounded queue is safe because the trusted selector emits only changes.
    sender: mpsc::Sender<PostTask>,
}

impl MetadataPoster {
    /// Start one reusable-agent poster independent from capture and supervision.
    pub fn spawn(url: String, stream_id: String) -> anyhow::Result<Self> {
        let (sender, receiver) = mpsc::channel::<PostTask>();
        thread::Builder::new()
            .name("stream-metadata".to_owned())
            .spawn(move || post_loop(&url, &stream_id, receiver))
            .context("failed to spawn stream metadata worker")?;
        Ok(Self { sender })
    }

    /// Queue one selector transition without waiting for the remote server.
    pub fn post_event(&self, generation: u64, event: SelectorEvent, capture_mode: &str) {
        self.queue(PostTask {
            generation,
            update: StreamInfoUpdate::from_event(event, capture_mode),
        });
    }

    /// Queue an explicit inactive state for startup or generation replacement.
    pub fn post_inactive(&self, generation: u64, capture_mode: &str) {
        self.queue(PostTask {
            generation,
            update: StreamInfoUpdate::inactive(capture_mode),
        });
    }

    /// Treat poster shutdown as non-fatal because transport is not capture policy.
    fn queue(&self, task: PostTask) {
        if self.sender.send(task).is_err() {
            log::warn!("stream metadata worker is unavailable");
        }
    }
}

/// Read complete selector JSON lines and forward only validated event shapes.
pub fn spawn_selector_reader(
    stdout: ChildStdout,
    generation: u64,
    capture_mode: &'static str,
    poster: MetadataPoster) -> anyhow::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("selector-metadata-{generation}"))
        .spawn(move || selector_reader(stdout, generation, capture_mode, &poster))
        .context("failed to spawn selector metadata reader")
}

/// Blocking line reader isolated from the supervisor control loop.
fn selector_reader(
    stdout: ChildStdout,
    generation: u64,
    capture_mode: &'static str,
    poster: &MetadataPoster) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_EVENT_BYTES => {
                log::warn!(
                    "generation {generation}: ignoring oversized selector metadata line ({} bytes)",
                    line.len());
            }
            Ok(_) => match serde_json::from_str::<SelectorEvent>(line.trim_end()) {
                Ok(event) => poster.post_event(generation, event, capture_mode),
                Err(error) => log::warn!(
                    "generation {generation}: invalid selector metadata JSON: {error}"),
            },
            Err(error) => {
                log::warn!(
                    "generation {generation}: failed to read selector metadata: {error}");
                break;
            }
        }
    }
}

/// Reuse one HTTP connection pool and drain every response for keep-alive reuse.
fn post_loop(url: &str, stream_id: &str, receiver: mpsc::Receiver<PostTask>) {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(POST_TIMEOUT))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    for task in receiver {
        let body = match serde_json::to_vec(&task.update) {
            Ok(body) => body,
            Err(error) => {
                log::error!(
                    "@{stream_id} generation {}: failed to serialize metadata: {error}",
                    task.generation);
                continue;
            }
        };
        match agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(&body)
        {
            Ok(mut response) => {
                let _ = response.body_mut().read_to_string();
            }
            Err(error) => log::warn!(
                "@{stream_id} generation {}: failed to POST stream metadata: {error}",
                task.generation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_event_preserves_profile_and_capture_facts() {
        let event: SelectorEvent = serde_json::from_str(r#"{
            "event":"selected",
            "hwnd":"0x1234",
            "title":"Editor",
            "file_description":"Code",
            "profile":"code"
        }"#).unwrap();
        let update = StreamInfoUpdate::from_event(event, "shared");
        assert!(update.active);
        assert_eq!(update.hwnd, "0x1234");
        assert_eq!(update.mode.as_deref(), Some("code"));
        assert_eq!(update.capture_mode, "shared");
    }

    #[test]
    fn clear_event_removes_target_specific_values() {
        let update = StreamInfoUpdate::from_event(SelectorEvent::Cleared, "shared");
        assert!(!update.active);
        assert!(update.hwnd.is_empty());
        assert!(update.mode.is_none());
        assert_eq!(update.capture_mode, "shared");
    }
}
