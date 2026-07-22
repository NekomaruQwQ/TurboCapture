//! Preview-owned Windows Graphics Capture types and viewport calculation.
//!
//! This module intentionally mirrors the initial implementation forked from
//! the original `live-capture` lineage (now `live-encoder`), but it belongs
//! exclusively to `live-selector` so preview
//! sizing and capture behavior can evolve without changing the stream encoder.

pub use winrt_capture::CaptureSession;

use nkcore::prelude::euclid::Size2D;
use windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT;

/// Compute a centered viewport that fits `source_size` inside `target_size`
/// while preserving the source aspect ratio.
///
/// Both dimensions of both sizes must be non-zero. The presenter enforces
/// this invariant before using the viewport for a D3D11 draw.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert the viewport geometry relevant to aspect-preserving placement.
    fn assert_viewport(
        viewport: D3D11_VIEWPORT,
        left: f32,
        top: f32,
        width: f32,
        height: f32) {
        assert_eq!(viewport.TopLeftX, left);
        assert_eq!(viewport.TopLeftY, top);
        assert_eq!(viewport.Width, width);
        assert_eq!(viewport.Height, height);
        assert_eq!(viewport.MinDepth, 0.0);
        assert_eq!(viewport.MaxDepth, 1.0);
    }

    #[test]
    fn matching_aspect_ratio_fills_target() {
        let viewport = calculate_resample_viewport(
            Size2D::new(1920, 1200),
            Size2D::new(1920, 1200));
        assert_viewport(viewport, 0.0, 0.0, 1920.0, 1200.0);
    }

    #[test]
    fn wide_source_is_letterboxed_vertically() {
        let viewport = calculate_resample_viewport(
            Size2D::new(1920, 1080),
            Size2D::new(1920, 1200));
        assert_viewport(viewport, 0.0, 60.0, 1920.0, 1080.0);
    }

    #[test]
    fn tall_source_is_letterboxed_horizontally() {
        let viewport = calculate_resample_viewport(
            Size2D::new(1200, 1920),
            Size2D::new(1920, 1200));
        assert_viewport(viewport, 585.0, 0.0, 750.0, 1200.0);
    }
}
