//! Fixed-output crop/resample and pooled GPU BGRA-to-NV12 conversion.

use std::collections::VecDeque;

use anyhow::{Context as _, ensure};
use capture_core::CropRect;
use euclid::default::Size2D;
use windows::{
    Win32::Graphics::{
        Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D},
        Direct3D11::*,
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_FORMAT_NV12,
            DXGI_SAMPLE_DESC,
        },
    },
    core::Interface as _,
};

/// Fixed letterbox color matching the private viewer's neutral background.
const CLEAR_COLOR: [f32; 4] = [41.0 / 255.0, 41.0 / 255.0, 41.0 / 255.0, 1.0];
/// Three surfaces allow an async encoder to retain inputs without aliasing.
const SURFACE_COUNT: usize = 3;

/// View gamma-encoded WGC bytes without applying the sRGB sampling transform.
///
/// WGC exposes display-encoded bytes in a UNORM texture. Sampling the wrapper's
/// sRGB staging view would decode those bytes to linear light, while the fixed
/// UNORM render target stores shader output verbatim for Media Foundation.
/// Keeping this view UNORM therefore preserves the screen's encoded RGB values.
pub fn create_gamma_encoded_source_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D) -> anyhow::Result<ID3D11ShaderResourceView> {
    let description = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
        },
    };
    let mut view = None;
    // SAFETY: WGC guarantees a single-mip BGRA UNORM texture on `device`.
    unsafe {
        device.CreateShaderResourceView(
            texture,
            Some(&raw const description),
            Some(&raw mut view))
    }.context("failed to create gamma-encoded WGC source view")?;
    view.context("D3D11 returned a null WGC source view")
}

/// GPU-owned fixed BGRA image whose media type survives target switches.
pub struct FixedFrame {
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    render_target: ID3D11RenderTargetView,
    resampler: Resampler,
    size: Size2D<u32>,
    revision: u64,
    is_clear: bool,
}

impl FixedFrame {
    /// Allocate the immutable output surface and seed it with clear content.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        size: Size2D<u32>) -> anyhow::Result<Self> {
        ensure!(size.width > 0 && size.height > 0, "fixed frame dimensions must be non-zero");
        let texture = create_texture(
            device,
            size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32)
            .context("failed to allocate fixed BGRA frame")?;
        let mut render_target = None;
        // SAFETY: `texture` belongs to `device`; a default view covers its sole subresource.
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&raw mut render_target)) }
            .context("failed to create fixed-frame render target")?;
        let render_target = render_target.context("D3D11 returned a null render target")?;
        let resampler = Resampler::new(device)?;
        let mut frame = Self {
            context: context.clone(),
            texture,
            render_target,
            resampler,
            size,
            revision: 0,
            is_clear: false,
        };
        frame.clear()?;
        Ok(frame)
    }

    /// Clear stale target pixels once per waiting/switch transition.
    pub fn clear(&mut self) -> anyhow::Result<()> {
        if self.is_clear {
            return Ok(());
        }
        // SAFETY: The render target and immediate context share one live device.
        unsafe { self.context.ClearRenderTargetView(&self.render_target, &CLEAR_COLOR); }
        self.revision = self.revision.checked_add(1).context("fixed-frame revision exhausted")?;
        self.is_clear = true;
        Ok(())
    }

    /// Crop and aspect-fit the newest captured texture into the fixed output.
    ///
    /// An out-of-bounds crop produces a clear frame instead of submitting an
    /// invalid D3D region. The caller may then keep the process alive while the
    /// target is concurrently resized.
    #[expect(clippy::multiple_unsafe_ops_per_block, reason = "one clear and viewport state transaction")]
    pub fn update(
        &mut self,
        source_view: &ID3D11ShaderResourceView,
        source_size: Size2D<u32>,
        crop: Option<CropRect>) -> anyhow::Result<bool> {
        let Some(region) = SourceRegion::clamped(source_size, crop) else {
            self.is_clear = false;
            self.clear()?;
            return Ok(false);
        };
        let viewport = calculate_viewport(region.size(), self.size);
        // Clearing before every draw prevents pixels from a prior aspect ratio
        // leaking through newly exposed letterbox space.
        // SAFETY: All state and views share the same device; `region` is clamped
        // to a non-empty source texture and the viewport fits the fixed target.
        unsafe {
            self.context.ClearRenderTargetView(&self.render_target, &CLEAR_COLOR);
            self.context.RSSetViewports(Some(&[viewport]));
        }
        self.resampler.draw(
            &self.context,
            source_view,
            &self.render_target,
            region.normalized(source_size));
        self.revision = self.revision.checked_add(1).context("fixed-frame revision exhausted")?;
        self.is_clear = false;
        Ok(true)
    }

    /// Return the output texture for fixed-resolution video conversion.
    #[inline]
    pub const fn texture(&self) -> &ID3D11Texture2D { &self.texture }

    /// Return the content revision used to avoid redundant conversion on reuse.
    #[inline]
    pub const fn revision(&self) -> u64 { self.revision }

    /// Return the fixed encoded dimensions.
    #[inline]
    pub const fn size(&self) -> Size2D<u32> { self.size }

}

