//! Command-line arguments for one Windows capture process.

use std::{num::NonZeroU16, path::PathBuf};

use clap::Parser;

/// Startup arguments consumed by one `capture-windows` process.
///
/// The listener is fixed to IPv4 loopback and its port requires process restart
/// to change. GPU and encoder selection follow the internal hardware policy.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "capture-windows", about = "Capture and serve one Windows video stream")]
pub struct CaptureArgs {
    /// Initial complete instance configuration in TOML format.
    #[arg(long)]
    pub config: PathBuf,

    /// Non-zero TCP port for this independent loopback instance.
    #[arg(long)]
    pub port: NonZeroU16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_args_should_accept_only_config_and_port() {
        let args = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--port", "48100"])
            .expect("representative command line should parse");

        assert_eq!(args.port.get(), 48_100);
        assert_eq!(args.config, PathBuf::from("capture.toml"));
    }

    #[test]
    fn capture_args_should_reject_an_encoder_override() {
        let result = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--port", "48100",
            "--encoder-name", "NVIDIA H.264 Encoder"]);

        assert_eq!(result.unwrap_err().kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn capture_args_should_require_a_config() {
        let result = CaptureArgs::try_parse_from(["capture-windows", "--port", "48100"]);

        assert_eq!(result.unwrap_err().kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn capture_args_should_require_a_port() {
        let result = CaptureArgs::try_parse_from(["capture-windows", "--config", "capture.toml"]);

        assert_eq!(result.unwrap_err().kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn capture_args_should_reject_port_zero() {
        let result = CaptureArgs::try_parse_from([
            "capture-windows", "--config", "capture.toml", "--port", "0"]);

        assert_eq!(result.unwrap_err().kind(), clap::error::ErrorKind::ValueValidation);
    }
}
