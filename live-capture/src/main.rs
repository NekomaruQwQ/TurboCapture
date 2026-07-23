//! `live-capture.exe` — safe standalone or managed GPU window capture.
//!
//! A local profile TOML selects allowed foreground windows for the primary
//! public mode. The stream supervisor may instead provide one already-resolved
//! HWND and crop rectangle for a policy it owns. Neither mode encodes or uses
//! network transport.

mod capture;
mod d3d11;
mod presenter;
mod publisher;
mod resample;
mod selector;

use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::Parser;
use crate::{
    capture::{CaptureSession, CropBox},
    selector::{SelectorCommand, SelectorConfig, spawn_selector},
};
use nkcore::{
    prelude::euclid::Size2D,
    winit::{AppEvent, EventLoopExt as _},
};
use live_shared_texture::{
    AdapterLuid,
    RESOURCE_GENERATION_LOST_EXIT_CODE,
    ResourceGenerationLost,
    SharedHandleValue,
    is_resource_generation_lost,
};
use presenter::Presenter;
use publisher::{FrameTransform, SharedPublisher};
use windows::Win32::{
    Foundation::HWND,
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
};
use winit::{
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ControlFlow, EventLoop},
    platform::windows::WindowAttributesExtWindows as _,
    raw_window_handle::{HasWindowHandle as _, RawWindowHandle},
    window::{Window, WindowButtons},
};

/// Default preview width in physical pixels.
const DEFAULT_WIDTH: u32 = 1920;
/// Default preview height in physical pixels.
const DEFAULT_HEIGHT: u32 = 1200;
/// Background used for letterboxing and periods without an active target.
const CLEAR_COLOR: [f32; 4] = [41.0 / 255.0, 41.0 / 255.0, 41.0 / 255.0, 1.0];
/// Normal selector polling cadence.
const SELECTOR_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Capture retry delay after a window closes or WGC reports an error.
const CAPTURE_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Event-loop wake cadence while waiting for the next captured frame.
const ACTIVE_WAKE_INTERVAL: Duration = Duration::from_millis(1);
/// Reduced wake cadence while no target is available or the preview is hidden.
const IDLE_WAKE_INTERVAL: Duration = Duration::from_millis(50);

/// CLI arguments for standalone safe sharing and managed publication.
#[derive(Parser)]
#[command(name = "live-capture", about = "Capture allowlisted windows into fixed-size GPU output")]
struct Args {
    /// Local selector profile TOML to load and monitor.
    #[arg(long, conflicts_with = "hwnd")]
    config: Option<PathBuf>,

    /// Resolved window handle for supervisor-owned generic crop capture.
    #[arg(long, value_parser = parse_hwnd, conflicts_with = "config")]
    hwnd: Option<isize>,

    /// Inclusive crop left edge in captured-texture pixels.
    #[arg(long, requires_all = ["hwnd", "crop_min_y", "crop_max_x", "crop_max_y"])]
    crop_min_x: Option<u32>,

    /// Inclusive crop top edge in captured-texture pixels.
    #[arg(long, requires_all = ["hwnd", "crop_min_x", "crop_max_x", "crop_max_y"])]
    crop_min_y: Option<u32>,

    /// Exclusive crop right edge in captured-texture pixels.
    #[arg(long, requires_all = ["hwnd", "crop_min_x", "crop_min_y", "crop_max_y"])]
    crop_max_x: Option<u32>,

    /// Exclusive crop bottom edge in captured-texture pixels.
    #[arg(long, requires_all = ["hwnd", "crop_min_x", "crop_min_y", "crop_max_x"])]
    crop_max_y: Option<u32>,

    /// Fixed physical width of the preview client area.
    #[arg(long, default_value_t = DEFAULT_WIDTH, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,

    /// Fixed physical height of the preview client area.
    #[arg(long, default_value_t = DEFAULT_HEIGHT, value_parser = clap::value_parser!(u32).range(1..))]
    height: u32,

