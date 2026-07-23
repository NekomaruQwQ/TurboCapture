//! `live-encoder.exe` — fixed shared BGRA texture to H.264 on stdout.
//!
//! The worker has no window, selection, crop, or network policy. A supervisor
//! supplies one inherited shared-texture handle and the exact adapter contract;
//! encoded output remains `live-protocol` framing suitable for piping directly
//! into `live-ws`.

use std::{
    io::Write as _,
    sync::Mutex,
};

use clap::Parser;
use live_encoder::pipeline::{
    BgraTextureInput,
    DEFAULT_BITRATE,
    VideoEncoderConfig,
    spawn_stdout_encoder,
};
use live_shared_texture::{
    AdapterLuid,
    RESOURCE_GENERATION_LOST_EXIT_CODE,
    ResourceGenerationLost,
    SharedHandleValue,
    is_resource_generation_lost,
};
use nkcore::prelude::euclid::Size2D;

/// Fixed managed-input and encoder settings.
#[derive(Parser)]
#[command(name = "live-encoder", about = "Encode a managed shared BGRA texture to stdout")]
struct Args {
    /// Fixed input width, which must be a multiple of 16.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,

    /// Fixed input height, which must be a multiple of 16.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    height: u32,

    /// Supervisor-owned NT shared-texture handle inherited by this process.
    #[arg(long)]
    shared_handle: SharedHandleValue,

    /// DXGI adapter LUID selected by the supervisor for the GPU cohort.
    #[arg(long)]
    adapter_luid: AdapterLuid,

    /// Encoder frame rate.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,

    /// Stream identifier included only in diagnostics.
    #[arg(long)]
    stream_id: Option<String>,
}

fn main() {
    let args = Args::parse();
    init_logger(args.stream_id.clone());
    if let Err(error) = validate_dimensions(args.width, args.height)
        .and_then(|()| run(&args))
    {
        eprintln!("fatal: {error:#}");
        let exit_code = if is_resource_generation_lost(&error) {
            RESOURCE_GENERATION_LOST_EXIT_CODE
        } else {
            1
        };
        std::process::exit(exit_code);
    }
}

/// Reject dimensions that the existing NV12 and H.264 path cannot represent.
fn validate_dimensions(width: u32, height: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        width.is_multiple_of(16) && height.is_multiple_of(16),
        "width and height must be multiples of 16 (got {width}x{height})");
    Ok(())
}

/// Encode private copies consumed from the supervisor-owned latest-frame mailbox.
fn run(args: &Args) -> anyhow::Result<()> {
    let frame_size = Size2D::new(args.width, args.height);
    let bundle = live_shared_texture::create_device_on_adapter(args.adapter_luid, true)
        .map_err(|error| ResourceGenerationLost::new(format!(
            "failed to create encoder device on supervisor-selected adapter: {error:#}")))?;
    log::info!(
        "shared input: {}x{}, adapter={} ({})",
        args.width,
        args.height,
        bundle.adapter_luid,
        bundle.adapter_name);
    let input = BgraTextureInput::from_shared(
        bundle.device,
        bundle.context,
        args.shared_handle.into_owned(),
        frame_size)
        .map_err(|error| ResourceGenerationLost::new(format!(
            "failed to open supervisor shared texture: {error:#}")))?;
    let encoding_handle = spawn_stdout_encoder(input, VideoEncoderConfig {
        frame_rate: args.fps,
        bitrate: DEFAULT_BITRATE,
    })?;
    encoding_handle
        .join()
        .map_err(|_panic_payload| anyhow::anyhow!("encoding thread panicked"))?
}

/// Keep verbose Media Foundation discovery beside the executable while normal
/// lifecycle logs remain visible on stderr without corrupting protocol stdout.
fn init_logger(stream_id: Option<String>) {
    use pretty_env_logger::env_logger::fmt::Color;

    let encoder_log_file: Option<Mutex<std::fs::File>> = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|directory| directory.join("live-encoder.log")))
        .and_then(|path| std::fs::File::create(path).ok())
        .map(Mutex::new);
    let tag = stream_id.map_or_else(String::new, |id| format!(" @{id}"));

    pretty_env_logger::env_logger::Builder::from_env(
        pretty_env_logger::env_logger::Env::default().default_filter_or("info"))
        .format(move |buffer, record| {
            let is_encoder = record.target().starts_with("live_encoder::encoder");
            let is_diagnostic = record.level() >= log::Level::Info;
            if is_encoder
                && is_diagnostic
                && let Some(file) = encoder_log_file.as_ref()
            {
                let mut file = file.lock().expect("encoder diagnostic log mutex was poisoned");
                writeln!(
                    file,
                    "[{}{tag} {}] {}",
                    record.level(),
                    record.target(),
                    record.args())?;
                drop(file);
                return Ok(());
            }

            let level = buffer.default_styled_level(record.level());
            let mut tag_style = buffer.style();
            tag_style.set_color(Color::Cyan).set_bold(true);
            let mut target_style = buffer.style();
            target_style.set_color(Color::Black).set_bold(true);
            writeln!(
                buffer,
                " {level} {} {} > {}",
                tag_style.value(&tag),
                target_style.value(record.target()),
                record.args())
        })
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_must_match_encoder_alignment() {
        validate_dimensions(1920, 1200).unwrap();
        assert!(validate_dimensions(1919, 1200).is_err());
        assert!(validate_dimensions(1920, 1199).is_err());
    }
}
