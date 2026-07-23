//! Capture-owned D3D11 device and texture-view helpers.

use nkcore::debug::*;
use nkcore::prelude::*;
use nkcore::*;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;

use live_shared_texture::AdapterLuid;

/// Create a D3D11 device on an explicit managed adapter or the standalone default.
///
/// BGRA support is required by WGC and the preview swap chain. Unlike the
/// streaming device, this preview-owned device does not request video-processor
/// support because it never converts to NV12. Managed output must pass the
/// supervisor's LUID; standalone preview selects the highest-performance GPU.
/// Multithread protection keeps WGC and presentation access safe if their
/// scheduling diverges internally.
pub fn create_device(
    adapter_luid: Option<AdapterLuid>)
    -> anyhow::Result<(IDXGIFactory6, ID3D11Device, ID3D11DeviceContext)> {
    let bundle = if let Some(adapter_luid) = adapter_luid {
        live_shared_texture::create_device_on_adapter(adapter_luid, false)?
    } else {
        live_shared_texture::create_high_performance_device(false)?
    };
    log::info!("device: {} ({})", bundle.adapter_name, bundle.adapter_luid);
    Ok((bundle.factory, bundle.device, bundle.context))
}

/// Create a default-format shader resource view for a captured 2D texture.
///
/// `device` and `texture` must originate from the same D3D11 device. The WGC
/// session and presenter preserve this invariant by sharing [`create_device`]'s
/// returned device.
pub fn create_srv_for_texture_2d(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D) -> anyhow::Result<ID3D11ShaderResourceView> {
    Ok({
        // SAFETY: `device` and `texture` are valid same-device COM objects.
        out_var_or_err(|out| api_call!(unsafe {
            device.CreateShaderResourceView(
                texture,
                None,
                Some(out))
        }))?.expect("unexpected null shader resource view")
    })
}

/// Create a default-format render target view for a swap-chain texture.
///
/// `device` and `texture` must originate from the same D3D11 device. A
/// successful call returns a non-null COM interface owned by the caller.
pub fn create_rtv_for_texture_2d(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D) -> anyhow::Result<ID3D11RenderTargetView> {
    Ok({
        // SAFETY: `device` and `texture` are valid same-device COM objects.
        out_var_or_err(|out| api_call!(unsafe {
            device.CreateRenderTargetView(
                texture,
                None,
                Some(out))
        }))?.expect("unexpected null render target view")
    })
}
