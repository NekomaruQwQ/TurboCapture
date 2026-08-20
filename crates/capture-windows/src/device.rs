//! Default-adapter D3D11 creation and startup capability validation.

use anyhow::{Context as _, ensure};
use windows::{
    Graphics::Capture::GraphicsCaptureSession,
    Graphics::DirectX::Direct3D11::IDirect3DDevice,
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_FORMAT_SUPPORT_RENDER_TARGET, D3D11_FORMAT_SUPPORT_SHADER_SAMPLE,
                D3D11_FORMAT_SUPPORT_TEXTURE2D, D3D11_FORMAT_SUPPORT_VIDEO_ENCODER,
                D3D11_FORMAT_SUPPORT_VIDEO_PROCESSOR_OUTPUT, D3D11_SDK_VERSION,
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
                ID3D11VideoContext, ID3D11VideoDevice,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12},
                IDXGIDevice,
            },
        },
        System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
    },
    core::Interface as _,
};

/// The one D3D11 device/context pair owned by the native media thread.
pub struct DeviceBundle {
    /// Device created on the operating system's default hardware adapter.
    pub device: ID3D11Device,
    /// Immediate context serialized by the media owner.
    pub context: ID3D11DeviceContext,
    /// Actual adapter LUID encoded for adapter-filtered MFT enumeration.
    pub adapter_luid: u64,
    /// WinRT projection used to prove WGC interoperability at startup.
    pub _winrt_device: IDirect3DDevice,
}

/// Create the default hardware device and validate every Phase 2 GPU contract.
pub fn create_and_validate() -> anyhow::Result<DeviceBundle> {
    let mut device = None;
    let mut context = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_0;
    // SAFETY: The feature-level slice is valid for the call, and all output
    // pointers address initialized stack options.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            Some(&raw mut feature_level),
            Some(&raw mut context))
    }.context("failed to create the capture D3D11 device")?;
    let device = device.context("D3D11 returned a null device")?;
    let context = context.context("D3D11 returned a null immediate context")?;
    let dxgi_device = device.cast::<IDXGIDevice>()
        .context("D3D11 device does not expose IDXGIDevice")?;
    // SAFETY: The DXGI device returns its own live adapter interface.
    let adapter = unsafe { dxgi_device.GetAdapter() }
        .context("failed to obtain the selected DXGI adapter")?;
    // SAFETY: `adapter` is live and fills its own descriptor value.
    let descriptor = unsafe { adapter.GetDesc() }
        .context("failed to describe the selected DXGI adapter")?;
    let adapter_luid = luid_value(descriptor.AdapterLuid);

    // The capture wrapper can receive callbacks on a system worker. Protection
    // is enabled even though all explicit context use remains media-thread-owned.
    let multithread = device.cast::<ID3D11Multithread>()
        .context("D3D11 multithread protection is unavailable")?;
    // SAFETY: `multithread` is the device's own synchronization interface.
    let _previous = unsafe { multithread.SetMultithreadProtected(true) };
    device.cast::<ID3D11VideoDevice>()
        .context("selected adapter has no D3D11 video device")?;
    context.cast::<ID3D11VideoContext>()
        .context("selected adapter has no D3D11 video context")?;

    validate_format(
        &device,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_FORMAT_SUPPORT_TEXTURE2D.0 as u32
            | D3D11_FORMAT_SUPPORT_SHADER_SAMPLE.0 as u32
            | D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 as u32,
        "BGRA capture/resample")?;
    validate_format(
        &device,
        DXGI_FORMAT_NV12,
        D3D11_FORMAT_SUPPORT_TEXTURE2D.0 as u32
            | D3D11_FORMAT_SUPPORT_VIDEO_PROCESSOR_OUTPUT.0 as u32
            | D3D11_FORMAT_SUPPORT_VIDEO_ENCODER.0 as u32,
        "NV12 conversion/encoding")?;

    ensure!(
        GraphicsCaptureSession::IsSupported()
            .context("failed to query Windows Graphics Capture support")?,
        "Windows Graphics Capture is not supported in this session");
    // SAFETY: `dxgi_device` is a live D3D11-backed DXGI device. The returned
    // inspectable is immediately cast to its documented WinRT interface.
    let winrt_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .context("selected adapter cannot be exposed to Windows Graphics Capture")?
        .cast::<IDirect3DDevice>()
        .context("WGC interop object does not expose IDirect3DDevice")?;

    let name_end = descriptor.Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(descriptor.Description.len());
    let adapter_name = String::from_utf16_lossy(&descriptor.Description[..name_end]);
    log::info!(
        "validated default adapter 0x{adapter_luid:016X}: {adapter_name} at D3D feature level {feature_level:?}");
    Ok(DeviceBundle {
        device,
        context,
        adapter_luid,
        _winrt_device: winrt_device,
    })
}

/// Validate an exact set of D3D11 format-support bits with a useful diagnostic.
fn validate_format(
    device: &ID3D11Device,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    required: u32,
    purpose: &str) -> anyhow::Result<()> {
    // SAFETY: Format capability lookup reads only immutable device state.
    let observed = unsafe { device.CheckFormatSupport(format) }
        .with_context(|| format!("failed to query {purpose} format support"))?;
    ensure!(
        observed & required == required,
        "selected adapter lacks {purpose} support (required {required:#010x}, observed {observed:#010x})");
    Ok(())
}

/// Preserve the native two-word LUID layout as one loggable unsigned value.
const fn luid_value(luid: LUID) -> u64 {
    ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_luid_value_should_preserve_both_words() {
        let input = LUID {
            LowPart: 0x0123_4567,
            HighPart: 0x89AB_CDEFu32 as i32,
        };

        assert_eq!(luid_value(input), 0x89AB_CDEF_0123_4567);
    }
}
