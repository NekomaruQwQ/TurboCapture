//! Texture-to-video pipeline owned by `live-encoder`.
//!
//! This module is the boundary between a BGRA frame producer and the encoded
//! stdout stream. The transitional CLI still produces its input texture from
//! Windows Graphics Capture in-process; Phase 3 can replace that producer with
//! a private copy of a supervisor-owned shared texture without changing NV12
//! conversion, NVENC configuration, AVCC serialization, or wire framing.

use crate::{
    NALUnit,
    NALUnitType,
    converter::NV12Converter,
    d3d11,
    encoder::{H264Encoder, H264EncoderConfig},
};

use live_protocol::{MessageType, flags, write_message};
use live_protocol::avcc::serialize_avcc_payload;
use live_protocol::video::{CodecParams, write_codec_params_payload, write_frame_payload};
use live_shared_texture::{
    AcquireStatus,
    CONSUMER_KEY,
    InheritedHandle,
    OpenedMailbox,
    PRODUCER_KEY,
    ResourceGenerationLost,
};

use nkcore::prelude::*;
use nkcore::prelude::euclid::Size2D;

use std::io::{BufWriter, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Com::*;

/// Codec bitrate retained from the legacy encoded-video producer.
pub const DEFAULT_BITRATE: u32 = 8_000_000;
/// Aggregation window for copy, conversion, and encoded-rate diagnostics.
const METRICS_INTERVAL: Duration = Duration::from_secs(5);
/// First-frame wait while the capture worker opens and seeds the mailbox.
const FIRST_FRAME_TIMEOUT_MS: u32 = 10_000;

/// Validated fixed-size BGRA texture consumed by the encoding pipeline.
///
/// The D3D11 objects are owned here so the texture and its device remain alive
/// for the entire worker lifetime. Phase 3 will construct this input only after
/// copying a shared publication texture into an encoder-private texture.
pub struct BgraTextureInput {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    source: BgraTextureSource,
    frame_size: Size2D<u32>,
}

/// Direct transitional input or cross-process latest-frame mailbox.
enum BgraTextureSource {
    /// Legacy capture and encoding threads share one in-process texture.
    Direct(ID3D11Texture2D),
    /// Managed mode copies the shared publication into a private texture.
    Shared(SharedBgraInput),
}

impl BgraTextureInput {
    /// Validate and retain a fixed-size BGRA encoder input.
    ///
    /// Returns an error when the descriptor would require an implicit format,
    /// dimension, mip, array, or multisample conversion. Device compatibility
    /// is enforced by D3D11 when the converter creates its input view.
    pub fn new(
        device: ID3D11Device,
        device_context: ID3D11DeviceContext,
        texture: ID3D11Texture2D,
        expected_size: Size2D<u32>)
        -> anyhow::Result<Self> {
        let mut descriptor = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` is a live COM object and `descriptor` is a valid
        // stack-local out parameter for the infallible D3D11 `GetDesc` call.
        unsafe { texture.GetDesc(&raw mut descriptor); }
        validate_bgra_texture_descriptor(&descriptor, expected_size)?;

        Ok(Self {
            device,
            device_context,
            source: BgraTextureSource::Direct(texture),
            frame_size: expected_size,
        })
    }

    /// Open a supervisor-owned mailbox and allocate the encoder-private copy.
    ///
    /// The inherited handle closes after `OpenSharedResource1`; the opened
    /// texture and keyed mutex retain independent COM references. The private
    /// texture uses the same validated fixed-size BGRA contract as transitional
    /// direct input.
    pub fn from_shared(
        device: ID3D11Device,
        device_context: ID3D11DeviceContext,
        handle: InheritedHandle,
        expected_size: Size2D<u32>) -> anyhow::Result<Self> {
        let mailbox = OpenedMailbox::open(&device, &handle, expected_size)?;
        drop(handle);
        let private_texture = d3d11::create_texture_2d(
            &device,
            expected_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            &[D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_RENDER_TARGET])
            .context("failed to create encoder-private BGRA texture")?;
        let mut descriptor = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `private_texture` is live and `descriptor` is a stack-local out value.
        unsafe { private_texture.GetDesc(&raw mut descriptor); }
        validate_bgra_texture_descriptor(&descriptor, expected_size)?;

        Ok(Self {
            device,
            device_context,
            source: BgraTextureSource::Shared(SharedBgraInput {
                mailbox,
                private_texture,
                has_frame: false,
                metrics: CopyMetrics::new(),
            }),
            frame_size: expected_size,
        })
    }

    /// Return the next private BGRA texture for conversion.
    fn next_texture(&mut self) -> anyhow::Result<ID3D11Texture2D> {
        match &mut self.source {
            &mut BgraTextureSource::Direct(ref texture) => Ok(texture.clone()),
            &mut BgraTextureSource::Shared(ref mut source) =>
                source.copy_latest(&self.device_context),
        }
    }
}

/// Managed shared input with ownership bounded to one `CopyResource` submission.
struct SharedBgraInput {
    /// Open supervisor-owned texture and two-key mutex.
    mailbox: OpenedMailbox,
    /// Encoder-only copy used after the producer key is released.
    private_texture: ID3D11Texture2D,
    /// Whether a timeout can safely reuse a previously copied complete frame.
    has_frame: bool,
    /// Aggregated acquisition and copy-submission measurements.
    metrics: CopyMetrics,
}

impl SharedBgraInput {
    /// Copy the latest complete publication or reuse the prior private frame.
    fn copy_latest(
        &mut self,
        context: &ID3D11DeviceContext) -> anyhow::Result<ID3D11Texture2D> {
        let timeout_ms = if self.has_frame { 0 } else { FIRST_FRAME_TIMEOUT_MS };
        match self.mailbox.mutex.acquire(CONSUMER_KEY, timeout_ms)? {
            AcquireStatus::Timeout if self.has_frame => {
                self.metrics.record_miss();
                return Ok(self.private_texture.clone());
            }
            AcquireStatus::Timeout => {
                anyhow::bail!(
                    "capture worker did not publish the first shared frame within {FIRST_FRAME_TIMEOUT_MS} ms");
            }
            AcquireStatus::Abandoned => {
                return Err(ResourceGenerationLost::new(
                    "shared texture keyed mutex was abandoned by the capture worker").into());
            }
            AcquireStatus::Acquired => {}
        }

        let started = Instant::now();
        // SAFETY: Both textures belong to `context`'s adapter and have identical
        // descriptors. `Flush` submits the private copy before key 0 is released.
        unsafe { context.CopyResource(&self.private_texture, &self.mailbox.texture); }
        // SAFETY: `context` is live and flushing has no pointer preconditions.
        unsafe { context.Flush(); }
        self.mailbox.mutex.release(PRODUCER_KEY)?;
        self.has_frame = true;
        self.metrics.record_copy(started.elapsed());
        Ok(self.private_texture.clone())
    }
}

/// Aggregated shared-copy measurements kept off individual frame logs.
struct CopyMetrics {
    /// Start of the current reporting interval.
    started: Instant,
    /// Successful shared-to-private copies.
    copied: u64,
    /// Consumer key misses that reused the previous private frame.
    misses: u64,
    /// CPU time spent submitting shared-to-private copies.
    submission_time: Duration,
}

impl CopyMetrics {
    /// Start an empty copy-metrics interval.
    fn new() -> Self {
        Self {
            started: Instant::now(),
            copied: 0,
            misses: 0,
            submission_time: Duration::ZERO,
        }
    }

    /// Record a successful copy submission.
    fn record_copy(&mut self, elapsed: Duration) {
        self.copied += 1;
        self.submission_time += elapsed;
        self.report_if_due();
    }

    /// Record a non-blocking consumer miss.
    fn record_miss(&mut self) {
        self.misses += 1;
        self.report_if_due();
    }

    /// Emit copy rate, misses, and average submission time every five seconds.
    fn report_if_due(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed < METRICS_INTERVAL {
            return;
        }
        let average_us = if self.copied == 0 {
            0.0
        } else {
            self.submission_time.as_secs_f64() * 1_000_000.0 / self.copied as f64
        };
        log::info!(
            "shared input: {:.1} copied fps, {} acquisition misses, {:.1} us average copy submission",
            self.copied as f64 / elapsed.as_secs_f64(),
            self.misses,
            average_us);
        *self = Self::new();
    }
}

/// Enforce the fixed, directly convertible portion of the encoder input contract.
///
/// Keeping descriptor checks independent from COM object construction makes the
/// safety boundary deterministic and testable before Phase 3 adds inherited
/// shared-resource handles.
fn validate_bgra_texture_descriptor(
    descriptor: &D3D11_TEXTURE2D_DESC,
    expected_size: Size2D<u32>)
    -> anyhow::Result<()> {
    anyhow::ensure!(
        descriptor.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
        "encoder input must use B8G8R8A8_UNORM (got {:?})",
        descriptor.Format);
    anyhow::ensure!(
        descriptor.Width == expected_size.width && descriptor.Height == expected_size.height,
        "encoder input dimensions must be {}x{} (got {}x{})",
        expected_size.width,
        expected_size.height,
        descriptor.Width,
        descriptor.Height);
    anyhow::ensure!(
        descriptor.MipLevels == 1 && descriptor.ArraySize == 1,
        "encoder input must be a single non-array mip (got {} mips, array size {})",
        descriptor.MipLevels,
        descriptor.ArraySize);
    anyhow::ensure!(
        descriptor.SampleDesc.Count == 1 && descriptor.SampleDesc.Quality == 0,
        "encoder input must not be multisampled");
    Ok(())
}

/// Settings that affect the encoded H.264 stream without describing its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoEncoderConfig {
    /// Constant output frame rate, in frames per second.
    pub frame_rate: u32,
    /// Constant target bitrate, in bits per second.
    pub bitrate: u32,
}

/// Spawn the texture-to-stdout encoder worker.
///
/// The worker initializes COM and Media Foundation on its own thread. Startup
/// and runtime failures finish the returned handle, allowing the transitional
/// capture loop—and later `live-stream`—to treat encoder exit as a component
/// failure. A closed stdout pipe retains the legacy clean-process-exit behavior.
pub fn spawn_stdout_encoder(
    input: BgraTextureInput,
    config: VideoEncoderConfig)
    -> anyhow::Result<thread::JoinHandle<anyhow::Result<()>>> {
    anyhow::ensure!(config.frame_rate > 0, "encoder frame rate must be non-zero");
    anyhow::ensure!(config.bitrate > 0, "encoder bitrate must be non-zero");

    thread::Builder::new()
        .name("encoding".to_owned())
        .spawn(move || {
            let mut input = input;
            run_stdout_encoder(&mut input, config)
        })
        .context("failed to spawn encoding thread")
}

/// Convert the validated BGRA input to NV12, encode it, and write framed AVCC.
#[expect(clippy::exit, reason = "a closed stdout pipe deliberately terminates this stdout-first producer")]
fn run_stdout_encoder(
    input: &mut BgraTextureInput,
    config: VideoEncoderConfig)
    -> anyhow::Result<()> {
    log::info!("encoding thread started");

    // SAFETY: This is the first COM call on the dedicated worker thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx failed on encoding thread")?;

    let nv12_converter = NV12Converter::new(
        &input.device,
        &input.device_context,
        input.frame_size.width,
        input.frame_size.height)
        .context("failed to create NV12 converter")?;
    let nv12_staging = d3d11::create_texture_2d(
        &input.device,
        input.frame_size,
        DXGI_FORMAT_NV12,
        &[D3D11_BIND_RENDER_TARGET])
        .context("failed to create NV12 staging texture")?;
    log::info!("NV12 converter and staging texture created");

    let stdout = std::io::stdout();
    let mut output = ProtocolWriter::new(BufWriter::new(stdout.lock()), input.frame_size);
    let encoder = H264Encoder::new(&input.device, H264EncoderConfig {
        frame_size: input.frame_size,
        frame_rate: config.frame_rate,
        bitrate: config.bitrate,
    }).context("failed to create H.264 encoder")?;

    let mut conversion_metrics = StageMetrics::new("BGRA to NV12 conversion");
    let mut output_metrics = FrameRateMetrics::new("encoded output");

    encoder.run(
        || {
            let bgra_texture = input.next_texture()?;
            let started = Instant::now();
            nv12_converter
                .convert(&bgra_texture, &nv12_staging)
                .context("BGRA8 to NV12 conversion failed")?;
            conversion_metrics.record(started.elapsed());
            Ok(nv12_staging.clone())
        },
        |nal_units| {
            output_metrics.record();
            let timestamp_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            if let Err(error) = output.write_access_unit(&nal_units, timestamp_us) {
                log::error!("failed to write encoded video: {error}");
                let _ = output.flush();
                // A closed downstream pipe means the stdout-first producer has
                // no consumer. Preserve the legacy clean exit used by launchers.
                std::process::exit(0);
            }
        })?;

    Ok(())
}

/// Average CPU submission time for one repeatedly invoked GPU stage.
struct StageMetrics {
    /// Human-readable stage name used in aggregate diagnostics.
    name: &'static str,
    /// Start of the current reporting interval.
    started: Instant,
    /// Number of invocations in this interval.
    count: u64,
    /// Accumulated CPU duration of those invocations.
    total: Duration,
}

impl StageMetrics {
    /// Start an empty interval for `name`.
    fn new(name: &'static str) -> Self {
        Self {
            name,
            started: Instant::now(),
            count: 0,
            total: Duration::ZERO,
        }
    }

    /// Record one invocation and report its aggregate when due.
    fn record(&mut self, duration: Duration) {
        self.count += 1;
        self.total += duration;
        let elapsed = self.started.elapsed();
        if elapsed < METRICS_INTERVAL {
            return;
        }
        let average_us = self.total.as_secs_f64() * 1_000_000.0 / self.count as f64;
        log::info!(
            "{}: {:.1} fps, {:.1} us average CPU submission",
            self.name,
            self.count as f64 / elapsed.as_secs_f64(),
            average_us);
        *self = Self::new(self.name);
    }
}

/// Output-rate counter separate from input timing to avoid shared closure state.
struct FrameRateMetrics {
    /// Human-readable counter name.
    name: &'static str,
    /// Start of the current reporting interval.
    started: Instant,
    /// Completed access units during this interval.
    count: u64,
}

impl FrameRateMetrics {
    /// Start an empty output-rate interval.
    fn new(name: &'static str) -> Self {
        Self { name, started: Instant::now(), count: 0 }
    }

    /// Count one access unit and report aggregate frames per second when due.
    fn record(&mut self) {
        self.count += 1;
        let elapsed = self.started.elapsed();
        if elapsed < METRICS_INTERVAL {
            return;
        }
        log::info!(
            "{}: {:.1} fps",
            self.name,
            self.count as f64 / elapsed.as_secs_f64());
        *self = Self::new(self.name);
    }
}

/// Stateful serializer for codec initialization and encoded access units.
///
/// SPS/PPS state is retained so unchanged codec parameters are not repeated,
/// preserving the legacy stdout stream byte-for-byte for equivalent NAL input.
struct ProtocolWriter<W> {
    writer: W,
    frame_size: Size2D<u32>,
    last_sps: Option<Vec<u8>>,
    last_pps: Option<Vec<u8>>,
}

impl<W: Write> ProtocolWriter<W> {
    /// Create a serializer for one fixed-resolution video stream.
    const fn new(writer: W, frame_size: Size2D<u32>) -> Self {
        Self {
            writer,
            frame_size,
            last_sps: None,
            last_pps: None,
        }
    }

    /// Write any changed codec parameters followed by one encoded frame.
    ///
    /// Empty MFT outputs are ignored because they do not represent an access
    /// unit. I/O errors are returned without partially updating SPS/PPS state,
    /// so a caller that can retry will resend initialization data.
    fn write_access_unit(&mut self, nal_units: &[NALUnit], timestamp_us: u64)
        -> std::io::Result<()> {
        if nal_units.is_empty() {
            return Ok(());
        }

        let sps = nal_units.iter().find(|unit| unit.unit_type == NALUnitType::SPS);
        let pps = nal_units.iter().find(|unit| unit.unit_type == NALUnitType::PPS);
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let sps_changed = self.last_sps.as_ref() != Some(&sps.data);
            let pps_changed = self.last_pps.as_ref() != Some(&pps.data);
            if sps_changed || pps_changed {
                let params = CodecParams {
                    sps: sps.data.clone(),
                    pps: pps.data.clone(),
                    width: self.frame_size.width,
                    height: self.frame_size.height,
                };
                let payload = write_codec_params_payload(&params);
                write_message(&mut self.writer, MessageType::CodecParams, 0, &payload)?;
                self.last_sps = Some(sps.data.clone());
                self.last_pps = Some(pps.data.clone());
                log::info!(
                    "sent CodecParams: {}x{}, SPS={}B, PPS={}B",
                    self.frame_size.width,
                    self.frame_size.height,
                    params.sps.len(),
                    params.pps.len());
            }
        }

        let is_keyframe = nal_units.iter().any(|unit| unit.unit_type == NALUnitType::IDR);
        let nal_data: Vec<&[u8]> = nal_units.iter().map(|unit| unit.data.as_slice()).collect();
        let avcc_payload = serialize_avcc_payload(&nal_data);
        let frame_payload = write_frame_payload(timestamp_us, &avcc_payload);
        let frame_flags = if is_keyframe { flags::IS_KEYFRAME } else { 0 };
        write_message(&mut self.writer, MessageType::Frame, frame_flags, &frame_payload)
    }

    /// Flush buffered output before a deliberate clean process exit.
    fn flush(&mut self) -> std::io::Result<()> { self.writer.flush() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use live_protocol::{read_message, video::{read_codec_params_payload, read_frame_payload}};
    use std::io::Cursor;

    /// Create one synthetic Annex B NAL unit for protocol-only tests.
    fn nal(unit_type: NALUnitType, header: u8, body: &[u8]) -> NALUnit {
        let mut data = vec![0, 0, 0, 1, header];
        data.extend_from_slice(body);
        NALUnit { unit_type, data }
    }

    /// Build the descriptor accepted by the fixed-size BGRA input contract.
    fn valid_bgra_descriptor() -> D3D11_TEXTURE2D_DESC {
        D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1200,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..D3D11_TEXTURE2D_DESC::default()
        }
    }

    #[test]
    fn accepts_fixed_size_bgra_descriptor() {
        validate_bgra_texture_descriptor(
            &valid_bgra_descriptor(),
            Size2D::new(1920, 1200))
            .unwrap();
    }

    #[test]
    fn rejects_non_bgra_descriptor() {
        let descriptor = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT_NV12,
            ..valid_bgra_descriptor()
        };
        let error = validate_bgra_texture_descriptor(
            &descriptor,
            Size2D::new(1920, 1200))
            .unwrap_err();
        assert!(error.to_string().contains("B8G8R8A8_UNORM"));
    }

    #[test]
    fn rejects_unexpected_dimensions() {
        let descriptor = D3D11_TEXTURE2D_DESC {
            Width: 1280,
            ..valid_bgra_descriptor()
        };
        let error = validate_bgra_texture_descriptor(
            &descriptor,
            Size2D::new(1920, 1200))
            .unwrap_err();
        assert!(error.to_string().contains("1280x1200"));
    }

    #[test]
    fn rejects_array_or_mip_chain() {
        for descriptor in [
            D3D11_TEXTURE2D_DESC { MipLevels: 2, ..valid_bgra_descriptor() },
            D3D11_TEXTURE2D_DESC { ArraySize: 2, ..valid_bgra_descriptor() },
        ] {
            let error = validate_bgra_texture_descriptor(
                &descriptor,
                Size2D::new(1920, 1200))
                .unwrap_err();
            assert!(error.to_string().contains("single non-array mip"));
        }
    }

    #[test]
    fn rejects_multisampled_descriptor() {
        let descriptor = D3D11_TEXTURE2D_DESC {
            SampleDesc: DXGI_SAMPLE_DESC { Count: 4, Quality: 0 },
            ..valid_bgra_descriptor()
        };
        let error = validate_bgra_texture_descriptor(
            &descriptor,
            Size2D::new(1920, 1200))
            .unwrap_err();
        assert!(error.to_string().contains("must not be multisampled"));
    }

    #[test]
    fn keyframe_preserves_codec_and_frame_wire_format() {
        let mut bytes = Vec::new();
        let mut output = ProtocolWriter::new(&mut bytes, Size2D::new(1920, 1200));
        let units = [
            nal(NALUnitType::SPS, 0x67, &[0x42, 0x00, 0x1F]),
            nal(NALUnitType::PPS, 0x68, &[0xCE, 0x38, 0x80]),
            nal(NALUnitType::IDR, 0x65, &[0x88, 0x84]),
        ];
        output.write_access_unit(&units, 16_667).unwrap();
        drop(output);

        let mut cursor = Cursor::new(bytes);
        let (codec_header, codec_payload) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(codec_header.message_type, MessageType::CodecParams as u8);
        let codec = read_codec_params_payload(&codec_payload).unwrap();
        assert_eq!((codec.width, codec.height), (1920, 1200));
        assert_eq!(codec.sps, units[0].data);
        assert_eq!(codec.pps, units[1].data);

        let (frame_header, frame_payload) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(frame_header.message_type, MessageType::Frame as u8);
        assert_eq!(frame_header.flags, flags::IS_KEYFRAME);
        let (timestamp_us, avcc) = read_frame_payload(&frame_payload).unwrap();
        assert_eq!(timestamp_us, 16_667);
        let expected_avcc = serialize_avcc_payload(
            &units
                .iter()
                .map(|unit| unit.data.as_slice())
                .collect::<Vec<_>>());
        assert_eq!(avcc, expected_avcc);
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn unchanged_codec_parameters_are_not_repeated() {
        let mut bytes = Vec::new();
        let mut output = ProtocolWriter::new(&mut bytes, Size2D::new(1920, 1200));
        let units = [
            nal(NALUnitType::SPS, 0x67, &[0x42, 0x00, 0x1F]),
            nal(NALUnitType::PPS, 0x68, &[0xCE, 0x38, 0x80]),
            nal(NALUnitType::IDR, 0x65, &[0x88]),
        ];
        output.write_access_unit(&units, 1).unwrap();
        output.write_access_unit(&units, 2).unwrap();
        drop(output);

        let mut cursor = Cursor::new(bytes);
        let mut message_types = Vec::new();
        while let Some((header, _)) = read_message(&mut cursor).unwrap() {
            message_types.push(header.message_type);
        }
        assert_eq!(message_types, [
            MessageType::CodecParams as u8,
            MessageType::Frame as u8,
            MessageType::Frame as u8,
        ]);
    }

    #[test]
    fn non_idr_access_unit_is_not_marked_as_keyframe() {
        let mut bytes = Vec::new();
        let mut output = ProtocolWriter::new(&mut bytes, Size2D::new(1920, 1200));
        output.write_access_unit(&[nal(NALUnitType::NonIDR, 0x61, &[0xAA])], 3).unwrap();
        drop(output);

        let (header, _) = read_message(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::Frame as u8);
        assert_eq!(header.flags, 0);
    }
}
