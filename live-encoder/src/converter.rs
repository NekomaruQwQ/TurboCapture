//! GPU-accelerated BGRA → NV12 format conversion via `ID3D11VideoProcessor`.
//!
//! Hardware H.264 encoders require NV12 input.  This module wraps the D3D11
//! video processor pipeline to perform the color space conversion entirely on
//! the GPU (~0.5–1 ms for 1920x1200).

use nkcore::prelude::*;
use nkcore::debug::*;

use windows::core::Interface as _;
use windows::Win32::Graphics::{
    Dxgi::Common::*,
    Direct3D11::*,
};

/// Stateless format converter: BGRA textures → NV12 textures via GPU video processor.
pub struct NV12Converter {
    device: ID3D11VideoDevice,
    device_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
}

impl NV12Converter {
    /// Create a new format converter for the given resolution.
    ///
    /// Both the input (BGRA) and output (NV12) textures are expected to be
    /// this size.  The video processor is configured once at creation —
    /// changing resolution requires a new converter.
    pub fn new(
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        width: u32,
        height: u32)
        -> anyhow::Result<Self> {
        let device =
            api_call!(device.cast::<ID3D11VideoDevice>())?;
        let device_context =
            api_call!(device_context.cast::<ID3D11VideoContext>())?;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL::default(),
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL::default(),
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        // SAFETY: `device` is a valid `ID3D11VideoDevice` (cast above); `desc` is
        // a fully initialized stack-local struct describing the video processor caps.
        let enumerator = api_call!(unsafe {
            device.CreateVideoProcessorEnumerator(&raw const desc)
        })?;

        // SAFETY: `device` and `enumerator` are valid COM objects from the lines above.
        let processor = api_call!(unsafe {
            device.CreateVideoProcessor(
                &enumerator,
                /* RateConversionIndex (0 = no rate conversion): */ 0)
        })?;

        Ok(Self {
            device,
            device_context,
            processor,
            enumerator,
        })
    }

    /// Convert a BGRA texture to NV12.
    ///
    /// # Arguments
    /// * `bgra_texture` — input texture ([`DXGI_FORMAT_B8G8R8A8_UNORM`])
    /// * `nv12_texture` — output texture ([`DXGI_FORMAT_NV12`]), must be pre-allocated
    pub fn convert(
        &self,
        bgra_texture: &ID3D11Texture2D,
        nv12_texture: &ID3D11Texture2D)
        -> anyhow::Result<()> {
        let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };

        let mut input_view = None;
        // SAFETY: All COM objects (`device`, `enumerator`, `bgra_texture`) are valid.
        // `input_view_desc` is a stack-local struct; `input_view` receives the result.
        api_call!(unsafe {
            self.device.CreateVideoProcessorInputView(
                bgra_texture,
                &self.enumerator,
                &raw const input_view_desc,
                Some(&raw mut input_view))
        })?;
        let input_view = input_view.ok_or_else(|| anyhow::anyhow!("input view is null"))?;

        let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV {
                    MipSlice: 0,
                },
            },
        };

        let mut output_view = None;
        // SAFETY: All COM objects (`device`, `enumerator`, `nv12_texture`) are valid.
        // `output_view_desc` is a stack-local struct; `output_view` receives the result.
        api_call!(unsafe {
            self.device.CreateVideoProcessorOutputView(
                nv12_texture,
                &self.enumerator,
                &raw const output_view_desc,
                Some(&raw mut output_view))
        })?;
        let output_view = output_view.ok_or_else(|| anyhow::anyhow!("output view is null"))?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
            ..default()
        };

        // SAFETY: `processor`, `output_view`, and `stream` (containing the input view)
        // are all valid COM objects created from the same D3D11 device in this call.
        api_call!(unsafe {
            self.device_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0, // OutputFrame
                &[stream])
        }).context("failed to perform BGRA -> NV12 conversion")?;

        Ok(())
    }
}
