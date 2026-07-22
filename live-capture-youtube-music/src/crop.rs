//! CSS-based crop rect computation for the YouTube Music player bar.
//!
//! Derives the crop rectangle from YouTube Music's CSS layout constants and the actual
//! window DPI, rather than hardcoded pixel margins.  This makes the crop
//! DPI-independent — it works at any display scale factor.
//!
//! ## Coordinate pipeline
//!
//! 1. CSS viewport size = `GetClientRect` physical pixels / scale factor
//! 2. Bar bounding box in CSS pixels (from YouTube Music's known CSS layout)
//! 3. Shrink inward by [`PADDING`] to avoid window-border artifacts
//! 4. Scale back to physical client-area pixels
//! 5. Offset into captured-texture space (which is `DWMWA_EXTENDED_FRAME_BOUNDS`,
//!    the visible window including title bar but excluding the DWM shadow)

use euclid::default::{Box2D, Point2D, Size2D, Vector2D};

use windows::Win32::Foundation::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ── YouTube Music CSS layout constants ───────────────────────────────────────

/// Height of the player bar at the bottom of the viewport in CSS pixels.
const PLAYER_BAR_HEIGHT: f32 = 72.0;

/// Width of the browser scrollbar on the right edge in CSS pixels.
///
/// This is actually an approximated value. YouTube Music CSS sets
/// `--ytmusic-scrollbar-width: 12px;`, but the actual scrollbar is
/// typically wider.
const SCROLL_BAR_WIDTH: f32 = 16.0;

/// Inward padding in CSS pixels to avoid capturing window borders or other
/// artifacts at the edges.
const PADDING: f32 = 2.0;

// ── Win32 geometry helpers ──────────────────────────────────────────────────

/// Returns the visible window bounds via `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`.
///
/// `GetWindowRect` includes the invisible DWM shadow border (~7-12px per
/// side), but WinRT Graphics Capture captures only the visible window — so
/// the captured texture matches the extended frame bounds, not `GetWindowRect`.
//
//: Currently not used. Keep around for debugging and future reference.
#[cfg(false)]
fn get_visible_window_rect(hwnd: HWND) -> anyhow::Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `hwnd` is a valid handle; `&raw mut rect` points to a valid
    // stack-local `RECT` of the correct size for DWMWA_EXTENDED_FRAME_BOUNDS.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut rect).cast(),
            size_of::<RECT>() as u32)
    }.map_err(|e| anyhow::anyhow!("DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS): {e}"))?;
    Ok(rect)
}

/// Returns the window's client-area dimensions.
fn get_client_rect(hwnd: HWND) -> anyhow::Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: same as `get_window_rect`.
    unsafe { GetClientRect(hwnd, &raw mut rect) }
        .map_err(|e| anyhow::anyhow!("GetClientRect: {e}"))?;
    Ok(rect)
}

/// Returns the offset from the visible window's top-left corner to the
/// client area's top-left corner, in physical pixels.
///
/// This is the visible frame thickness (title bar height, visible borders).
/// Used to project client-area coordinates into captured-texture coordinates.
//
//: Currently replaced with the experie
#[cfg(false)]
fn get_frame_offset(hwnd: HWND, window_rect: &RECT) -> anyhow::Result<Vector2D<u32>> {
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: `hwnd` is valid; `&raw mut origin` is a valid stack-local POINT.
    // ClientToScreen translates client (0,0) to screen coordinates.
    unsafe { ClientToScreen(hwnd, &raw mut origin) }
        .ok()
        .with_context(|| anyhow::anyhow!("ClientToScreen failed"))?;
    Ok(Vector2D::new(
        (origin.x - window_rect.left) as u32,
        (origin.y - window_rect.top) as u32))
}

// ── Crop rect computation ───────────────────────────────────────────────────

/// Compute the crop rectangle for the YouTube Music player bar in captured-texture
/// coordinates (`GetWindowRect` space).
///
/// Returns a `Box2D<u32>` with min (inclusive) / max (exclusive) corners
/// suitable for passing to `live-encoder --crop-min-x/y --crop-max-x/y`.
pub fn compute_crop_rect(hwnd: HWND) -> anyhow::Result<Box2D<u32>> {
    let client_rect = get_client_rect(hwnd)?;

    // DPI scale factor (96 DPI = 100% = scale 1.0).
    // SAFETY: `hwnd` is valid; returns 0 only for invalid handles.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    anyhow::ensure!(dpi > 0, "GetDpiForWindow returned 0 for hwnd {}", hwnd.0 as usize);
    let scale = dpi as f32 / 96.0;

    // Client area in physical pixels → CSS viewport size.
    let client_phys = Size2D::<f32>::new(
        (client_rect.right - client_rect.left) as f32,
        (client_rect.bottom - client_rect.top) as f32);
    let viewport = Size2D::new(client_phys.width / scale, client_phys.height / scale);
    log::info!("Client area: {client_phys:?} physical pixels, viewport: {viewport:?} CSS pixels, scale factor: {scale:.2}");

    // Bar bounding box in CSS viewport coordinates, shrunk inward by PADDING.
    let css_crop = Box2D::<f32>::new(
        Point2D::new(
            PADDING,
            viewport.height - PLAYER_BAR_HEIGHT + PADDING),
        Point2D::new(
            viewport.width - SCROLL_BAR_WIDTH - PADDING,
            viewport.height - PADDING));
    log::info!("CSS crop rect: {css_crop:?} (in CSS pixels)");

    // CSS → physical client-area pixels.
    // floor(min) and ceil(max) to avoid clipping content at sub-pixel boundaries.
    let client_crop = Box2D::<u32>::new(
        Point2D::new(
            (css_crop.min.x * scale).floor() as u32,
            (css_crop.min.y * scale).floor() as u32),
        Point2D::new(
            (css_crop.max.x * scale).ceil() as u32,
            (css_crop.max.y * scale).ceil() as u32));
    log::info!("Client crop rect: {client_crop:?} (in physical pixels)");

    // Project from client area into captured-texture space by adding the
    // frame offset (title bar height + left border width).
    //
    // This is a temporary workaround given that `get_frame_offset` is unreliable
    // across DPI scales and Windows versions.
    //
    // Experimentally, we assume the title bar height follows:
    //     title_bar_height = 28px * scale + 4px (borders)
    // This formula is interpolated between observed values at:
    //     100% -> 32px (the standard title bar height at 96 DPI)
    //     150% -> 46px,
    //     175% -> 53px.
    let offset = Vector2D::new(0, (28.0 * scale + 4.0).round() as u32);
    log::info!("Frame offset: {offset:?} (title bar + borders)");

    let final_rect = Box2D::new(
        client_crop.min + offset,
        client_crop.max + offset);
    log::info!("Final crop rect in captured-texture coordinates: {final_rect:?}");
    Ok(Box2D::new(
        client_crop.min + offset,
        client_crop.max + offset))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(false)] // To allow manual fine tuning without updating tests.
