//! H.264 hardware encoder using an asynchronous Media Foundation Transform (MFT).
//!
//! The encoder runs an infinite event loop, pulling frames via a caller-provided
//! closure and pushing encoded NAL units through another closure.  This design
//! keeps the encoder reusable — the binary wires the closures to the staging
//! texture and stdout, while tests can wire them to buffers.
//!
//! ## Initialization order (critical — Bug #1 in README)
//!
//! 1. Output media type (H.264, resolution, frame rate, bitrate, profile)
//! 2. Input media type (NV12, resolution, frame rate)
//! 3. D3D manager (attach GPU device)
//! 4. Codec API values (B-frames, GOP, latency mode, rate control)
//! 5. Start streaming

#![expect(
    non_upper_case_globals,
    reason = "false positive on windows-rs constants")]

mod debug;
mod helper;

use crate::{NALUnit, NALUnitType};

use nkcore::prelude::*;
use nkcore::debug::*;
use nkcore::*;

use std::time::*;
use euclid::*;

use windows::core::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;

pub struct H264Encoder {
    _mf_dxgi_manager: IMFDXGIDeviceManager,

    mf_transform: IMFTransform,
    mf_event_generator: IMFMediaEventGenerator,
    frame_rate: u32,
    frame_count: u64,
    time_elapsed: Duration,
    time_of_last_frame: SystemTime,
}

/// Construction parameters for the encoder.
///
/// Does not include runtime callbacks — those are passed to [`H264Encoder::run`]
/// so the compiler can monomorphize them instead of boxing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct H264EncoderConfig {
    pub frame_size: Size2D<u32>,
    pub frame_rate: u32,
    pub bitrate: u32,
}

