//! Local-file-driven foreground-window selector.
//!
//! Its own HWND is ignored so focusing the preview preserves the last selected
//! application rather than creating recursive capture. A different disallowed
//! foreground window also preserves that last selection, while an active target
//! that becomes disallowed is explicitly cleared.

pub mod config;

use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use config::{SelectorPolicy, SelectorPolicyStore};

/// Configuration reload cadence, kept far away from the per-frame preview path.
const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(20);

/// Selection update delivered to the preview event loop.
pub enum SelectorCommand {
    /// Replace the capture target with a newly validated foreground window.
    Select {
        /// Win32 handle of the newly selected foreground window.
        hwnd: isize,
        /// Human-readable executable description used in lifecycle logging.
        capture_info: String,
    },
    /// Drop the current target because it no longer satisfies active policy.
    Clear,
}

/// Runtime configuration for the preview selector thread.
pub struct SelectorConfig {
    /// Local selector profile document reloaded without server access.
    pub config_path: PathBuf,
    /// Preview HWND excluded before matching to prevent self-capture.
    pub ignored_hwnd: isize,
    /// Foreground-window polling cadence.
    pub poll_interval: Duration,
}

/// Policy-relevant metadata retained for active-target revalidation.
struct SelectedWindow {
    /// Win32 handle supplied to Windows Graphics Capture.
    hwnd: isize,
    /// Owning process ID used to detect HWND reuse after a window closes.
    pid: u32,
    /// Executable path last observed for this HWND.
    executable_path: String,
}

/// Spawn the preview selector and return its update receiver.
///
/// The worker exits after the receiver is dropped and it next needs to send an
/// update. Missing or invalid initial configuration fails closed; later reload
/// failures retain the last fully validated policy.
pub fn spawn_selector(config: SelectorConfig) -> mpsc::Receiver<SelectorCommand> {
    let (tx, rx) = mpsc::channel();

    std::thread::Builder::new()
        .name("selector-preview".into())
        .spawn(move || selector_loop(&tx, &config))
        .expect("failed to spawn preview selector thread");

    rx
}

/// Poll foreground state and emit only policy-valid target changes.
///
/// Policy reload and foreground enumeration run on this low-frequency worker,
/// never on the preview's per-frame event loop. A same-HWND candidate is matched
/// again before it may remain active, so mutable foreground metadata is not
/// permanently trusted.
fn selector_loop(tx: &mpsc::Sender<SelectorCommand>, config: &SelectorConfig) {
    log::info!(
        "preview selector started (poll: {:?}, config: {})",
        config.poll_interval,
        config.config_path.display());

    let mut policies = SelectorPolicyStore::default();
    let mut selected: Option<SelectedWindow> = None;
    let mut reload_at = Instant::now();

    loop {
        let now = Instant::now();
        if now >= reload_at {
            if !reload_policy(tx, config, &mut policies, &mut selected) {
                break;
            }
            reload_at = now + CONFIG_RELOAD_INTERVAL;
        }

        if let Some(policy) = policies.active()
            && let Some(window) = enumerate_windows::get_foreground_window()
            && !select_foreground(tx, config, policy, &mut selected, &window)
        {
            break;
        }

        std::thread::sleep(config.poll_interval);
    }
}

/// Reload policy atomically and revoke a selected target excluded by the update.
///
/// Returns `false` only when the receiver has closed while sending a required
/// clear command, which tells the selector thread to exit.
fn reload_policy(
    tx: &mpsc::Sender<SelectorCommand>,
    config: &SelectorConfig,
    policies: &mut SelectorPolicyStore,
    selected: &mut Option<SelectedWindow>) -> bool {
    match policies.reload_path(&config.config_path) {
        Ok(false) => true,
        Ok(true) => {
            log::info!("activated selector config {}", config.config_path.display());
            let remains_allowed = selected.as_ref().is_none_or(|window| {
                policies
                    .active()
                    .is_some_and(|policy| policy.allows_executable(&window.executable_path))
            });
            if remains_allowed {
                return true;
            }

            log::info!("active selector target is no longer allowed; clearing preview");
            *selected = None;
            tx.send(SelectorCommand::Clear).is_ok()
        }
        Err(error) => {
            log::warn!(
                "failed to reload selector config {}; retaining last valid policy: {error:#}",
                config.config_path.display());
            true
        }
    }
}

/// Revalidate one foreground candidate and send a target transition if needed.
///
/// Returns `false` when the preview receiver has closed. Disallowed windows do
/// not replace a different valid selection, but a selected HWND that becomes
/// disallowed is cleared immediately.
fn select_foreground(
    tx: &mpsc::Sender<SelectorCommand>,
    config: &SelectorConfig,
    policy: &SelectorPolicy,
    selected: &mut Option<SelectedWindow>,
    window: &enumerate_windows::WindowInfo) -> bool {
    let hwnd = window.hwnd as isize;
    if config.ignored_hwnd == hwnd {
        return true;
    }

    let executable_path = window.executable_path.to_string_lossy().into_owned();
    if !policy.allows_executable(&executable_path) {
        if selected
            .as_ref()
            .is_some_and(|current| current.hwnd == hwnd)
        {
            log::info!("selected HWND 0x{hwnd:X} is no longer allowed; clearing preview");
            *selected = None;
            return tx.send(SelectorCommand::Clear).is_ok();
        }
        return true;
    }

    if let Some(current) = selected.as_mut()
        && current.hwnd == hwnd
            && current.pid == window.pid
    {
        // Retain the refreshed path so the next policy reload validates the
        // exact metadata observed most recently for this active HWND.
        current.executable_path = executable_path;
        return true;
    }

    let capture_info = win32_version_info::VersionInfo::from_file(&window.executable_path)
        .ok()
        .map(|version| version.file_description)
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| window.title.clone());
    log::info!("selecting HWND 0x{hwnd:X} ({capture_info})");

    if tx
        .send(SelectorCommand::Select { hwnd, capture_info })
        .is_err()
    {
        log::info!("preview event loop closed, selector exiting");
        return false;
    }
    *selected = Some(SelectedWindow {
        hwnd,
        pid: window.pid,
        executable_path,
    });
    true
}
