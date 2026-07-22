//! Minimal Phase 3 supervisor for the cross-process shared-texture proof.
//!
//! This executable deliberately proves only adapter selection, resource
//! lifetime, handle inheritance, worker launch, direct encoder stdout wiring,
//! and coupled shutdown. Restart policy, Job Objects, selector synchronization,
//! and stream metadata remain Phase 4 responsibilities of `live-stream`.

use std::{
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::Parser;
use euclid::default::Size2D;
use live_shared_texture::OwnedMailbox;

/// Child-liveness polling interval; supervision is not on a media hot path.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// CLI for the disposable Phase 3 shared-texture proof supervisor.
#[derive(Parser)]
#[command(name = "live-texture-proof")]
struct Args {
    /// Built `live-selector` executable to launch as the producer.
    #[arg(long)]
    selector: PathBuf,

    /// Built `live-encoder` executable to launch as the consumer.
    #[arg(long)]
    encoder: PathBuf,

    /// Local selector profile TOML passed through without interpretation.
    #[arg(long)]
    config: PathBuf,

    /// Fixed mailbox and encoded-stream width.
    #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,

    /// Fixed mailbox and encoded-stream height.
    #[arg(long, default_value_t = 1200, value_parser = clap::value_parser!(u32).range(1..))]
    height: u32,

    /// Encoder frame rate.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,

    /// Stop cleanly after this many seconds; intended for bounded proof runs.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    duration_seconds: Option<u64>,
}

fn main() {
    pretty_env_logger::init();
    let args = Args::parse();
    if let Err(error) = run(&args) {
        eprintln!("fatal: {error:#}");
        std::process::exit(1);
    }
}

/// Create one resource generation, launch both workers, and couple lifetimes.
fn run(args: &Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.width.is_multiple_of(16) && args.height.is_multiple_of(16),
        "width and height must be multiples of 16 (got {}x{})",
        args.width,
        args.height);
    let mut mailbox = OwnedMailbox::new(Size2D::new(args.width, args.height))
        .context("failed to create shared-texture resource generation")?;
    let adapter = mailbox.device_bundle().adapter_luid;
    let adapter_name = mailbox.device_bundle().adapter_name.clone();
    let inheritance = mailbox.inheritable_handle()?;
    let handle = inheritance.value();
    log::info!(
        "shared-texture proof: {}x{}, adapter={} ({}), inherited handle=0x{handle:X}",
        args.width,
        args.height,
        adapter,
        adapter_name);

    let selector = Command::new(&args.selector)
        .arg("--config").arg(&args.config)
        .arg("--width").arg(args.width.to_string())
        .arg("--height").arg(args.height.to_string())
        .arg("--shared-handle").arg(handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to launch selector worker {}", args.selector.display()))?;

    let encoder = match Command::new(&args.encoder)
        .arg("--mode").arg("shared")
        .arg("--width").arg(args.width.to_string())
        .arg("--height").arg(args.height.to_string())
        .arg("--fps").arg(args.fps.to_string())
        .arg("--shared-handle").arg(handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        // Inheriting the supervisor's stdout wires the encoder directly to the
        // caller's pipe without parsing or copying media in this process.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut selector = selector;
            let _ = selector.kill();
            let _ = selector.wait();
            return Err(error).with_context(|| {
                format!("failed to launch encoder worker {}", args.encoder.display())
            });
        }
    };

    // The guard revokes inheritance before any later descendant can receive
    // the mailbox. Existing child copies remain valid until those workers exit.
    inheritance.revoke()?;
    let mut children = ProofChildren { selector, encoder };
    let deadline = args.duration_seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let result = children.wait_for_exit(deadline);
    drop(mailbox);
    result
}

/// Both proof workers, killed together when either side exits or setup unwinds.
struct ProofChildren {
    /// Safe-capture producer and local preview.
    selector: Child,
    /// Shared-copy consumer and stdout-first H.264 encoder.
    encoder: Child,
}

impl ProofChildren {
    /// Poll both workers until one exits, then terminate its peer.
    fn wait_for_exit(&mut self, deadline: Option<Instant>) -> anyhow::Result<()> {
        loop {
            if let Some(status) = self.selector.try_wait()
                .context("failed to query selector worker status")? {
                terminate(&mut self.encoder);
                anyhow::bail!("selector worker exited unexpectedly with {status}");
            }
            if let Some(status) = self.encoder.try_wait()
                .context("failed to query encoder worker status")? {
                terminate(&mut self.selector);
                return exit_result("encoder", status);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                log::info!("bounded shared-texture proof completed");
                return Ok(());
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
    }
}

impl Drop for ProofChildren {
    fn drop(&mut self) {
        terminate(&mut self.selector);
        terminate(&mut self.encoder);
    }
}

/// Best-effort bounded cleanup for one already-started worker.
fn terminate(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Treat the encoder's clean downstream-pipe exit as normal proof completion.
fn exit_result(component: &str, status: ExitStatus) -> anyhow::Result<()> {
    if status.success() {
        log::info!("{component} worker exited cleanly");
        Ok(())
    } else {
        anyhow::bail!("{component} worker exited with {status}")
    }
}
