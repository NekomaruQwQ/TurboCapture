//! Command-line arguments for one Windows capture process.

use std::{num::NonZeroU16, path::PathBuf};

use clap::{Parser, builder::NonEmptyStringValueParser};

/// Startup arguments consumed by one `capture-windows` process.
///
/// The listener is fixed to IPv4 loopback. Port and encoder identity are
/// startup-only settings, so changing either requires replacing the process.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "capture-windows", about = "Capture and serve one Windows video stream")]
pub struct CaptureArgs {
    /// Initial complete instance configuration in TOML format.
    #[arg(long)]
    pub config: PathBuf,

    /// Non-zero TCP port for this independent loopback instance.
    #[arg(long)]
    pub port: NonZeroU16,

    /// Exact Media Foundation encoder display name required at startup.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub encoder_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_args_should_require_instance_and_encoder_identity() {
        let args = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--port", "48100",
            "--encoder-name", "NVIDIA H.264 Encoder"])
            .expect("representative command line should parse");

        assert_eq!(args.port.get(), 48_100);
        assert_eq!(args.encoder_name, "NVIDIA H.264 Encoder");
    }

    #[test]
    fn capture_args_should_reject_an_empty_encoder_identity() {
        let result = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--port", "48100",
            "--encoder-name", ""]);

        result.unwrap_err();
    }
}