/// One clamped non-empty source rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceRegion {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

impl SourceRegion {
    /// Clamp optional configuration to a live source surface.
    fn clamped(source: Size2D<u32>, crop: Option<CropRect>) -> Option<Self> {
        let crop = crop.unwrap_or(CropRect {
            min_x: 0,
            min_y: 0,
            max_x: source.width,
            max_y: source.height,
        });
        let region = Self {
            min_x: crop.min_x.min(source.width),
            min_y: crop.min_y.min(source.height),
            max_x: crop.max_x.min(source.width),
            max_y: crop.max_y.min(source.height),
        };
        (region.max_x > region.min_x && region.max_y > region.min_y).then_some(region)
    }

    /// Return integer source dimensions for aspect-fit viewport calculation.
    const fn size(self) -> Size2D<u32> {
        Size2D::new(self.max_x - self.min_x, self.max_y - self.min_y)
    }

    /// Map clamped pixel edges into normalized texture coordinates.
    fn normalized(self, source: Size2D<u32>) -> [f32; 4] {
        [
            self.min_x as f32 / source.width as f32,
            self.min_y as f32 / source.height as f32,
            (self.max_x - self.min_x) as f32 / source.width as f32,
            (self.max_y - self.min_y) as f32 / source.height as f32,
        ]
    }
}

