//! `live-stream` — process and resource supervisor for one video stream.
//!
//! Main mode owns the shared-texture GPU cohort. YouTube Music mode owns fixed
//! window discovery and crop orchestration. Both wire encoder stdout directly
//! into `live-ws` stdin without parsing media frames.

mod metadata;
mod restart;
mod youtube_music;

use std::{
    fs,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::{Parser, ValueEnum};
use euclid::default::Size2D;
use live_shared_texture::OwnedMailbox;
use metadata::{MetadataPoster, spawn_selector_reader};
use restart::{Component, RecoveryScope, RestartBackoff, recovery_scope};
use win32job::{ExtendedLimitInfo, Job};

/// Child-status polling remains off the media path and needs no busy wait.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// User-facing stream topologies owned exclusively by the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StreamMode {
    /// Local profile selector, shared-texture encoder, and video relay.
    Main,
    /// YouTube Music window discovery, generic crop encoder, and video relay.
    YoutubeMusic,
}

impl StreamMode {
    /// Stable CLI/metadata spelling for this topology.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::YoutubeMusic => "youtube-music",
        }
    }

    /// Well-known transport identifier used when the caller omits one.
    pub const fn default_stream_id(self) -> &'static str { self.name() }

    /// Existing per-stream frame-rate defaults preserved across integration.
    pub const fn default_fps(self) -> u32 {
        match self {
            Self::Main => 60,
            Self::YoutubeMusic => 15,
        }
    }
}

/// Supervisor CLI carrying all local worker paths and stream configuration.
#[derive(Parser)]
#[command(name = "live-stream", about = "Supervise one well-known video stream")]
struct Args {
    /// Supervisor-owned topology selection.
    #[arg(long, value_enum, default_value = "main")]
    mode: StreamMode,

    /// Built `live-selector` executable required by main mode.
    #[arg(long)]
    selector: Option<PathBuf>,

    /// Built `live-encoder` executable.
    #[arg(long)]
    encoder: PathBuf,

    /// Built `live-ws` executable.
    #[arg(long)]
    relay: PathBuf,

    /// Local selector TOML carried unchanged by main mode.
    #[arg(long)]
    config: Option<PathBuf>,

    /// WebSocket ingestion URL passed unchanged to `live-ws`.
    #[arg(long)]
    server: String,

    /// HTTP endpoint receiving main-mode selection metadata.
    #[arg(long)]
    info_url: Option<String>,

    /// Transport identifier; defaults to the selected mode's well-known ID.
    #[arg(long)]
    stream_id: Option<String>,

    /// Fixed mailbox and encoded-stream width.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    width: Option<u32>,

    /// Fixed mailbox and encoded-stream height.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    height: Option<u32>,

    /// Encoder output frame rate.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: Option<u32>,

    /// Window title prefix required by YouTube Music mode.
    #[arg(long)]
    youtube_music_title: Option<String>,

    /// YouTube Music window rediscovery interval after capture loss.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    poll_interval_seconds: Option<u64>,

    /// Stop successfully after a bounded hardware/integration proof.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    duration_seconds: Option<u64>,

    /// Inject one selector exit while holding the producer keyed mutex.
    #[arg(long, hide = true, value_parser = clap::value_parser!(u64).range(1..))]
    fault_abandon_selector_after_publications: Option<u64>,
}

/// Validated immutable main-stream configuration shared by every generation.
struct MainConfig {
    /// Canonical selector executable path.
    selector: PathBuf,
    /// Canonical encoder executable path.
    encoder: PathBuf,
    /// Canonical relay executable path.
    relay: PathBuf,
    /// Canonical local TOML path passed directly to every selector generation.
    selector_config: PathBuf,
    /// Relay WebSocket destination.
    server: String,
    /// Well-known stream identifier.
    stream_id: String,
    /// Fixed resource dimensions.
    size: Size2D<u32>,
    /// Fixed encoder frame rate.
    fps: u32,
    /// One-shot keyed-mutex abandonment count used by hardware verification.
    abandonment_fault: Option<u64>,
}

/// Fully validated mode-specific invocation.
enum ValidatedMode {
    /// Safe profile selector and shared-texture cohort.
    Main {
        /// Resource and worker configuration.
        config: MainConfig,
        /// Server endpoint for selector transition metadata.
        info_url: String,
    },
    /// Fixed YouTube Music crop producer and relay.
    YoutubeMusic(youtube_music::Config),
}

