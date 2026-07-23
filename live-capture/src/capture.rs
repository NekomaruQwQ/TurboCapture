//! Windows Graphics Capture types and fixed-output geometry.

pub use winrt_capture::CaptureSession;

use nkcore::prelude::euclid::Size2D;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_VIEWPORT};

/// Absolute crop rectangle in captured-texture pixel coordinates.
///
/// The supervisor computes policy-specific geometry. This generic worker clamps
/// it against each observed frame so a concurrent window resize cannot issue an
/// out-of-bounds GPU copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropBox {
    /// Inclusive source x coordinate.
    pub min_x: u32,
    /// Inclusive source y coordinate.
    pub min_y: u32,
    /// Exclusive source x coordinate.
    pub max_x: u32,
    /// Exclusive source y coordinate.
    pub max_y: u32,
}

impl CropBox {
    /// Fixed encoder-compatible output size, padded on the right and bottom.
    pub const fn output_size(self) -> Size2D<u32> {
        let width = self.max_x - self.min_x;
        let height = self.max_y - self.min_y;
        Size2D::new((width + 15) & !15, (height + 15) & !15)
    }

    /// Clamp the crop to a live frame, returning no copy for an empty overlap.
    pub fn clamped_d3d11_box(self, source: Size2D<u32>) -> Option<D3D11_BOX> {
        let left = self.min_x.min(source.width);
        let top = self.min_y.min(source.height);
        let right = self.max_x.min(source.width);
        let bottom = self.max_y.min(source.height);
        if right <= left || bottom <= top {
            return None;
        }
        Some(D3D11_BOX { left, top, front: 0, right, bottom, back: 1 })
    }
}

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

    #[test]
    fn crop_output_size_rounds_up_for_nv12_and_h264() {
        let crop = CropBox { min_x: 2, min_y: 682, max_x: 1262, max_y: 750 };
        assert_eq!(crop.output_size(), Size2D::new(1264, 80));
    }

    #[test]
    fn crop_clamps_to_resized_frame_and_rejects_empty_overlap() {
        let crop = CropBox { min_x: 100, min_y: 50, max_x: 300, max_y: 200 };
        let clamped = crop.clamped_d3d11_box(Size2D::new(250, 125)).unwrap();
        assert_eq!((clamped.left, clamped.top), (100, 50));
        assert_eq!((clamped.right, clamped.bottom), (250, 125));
        assert!(crop.clamped_d3d11_box(Size2D::new(50, 50)).is_none());
    }

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
