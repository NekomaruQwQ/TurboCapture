//! Dedicated native media owner and its bounded `capture-core` boundary.

use std::{
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, ensure};
use capture_core::{
    CaptureState, ConfigSnapshot, HostChannels, MediaCommand, MediaCompletion,
    MediaStatus, ObservationId, SelectionDecision, TargetSummary, VideoEvent,
    select_window,
};
use euclid::default::Size2D;
use tokio::sync::{mpsc, watch};
use windows::{
    Win32::{
        Graphics::Direct3D11::ID3D11ShaderResourceView,
        Graphics::Dxgi::{
            DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DEVICE_RESET,
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
    },
    core::Error as WindowsError,
};
use winrt_capture::{CaptureOptions, CaptureSession};

use crate::{
    device::{self, DeviceBundle},
    encoder::{EncoderEvent, H264Encoder, MediaFoundation, SurfaceReleaseTracker},
    frame::{FixedFrame, Nv12Pool},
    h264::H264Packetizer,
    observation::{NativeObservation, observe_windows},
};

/// Fixed startup identity not replaceable through the configuration channel.
#[derive(Debug, Clone)]
pub struct MediaStartup {
    /// Exact adapter-filtered H.264 MFT friendly name.
    pub encoder_name: String,
}

/// Spawn one named native owner and convert every exit into one completion event.
pub fn spawn(
    startup: MediaStartup,
    channels: HostChannels) -> anyhow::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("capture-media".to_owned())
        .spawn(move || {
            let HostChannels {
                configurations,
                commands,
                status,
                video,
                completion,
            } = channels;
            let result = run_owner(
                &startup,
                MediaChannels { configurations, commands, status, video });
            let completion_value = match result {
                Ok(()) => MediaCompletion::Clean,
                Err(error) => MediaCompletion::Fatal { message: format!("{error:#}") },
            };
            // Receiver loss means the async process domain is already exiting.
            let _ = completion.send(completion_value);
        })
        .context("failed to spawn dedicated media thread")
}

/// Media-side channel endpoints after the completion sender is separated.
struct MediaChannels {
    configurations: watch::Receiver<ConfigSnapshot>,
    commands: mpsc::Receiver<MediaCommand>,
    status: watch::Sender<MediaStatus>,
    video: mpsc::Sender<VideoEvent>,
}

/// Initialize native foundations in strict ownership order and enter the MFT loop.
fn run_owner(startup: &MediaStartup, channels: MediaChannels) -> anyhow::Result<()> {
    let _com = ComApartment::initialize()?;
    let _media_foundation = MediaFoundation::start()?;
    let device = device::create_and_validate()?;
    let initial = channels.configurations.borrow().clone();
    let video_config = &initial.config.config().video;
    let output_size = Size2D::new(video_config.width, video_config.height);
    let fixed_frame = FixedFrame::new(&device.device, &device.context, output_size)
        .context("failed to create fixed-output frame")?;
    let pool = Nv12Pool::new(&device.device, &device.context, &fixed_frame)
        .context("failed to create tracked NV12 surface pool")?;
    let release_tracker = SurfaceReleaseTracker::new(pool.surface_count());
    let encoder = H264Encoder::new(
        &device.device,
        device.adapter_luid,
        &startup.encoder_name,
        video_config)
        .context("failed to initialize exact H.264 encoder")?;
    let packetizer = H264Packetizer::new(video_config.width, video_config.height);
    let frame_clock = FrameClock::new(video_config.frame_rate)?;
    let mut owner = MediaOwner {
        device,
        channels,
        configuration: initial,
        fixed_frame,
        pool,
        release_tracker,
        encoder,
        packetizer,
        current_target: None,
        last_source: None,
        force_transform: false,
        next_observation: Instant::now(),
        force_observation: true,
        force_keyframe: true,
        frame_clock,
        status: StatusReporter::new(),
        performance: PerformanceMetrics::new(),
    };
    owner.publish_status()?;
    owner.run()
}