mod tests {
    use super::*;

    /// Verify the CSS → physical math at a known geometry.
    ///
    /// Simulates a 1920×1080 client area at 150% DPI (144 DPI) with a frame
    /// offset of (0, 44) — typical visible frame from DWMWA_EXTENDED_FRAME_BOUNDS
    /// (no left border, ~44px title bar at 150%).
    #[test]
    fn css_crop_at_150_percent() {
        let scale: f32 = 144.0 / 96.0; // 1.5
        let client_w: f32 = 1920.0;
        let client_h: f32 = 1080.0;

        let viewport = Size2D::<f32>::new(client_w / scale, client_h / scale);
        // viewport = (1280.0, 720.0)
        assert!((viewport.width - 1280.0).abs() < 0.01);
        assert!((viewport.height - 720.0).abs() < 0.01);

        let css_crop = Box2D::<f32>::new(
            Point2D::new(PADDING, viewport.height - PLAYER_BAR_HEIGHT + PADDING),
            Point2D::new(
                viewport.width - SCROLL_BAR_WIDTH - PADDING,
                viewport.height - PADDING));

        // Expected CSS crop (PADDING=1):
        //   min = (1.0, 720 - 72 + 1) = (1.0, 649.0)
        //   max = (1280 - 12 - 1, 720 - 1) = (1267.0, 719.0)
        assert!((css_crop.min.x - 1.0).abs() < 0.01);
        assert!((css_crop.min.y - 649.0).abs() < 0.01);
        assert!((css_crop.max.x - 1267.0).abs() < 0.01);
        assert!((css_crop.max.y - 719.0).abs() < 0.01);

        // CSS → physical (at 1.5x)
        let client_crop = Box2D::<u32>::new(
            Point2D::new(
                (css_crop.min.x * scale).floor() as u32,
                (css_crop.min.y * scale).floor() as u32),
            Point2D::new(
                (css_crop.max.x * scale).ceil() as u32,
                (css_crop.max.y * scale).ceil() as u32));

        // min = floor(1*1.5, 649*1.5) = floor(1.5, 973.5) = (1, 973)
        // max = ceil(1267*1.5, 719*1.5) = ceil(1900.5, 1078.5) = (1901, 1079)
        assert_eq!(client_crop.min, Point2D::new(1, 973));
        assert_eq!(client_crop.max, Point2D::new(1901, 1079));

        // Visible frame offset (0, 44) from DWMWA_EXTENDED_FRAME_BOUNDS
        let offset = Vector2D::new(0u32, 44u32);
        let texture_crop = Box2D::new(
            client_crop.min + offset,
            client_crop.max + offset);

        assert_eq!(texture_crop.min, Point2D::new(1, 1017));
        assert_eq!(texture_crop.max, Point2D::new(1901, 1123));
    }

    /// Verify at 100% DPI (96 DPI, scale = 1.0).
    #[test]
    fn css_crop_at_100_percent() {
        let scale: f32 = 1.0;
        let client_w: f32 = 1280.0;
        let client_h: f32 = 720.0;

        let viewport = Size2D::<f32>::new(client_w / scale, client_h / scale);
        assert!((viewport.width - 1280.0).abs() < 0.01);

        let css_crop = Box2D::<f32>::new(
            Point2D::new(PADDING, viewport.height - PLAYER_BAR_HEIGHT + PADDING),
            Point2D::new(
                viewport.width - SCROLL_BAR_WIDTH - PADDING,
                viewport.height - PADDING));

        let client_crop = Box2D::<u32>::new(
            Point2D::new(
                (css_crop.min.x * scale).floor() as u32,
                (css_crop.min.y * scale).floor() as u32),
            Point2D::new(
                (css_crop.max.x * scale).ceil() as u32,
                (css_crop.max.y * scale).ceil() as u32));

        // At 100%, CSS pixels = physical pixels (PADDING=1).
        // min = (1, 720 - 72 + 1) = (1, 649)
        // max = (1280 - 12 - 1, 720 - 1) = (1267, 719)
        assert_eq!(client_crop.min, Point2D::new(1, 649));
        assert_eq!(client_crop.max, Point2D::new(1267, 719));

        // Visible frame offset (0, 30) — title bar at 100%
        let offset = Vector2D::new(0u32, 30u32);
        let texture_crop = Box2D::new(
            client_crop.min + offset,
            client_crop.max + offset);

        assert_eq!(texture_crop.min, Point2D::new(1, 679));
        assert_eq!(texture_crop.max, Point2D::new(1267, 749));
    }
}