fn main() {
    pretty_env_logger::init();
    let args = Args::parse();
    let exit_code = match run(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("fatal: {error:#}");
            1
        }
    };

    // `run` has already dropped and reaped its Rust child wrappers. Exiting
    // closes the intentionally retained Job Object handle at the kernel level,
    // which also covers abrupt paths that never reached those wrappers.
    std::process::exit(exit_code);
}

/// Validate inputs, establish race-free Job containment, and supervise forever.
fn run(args: Args) -> anyhow::Result<()> {
    let deadline = args.duration_seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let config = validate_args(args)?;

    // Assigning the supervisor before any spawn makes every descendant join the
    // kill-on-close job atomically. `ManuallyDrop` avoids closing a job containing
    // the current process during Rust unwinding; process exit closes it instead.
    let job = create_containment_job()?;
    let _job = ManuallyDrop::new(job);
    match config {
        ValidatedMode::Main { config, info_url } => {
            let metadata = MetadataPoster::spawn(info_url, config.stream_id.clone())?;
            let mut supervisor = MainSupervisor::new(config, metadata);
            supervisor.start()?;
            supervisor.monitor(deadline)
        }
        ValidatedMode::YoutubeMusic(config) => {
            set_dpi_awareness::per_monitor_v2()
                .context("failed to enable per-monitor DPI awareness")?;
            youtube_music::run(config, deadline)
        }
    }
}

/// Reject ambiguous mode combinations before creating any kernel resource.
fn validate_args(args: Args) -> anyhow::Result<ValidatedMode> {
    let stream_id = args.stream_id
        .unwrap_or_else(|| args.mode.default_stream_id().to_owned());
    anyhow::ensure!(!stream_id.trim().is_empty(), "stream ID must not be empty");
    anyhow::ensure!(
        args.server.starts_with("ws://") || args.server.starts_with("wss://"),
        "--server must be a ws:// or wss:// URL");
    let encoder = canonical_file(&args.encoder, "encoder executable")?;
    let relay = canonical_file(&args.relay, "relay executable")?;
    let fps = args.fps.unwrap_or_else(|| args.mode.default_fps());

    match args.mode {
        StreamMode::Main => {
            anyhow::ensure!(
                args.youtube_music_title.is_none() && args.poll_interval_seconds.is_none(),
                "--youtube-music-title and --poll-interval-seconds require --mode youtube-music");
            let width = args.width.unwrap_or(1920);
            let height = args.height.unwrap_or(1200);
            anyhow::ensure!(
                width.is_multiple_of(16) && height.is_multiple_of(16),
                "width and height must be multiples of 16 (got {width}x{height})");
            let info_url = args.info_url
                .context("--info-url is required for --mode main")?;
            anyhow::ensure!(
                info_url.starts_with("http://") || info_url.starts_with("https://"),
                "--info-url must be an http:// or https:// URL");
            let selector = args.selector
                .context("--selector is required for --mode main")?;
            let selector_config = args.config
                .context("--config is required for --mode main")?;
            Ok(ValidatedMode::Main {
                config: MainConfig {
                    selector: canonical_file(&selector, "selector executable")?,
                    encoder,
                    relay,
                    selector_config: canonical_file(&selector_config, "selector config")?,
                    server: args.server,
                    stream_id,
                    size: Size2D::new(width, height),
                    fps,
                    abandonment_fault: args.fault_abandon_selector_after_publications,
                },
                info_url,
            })
        }
        StreamMode::YoutubeMusic => {
            anyhow::ensure!(
                args.selector.is_none()
                    && args.config.is_none()
                    && args.info_url.is_none()
                    && args.width.is_none()
                    && args.height.is_none()
                    && args.fault_abandon_selector_after_publications.is_none(),
                "selector, config, metadata, dimensions, and selector fault options require --mode main");
            let title = args.youtube_music_title
                .context("--youtube-music-title is required for --mode youtube-music")?;
            anyhow::ensure!(!title.is_empty(), "YouTube Music title prefix must not be empty");
            Ok(ValidatedMode::YoutubeMusic(youtube_music::Config {
                encoder,
                relay,
                server: args.server,
                stream_id,
                title,
                fps,
                poll_interval: Duration::from_secs(args.poll_interval_seconds.unwrap_or(5)),
            }))
        }
    }
}

/// Canonicalize one required regular file for stable restart diagnostics.
fn canonical_file(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(path.is_file(), "{label} is not a file: {}", path.display());
    fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize {label} {}", path.display()))
}

