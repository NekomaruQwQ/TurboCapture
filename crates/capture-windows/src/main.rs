//! One-process Windows capture and H.264 host for TurboCapture M0.

mod device;
mod encoder;
mod frame;
mod h264;
mod media;
mod observation;

use std::{
    future::IntoFuture as _,
    net::{Ipv4Addr, SocketAddr},
};

use anyhow::Context as _;
use capture_core::{
    ChannelCapacities, InstanceService, MediaCompletion, load_config,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use std::num::NonZero;
use std::path::PathBuf;

use clap::Parser;

/// Startup arguments consumed by one `capture-windows` process.
///
/// The listener is fixed to IPv4 loopback and its port requires process restart
/// to change. GPU and encoder selection follow the internal hardware policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[derive(Parser)]
#[command(name = "capture-windows", about = "Capture and serve one Windows video stream")]
pub struct CaptureArgs {
    /// Initial complete instance configuration in TOML format.
    #[arg(long, short = 'c', env = "CAPTURE_CONFIG", value_name = "FILE")]
    pub config: PathBuf,

    /// Non-zero TCP port for the viewer to connect to.
    #[arg(long, short = 'p', env = "CAPTURE_PORT", value_name = "PORT")]
    pub port: NonZero<u16>,
}

/// Parse startup identity, run one instance, and preserve a non-zero fatal exit.
fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    enable_per_monitor_dpi_awareness()
        .context("failed to enable per-monitor-v2 DPI awareness")?;

    let args = CaptureArgs::parse();
    let config = load_config(&args.config)?;
    let runtime =
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create capture-core runtime")?;
    runtime.block_on(run_instance(args, config))
}

/// Select physical-pixel Win32 geometry before observing or capturing windows.
fn enable_per_monitor_dpi_awareness() -> windows::core::Result<()> {
    // SAFETY: This runs once before the process calls any window or geometry API.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
}

/// Bind and serve one complete capture instance until its media owner exits.
async fn run_instance(
    args: CaptureArgs,
    config: capture_core::ValidatedInstanceConfig) -> anyhow::Result<()> {
    let (mut service, channels) = InstanceService::new(config, ChannelCapacities::default())?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port.get()));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind private instance API to {address}"))?;
    let router = service.router()?;
    let mut completion = service.take_media_completion()
        .context("capture-core media completion receiver was already taken")?;
    let media_thread = media::spawn(channels)?;
    log::info!("capture instance API listening on http://{address}");

    let server = axum::serve(listener, router).into_future();
    tokio::pin!(server);
    tokio::select! {
        biased;
        completion = &mut completion => {
            let completion = completion
                .context("media thread exited without reporting terminal status")?;
            media_thread.join()
                .map_err(|_panic| anyhow::anyhow!("media thread panicked after completion"))?;
            match completion {
                MediaCompletion::Clean => Ok(()),
                MediaCompletion::Fatal { message } => anyhow::bail!(message),
            }
        }
        result = &mut server => {
            result.context("capture instance API server failed")?;
            anyhow::bail!("capture instance API server stopped unexpectedly")
        }
    }
}