    /// Preview window title.
    #[arg(long, default_value = "Live Capture")]
    title: String,

    /// Keep the capture path headless; valid only with managed shared output.
    #[arg(long, requires = "shared_handle")]
    no_preview: bool,

    /// Supervisor-owned NT shared-texture handle inherited by this process.
    #[arg(long, requires = "adapter_luid")]
    shared_handle: Option<SharedHandleValue>,

    /// DXGI adapter LUID selected by the supervisor for the managed GPU cohort.
    #[arg(long, requires = "shared_handle")]
    adapter_luid: Option<AdapterLuid>,

    /// Exit while owning the producer mutex after this many real publications.
    #[arg(long, hide = true, requires = "shared_handle", value_parser = clap::value_parser!(u64).range(1..))]
    fault_abandon_after_publications: Option<u64>,
}

/// Validated capture-source policy chosen before any GPU resource is created.
enum CaptureSource {
    /// Local allowlist with live atomic reload.
    Profiles(PathBuf),
    /// Supervisor-resolved generic crop with no special-stream knowledge.
    Crop { hwnd: isize, crop: CropBox },
}

/// Parse decimal or `0x`-prefixed Win32 window handles without accepting null.
fn parse_hwnd(value: &str) -> Result<isize, String> {
    let parsed = value
        .strip_prefix("0x")
        .map_or_else(|| value.parse(), |hex| isize::from_str_radix(hex, 16))
        .map_err(|error| format!("invalid HWND {value:?}: {error}"))?;
    if parsed == 0 {
        Err("HWND must be non-zero".to_owned())
    } else {
        Ok(parsed)
    }
}

/// Reject partial or ambiguous source specifications before starting capture.
fn resolve_source(args: &Args) -> anyhow::Result<CaptureSource> {
    match (
        args.config.as_ref(),
        args.hwnd,
        args.crop_min_x,
        args.crop_min_y,
        args.crop_max_x,
        args.crop_max_y,
    ) {
        (Some(config), None, None, None, None, None) =>
            Ok(CaptureSource::Profiles(config.clone())),
        (None, Some(hwnd), Some(min_x), Some(min_y), Some(max_x), Some(max_y)) => {
            anyhow::ensure!(args.shared_handle.is_some(), "crop capture requires managed shared output");
            anyhow::ensure!(args.no_preview, "crop capture requires --no-preview");
            anyhow::ensure!(max_x > min_x, "--crop-max-x must be greater than --crop-min-x");
            anyhow::ensure!(max_y > min_y, "--crop-max-y must be greater than --crop-min-y");
            let crop = CropBox { min_x, min_y, max_x, max_y };
            anyhow::ensure!(
                crop.output_size() == Size2D::new(args.width, args.height),
                "crop output is {}x{}, but --width/--height specify {}x{}",
                crop.output_size().width,
                crop.output_size().height,
                args.width,
                args.height);
            Ok(CaptureSource::Crop { hwnd, crop })
        }
        _ => anyhow::bail!(
            "provide either --config or the complete --hwnd + --crop-min-x/y + --crop-max-x/y source"),
    }
}

/// Last selector target, retained so a failed WGC session can be recreated
/// without waiting for the foreground window to change.
struct SelectedTarget {
    /// Win32 window handle selected by the shared foreground matcher.
    hwnd: isize,
    /// Human-readable executable description used in lifecycle logs.
    capture_info: String,
    /// Fixed-output operation applied before presentation to the encoder.
    transform: FrameTransform,
}

fn main() {
    pretty_env_logger::init();

    let args = Args::parse();
    let managed = args.shared_handle.is_some();
    if let Err(error) = run(args) {
        exit_worker(error, managed);
    }
}

