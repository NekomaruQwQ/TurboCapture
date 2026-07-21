//! Preview-owned foreground-window selector.
//!
//! The selector polls the same server configuration and applies the same
//! pattern semantics as `live-capture`, but has no stream-info reporting side
//! effects. Its own HWND is ignored so focusing the preview preserves the last
//! selected application rather than creating recursive capture.

pub mod config;

use std::{
    sync::mpsc,
    time::Duration,
};

use config::{PresetConfig, should_capture};

/// Window-switch request delivered to the preview event loop.
pub struct SwapCommand {
    /// Win32 handle of the newly selected foreground window.
    pub hwnd: isize,
    /// Human-readable executable description used for lifecycle logging.
    pub capture_info: String,
}

/// Runtime configuration for the preview selector thread.
pub struct SelectorConfig {
    /// Server endpoint returning the complete selector preset configuration.
    pub config_url: String,
    /// Preview HWND excluded before pattern matching to prevent self-capture.
    pub ignored_hwnd: isize,
    /// Foreground-window polling cadence.
    pub poll_interval: Duration,
}

/// Spawn the preview selector and return its window-switch receiver.
///
/// The worker exits after the receiver is dropped and a subsequent matched
/// selection cannot be sent. Configuration/network failures are logged and the
/// last valid configuration remains active.
pub fn spawn_selector(config: SelectorConfig) -> mpsc::Receiver<SwapCommand> {
    let (tx, rx) = mpsc::channel();

    std::thread::Builder::new()
        .name("selector-preview".into())
        .spawn(move || selector_loop(&tx, &config))
        .expect("failed to spawn preview selector thread");

    rx
}

/// Poll foreground state forever and emit only newly matched HWNDs.
///
/// An unmatched or ignored foreground window deliberately leaves the previous
/// preview target unchanged. Configuration is refreshed every ten ticks, with
/// the previous successful response retained across transient HTTP failures.
fn selector_loop(tx: &mpsc::Sender<SwapCommand>, config: &SelectorConfig) {
    /// Configuration refresh frequency relative to foreground polling.
    const CONFIG_POLL_EVERY: u32 = 10;

    log::info!(
        "preview selector started (poll: {:?}, config: {})",
        config.poll_interval,
        config.config_url);

    let mut last_hwnd: Option<isize> = None;
    let mut preset_config: Option<PresetConfig> = None;
    let mut config_poll_counter: u32 = 0;

    loop {
        std::thread::sleep(config.poll_interval);

        if config_poll_counter.is_multiple_of(CONFIG_POLL_EVERY) {
            match fetch_config(&config.config_url) {
                Ok(new_config) => {
                    log::debug!(
                        "fetched preview selector config: preset=\"{}\"",
                        new_config.preset);
                    preset_config = Some(new_config);
                }
                Err(error) =>
                    log::warn!("failed to fetch preview selector config: {error}"),
            }
        }
        config_poll_counter = config_poll_counter.wrapping_add(1);

        let Some(ref preset_config) = preset_config else { continue };
        let Some(patterns) = preset_config.active_patterns() else { continue };
        let Some(window) = enumerate_windows::get_foreground_window() else { continue };

        let hwnd = window.hwnd as isize;
        if config.ignored_hwnd == hwnd || last_hwnd == Some(hwnd) {
            continue;
        }

        let executable_path = window.executable_path.to_string_lossy();
        if !should_capture(patterns, &executable_path, &window.title) {
            continue;
        }

        let capture_info =
            win32_version_info::VersionInfo::from_file(&window.executable_path)
                .ok()
                .map(|version| version.file_description)
                .filter(|description| !description.is_empty())
                .unwrap_or_else(|| window.title.clone());
        log::info!("selecting HWND 0x{hwnd:X} ({capture_info})");

        if tx.send(SwapCommand { hwnd, capture_info }).is_err() {
            log::info!("preview event loop closed, selector exiting");
            break;
        }
        last_hwnd = Some(hwnd);
    }
}

/// Fetch and deserialize the complete selector preset configuration.
///
/// HTTP status, response-body, and JSON errors are returned to the polling loop
/// so it can log the failure and continue using its previous configuration.
fn fetch_config(url: &str) -> anyhow::Result<PresetConfig> {
    let body = ureq::get(url)
        .call()
        .map_err(|error| anyhow::anyhow!("HTTP GET failed: {error}"))?
        .body_mut()
        .read_to_string()
        .map_err(|error| anyhow::anyhow!("failed to read response body: {error}"))?;
    Ok(serde_json::from_str(&body)?)
}
