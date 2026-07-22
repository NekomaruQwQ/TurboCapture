//! Preview-owned D3D11 device and texture-view helpers.
//!
//! These helpers intentionally start from the original `live-capture`
//! implementation (now `live-encoder`),
//! but are local so preview adapter selection and presentation requirements can
//! diverge from the encoder without creating a shared compatibility contract.

use nkcore::debug::*;
use nkcore::prelude::*;
use nkcore::*;

use windows::core::Interface as _;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;

/// Create a D3D11 device on the highest-performance GPU adapter.
///
/// BGRA support is required by WGC and the preview swap chain. Unlike the
/// streaming device, this preview-owned device does not request video-processor
/// support because it never converts to NV12. Multithread protection keeps WGC
/// and presentation access safe if their scheduling diverges internally.
pub fn create_device() -> anyhow::Result<(IDXGIFactory6, ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: No preconditions; creates a new DXGI factory COM object.
    let dxgi_factory =
        api_call!(unsafe { CreateDXGIFactory::<IDXGIFactory6>() })?;
    // SAFETY: `dxgi_factory` is a valid COM object from the line above.
    let dxgi_adapter =
        api_call!(unsafe {
            dxgi_factory.EnumAdapterByGpuPreference::<IDXGIAdapter>(
                0,
                DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
        })?;

    let DXGI_ADAPTER_DESC { Description: adapter_name, .. } =
        // SAFETY: `dxgi_adapter` is a valid COM object obtained above.
        api_call!(unsafe { dxgi_adapter.GetDesc() })?;
    // SAFETY: `adapter_name` is a null-terminated wide string from `DXGI_ADAPTER_DESC`.
    let adapter_name =
        unsafe { widestring::U16CString::from_ptr_str(adapter_name.as_ptr()) }
            .to_string_lossy();
    log::info!("device: {adapter_name}");

    let mut device = None;
    let mut device_context = None;
    // SAFETY: `dxgi_adapter` is valid. Output pointers are stack-local `Option`s
    // initialized to `None`; D3D11 writes into them on success.
    api_call!(unsafe {
        D3D11CreateDevice(
            &dxgi_adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT |
            cfg!(debug_assertions)
                .then_some(D3D11_CREATE_DEVICE_DEBUG)
                .unwrap_or_default(),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut device_context))
    })?;

    let device =
        device
            .ok_or_else(|| anyhow::anyhow!("failed to create D3D11 device"))?;
    let device_context =
        device_context
            .ok_or_else(|| anyhow::anyhow!("failed to create D3D11 device context"))?;

    // SAFETY: Every D3D11 device implements `ID3D11Multithread`; the interface
    // remains valid because it owns a COM reference to `device`.
    let multithread = api_call!(unsafe { device.cast::<ID3D11Multithread>() })?;
    // SAFETY: `multithread` is a valid interface obtained from the cast above.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    Ok((dxgi_factory, device, device_context))
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