/// Create the preview window and run capture/presentation until it closes.
///
/// # Panics
///
/// Winit's resume callback cannot return a `Result`, so window, HWND, or D3D
/// initialization failures inside that callback panic with contextual messages.
fn run(args: Args) -> anyhow::Result<()> {
    let source = resolve_source(&args)?;
    set_dpi_awareness::per_monitor_v2().context("failed to enable per-monitor DPI awareness")?;

    // SAFETY: This runs once on the main thread before winit, WGC, or any
    // other COM-using object is created on the thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("failed to initialize COM")?;

    let output_size = Size2D::new(args.width, args.height);
    let physical_size = PhysicalSize::new(args.width, args.height);
    let mut event_loop = EventLoop::<()>::new().context("failed to create event loop")?;

    event_loop
        .run_app_with(move |event_loop| {
            let window =
                event_loop.create_window(
                    Window::default_attributes()
                        .with_title(args.title)
                        .with_inner_size(physical_size)
                        .with_resizable(false)
                        .with_visible(!args.no_preview)
                        // Winit's drag/drop path calls `OleInitialize` for STA,
                        // which conflicts with the MTA required by WGC on this
                        // thread. The preview has no file-drop behavior.
                        .with_drag_and_drop(false)
                        .with_enabled_buttons(
                            WindowButtons::CLOSE |
                            WindowButtons::MINIMIZE))
                    .expect("failed to create selector preview window");
            let preview_hwnd =
                hwnd_from_window(&window).expect("failed to obtain selector preview HWND");
            let presenter = Presenter::new(
                preview_hwnd,
                output_size,
                CLEAR_COLOR,
                args.adapter_luid)
                .unwrap_or_else(|error| exit_worker(
                    error.context("failed to initialize selector preview presenter"),
                    args.adapter_luid.is_some()));
            let publisher = args.shared_handle
                .map(|handle| SharedPublisher::new(
                    presenter.device(),
                    presenter.device_context(),
                    handle.into_owned(),
                    output_size,
                    CLEAR_COLOR,
                    args.fault_abandon_after_publications)
                    .map_err(|error| ResourceGenerationLost::new(format!(
                        "failed to initialize managed shared output: {error:#}")))
                    .unwrap_or_else(|error| exit_worker(error.into(), true)));
            let (swap_rx, selected, fixed_source) = match source {
                CaptureSource::Profiles(config_path) => (
                    Some(spawn_selector(SelectorConfig {
                        config_path,
                        ignored_hwnd: preview_hwnd.0 as isize,
                        poll_interval: SELECTOR_POLL_INTERVAL,
                    })),
                    None,
                    false,
                ),
                CaptureSource::Crop { hwnd, crop } => (
                    None,
                    Some(SelectedTarget {
                        hwnd,
                        capture_info: format!("managed HWND 0x{hwnd:X}"),
                        transform: FrameTransform::Crop(crop),
                    }),
                    true,
                ),
            };

            log::info!(
                "capture started: {}x{}, preview={}, HWND=0x{:X}",
                output_size.width,
                output_size.height,
                !args.no_preview,
                preview_hwnd.0 as isize);

            let mut state = PreviewState {
                presenter,
                publisher,
                swap_rx,
                selected,
                capture: None,
                retry_at: Instant::now(),
                occluded: false,
                managed: args.shared_handle.is_some(),
                fixed_source,
                preview_enabled: !args.no_preview,
                window,
            };

            move |event_loop, event| state.handle_event(event_loop, event)
        })
        .context("selector preview event loop failed")
}

/// Mutable preview lifecycle state owned by the winit event-loop callback.
///
/// Field order intentionally keeps the HWND-owning window alive until after
/// the presenter and capture session have been dropped.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent window, output, source, and recovery state are clearer as named flags")]
struct PreviewState {
    /// GPU presentation state, declared before `window` so it drops first.
    presenter: Presenter,
    /// Optional non-blocking managed output using the presenter's adapter.
    publisher: Option<SharedPublisher>,
    /// Selector updates consumed without blocking the window event loop.
    ///
    /// A supervisor-resolved fixed crop has no selector thread.
    swap_rx: Option<mpsc::Receiver<SelectorCommand>>,
    /// Latest selected target retained across capture-session failures.
    selected: Option<SelectedTarget>,
    /// Active WGC session, recreated independently of selector polling.
    capture: Option<CaptureSession>,
    /// Earliest retry time after a recoverable WGC initialization error.
    retry_at: Instant,
    /// Whether rendering should pause to avoid hidden-window GPU work.
    occluded: bool,
    /// Whether device/mailbox loss must request complete generation recovery.
    managed: bool,
    /// Whether a stale WGC session should return control to window discovery.
    fixed_source: bool,
    /// Whether frames should also be presented to the local swap chain.
    preview_enabled: bool,
    /// HWND owner, deliberately declared last so every dependent field drops first.
    window: Window,
}

