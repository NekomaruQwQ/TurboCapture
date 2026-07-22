//! Auto-selector: foreground window polling + pattern matching.
//!
//! Runs on a dedicated thread, polls the foreground window every 2 seconds,
//! matches against patterns from the server config, and sends swap commands
//! to the capture loop via a channel. Stream-info POSTs are dispatched to a
//! small worker thread (`spawn_info_poster`) so server stalls never freeze the
//! polling loop; the worker also re-posts the cached info every tick (heartbeat)
//! so computed strings stay fresh after a server restart.
//!
//! ## Ordering invariant (best-effort)
//!
//! On a matched swap, the stream-info POST is queued *before* the swap
//! command is sent to the capture loop, so the frontend's `$captureInfo` /
//! `$liveMode` updates land ahead of any new-window video frames.  Strict
//! ordering held when the POST was synchronous; with the worker-thread
//! design the POST may still be in flight when the swap command is
//! dispatched, but localhost POST + WS broadcast (~5–15 ms) is well under
//! the capture + NVENC + WS pipeline (~20–60 ms), so the frontend almost
//! always receives the string update first.  Features that gate rendering
//! on these strings (e.g. color-key props in `App.svelte`) would otherwise
//! flicker for one frame at every swap.

pub mod config;

use config::{PresetConfig, should_capture};

use std::sync::mpsc;
use std::time::Duration;

/// Command sent from the selector thread to the capture loop.
pub struct SwapCommand {
    /// New window handle to capture.
    pub hwnd: isize,
    /// Human-readable label for the captured window.
    pub capture_info: String,
}

/// Configuration for the selector thread.
pub struct SelectorConfig {
    /// URL to poll for the selector preset config (GET, returns JSON).
    pub config_url: String,
    /// URL to POST stream info on every poll tick.
    pub info_url: String,
    /// Poll interval for foreground window checks.
    pub poll_interval: Duration,
}

/// One stream-info POST task, sent from the selector loop to the poster
/// worker over an mpsc channel.
struct PostTask {
    hwnd: isize,
    title: String,
    file_description: String,
    mode: Option<String>,
    /// Set on swap ticks.  The worker preserves these (never coalesces
    /// across one) so the new window's title reaches the server in order.
    is_swap: bool,
}

/// Cached stream info from the last successful pattern match.
/// Re-posted on every tick to keep the server's computed strings fresh.
struct CachedStreamInfo {
    hwnd: isize,
    title: String,
    file_description: String,
    mode: Option<String>,
}

impl CachedStreamInfo {
    fn to_task(&self, is_swap: bool) -> PostTask {
        PostTask {
            hwnd: self.hwnd,
            title: self.title.clone(),
            file_description: self.file_description.clone(),
            mode: self.mode.clone(),
            is_swap,
        }
    }
}

/// Spawn the selector polling thread. Returns the receiving end of the swap
/// command channel. The thread exits after the receiver is dropped and its
/// next attempted selection send fails.
pub fn spawn_selector(config: SelectorConfig) -> mpsc::Receiver<SwapCommand> {
    let (tx, rx) = mpsc::channel();
    let post_tx = spawn_info_poster(config.info_url.clone());

    std::thread::Builder::new()
        .name("selector".into())
        .spawn(move || selector_loop(&tx, &post_tx, &config))
        .expect("failed to spawn selector thread");

    rx
}

/// Spawn the stream-info poster worker thread.  Owns a single `ureq::Agent`
/// so HTTP keep-alive is reused across POSTs, and processes tasks FIFO.
///
/// Heartbeat tasks queued behind another heartbeat are coalesced — only
/// the most recent state matters, and dropping older entries prevents a
/// flood when the server recovers from a stall.  Swap tasks are flagged
/// and never dropped (and coalescing stops the moment one is reached, so
/// they're never skipped over either).
///
/// Exits when the sender is dropped (selector loop terminated).
fn spawn_info_poster(url: String) -> mpsc::Sender<PostTask> {
    let (tx, rx) = mpsc::channel::<PostTask>();

    std::thread::Builder::new()
        .name("selector-poster".into())
        .spawn(move || {
            // Owns one Agent so HTTP keep-alive is reused across POSTs.
            // Reuse depends on `post_stream_info` draining the response
            // body — without that, the pool stays empty and every POST
            // pays a fresh-connect cost.
            let agent = ureq::Agent::new_with_defaults();

            while let Ok(mut task) = rx.recv() {
                // While the head is a heartbeat, peek further entries and
                // adopt the latest.  Stops at the first swap or empty queue.
                // A heartbeat in front of a swap is replaced by the swap —
                // the swap carries the up-to-date state, so nothing is lost.
                while !task.is_swap {
                    match rx.try_recv() {
                        Ok(next) => task = next,
                        Err(_) => break,
                    }
                }

                post_stream_info(&agent, &url, &task);
            }
        })
        .expect("failed to spawn selector-poster thread");

    tx
}

