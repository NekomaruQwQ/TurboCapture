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

use nkcore::prelude::*;
use nkcore::prelude::euclid::Size2D;

use std::io::{BufWriter, Write};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Com::*;

/// Codec bitrate retained from the legacy encoded-video producer.
pub const DEFAULT_BITRATE: u32 = 8_000_000;

/// Validated fixed-size BGRA texture consumed by the encoding pipeline.
///
/// The D3D11 objects are owned here so the texture and its device remain alive
/// for the entire worker lifetime. Phase 3 will construct this input only after
/// copying a shared publication texture into an encoder-private texture.
pub struct BgraTextureInput {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    frame_size: Size2D<u32>,
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
            texture,
            frame_size: expected_size,
        })
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
        .spawn(move || run_stdout_encoder(&input, config))
        .context("failed to spawn encoding thread")
}

/// Convert the validated BGRA input to NV12, encode it, and write framed AVCC.
#[expect(clippy::exit, reason = "a closed stdout pipe deliberately terminates this stdout-first producer")]
fn run_stdout_encoder(
    input: &BgraTextureInput,
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

    encoder.run(
        || {
            nv12_converter
                .convert(&input.texture, &nv12_staging)
                .expect("BGRA8 to NV12 conversion failed");
            nv12_staging.clone()
        },
        |nal_units| {
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
        });

    Ok(())
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
