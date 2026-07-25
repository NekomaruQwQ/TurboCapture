//! D3D11 swap-chain presentation for the optional capture preview window.
//!
//! Captured BGRA textures stay on the GPU: the shared capture resampler draws
//! directly into the fixed-size swap-chain backbuffer, then DXGI presents it.

use anyhow::Context as _;
use crate::{
    capture::calculate_resample_viewport,
    d3d11,
    resample::Resampler,
};
use nkcore::prelude::euclid::Size2D;
use live_capture_shared::AdapterLuid;
use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D},
        Dxgi::{
            Common::{
                DXGI_ALPHA_MODE_UNSPECIFIED,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SAMPLE_DESC,
            },
            DXGI_MWA_NO_ALT_ENTER,
            DXGI_PRESENT,
            DXGI_SCALING_NONE,
            DXGI_SWAP_CHAIN_DESC1,
            DXGI_SWAP_EFFECT_FLIP_DISCARD,
            DXGI_USAGE_RENDER_TARGET_OUTPUT,
            IDXGISwapChain1,
        },
    },
};

/// Fixed-size D3D11 presentation target owned by the preview window.
///
/// The window referenced by the swap chain's HWND must outlive this value.
/// Every D3D resource stored here is created from `device`, which preserves
/// the same-device invariant required by D3D11 draw and copy operations.
pub struct Presenter {
    /// Device shared with every WGC capture session rendered by this target.
    device: ID3D11Device,
    /// Immediate context that serializes capture copies, draws, and presents.
    device_context: ID3D11DeviceContext,
    /// Flip-model surface bound to the preview window's HWND.
    swap_chain: IDXGISwapChain1,
    /// View of swap-chain buffer zero used as the resample destination.
    render_target: ID3D11RenderTargetView,
    /// Shared shader path used by the streaming capture pipeline.
    resampler: Resampler,
    /// Immutable physical output size used for viewport calculation.
    output_size: Size2D<u32>,
    /// Background color that replaces stale pixels outside the viewport.
    clear_color: [f32; 4],
}

impl Presenter {
    /// Create a D3D11 device and flip-discard swap chain for `hwnd`.
    ///
    /// `output_size` must match the physical client-area size and contain no
    /// zero dimension. The caller must retain the window until after this
    /// presenter is dropped.
    pub fn new(
        hwnd: HWND,
        output_size: Size2D<u32>,
        clear_color: [f32; 4],
        adapter_luid: Option<AdapterLuid>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            output_size.width > 0 && output_size.height > 0,
            "preview dimensions must be non-zero");

        let (factory, device, device_context) =
            d3d11::create_device(adapter_luid).context("failed to create preview D3D11 device")?;

        let descriptor = DXGI_SWAP_CHAIN_DESC1 {
            Width: output_size.width,
            Height: output_size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: 0,
        };

        // SAFETY: `factory`, `device`, and `hwnd` are valid for the lifetime of
        // the resulting swap chain. `descriptor` describes a supported,
        // non-multisampled flip-model BGRA surface with non-zero dimensions.
        let swap_chain = unsafe {
            factory.CreateSwapChainForHwnd(
                &device,
                hwnd,
                &raw const descriptor,
                None,
                None)
        }.context("failed to create preview swap chain")?;

        // Prevent DXGI's implicit Alt+Enter handler from violating the fixed
        // window-size contract by switching the preview to fullscreen.
        // SAFETY: `factory` created `swap_chain`, and `hwnd` remains valid.
        unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
            .context("failed to disable DXGI Alt+Enter handling")?;

        // SAFETY: Buffer zero exists because the swap chain has two buffers.
        let backbuffer = unsafe { swap_chain.GetBuffer::<ID3D11Texture2D>(0) }
            .context("failed to acquire preview backbuffer")?;
        let render_target =
            d3d11::create_rtv_for_texture_2d(&device, &backbuffer)
                .context("failed to create preview render target")?;
        let resampler =
            Resampler::new(&device).context("failed to create preview resampler")?;

        let presenter = Self {
            device,
            device_context,
            swap_chain,
            render_target,
            resampler,
            output_size,
            clear_color,
        };
        presenter.clear_and_present()?;
        Ok(presenter)
    }

    /// D3D11 device used by both the WGC session and the presentation target.
    pub const fn device(&self) -> &ID3D11Device { &self.device }

    /// Immediate context used to acquire WGC frames and submit preview draws.
    pub const fn device_context(&self) -> &ID3D11DeviceContext { &self.device_context }

    /// Fixed physical width of the swap-chain backbuffer.
    pub const fn output_width(&self) -> u32 { self.output_size.width }

    /// Fixed physical height of the swap-chain backbuffer.
    pub const fn output_height(&self) -> u32 { self.output_size.height }

    /// Clear the preview to its letterbox color and present the result.
    pub fn clear_and_present(&self) -> anyhow::Result<()> {
        // SAFETY: The render target belongs to `device_context`'s device.
        unsafe {
            self.device_context.ClearRenderTargetView(
                &self.render_target,
                &self.clear_color);
        }
        self.present()
    }

    /// Resample one captured texture into the fixed output and present it.
    ///
    /// `source_texture` must originate from a capture session created with
    /// [`Self::device`]. Letterbox regions are cleared before every draw so a
    /// source aspect-ratio change cannot leave pixels from the prior target.
    pub fn render(
        &self,
        source_texture: &ID3D11Texture2D,
        source_size: Size2D<u32>) -> anyhow::Result<()> {
        anyhow::ensure!(
            source_size.width > 0 && source_size.height > 0,
            "captured frame dimensions must be non-zero");
        let source_view =
            d3d11::create_srv_for_texture_2d(&self.device, source_texture)
                .context("failed to create captured-frame shader view")?;
        let viewport = calculate_resample_viewport(source_size, self.output_size);

        // SAFETY: The render target belongs to `device_context`'s device.
        unsafe {
            self.device_context.ClearRenderTargetView(
                &self.render_target,
                &self.clear_color);
        }
        // SAFETY: The viewport fits the fixed-size backbuffer.
        unsafe { self.device_context.RSSetViewports(Some(&[viewport])); }
        self.resampler.resample(
            &self.device_context,
            &source_view,
            &self.render_target);
        // SAFETY: Resetting the viewport unbinds state that should not leak
        // into subsequent rendering work on the immediate context.
        unsafe { self.device_context.RSSetViewports(Some(&[])); }

        self.present()
    }

    /// Present the current backbuffer with one vertical-sync interval.
    fn present(&self) -> anyhow::Result<()> {
        // SAFETY: `swap_chain` is valid and its HWND outlives this presenter.
        unsafe { self.swap_chain.Present(1, DXGI_PRESENT(0)) }
            .ok()
            .context("failed to present preview frame")
    }
}