impl H264Encoder {
    /// Create and initialize the hardware H.264 encoder.
    ///
    /// # Panics
    /// Panics if `frame_size` width or height is not a multiple of 16 (NVENC requirement).
    #[expect(clippy::panic_in_result_fn, reason = "asserts for valid input")]
    pub fn new(device: &ID3D11Device, config: H264EncoderConfig) -> anyhow::Result<Self> {
        assert!(
            config.frame_size.width.is_multiple_of(16) &&
            config.frame_size.height.is_multiple_of(16),
            "frame size must be multiple of 16");
        log::info!(
            "creating H.264 encoder with config: {}x{} @ {}fps, {} bps",
            config.frame_size.width,
            config.frame_size.height,
            config.frame_rate,
            config.bitrate);

        // Initialize Media Foundation
        // SAFETY: Called once during encoder construction, before any other MF calls.
        api_call!(unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) })?;

        // Find H.264 encoder transform
        let mf_transform =
            helper::find_h264_encoder(&api_call!(device.cast())?)
                .context("failed to find H264Encoder")?;
        // SAFETY: `mf_transform` is a valid MFT obtained from `find_h264_encoder`.
        let mf_attributes =
            api_call!(unsafe { mf_transform.GetAttributes() })?;

        // Get event generator interface for async MFT event handling
        let mf_event_generator =
            api_call!(mf_transform.cast::<IMFMediaEventGenerator>())
                .context("failed to get IMFMediaEventGenerator interface")?;

        // Unlock async MFT
        // SAFETY: `mf_attributes` is a valid IMFAttributes from the MFT above.
        api_call!(unsafe { mf_attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) })
            .context("failed to unlock async MFT")?;
        log::info!("MFT async unlocked");

        // Print supported media types for debugging (after unlock)
        debug::print_mft_supported_output_types(&mf_transform);

        // Configure output type (H.264) BEFORE setting D3D manager
        // SAFETY: `mf_transform` is a valid, async-unlocked MFT. Stream index 0, type index 0.
        let output_type = unsafe { mf_transform.GetOutputAvailableType(0, 0)? };

        // SAFETY: `output_type` is a valid IMFMediaType from the MFT.
        api_call!(unsafe { output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate) })?;

        let frame_size_val =
            ((config.frame_size.width as u64) << 32) |
            (config.frame_size.height as u64);
        // SAFETY: `output_type` is a valid IMFMediaType; packed u64 encodes width/height.
        api_call!(unsafe { output_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size_val) })?;

        let frame_rate_ratio = ((config.frame_rate as u64) << 32) | 1u64;
        // SAFETY: `output_type` is a valid IMFMediaType; packed u64 encodes rate/denominator.
        api_call!(unsafe { output_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate_ratio) })?;

        // Baseline profile: no B-frames, maximum WebCodecs compatibility
        // SAFETY: `output_type` is a valid IMFMediaType.
        api_call!(unsafe {
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
        })?;
        log::info!("H.264 profile set to Baseline (no B-frames)");

        log::info!("Setting output type (H.264)...");
        // SAFETY: `mf_transform` and `output_type` are valid; output type is fully configured.
        api_call!(unsafe { mf_transform.SetOutputType(0, &output_type, 0) })
            .context("failed to set output type")?;
        log::info!("Output type set successfully!");

        // Print supported input types after output type is set
        debug::print_mft_supported_input_types(&mf_transform);

        // Configure input type (NV12) — use enumerated type #1
        // SAFETY: `mf_transform` is valid; stream index 0, type index 1 (NV12).
        let input_type = unsafe { mf_transform.GetInputAvailableType(0, 1)? };

        log::info!("Setting input type (NV12)...");
        // SAFETY: `mf_transform` and `input_type` are valid COM objects.
        api_call!(unsafe { mf_transform.SetInputType(0, &input_type, 0) })
            .context("failed to set input type")?;
        log::info!("Input type set successfully!");

        // Create DXGI Device Manager AFTER types are configured
        let mut reset_token = 0;
        // SAFETY: `reset_token` is a stack-local `u32`; `out` receives the manager.
        let mf_dxgi_manager =
            out_var_or_err(|out| api_call!(unsafe {
                MFCreateDXGIDeviceManager(&raw mut reset_token, out)
            }))?
                .ok_or_else(|| anyhow::anyhow!("DXGI device manager is null"))?;
        let mf_dxgi_manager_as_unknown: IUnknown =
            api_call!(mf_dxgi_manager.cast::<IUnknown>())?;

        // Register D3D11 device with DXGI manager
        // SAFETY: `mf_dxgi_manager` is valid; `device` is a live D3D11 device;
        // `reset_token` is the token returned by `MFCreateDXGIDeviceManager`.
        api_call!(unsafe { mf_dxgi_manager.ResetDevice(device, reset_token) })
            .context("failed to register D3D11 device with DXGI manager")?;

        // Set D3D manager after both types are configured
        // SAFETY: `mf_transform` is valid; the DXGI manager pointer is passed as a
        // `usize` per the MFT_MESSAGE_SET_D3D_MANAGER contract.
        api_call!(unsafe {
            mf_transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                mf_dxgi_manager_as_unknown.as_raw().addr())
        }).context("failed to set D3D manager")?;
        log::info!("DXGI device manager attached to encoder");

        // Configure low-latency settings via ICodecAPI (after types and D3D manager)
        log::info!("Configuring low-latency codec settings...");
        let codec_api = api_call!(mf_transform.cast::<ICodecAPI>())?;

        for (name, api, value) in [
            // No B-frames for low latency (B-frames add 2+ frame latency)
            ("B-frame count", &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0)),
            // All-IDR at very low frame rates (≤5fps): keyframe wait is eliminated
            // and the bitrate cost is negligible for small/static content.
            // At higher rates, GOP = 2 seconds for fast recovery from packet loss.
            ("GOP size", &CODECAPI_AVEncMPVGOPSize, VARIANT::from(
                if config.frame_rate <= 5 { 1 } else { config.frame_rate * 2 })),
            // Low latency mode
            ("Low latency mode", &CODECAPI_AVLowLatencyMode, VARIANT::from(true)),
            // CBR rate control (constant bitrate for predictable latency)
            ("Rate control mode", &CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32)),
        ] {
            // SAFETY: `codec_api` is a valid ICodecAPI cast from the MFT; `api` is a
            // static GUID reference; `value` is a stack-local VARIANT with correct type.
            match api_call!(unsafe { codec_api.SetValue(api, &raw const value) }) {
                Ok(()) => log::info!("  {name} set successfully"),
                Err(e) => log::warn!("  Failed to set {name} (encoder may not support this setting): {e:?}"),
            }
        }

        // Start streaming
        // SAFETY: `mf_transform` is fully configured (types + D3D manager + codec API).
        api_call!(unsafe {
            mf_transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        })?;
        // SAFETY: Same as above; notifies the MFT that the first sample is imminent.
        api_call!(unsafe {
            mf_transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        })?;

        log::info!("H.264 encoder ready");

        Ok(Self {
            _mf_dxgi_manager: mf_dxgi_manager,
            mf_transform,
            mf_event_generator,
            frame_rate: config.frame_rate,
            frame_count: 0,
            time_elapsed: Duration::ZERO,
            time_of_last_frame: SystemTime::now(),
        })
    }

    /// Run the encoder event loop (blocks forever).
    ///
    /// - `frame_source`: called on `METransformNeedInput` — must return an NV12 texture.
    /// - `frame_target`: called on `METransformHaveOutput` — receives the parsed NAL units.
    ///
    /// Input acquisition failures terminate the loop so an abandoned shared
    /// texture can be reported to the process supervisor instead of panicking.
    pub fn run<
        Src: FnMut() -> anyhow::Result<ID3D11Texture2D>,
        Dst: FnMut(Vec<NALUnit>)>(
        mut self,
        mut frame_source: Src,
        mut frame_target: Dst) -> anyhow::Result<()> {
        loop {
            // Blocking wait for the next async MFT event.
            // SAFETY: `mf_event_generator` is a valid IMFMediaEventGenerator from
            // the MFT; `default()` means synchronous (blocking) wait.
            let event = match unsafe { self.mf_event_generator.GetEvent(default()) } {
                Ok(event) => event,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE =>
                    continue,
                Err(err) => {
                    log::warn!("failed to poll encoder event: {err:?}");
                    continue;
                }
            };

            // SAFETY: `event` is a valid IMFMediaEvent obtained from `GetEvent` above.
            let event_type = match api_call!(unsafe { event.GetType() }) {
                Ok(event_type) => MF_EVENT_TYPE(event_type as _),
                Err(err) => {
                    log::warn!("failed to poll encoder event: {err:?}");
                    continue;
                }
            };

            match event_type {
                METransformDrainComplete => {
                    log::trace!("METransformDrainComplete received");
                },
                METransformNeedInput => {
                    log::trace!("METransformNeedInput received");
                    self.process_input(&mut frame_source)
                        .context("H264Encoder::process_input failed")?;
                },
                METransformHaveOutput => {
                    log::trace!("METransformHaveOutput received");
                    self.process_output(&mut frame_target)
                        .context("H264Encoder::process_output failed")?;
                }
                _ => {
                    log::trace!("Unhandled MFT event type: {event_type:?}");
                }
            }
        }
    }

    fn process_input(
        &mut self,
        frame_source: &mut impl FnMut() -> anyhow::Result<ID3D11Texture2D>)
        -> anyhow::Result<()> {
        // Throttle to target frame rate
        while SystemTime::now()
            .duration_since(self.time_of_last_frame)
            .context("unexpected time drift")?
            .as_secs_f64() < (1.0f64 / self.frame_rate as f64) {
            std::thread::sleep(Duration::from_millis(1));
        }

        let now = SystemTime::now();
        let elapsed =
            now
                .duration_since(self.time_of_last_frame)
                .context("unexpected time drift")?;
        self.frame_count += 1;
        self.time_elapsed += elapsed;
        self.time_of_last_frame = now;
        log::debug!("encoding frame #{} at {}s {}ms",
            self.frame_count,
            self.time_elapsed.as_secs(),
            self.time_elapsed.subsec_millis());

        // Create DXGI surface buffer from the caller's NV12 texture
        log::trace!("feeding frame to encoder...");
        let frame_texture = frame_source()?;
        // SAFETY: `frame_texture` is a valid NV12 `ID3D11Texture2D` from the caller.
        // The IID identifies the texture interface; subresource 0, not bottom-up.
        let buffer = api_call!(unsafe {
            MFCreateDXGISurfaceBuffer(
                &ID3D11Texture2D::IID,
                &frame_texture,
                0,
                false)
        })?;

        // SAFETY: Creates an empty MF sample (no preconditions).
        let sample = api_call!(unsafe { MFCreateSample() })?;
        // SAFETY: `sample` and `buffer` are valid COM objects created above.
        api_call!(unsafe { sample.AddBuffer(&buffer) })?;

        // Set sample time (convert us to 100ns units)
        let timestamp_us = self.time_elapsed.as_micros() as u64;
        // SAFETY: `sample` is a valid IMFSample with one buffer attached.
        api_call!(unsafe { sample.SetSampleTime((timestamp_us * 10) as i64) })?;

        // Set sample duration
        let duration_100ns = elapsed.as_micros() as i64 * 10;
        // SAFETY: `sample` is a valid IMFSample.
        api_call!(unsafe { sample.SetSampleDuration(duration_100ns) })?;

        // SAFETY: `mf_transform` is a streaming MFT; `sample` is fully configured
        // with a DXGI surface buffer, timestamp, and duration.
        match unsafe { self.mf_transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {
                log::trace!("feeding succeeded");
            },
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                Err(e).context("encoder not accepting input \u{2014} should not happen after METransformNeedInput")?;
            },
            Err(e) => {
                Err(e).context("failed to process input sample")?;
            }
        }

        Ok(())
    }

    #[expect(clippy::try_err, reason = "project style requires Err(error)? for explicit propagation")]
    fn process_output(
        &mut self,
        frame_target: &mut impl FnMut(Vec<NALUnit>))
        -> anyhow::Result<()> {
        let mut output_buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
        let mut status = 0;

        // SAFETY: `mf_transform` is a streaming MFT; `output_buffers` is a stack-local
        // array of one default-initialized `MFT_OUTPUT_DATA_BUFFER`. The MFT fills in
        // `pSample` on success. `status` receives output status flags.
        match unsafe {
            self.mf_transform.ProcessOutput(
                0,
                &mut output_buffers,
                &raw mut status)
        } {
            Ok(()) => {
                if let Some(sample) = output_buffers[0].pSample.take() {
                    // SAFETY: `sample` is a valid IMFSample produced by `ProcessOutput`.
                    let buffer = api_call!(unsafe { sample.ConvertToContiguousBuffer() })?;
                    let nal_units = Self::parse_nal_units_from_buffer(&buffer)?;

                    if !nal_units.is_empty() {
                        log::debug!("encoded {} NAL unit(s):", nal_units.len());
                        for (i, unit) in nal_units.iter().enumerate() {
                            log::debug!("  NAL #{}: type={:?}, size={} bytes",
                                i, unit.unit_type, unit.data.len());
                        }
                    }

                    frame_target(nal_units);
                }
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                anyhow::bail!("unexpected MF_E_TRANSFORM_NEED_MORE_INPUT during ProcessOutput");
            },
            Err(e) => {
                Err(e)?;
            },
        }

        Ok(())
    }

    /// Parse NAL units from encoder output buffer using Annex B start code scanning.
    fn parse_nal_units_from_buffer(buffer: &IMFMediaBuffer)
        -> anyhow::Result<Vec<NALUnit>> {
        let buffer_lock = BufferLock::lock(buffer)?;
        let data = buffer_lock.as_slice();

        let mut nal_units = Vec::new();
        let mut i = 0;

        while i < data.len() {
            // Look for start code (00 00 00 01 or 00 00 01)
            let start_code_len = if i + 3 < data.len()
                && data[i] == 0x00
                && data[i + 1] == 0x00
                && data[i + 2] == 0x00
                && data[i + 3] == 0x01 {
                4
            } else if i + 2 < data.len()
                && data[i] == 0x00
                && data[i + 1] == 0x00
                && data[i + 2] == 0x01 {
                3
            } else {
                i += 1;
                continue;
            };

            // Parse NAL unit header
            let nal_header_pos = i + start_code_len;
            if nal_header_pos >= data.len() {
                break;
            }

            let nal_header = data[nal_header_pos];
            let nal_type = NALUnitType::from_header(nal_header);

            // Find next start code
            let mut next_start = data.len();
            for j in (nal_header_pos + 1)..data.len() {
                if j + 3 < data.len()
                    && data[j] == 0x00
                    && data[j + 1] == 0x00
                    && data[j + 2] == 0x00
                    && data[j + 3] == 0x01 {
                    next_start = j;
                    break;
                }
                if j + 2 < data.len()
                    && data[j] == 0x00
                    && data[j + 1] == 0x00
                    && data[j + 2] == 0x01 {
                    next_start = j;
                    break;
                }
            }

            if let Some(unit_type) = nal_type {
                let data = data[i..next_start].to_vec();
                nal_units.push(NALUnit {
                    unit_type,
                    data,
                });
            }

            i = next_start;
        }

        Ok(nal_units)
    }
}