/// All mutable COM/GPU/media state, confined to one native thread.
struct MediaOwner {
    device: DeviceBundle,
    channels: MediaChannels,
    configuration: ConfigSnapshot,
    fixed_frame: FixedFrame,
    pool: Nv12Pool,
    release_tracker: SurfaceReleaseTracker,
    encoder: H264Encoder,
    packetizer: H264Packetizer,
    current_target: Option<CurrentTarget>,
    last_source: Option<SourceFrame>,
    force_transform: bool,
    next_observation: Instant,
    force_observation: bool,
    force_keyframe: bool,
    frame_clock: FrameClock,
    status: StatusReporter,
    performance: PerformanceMetrics,
}

impl MediaOwner {
    /// Drive the asynchronous MFT without allowing more than its requested inputs.
    fn run(&mut self) -> anyhow::Result<()> {
        let mut pending_inputs = 0usize;
        loop {
            self.recycle_surfaces()?;
            self.apply_commands()?;
            if pending_inputs > 0
                && let Some(slot) = self.pool.acquire()
            {
                self.submit_frame(slot)?;
                pending_inputs -= 1;
                continue;
            }

            match self.encoder.next_event()? {
                EncoderEvent::NeedInput => {
                    pending_inputs = pending_inputs.checked_add(1)
                        .context("encoder input-request count overflowed")?;
                }
                EncoderEvent::HaveOutput => self.publish_output()?,
                EncoderEvent::DrainComplete => log::debug!("encoder drain completed"),
                EncoderEvent::Other(event_type) => {
                    log::trace!("ignored informational encoder event {event_type:?}");
                }
            }
        }
    }

