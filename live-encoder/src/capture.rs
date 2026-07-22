//! Capture session wrapper, viewport calculation, and crop specification.
//!
//! Re-exports `winrt_capture::CaptureSession` and provides the
//! aspect-ratio-preserving viewport calculation used by the resample path,
//! plus the crop types and box computation used by the crop path.

pub use winrt_capture::CaptureSession;

use nkcore::prelude::euclid::Size2D;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_VIEWPORT};

// ── Crop types ──────────────────────────────────────────────────────────────

/// Absolute crop rectangle in source-pixel coordinates.
///
/// Specifies the exact subrect to extract from the captured window. The caller
/// is responsible for providing valid coordinates (e.g. from `WindowInfo` or a
/// prior size query).  Coordinates are clamped to source bounds at capture time
/// to guard against window resizes.
#[derive(Debug, Clone, Copy)]
pub struct CropBox {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

impl CropBox {
    /// Encoder-compatible output size: box dimensions rounded UP to the nearest
    /// multiple of 16.  Used for texture allocation, NV12 converter, and H.264
    /// encoder.  When the rounded size exceeds the actual crop, the extra pixels
    /// appear as padding on the right / bottom edge (filled with the staging
    /// texture's clear colour).
    pub const fn output_size(&self) -> Size2D<u32> {
        let w = self.max_x - self.min_x;
        let h = self.max_y - self.min_y;
        Size2D::new((w + 15) & !15, (h + 15) & !15)
    }

    /// Convert to a `D3D11_BOX` for `CopySubresourceRegion`, clamping to
    /// `source` so the box never reads out of bounds.
    pub fn to_d3d11_box(self, source: Size2D<u32>) -> D3D11_BOX {
        let left = self.min_x.min(source.width);
        let top  = self.min_y.min(source.height);
        let right  = self.max_x.min(source.width);
        let bottom = self.max_y.min(source.height);
        D3D11_BOX { left, top, front: 0, right, bottom, back: 1 }
    }
}

/// Compute a viewport that fits `source_size` into `target_size` with
/// aspect-ratio-preserving letterboxing.
///
/// The result is a `D3D11_VIEWPORT` centered within `target_size`, scaled
/// uniformly so the source fills as much of the target as possible without
/// stretching.
pub fn calculate_resample_viewport(
    source_size: Size2D<u32>,
    target_size: Size2D<u32>) -> D3D11_VIEWPORT {
    let scale =
        f32::min(
            target_size.width as f32 / source_size.width as f32,
            target_size.height as f32 / source_size.height as f32);
    let source_size_scaled =
        (source_size.to_f32() * scale).floor().to_u32();
    let target_offset =
        (target_size - source_size_scaled).to_vector() / 2;

    D3D11_VIEWPORT {
        TopLeftX: target_offset.x as _,
        TopLeftY: target_offset.y as _,
        Width: source_size_scaled.width as _,
        Height: source_size_scaled.height as _,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}