/// Main selector loop. Polls the foreground window, matches patterns, sends
/// swap commands to the capture loop, and queues capture-info POSTs.
fn selector_loop(
    tx: &mpsc::Sender<SwapCommand>,
    post_tx: &mpsc::Sender<PostTask>,
    config: &SelectorConfig,
) {
    // Poll config on first iteration, then every ~10 iterations (20s at 2s poll).
    const CONFIG_POLL_EVERY: u32 = 10;

    log::info!("selector started (poll: {:?}, config: {})",
        config.poll_interval, config.config_url);

    let mut last_info: Option<CachedStreamInfo> = None;
    let mut preset_config: Option<PresetConfig> = None;
    let mut config_poll_counter: u32 = 0;

    loop {
        std::thread::sleep(config.poll_interval);

        // Periodically refresh the preset config from the server.
        if config_poll_counter.is_multiple_of(CONFIG_POLL_EVERY) {
            match fetch_config(&config.config_url) {
                Ok(cfg) => {
                    log::debug!("fetched selector config: preset=\"{}\"", cfg.preset);
                    preset_config = Some(cfg);
                }
                Err(e) => {
                    log::warn!("failed to fetch selector config: {e}");
                    // Keep using the last known config.
                }
            }
        }
        config_poll_counter = config_poll_counter.wrapping_add(1);

        let Some(ref cfg) = preset_config else { continue };
        let Some(patterns) = cfg.active_patterns() else { continue };

        // Get the current foreground window.
        let Some(info) = enumerate_windows::get_foreground_window() else { continue };

        // Resolve a candidate swap for this tick without mutating `last_info`
        // yet — we want the title POST to run before the swap command so the
        // frontend's `$captureInfo`/`$liveMode` updates land ahead of the
        // first new-window frame (see the ordering invariant in module docs).
        let hwnd = info.hwnd as isize;
        let hwnd_changed = last_info.as_ref().is_none_or(|li| li.hwnd != hwnd);

        let mut staged: Option<(SwapCommand, CachedStreamInfo)> = None;
        if hwnd_changed {
            // Match against patterns.
            let exe_path = info.executable_path.to_string_lossy().to_string();
            if let Some(capture_match) = should_capture(patterns, &exe_path, &info.title) {
                // Resolve capture info: prefer exe FileDescription, fall back to window title.
                let file_description = win32_version_info::VersionInfo::from_file(&info.executable_path)
                    .ok()
                    .map(|v| v.file_description)
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| info.title.clone());

                log::info!("switching to HWND 0x{hwnd:X} ({file_description})");

                staged = Some((
                    SwapCommand {
                        hwnd,
                        capture_info: file_description.clone(),
                    },
                    CachedStreamInfo {
                        hwnd,
                        title: info.title.clone(),
                        file_description,
                        mode: capture_match.mode,
                    }));
            }
            // If the new window doesn't match, leave `last_info` unchanged —
            // capture is still running on the previously matched window.
        }

        // Single POST per tick. Swap ticks queue a swap-tagged task (the worker
        // won't coalesce it). Otherwise re-queue the cached info as a heartbeat
        // so a restarted server sees it on the next tick.
        let task = match (staged.as_ref(), last_info.as_ref()) {
            (Some(&(_, ref cached)), _) => Some(cached.to_task(true)),
            (None, Some(cached)) => Some(cached.to_task(false)),
            (None, None) => None,
        };
        if let Some(task) = task {
            // Send is non-blocking on an unbounded channel; only fails if
            // the worker died, in which case there's nothing useful to do.
            let _ = post_tx.send(task);
        }

        // Swap POST is queued; dispatch the swap command.  Capture loop
        // won't produce new-window frames until it processes this.
        if let Some((cmd, cached)) = staged {
            if tx.send(cmd).is_err() {
                log::info!("capture loop closed, selector exiting");
                break;
            }
            last_info = Some(cached);
        }
    }
}

/// Fetch the selector preset config from the server.
fn fetch_config(url: &str) -> anyhow::Result<PresetConfig> {
    let body: String = ureq::get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("HTTP GET failed: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("failed to read response body: {e}"))?;
    let config: PresetConfig = serde_json::from_str(&body)?;
    Ok(config)
}

/// POST stream info to the server.  Called only from the poster worker
/// thread; the shared `Agent` is reused across calls for HTTP keep-alive.
///
/// We must drain the response body before dropping the `Response`: ureq
/// only returns a connection to the keep-alive pool once the body is read
/// to end (see ureq's `Body` docs, "Pool reuse").  Skipping the drain
/// leaks the connection, so every POST opens a fresh TCP socket — on
/// Windows-localhost that ~1–2 s per-connect overhead made the worker
/// take a full poll tick per task, queuing swap POSTs ~2 s behind the
/// video swap they were supposed to precede.
fn post_stream_info(agent: &ureq::Agent, url: &str, task: &PostTask) {
    let body = serde_json::json!({
        "hwnd": format!("0x{:X}", task.hwnd),
        "title": task.title,
        "file_description": task.file_description,
        "mode": task.mode,
    });

    match agent.post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
    {
        // Server returns `{"ok":true}` — discard, but read so the
        // connection is recycled into the pool.
        Ok(mut resp) => { let _ = resp.body_mut().read_to_string(); }
        Err(e) => log::warn!("failed to POST stream info: {e}"),
    }
}
