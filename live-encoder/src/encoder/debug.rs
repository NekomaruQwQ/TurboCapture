//! Debug logging for Media Foundation Transform (MFT) media types.

use nkcore::prelude::*;
use nkcore::debug::*;

use windows::core::*;
use windows::Win32::Media::MediaFoundation::*;

fn name_of_media_type(guid: GUID) -> Option<&'static str> {
    Some(match guid {
        MFMediaType_Video => "Video",
        MFMediaType_Audio => "Audio",
        _ => None?,
    })
}

fn name_of_video_format(guid: GUID) -> Option<&'static str> {
    Some(match guid {
        MFVideoFormat_NV12 => "NV12",
        MFVideoFormat_H264 => "H264",
        MFVideoFormat_HEVC => "HEVC",
        MFVideoFormat_RGB32 => "RGB32",
        MFVideoFormat_ARGB32 => "ARGB32",
        _ => None?,
    })
}

pub fn print_mft_supported_input_types(transform: &IMFTransform) {
    log::info!("--- Supported Input Types ---");
    for i in 0..100 {
        // SAFETY: `transform` is a valid IMFTransform; we enumerate types until
        // the MFT returns `MF_E_NO_MORE_TYPES`.
        match unsafe { transform.GetInputAvailableType(0, i) } {
            Ok(media_type) => {
                log::info!("Input type #{i}:");
                print_media_type(&media_type);
            }
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => {
                log::info!("Total input types: {i}");
                break;
            }
            Err(e) => {
                log::warn!("Failed to get input type #{i}: {e:?}");
                break;
            }
        }
    }
}

pub fn print_mft_supported_output_types(transform: &IMFTransform) {
    log::info!("--- Supported Output Types ---");
    for i in 0..100 {
        // SAFETY: `transform` is a valid IMFTransform; we enumerate types until
        // the MFT returns `MF_E_NO_MORE_TYPES`.
        match unsafe { transform.GetOutputAvailableType(0, i) } {
            Ok(media_type) => {
                log::info!("Output type #{i}:");
                print_media_type(&media_type);
            }
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => {
                log::info!("Total output types: {i}");
                break;
            }
            Err(e) => {
                log::warn!("Failed to get output type #{i}: {e:?}");
                break;
            }
        }
    }
}

fn print_media_type(media_type: &IMFMediaType) {
    // SAFETY: All calls below operate on a valid `IMFMediaType` obtained from the MFT.
    // Each attribute query returns `Err` if the attribute is absent (handled by `if let Ok`).
    if let Ok(major_type) = api_call!(unsafe { media_type.GetGUID(&MF_MT_MAJOR_TYPE) }) {
        log::info!(
            "  Major type: {}",
            name_of_media_type(major_type).map_or_else(|| format!("{major_type:?}"), ToOwned::to_owned));
    }

    // SAFETY: `media_type` is a valid IMFMediaType; returns Err if attribute is absent.
    if let Ok(subtype) = api_call!(unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }) {
        log::info!(
            "  Subtype: {}",
            name_of_video_format(subtype).map_or_else(|| format!("{subtype:?}"), ToOwned::to_owned));
    }

    // SAFETY: `media_type` is a valid IMFMediaType; returns Err if attribute is absent.
    if let Ok(frame_size) = api_call!(unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }) {
        let width = (frame_size >> 32) as u32;
        let height = (frame_size & 0xFFFF_FFFF) as u32;
        log::info!("  Frame size: {width}x{height}");
    }

    // SAFETY: `media_type` is a valid IMFMediaType; returns Err if attribute is absent.
    if let Ok(frame_rate) = api_call!(unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }) {
        let numerator = (frame_rate >> 32) as u32;
        let denominator = (frame_rate & 0xFFFF_FFFF) as u32;
        if denominator > 0 {
            log::info!("  Frame rate: {}/{} ({:.2} fps)",
                numerator, denominator,
                numerator as f64 / denominator as f64);
        }
    }

    // SAFETY: `media_type` is a valid IMFMediaType; returns Err if attribute is absent.
    if let Ok(interlace) = api_call!(unsafe { media_type.GetUINT32(&MF_MT_INTERLACE_MODE) }) {
        log::info!("  Interlace mode: {interlace:?}");
    }
}