impl PreviewState {
    /// Route window lifecycle events and idle ticks to the preview loop.
    fn handle_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, event: AppEvent<()>) {
        match event {
            AppEvent::WindowEvent(window_id, event) if window_id == self.window.id() => {
                self.handle_window_event(event_loop, event);
            }
            AppEvent::Idle => self.tick(event_loop),
            _ => {}
        }
    }

    /// Handle close, occlusion, and attempts to change the fixed client size.
    #[expect(clippy::needless_pass_by_value, reason = "winit transfers ownership of each event to the application")]
    fn handle_window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Occluded(occluded) => self.occluded = occluded,
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                let expected = PhysicalSize::new(
                    self.presenter.output_width(),
                    self.presenter.output_height());
                if size != expected {
                    let _ = self.window.request_inner_size(expected);
                }
            }
            _ => {}
        }
    }

    /// Process selector updates, acquire at most one newest WGC frame, and
    /// choose a bounded wake interval so an idle preview does not busy-spin.
    fn tick(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        self.apply_latest_selection();

        let has_active_output = !self.occluded || self.publisher.is_some();
        let delay = if !has_active_output {
            IDLE_WAKE_INTERVAL
        } else {
            self.ensure_capture();
            self.render_next_frame();
            if self.capture.is_some() {
                ACTIVE_WAKE_INTERVAL
            } else {
                IDLE_WAKE_INTERVAL
            }
        };

        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
    }

    /// Apply only the newest queued selection because intermediate targets can
    /// no longer be foreground by the time the event loop observes them.
    fn apply_latest_selection(&mut self) {
        let Some(swap_rx) = self.swap_rx.as_ref() else { return };
        let mut latest = None;
        while let Ok(command) = swap_rx.try_recv() {
            latest = Some(command);
        }
        let Some(command) = latest else { return };

        match command {
            SelectorCommand::Select { hwnd, capture_info } => {
                log::info!("switching capture to HWND 0x{hwnd:X} ({capture_info})");
                self.selected = Some(SelectedTarget {
                    hwnd,
                    capture_info,
                    transform: FrameTransform::Resample,
                });
            }
            SelectorCommand::Clear => {
                log::info!("clearing capture output");
                self.selected = None;
            }
        }
        self.capture = None;
        self.retry_at = Instant::now();

        if let Err(error) = self.presenter.clear_and_present() {
            exit_worker(
                error.context("failed to clear preview during window switch"),
                self.managed);
        }
    }

    /// Recreate a missing WGC session after the retry backoff expires.
    fn ensure_capture(&mut self) {
        if self.capture.is_some() || Instant::now() < self.retry_at {
            return;
        }
        let Some(target) = self.selected.as_ref() else { return };

        match CaptureSession::from_hwnd(
            self.presenter.device(),
            HWND(target.hwnd as *mut core::ffi::c_void)) {
            Ok(capture) => {
                log::info!("capturing HWND 0x{:X} ({})", target.hwnd, target.capture_info);
                self.capture = Some(capture);
            }
            Err(error) => {
                if self.fixed_source {
                    exit_worker(
                        error.context("fixed capture target is unavailable"),
                        self.managed);
                }
                log::warn!("failed to open HWND 0x{:X}: {error:#}", target.hwnd);
                self.retry_at = Instant::now() + CAPTURE_RETRY_INTERVAL;
            }
        }
    }

    /// Render the newest available frame and recover from stale capture sessions.
    fn render_next_frame(&mut self) {
        let Some(capture) = self.capture.as_mut() else { return };

        match capture.get_next_frame(self.presenter.device_context()) {
            Ok(Some(frame)) => {
                if self.preview_enabled
                    && !self.occluded
                    && let Err(error) = self.presenter.render(&frame.raw_texture, frame.size) {
                    exit_worker(
                        error.context("failed to render capture preview"),
                        self.managed);
                }
                if let Some(publisher) = self.publisher.as_mut()
                    && let Err(error) = publisher.publish(
                        self.presenter.device(),
                        self.presenter.device_context(),
                        &frame.raw_texture,
                        frame.size,
                        self.selected
                            .as_ref()
                            .expect("an active capture has a selected target")
                            .transform) {
                    exit_worker(
                        error.context("failed to publish captured frame"),
                        true);
                }
            }
            Ok(None) => {}
            Err(error) => {
                if self.fixed_source {
                    exit_worker(
                        error.context("fixed capture session failed"),
                        self.managed);
                }
                log::warn!("capture session failed; retrying: {error:#}");
                self.capture = None;
                self.retry_at = Instant::now() + CAPTURE_RETRY_INTERVAL;
            }
        }
    }
}

