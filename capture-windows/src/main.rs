//! One-process Windows capture and H.264 host for TurboCapture M0.

mod device;
mod encoder;
mod frame;
mod h264;
mod media;
mod observation;

use std::process::ExitCode;

use anyhow::Context as _;
use capture_core::{
    CaptureArgs, ChannelCapacities, InstanceService, MediaCompletion, load_config,
};
use clap::Parser as _;

/// Parse startup identity, run one instance, and preserve a non-zero fatal exit.
fn main() -> ExitCode {
    let args = CaptureArgs::parse();
    if let Err(error) = initialize_logging(&args.log_filter) {
        eprintln!("fatal: {error:#}");
        return ExitCode::FAILURE;
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("fatal: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Initialize process-global logging from the shared CLI filter.
fn initialize_logging(filter: &str) -> anyhow::Result<()> {
    env_logger::Builder::new()
        .parse_filters(filter)
        .try_init()
        .context("failed to initialize logging")
}

/// Establish DPI/config/service state before starting the native owner.
fn run(args: CaptureArgs) -> anyhow::Result<()> {
    set_dpi_awareness::per_monitor_v2()
        .context("failed to enable per-monitor-v2 DPI awareness")?;
    let config = load_config(&args.config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create capture-core runtime")?;
    runtime.block_on(run_instance(args, config))
}

/// Run the Phase 2 host with its core dispatcher alive but listener unbound.
async fn run_instance(
    args: CaptureArgs,
    config: capture_core::ValidatedInstanceConfig) -> anyhow::Result<()> {
    let (mut service, channels) = InstanceService::new(config, ChannelCapacities::default())?;
    let _router = service.router()?;
    let completion = service.take_media_completion()
        .context("capture-core media completion receiver was already taken")?;
    let media_thread = media::spawn(
        media::MediaStartup {
            adapter_luid: args.adapter_luid,
            encoder_name: args.encoder_name.clone(),
        },
        channels)?;
    log::info!(
        "Phase 2 instance is running headlessly; API endpoint {}:{} remains unbound until Phase 3",
        args.listen_address,
        args.port);

    let completion = completion.await
        .context("media thread exited without reporting terminal status")?;
    media_thread.join()
        .map_err(|_panic| anyhow::anyhow!("media thread panicked after completion"))?;
    match completion {
        MediaCompletion::Clean => Ok(()),
        MediaCompletion::Fatal { message } => anyhow::bail!(message),
    }
}
