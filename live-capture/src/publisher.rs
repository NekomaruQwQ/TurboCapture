//! Non-blocking publication of captured frames into a managed shared texture.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use live_capture_shared::{
    AcquireStatus,
    CONSUMER_KEY,
    InheritedHandle,
    OpenedMailbox,
    PRODUCER_KEY,
    ResourceGenerationLost,
};
use nkcore::prelude::euclid::Size2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device,
    ID3D11DeviceContext,
    ID3D11RenderTargetView,
    ID3D11Texture2D,
};

use crate::{
    capture::{CropBox, calculate_resample_viewport},
    d3d11,
    resample::Resampler,
};

/// Interval for aggregated publication-rate and miss diagnostics.
const METRICS_INTERVAL: Duration = Duration::from_secs(5);

/// GPU operation that maps one captured frame into the fixed mailbox.
#[derive(Debug, Clone, Copy)]
pub enum FrameTransform {
    /// Aspect-preserving scale with clear-color letterboxing.
    Resample,
    /// Native-pixel subrectangle with right/bottom encoder padding.
    Crop(CropBox),
}

/// Optional managed output attached to the standalone capture renderer.
pub struct SharedPublisher {
    /// Open shared mailbox, including its two-key mutex.
    mailbox: OpenedMailbox,
    /// Render target view used by the selector resampler.
    render_target: ID3D11RenderTargetView,
    /// Independent shader state because the preview presenter's state is private.
    resampler: Resampler,
    /// Letterbox color shared with the local preview.
    clear_color: [f32; 4],
    /// Aggregated hot-path measurements emitted outside individual frame logs.
    metrics: PublicationMetrics,
    /// Optional one-shot proof hook that exits while the producer key is owned.
    abandonment_fault: Option<AbandonmentFault>,
}

impl SharedPublisher {
    /// Open the inherited mailbox and publish its deterministic initial clear.
    ///
    /// The caller's device must use the supervisor-selected adapter. The
    /// inherited kernel handle is closed when this constructor returns because
    /// the opened texture retains an independent COM reference.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        handle: InheritedHandle,
        output_size: Size2D<u32>,
        clear_color: [f32; 4],
        debug_abandon_after_acquisitions: Option<u64>) -> anyhow::Result<Self> {
        let mailbox = OpenedMailbox::open(device, &handle, output_size)?;
        drop(handle);
        let render_target = d3d11::create_rtv_for_texture_2d(device, &mailbox.texture)
            .context("failed to create shared-texture render target")?;
        let publisher = Self {
            mailbox,
            render_target,
            resampler: Resampler::new(device)
                .context("failed to create managed-output resampler")?,
            clear_color,
            metrics: PublicationMetrics::new(),
            abandonment_fault: debug_abandon_after_acquisitions.map(AbandonmentFault::new),
        };
        publisher.publish_initial_clear(context)?;
        Ok(publisher)
    }

    /// Publish one transformed frame without ever waiting for the encoder.
    ///
    /// A busy consumer causes this frame to be dropped and counted. An
    /// abandoned mutex is fatal because partially submitted GPU work makes the
    /// resource generation untrustworthy.
    pub fn publish(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source_texture: &ID3D11Texture2D,
        source_size: Size2D<u32>,
        transform: FrameTransform) -> anyhow::Result<()> {
        let started = Instant::now();
        match self.mailbox.mutex.acquire(PRODUCER_KEY, 0)? {
            AcquireStatus::Timeout => {
                self.metrics.record_miss();
                return Ok(());
            }
            AcquireStatus::Abandoned => {
                return Err(ResourceGenerationLost::new(
                    "shared texture keyed mutex was abandoned by the encoder").into());
            }
            AcquireStatus::Acquired => {}
        }
        if self
            .abandonment_fault
            .as_mut()
            .is_some_and(AbandonmentFault::record_acquisition)
        {
            abandon_for_fault_injection();
        }

        let publication = (|| {
            anyhow::ensure!(
                source_size.width > 0 && source_size.height > 0,
                "captured frame dimensions must be non-zero");
            // SAFETY: The render target and context share the selected device.
            unsafe { context.ClearRenderTargetView(&self.render_target, &self.clear_color); }
            match transform {
                FrameTransform::Resample => {
                    let source_view = d3d11::create_srv_for_texture_2d(device, source_texture)
                        .context("failed to create managed-output source view")?;
                    let viewport = calculate_resample_viewport(source_size, self.mailbox.size);
                    // SAFETY: The viewport fits the fixed-size shared texture.
                    unsafe { context.RSSetViewports(Some(&[viewport])); }
                    self.resampler.resample(context, &source_view, &self.render_target);
                    // SAFETY: Clearing the viewport avoids leaking capture
                    // state into later D3D work on this immediate context.
                    unsafe { context.RSSetViewports(Some(&[])); }
                }
                FrameTransform::Crop(crop) => {
                    if let Some(source_box) = crop.clamped_d3d11_box(source_size) {
                        // SAFETY: Both textures use the selected device. The
                        // source box is clamped to the live WGC frame and fits
                        // within the crop-sized, padded mailbox.
                        unsafe {
                            context.CopySubresourceRegion(
                                &self.mailbox.texture,
                                0,
                                0,
                                0,
                                0,
                                source_texture,
                                0,
                                Some(&raw const source_box));
                        }
                    }
                }
            }
            // SAFETY: `Flush` submits the complete clear/draw or clear/copy
            // before ownership transfers to the consumer.
            // SAFETY: `context` is live and flushing has no pointer preconditions.
            unsafe { context.Flush(); }
            Ok::<(), anyhow::Error>(())
        })();

        // A failed draw returns the producer key so a partial frame is never
        // advertised. The caller still exits because the publication failed.
        let next_key = if publication.is_ok() { CONSUMER_KEY } else { PRODUCER_KEY };
        let release = self.mailbox.mutex.release(next_key);
        publication?;
        release?;
        self.metrics.record_publication(started.elapsed());
        Ok(())
    }

    /// Seed the mailbox before the first safe target is selected.
    fn publish_initial_clear(&self, context: &ID3D11DeviceContext) -> anyhow::Result<()> {
        match self.mailbox.mutex.acquire(PRODUCER_KEY, 0)? {
            AcquireStatus::Acquired => {}
            AcquireStatus::Timeout => anyhow::bail!("new shared texture did not expose producer key 0"),
            AcquireStatus::Abandoned => return Err(ResourceGenerationLost::new(
                "new shared texture mutex was already abandoned").into()),
        }
        // SAFETY: The render target belongs to `context`'s device; flushing
        // completes submission before the consumer key becomes visible.
        unsafe { context.ClearRenderTargetView(&self.render_target, &self.clear_color); }
        // SAFETY: `context` is live and flushing has no pointer preconditions.
        unsafe { context.Flush(); }
        self.mailbox.mutex.release(CONSUMER_KEY)
    }
}