/// Report a fatal selector error through the supervisor's stable exit contract.
///
/// Standalone errors use the conventional failure code. Managed DXGI device
/// loss and explicit mailbox invalidation request complete resource recreation.
#[expect(clippy::exit, reason = "winit callbacks cannot return worker failures to the supervisor")]
#[expect(clippy::needless_pass_by_value, reason = "the diverging function owns its final diagnostic error")]
fn exit_worker(error: anyhow::Error, managed: bool) -> ! {
    eprintln!("fatal: {error:#}");
    let exit_code = if managed && is_resource_generation_lost(&error) {
        RESOURCE_GENERATION_LOST_EXIT_CODE
    } else {
        1
    };
    std::process::exit(exit_code)
}

/// Extract the Win32 HWND owned by a winit window.
///
/// Returns an error if the program is ever built on a non-Win32 winit backend.
fn hwnd_from_window(window: &Window) -> anyhow::Result<HWND> {
    let handle = window.window_handle().context("winit window handle unavailable")?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) =>
            Ok(HWND(handle.hwnd.get() as *mut core::ffi::c_void)),
        other => anyhow::bail!("expected a Win32 window handle, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_source_is_the_primary_public_interface() {
        let args = Args::try_parse_from([
            "live-capture",
            "--config",
            "profiles.toml",
        ]).unwrap();
        let CaptureSource::Profiles(path) = resolve_source(&args).unwrap() else {
            panic!("expected profile source");
        };
        assert_eq!(path, PathBuf::from("profiles.toml"));
        assert_eq!((args.width, args.height), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn managed_crop_requires_exact_padded_output_dimensions() {
        let args = Args::try_parse_from([
            "live-capture",
            "--hwnd", "0x1234",
            "--crop-min-x", "2",
            "--crop-min-y", "682",
            "--crop-max-x", "1262",
            "--crop-max-y", "750",
            "--width", "1264",
            "--height", "80",
            "--no-preview",
            "--shared-handle", "123",
            "--adapter-luid", "1",
        ]).unwrap();
        let CaptureSource::Crop { hwnd, crop } = resolve_source(&args).unwrap() else {
            panic!("expected crop source");
        };
        assert_eq!(hwnd, 0x1234);
        assert_eq!(crop.output_size(), Size2D::new(1264, 80));
    }

    #[test]
    fn incomplete_crop_source_is_rejected() {
        let error = Args::try_parse_from([
            "live-capture",
            "--hwnd", "123",
            "--crop-min-x", "2",
        ]).err().expect("partial crop should fail CLI validation");
        assert!(error.to_string().contains("--crop-min-y"));
    }
}