    /// Apply latest-value configuration and all rare media commands.
    fn apply_commands(&mut self) -> anyhow::Result<()> {
        match self.channels.configurations.has_changed() {
            Ok(true) => {
                self.configuration = self.channels.configurations.borrow_and_update().clone();
                self.force_observation = true;
                self.force_transform = true;
                log::info!(
                    "media owner applied configuration generation {}",
                    self.configuration.generation);
            }
            Ok(false) => {}
            Err(_closed) => anyhow::bail!("configuration channel closed while media owner was active"),
        }
        loop {
            match self.channels.commands.try_recv() {
                Ok(MediaCommand::RequestKeyframe) => self.force_keyframe = true,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    anyhow::bail!("media command channel closed while media owner was active");
                }
            }
        }
        Ok(())
    }

    /// Wait for the fixed cadence, refresh target/frame state, and submit one slot.
    fn submit_frame(&mut self, slot: usize) -> anyhow::Result<()> {
        self.frame_clock.wait();
        self.apply_commands()?;
        self.refresh_selection_if_due()?;

        let capture_started = Instant::now();
        self.capture_latest()?;
        self.performance.capture_submission += capture_started.elapsed();
        let convert_started = Instant::now();
        self.pool.prepare(slot, self.fixed_frame.revision())?;
        self.performance.conversion_submission += convert_started.elapsed();

        let callback = self.release_tracker.callback(slot)?;
        let texture = self.pool.texture(slot)?;
        let (timestamp, duration) = self.frame_clock.sample_times()?;
        let force_keyframe = self.force_keyframe;
        let encode_started = Instant::now();
        self.encoder.submit(texture, callback, timestamp, duration, force_keyframe)?;
        self.performance.encoder_submission += encode_started.elapsed();
        self.force_keyframe = false;
        self.status.tick();
        self.publish_status()?;
        self.performance.submitted = self.performance.submitted.saturating_add(1);
        self.performance.report_if_due();
        Ok(())
    }

    /// Re-run pure selection on a bounded cadence or immediately after policy loss.
    fn refresh_selection_if_due(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        if !self.force_observation && now < self.next_observation {
            return Ok(());
        }
        self.force_observation = false;
        self.next_observation = now + Duration::from_millis(500);
        let observations = observe_windows();
        let facts = observations.iter().map(|observation| observation.fact.clone()).collect::<Vec<_>>();
        let current = self.current_target.as_ref().map(|target| target.id);
        let decision = match select_window(
            self.configuration.config.selector(),
            &facts,
            current)
        {
            SelectionDecision::Keep(selection) => OwnedDecision::Keep {
                id: selection.window.id,
                profile: selection.profile.to_owned(),
            },
            SelectionDecision::Switch(selection) => OwnedDecision::Switch {
                id: selection.window.id,
                profile: selection.profile.to_owned(),
            },
            SelectionDecision::Wait => OwnedDecision::Wait,
        };
        self.apply_selection(decision, &observations)
    }

    /// Translate a pure selector decision back to one ephemeral WGC handle.
    fn apply_selection(
        &mut self,
        decision: OwnedDecision,
        observations: &[NativeObservation]) -> anyhow::Result<()> {
        match decision {
            OwnedDecision::Keep { id, profile } => {
                let selected = find_observation(observations, id)?;
                let summary = summary(selected, profile);
                if let Some(current) = self.current_target.as_mut() {
                    current.summary = summary.clone();
                }
                self.status.transition(CaptureState::Capturing, Some(summary));
            }
            OwnedDecision::Switch { id, profile } => {
                let selected = find_observation(observations, id)?;
                let summary = summary(selected, profile);
                self.status.transition(CaptureState::Switching, Some(summary.clone()));
                self.publish_status()?;
                self.current_target = None;
                self.last_source = None;
                self.force_transform = false;
                self.fixed_frame.clear()?;
                self.force_keyframe = true;
                let mut options = CaptureOptions::default();
                options.capture_cursor = false;
                match CaptureSession::from_hwnd(&self.device.device, selected.hwnd, &options) {
                    Ok(session) => {
                        log::info!(
                            "capturing profile '{}' from {} ({})",
                            summary.profile,
                            summary.executable_name,
                            summary.title);
                        self.current_target = Some(CurrentTarget { id, session, summary: summary.clone() });
                        self.status.transition(CaptureState::Capturing, Some(summary));
                    }
                    Err(error) => {
                        let error = error.context("target disappeared while creating WGC session");
                        if is_device_lost(&error) {
                            return Err(error).context("D3D device lost during target switch");
                        }
                        self.recover_target("target_open_failed", &error)?;
                    }
                }
            }
            OwnedDecision::Wait => {
                if self.current_target.take().is_some() {
                    log::info!("selected target closed or became ineligible; returning to waiting");
                }
                self.last_source = None;
                self.force_transform = false;
                self.fixed_frame.clear()?;
                self.status.transition(CaptureState::Waiting, None);
            }
        }
        self.publish_status()
    }

    /// Drain WGC's two-frame pool and render only its newest complete frame.
    fn capture_latest(&mut self) -> anyhow::Result<()> {
        let Some(target) = self.current_target.as_mut() else {
            self.performance.capture_misses = self.performance.capture_misses.saturating_add(1);
            return Ok(());
        };
        match target.session.get_next_frame(&self.device.context) {
            Ok(Some(frame)) => {
                let crop = self.configuration.config.config().source.crop;
                self.fixed_frame.update(&frame.texture_view, frame.size, crop)?;
                self.last_source = Some(SourceFrame {
                    view: frame.texture_view,
                    size: frame.size,
                });
                self.force_transform = false;
                self.status.captured()?;
            }
            Ok(None) => {
                // Reusing the last complete fixed frame is intentional. The WGC
                // wrapper already drains stale frames and retains only the newest.
                self.performance.capture_misses = self.performance.capture_misses.saturating_add(1);
                if self.force_transform
                    && let Some(source) = self.last_source.as_ref()
                {
                    let crop = self.configuration.config.config().source.crop;
                    self.fixed_frame.update(&source.view, source.size, crop)?;
                    self.force_transform = false;
                }
            }
            Err(error) => {
                let error = error.context("failed to acquire newest WGC frame");
                if is_device_lost(&error) {
                    return Err(error).context("D3D device lost during capture");
                }
                self.recover_target("capture_target_lost", &error)?;
            }
        }
        Ok(())
    }

    /// Convert an ordinary target race into clear/waiting state and reselection.
    fn recover_target(&mut self, condition: &str, error: &anyhow::Error) -> anyhow::Result<()> {
        log::warn!("recoverable media condition [{condition}]: {error:#}");
        self.current_target = None;
        self.last_source = None;
        self.force_transform = false;
        self.fixed_frame.clear()?;
        self.force_observation = true;
        self.force_keyframe = true;
        self.status.transition(CaptureState::Waiting, None);
        self.publish_status()
    }

    /// Packetize one encoder output and cross the bounded core channel.
    fn publish_output(&mut self) -> anyhow::Result<()> {
        let packetizer = &mut self.packetizer;
        let mut events = Vec::new();
        self.encoder.process_output(|bytes, timestamp_us| {
            events = packetizer.packetize(bytes, timestamp_us)?;
            Ok(())
        })?;
        for event in events {
            match &event {
                &VideoEvent::CodecConfiguration(ref codec) => log::info!(
                    "published H.264 codec generation {}",
                    codec.generation()),
                &VideoEvent::AccessUnit(ref unit) if unit.is_keyframe() => log::debug!(
                    "published IDR for codec generation {}",
                    unit.codec_generation()),
                &VideoEvent::AccessUnit(_) => {}
            }
            let is_access_unit = matches!(event, VideoEvent::AccessUnit(_));
            self.channels.video.blocking_send(event)
                .map_err(|_closed| anyhow::anyhow!("bounded encoded-output channel closed"))?;
            if is_access_unit {
                self.status.encoded()?;
                self.performance.encoded = self.performance.encoded.saturating_add(1);
            }
        }
        self.publish_status()
    }

    /// Apply every completed tracked-sample callback on the sole GPU owner.
    fn recycle_surfaces(&mut self) -> anyhow::Result<()> {
        for slot in self.release_tracker.drain() {
            self.pool.release(slot)?;
        }
        Ok(())
    }

    /// Publish latest status and treat loss of the async owner as terminal.
    fn publish_status(&self) -> anyhow::Result<()> {
        ensure!(!self.channels.status.is_closed(), "media status channel closed");
        self.channels.status.send_replace(self.status.snapshot.clone());
        Ok(())
    }
}

