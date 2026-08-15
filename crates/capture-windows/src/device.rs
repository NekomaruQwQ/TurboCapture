//! Exact-adapter D3D11 creation and startup capability validation.

use anyhow::{Context as _, ensure};
use capture_core::AdapterLuid;
use windows::{
    Graphics::Capture::GraphicsCaptureSession,
    Graphics::DirectX::Direct3D11::IDirect3DDevice,
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
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
                CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory6,
            },
        },
        System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
    },
    core::Interface as _,
};

/// The one D3D11 device/context pair owned by the native media thread.
pub struct DeviceBundle {
    /// Device created on the exact configured adapter.
    pub device: ID3D11Device,
    /// Immediate context serialized by the media owner.
    pub context: ID3D11DeviceContext,
    /// WinRT projection used to prove WGC interoperability at startup.
    pub _winrt_device: IDirect3DDevice,
}

/// Create the exact requested device and validate every Phase 2 GPU contract.
pub fn create_and_validate(requested: AdapterLuid) -> anyhow::Result<DeviceBundle> {
    // SAFETY: DXGI factory creation has no caller preconditions.
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory6>() }
        .context("failed to create DXGI 1.1 factory")?;
    let requested_raw = raw_luid(requested);
    // SAFETY: DXGI treats the supplied LUID as an opaque lookup key.
    let adapter = unsafe { factory.EnumAdapterByLuid::<IDXGIAdapter>(requested_raw) }
        .with_context(|| format!("configured adapter {requested} is unavailable"))?;
    // SAFETY: `adapter` is a live DXGI object and fills its own return value.
    let descriptor = unsafe { adapter.GetDesc() }.context("failed to describe configured adapter")?;
    ensure!(
        descriptor.AdapterLuid == requested_raw,
        "DXGI returned a different adapter for configured LUID {requested}");

    let mut device = None;
    let mut context = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_0;
    // SAFETY: The adapter remains live, the feature-level slice is valid for
    // the call, and all output pointers address initialized stack options.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
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

    // The capture wrapper can receive callbacks on a system worker. Protection
    // is enabled even though all explicit context use remains media-thread-owned.
    let multithread = device.cast::<ID3D11Multithread>()
        .context("D3D11 multithread protection is unavailable")?;
    // SAFETY: `multithread` is the device's own synchronization interface.
    let _previous = unsafe { multithread.SetMultithreadProtected(true) };
    device.cast::<ID3D11VideoDevice>()
        .context("configured adapter has no D3D11 video device")?;
    context.cast::<ID3D11VideoContext>()
        .context("configured adapter has no D3D11 video context")?;

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
    let dxgi_device = device.cast::<IDXGIDevice>()
        .context("D3D11 device does not expose IDXGIDevice")?;
    // SAFETY: `dxgi_device` is a live D3D11-backed DXGI device. The returned
    // inspectable is immediately cast to its documented WinRT interface.
    let winrt_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .context("configured adapter cannot be exposed to Windows Graphics Capture")?
        .cast::<IDirect3DDevice>()
        .context("WGC interop object does not expose IDirect3DDevice")?;

    let name_end = descriptor.Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(descriptor.Description.len());
    let adapter_name = String::from_utf16_lossy(&descriptor.Description[..name_end]);
    log::info!(
        "validated adapter {requested}: {adapter_name} at D3D feature level {feature_level:?}");
    Ok(DeviceBundle {
        device,
        context,
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
        "configured adapter lacks {purpose} support (required {required:#010x}, observed {observed:#010x})");
    Ok(())
}

/// Convert the platform-neutral unsigned representation to the Win32 layout.
const fn raw_luid(luid: AdapterLuid) -> LUID {
    let value = luid.get();
    LUID {
        LowPart: value as u32,
        HighPart: (value >> 32) as u32 as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_luid_translation_should_preserve_both_words() {
        let input = "0x89ABCDEF01234567".parse::<AdapterLuid>().unwrap();
        let output = raw_luid(input);

        assert_eq!(output.LowPart, 0x0123_4567);
        assert_eq!(output.HighPart as u32, 0x89AB_CDEF);
    }
}
