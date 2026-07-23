//! YouTube Music stream discovery, crop calculation, and process supervision.
//!
//! This module owns the only special-stream policy in the media pipeline. The
//! capture worker receives a generic HWND and absolute crop rectangle. The
//! encoder receives only the resulting fixed shared texture, while `live-ws`
//! receives only the normal stream transport arguments.

use std::{
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use euclid::default::{Box2D, Point2D, Size2D, Vector2D};
use live_shared_texture::OwnedMailbox;
use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::{
        HiDpi::GetDpiForWindow,
        WindowsAndMessaging::GetClientRect,
    },
};

use crate::{CHILD_POLL_INTERVAL, next_delay, restart::RestartBackoff, terminate};

/// Height of the player bar at the bottom of the viewport in CSS pixels.
const PLAYER_BAR_HEIGHT: f32 = 72.0;
/// Approximate browser scrollbar width reserved by the page in CSS pixels.
const SCROLL_BAR_WIDTH: f32 = 16.0;
/// Inward padding that avoids window-border artifacts in CSS pixels.
const PADDING: f32 = 2.0;

/// Validated immutable configuration for the fixed crop topology.
pub struct Config {
    /// Canonical generic capture executable path.
    pub capture: PathBuf,
    /// Canonical shared-texture encoder executable path.
    pub encoder: PathBuf,
    /// Canonical relay executable path.
    pub relay: PathBuf,
    /// Relay WebSocket destination.
    pub server: String,
    /// Well-known transport identifier.
    pub stream_id: String,
    /// Window title prefix used only by supervisor discovery.
    pub title: String,
    /// Encoded output frame rate.
    pub fps: u32,
    /// Delay between window discovery attempts while unavailable.
    pub poll_interval: Duration,
}

/// Supervise discovery and the current generic encoder-to-relay pipe.
pub fn run(config: Config, deadline: Option<Instant>) -> anyhow::Result<()> {
    let mut supervisor = Supervisor {
        config,
        pipeline: None,
        pipeline_backoff: RestartBackoff::default(),
    };
    supervisor.monitor(deadline)
}

/// Stateful owner for the current discovered window and media pipe pair.
struct Supervisor {
    /// Immutable stream settings and worker paths.
    config: Config,
    /// Current crop encoder and directly connected relay, if a window exists.
    pipeline: Option<Pipeline>,
    /// Bounded restart policy shared by discovery-to-spawn attempts.
    pipeline_backoff: RestartBackoff,
}

impl Supervisor {
    /// Poll window and child state without blocking the media pipe.
    fn monitor(&mut self, deadline: Option<Instant>) -> anyhow::Result<()> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                log::info!("@{}: bounded YouTube Music stream proof completed", self.config.stream_id);
                return Ok(());
            }

            if self.pipeline.is_none() {
                let attempted = self.try_start_pipeline()?;
                if self.pipeline.is_none() {
                    if !attempted {
                        thread::sleep(self.config.poll_interval);
                    }
                    continue;
                }
            }

            if let Some(observed) = self.poll_exit()? {
                log::warn!(
                    "@{}: {} exited with {}; restarting shared-texture cohort",
                    self.config.stream_id,
                    observed.component,
                    observed.status);
                drop(self.pipeline.take());
                let delay = next_delay(
                    &mut self.pipeline_backoff,
                    observed.stable_for,
                    "YouTube Music capture/encoder/live-ws")?;
                thread::sleep(delay);
            } else {
                thread::sleep(CHILD_POLL_INTERVAL);
            }
        }
    }

    /// Discover one matching window and create a complete pipe transactionally.
    fn try_start_pipeline(&mut self) -> anyhow::Result<bool> {
        let Some(window) = find_window(&self.config.title) else {
            log::info!(
                "@{}: waiting for window {:?}",
                self.config.stream_id,
                self.config.title);
            return Ok(false);
        };
        let crop = match compute_crop_rect(HWND(window.hwnd as *mut core::ffi::c_void)) {
            Ok(crop) => crop,
            Err(error) => {
                log::warn!(
                    "@{}: failed to compute crop for HWND 0x{:X}: {error:#}",
                    self.config.stream_id,
                    window.hwnd);
                return Ok(false);
            }
        };
        match spawn_pipeline(&self.config, window.hwnd, crop) {
            Ok(pipeline) => {
                self.pipeline = Some(pipeline);
                Ok(true)
            }
            Err(error) => {
                let delay = next_delay(
                    &mut self.pipeline_backoff,
                    Duration::ZERO,
                    "YouTube Music capture/encoder/live-ws")?;
                log::error!("@{}: shared cohort startup failed: {error:#}", self.config.stream_id);
                thread::sleep(delay);
                Ok(true)
            }
        }
    }

    /// Observe one pipe endpoint without waiting on its healthy peer.
    fn poll_exit(&mut self) -> anyhow::Result<Option<ObservedExit>> {
        let Some(pipeline) = self.pipeline.as_mut() else { return Ok(None) };
        if let Some(status) = pipeline.capture.try_wait()
            .context("failed to query YouTube Music capture status")?
        {
            return Ok(Some(ObservedExit {
                component: "live-capture",
                status,
                stable_for: pipeline.started.elapsed(),
            }));
        }
        if let Some(status) = pipeline.encoder.try_wait()
            .context("failed to query YouTube Music encoder status")?
        {
            return Ok(Some(ObservedExit {
                component: "live-encoder",
                status,
                stable_for: pipeline.started.elapsed(),
            }));
        }
        if let Some(status) = pipeline.relay.try_wait()
            .context("failed to query YouTube Music relay status")?
        {
            return Ok(Some(ObservedExit {
                component: "live-ws",
                status,
                stable_for: pipeline.started.elapsed(),
            }));
        }
        Ok(None)
    }
}