/// One-shot counter used only by the explicit abandonment hardware proof.
struct AbandonmentFault {
    /// Successful producer acquisitions required before forced termination.
    after: u64,
    /// Successful producer acquisitions observed so far.
    acquisitions: u64,
}

impl AbandonmentFault {
    /// Start a fault counter validated as non-zero by the CLI.
    const fn new(after: u64) -> Self { Self { after, acquisitions: 0 } }

    /// Return `true` exactly when this acquisition should abandon the mutex.
    const fn record_acquisition(&mut self) -> bool {
        self.acquisitions += 1;
        self.acquisitions == self.after
    }
}

/// Terminate without releasing producer key 0 so the peer observes abandonment.
#[cold]
#[expect(clippy::exit, reason = "this explicit proof hook must abandon the owned keyed mutex")]
fn abandon_for_fault_injection() -> ! {
    eprintln!("fault injection: exiting while producer keyed mutex is owned");
    std::process::exit(86)
}

/// Five-second aggregate that avoids per-frame logging in the capture hot path.
struct PublicationMetrics {
    /// Beginning of the current reporting window.
    started: Instant,
    /// Successfully published complete frames.
    published: u64,
    /// Frames dropped because producer key 0 was busy.
    misses: u64,
    /// CPU time spent submitting successful shared-texture draws.
    submission_time: Duration,
}

impl PublicationMetrics {
    /// Start an empty reporting window.
    fn new() -> Self {
        Self {
            started: Instant::now(),
            published: 0,
            misses: 0,
            submission_time: Duration::ZERO,
        }
    }

    /// Count one successful publication and possibly emit the aggregate.
    fn record_publication(&mut self, submission_time: Duration) {
        self.published += 1;
        self.submission_time += submission_time;
        self.report_if_due();
    }

    /// Count one non-blocking miss and possibly emit the aggregate.
    fn record_miss(&mut self) {
        self.misses += 1;
        self.report_if_due();
    }

    /// Log rate, misses, and average CPU submission latency every five seconds.
    fn report_if_due(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed < METRICS_INTERVAL {
            return;
        }
        let average_us = if self.published == 0 {
            0.0
        } else {
            self.submission_time.as_secs_f64() * 1_000_000.0 / self.published as f64
        };
        log::info!(
            "shared output: {:.1} fps, {} acquisition misses, {:.1} us average submission",
            self.published as f64 / elapsed.as_secs_f64(),
            self.misses,
            average_us);
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandonment_fault_triggers_only_at_requested_acquisition() {
        let mut fault = AbandonmentFault::new(3);
        assert!(!fault.record_acquisition());
        assert!(!fault.record_acquisition());
        assert!(fault.record_acquisition());
        assert!(!fault.record_acquisition());
    }
}