/// Compute a centered aspect-fit viewport with deterministic integer pixels.
fn calculate_viewport(source: Size2D<u32>, target: Size2D<u32>) -> D3D11_VIEWPORT {
    let scale = f32::min(
        target.width as f32 / source.width as f32,
        target.height as f32 / source.height as f32);
    let width = (source.width as f32 * scale).floor();
    let height = (source.height as f32 * scale).floor();
    D3D11_VIEWPORT {
        TopLeftX: ((target.width as f32 - width) / 2.0).floor(),
        TopLeftY: ((target.height as f32 - height) / 2.0).floor(),
        Width: width,
        Height: height,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

/// Immutable shader state plus one dynamic four-float crop constant.
struct Resampler {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    source_region: ID3D11Buffer,
}

impl Resampler {
    /// Compiled vertex shader produced by the repository shader recipe.
    const VERTEX_BYTECODE: &'static [u8] =
        include_bytes!("../generated/fixed_frame_vs_main.fxo");
    /// Compiled pixel shader produced by the repository shader recipe.
    const PIXEL_BYTECODE: &'static [u8] =
        include_bytes!("../generated/fixed_frame_ps_main.fxo");

    /// Create immutable pipeline objects on the fixed-frame device.
    fn new(device: &ID3D11Device) -> anyhow::Result<Self> {
        let mut vertex_shader = None;
        // SAFETY: Bytecode comes from the repository's fixed-frame HLSL source.
        unsafe { device.CreateVertexShader(Self::VERTEX_BYTECODE, None, Some(&raw mut vertex_shader)) }
            .context("failed to create fixed-frame vertex shader")?;
        let vertex_shader = vertex_shader.context("D3D11 returned a null vertex shader")?;

        let mut pixel_shader = None;
        // SAFETY: Bytecode comes from the repository's fixed-frame HLSL source.
        unsafe { device.CreatePixelShader(Self::PIXEL_BYTECODE, None, Some(&raw mut pixel_shader)) }
            .context("failed to create fixed-frame pixel shader")?;
        let pixel_shader = pixel_shader.context("D3D11 returned a null pixel shader")?;

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: D3D11_FLOAT32_MAX,
        };
        let mut sampler = None;
        // SAFETY: The descriptor contains supported linear-clamp values.
        unsafe { device.CreateSamplerState(&raw const sampler_desc, Some(&raw mut sampler)) }
            .context("failed to create fixed-frame sampler")?;
        let sampler = sampler.context("D3D11 returned a null sampler")?;

        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<[f32; 4]>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut source_region = None;
        // SAFETY: The constant-buffer descriptor is complete and has 16-byte size.
        unsafe { device.CreateBuffer(&raw const buffer_desc, None, Some(&raw mut source_region)) }
            .context("failed to create source-region constant buffer")?;
        let source_region = source_region.context("D3D11 returned a null constant buffer")?;
        Ok(Self { vertex_shader, pixel_shader, sampler, source_region })
    }

    /// Submit one fullscreen draw and unbind input resources before WGC reuse.
    #[expect(clippy::multiple_unsafe_ops_per_block, reason = "one documented D3D11 draw transaction")]
    fn draw(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11ShaderResourceView,
        target: &ID3D11RenderTargetView,
        region: [f32; 4]) {
        // SAFETY: The constant data is exactly the buffer's 16-byte extent. All
        // resources share a device and remain alive through the state transaction.
        unsafe {
            context.UpdateSubresource(
                &self.source_region,
                0,
                None,
                region.as_ptr().cast(),
                0,
                0);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(Some(&self.vertex_shader), None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.source_region.clone())]));
            context.PSSetShader(Some(&self.pixel_shader), None);
            context.PSSetShaderResources(0, Some(&[Some(source.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(6, 0);

            context.OMSetRenderTargets(Some(&[]), None);
            context.VSSetShader(None, None);
            context.VSSetConstantBuffers(0, Some(&[None]));
            context.PSSetShader(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
            context.PSSetSamplers(0, Some(&[None]));
        }
    }
}

/// Small NV12 pool whose slots are unavailable while retained by the async MFT.
pub struct Nv12Pool {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    surfaces: Vec<Nv12Surface>,
    state: PoolState,
}

impl Nv12Pool {
    /// Allocate fixed media surfaces and cache all video-processor views.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &FixedFrame) -> anyhow::Result<Self> {
        let source_size = source.size();
        ensure!(source_size.width.is_multiple_of(2) && source_size.height.is_multiple_of(2),
            "NV12 dimensions must be even");
        let video_device = device.cast::<ID3D11VideoDevice>()
            .context("failed to query D3D11 video device")?;
        let video_context = context.cast::<ID3D11VideoContext>()
            .context("failed to query D3D11 video context")?;
        let video_context_1 = context.cast::<ID3D11VideoContext1>()
            .context("failed to query color-space-aware D3D11 video context")?;
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL { Numerator: 1, Denominator: 1 },
            InputWidth: source_size.width,
            InputHeight: source_size.height,
            OutputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL { Numerator: 1, Denominator: 1 },
            OutputWidth: source_size.width,
            OutputHeight: source_size.height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: The content descriptor uses validated non-zero fixed dimensions.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&raw const content_desc) }
            .context("failed to create video-processor enumerator")?;
        // SAFETY: Conversion uses the first supported rate-conversion capability.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("failed to create BGRA-to-NV12 processor")?;
        // The source contains display-encoded full-range RGB. Emit studio-range
        // BT.709 NV12 so H.264 decoders apply the inverse transform consistently.
        // SAFETY: The processor and versioned immediate context share `device`.
        unsafe {
            video_context_1.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        }
        // SAFETY: The processor and versioned immediate context share `device`.
        unsafe {
            video_context_1.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
        }

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view = None;
        // SAFETY: The fixed BGRA surface and enumerator share `device`.
        unsafe {
            video_device.CreateVideoProcessorInputView(
                source.texture(),
                &enumerator,
                &raw const input_desc,
                Some(&raw mut input_view))
        }.context("failed to create fixed BGRA video input view")?;
        let input_view = input_view.context("D3D11 returned a null video input view")?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut surfaces = Vec::with_capacity(SURFACE_COUNT);
        for index in 0..SURFACE_COUNT {
            let texture = create_texture(
                device,
                source_size,
                DXGI_FORMAT_NV12,
                D3D11_BIND_RENDER_TARGET.0 as u32)
                .with_context(|| format!("failed to allocate NV12 surface {index}"))?;
            let mut output_view = None;
            // SAFETY: The NV12 surface and enumerator share `device`.
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &raw const output_desc,
                    Some(&raw mut output_view))
            }.with_context(|| format!("failed to create NV12 output view {index}"))?;
            surfaces.push(Nv12Surface {
                texture,
                output_view: output_view.context("D3D11 returned a null video output view")?,
                revision: None,
            });
        }

        Ok(Self {
            video_context,
            processor,
            input_view,
            surfaces,
            state: PoolState::new(SURFACE_COUNT),
        })
    }

    /// Acquire one surface not retained by the encoder.
    #[inline]
    pub fn acquire(&mut self) -> Option<usize> { self.state.acquire() }

    /// Convert the fixed frame only if this rotating slot contains older content.
    pub fn prepare(&mut self, slot: usize, revision: u64) -> anyhow::Result<()> {
        let surface = self.surfaces.get_mut(slot).context("NV12 slot is out of range")?;
        if surface.revision == Some(revision) {
            return Ok(());
        }
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
            ..D3D11_VIDEO_PROCESSOR_STREAM::default()
        };
        // SAFETY: Processor, cached views, and context share the same device.
        unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &surface.output_view,
                0,
                &[stream])
        }.context("failed to convert fixed BGRA frame to NV12")?;
        surface.revision = Some(revision);
        Ok(())
    }

    /// Borrow a checked slot texture for Media Foundation submission.
    pub fn texture(&self, slot: usize) -> anyhow::Result<&ID3D11Texture2D> {
        self.surfaces.get(slot)
            .map(|surface| &surface.texture)
            .context("NV12 slot is out of range")
    }

    /// Return the fixed number of callbacks required by this pool.
    #[inline]
    pub const fn surface_count(&self) -> usize { self.surfaces.len() }

    /// Recycle a surface after its tracked MF sample is finally released.
    #[inline]
    pub fn release(&mut self, slot: usize) -> anyhow::Result<()> { self.state.release(slot) }
}