/// One exited crop-pipeline endpoint and its common uptime.
struct ObservedExit {
    /// Stable worker label for diagnostics.
    component: &'static str,
    /// Concrete process status.
    status: ExitStatus,
    /// Pipe-pair lifetime used by restart policy.
    stable_for: Duration,
}

/// Generic capture, shared encoder, direct transport worker, and mailbox owner.
struct Pipeline {
    /// Generic HWND/crop producer.
    capture: Child,
    /// Shared-texture consumer and encoded stdout producer.
    encoder: Child,
    /// Direct stdin consumer and WebSocket reconnect worker.
    relay: Child,
    /// Common creation time for backoff stability.
    started: Instant,
    /// Shared texture kept alive until every child has been reaped.
    _mailbox: OwnedMailbox,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // Close the pipe reader first, then reap every GPU worker before the
        // mailbox field is released and window geometry is rediscovered.
        terminate(&mut self.relay);
        terminate(&mut self.encoder);
        terminate(&mut self.capture);
    }
}

/// Find the first visible window whose title begins with the configured prefix.
fn find_window(prefix: &str) -> Option<enumerate_windows::WindowInfo> {
    let mut matches = enumerate_windows::enumerate_windows()
        .into_iter()
        .filter(|window| window.title.starts_with(prefix));
    let first = matches.next()?;
    if matches.next().is_some() {
        log::warn!("multiple windows match {prefix:?}; using the first");
    }
    Some(first)
}

/// Compute the player-bar crop in WGC texture coordinates for one HWND.
fn compute_crop_rect(hwnd: HWND) -> anyhow::Result<Box2D<u32>> {
    let mut client_rect = RECT::default();
    // SAFETY: `hwnd` came from the current filtered enumeration and the output
    // pointer refers to a correctly sized stack-local `RECT`.
    unsafe { GetClientRect(hwnd, &raw mut client_rect) }
        .map_err(|error| anyhow::anyhow!("GetClientRect failed: {error}"))?;
    // SAFETY: The same enumerated HWND remains valid for this immediate query.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    anyhow::ensure!(dpi > 0, "GetDpiForWindow returned zero");
    let client_size = Size2D::new(
        u32::try_from(client_rect.right - client_rect.left)
            .context("client width was negative")?,
        u32::try_from(client_rect.bottom - client_rect.top)
            .context("client height was negative")?);
    crop_rect_for_client(client_size, dpi)
}

/// Convert DPI-scaled client geometry into the captured texture crop rectangle.
fn crop_rect_for_client(client_size: Size2D<u32>, dpi: u32) -> anyhow::Result<Box2D<u32>> {
    anyhow::ensure!(dpi > 0, "DPI must be non-zero");
    let scale = dpi as f32 / 96.0;
    let viewport = client_size.to_f32() / scale;
    anyhow::ensure!(
        viewport.width > SCROLL_BAR_WIDTH + PADDING * 2.0
            && viewport.height > PLAYER_BAR_HEIGHT,
        "client area {}x{} is too small for the player bar",
        client_size.width,
        client_size.height);

    let css_crop = Box2D::new(
        Point2D::new(PADDING, viewport.height - PLAYER_BAR_HEIGHT + PADDING),
        Point2D::new(
            viewport.width - SCROLL_BAR_WIDTH - PADDING,
            viewport.height - PADDING));
    let client_crop = Box2D::new(
        Point2D::new(
            (css_crop.min.x * scale).floor() as u32,
            (css_crop.min.y * scale).floor() as u32),
        Point2D::new(
            (css_crop.max.x * scale).ceil() as u32,
            (css_crop.max.y * scale).ceil() as u32));

    // The visible WGC texture includes the title bar. This retained empirical
    // formula matches the previous wrapper at 100%, 150%, and 175% scaling.
    let frame_offset = Vector2D::new(0, (28.0 * scale + 4.0).round() as u32);
    Ok(Box2D::new(
        client_crop.min + frame_offset,
        client_crop.max + frame_offset))
}