/// One active WGC session and its core-facing identity.
struct CurrentTarget {
    id: ObservationId,
    session: CaptureSession,
    summary: TargetSummary,
}

/// Last complete WGC source retained for crop-only configuration updates.
struct SourceFrame {
    view: ID3D11ShaderResourceView,
    size: Size2D<u32>,
}

/// Borrow-free selector result used after the fact vector leaves scope.
enum OwnedDecision {
    Keep { id: ObservationId, profile: String },
    Switch { id: ObservationId, profile: String },
    Wait,
}

/// Find the native half of a selection made from the same complete snapshot.
fn find_observation(
    observations: &[NativeObservation],
    id: ObservationId) -> anyhow::Result<&NativeObservation> {
    observations.iter()
        .find(|observation| observation.fact.id == id)
        .context("selector returned an identity absent from its native snapshot")
}

/// Build a stable operator-facing target summary without exposing HWND/PID.
fn summary(observation: &NativeObservation, profile: String) -> TargetSummary {
    TargetSummary {
        profile,
        executable_name: observation.fact.executable_name.clone(),
        title: observation.fact.title.clone(),
    }
}

/// Media-clock pacing that skips accumulated deadlines after a stall.
struct FrameClock {
    started: Instant,
    next_deadline: Instant,
    interval: Duration,
    duration_100ns: i64,
}

impl FrameClock {
    /// Construct a fixed cadence from a validated non-zero frame rate.
    fn new(frame_rate: u32) -> anyhow::Result<Self> {
        ensure!(frame_rate > 0, "frame rate must be non-zero");
        let interval = Duration::from_secs_f64(1.0 / f64::from(frame_rate));
        let duration_100ns = (10_000_000u64 / u64::from(frame_rate)).max(1) as i64;
        let started = Instant::now();
        Ok(Self { started, next_deadline: started, interval, duration_100ns })
    }

    /// Sleep until the next useful deadline and discard accumulated lag.
    fn wait(&mut self) {
        let now = Instant::now();
        if now < self.next_deadline {
            thread::sleep(self.next_deadline - now);
        }
        let now = Instant::now();
        self.next_deadline = now + self.interval;
    }

    /// Return monotonic MF sample time/duration in 100 ns units.
    fn sample_times(&self) -> anyhow::Result<(i64, i64)> {
        let elapsed_100ns = self.started.elapsed().as_nanos() / 100;
        let timestamp = i64::try_from(elapsed_100ns).context("media timestamp exhausted i64")?;
        Ok((timestamp, self.duration_100ns))
    }
}

/// Latest status plus one-second rate baselines.
struct StatusReporter {
    snapshot: MediaStatus,
    rate_started: Instant,
    captured_baseline: u64,
    encoded_baseline: u64,
}

