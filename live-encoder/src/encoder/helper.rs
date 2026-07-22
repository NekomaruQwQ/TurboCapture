//! Hardware H.264 encoder enumeration via Media Foundation.
//!
//! Searches for async hardware MFTs that accept NV12 input and produce H.264
//! output.  Currently hardcoded to prefer NVIDIA encoders by friendly name.

use nkcore::prelude::*;
use nkcore::debug::*;
use nkcore::*;

use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

/// Find and activate a hardware H.264 encoder on the same GPU as `dxgi_device`.
///
/// Currently selects the first encoder whose friendly name contains "nvidia".
/// Falls back to error if no NVIDIA encoder is found.
pub fn find_h264_encoder(dxgi_device: &IDXGIDevice) -> anyhow::Result<IMFTransform> {
    static INPUT_TYPE: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    static OUTPUT_TYPE: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    // Query the adapter (used for logging; LUID matching is commented out for now)
    // SAFETY: `dxgi_device` is a valid IDXGIDevice cast from the D3D11 device.
    let _dxgi_adapter = api_call!(unsafe { dxgi_device.GetAdapter() })?;

    let mut out_activate = std::ptr::null_mut();
    let mut out_count = 0;
    // SAFETY: `INPUT_TYPE` and `OUTPUT_TYPE` are static structs with valid GUIDs.
    // `out_activate` and `out_count` are stack-local out-params that MFTEnumEx
    // fills with a CoTaskMem-allocated array of IMFActivate pointers.
    api_call!(unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT,
            Some(&raw const INPUT_TYPE),
            Some(&raw const OUTPUT_TYPE),
            &raw mut out_activate,
            &raw mut out_count)
    })?;

    if out_activate.is_null() || out_count == 0 {
        anyhow::bail!("no hardware H.264 encoder found");
    }

    log::info!("found {out_count} hardware H.264 encoder(s)");
    // SAFETY: `out_activate` was allocated by `MFTEnumEx` via `CoTaskMemAlloc`;
    // freeing it with `CoTaskMemFree` is the required cleanup.
    defer(|| unsafe { CoTaskMemFree(Some(out_activate.cast())) });

    // SAFETY: `out_activate` is non-null (checked above) and points to `out_count`
    // contiguous `Option<IMFActivate>` pointers allocated by `MFTEnumEx`.
    let activates = unsafe { std::slice::from_raw_parts(out_activate, out_count as usize) };

    log::info!("scanning {out_count} hardware H.264 encoder(s) for matching adapter...");
    for (index, activate) in activates.iter().enumerate() {
        let activate =
            activate
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("null activate pointer at index {index}"))?;

        let mut buf = [0u16; 256];
        let mut len = 0;
        // SAFETY: `activate` is a valid IMFActivate from the `MFTEnumEx` array.
        // `buf` is a 256-element stack array; `len` receives the actual string length.
        api_call!(unsafe {
            activate.GetString(
                &MFT_FRIENDLY_NAME_Attribute,
                &mut buf,
                Some(&raw mut len))
        })?;

        // SAFETY: `buf` contains a valid wide string of `len` characters written
        // by `GetString`. The pointer is valid for the lifetime of `buf`.
        let name = unsafe {
            widestring::U16Str::from_ptr(
                buf.as_ptr(),
                len as _)
        };

        let name = name.to_string_lossy();
        log::info!("Encoder #{index}: '{name}'");

        if name.to_ascii_lowercase().contains("nvidia") {
            log::info!("Selecting encoder #{index} ('{name}')");
            // SAFETY: `activate` is a valid IMFActivate for an NVIDIA H.264 encoder.
            return Ok(api_call!(unsafe {
                activate.ActivateObject::<IMFTransform>()
            })?);
        }
    }

    anyhow::bail!("no matching hardware H.264 encoder found for the current adapter");
}