/// Create one kill-on-close job and place the supervisor inside it before spawn.
fn create_containment_job() -> anyhow::Result<Job> {
    let mut limits = ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = Job::create_with_limit_info(&limits)
        .context("failed to create kill-on-close Job Object")?;
    job.assign_current_process()
        .context("failed to assign live-stream to its Job Object")?;
    Ok(job)
}

/// Stateful process/resource owner for one well-known stream.
struct MainSupervisor {
    /// Immutable worker paths and stream settings.
    config: MainConfig,
    /// Non-media metadata poster shared with selector stdout readers.
    metadata: MetadataPoster,
    /// Monotonic resource-generation identifier.
    generation: u64,
    /// Creation time of the current complete resource generation.
    generation_started: Instant,
    /// Current mailbox; declared before children so children drop first.
    mailbox: Option<OwnedMailbox>,
    /// Safe selector process and JSONL reader.
    selector: Option<SelectorProcess>,
    /// Encoder plus its directly connected relay.
    pipeline: Option<EncoderRelay>,
    /// Selector-local consecutive failure policy.
    selector_backoff: RestartBackoff,
    /// Encoder/relay consecutive failure policy.
    pipeline_backoff: RestartBackoff,
    /// Complete-resource consecutive failure policy.
    generation_backoff: RestartBackoff,
    /// Whether the optional selector abandonment hook has already been launched.
    abandonment_fault_consumed: bool,
}

impl MainSupervisor {
    /// Construct an empty supervisor before the first generation attempt.
    fn new(config: MainConfig, metadata: MetadataPoster) -> Self {
        Self {
            config,
            metadata,
            generation: 0,
            generation_started: Instant::now(),
            mailbox: None,
            selector: None,
            pipeline: None,
            selector_backoff: RestartBackoff::default(),
            pipeline_backoff: RestartBackoff::default(),
            generation_backoff: RestartBackoff::default(),
            abandonment_fault_consumed: false,
        }
    }