/// One cached NV12 texture/view and the fixed-frame revision it contains.
struct Nv12Surface {
    texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    revision: Option<u64>,
}

/// Pure checked ownership state for the async-MFT surface pool.
struct PoolState {
    free: VecDeque<usize>,
    in_use: Vec<bool>,
}

impl PoolState {
    /// Create a pool with every slot available exactly once.
    fn new(count: usize) -> Self {
        Self {
            free: (0..count).collect(),
            in_use: vec![false; count],
        }
    }

    /// Mark the oldest free slot retained until a release callback arrives.
    fn acquire(&mut self) -> Option<usize> {
        let slot = self.free.pop_front()?;
        self.in_use[slot] = true;
        Some(slot)
    }

    /// Validate and return one retained slot to the rotation.
    fn release(&mut self, slot: usize) -> anyhow::Result<()> {
        let in_use = self.in_use.get_mut(slot).context("released NV12 slot is out of range")?;
        ensure!(*in_use, "NV12 slot {slot} was released more than once");
        *in_use = false;
        self.free.push_back(slot);
        Ok(())
    }
}

/// Allocate one single-mip GPU texture and require a non-null result.
fn create_texture(
    device: &ID3D11Device,
    size: Size2D<u32>,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind_flags: u32) -> anyhow::Result<ID3D11Texture2D> {
    let descriptor = D3D11_TEXTURE2D_DESC {
        Width: size.width,
        Height: size.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: The descriptor is complete and the output option lives through the call.
    unsafe { device.CreateTexture2D(&raw const descriptor, None, Some(&raw mut texture)) }
        .context("D3D11 texture allocation failed")?;
    texture.context("D3D11 returned a null texture")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_crop_should_clamp_and_reject_empty_resize_overlap() {
        let crop = CropRect { min_x: 100, min_y: 50, max_x: 300, max_y: 200 };
        assert_eq!(
            SourceRegion::clamped(Size2D::new(250, 125), Some(crop)),
            Some(SourceRegion { min_x: 100, min_y: 50, max_x: 250, max_y: 125 }));
        assert_eq!(SourceRegion::clamped(Size2D::new(50, 50), Some(crop)), None);
    }

    #[test]
    fn viewport_should_aspect_fit_wide_and_tall_sources() {
        let wide = calculate_viewport(Size2D::new(1920, 1080), Size2D::new(1920, 1200));
        let tall = calculate_viewport(Size2D::new(1200, 1920), Size2D::new(1920, 1200));

        assert_eq!((wide.TopLeftX, wide.TopLeftY, wide.Width, wide.Height), (0.0, 60.0, 1920.0, 1080.0));
        assert_eq!((tall.TopLeftX, tall.TopLeftY, tall.Width, tall.Height), (585.0, 0.0, 750.0, 1200.0));
    }

    #[test]
    fn pool_should_not_reissue_a_retained_surface() {
        let mut pool = PoolState::new(2);
        assert_eq!(pool.acquire(), Some(0));
        assert_eq!(pool.acquire(), Some(1));
        assert_eq!(pool.acquire(), None);
        pool.release(0).unwrap();
        assert_eq!(pool.acquire(), Some(0));
    }

    #[test]
    fn pool_should_reject_double_and_unknown_releases() {
        let mut pool = PoolState::new(1);
        assert!(pool.release(0).unwrap_err().to_string().contains("more than once"));
        assert!(pool.release(2).unwrap_err().to_string().contains("out of range"));
    }
}
