//! `live-selector.exe` — local fixed-size preview of the auto-selector output.
//!
//! Uses the same foreground matching and Windows Graphics Capture path as
//! `live-capture --mode auto`, but presents frames directly through D3D11
//! instead of converting to NV12 or encoding an H.264 stream.

mod capture;
mod d3d11;
mod presenter;
mod resample;
mod selector;

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::Parser;
use crate::{
    capture::CaptureSession,
    selector::{SelectorConfig, SwapCommand, spawn_selector},
};
use nkcore::{
    os::windows::winit::{AppEvent, EventLoopExt as _},
    prelude::euclid::Size2D,
};
use presenter::Presenter;
use windows::Win32::{
    Foundation::HWND,
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
};
use winit::{
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ControlFlow, EventLoop},
    raw_window_handle::{HasWindowHandle as _, RawWindowHandle},
    window::{Window, WindowButtons},
};

/// Default preview width in physical pixels.
const DEFAULT_WIDTH: u32 = 1920;
/// Default preview height in physical pixels.
const DEFAULT_HEIGHT: u32 = 1200;
/// Background used for letterboxing and periods without an active target.
const CLEAR_COLOR: [f32; 4] = [41.0 / 255.0, 41.0 / 255.0, 41.0 / 255.0, 1.0];
/// Normal selector polling cadence, matching `live-capture --mode auto`.
const SELECTOR_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Capture retry delay after a window closes or WGC reports an error.
const CAPTURE_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Event-loop wake cadence while waiting for the next captured frame.
const ACTIVE_WAKE_INTERVAL: Duration = Duration::from_millis(1);
/// Reduced wake cadence while no target is available or the preview is hidden.
const IDLE_WAKE_INTERVAL: Duration = Duration::from_millis(50);

/// CLI arguments for the local selector preview.
#[derive(Parser)]
#[command(name = "live-selector", about = "Preview the auto-selector in a fixed-size window")]
struct Args {
    /// URL to poll for selector configuration.
    #[arg(long)]
    config_url: String,

    /// Fixed physical width of the preview client area.
    #[arg(long, default_value_t = DEFAULT_WIDTH, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,

    /// Fixed physical height of the preview client area.
    #[arg(long, default_value_t = DEFAULT_HEIGHT, value_parser = clap::value_parser!(u32).range(1..))]
    height: u32,

    /// Preview window title.
    #[arg(long, default_value = "Live Selector")]
    title: String,
}

/// Last selector target, retained so a failed WGC session can be recreated
/// without waiting for the foreground window to change.
struct SelectedTarget {
    /// Win32 window handle selected by the shared foreground matcher.
    hwnd: isize,
    /// Human-readable executable description used in lifecycle logs.
    capture_info: String,
}

impl From<SwapCommand> for SelectedTarget {
    fn from(command: SwapCommand) -> Self {
        Self {
            hwnd: command.hwnd,
            capture_info: command.capture_info,
        }
    }
}

fn main() {
    pretty_env_logger::init();

    if let Err(error) = run(Args::parse()) {
        eprintln!("fatal: {error:#}");
        std::process::exit(1);
    }
}

/// Create the preview window and run capture/presentation until it closes.
///
/// # Panics
///
/// Winit's resume callback cannot return a `Result`, so window, HWND, or D3D
/// initialization failures inside that callback panic with contextual messages.
fn run(args: Args) -> anyhow::Result<()> {
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
                        .with_enabled_buttons(
                            WindowButtons::CLOSE |
                            WindowButtons::MINIMIZE))
                    .expect("failed to create selector preview window");
            let preview_hwnd =
                hwnd_from_window(&window).expect("failed to obtain selector preview HWND");
            let presenter =
                Presenter::new(preview_hwnd, output_size, CLEAR_COLOR)
                    .expect("failed to initialize selector preview presenter");
            let swap_rx = spawn_selector(SelectorConfig {
                config_url: args.config_url,
                ignored_hwnd: preview_hwnd.0 as isize,
                poll_interval: SELECTOR_POLL_INTERVAL,
            });

            log::info!(
                "selector preview started: {}x{}, HWND=0x{:X}",
                output_size.width,
                output_size.height,
                preview_hwnd.0 as isize);

            let mut state = PreviewState {
                presenter,
                swap_rx,
                selected: None,
                capture: None,
                retry_at: Instant::now(),
                occluded: false,
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
struct PreviewState {
    /// GPU presentation state, declared before `window` so it drops first.
    presenter: Presenter,
    /// Selector updates consumed without blocking the window event loop.
    swap_rx: mpsc::Receiver<SwapCommand>,
    /// Latest selected target retained across capture-session failures.
    selected: Option<SelectedTarget>,
    /// Active WGC session, recreated independently of selector polling.
    capture: Option<CaptureSession>,
    /// Earliest retry time after a recoverable WGC initialization error.
    retry_at: Instant,
    /// Whether rendering should pause to avoid hidden-window GPU work.
    occluded: bool,
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
        self.apply_latest_selection(event_loop);

        let delay = if self.occluded {
            IDLE_WAKE_INTERVAL
        } else {
            self.ensure_capture();
            self.render_next_frame(event_loop);
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
    fn apply_latest_selection(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let mut latest = None;
        while let Ok(command) = self.swap_rx.try_recv() {
            latest = Some(command);
        }
        let Some(command) = latest else { return };

        let target = SelectedTarget::from(command);
        log::info!(
            "switching preview to HWND 0x{:X} ({})",
            target.hwnd,
            target.capture_info);
        self.capture = None;
        self.selected = Some(target);
        self.retry_at = Instant::now();

        if let Err(error) = self.presenter.clear_and_present() {
            log::error!("failed to clear preview during window switch: {error:#}");
            event_loop.exit();
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
                log::warn!("failed to open HWND 0x{:X}: {error:#}", target.hwnd);
                self.retry_at = Instant::now() + CAPTURE_RETRY_INTERVAL;
            }
        }
    }

    /// Render the newest available frame and recover from stale capture sessions.
    fn render_next_frame(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let Some(capture) = self.capture.as_mut() else { return };

        match capture.get_next_frame(self.presenter.device_context()) {
            Ok(Some(frame)) => {
                if let Err(error) = self.presenter.render(&frame.raw_texture, frame.size) {
                    log::error!("failed to render selector preview: {error:#}");
                    event_loop.exit();
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("capture session failed; retrying: {error:#}");
                self.capture = None;
                self.retry_at = Instant::now() + CAPTURE_RETRY_INTERVAL;
            }
        }
    }
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