    /// Start the first generation immediately, then back off boundedly on error.
    fn start(&mut self) -> anyhow::Result<()> {
        loop {
            match self.start_generation_once() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::error!("resource generation startup failed: {error:#}");
                    let delay = next_delay(
                        &mut self.generation_backoff,
                        Duration::ZERO,
                        "resource generation")?;
                    thread::sleep(delay);
                }
            }
        }
    }

    /// Poll child state and apply the smallest safe recovery boundary.
    fn monitor(&mut self, deadline: Option<Instant>) -> anyhow::Result<()> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                log::info!(
                    "@{} generation {}: bounded live-stream proof completed",
                    self.config.stream_id,
                    self.generation);
                return Ok(());
            }
            if let Some(observed) = self.poll_exit()? {
                let scope = recovery_scope(observed.component, observed.status.code());
                log::warn!(
                    "@{} generation {}: {} exited with {}; recovery={scope:?}",
                    self.config.stream_id,
                    self.generation,
                    observed.component.name(),
                    observed.status);
                match scope {
                    RecoveryScope::Selector => self.restart_selector(observed.stable_for)?,
                    RecoveryScope::EncoderRelay => self.restart_pipeline(observed.stable_for)?,
                    RecoveryScope::ResourceGeneration => {
                        self.restart_generation(self.generation_started.elapsed())?;
                    }
                }
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
    }

    /// Create and transactionally publish one complete worker/resource cohort.
    fn start_generation_once(&mut self) -> anyhow::Result<()> {
        self.generation = self.generation
            .checked_add(1)
            .context("resource generation counter overflowed")?;
        let generation = self.generation;
        self.metadata.post_inactive(generation, StreamMode::Main.name());

        let mut mailbox = OwnedMailbox::new(self.config.size)
            .with_context(|| format!("generation {generation}: failed to create shared mailbox"))?;
        let adapter = mailbox.device_bundle().adapter_luid;
        let adapter_name = mailbox.device_bundle().adapter_name.clone();
        log::info!(
            "@{} generation {generation}: {}x{} on adapter {} ({adapter_name})",
            self.config.stream_id,
            self.config.size.width,
            self.config.size.height,
            adapter);

        let abandonment_fault = if self.abandonment_fault_consumed {
            None
        } else {
            self.config.abandonment_fault
        };
        let selector = spawn_selector(
            &self.config,
            &mut mailbox,
            generation,
            self.metadata.clone(),
            abandonment_fault)?;
        self.abandonment_fault_consumed |= abandonment_fault.is_some();
        let pipeline = spawn_pipeline(&self.config, &mut mailbox, generation)?;

        self.mailbox = Some(mailbox);
        self.selector = Some(selector);
        self.pipeline = Some(pipeline);
        self.generation_started = Instant::now();
        self.selector_backoff.reset();
        self.pipeline_backoff.reset();
        Ok(())
    }

    /// Return the first observed child exit without waiting on a healthy peer.
    fn poll_exit(&mut self) -> anyhow::Result<Option<ObservedExit>> {
        if let Some(selector) = self.selector.as_mut()
            && let Some(status) = selector
                .child
                .try_wait()
                .context("failed to query live-selector status")?
        {
            return Ok(Some(ObservedExit {
                component: Component::Selector,
                status,
                stable_for: selector.started.elapsed(),
            }));
        }
        if let Some(pipeline) = self.pipeline.as_mut() {
            if let Some(status) = pipeline
                .encoder
                .try_wait()
                .context("failed to query live-encoder status")?
            {
                return Ok(Some(ObservedExit {
                    component: Component::Encoder,
                    status,
                    stable_for: pipeline.started.elapsed(),
                }));
            }
            if let Some(status) = pipeline
                .relay
                .try_wait()
                .context("failed to query live-ws status")?
            {
                return Ok(Some(ObservedExit {
                    component: Component::Relay,
                    status,
                    stable_for: pipeline.started.elapsed(),
                }));
            }
        }
        Ok(None)
    }

    /// Restart only capture while retaining the complete mailbox and media pipe.
    fn restart_selector(&mut self, mut stable_for: Duration) -> anyhow::Result<()> {
        drop(self.selector.take());
        loop {
            let delay = next_delay(
                &mut self.selector_backoff,
                stable_for,
                "live-selector")?;
            thread::sleep(delay);
            let result = spawn_selector(
                &self.config,
                self.mailbox.as_mut().context("selector restart has no mailbox")?,
                self.generation,
                self.metadata.clone(),
                None);
            match result {
                Ok(selector) => {
                    self.selector = Some(selector);
                    return Ok(());
                }
                Err(error) => {
                    log::error!(
                        "@{} generation {}: selector restart failed: {error:#}",
                        self.config.stream_id,
                        self.generation);
                    stable_for = Duration::ZERO;
                }
            }
        }
    }

    /// Recreate the encoder stdout pipe and its relay without touching capture.
    fn restart_pipeline(&mut self, mut stable_for: Duration) -> anyhow::Result<()> {
        drop(self.pipeline.take());
        loop {
            let delay = next_delay(
                &mut self.pipeline_backoff,
                stable_for,
                "live-encoder/live-ws")?;
            thread::sleep(delay);
            let result = spawn_pipeline(
                &self.config,
                self.mailbox.as_mut().context("pipeline restart has no mailbox")?,
                self.generation);
            match result {
                Ok(pipeline) => {
                    self.pipeline = Some(pipeline);
                    return Ok(());
                }
                Err(error) => {
                    log::error!(
                        "@{} generation {}: media pipeline restart failed: {error:#}",
                        self.config.stream_id,
                        self.generation);
                    stable_for = Duration::ZERO;
                }
            }
        }
    }

    /// Drop every GPU-dependent worker and retry with a newly selected adapter.
    fn restart_generation(&mut self, mut stable_for: Duration) -> anyhow::Result<()> {
        drop(self.selector.take());
        drop(self.pipeline.take());
        drop(self.mailbox.take());
        loop {
            let delay = next_delay(
                &mut self.generation_backoff,
                stable_for,
                "resource generation")?;
            thread::sleep(delay);
            match self.start_generation_once() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::error!("resource generation restart failed: {error:#}");
                    stable_for = Duration::ZERO;
                }
            }
        }
    }
}

/// Charge one bounded restart attempt and produce a contextual exhaustion error.
pub(crate) fn next_delay(
    backoff: &mut RestartBackoff,
    stable_for: Duration,
    component: &str) -> anyhow::Result<Duration> {
    let delay = backoff.next_delay(stable_for).with_context(|| format!(
        "{component} restart budget exhausted after {} consecutive attempts",
        backoff.failures()))?;
    log::warn!(
        "restarting {component} in {:.2}s (attempt {})",
        delay.as_secs_f64(),
        backoff.failures());
    Ok(delay)
}

/// One child status paired with the uptime used to reset its backoff history.
struct ObservedExit {
    /// Process role used to choose the restart boundary.
    component: Component,
    /// Concrete exit status retained for logs and stable-code classification.
    status: ExitStatus,
    /// Uptime of the failed component group.
    stable_for: Duration,
}