/// RAII guard for `IMFMediaBuffer::Lock` / `Unlock`.
struct BufferLock<'a> {
    mf_buffer: &'a IMFMediaBuffer,
    ptr: *mut u8,
    len: usize,
}

impl<'a> BufferLock<'a> {
    fn lock(buffer: &'a IMFMediaBuffer) -> anyhow::Result<Self> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;

        // SAFETY: `buffer` is a valid IMFMediaBuffer. `ptr` and `len` are stack-local
        // out-params. After Lock succeeds, `ptr` points to a contiguous byte array of
        // `len` bytes owned by the buffer, valid until `Unlock`.
        api_call!(unsafe {
            buffer.Lock(
                &raw mut ptr,
                None,
                Some(&raw mut len))
        })?;

        Ok(Self { mf_buffer: buffer, ptr, len: len as _ })
    }

    const fn as_slice(&self) -> &[u8] {
        // SAFETY: `self.ptr` is non-null and points to `self.len` contiguous bytes,
        // guaranteed by a successful `IMFMediaBuffer::Lock` in `BufferLock::lock`.
        // The buffer remains locked (and thus the pointer valid) for the lifetime `'a`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.mf_buffer` is a valid locked buffer (locked in `BufferLock::lock`).
        // Unlock is the required counterpart; errors are ignored because we're in Drop.
        unsafe {
            // Ignore error — we're in Drop, can't propagate
            let _ = self.mf_buffer.Unlock();
        }
    }
}