impl StatusReporter {
    /// Begin in the externally defined waiting state.
    fn new() -> Self {
        Self {
            snapshot: MediaStatus::default(),
            rate_started: Instant::now(),
            captured_baseline: 0,
            encoded_baseline: 0,
        }
    }

    /// Apply a visible lifecycle transition atomically to the next snapshot.
    fn transition(&mut self, state: CaptureState, target: Option<TargetSummary>) {
        self.snapshot.state = state;
        self.snapshot.target = target;
    }

    /// Count one newly captured WGC frame with checked lifetime arithmetic.
    fn captured(&mut self) -> anyhow::Result<()> {
        self.snapshot.captured_frames = self.snapshot.captured_frames.checked_add(1)
            .context("captured-frame counter exhausted")?;
        Ok(())
    }

    /// Count one encoded access unit with checked lifetime arithmetic.
    fn encoded(&mut self) -> anyhow::Result<()> {
        self.snapshot.encoded_frames = self.snapshot.encoded_frames.checked_add(1)
            .context("encoded-frame counter exhausted")?;
        Ok(())
    }

    /// Refresh recent rates no more than once per second.
    fn tick(&mut self) {
        let elapsed = self.rate_started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        self.snapshot.capture_rate =
            (self.snapshot.captured_frames - self.captured_baseline) as f64 / seconds;
        self.snapshot.encode_rate =
            (self.snapshot.encoded_frames - self.encoded_baseline) as f64 / seconds;
        self.captured_baseline = self.snapshot.captured_frames;
        self.encoded_baseline = self.snapshot.encoded_frames;
        self.rate_started = Instant::now();
    }
}

/// Release-build submission timings kept off per-frame logs.
struct PerformanceMetrics {
    started: Instant,
    submitted: u64,
    encoded: u64,
    capture_misses: u64,
    capture_submission: Duration,
    conversion_submission: Duration,
    encoder_submission: Duration,
}

impl PerformanceMetrics {
    /// Begin one five-second aggregate measurement interval.
    fn new() -> Self {
        Self {
            started: Instant::now(),
            submitted: 0,
            encoded: 0,
            capture_misses: 0,
            capture_submission: Duration::ZERO,
            conversion_submission: Duration::ZERO,
            encoder_submission: Duration::ZERO,
        }
    }

    /// Emit averages suitable for comparing the one-owner release pipeline.
    fn report_if_due(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        let denominator = self.submitted.max(1) as f64;
        log::info!(
            "media metrics: submitted={}, encoded={}, WGC misses={}, capture/resample={:.3} ms, BGRA->NV12={:.3} ms, encoder submit={:.3} ms",
            self.submitted,
            self.encoded,
            self.capture_misses,
            self.capture_submission.as_secs_f64() * 1000.0 / denominator,
            self.conversion_submission.as_secs_f64() * 1000.0 / denominator,
            self.encoder_submission.as_secs_f64() * 1000.0 / denominator);
        *self = Self::new();
    }
}

/// Thread-scoped multithreaded COM initialization.
struct ComApartment;

impl ComApartment {
    /// Initialize COM before any WinRT, DXGI, or MF object is created.
    fn initialize() -> anyhow::Result<Self> {
        // SAFETY: This is the first COM call on the named media thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("failed to initialize media-thread COM apartment")?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: Paired with this guard's successful `CoInitializeEx`.
        unsafe { CoUninitialize(); }
    }
}

/// Whether an anyhow chain contains a terminal DXGI device-loss code.
fn is_device_lost(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source.downcast_ref::<WindowsError>().is_some_and(|error| {
            matches!(
                error.code(),
                DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET | DXGI_ERROR_DEVICE_HUNG)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_clock_should_produce_positive_fixed_media_duration() {
        let clock = FrameClock::new(60).unwrap();
        let (timestamp, duration) = clock.sample_times().unwrap();
        assert!(timestamp >= 0);
        assert_eq!(duration, 166_666);
    }

    #[test]
    fn status_rates_should_use_delta_counts() {
        let mut reporter = StatusReporter::new();
        reporter.captured().unwrap();
        reporter.encoded().unwrap();
        reporter.rate_started = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap();
        reporter.tick();

        assert!(reporter.snapshot.capture_rate > 0.4);
        assert!(reporter.snapshot.encode_rate > 0.4);
    }
}