/// Selector process plus the thread draining its unconditional JSONL stdout.
struct SelectorProcess {
    /// Managed selector child.
    child: Child,
    /// Time this selector attempt became live.
    started: Instant,
    /// Reader exits after the child closes its stdout pipe.
    reader: Option<thread::JoinHandle<()>>,
}

impl Drop for SelectorProcess {
    fn drop(&mut self) {
        terminate(&mut self.child);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Encoder and relay bound to one unparsed anonymous stdout pipe.
struct EncoderRelay {
    /// Shared-texture consumer and H.264 producer.
    encoder: Child,
    /// Direct stdin consumer and WebSocket reconnect worker.
    relay: Child,
    /// Common creation time for pipeline backoff stability.
    started: Instant,
}

impl Drop for EncoderRelay {
    fn drop(&mut self) {
        // Close the reader first so an encoder still writing observes a broken
        // pipe, then reap both sides before any replacement pipe is created.
        terminate(&mut self.relay);
        terminate(&mut self.encoder);
    }
}

/// Launch a selector with the mailbox inheritable only during this exact spawn.
fn spawn_selector(
    config: &MainConfig,
    mailbox: &mut OwnedMailbox,
    generation: u64,
    metadata: MetadataPoster,
    abandonment_fault: Option<u64>) -> anyhow::Result<SelectorProcess> {
    let adapter = mailbox.device_bundle().adapter_luid;
    let inheritance = mailbox.inheritable_handle()?;
    let handle = inheritance.value();
    let mut command = Command::new(&config.selector);
    command
        .arg("--config").arg(&config.selector_config)
        .arg("--width").arg(config.size.width.to_string())
        .arg("--height").arg(config.size.height.to_string())
        .arg("--shared-handle").arg(handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(after) = abandonment_fault {
        command
            .arg("--fault-abandon-after-publications")
            .arg(after.to_string());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!(
            "generation {generation}: failed to launch {}",
            config.selector.display()))?;
    if let Err(error) = inheritance.revoke() {
        terminate(&mut child);
        return Err(error).context("failed to revoke mailbox inheritance after selector spawn");
    }
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        anyhow::bail!("generation {generation}: selector stdout pipe was not created");
    };
    let reader = match spawn_selector_reader(
        stdout,
        generation,
        StreamMode::Main.name(),
        metadata)
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate(&mut child);
            return Err(error);
        }
    };
    log::info!(
        "@{} generation {generation}: launched live-selector pid={}",
        config.stream_id,
        child.id());
    Ok(SelectorProcess {
        child,
        started: Instant::now(),
        reader: Some(reader),
    })
}

/// Launch encoder then attach its stdout handle directly as relay stdin.
fn spawn_pipeline(
    config: &MainConfig,
    mailbox: &mut OwnedMailbox,
    generation: u64) -> anyhow::Result<EncoderRelay> {
    let adapter = mailbox.device_bundle().adapter_luid;
    let inheritance = mailbox.inheritable_handle()?;
    let handle = inheritance.value();
    let mut encoder = Command::new(&config.encoder)
        .arg("--mode").arg("shared")
        .arg("--width").arg(config.size.width.to_string())
        .arg("--height").arg(config.size.height.to_string())
        .arg("--fps").arg(config.fps.to_string())
        .arg("--stream-id").arg(&config.stream_id)
        .arg("--shared-handle").arg(handle.to_string())
        .arg("--adapter-luid").arg(adapter.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!(
            "generation {generation}: failed to launch {}",
            config.encoder.display()))?;
    if let Err(error) = inheritance.revoke() {
        terminate(&mut encoder);
        return Err(error).context("failed to revoke mailbox inheritance after encoder spawn");
    }
    let Some(encoded_stdout) = encoder.stdout.take() else {
        terminate(&mut encoder);
        anyhow::bail!("generation {generation}: encoder stdout pipe was not created");
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
            return Err(error).with_context(|| format!(
                "generation {generation}: failed to launch {}",
                config.relay.display()));
        }
    };
    log::info!(
        "@{} generation {generation}: launched live-encoder pid={} -> live-ws pid={}",
        config.stream_id,
        encoder.id(),
        relay.id());
    Ok(EncoderRelay {
        encoder,
        relay,
        started: Instant::now(),
    })
}

/// Best-effort bounded cleanup for one already-started managed child.
pub(crate) fn terminate(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
}
