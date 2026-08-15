//! Command-line arguments shared by every `capture-windows` instance.

use std::{
    fmt,
    net::IpAddr,
    num::NonZeroU16,
    path::PathBuf,
    str::FromStr,
};

use clap::Parser;

/// Stable command-line representation of a DXGI adapter LUID.
///
/// The value is deliberately an opaque integer here. The Windows host is
/// responsible for converting it to the platform-specific two-word `LUID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterLuid(u64);

impl AdapterLuid {
    /// Returns the lossless unsigned representation supplied on the command line.
    #[inline]
    pub const fn get(self) -> u64 { self.0 }
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:016X}", self.0)
    }
}

impl FromStr for AdapterLuid {
    type Err = ParseAdapterLuidError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let parsed = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .map_or_else(
                || value.parse::<u64>(),
                |hex| u64::from_str_radix(hex, 16))
            .map_err(|_parse_error| ParseAdapterLuidError)?;
        if parsed == 0 {
            return Err(ParseAdapterLuidError);
        }
        Ok(Self(parsed))
    }
}

/// Error returned when an adapter LUID is zero or is not valid decimal/hexadecimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("adapter LUID must be a non-zero decimal integer or 0x-prefixed hexadecimal integer")]
pub struct ParseAdapterLuidError;

/// Startup arguments consumed by the `capture-windows` binary.
///
/// Listener and hardware identity are startup-only settings: changing any of
/// them requires replacing the process instead of mutating a live instance.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "capture-windows", about = "Capture and serve one Windows video stream")]
pub struct CaptureArgs {
    /// Initial complete instance configuration in TOML format.
    #[arg(long)]
    pub config: PathBuf,

    /// Explicit interface on which the private instance API listens.
    #[arg(long)]
    pub listen_address: IpAddr,

    /// Explicit non-zero TCP port for this independent instance.
    #[arg(long)]
    pub port: NonZeroU16,

    /// Exact DXGI adapter LUID required by the configured machine.
    #[arg(long)]
    pub adapter_luid: AdapterLuid,

    /// Exact Media Foundation encoder display name required at startup.
    #[arg(long, value_parser = parse_non_empty)]
    pub encoder_name: String,

    /// Logging filter consumed by the process entry point.
    #[arg(long, default_value = "info")]
    pub log_filter: String,
}

/// Trim and reject an empty startup identity without adding another public type.
fn parse_non_empty(value: &str) -> Result<String, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        Err("value must not be empty")
    } else {
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_luid_should_parse_decimal_and_hexadecimal_forms() {
        assert_eq!("42".parse::<AdapterLuid>().map(AdapterLuid::get), Ok(42));
        assert_eq!("0x2A".parse::<AdapterLuid>().map(AdapterLuid::get), Ok(42));
    }

    #[test]
    fn adapter_luid_should_reject_zero() {
        assert_eq!("0".parse::<AdapterLuid>(), Err(ParseAdapterLuidError));
    }

    #[test]
    fn capture_args_should_require_explicit_instance_and_hardware_identity() {
        let args = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--listen-address", "127.0.0.1",
            "--port", "48100",
            "--adapter-luid", "0x2A",
            "--encoder-name", "NVIDIA H.264 Encoder"])
            .expect("representative command line should parse");

        assert_eq!(args.port.get(), 48_100);
        assert_eq!(args.adapter_luid.get(), 42);
    }

    #[test]
    fn capture_args_should_reject_an_empty_encoder_identity() {
        let result = CaptureArgs::try_parse_from([
            "capture-windows",
            "--config", "capture.toml",
            "--listen-address", "127.0.0.1",
            "--port", "48100",
            "--adapter-luid", "0x2A",
            "--encoder-name", "   "]);

        result.unwrap_err();
    }
}