/// Launch one crop/shared-texture/encoder cohort and direct encoder stdout to the relay.
fn spawn_pipeline(
    config: &Config,
    hwnd: usize,
    crop: Box2D<u32>) -> anyhow::Result<Pipeline> {
    let output_size = encoder_output_size(crop);
    let mut mailbox = OwnedMailbox::new(output_size)
        .context("failed to create YouTube Music shared mailbox")?;
    let adapter = mailbox.device_bundle().adapter_luid;

    let capture_inheritance = mailbox.inheritable_handle()?;
    let capture_handle = capture_inheritance.value();
    let mut capture = Command::new(&config.capture)
        .arg("--hwnd").arg(hwnd.to_string())
        .arg("--crop-min-x").arg(crop.min.x.to_string())
        .arg("--crop-min-y").arg(crop.min.y.to_string())
        .arg("--crop-max-x").arg(crop.max.x.to_string())
        .arg("--crop-max-y").arg(crop.max.y.to_string())
        .arg("--width").arg(output_size.width.to_string())
        .arg("--height").arg(output_size.height.to_string())
        .arg("--no-preview")
        .arg("--shared-handle").arg(capture_handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to launch {}", config.capture.display()))?;
    if let Err(error) = capture_inheritance.revoke() {
        terminate(&mut capture);
        return Err(error).context("failed to revoke mailbox inheritance after capture spawn");
    }

    let encoder_inheritance = match mailbox.inheritable_handle() {
        Ok(inheritance) => inheritance,
        Err(error) => {
            terminate(&mut capture);
            return Err(error).context("failed to prepare mailbox inheritance for encoder");
        }
    };
    let encoder_handle = encoder_inheritance.value();
    let mut encoder = match Command::new(&config.encoder)
        .arg("--width").arg(output_size.width.to_string())
        .arg("--height").arg(output_size.height.to_string())
        .arg("--fps").arg(config.fps.to_string())
        .arg("--stream-id").arg(&config.stream_id)
        .arg("--shared-handle").arg(encoder_handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = encoder_inheritance.revoke();
            terminate(&mut capture);
            return Err(error).with_context(|| format!("failed to launch {}", config.encoder.display()));
        }
    };
    if let Err(error) = encoder_inheritance.revoke() {
        terminate(&mut encoder);
        terminate(&mut capture);
        return Err(error).context("failed to revoke mailbox inheritance after encoder spawn");
    }
    let Some(encoded_stdout) = encoder.stdout.take() else {
        terminate(&mut encoder);
        terminate(&mut capture);
        anyhow::bail!("YouTube Music encoder stdout pipe was not created");
    };
    let relay = match Command::new(&config.relay)
        .arg("--mode").arg("video")
        .arg("--server").arg(&config.server)
        .arg("--stream-id").arg(&config.stream_id)
        .stdin(Stdio::from(encoded_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(relay) => relay,
        Err(error) => {
            terminate(&mut encoder);
            terminate(&mut capture);
            return Err(error).with_context(|| format!("failed to launch {}", config.relay.display()));
        }
    };
    log::info!(
        "@{}: HWND 0x{hwnd:X}, crop=({},{})..({},{}) -> {}x{} shared texture on adapter {}; live-capture pid={} -> live-encoder pid={} -> live-ws pid={}",
        config.stream_id,
        crop.min.x,
        crop.min.y,
        crop.max.x,
        crop.max.y,
        output_size.width,
        output_size.height,
        adapter,
        capture.id(),
        encoder.id(),
        relay.id());
    Ok(Pipeline {
        capture,
        encoder,
        relay,
        started: Instant::now(),
        _mailbox: mailbox,
    })
}

/// Round native crop dimensions up to the encoder's 16-pixel alignment.
fn encoder_output_size(crop: Box2D<u32>) -> Size2D<u32> {
    let crop_size = crop.size();
    Size2D::new(
        (crop_size.width + 15) & !15,
        (crop_size.height + 15) & !15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_geometry_scales_css_and_title_bar_at_100_percent() {
        let crop = crop_rect_for_client(Size2D::new(1280, 720), 96).unwrap();
        assert_eq!(crop.min, Point2D::new(2, 682));
        assert_eq!(crop.max, Point2D::new(1262, 750));
    }

    #[test]
    fn crop_geometry_scales_css_and_title_bar_at_150_percent() {
        let crop = crop_rect_for_client(Size2D::new(1920, 1080), 144).unwrap();
        assert_eq!(crop.min, Point2D::new(3, 1021));
        assert_eq!(crop.max, Point2D::new(1893, 1123));
    }

    #[test]
    fn crop_geometry_rejects_windows_smaller_than_the_known_layout() {
        let error = crop_rect_for_client(Size2D::new(20, 20), 96).unwrap_err();
        assert!(error.to_string().contains("too small"));
    }

    #[test]
    fn crop_output_is_padded_for_the_shared_encoder_contract() {
        let crop = Box2D::new(Point2D::new(2, 682), Point2D::new(1262, 750));
        assert_eq!(encoder_output_size(crop), Size2D::new(1264, 80));
    }
}
